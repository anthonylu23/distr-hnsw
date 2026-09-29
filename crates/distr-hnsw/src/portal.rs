use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{
        decrypt_chunk, encrypt_chunk, random_key, random_nonce, unwrap_key, wrap_key, MasterKey,
        ENVELOPE_VERSION,
    },
    durability::sync_directory,
    format::{
        decode_manifest, encode_deletion_marker, encode_manifest, ChunkRecord, ManifestPayload,
    },
    metadata::{
        Database, DeleteOperation, DeleteState, FileRecord, FileState, NewChunk,
        NewDeleteOperation, NewUpload, UploadState,
    },
    object::{ObjectHash, ObjectKind},
    CHUNK_SIZE,
};

const STORAGE_CLASS: &str = "regular-rf2";
const MINIMUM_REPLICAS: usize = 2;
const CONTENT_KEY_PURPOSE: &[u8] = b"content";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTarget {
    pub id: String,
    pub failure_domain: String,
    pub base_url: String,
}

impl FromStr for AgentTarget {
    type Err = PortalError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let fields: Vec<_> = value.splitn(3, ',').collect();
        if fields.len() != 3 || fields.iter().any(|field| field.trim().is_empty()) {
            return Err(PortalError::InvalidAgent(value.to_owned()));
        }
        Ok(Self {
            id: fields[0].trim().to_owned(),
            failure_domain: fields[1].trim().to_owned(),
            base_url: fields[2].trim().trim_end_matches('/').to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failpoint {
    AfterPlan,
    AfterFirstChunkReplica,
    AfterChunksDurable,
    AfterFirstManifestReplica,
    AfterManifestDurable,
    BeforeCommit,
    AfterCommit,
    AfterDeletePlan,
    AfterFirstMarkerReplica,
    AfterMarkerDurable,
    BeforeDeleteCommit,
    AfterDeleteCommit,
}

impl Failpoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AfterPlan => "after_plan",
            Self::AfterFirstChunkReplica => "after_first_chunk_replica",
            Self::AfterChunksDurable => "after_chunks_durable",
            Self::AfterFirstManifestReplica => "after_first_manifest_replica",
            Self::AfterManifestDurable => "after_manifest_durable",
            Self::BeforeCommit => "before_commit",
            Self::AfterCommit => "after_commit",
            Self::AfterDeletePlan => "after_delete_plan",
            Self::AfterFirstMarkerReplica => "after_first_marker_replica",
            Self::AfterMarkerDurable => "after_marker_durable",
            Self::BeforeDeleteCommit => "before_delete_commit",
            Self::AfterDeleteCommit => "after_delete_commit",
        }
    }
}

impl FromStr for Failpoint {
    type Err = PortalError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "after_plan" => Ok(Self::AfterPlan),
            "after_first_chunk_replica" => Ok(Self::AfterFirstChunkReplica),
            "after_chunks_durable" => Ok(Self::AfterChunksDurable),
            "after_first_manifest_replica" => Ok(Self::AfterFirstManifestReplica),
            "after_manifest_durable" => Ok(Self::AfterManifestDurable),
            "before_commit" => Ok(Self::BeforeCommit),
            "after_commit" => Ok(Self::AfterCommit),
            "after_delete_plan" => Ok(Self::AfterDeletePlan),
            "after_first_marker_replica" => Ok(Self::AfterFirstMarkerReplica),
            "after_marker_durable" => Ok(Self::AfterMarkerDurable),
            "before_delete_commit" => Ok(Self::BeforeDeleteCommit),
            "after_delete_commit" => Ok(Self::AfterDeleteCommit),
            _ => Err(PortalError::InvalidFailpoint(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailpointAction {
    ReturnError,
    ExitProcess,
}

pub struct Portal {
    pub(crate) database: Database,
    pub(crate) master_key: MasterKey,
    pub(crate) agents: Vec<AgentTarget>,
    pub(crate) client: reqwest::Client,
    /// Incarnation each configured agent reported when last verified in this
    /// process. Placements are only recorded against a verified incarnation.
    incarnations: HashMap<String, String>,
    failpoint: Option<(Failpoint, FailpointAction)>,
}

impl Portal {
    pub fn open(
        database_path: &Path,
        master_key: MasterKey,
        agents: Vec<AgentTarget>,
    ) -> Result<Self, PortalError> {
        let agents = prepare_agents(agents)?;
        Ok(Self {
            database: Database::open(database_path)?,
            master_key,
            agents,
            client: reqwest::Client::new(),
            incarnations: HashMap::new(),
            failpoint: None,
        })
    }

    pub fn with_failpoint(mut self, failpoint: Failpoint, action: FailpointAction) -> Self {
        self.failpoint = Some((failpoint, action));
        self
    }

    pub async fn recover(
        &mut self,
        apply: bool,
    ) -> Result<crate::recovery::RecoveryReportV1, crate::recovery::RecoveryError> {
        crate::recovery::recover(
            &mut self.database,
            &self.master_key,
            &self.agents,
            &self.client,
            if apply {
                crate::recovery::RecoveryMode::Apply
            } else {
                crate::recovery::RecoveryMode::Plan
            },
        )
        .await
    }

    pub async fn upload(
        &mut self,
        source: &Path,
        idempotency_key: &str,
    ) -> Result<Uuid, PortalError> {
        let mut upload = match self.database.upload_by_idempotency(idempotency_key)? {
            Some(existing) if existing.state == UploadState::Committed => {
                if source.exists() {
                    let inspection = inspect_source(source)?;
                    let fingerprint = request_fingerprint(&inspection, STORAGE_CLASS);
                    if existing.request_fingerprint != fingerprint {
                        return Err(PortalError::IdempotencyConflict);
                    }
                }
                return Ok(existing.file_id);
            }
            Some(existing) => {
                self.verify_remote_agents().await?;
                let inspection = inspect_source(source)?;
                let fingerprint = request_fingerprint(&inspection, STORAGE_CLASS);
                if existing.request_fingerprint != fingerprint {
                    return Err(PortalError::IdempotencyConflict);
                }
                existing
            }
            None => {
                self.verify_remote_agents().await?;
                let inspection = inspect_source(source)?;
                let fingerprint = request_fingerprint(&inspection, STORAGE_CLASS);
                let upload =
                    new_upload(idempotency_key, inspection, fingerprint, &self.master_key)?;
                self.database.create_upload(&upload)?;
                self.database
                    .upload_by_idempotency(idempotency_key)?
                    .ok_or(PortalError::MissingUpload)?
            }
        };

        self.hit(Failpoint::AfterPlan)?;

        if upload.state == UploadState::Staging {
            self.database
                .advance_upload(upload.upload_id, UploadState::ReplicatingChunks)?;
            upload.state = UploadState::ReplicatingChunks;
        }

        let content_key = unwrap_key(
            &self.master_key,
            CONTENT_KEY_PURPOSE,
            upload.file_id,
            upload.generation,
            &upload.content_key,
        )?;
        let mut source_file = File::open(source)?;
        let mut chunk_replica_confirmations = 0_usize;
        let mut chunk_records = Vec::new();
        for plan in self.database.chunks(upload.upload_id)? {
            let mut plaintext = vec![0_u8; plan.plaintext_len as usize];
            read_exact_or_source_changed(&mut source_file, &mut plaintext)?;
            if blake3::hash(&plaintext).as_bytes() != &plan.plaintext_hash {
                return Err(PortalError::SourceChanged);
            }
            let ciphertext = encrypt_chunk(
                &content_key,
                plan.envelope_version,
                upload.file_id,
                plan.ordinal,
                plan.plaintext_len,
                &plan.nonce,
                &plaintext,
            )?;
            let hash = ObjectHash::digest(&ciphertext);
            let ciphertext_len =
                u32::try_from(ciphertext.len()).map_err(|_| PortalError::NumericOverflow)?;
            self.database.set_chunk_object(
                upload.upload_id,
                plan.ordinal,
                &hash,
                ciphertext_len,
            )?;
            self.replicate(
                ObjectKind::Chunk,
                &hash,
                &ciphertext,
                Failpoint::AfterFirstChunkReplica,
                &mut chunk_replica_confirmations,
            )
            .await?;
            chunk_records.push(ChunkRecord {
                ordinal: plan.ordinal,
                plaintext_len: plan.plaintext_len,
                nonce: plan.nonce,
                ciphertext_hash: hash,
                ciphertext_len,
            });
        }
        let mut trailing = [0_u8; 1];
        if source_file.read(&mut trailing)? != 0 {
            return Err(PortalError::SourceChanged);
        }
        self.hit(Failpoint::AfterChunksDurable)?;

        let (manifest_hash, manifest_bytes) = if let (Some(hash), Some(bytes)) =
            (upload.manifest_hash.clone(), upload.manifest_bytes.clone())
        {
            (hash, bytes)
        } else {
            let payload = ManifestPayload {
                name: upload.file_name.clone(),
                plaintext_len: upload.plaintext_size,
                plaintext_hash: upload.plaintext_hash,
                content_key: upload.content_key.clone(),
                chunks: chunk_records,
            };
            let bytes = encode_manifest(
                &self.master_key,
                upload.file_id,
                upload.generation,
                &payload,
            )?;
            let hash = ObjectHash::digest(&bytes);
            self.database
                .set_manifest_plan(upload.upload_id, &hash, &bytes)?;
            (hash, bytes)
        };

        let mut manifest_replica_confirmations = 0_usize;
        self.replicate(
            ObjectKind::Manifest,
            &manifest_hash,
            &manifest_bytes,
            Failpoint::AfterFirstManifestReplica,
            &mut manifest_replica_confirmations,
        )
        .await?;
        self.hit(Failpoint::AfterManifestDurable)?;
        self.ensure_live_upload_replica_floor(upload.upload_id, &manifest_hash)
            .await?;
        self.hit(Failpoint::BeforeCommit)?;
        let file = self
            .database
            .commit_file(upload.upload_id, MINIMUM_REPLICAS)?;
        self.hit(Failpoint::AfterCommit)?;
        Ok(file.file_id)
    }

    pub async fn delete(
        &mut self,
        file_id: Uuid,
        idempotency_key: &str,
    ) -> Result<DeleteOperation, PortalError> {
        let mut operation = match self.database.delete_by_idempotency(idempotency_key)? {
            Some(existing) => {
                if existing.file_id != file_id {
                    return Err(PortalError::IdempotencyConflict);
                }
                if existing.state == DeleteState::Committed {
                    return Ok(existing);
                }
                existing
            }
            None => {
                let projection = self
                    .database
                    .file_projection(file_id)?
                    .ok_or(PortalError::FileNotVisible(file_id))?;
                match projection.state {
                    FileState::Deleted => return Err(PortalError::AlreadyDeleted(file_id)),
                    FileState::RecoveryBlocked => {
                        return Err(PortalError::FileNotVisible(file_id));
                    }
                    FileState::Committed => {}
                }
                let generation = projection
                    .generation
                    .checked_add(1)
                    .ok_or(PortalError::NumericOverflow)?;
                let deleted_at = i64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| PortalError::SystemClockBeforeEpoch)?
                        .as_secs(),
                )
                .map_err(|_| PortalError::NumericOverflow)?;
                let marker_bytes =
                    encode_deletion_marker(&self.master_key, file_id, generation, deleted_at)?;
                let marker_hash = ObjectHash::digest(&marker_bytes);
                self.database.create_delete_operation(&NewDeleteOperation {
                    operation_id: Uuid::new_v4(),
                    idempotency_key: idempotency_key.to_owned(),
                    file_id,
                    generation,
                    deleted_at,
                    marker_hash,
                    marker_bytes,
                })?;
                self.database
                    .delete_by_idempotency(idempotency_key)?
                    .ok_or(PortalError::MissingDeleteOperation)?
            }
        };

        self.hit(Failpoint::AfterDeletePlan)?;
        self.verify_remote_agents().await?;
        let mut confirmations = 0_usize;
        self.replicate(
            ObjectKind::DeletionMarker,
            &operation.marker_hash,
            &operation.marker_bytes,
            Failpoint::AfterFirstMarkerReplica,
            &mut confirmations,
        )
        .await?;
        self.hit(Failpoint::AfterMarkerDurable)?;
        self.require_live_domains(ObjectKind::DeletionMarker, &operation.marker_hash)
            .await?;
        self.hit(Failpoint::BeforeDeleteCommit)?;
        operation = self
            .database
            .commit_delete(operation.operation_id, MINIMUM_REPLICAS)?;
        self.hit(Failpoint::AfterDeleteCommit)?;
        Ok(operation)
    }

    pub async fn download(&self, file_id: Uuid, destination: &Path) -> Result<(), PortalError> {
        if destination.exists() {
            return Err(PortalError::DestinationExists(destination.to_owned()));
        }
        let file = self
            .database
            .file_by_id(file_id)?
            .ok_or(PortalError::FileNotVisible(file_id))?;
        let manifest_bytes = self
            .fetch_valid_object(ObjectKind::Manifest, &file.manifest_hash)
            .await?;
        let manifest = decode_manifest(&self.master_key, &manifest_bytes)?;
        validate_manifest_file(&manifest, &file)?;
        let content_key = unwrap_key(
            &self.master_key,
            CONTENT_KEY_PURPOSE,
            manifest.file_id,
            manifest.generation,
            &manifest.payload.content_key,
        )?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = destination.with_extension(format!("distr-hnsw-{}.tmp", Uuid::new_v4()));
        let result = async {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            let mut hasher = blake3::Hasher::new();
            let mut total = 0_u64;
            for chunk in &manifest.payload.chunks {
                let ciphertext = self
                    .fetch_valid_object(ObjectKind::Chunk, &chunk.ciphertext_hash)
                    .await?;
                if ciphertext.len() != chunk.ciphertext_len as usize {
                    return Err(PortalError::ManifestMismatch("ciphertext length"));
                }
                let plaintext = decrypt_chunk(
                    &content_key,
                    ENVELOPE_VERSION,
                    manifest.file_id,
                    chunk.ordinal,
                    chunk.plaintext_len,
                    &chunk.nonce,
                    &ciphertext,
                )?;
                output.write_all(&plaintext)?;
                hasher.update(&plaintext);
                total = total
                    .checked_add(plaintext.len() as u64)
                    .ok_or(PortalError::NumericOverflow)?;
            }
            if total != manifest.payload.plaintext_len
                || hasher.finalize().as_bytes() != &manifest.payload.plaintext_hash
            {
                return Err(PortalError::PlaintextHashMismatch);
            }
            output.sync_all()?;
            drop(output);
            fs::rename(&temporary, destination)?;
            if let Some(parent) = destination.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn is_visible(&self, file_id: Uuid) -> Result<bool, PortalError> {
        Ok(self.database.file_by_id(file_id)?.is_some())
    }

    async fn replicate(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        bytes: &[u8],
        first_replica_failpoint: Failpoint,
        confirmation_count: &mut usize,
    ) -> Result<(), PortalError> {
        let mut last_failure = None;
        for agent in self.agents.clone() {
            if self.database.placement_confirmed(kind, hash, &agent.id)? {
                continue;
            }
            let Some(incarnation_id) = self.incarnations.get(&agent.id).cloned() else {
                last_failure = Some(PortalError::AgentNotVerified(agent.id.clone()));
                continue;
            };
            self.database.ensure_pending_placement(
                kind,
                hash,
                &agent.id,
                &agent.failure_domain,
                &incarnation_id,
            )?;
            let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
            let response = match self.client.put(url).body(bytes.to_vec()).send().await {
                Ok(response) => response,
                Err(error) => {
                    last_failure = Some(PortalError::Http(error));
                    continue;
                }
            };
            if !response.status().is_success() {
                last_failure = Some(PortalError::AgentRejected {
                    agent: agent.id.clone(),
                    status: response.status().as_u16(),
                    body: response.text().await.unwrap_or_default(),
                });
                continue;
            }
            self.database.confirm_placement(kind, hash, &agent.id)?;
            *confirmation_count += 1;
            if *confirmation_count == 1 {
                self.hit(first_replica_failpoint)?;
            }
        }
        if self.database.confirmed_domains(kind, hash)? < MINIMUM_REPLICAS {
            return Err(
                last_failure.unwrap_or_else(|| PortalError::ReplicaFloorNotMet {
                    kind,
                    hash: hash.clone(),
                }),
            );
        }
        Ok(())
    }

    async fn ensure_live_upload_replica_floor(
        &self,
        upload_id: Uuid,
        manifest_hash: &ObjectHash,
    ) -> Result<(), PortalError> {
        self.require_live_domains(ObjectKind::Manifest, manifest_hash)
            .await?;
        for chunk in self.database.chunks(upload_id)? {
            let hash = chunk
                .ciphertext_hash
                .ok_or(PortalError::MissingChunkObject)?;
            self.require_live_domains(ObjectKind::Chunk, &hash).await?;
        }
        Ok(())
    }

    async fn require_live_domains(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<(), PortalError> {
        let agent_map: HashMap<_, _> = self
            .agents
            .iter()
            .map(|agent| (agent.id.as_str(), agent))
            .collect();
        let mut live_domains = HashSet::new();
        let mut last_error = None;
        for agent_id in self.database.confirmed_agents(kind, hash)? {
            let Some(agent) = agent_map.get(agent_id.as_str()) else {
                continue;
            };
            let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
            match self.client.get(url).send().await {
                Ok(response) if response.status().is_success() => match response.bytes().await {
                    Ok(bytes) if ObjectHash::digest(&bytes) == *hash => {
                        live_domains.insert(agent.failure_domain.as_str());
                    }
                    Ok(_) => last_error = Some("portal hash verification failed".to_owned()),
                    Err(error) => last_error = Some(error.to_string()),
                },
                Ok(response) => {
                    last_error = Some(format!("agent returned HTTP {}", response.status()))
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        if live_domains.len() < MINIMUM_REPLICAS {
            return Err(PortalError::NoValidReplica {
                kind,
                hash: hash.clone(),
                detail: last_error.unwrap_or_else(|| {
                    format!(
                        "live replica floor not met ({}/{})",
                        live_domains.len(),
                        MINIMUM_REPLICAS
                    )
                }),
            });
        }
        Ok(())
    }

    async fn fetch_valid_object(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<Vec<u8>, PortalError> {
        let agent_map: HashMap<_, _> = self
            .agents
            .iter()
            .map(|agent| (agent.id.as_str(), agent))
            .collect();
        let mut last_error = None;
        for agent_id in self.database.confirmed_agents(kind, hash)? {
            let Some(agent) = agent_map.get(agent_id.as_str()) else {
                continue;
            };
            let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
            match self.client.get(url).send().await {
                Ok(response) if response.status().is_success() => match response.bytes().await {
                    Ok(bytes) if ObjectHash::digest(&bytes) == *hash => return Ok(bytes.to_vec()),
                    Ok(_) => last_error = Some("portal hash verification failed".to_owned()),
                    Err(error) => last_error = Some(error.to_string()),
                },
                Ok(response) => {
                    last_error = Some(format!("agent returned HTTP {}", response.status()))
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        Err(PortalError::NoValidReplica {
            kind,
            hash: hash.clone(),
            detail: last_error.unwrap_or_else(|| "no configured confirmed placement".to_owned()),
        })
    }

    /// Verify every configured agent's identity and record its incarnation.
    /// Unreachable agents are tolerated as long as the live failure domains
    /// still meet the replica floor; identity mismatches fail closed.
    async fn verify_remote_agents(&mut self) -> Result<(), PortalError> {
        let mut live_domains = HashSet::new();
        let mut last_failure = None;
        for agent in self.agents.clone() {
            let response = match self
                .client
                .get(format!("{}/v1/health", agent.base_url))
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_failure = Some(PortalError::Http(error));
                    continue;
                }
            };
            if !response.status().is_success() {
                last_failure = Some(PortalError::AgentRejected {
                    agent: agent.id.clone(),
                    status: response.status().as_u16(),
                    body: response.text().await.unwrap_or_default(),
                });
                continue;
            }
            let identity: RemoteAgentIdentity = response.json().await?;
            if identity.id != agent.id || identity.failure_domain != agent.failure_domain {
                return Err(PortalError::AgentIdentityMismatch(agent.id.clone()));
            }
            self.database.observe_agent_incarnation(
                &agent.id,
                &agent.failure_domain,
                &identity.incarnation_id,
            )?;
            self.incarnations
                .insert(agent.id.clone(), identity.incarnation_id);
            live_domains.insert(agent.failure_domain.clone());
        }
        if live_domains.len() < MINIMUM_REPLICAS {
            return Err(last_failure.unwrap_or(PortalError::InsufficientFailureDomains));
        }
        Ok(())
    }

    fn hit(&self, point: Failpoint) -> Result<(), PortalError> {
        let Some((configured, action)) = self.failpoint else {
            return Ok(());
        };
        if configured != point {
            return Ok(());
        }
        match action {
            FailpointAction::ReturnError => Err(PortalError::InjectedFailure(point)),
            FailpointAction::ExitProcess => std::process::exit(86),
        }
    }
}

#[derive(Clone)]
struct SourceInspection {
    name: String,
    plaintext_size: u64,
    plaintext_hash: [u8; 32],
    chunks: Vec<InspectedChunk>,
}

#[derive(Clone)]
struct InspectedChunk {
    plaintext_len: u32,
    plaintext_hash: [u8; 32],
}

fn inspect_source(path: &Path) -> Result<SourceInspection, PortalError> {
    if !path.is_file() {
        return Err(PortalError::SourceNotRegularFile(path.to_owned()));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PortalError::InvalidFileName(path.to_owned()))?
        .to_owned();
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut plaintext_size = 0_u64;
    loop {
        let mut chunk = vec![0_u8; CHUNK_SIZE];
        let mut read = 0_usize;
        while read < chunk.len() {
            let count = file.read(&mut chunk[read..])?;
            if count == 0 {
                break;
            }
            read += count;
        }
        if read == 0 {
            break;
        }
        chunk.truncate(read);
        hasher.update(&chunk);
        plaintext_size = plaintext_size
            .checked_add(read as u64)
            .ok_or(PortalError::NumericOverflow)?;
        chunks.push(InspectedChunk {
            plaintext_len: u32::try_from(read).map_err(|_| PortalError::NumericOverflow)?,
            plaintext_hash: *blake3::hash(&chunk).as_bytes(),
        });
    }
    Ok(SourceInspection {
        name,
        plaintext_size,
        plaintext_hash: *hasher.finalize().as_bytes(),
        chunks,
    })
}

fn request_fingerprint(inspection: &SourceInspection, storage_class: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"distr-hnsw:idempotency:v1");
    hasher.update(&inspection.plaintext_hash);
    hasher.update(&inspection.plaintext_size.to_le_bytes());
    hasher.update(&(inspection.name.len() as u64).to_le_bytes());
    hasher.update(inspection.name.as_bytes());
    hasher.update(&(storage_class.len() as u64).to_le_bytes());
    hasher.update(storage_class.as_bytes());
    *hasher.finalize().as_bytes()
}

fn new_upload(
    idempotency_key: &str,
    inspection: SourceInspection,
    request_fingerprint: [u8; 32],
    master_key: &MasterKey,
) -> Result<NewUpload, PortalError> {
    let upload_id = Uuid::new_v4();
    let file_id = Uuid::new_v4();
    let generation = 1;
    let content_key_bytes = random_key();
    let content_key = wrap_key(
        master_key,
        CONTENT_KEY_PURPOSE,
        file_id,
        generation,
        &content_key_bytes,
    )?;
    let chunk_count =
        u32::try_from(inspection.chunks.len()).map_err(|_| PortalError::TooManyChunks)?;
    let mut chunks = Vec::with_capacity(chunk_count as usize);
    for (ordinal, inspected) in (0..chunk_count).zip(inspection.chunks) {
        chunks.push(NewChunk {
            ordinal,
            envelope_version: ENVELOPE_VERSION,
            plaintext_len: inspected.plaintext_len,
            plaintext_hash: inspected.plaintext_hash,
            nonce: random_nonce(),
        });
    }
    Ok(NewUpload {
        upload_id,
        idempotency_key: idempotency_key.to_owned(),
        request_fingerprint,
        file_id,
        file_name: inspection.name,
        plaintext_hash: inspection.plaintext_hash,
        plaintext_size: inspection.plaintext_size,
        storage_class: STORAGE_CLASS.to_owned(),
        content_key,
        chunks,
    })
}

/// Validate a configured agent set and order it deterministically by id.
pub fn prepare_agents(mut agents: Vec<AgentTarget>) -> Result<Vec<AgentTarget>, PortalError> {
    validate_agent_set(&agents)?;
    agents.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(agents)
}

fn validate_agent_set(agents: &[AgentTarget]) -> Result<(), PortalError> {
    let ids: HashSet<_> = agents.iter().map(|agent| agent.id.as_str()).collect();
    let domains: HashSet<_> = agents
        .iter()
        .map(|agent| agent.failure_domain.as_str())
        .collect();
    if ids.len() != agents.len() {
        return Err(PortalError::DuplicateAgentId);
    }
    if domains.len() < MINIMUM_REPLICAS {
        return Err(PortalError::InsufficientFailureDomains);
    }
    Ok(())
}

fn validate_manifest_file(
    manifest: &crate::format::DecodedManifest,
    file: &FileRecord,
) -> Result<(), PortalError> {
    if manifest.file_id != file.file_id
        || manifest.generation != file.generation
        || manifest.payload.name != file.name
        || manifest.payload.plaintext_hash != file.plaintext_hash
        || manifest.payload.plaintext_len != file.plaintext_size
    {
        return Err(PortalError::ManifestMismatch("file metadata"));
    }
    Ok(())
}

fn read_exact_or_source_changed(
    file: &mut impl Read,
    buffer: &mut [u8],
) -> Result<(), PortalError> {
    match file.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(PortalError::SourceChanged)
        }
        Err(error) => Err(PortalError::Io(error)),
    }
}

#[derive(Deserialize)]
struct RemoteAgentIdentity {
    id: String,
    failure_domain: String,
    incarnation_id: String,
}

#[derive(Debug, Error)]
pub enum PortalError {
    #[error("agent specification must be id,failure-domain,url: {0}")]
    InvalidAgent(String),
    #[error("duplicate agent id")]
    DuplicateAgentId,
    #[error("RF2 requires at least two distinct configured failure domains")]
    InsufficientFailureDomains,
    #[error("remote agent identity does not match configuration: {0}")]
    AgentIdentityMismatch(String),
    #[error("agent {0} was not verified before placement; refusing to record an unverified copy")]
    AgentNotVerified(String),
    #[error("source is not a regular file: {0}")]
    SourceNotRegularFile(PathBuf),
    #[error("source file name is not valid UTF-8: {0}")]
    InvalidFileName(PathBuf),
    #[error("source changed after it was prehashed")]
    SourceChanged,
    #[error("idempotency key was already used for a different request")]
    IdempotencyConflict,
    #[error("persisted upload disappeared")]
    MissingUpload,
    #[error("chunk object plan is incomplete")]
    MissingChunkObject,
    #[error("persisted delete operation disappeared")]
    MissingDeleteOperation,
    #[error("file is already deleted: {0}")]
    AlreadyDeleted(Uuid),
    #[error("file is not committed or visible: {0}")]
    FileNotVisible(Uuid),
    #[error("download destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("manifest does not match committed {0}")]
    ManifestMismatch(&'static str),
    #[error("reassembled plaintext hash or length does not match the manifest")]
    PlaintextHashMismatch,
    #[error("file contains too many chunks for the v1 format")]
    TooManyChunks,
    #[error("numeric conversion overflow")]
    NumericOverflow,
    #[error("system clock is before the Unix epoch")]
    SystemClockBeforeEpoch,
    #[error("invalid failpoint: {0}")]
    InvalidFailpoint(String),
    #[error("injected failure at {}", .0.as_str())]
    InjectedFailure(Failpoint),
    #[error("agent {agent} rejected an object with HTTP {status}: {body}")]
    AgentRejected {
        agent: String,
        status: u16,
        body: String,
    },
    #[error("replica floor is not met for {kind} {hash}")]
    ReplicaFloorNotMet { kind: ObjectKind, hash: ObjectHash },
    #[error("no valid replica for {kind} {hash}: {detail}")]
    NoValidReplica {
        kind: ObjectKind,
        hash: ObjectHash,
        detail: String,
    },
    #[error(transparent)]
    Metadata(#[from] crate::metadata::MetadataError),
    #[error(transparent)]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error(transparent)]
    Format(#[from] crate::format::FormatError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn unexpected_eof_while_reading_chunks_is_source_changed() {
        let mut cursor = Cursor::new(vec![1_u8, 2, 3]);
        let mut buffer = [0_u8; 8];
        assert!(matches!(
            read_exact_or_source_changed(&mut cursor, &mut buffer),
            Err(PortalError::SourceChanged)
        ));
    }
}

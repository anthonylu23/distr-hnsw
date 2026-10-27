use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::MasterKey,
    durability::{InventoryPage, MAX_INVENTORY_PAGE},
    format::{decode_deletion_marker, decode_manifest, DecodedManifest, DeletionMarker},
    metadata::{Database, FileState},
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
};

const MINIMUM_REPLICAS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMode {
    Plan,
    Apply,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentRecoverySummary {
    pub agent_id: String,
    pub failure_domain: String,
    pub objects: usize,
    pub valid_objects: usize,
    pub corrupt_objects: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryRepair {
    pub kind: String,
    pub hash: String,
    pub source_agent: String,
    pub target_agent: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileRecoveryDecision {
    pub file_id: String,
    pub generation: u64,
    pub winner: String,
    pub decision: String,
    pub repairs: Vec<RecoveryRepair>,
    pub issues: Vec<String>,
    /// Non-blocking observations, such as a redundant deletion marker.
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RecoveryTotals {
    pub agents: usize,
    pub files: usize,
    pub converged: usize,
    pub blocked: usize,
    pub repairs_planned: usize,
    pub repairs_applied: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryReportV1 {
    pub report_type: String,
    pub version: u16,
    pub mode: RecoveryMode,
    pub inventory_digest: String,
    pub agents: Vec<AgentRecoverySummary>,
    pub files: Vec<FileRecoveryDecision>,
    pub conflicts: Vec<String>,
    pub totals: RecoveryTotals,
}

impl RecoveryReportV1 {
    pub fn exit_code(&self) -> u8 {
        if self.totals.blocked == 0 {
            0
        } else {
            2
        }
    }
}

/// Agents holding a hash-verified copy of one object. Control objects keep
/// their bytes for decoding; chunk bytes are verified and dropped, and a
/// repair re-reads them from a valid source, so recovery memory is bounded by
/// the control-object set rather than the stored data.
#[derive(Clone)]
struct ReplicaSet {
    bytes: Option<Vec<u8>>,
    valid_agents: BTreeSet<String>,
}

#[derive(Clone)]
enum Evidence {
    Manifest {
        hash: ObjectHash,
        decoded: Option<DecodedManifest>,
    },
    Deletion {
        hash: ObjectHash,
        decoded: Option<DeletionMarker>,
    },
}

impl Evidence {
    fn hash(&self) -> &ObjectHash {
        match self {
            Self::Manifest { hash, .. } | Self::Deletion { hash, .. } => hash,
        }
    }

    fn kind(&self) -> ObjectKind {
        match self {
            Self::Manifest { .. } => ObjectKind::Manifest,
            Self::Deletion { .. } => ObjectKind::DeletionMarker,
        }
    }
}

pub async fn recover(
    database: &mut Database,
    master_key: &MasterKey,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    mode: RecoveryMode,
) -> Result<RecoveryReportV1, RecoveryError> {
    let mut inventory_hasher = blake3::Hasher::new();
    inventory_hasher.update(b"distr-hnsw:recovery-inventory:v1");
    let mut listings: BTreeMap<(String, String), BTreeMap<String, u64>> = BTreeMap::new();
    let mut summaries = Vec::new();

    let mut incarnations: BTreeMap<String, String> = BTreeMap::new();
    for agent in agents {
        let incarnation_id = verify_agent(client, agent).await?;
        if mode == RecoveryMode::Apply {
            database.observe_agent_incarnation(
                &agent.id,
                &agent.failure_domain,
                &incarnation_id,
            )?;
        } else {
            database.check_agent_incarnation(&agent.id, &incarnation_id)?;
        }
        incarnations.insert(agent.id.clone(), incarnation_id);
        let mut object_count = 0_usize;
        for kind in ObjectKind::ALL {
            let mut after: Option<ObjectHash> = None;
            loop {
                let mut request = client
                    .get(format!("{}/v1/inventory/{}", agent.base_url, kind.as_str()))
                    .query(&[("limit", MAX_INVENTORY_PAGE.to_string())]);
                if let Some(cursor) = &after {
                    request = request.query(&[("after", cursor.as_str())]);
                }
                let response = request.send().await?;
                if !response.status().is_success() {
                    return Err(RecoveryError::InventoryRejected {
                        agent: agent.id.clone(),
                        status: response.status().as_u16(),
                        body: response.text().await.unwrap_or_default(),
                    });
                }
                let page: InventoryPage = response.json().await?;
                let mut previous = after.as_ref().map(ObjectHash::as_str);
                for object in &page.objects {
                    if previous.is_some_and(|value| object.hash.as_str() <= value) {
                        return Err(RecoveryError::MalformedInventory(agent.id.clone()));
                    }
                    previous = Some(object.hash.as_str());
                    object_count += 1;
                    inventory_hasher.update(agent.id.as_bytes());
                    inventory_hasher.update(kind.as_str().as_bytes());
                    inventory_hasher.update(object.hash.as_str().as_bytes());
                    inventory_hasher.update(&object.size.to_le_bytes());
                    listings
                        .entry((kind.as_str().to_owned(), object.hash.to_string()))
                        .or_default()
                        .insert(agent.id.clone(), object.size);
                }
                match page.next_after {
                    Some(cursor) if !page.objects.is_empty() => after = Some(cursor),
                    Some(_) => return Err(RecoveryError::MalformedInventory(agent.id.clone())),
                    None => break,
                }
            }
        }
        summaries.push(AgentRecoverySummary {
            agent_id: agent.id.clone(),
            failure_domain: agent.failure_domain.clone(),
            objects: object_count,
            valid_objects: 0,
            corrupt_objects: 0,
        });
    }

    let agent_map: BTreeMap<_, _> = agents
        .iter()
        .map(|agent| (agent.id.clone(), agent))
        .collect();
    let projections = database.all_file_projections()?;
    let known_control_objects: BTreeSet<(String, String)> = projections
        .iter()
        .filter_map(|projection| match projection.state {
            FileState::Deleted => projection.deletion_hash.as_ref().map(|hash| {
                (
                    ObjectKind::DeletionMarker.as_str().to_owned(),
                    hash.to_string(),
                )
            }),
            FileState::Committed | FileState::RecoveryBlocked => projection
                .manifest_hash
                .as_ref()
                .map(|hash| (ObjectKind::Manifest.as_str().to_owned(), hash.to_string())),
        })
        .collect();
    let mut replicas: BTreeMap<(String, String), ReplicaSet> = BTreeMap::new();
    for ((kind_name, hash_name), listed) in &listings {
        let kind: ObjectKind = kind_name.parse()?;
        let hash = ObjectHash::parse(hash_name.clone())?;
        let mut valid_agents = BTreeSet::new();
        let mut valid_bytes = None;
        let mut any_valid = false;
        for (agent_id, expected_size) in listed {
            let agent = agent_map
                .get(agent_id)
                .ok_or_else(|| RecoveryError::MalformedInventory(agent_id.clone()))?;
            let response = client
                .get(format!(
                    "{}/v1/objects/{}/{}",
                    agent.base_url,
                    kind.as_str(),
                    hash
                ))
                .send()
                .await?;
            let valid = if response.status().is_success() {
                let bytes = response.bytes().await?;
                ObjectHash::digest(&bytes) == hash && bytes.len() as u64 == *expected_size && {
                    if valid_bytes.is_none() && kind != ObjectKind::Chunk {
                        valid_bytes = Some(bytes.to_vec());
                    }
                    any_valid = true;
                    true
                }
            } else {
                false
            };
            let summary = summaries
                .iter_mut()
                .find(|summary| summary.agent_id == *agent_id)
                .expect("configured agent has a summary");
            if valid {
                summary.valid_objects += 1;
                valid_agents.insert(agent_id.clone());
            } else {
                summary.corrupt_objects += 1;
            }
        }
        if any_valid {
            replicas.insert(
                (kind_name.clone(), hash_name.clone()),
                ReplicaSet {
                    bytes: valid_bytes,
                    valid_agents,
                },
            );
        } else if kind != ObjectKind::Chunk
            && !known_control_objects.contains(&(kind_name.clone(), hash_name.clone()))
        {
            return Err(RecoveryError::Trust(format!(
                "no valid replica for unindexed {kind} {hash}"
            )));
        }
    }

    let mut candidates: BTreeMap<String, BTreeMap<u64, Vec<Evidence>>> = BTreeMap::new();
    for ((kind_name, hash_name), replica) in &replicas {
        let hash = ObjectHash::parse(hash_name.clone())?;
        let Some(bytes) = &replica.bytes else {
            continue;
        };
        match kind_name.as_str() {
            "manifest" => {
                let manifest = decode_manifest(master_key, bytes)
                    .map_err(|error| RecoveryError::Trust(error.to_string()))?;
                candidates
                    .entry(manifest.file_id.to_string())
                    .or_default()
                    .entry(manifest.generation)
                    .or_default()
                    .push(Evidence::Manifest {
                        hash,
                        decoded: Some(manifest),
                    });
            }
            "deletion_marker" => {
                let marker = decode_deletion_marker(master_key, bytes)
                    .map_err(|error| RecoveryError::Trust(error.to_string()))?;
                candidates
                    .entry(marker.file_id.to_string())
                    .or_default()
                    .entry(marker.generation)
                    .or_default()
                    .push(Evidence::Deletion {
                        hash,
                        decoded: Some(marker),
                    });
            }
            _ => {}
        }
    }
    for projection in projections {
        let evidence = match projection.state {
            FileState::Deleted => projection.deletion_hash.map(|hash| Evidence::Deletion {
                hash,
                decoded: None,
            }),
            FileState::Committed | FileState::RecoveryBlocked => {
                projection.manifest_hash.map(|hash| Evidence::Manifest {
                    hash,
                    decoded: None,
                })
            }
        };
        if let Some(evidence) = evidence {
            let values = candidates
                .entry(projection.file_id.to_string())
                .or_default()
                .entry(projection.generation)
                .or_default();
            if !values
                .iter()
                .any(|value| value.kind() == evidence.kind() && value.hash() == evidence.hash())
            {
                values.push(evidence);
            }
        }
    }

    let mut file_reports = Vec::new();
    let mut conflicts = Vec::new();
    let mut totals = RecoveryTotals {
        agents: agents.len(),
        files: candidates.len(),
        ..RecoveryTotals::default()
    };
    let mut repairer = RepairContext {
        database,
        client,
        agents,
        incarnations: &incarnations,
        replicas: &mut replicas,
        mode,
    };
    let committed_deletions: BTreeMap<String, ObjectHash> = repairer
        .database
        .all_file_projections()?
        .into_iter()
        .filter(|projection| projection.state == FileState::Deleted)
        .filter_map(|projection| {
            projection
                .deletion_hash
                .map(|hash| (projection.file_id.to_string(), hash))
        })
        .collect();
    for (file_id_text, generations) in candidates {
        let file_id = Uuid::parse_str(&file_id_text)
            .map_err(|_| RecoveryError::Trust("invalid file id in recovery evidence".to_owned()))?;
        let (&generation, evidence) = generations
            .last_key_value()
            .expect("candidate generation map is nonempty");
        let mut issues = Vec::new();
        let mut notes = Vec::new();
        let mut repairs = Vec::new();
        let unique: BTreeSet<_> = evidence
            .iter()
            .map(|item| (item.kind().as_str(), item.hash().as_str()))
            .collect();
        let kinds: BTreeSet<_> = evidence.iter().map(Evidence::kind).collect();
        // Several distinct deletion markers at one generation all say the
        // same thing (the file is deleted); earlier releases could mint a
        // second one when an abandoned delete was retried under a new key.
        // Pick one deterministically instead of blocking: the marker the
        // projection committed, otherwise the earliest deletion, then hash.
        let redundant_markers =
            unique.len() > 1 && kinds.len() == 1 && kinds.contains(&ObjectKind::DeletionMarker);
        let conflicted = unique.len() > 1 && !redundant_markers;
        if conflicted {
            let issue = format!("conflicting immutable evidence at generation {generation}");
            conflicts.push(format!("{file_id_text}: {issue}"));
            issues.push(issue);
        }

        let winner = if redundant_markers {
            let committed = committed_deletions.get(&file_id_text);
            let chosen = evidence
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        Evidence::Deletion {
                            decoded: Some(_),
                            ..
                        }
                    )
                })
                .min_by_key(|item| {
                    let deleted_at = match item {
                        Evidence::Deletion {
                            decoded: Some(marker),
                            ..
                        } => marker.deleted_at,
                        _ => i64::MAX,
                    };
                    (
                        Some(item.hash()) != committed,
                        deleted_at,
                        item.hash().to_string(),
                    )
                })
                .unwrap_or_else(|| evidence.first().expect("winning evidence exists"))
                .clone();
            notes.push(format!(
                "{} deletion markers at generation {generation}; kept {}",
                unique.len(),
                chosen.hash()
            ));
            chosen
        } else {
            evidence.first().expect("winning evidence exists").clone()
        };
        if !conflicted {
            match &winner {
                Evidence::Manifest { hash, decoded } => {
                    if let Some(manifest) = decoded {
                        repairer
                            .ensure_rf2(ObjectKind::Manifest, hash, &mut repairs, &mut issues)
                            .await?;
                        for chunk in &manifest.payload.chunks {
                            repairer
                                .ensure_rf2(
                                    ObjectKind::Chunk,
                                    &chunk.ciphertext_hash,
                                    &mut repairs,
                                    &mut issues,
                                )
                                .await?;
                        }
                    } else {
                        issues.push("winning manifest is absent from agent inventories".to_owned());
                    }
                }
                Evidence::Deletion { hash, decoded } => {
                    if decoded.is_some() {
                        repairer
                            .ensure_rf2(ObjectKind::DeletionMarker, hash, &mut repairs, &mut issues)
                            .await?;
                    } else {
                        issues.push(
                            "winning deletion marker is absent from agent inventories".to_owned(),
                        );
                    }
                }
            }
        }

        let blocked = !issues.is_empty();
        if mode == RecoveryMode::Apply {
            if blocked {
                repairer.database.mark_recovery_blocked(
                    file_id,
                    generation,
                    match (&winner, conflicted) {
                        (_, true) => None,
                        (Evidence::Deletion { hash, .. }, false) => Some(hash),
                        (Evidence::Manifest { .. }, false) => None,
                    },
                    &issues.join("; "),
                )?;
            } else {
                match &winner {
                    Evidence::Manifest {
                        hash,
                        decoded: Some(manifest),
                    } => {
                        let bytes = repairer
                            .replicas
                            .get(&(ObjectKind::Manifest.as_str().to_owned(), hash.to_string()))
                            .and_then(|replica| replica.bytes.clone())
                            .expect("a decoded manifest keeps its verified bytes");
                        repairer
                            .database
                            .apply_recovered_manifest(manifest, hash, &bytes)?
                    }
                    Evidence::Deletion {
                        hash,
                        decoded: Some(marker),
                    } => repairer.database.apply_recovered_deletion(marker, hash)?,
                    _ => unreachable!("unblocked evidence has decoded bytes"),
                }
            }
        }
        totals.repairs_planned += repairs.len();
        totals.repairs_applied += repairs
            .iter()
            .filter(|repair| repair.status == "applied")
            .count();
        if blocked {
            totals.blocked += 1;
        } else {
            totals.converged += 1;
        }
        file_reports.push(FileRecoveryDecision {
            file_id: file_id_text,
            generation,
            winner: winner.kind().as_str().to_owned(),
            decision: if blocked {
                "blocked".to_owned()
            } else if mode == RecoveryMode::Apply {
                "applied".to_owned()
            } else {
                "planned".to_owned()
            },
            repairs,
            issues,
            notes,
        });
    }
    // Every control object on the agents is now reflected in the projection
    // (applied or recorded as blocked), so a restored database is reconciled.
    if mode == RecoveryMode::Apply {
        repairer.database.clear_recovery_pending()?;
    }

    Ok(RecoveryReportV1 {
        report_type: "RecoveryReportV1".to_owned(),
        version: 1,
        mode,
        inventory_digest: inventory_hasher.finalize().to_hex().to_string(),
        agents: summaries,
        files: file_reports,
        conflicts,
        totals,
    })
}

struct RepairContext<'a> {
    database: &'a mut Database,
    client: &'a reqwest::Client,
    agents: &'a [AgentTarget],
    incarnations: &'a BTreeMap<String, String>,
    replicas: &'a mut BTreeMap<(String, String), ReplicaSet>,
    mode: RecoveryMode,
}

fn incarnation_of(
    incarnations: &BTreeMap<String, String>,
    agent_id: &str,
) -> Result<String, RecoveryError> {
    incarnations
        .get(agent_id)
        .cloned()
        .ok_or_else(|| RecoveryError::AgentIdentity(agent_id.to_owned()))
}

impl RepairContext<'_> {
    async fn ensure_rf2(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        repairs: &mut Vec<RecoveryRepair>,
        issues: &mut Vec<String>,
    ) -> Result<(), RecoveryError> {
        let key = (kind.as_str().to_owned(), hash.to_string());
        let Some(replica) = self.replicas.get_mut(&key) else {
            issues.push(format!("missing required {kind} {hash}"));
            return Ok(());
        };
        let source = replica.valid_agents.iter().next().cloned();
        let Some(source) = source else {
            issues.push(format!("no valid replica for required {kind} {hash}"));
            return Ok(());
        };
        let mut domains: BTreeSet<String> = self
            .agents
            .iter()
            .filter(|agent| replica.valid_agents.contains(&agent.id))
            .map(|agent| agent.failure_domain.clone())
            .collect();
        for target in self.agents {
            if domains.len() >= MINIMUM_REPLICAS {
                break;
            }
            if replica.valid_agents.contains(&target.id) || domains.contains(&target.failure_domain)
            {
                continue;
            }
            let mut repair = RecoveryRepair {
                kind: kind.as_str().to_owned(),
                hash: hash.to_string(),
                source_agent: source.clone(),
                target_agent: target.id.clone(),
                status: "planned".to_owned(),
            };
            if self.mode == RecoveryMode::Apply {
                let bytes = match &replica.bytes {
                    Some(bytes) => Some(bytes.clone()),
                    None => {
                        fetch_from(self.client, self.agents, &replica.valid_agents, kind, hash)
                            .await
                    }
                };
                let Some(bytes) = bytes else {
                    issues.push(format!(
                        "no valid source could be read for {kind} {hash} during repair"
                    ));
                    repairs.push(repair);
                    continue;
                };
                let response = match self
                    .client
                    .put(format!(
                        "{}/v1/objects/{}/{}",
                        target.base_url,
                        kind.as_str(),
                        hash
                    ))
                    .body(bytes)
                    .send()
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        issues.push(format!(
                            "repair target {} failed for {kind} {hash}: {error}",
                            target.id
                        ));
                        repairs.push(repair);
                        continue;
                    }
                };
                if !response.status().is_success() {
                    issues.push(format!(
                        "repair target {} rejected {kind} {hash}",
                        target.id
                    ));
                    repairs.push(repair);
                    continue;
                }
                // Copy-first protocol: the destination counts only after a
                // read-back proves the bytes landed intact.
                if !read_back_matches(self.client, target, kind, hash).await {
                    issues.push(format!(
                        "repair target {} did not read back {kind} {hash} intact",
                        target.id
                    ));
                    repairs.push(repair);
                    continue;
                }
                let incarnation_id = incarnation_of(self.incarnations, &target.id)?;
                self.database.ensure_pending_placement(
                    kind,
                    hash,
                    &target.id,
                    &target.failure_domain,
                    &incarnation_id,
                )?;
                self.database.confirm_placement(kind, hash, &target.id)?;
                replica.valid_agents.insert(target.id.clone());
                domains.insert(target.failure_domain.clone());
                repair.status = "applied".to_owned();
            } else {
                domains.insert(target.failure_domain.clone());
            }
            repairs.push(repair);
        }
        if domains.len() < MINIMUM_REPLICAS {
            issues.push(format!("required {kind} {hash} cannot reach RF2"));
        }
        if self.mode == RecoveryMode::Apply {
            for agent_id in &replica.valid_agents {
                if let Some(agent) = self.agents.iter().find(|agent| agent.id == *agent_id) {
                    let incarnation_id = incarnation_of(self.incarnations, &agent.id)?;
                    self.database.ensure_pending_placement(
                        kind,
                        hash,
                        &agent.id,
                        &agent.failure_domain,
                        &incarnation_id,
                    )?;
                    self.database.confirm_placement(kind, hash, &agent.id)?;
                }
            }
        }
        Ok(())
    }
}

/// Read a hash-verified copy of an object from any of `holders`.
async fn fetch_from(
    client: &reqwest::Client,
    agents: &[AgentTarget],
    holders: &BTreeSet<String>,
    kind: ObjectKind,
    hash: &ObjectHash,
) -> Option<Vec<u8>> {
    for agent in agents.iter().filter(|agent| holders.contains(&agent.id)) {
        let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
        if let Ok(response) = client.get(url).send().await {
            if response.status().is_success() {
                if let Ok(bytes) = response.bytes().await {
                    if ObjectHash::digest(&bytes) == *hash {
                        return Some(bytes.to_vec());
                    }
                }
            }
        }
    }
    None
}

async fn read_back_matches(
    client: &reqwest::Client,
    agent: &AgentTarget,
    kind: ObjectKind,
    hash: &ObjectHash,
) -> bool {
    let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(bytes) => ObjectHash::digest(&bytes) == *hash,
            Err(_) => false,
        },
        _ => false,
    }
}

async fn verify_agent(
    client: &reqwest::Client,
    agent: &AgentTarget,
) -> Result<String, RecoveryError> {
    #[derive(serde::Deserialize)]
    struct Identity {
        id: String,
        failure_domain: String,
        incarnation_id: String,
    }
    let response = client
        .get(format!("{}/v1/health", agent.base_url))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(RecoveryError::InventoryRejected {
            agent: agent.id.clone(),
            status: response.status().as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }
    let identity: Identity = response.json().await?;
    if identity.id != agent.id || identity.failure_domain != agent.failure_domain {
        return Err(RecoveryError::AgentIdentity(agent.id.clone()));
    }
    Ok(identity.incarnation_id)
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("agent identity does not match recovery configuration: {0}")]
    AgentIdentity(String),
    #[error("agent {agent} rejected inventory with HTTP {status}: {body}")]
    InventoryRejected {
        agent: String,
        status: u16,
        body: String,
    },
    #[error("agent returned malformed or non-monotonic inventory: {0}")]
    MalformedInventory(String),
    #[error("recovery scan cannot be trusted: {0}")]
    Trust(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Metadata(#[from] crate::metadata::MetadataError),
    #[error(transparent)]
    Object(#[from] crate::object::ObjectError),
}

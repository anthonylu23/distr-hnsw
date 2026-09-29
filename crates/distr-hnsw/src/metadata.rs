use std::{path::Path, str::FromStr};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{WrappedKey, ENVELOPE_VERSION, NONCE_LEN},
    durability::{ensure_directory, sync_directory},
    object::{ObjectHash, ObjectKind},
};

const SCHEMA_VERSION: i64 = 6;
const MASTER_KEY_ID_META: &str = "master_key_id";
const CONTENT_GENERATION_META: &str = "content_generation";

/// SQL predicate selecting placements that count toward the live durability
/// floor: confirmed rows whose agent incarnation is still active. Legacy rows
/// without an incarnation count until the agent is observed and adopts them.
const LIVE_CONFIRMED: &str =
    "state = 'confirmed' AND (incarnation_id IS NULL OR incarnation_id IN (
    SELECT incarnation_id FROM agent_incarnations WHERE status = 'active'))";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadState {
    Staging,
    ReplicatingChunks,
    ReplicatingManifest,
    Committed,
}

impl UploadState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::ReplicatingChunks => "replicating_chunks",
            Self::ReplicatingManifest => "replicating_manifest",
            Self::Committed => "committed",
        }
    }

    pub fn permits(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Staging, Self::ReplicatingChunks)
                    | (Self::ReplicatingChunks, Self::ReplicatingManifest)
                    | (Self::ReplicatingManifest, Self::Committed)
            )
    }
}

impl FromStr for UploadState {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "staging" => Ok(Self::Staging),
            "replicating_chunks" => Ok(Self::ReplicatingChunks),
            "replicating_manifest" => Ok(Self::ReplicatingManifest),
            "committed" => Ok(Self::Committed),
            _ => Err(MetadataError::InvalidState(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewUpload {
    pub upload_id: Uuid,
    pub idempotency_key: String,
    pub request_fingerprint: [u8; 32],
    pub file_id: Uuid,
    pub file_name: String,
    pub plaintext_hash: [u8; 32],
    pub plaintext_size: u64,
    pub storage_class: String,
    pub content_key: WrappedKey,
    pub chunks: Vec<NewChunk>,
}

#[derive(Clone, Debug)]
pub struct NewChunk {
    pub ordinal: u32,
    pub envelope_version: u16,
    pub plaintext_len: u32,
    pub plaintext_hash: [u8; 32],
    pub nonce: [u8; NONCE_LEN],
}

#[derive(Clone, Debug)]
pub struct UploadRecord {
    pub upload_id: Uuid,
    pub idempotency_key: String,
    pub request_fingerprint: [u8; 32],
    pub file_id: Uuid,
    pub file_name: String,
    pub plaintext_hash: [u8; 32],
    pub plaintext_size: u64,
    pub storage_class: String,
    pub content_key: WrappedKey,
    pub state: UploadState,
    pub generation: u64,
    pub manifest_hash: Option<ObjectHash>,
    pub manifest_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct ChunkPlan {
    pub ordinal: u32,
    pub envelope_version: u16,
    pub plaintext_len: u32,
    pub plaintext_hash: [u8; 32],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext_hash: Option<ObjectHash>,
    pub ciphertext_len: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct FileRecord {
    pub file_id: Uuid,
    pub generation: u64,
    pub name: String,
    pub plaintext_hash: [u8; 32],
    pub plaintext_size: u64,
    pub manifest_hash: ObjectHash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileState {
    Committed,
    Deleted,
    RecoveryBlocked,
}

impl FileState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Deleted => "deleted",
            Self::RecoveryBlocked => "recovery_blocked",
        }
    }
}

impl FromStr for FileState {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "committed" => Ok(Self::Committed),
            "deleted" => Ok(Self::Deleted),
            "recovery_blocked" => Ok(Self::RecoveryBlocked),
            _ => Err(MetadataError::InvalidFileState(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileProjection {
    pub file_id: Uuid,
    pub generation: u64,
    pub state: FileState,
    pub name: Option<String>,
    pub plaintext_hash: Option<[u8; 32]>,
    pub plaintext_size: Option<u64>,
    pub manifest_hash: Option<ObjectHash>,
    pub deletion_hash: Option<ObjectHash>,
    pub deleted_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteState {
    ReplicatingMarker,
    Committed,
}

impl DeleteState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReplicatingMarker => "replicating_marker",
            Self::Committed => "committed",
        }
    }
}

impl FromStr for DeleteState {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "replicating_marker" => Ok(Self::ReplicatingMarker),
            "committed" => Ok(Self::Committed),
            _ => Err(MetadataError::InvalidDeleteState(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewDeleteOperation {
    pub operation_id: Uuid,
    pub idempotency_key: String,
    pub file_id: Uuid,
    pub generation: u64,
    pub deleted_at: i64,
    pub marker_hash: ObjectHash,
    pub marker_bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct DeleteOperation {
    pub operation_id: Uuid,
    pub idempotency_key: String,
    pub file_id: Uuid,
    pub generation: u64,
    pub deleted_at: i64,
    pub marker_hash: ObjectHash,
    pub marker_bytes: Vec<u8>,
    pub state: DeleteState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementState {
    Pending,
    Confirmed,
    Missing,
    Corrupt,
}

impl PlacementState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
        }
    }
}

impl FromStr for PlacementState {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "confirmed" => Ok(Self::Confirmed),
            "missing" => Ok(Self::Missing),
            "corrupt" => Ok(Self::Corrupt),
            _ => Err(MetadataError::InvalidPlacementState(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IncarnationStatus {
    Active,
    Superseded,
    Retired,
}

impl IncarnationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Retired => "retired",
        }
    }
}

impl FromStr for IncarnationStatus {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "retired" => Ok(Self::Retired),
            _ => Err(MetadataError::InvalidIncarnationStatus(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct AgentIncarnation {
    pub agent_id: String,
    pub incarnation_id: String,
    pub failure_domain: String,
    pub status: IncarnationStatus,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub superseded_by: Option<String>,
}

/// Outcome of recording an agent's reported incarnation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum IncarnationObservation {
    /// The active incarnation was seen again.
    Known,
    /// First incarnation ever recorded for this agent id. Legacy placement
    /// rows without an incarnation were attributed to it.
    Adopted { adopted_placements: usize },
    /// A different incarnation replaced the previously active one. The old
    /// incarnation's placements no longer count toward durability.
    Superseded { previous: String },
}

/// Outcome of binding a master key identifier to the database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyBinding {
    /// The database had no bound key and now records this one.
    Bound,
    /// The presented key matches the bound identifier.
    Matched,
}

/// The agent and incarnation a placement row belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacementTarget {
    pub agent_id: String,
    pub failure_domain: String,
    pub incarnation_id: String,
}

/// The latest complete inventory observation of one namespace on one active
/// incarnation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CompleteScan {
    pub agent_id: String,
    pub incarnation_id: String,
    pub kind: ObjectKind,
    pub inventory_digest: String,
    pub completed_at: i64,
}

/// A SQLite snapshot shipped to a backup target.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct BackupSnapshot {
    pub name: String,
    pub snapshot_hash: String,
    pub content_generation: i64,
    pub size: u64,
    pub created_at: i64,
}

/// One immutable object referenced by any generation in history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryObject {
    pub kind: ObjectKind,
    pub hash: ObjectHash,
    pub expected_len: Option<u64>,
    /// When the owning file projection last changed; used for backup lag.
    pub committed_at: i64,
}

/// One object the current file projection requires to be durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredObject {
    pub kind: ObjectKind,
    pub hash: ObjectHash,
    pub file_id: Uuid,
    pub generation: u64,
    pub expected_len: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobMode {
    Verify,
    Repair,
}

impl JobMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Repair => "repair",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Complete,
    Failed,
    Interrupted,
}

impl JobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

impl FromStr for JobStatus {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "running" => Ok(Self::Running),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(MetadataError::InvalidJobStatus(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ReconcileJob {
    pub job_id: Uuid,
    pub mode: JobMode,
    pub status: JobStatus,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub report: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    Running,
    Complete,
    Failed,
}

impl ScanStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// A complete inventory observation of one namespace on one incarnation.
#[derive(Clone, Debug)]
pub struct ScanCompletion {
    pub scan_id: Uuid,
    pub final_cursor: Option<ObjectHash>,
    pub object_count: u64,
    pub inventory_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectHealthState {
    Durable,
    Degraded,
    AtRisk,
    Lost,
}

impl ObjectHealthState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Degraded => "degraded",
            Self::AtRisk => "at_risk",
            Self::Lost => "lost",
        }
    }
}

impl FromStr for ObjectHealthState {
    type Err = MetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "durable" => Ok(Self::Durable),
            "degraded" => Ok(Self::Degraded),
            "at_risk" => Ok(Self::AtRisk),
            "lost" => Ok(Self::Lost),
            _ => Err(MetadataError::InvalidHealthState(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ObjectHealth {
    pub kind: ObjectKind,
    pub hash: ObjectHash,
    pub file_id: Uuid,
    pub generation: u64,
    pub verified_copies: usize,
    pub verified_domains: usize,
    pub state: ObjectHealthState,
    pub job_id: Uuid,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct ObjectHealthSummary {
    pub durable: usize,
    pub degraded: usize,
    pub at_risk: usize,
    pub lost: usize,
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, MetadataError> {
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => apply_schema(&connection)?,
            1 => {
                migrate_v1_to_v3(&mut connection)?;
                migrate_v3_to_v4(&mut connection)?;
            }
            2 => {
                migrate_v2_to_v3(&mut connection)?;
                migrate_v3_to_v4(&mut connection)?;
            }
            3 => migrate_v3_to_v4(&mut connection)?,
            4 | 5 => apply_schema(&connection)?,
            SCHEMA_VERSION => apply_schema(&connection)?,
            _ => return Err(MetadataError::UnsupportedSchemaVersion(version)),
        }
        let mut database = Self { connection };
        database.interrupt_running_jobs()?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(database)
    }

    /// Bind this database to a master key by its non-secret identifier. The
    /// first key to open an unbound (or pre-v5) database binds it; any later
    /// key must match, and a mismatch fails before anything is decrypted.
    pub fn bind_master_key_id(&mut self, key_id_hex: &str) -> Result<KeyBinding, MetadataError> {
        let transaction = self.connection.transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT value FROM portal_meta WHERE key = ?1",
                [MASTER_KEY_ID_META],
                |row| row.get(0),
            )
            .optional()?;
        let outcome = match existing {
            Some(bound) if bound == key_id_hex => KeyBinding::Matched,
            Some(bound) => {
                return Err(MetadataError::MasterKeyMismatch {
                    bound,
                    presented: key_id_hex.to_owned(),
                });
            }
            None => {
                transaction.execute(
                    "INSERT INTO portal_meta (key, value) VALUES (?1, ?2)",
                    params![MASTER_KEY_ID_META, key_id_hex],
                )?;
                KeyBinding::Bound
            }
        };
        transaction.commit()?;
        Ok(outcome)
    }

    /// Monotonic counter bumped by every file-visible mutation (commit,
    /// delete, recovery apply). Backup uses it to skip snapshots when no
    /// content changed.
    pub fn content_generation(&self) -> Result<i64, MetadataError> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM portal_meta WHERE key = ?1",
                [CONTENT_GENERATION_META],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|text| text.parse().ok()).unwrap_or(0))
    }

    pub fn master_key_id(&self) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT value FROM portal_meta WHERE key = ?1",
                [MASTER_KEY_ID_META],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn create_upload(&mut self, upload: &NewUpload) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO uploads (
                upload_id, idempotency_key, request_fingerprint, file_id,
                file_name, plaintext_hash, plaintext_size, storage_class,
                content_key_nonce, wrapped_content_key, state, generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'staging', 1)",
            params![
                upload.upload_id.to_string(),
                upload.idempotency_key,
                upload.request_fingerprint.as_slice(),
                upload.file_id.to_string(),
                upload.file_name,
                upload.plaintext_hash.as_slice(),
                to_i64(upload.plaintext_size)?,
                upload.storage_class,
                upload.content_key.nonce.as_slice(),
                upload.content_key.ciphertext,
            ],
        )?;
        for chunk in &upload.chunks {
            if chunk.envelope_version != ENVELOPE_VERSION {
                return Err(MetadataError::UnsupportedEnvelopeVersion(
                    chunk.envelope_version,
                ));
            }
            transaction.execute(
                "INSERT INTO upload_chunks
                 (upload_id, ordinal, envelope_version, plaintext_len, plaintext_hash, nonce)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    upload.upload_id.to_string(),
                    chunk.ordinal,
                    chunk.envelope_version,
                    chunk.plaintext_len,
                    chunk.plaintext_hash.as_slice(),
                    chunk.nonce.as_slice(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn upload_by_idempotency(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<UploadRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT upload_id, idempotency_key, request_fingerprint, file_id,
                        file_name, plaintext_hash, plaintext_size, storage_class,
                        content_key_nonce, wrapped_content_key, state, generation,
                        manifest_hash, manifest_bytes
                 FROM uploads WHERE idempotency_key = ?1",
                [idempotency_key],
                row_to_upload,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn chunks(&self, upload_id: Uuid) -> Result<Vec<ChunkPlan>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT ordinal, envelope_version, plaintext_len, plaintext_hash, nonce,
                    ciphertext_hash, ciphertext_len
             FROM upload_chunks WHERE upload_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([upload_id.to_string()], |row| {
            let plaintext_hash: Vec<u8> = row.get(3)?;
            let nonce: Vec<u8> = row.get(4)?;
            let hash: Option<String> = row.get(5)?;
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, u16>(1)?,
                row.get::<_, u32>(2)?,
                plaintext_hash,
                nonce,
                hash,
                row.get::<_, Option<u32>>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (
                ordinal,
                envelope_version,
                plaintext_len,
                plaintext_hash,
                nonce,
                hash,
                ciphertext_len,
            ) = row?;
            Ok(ChunkPlan {
                ordinal,
                envelope_version,
                plaintext_len,
                plaintext_hash: fixed_bytes(&plaintext_hash, "chunk plaintext hash")?,
                nonce: fixed_bytes(&nonce, "chunk nonce")?,
                ciphertext_hash: hash.map(ObjectHash::parse).transpose()?,
                ciphertext_len,
            })
        })
        .collect()
    }

    pub fn advance_upload(
        &mut self,
        upload_id: Uuid,
        next: UploadState,
    ) -> Result<(), MetadataError> {
        let current: String = self.connection.query_row(
            "SELECT state FROM uploads WHERE upload_id = ?1",
            [upload_id.to_string()],
            |row| row.get(0),
        )?;
        let current = UploadState::from_str(&current)?;
        if !current.permits(next) {
            return Err(MetadataError::IllegalTransition { current, next });
        }
        self.connection.execute(
            "UPDATE uploads SET state = ?2 WHERE upload_id = ?1",
            params![upload_id.to_string(), next.as_str()],
        )?;
        Ok(())
    }

    pub fn set_chunk_object(
        &mut self,
        upload_id: Uuid,
        ordinal: u32,
        hash: &ObjectHash,
        ciphertext_len: u32,
    ) -> Result<(), MetadataError> {
        let existing: (Option<String>, Option<u32>) = self.connection.query_row(
            "SELECT ciphertext_hash, ciphertext_len FROM upload_chunks
             WHERE upload_id = ?1 AND ordinal = ?2",
            params![upload_id.to_string(), ordinal],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if let Some(existing_hash) = existing.0 {
            if existing_hash != hash.as_str() || existing.1 != Some(ciphertext_len) {
                return Err(MetadataError::PlanConflict);
            }
            return Ok(());
        }
        self.connection.execute(
            "UPDATE upload_chunks SET ciphertext_hash = ?3, ciphertext_len = ?4
             WHERE upload_id = ?1 AND ordinal = ?2",
            params![
                upload_id.to_string(),
                ordinal,
                hash.as_str(),
                ciphertext_len
            ],
        )?;
        Ok(())
    }

    /// Record a pending placement for the agent's current incarnation. A row
    /// left by a superseded incarnation is reset to pending so the object is
    /// written again; a legacy row without an incarnation is adopted as is.
    pub fn ensure_pending_placement(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        agent_id: &str,
        failure_domain: &str,
        incarnation_id: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO placements
             (object_kind, object_hash, agent_id, failure_domain, incarnation_id, state)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')
             ON CONFLICT(object_kind, object_hash, agent_id) DO UPDATE SET
                 failure_domain = excluded.failure_domain,
                 incarnation_id = excluded.incarnation_id,
                 state = CASE
                     WHEN placements.incarnation_id IS NULL
                       OR placements.incarnation_id = excluded.incarnation_id
                     THEN placements.state ELSE 'pending' END,
                 confirmed_at = CASE
                     WHEN placements.incarnation_id IS NULL
                       OR placements.incarnation_id = excluded.incarnation_id
                     THEN placements.confirmed_at ELSE NULL END,
                 last_verified_at = CASE
                     WHEN placements.incarnation_id IS NULL
                       OR placements.incarnation_id = excluded.incarnation_id
                     THEN placements.last_verified_at ELSE NULL END,
                 last_verified_job = CASE
                     WHEN placements.incarnation_id IS NULL
                       OR placements.incarnation_id = excluded.incarnation_id
                     THEN placements.last_verified_job ELSE NULL END
             WHERE placements.incarnation_id IS NOT excluded.incarnation_id",
            params![
                kind.as_str(),
                hash.as_str(),
                agent_id,
                failure_domain,
                incarnation_id
            ],
        )?;
        Ok(())
    }

    /// Persist the verification outcome of one placement. Verification never
    /// removes rows; it only changes their state.
    pub fn set_placement_verification(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        target: &PlacementTarget,
        state: PlacementState,
        job_id: Uuid,
    ) -> Result<(), MetadataError> {
        if state == PlacementState::Pending {
            return Err(MetadataError::InvalidPlacementState(
                "verification cannot reset a placement to pending".to_owned(),
            ));
        }
        self.ensure_pending_placement(
            kind,
            hash,
            &target.agent_id,
            &target.failure_domain,
            &target.incarnation_id,
        )?;
        self.connection.execute(
            "UPDATE placements
             SET state = ?4,
                 confirmed_at = CASE WHEN ?4 = 'confirmed'
                     THEN COALESCE(confirmed_at, unixepoch()) ELSE confirmed_at END,
                 last_verified_at = unixepoch(),
                 last_verified_job = ?5
             WHERE object_kind = ?1 AND object_hash = ?2 AND agent_id = ?3",
            params![
                kind.as_str(),
                hash.as_str(),
                target.agent_id,
                state.as_str(),
                job_id.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn placement_states(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<Vec<(String, PlacementState, Option<String>)>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT agent_id, state, incarnation_id FROM placements
             WHERE object_kind = ?1 AND object_hash = ?2 ORDER BY agent_id",
        )?;
        let rows = statement
            .query_map(params![kind.as_str(), hash.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(agent, state, incarnation)| {
                Ok((agent, PlacementState::from_str(&state)?, incarnation))
            })
            .collect()
    }

    /// Record the incarnation an agent reports on contact. See
    /// [`IncarnationObservation`] for the outcomes. A retired incarnation
    /// attempting to rejoin fails closed.
    pub fn observe_agent_incarnation(
        &mut self,
        agent_id: &str,
        failure_domain: &str,
        incarnation_id: &str,
    ) -> Result<IncarnationObservation, MetadataError> {
        let transaction = self.connection.transaction()?;
        let observed_status: Option<String> = transaction
            .query_row(
                "SELECT status FROM agent_incarnations WHERE incarnation_id = ?1",
                [incarnation_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(status) = &observed_status {
            match IncarnationStatus::from_str(status)? {
                IncarnationStatus::Retired => {
                    return Err(MetadataError::RetiredIncarnation {
                        agent_id: agent_id.to_owned(),
                        incarnation_id: incarnation_id.to_owned(),
                    });
                }
                IncarnationStatus::Superseded => {
                    return Err(MetadataError::SupersededIncarnation {
                        agent_id: agent_id.to_owned(),
                        incarnation_id: incarnation_id.to_owned(),
                    });
                }
                IncarnationStatus::Active => {}
            }
        }
        let active: Option<(String, String)> = transaction
            .query_row(
                "SELECT incarnation_id, failure_domain FROM agent_incarnations
                 WHERE agent_id = ?1 AND status = 'active'",
                [agent_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let outcome = match active {
            Some((current, _)) if current == incarnation_id => {
                transaction.execute(
                    "UPDATE agent_incarnations
                     SET last_seen_at = unixepoch(), failure_domain = ?2
                     WHERE incarnation_id = ?1",
                    params![incarnation_id, failure_domain],
                )?;
                IncarnationObservation::Known
            }
            Some((previous, _)) => {
                transaction.execute(
                    "UPDATE agent_incarnations
                     SET status = 'superseded', superseded_by = ?2, last_seen_at = unixepoch()
                     WHERE incarnation_id = ?1",
                    params![previous, incarnation_id],
                )?;
                transaction.execute(
                    "INSERT INTO agent_incarnations
                     (incarnation_id, agent_id, failure_domain, status)
                     VALUES (?1, ?2, ?3, 'active')",
                    params![incarnation_id, agent_id, failure_domain],
                )?;
                IncarnationObservation::Superseded { previous }
            }
            None => {
                transaction.execute(
                    "INSERT INTO agent_incarnations
                     (incarnation_id, agent_id, failure_domain, status)
                     VALUES (?1, ?2, ?3, 'active')",
                    params![incarnation_id, agent_id, failure_domain],
                )?;
                let adopted = transaction.execute(
                    "UPDATE placements SET incarnation_id = ?2
                     WHERE agent_id = ?1 AND incarnation_id IS NULL",
                    params![agent_id, incarnation_id],
                )?;
                IncarnationObservation::Adopted {
                    adopted_placements: adopted,
                }
            }
        };
        transaction.commit()?;
        Ok(outcome)
    }

    pub fn agent_incarnations(&self) -> Result<Vec<AgentIncarnation>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT agent_id, incarnation_id, failure_domain, status, first_seen_at,
                    last_seen_at, superseded_by
             FROM agent_incarnations ORDER BY agent_id, first_seen_at, incarnation_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(agent_id, incarnation_id, failure_domain, status, first, last, by)| {
                    Ok(AgentIncarnation {
                        agent_id,
                        incarnation_id,
                        failure_domain,
                        status: IncarnationStatus::from_str(&status)?,
                        first_seen_at: first,
                        last_seen_at: last,
                        superseded_by: by,
                    })
                },
            )
            .collect()
    }

    /// Objects the current projection requires: the manifest and every chunk of
    /// each committed file and the marker of each deleted file. Files blocked
    /// in recovery are owned by `recover` and are not listed.
    pub fn required_objects(&self) -> Result<Vec<RequiredObject>, MetadataError> {
        let mut required = Vec::new();
        for projection in self.all_file_projections()? {
            match projection.state {
                FileState::Committed => {
                    let manifest_hash = projection
                        .manifest_hash
                        .ok_or(MetadataError::MissingManifest)?;
                    required.push(RequiredObject {
                        kind: ObjectKind::Manifest,
                        hash: manifest_hash,
                        file_id: projection.file_id,
                        generation: projection.generation,
                        expected_len: None,
                    });
                    let mut statement = self.connection.prepare(
                        "SELECT ciphertext_hash, ciphertext_len FROM file_chunks
                         WHERE file_id = ?1 AND generation = ?2 ORDER BY ordinal",
                    )?;
                    let chunks = statement
                        .query_map(
                            params![
                                projection.file_id.to_string(),
                                to_i64(projection.generation)?
                            ],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                        )?
                        .collect::<Result<Vec<_>, _>>()?;
                    for (hash, len) in chunks {
                        required.push(RequiredObject {
                            kind: ObjectKind::Chunk,
                            hash: ObjectHash::parse(hash)?,
                            file_id: projection.file_id,
                            generation: projection.generation,
                            expected_len: Some(
                                u64::try_from(len).map_err(|_| MetadataError::NumericOverflow)?,
                            ),
                        });
                    }
                }
                FileState::Deleted => {
                    let marker = projection
                        .deletion_hash
                        .ok_or(MetadataError::MissingDeletionMarker)?;
                    required.push(RequiredObject {
                        kind: ObjectKind::DeletionMarker,
                        hash: marker,
                        file_id: projection.file_id,
                        generation: projection.generation,
                        expected_len: None,
                    });
                }
                FileState::RecoveryBlocked => {}
            }
        }
        Ok(required)
    }

    pub fn create_reconcile_job(&mut self, mode: JobMode) -> Result<Uuid, MetadataError> {
        let job_id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO reconcile_jobs (job_id, kind, mode, started_at, status)
             VALUES (?1, 'scrub', ?2, unixepoch(), 'running')",
            params![job_id.to_string(), mode.as_str()],
        )?;
        Ok(job_id)
    }

    pub fn finish_reconcile_job(
        &mut self,
        job_id: Uuid,
        status: JobStatus,
        report: Option<&str>,
    ) -> Result<(), MetadataError> {
        if status == JobStatus::Running {
            return Err(MetadataError::InvalidJobStatus(
                "a job cannot finish as running".to_owned(),
            ));
        }
        let changed = self.connection.execute(
            "UPDATE reconcile_jobs
             SET status = ?2, completed_at = unixepoch(), report = ?3
             WHERE job_id = ?1 AND status = 'running'",
            params![job_id.to_string(), status.as_str(), report],
        )?;
        if changed != 1 {
            return Err(MetadataError::MissingJob(job_id));
        }
        Ok(())
    }

    /// Every immutable object any generation in history references, so the
    /// backup set retains deleted generations until offsite retention expires.
    pub fn history_objects(&self) -> Result<Vec<HistoryObject>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT 'manifest', m.manifest_hash, NULL, f.updated_at
             FROM file_manifests AS m JOIN files AS f ON f.file_id = m.file_id
             UNION ALL
             SELECT 'chunk', c.ciphertext_hash, c.ciphertext_len, f.updated_at
             FROM file_chunks AS c JOIN files AS f ON f.file_id = c.file_id
             UNION ALL
             SELECT 'deletion_marker', d.marker_hash, NULL, f.updated_at
             FROM deletion_markers AS d JOIN files AS f ON f.file_id = d.file_id
             ORDER BY 1, 2",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut objects = Vec::with_capacity(rows.len());
        let mut seen = std::collections::BTreeSet::new();
        for (kind, hash, len, committed_at) in rows {
            if !seen.insert((kind.clone(), hash.clone())) {
                continue;
            }
            objects.push(HistoryObject {
                kind: kind.parse()?,
                hash: ObjectHash::parse(hash)?,
                expected_len: len
                    .map(|value| u64::try_from(value).map_err(|_| MetadataError::NumericOverflow))
                    .transpose()?,
                committed_at,
            });
        }
        Ok(objects)
    }

    pub fn create_backup_job(&mut self, target_id: &str) -> Result<Uuid, MetadataError> {
        let job_id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO backup_jobs (job_id, target_id, started_at, status)
             VALUES (?1, ?2, unixepoch(), 'running')",
            params![job_id.to_string(), target_id],
        )?;
        Ok(job_id)
    }

    pub fn finish_backup_job(
        &mut self,
        job_id: Uuid,
        status: JobStatus,
        report: Option<&str>,
    ) -> Result<(), MetadataError> {
        if status == JobStatus::Running {
            return Err(MetadataError::InvalidJobStatus(
                "a job cannot finish as running".to_owned(),
            ));
        }
        let changed = self.connection.execute(
            "UPDATE backup_jobs SET status = ?2, completed_at = unixepoch(), report = ?3
             WHERE job_id = ?1 AND status = 'running'",
            params![job_id.to_string(), status.as_str(), report],
        )?;
        if changed != 1 {
            return Err(MetadataError::MissingJob(job_id));
        }
        Ok(())
    }

    pub fn latest_backup_job(
        &self,
        target_id: &str,
    ) -> Result<Option<ReconcileJob>, MetadataError> {
        self.connection
            .query_row(
                "SELECT job_id, status, started_at, completed_at, report
                 FROM backup_jobs WHERE target_id = ?1
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [target_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(job_id, status, started, completed, report)| {
                Ok(ReconcileJob {
                    job_id: Uuid::parse_str(&job_id)
                        .map_err(|_| MetadataError::InvalidBinaryField("job id".to_owned()))?,
                    mode: JobMode::Repair,
                    status: JobStatus::from_str(&status)?,
                    started_at: started,
                    completed_at: completed,
                    report,
                })
            })
            .transpose()
    }

    pub fn backup_target_ids(&self) -> Result<Vec<String>, MetadataError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT target_id FROM backup_jobs ORDER BY target_id")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub fn backup_object_verified(
        &self,
        target_id: &str,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<bool, MetadataError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM backup_objects
                 WHERE target_id = ?1 AND object_kind = ?2 AND object_hash = ?3)",
                params![target_id, kind.as_str(), hash.as_str()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn record_backup_object(
        &mut self,
        target_id: &str,
        kind: ObjectKind,
        hash: &ObjectHash,
        size: u64,
        job_id: Uuid,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO backup_objects
             (target_id, object_kind, object_hash, size, verified_at, job_id)
             VALUES (?1, ?2, ?3, ?4, unixepoch(), ?5)
             ON CONFLICT(target_id, object_kind, object_hash) DO UPDATE SET
                 verified_at = unixepoch(), job_id = excluded.job_id",
            params![
                target_id,
                kind.as_str(),
                hash.as_str(),
                to_i64(size)?,
                job_id.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn backup_object_count(&self, target_id: &str) -> Result<usize, MetadataError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM backup_objects WHERE target_id = ?1",
            [target_id],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| MetadataError::NumericOverflow)
    }

    pub fn record_backup_snapshot(
        &mut self,
        target_id: &str,
        name: &str,
        snapshot_hash: &str,
        content_generation: i64,
        size: u64,
        job_id: Uuid,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO backup_snapshots
             (target_id, name, snapshot_hash, content_generation, size, job_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                target_id,
                name,
                snapshot_hash,
                content_generation,
                to_i64(size)?,
                job_id.to_string()
            ],
        )?;
        Ok(())
    }

    /// Latest shipped snapshot for a target.
    pub fn latest_backup_snapshot(
        &self,
        target_id: &str,
    ) -> Result<Option<BackupSnapshot>, MetadataError> {
        self.connection
            .query_row(
                "SELECT name, snapshot_hash, content_generation, size, created_at
                 FROM backup_snapshots
                 WHERE target_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [target_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(name, hash, generation, size, created_at)| {
                Ok(BackupSnapshot {
                    name,
                    snapshot_hash: hash,
                    content_generation: generation,
                    size: u64::try_from(size).map_err(|_| MetadataError::NumericOverflow)?,
                    created_at,
                })
            })
            .transpose()
    }

    /// Write a transactional, fsynced copy of the database to `path`.
    pub fn snapshot_into(&self, path: &Path) -> Result<(), MetadataError> {
        let target = path
            .to_str()
            .ok_or_else(|| MetadataError::InvalidBinaryField("snapshot path".to_owned()))?;
        self.connection.execute("VACUUM INTO ?1", [target])?;
        Ok(())
    }

    /// Mark jobs left `running` by a previous process as interrupted. Their
    /// scans prove nothing; the next job repeats the work.
    pub fn interrupt_running_jobs(&mut self) -> Result<usize, MetadataError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE backup_jobs SET status = 'interrupted', completed_at = unixepoch()
             WHERE status = 'running'",
            [],
        )?;
        transaction.execute(
            "UPDATE scan_observations SET status = 'failed',
                 completed_at = unixepoch(), detail = 'job interrupted before completion'
             WHERE status = 'running'",
            [],
        )?;
        let changed = transaction.execute(
            "UPDATE reconcile_jobs SET status = 'interrupted', completed_at = unixepoch()
             WHERE status = 'running'",
            [],
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn latest_reconcile_job(&self) -> Result<Option<ReconcileJob>, MetadataError> {
        self.connection
            .query_row(
                "SELECT job_id, mode, status, started_at, completed_at, report
                 FROM reconcile_jobs ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?
            .map(|(job_id, mode, status, started, completed, report)| {
                Ok(ReconcileJob {
                    job_id: Uuid::parse_str(&job_id)
                        .map_err(|_| MetadataError::InvalidBinaryField("job id".to_owned()))?,
                    mode: match mode.as_str() {
                        "verify" => JobMode::Verify,
                        "repair" => JobMode::Repair,
                        other => return Err(MetadataError::InvalidJobStatus(other.to_owned())),
                    },
                    status: JobStatus::from_str(&status)?,
                    started_at: started,
                    completed_at: completed,
                    report,
                })
            })
            .transpose()
    }

    pub fn start_scan(
        &mut self,
        job_id: Uuid,
        agent_id: &str,
        incarnation_id: &str,
        kind: ObjectKind,
    ) -> Result<Uuid, MetadataError> {
        let scan_id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO scan_observations
             (scan_id, job_id, agent_id, incarnation_id, object_kind, started_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch(), 'running')",
            params![
                scan_id.to_string(),
                job_id.to_string(),
                agent_id,
                incarnation_id,
                kind.as_str()
            ],
        )?;
        Ok(scan_id)
    }

    pub fn complete_scan(&mut self, completion: &ScanCompletion) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE scan_observations
             SET status = 'complete', completed_at = unixepoch(), final_cursor = ?2,
                 object_count = ?3, inventory_digest = ?4
             WHERE scan_id = ?1 AND status = 'running'",
            params![
                completion.scan_id.to_string(),
                completion.final_cursor.as_ref().map(ObjectHash::as_str),
                to_i64(completion.object_count)?,
                completion.inventory_digest
            ],
        )?;
        if changed != 1 {
            return Err(MetadataError::MissingScan(completion.scan_id));
        }
        Ok(())
    }

    pub fn fail_scan(&mut self, scan_id: Uuid, detail: &str) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE scan_observations
             SET status = 'failed', completed_at = unixepoch(), detail = ?2
             WHERE scan_id = ?1 AND status = 'running'",
            params![scan_id.to_string(), detail],
        )?;
        if changed != 1 {
            return Err(MetadataError::MissingScan(scan_id));
        }
        Ok(())
    }

    /// Latest complete observation per active incarnation and namespace.
    pub fn latest_complete_scans(&self) -> Result<Vec<CompleteScan>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT agent_id, incarnation_id, object_kind, inventory_digest, completed_at
             FROM (
                 SELECT s.agent_id, s.incarnation_id, s.object_kind, s.inventory_digest,
                        s.completed_at,
                        ROW_NUMBER() OVER (
                            PARTITION BY s.incarnation_id, s.object_kind
                            ORDER BY s.completed_at DESC, s.rowid DESC
                        ) AS recency
                 FROM scan_observations AS s
                 JOIN agent_incarnations AS i ON i.incarnation_id = s.incarnation_id
                 WHERE s.status = 'complete' AND i.status = 'active'
             )
             WHERE recency = 1
             ORDER BY agent_id, object_kind",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(agent_id, incarnation_id, kind, inventory_digest, completed_at)| {
                    Ok(CompleteScan {
                        agent_id,
                        incarnation_id,
                        kind: kind.parse()?,
                        inventory_digest,
                        completed_at,
                    })
                },
            )
            .collect()
    }

    pub fn replace_object_health(
        &mut self,
        job_id: Uuid,
        health: &[ObjectHealth],
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM object_health", [])?;
        for object in health {
            transaction.execute(
                "INSERT INTO object_health
                 (object_kind, object_hash, file_id, generation, verified_copies,
                  verified_domains, state, job_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch())",
                params![
                    object.kind.as_str(),
                    object.hash.as_str(),
                    object.file_id.to_string(),
                    to_i64(object.generation)?,
                    to_i64(object.verified_copies as u64)?,
                    to_i64(object.verified_domains as u64)?,
                    object.state.as_str(),
                    job_id.to_string(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn object_health_summary(&self) -> Result<ObjectHealthSummary, MetadataError> {
        let mut statement = self
            .connection
            .prepare("SELECT state, COUNT(*) FROM object_health GROUP BY state")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut summary = ObjectHealthSummary::default();
        for (state, count) in rows {
            let count = usize::try_from(count).map_err(|_| MetadataError::NumericOverflow)?;
            match ObjectHealthState::from_str(&state)? {
                ObjectHealthState::Durable => summary.durable = count,
                ObjectHealthState::Degraded => summary.degraded = count,
                ObjectHealthState::AtRisk => summary.at_risk = count,
                ObjectHealthState::Lost => summary.lost = count,
            }
        }
        Ok(summary)
    }

    pub fn unhealthy_objects(&self) -> Result<Vec<ObjectHealth>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT object_kind, object_hash, file_id, generation, verified_copies,
                    verified_domains, state, job_id, updated_at
             FROM object_health WHERE state != 'durable'
             ORDER BY file_id, generation, object_kind, object_hash",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(kind, hash, file_id, generation, copies, domains, state, job, at)| {
                    Ok(ObjectHealth {
                        kind: kind.parse()?,
                        hash: ObjectHash::parse(hash)?,
                        file_id: Uuid::parse_str(&file_id)
                            .map_err(|_| MetadataError::InvalidBinaryField("file id".to_owned()))?,
                        generation: u64::try_from(generation)
                            .map_err(|_| MetadataError::NumericOverflow)?,
                        verified_copies: usize::try_from(copies)
                            .map_err(|_| MetadataError::NumericOverflow)?,
                        verified_domains: usize::try_from(domains)
                            .map_err(|_| MetadataError::NumericOverflow)?,
                        state: ObjectHealthState::from_str(&state)?,
                        job_id: Uuid::parse_str(&job)
                            .map_err(|_| MetadataError::InvalidBinaryField("job id".to_owned()))?,
                        updated_at: at,
                    })
                },
            )
            .collect()
    }

    pub fn confirm_placement(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        agent_id: &str,
    ) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE placements SET state = 'confirmed', confirmed_at = unixepoch()
             WHERE object_kind = ?1 AND object_hash = ?2 AND agent_id = ?3",
            params![kind.as_str(), hash.as_str(), agent_id],
        )?;
        if changed != 1 {
            return Err(MetadataError::MissingPlacement);
        }
        Ok(())
    }

    pub fn placement_confirmed(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
        agent_id: &str,
    ) -> Result<bool, MetadataError> {
        let live: bool = self.connection.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM placements
                 WHERE object_kind = ?1 AND object_hash = ?2 AND agent_id = ?3
                   AND {LIVE_CONFIRMED})"
            ),
            params![kind.as_str(), hash.as_str(), agent_id],
            |row| row.get(0),
        )?;
        Ok(live)
    }

    pub fn confirmed_domains(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<usize, MetadataError> {
        let count: i64 = self.connection.query_row(
            &format!(
                "SELECT COUNT(DISTINCT failure_domain) FROM placements
                 WHERE object_kind = ?1 AND object_hash = ?2 AND {LIVE_CONFIRMED}"
            ),
            params![kind.as_str(), hash.as_str()],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| MetadataError::NumericOverflow)
    }

    pub fn set_manifest_plan(
        &mut self,
        upload_id: Uuid,
        hash: &ObjectHash,
        bytes: &[u8],
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        let state = upload_state(&transaction, upload_id)?;
        if state == UploadState::ReplicatingManifest {
            let existing: (String, Vec<u8>) = transaction.query_row(
                "SELECT manifest_hash, manifest_bytes FROM uploads WHERE upload_id = ?1",
                [upload_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if existing.0 != hash.as_str() || existing.1 != bytes {
                return Err(MetadataError::PlanConflict);
            }
            return Ok(());
        }
        if state != UploadState::ReplicatingChunks {
            return Err(MetadataError::IllegalTransition {
                current: state,
                next: UploadState::ReplicatingManifest,
            });
        }
        transaction.execute(
            "UPDATE uploads
             SET manifest_hash = ?2, manifest_bytes = ?3, state = 'replicating_manifest'
             WHERE upload_id = ?1",
            params![upload_id.to_string(), hash.as_str(), bytes],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn commit_file(
        &mut self,
        upload_id: Uuid,
        minimum_replicas: usize,
    ) -> Result<FileRecord, MetadataError> {
        let transaction = self.connection.transaction()?;
        let upload = transaction.query_row(
            "SELECT upload_id, idempotency_key, request_fingerprint, file_id,
                    file_name, plaintext_hash, plaintext_size, storage_class,
                    content_key_nonce, wrapped_content_key, state, generation,
                    manifest_hash, manifest_bytes
             FROM uploads WHERE upload_id = ?1",
            [upload_id.to_string()],
            row_to_upload,
        )?;
        if upload.state == UploadState::Committed {
            let file = file_by_id_transaction(&transaction, upload.file_id)?
                .ok_or(MetadataError::MissingCommittedFile)?;
            transaction.commit()?;
            return Ok(file);
        }
        if upload.state != UploadState::ReplicatingManifest {
            return Err(MetadataError::IllegalTransition {
                current: upload.state,
                next: UploadState::Committed,
            });
        }
        let manifest_hash = upload.manifest_hash.ok_or(MetadataError::MissingManifest)?;
        require_replica_floor(
            &transaction,
            ObjectKind::Manifest,
            &manifest_hash,
            minimum_replicas,
        )?;

        let mut statement = transaction.prepare(
            "SELECT ordinal, envelope_version, plaintext_len, nonce,
                    ciphertext_hash, ciphertext_len
             FROM upload_chunks
             WHERE upload_id = ?1 ORDER BY ordinal",
        )?;
        let chunk_rows = statement
            .query_map([upload_id.to_string()], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (_, _, _, _, hash, _) in &chunk_rows {
            let hash = hash.as_ref().ok_or(MetadataError::MissingChunkObject)?;
            let hash = ObjectHash::parse(hash.clone())?;
            require_replica_floor(&transaction, ObjectKind::Chunk, &hash, minimum_replicas)?;
        }

        transaction.execute(
            "INSERT INTO files
             (file_id, current_generation, state, name, plaintext_hash,
              plaintext_size, current_manifest_hash, source_upload_id)
             VALUES (?1, ?2, 'committed', ?3, ?4, ?5, ?6, ?7)",
            params![
                upload.file_id.to_string(),
                to_i64(upload.generation)?,
                &upload.file_name,
                upload.plaintext_hash.as_slice(),
                to_i64(upload.plaintext_size)?,
                manifest_hash.as_str(),
                upload.upload_id.to_string(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO file_manifests
             (file_id, generation, manifest_hash, name, plaintext_hash, plaintext_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                upload.file_id.to_string(),
                to_i64(upload.generation)?,
                manifest_hash.as_str(),
                &upload.file_name,
                upload.plaintext_hash.as_slice(),
                to_i64(upload.plaintext_size)?,
            ],
        )?;
        for (ordinal, envelope_version, plaintext_len, nonce, hash, ciphertext_len) in chunk_rows {
            transaction.execute(
                "INSERT INTO file_chunks
                 (file_id, generation, ordinal, envelope_version, plaintext_len, nonce,
                  ciphertext_hash, ciphertext_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    upload.file_id.to_string(),
                    to_i64(upload.generation)?,
                    ordinal,
                    envelope_version,
                    plaintext_len,
                    nonce,
                    hash.ok_or(MetadataError::MissingChunkObject)?,
                    ciphertext_len.ok_or(MetadataError::MissingChunkObject)?,
                ],
            )?;
        }
        transaction.execute(
            "UPDATE uploads SET state = 'committed' WHERE upload_id = ?1",
            [upload_id.to_string()],
        )?;
        bump_content_generation(&transaction)?;
        transaction.commit()?;
        self.file_by_id(upload.file_id)?
            .ok_or(MetadataError::MissingCommittedFile)
    }

    pub fn file_by_id(&self, file_id: Uuid) -> Result<Option<FileRecord>, MetadataError> {
        file_by_id_connection(&self.connection, file_id)
    }

    pub fn file_projection(&self, file_id: Uuid) -> Result<Option<FileProjection>, MetadataError> {
        self.connection
            .query_row(
                "SELECT file_id, current_generation, state, name, plaintext_hash,
                        plaintext_size, current_manifest_hash,
                        current_deletion_hash, deleted_at
                 FROM files WHERE file_id = ?1",
                [file_id.to_string()],
                row_to_file_projection,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn all_file_projections(&self) -> Result<Vec<FileProjection>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT file_id, current_generation, state, name, plaintext_hash,
                    plaintext_size, current_manifest_hash,
                    current_deletion_hash, deleted_at
             FROM files ORDER BY file_id",
        )?;
        let values = statement
            .query_map([], row_to_file_projection)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(values)
    }

    pub fn delete_by_idempotency(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<DeleteOperation>, MetadataError> {
        self.connection
            .query_row(
                "SELECT operation_id, idempotency_key, file_id, generation,
                        deleted_at, marker_hash, marker_bytes, state
                 FROM delete_operations WHERE idempotency_key = ?1",
                [idempotency_key],
                row_to_delete_operation,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn create_delete_operation(
        &mut self,
        operation: &NewDeleteOperation,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        let projection = transaction
            .query_row(
                "SELECT file_id, current_generation, state, name, plaintext_hash,
                        plaintext_size, current_manifest_hash,
                        current_deletion_hash, deleted_at
                 FROM files WHERE file_id = ?1",
                [operation.file_id.to_string()],
                row_to_file_projection,
            )
            .optional()?
            .ok_or(MetadataError::MissingFile(operation.file_id))?;
        if projection.state != FileState::Committed {
            return Err(MetadataError::FileNotCommitted(operation.file_id));
        }
        if projection
            .generation
            .checked_add(1)
            .ok_or(MetadataError::NumericOverflow)?
            != operation.generation
        {
            return Err(MetadataError::GenerationConflict(operation.file_id));
        }
        transaction.execute(
            "INSERT INTO delete_operations
             (operation_id, idempotency_key, file_id, generation, deleted_at,
              marker_hash, marker_bytes, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'replicating_marker')",
            params![
                operation.operation_id.to_string(),
                operation.idempotency_key,
                operation.file_id.to_string(),
                to_i64(operation.generation)?,
                operation.deleted_at,
                operation.marker_hash.as_str(),
                operation.marker_bytes,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn commit_delete(
        &mut self,
        operation_id: Uuid,
        minimum_replicas: usize,
    ) -> Result<DeleteOperation, MetadataError> {
        let transaction = self.connection.transaction()?;
        let operation = transaction.query_row(
            "SELECT operation_id, idempotency_key, file_id, generation,
                    deleted_at, marker_hash, marker_bytes, state
             FROM delete_operations WHERE operation_id = ?1",
            [operation_id.to_string()],
            row_to_delete_operation,
        )?;
        if operation.state == DeleteState::Committed {
            transaction.commit()?;
            return Ok(operation);
        }
        require_replica_floor(
            &transaction,
            ObjectKind::DeletionMarker,
            &operation.marker_hash,
            minimum_replicas,
        )?;
        let projection = transaction.query_row(
            "SELECT file_id, current_generation, state, name, plaintext_hash,
                    plaintext_size, current_manifest_hash,
                    current_deletion_hash, deleted_at
             FROM files WHERE file_id = ?1",
            [operation.file_id.to_string()],
            row_to_file_projection,
        )?;
        if projection.generation > operation.generation {
            return Err(MetadataError::GenerationConflict(operation.file_id));
        }
        if projection.generation == operation.generation
            && (projection.state != FileState::Deleted
                || projection.deletion_hash.as_ref() != Some(&operation.marker_hash))
        {
            return Err(MetadataError::GenerationConflict(operation.file_id));
        }
        transaction.execute(
            "INSERT INTO deletion_markers
             (file_id, generation, marker_hash, deleted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file_id, generation) DO NOTHING",
            params![
                operation.file_id.to_string(),
                to_i64(operation.generation)?,
                operation.marker_hash.as_str(),
                operation.deleted_at,
            ],
        )?;
        let persisted_marker: String = transaction.query_row(
            "SELECT marker_hash FROM deletion_markers WHERE file_id = ?1 AND generation = ?2",
            params![operation.file_id.to_string(), to_i64(operation.generation)?],
            |row| row.get(0),
        )?;
        if persisted_marker != operation.marker_hash.as_str() {
            return Err(MetadataError::GenerationConflict(operation.file_id));
        }
        transaction.execute(
            "UPDATE files
             SET current_generation = ?2, state = 'deleted',
                 current_deletion_hash = ?3, deleted_at = ?4,
                 updated_at = unixepoch()
             WHERE file_id = ?1",
            params![
                operation.file_id.to_string(),
                to_i64(operation.generation)?,
                operation.marker_hash.as_str(),
                operation.deleted_at,
            ],
        )?;
        transaction.execute(
            "UPDATE delete_operations SET state = 'committed'
             WHERE operation_id = ?1",
            [operation.operation_id.to_string()],
        )?;
        bump_content_generation(&transaction)?;
        transaction.commit()?;
        self.delete_by_idempotency(&operation.idempotency_key)?
            .ok_or(MetadataError::MissingDeleteOperation)
    }

    pub fn confirmed_agents(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<Vec<String>, MetadataError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT agent_id FROM placements
             WHERE object_kind = ?1 AND object_hash = ?2 AND {LIVE_CONFIRMED}
             ORDER BY agent_id"
        ))?;
        let agents = statement
            .query_map(params![kind.as_str(), hash.as_str()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(MetadataError::from)?;
        Ok(agents)
    }

    pub fn apply_recovered_manifest(
        &mut self,
        manifest: &crate::format::DecodedManifest,
        manifest_hash: &ObjectHash,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        let existing = file_projection_transaction(&transaction, manifest.file_id)?;
        if existing
            .as_ref()
            .is_some_and(|file| file.generation > manifest.generation)
        {
            return Err(MetadataError::GenerationConflict(manifest.file_id));
        }
        transaction.execute(
            "INSERT INTO file_manifests
             (file_id, generation, manifest_hash, name, plaintext_hash, plaintext_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(file_id, generation) DO NOTHING",
            params![
                manifest.file_id.to_string(),
                to_i64(manifest.generation)?,
                manifest_hash.as_str(),
                &manifest.payload.name,
                manifest.payload.plaintext_hash.as_slice(),
                to_i64(manifest.payload.plaintext_len)?,
            ],
        )?;
        let persisted_hash: String = transaction.query_row(
            "SELECT manifest_hash FROM file_manifests WHERE file_id = ?1 AND generation = ?2",
            params![manifest.file_id.to_string(), to_i64(manifest.generation)?],
            |row| row.get(0),
        )?;
        if persisted_hash != manifest_hash.as_str() {
            return Err(MetadataError::GenerationConflict(manifest.file_id));
        }
        for chunk in &manifest.payload.chunks {
            transaction.execute(
                "INSERT INTO file_chunks
                 (file_id, generation, ordinal, envelope_version, plaintext_len, nonce,
                  ciphertext_hash, ciphertext_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(file_id, generation, ordinal) DO NOTHING",
                params![
                    manifest.file_id.to_string(),
                    to_i64(manifest.generation)?,
                    chunk.ordinal,
                    ENVELOPE_VERSION,
                    chunk.plaintext_len,
                    chunk.nonce.as_slice(),
                    chunk.ciphertext_hash.as_str(),
                    chunk.ciphertext_len,
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO files
             (file_id, current_generation, state, name, plaintext_hash,
              plaintext_size, current_manifest_hash)
             VALUES (?1, ?2, 'committed', ?3, ?4, ?5, ?6)
             ON CONFLICT(file_id) DO UPDATE SET
                 current_generation = excluded.current_generation,
                 state = 'committed', name = excluded.name,
                 plaintext_hash = excluded.plaintext_hash,
                 plaintext_size = excluded.plaintext_size,
                 current_manifest_hash = excluded.current_manifest_hash,
                 current_deletion_hash = NULL, deleted_at = NULL,
                 updated_at = unixepoch()",
            params![
                manifest.file_id.to_string(),
                to_i64(manifest.generation)?,
                &manifest.payload.name,
                manifest.payload.plaintext_hash.as_slice(),
                to_i64(manifest.payload.plaintext_len)?,
                manifest_hash.as_str(),
            ],
        )?;
        transaction.execute(
            "DELETE FROM recovery_issues WHERE file_id = ?1",
            [manifest.file_id.to_string()],
        )?;
        bump_content_generation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn apply_recovered_deletion(
        &mut self,
        marker: &crate::format::DeletionMarker,
        marker_hash: &ObjectHash,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        let existing = file_projection_transaction(&transaction, marker.file_id)?;
        if existing
            .as_ref()
            .is_some_and(|file| file.generation > marker.generation)
        {
            return Err(MetadataError::GenerationConflict(marker.file_id));
        }
        transaction.execute(
            "INSERT INTO deletion_markers (file_id, generation, marker_hash, deleted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file_id, generation) DO NOTHING",
            params![
                marker.file_id.to_string(),
                to_i64(marker.generation)?,
                marker_hash.as_str(),
                marker.deleted_at,
            ],
        )?;
        let persisted_hash: String = transaction.query_row(
            "SELECT marker_hash FROM deletion_markers WHERE file_id = ?1 AND generation = ?2",
            params![marker.file_id.to_string(), to_i64(marker.generation)?],
            |row| row.get(0),
        )?;
        if persisted_hash != marker_hash.as_str() {
            return Err(MetadataError::GenerationConflict(marker.file_id));
        }
        transaction.execute(
            "INSERT INTO files
             (file_id, current_generation, state, current_deletion_hash, deleted_at)
             VALUES (?1, ?2, 'deleted', ?3, ?4)
             ON CONFLICT(file_id) DO UPDATE SET
                 current_generation = excluded.current_generation,
                 state = 'deleted', current_deletion_hash = excluded.current_deletion_hash,
                 deleted_at = excluded.deleted_at, updated_at = unixepoch()",
            params![
                marker.file_id.to_string(),
                to_i64(marker.generation)?,
                marker_hash.as_str(),
                marker.deleted_at,
            ],
        )?;
        transaction.execute(
            "DELETE FROM recovery_issues WHERE file_id = ?1",
            [marker.file_id.to_string()],
        )?;
        bump_content_generation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_recovery_blocked(
        &mut self,
        file_id: Uuid,
        generation: u64,
        deletion_hash: Option<&ObjectHash>,
        issue: &str,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        let existing = file_projection_transaction(&transaction, file_id)?;
        let state = if deletion_hash.is_some()
            || existing
                .as_ref()
                .is_some_and(|file| file.state == FileState::Deleted)
        {
            FileState::Deleted
        } else {
            FileState::RecoveryBlocked
        };
        transaction.execute(
            "INSERT INTO files (file_id, current_generation, state, current_deletion_hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file_id) DO UPDATE SET
                 current_generation = MAX(files.current_generation, excluded.current_generation),
                 state = excluded.state,
                 current_deletion_hash = COALESCE(excluded.current_deletion_hash, files.current_deletion_hash),
                 updated_at = unixepoch()",
            params![
                file_id.to_string(),
                to_i64(generation)?,
                state.as_str(),
                deletion_hash.map(ObjectHash::as_str),
            ],
        )?;
        transaction.execute(
            "DELETE FROM recovery_issues WHERE file_id = ?1",
            [file_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO recovery_issues (file_id, generation, issue_kind, detail)
             VALUES (?1, ?2, 'recovery_blocked', ?3)",
            params![file_id.to_string(), to_i64(generation)?, issue],
        )?;
        bump_content_generation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }
}

fn bump_content_generation(transaction: &Transaction<'_>) -> Result<(), MetadataError> {
    transaction.execute(
        "INSERT INTO portal_meta (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET
             value = CAST(CAST(portal_meta.value AS INTEGER) + 1 AS TEXT),
             updated_at = unixepoch()",
        [CONTENT_GENERATION_META],
    )?;
    Ok(())
}

fn file_projection_transaction(
    transaction: &Transaction<'_>,
    file_id: Uuid,
) -> Result<Option<FileProjection>, MetadataError> {
    transaction
        .query_row(
            "SELECT file_id, current_generation, state, name, plaintext_hash,
                    plaintext_size, current_manifest_hash,
                    current_deletion_hash, deleted_at
             FROM files WHERE file_id = ?1",
            [file_id.to_string()],
            row_to_file_projection,
        )
        .optional()
        .map_err(Into::into)
}

fn upload_state(
    transaction: &Transaction<'_>,
    upload_id: Uuid,
) -> Result<UploadState, MetadataError> {
    let state: String = transaction.query_row(
        "SELECT state FROM uploads WHERE upload_id = ?1",
        [upload_id.to_string()],
        |row| row.get(0),
    )?;
    UploadState::from_str(&state)
}

fn require_replica_floor(
    transaction: &Transaction<'_>,
    kind: ObjectKind,
    hash: &ObjectHash,
    minimum_replicas: usize,
) -> Result<(), MetadataError> {
    let count: i64 = transaction.query_row(
        &format!(
            "SELECT COUNT(DISTINCT failure_domain) FROM placements
             WHERE object_kind = ?1 AND object_hash = ?2 AND {LIVE_CONFIRMED}"
        ),
        params![kind.as_str(), hash.as_str()],
        |row| row.get(0),
    )?;
    if usize::try_from(count).map_err(|_| MetadataError::NumericOverflow)? < minimum_replicas {
        return Err(MetadataError::ReplicaFloorNotMet {
            kind,
            hash: hash.clone(),
        });
    }
    Ok(())
}

fn row_to_upload(row: &rusqlite::Row<'_>) -> rusqlite::Result<UploadRecord> {
    let upload_id: String = row.get(0)?;
    let request_fingerprint: Vec<u8> = row.get(2)?;
    let file_id: String = row.get(3)?;
    let plaintext_hash: Vec<u8> = row.get(5)?;
    let plaintext_size: i64 = row.get(6)?;
    let content_key_nonce: Vec<u8> = row.get(8)?;
    let state: String = row.get(10)?;
    let generation: i64 = row.get(11)?;
    let manifest_hash: Option<String> = row.get(12)?;
    Ok(UploadRecord {
        upload_id: parse_uuid_sql(&upload_id)?,
        idempotency_key: row.get(1)?,
        request_fingerprint: fixed_bytes_sql(&request_fingerprint, "request fingerprint")?,
        file_id: parse_uuid_sql(&file_id)?,
        file_name: row.get(4)?,
        plaintext_hash: fixed_bytes_sql(&plaintext_hash, "plaintext hash")?,
        plaintext_size: from_i64_sql(plaintext_size)?,
        storage_class: row.get(7)?,
        content_key: WrappedKey {
            nonce: fixed_bytes_sql(&content_key_nonce, "content key nonce")?,
            ciphertext: row.get(9)?,
        },
        state: UploadState::from_str(&state).map_err(metadata_to_sql)?,
        generation: from_i64_sql(generation)?,
        manifest_hash: manifest_hash
            .map(ObjectHash::parse)
            .transpose()
            .map_err(|error| metadata_to_sql(error.into()))?,
        manifest_bytes: row.get(13)?,
    })
}

fn file_by_id_connection(
    connection: &Connection,
    file_id: Uuid,
) -> Result<Option<FileRecord>, MetadataError> {
    connection
        .query_row(
            "SELECT file_id, current_generation, name, plaintext_hash,
                    plaintext_size, current_manifest_hash
             FROM files WHERE file_id = ?1 AND state = 'committed'",
            [file_id.to_string()],
            row_to_file,
        )
        .optional()
        .map_err(Into::into)
}

fn file_by_id_transaction(
    transaction: &Transaction<'_>,
    file_id: Uuid,
) -> Result<Option<FileRecord>, MetadataError> {
    transaction
        .query_row(
            "SELECT file_id, current_generation, name, plaintext_hash,
                    plaintext_size, current_manifest_hash
             FROM files WHERE file_id = ?1 AND state = 'committed'",
            [file_id.to_string()],
            row_to_file,
        )
        .optional()
        .map_err(Into::into)
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let file_id: String = row.get(0)?;
    let generation: i64 = row.get(1)?;
    let plaintext_hash: Vec<u8> = row.get(3)?;
    let plaintext_size: i64 = row.get(4)?;
    let manifest_hash: String = row.get(5)?;
    Ok(FileRecord {
        file_id: parse_uuid_sql(&file_id)?,
        generation: from_i64_sql(generation)?,
        name: row.get(2)?,
        plaintext_hash: fixed_bytes_sql(&plaintext_hash, "plaintext hash")?,
        plaintext_size: from_i64_sql(plaintext_size)?,
        manifest_hash: ObjectHash::parse(manifest_hash)
            .map_err(|error| metadata_to_sql(error.into()))?,
    })
}

fn row_to_file_projection(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileProjection> {
    let file_id: String = row.get(0)?;
    let generation: i64 = row.get(1)?;
    let state: String = row.get(2)?;
    let plaintext_hash: Option<Vec<u8>> = row.get(4)?;
    let plaintext_size: Option<i64> = row.get(5)?;
    let manifest_hash: Option<String> = row.get(6)?;
    let deletion_hash: Option<String> = row.get(7)?;
    Ok(FileProjection {
        file_id: parse_uuid_sql(&file_id)?,
        generation: from_i64_sql(generation)?,
        state: FileState::from_str(&state).map_err(metadata_to_sql)?,
        name: row.get(3)?,
        plaintext_hash: plaintext_hash
            .map(|bytes| fixed_bytes(&bytes, "plaintext hash"))
            .transpose()
            .map_err(metadata_to_sql)?,
        plaintext_size: plaintext_size.map(from_i64_sql).transpose()?,
        manifest_hash: manifest_hash
            .map(ObjectHash::parse)
            .transpose()
            .map_err(|error| metadata_to_sql(error.into()))?,
        deletion_hash: deletion_hash
            .map(ObjectHash::parse)
            .transpose()
            .map_err(|error| metadata_to_sql(error.into()))?,
        deleted_at: row.get(8)?,
    })
}

fn row_to_delete_operation(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeleteOperation> {
    let operation_id: String = row.get(0)?;
    let file_id: String = row.get(2)?;
    let generation: i64 = row.get(3)?;
    let marker_hash: String = row.get(5)?;
    let state: String = row.get(7)?;
    Ok(DeleteOperation {
        operation_id: parse_uuid_sql(&operation_id)?,
        idempotency_key: row.get(1)?,
        file_id: parse_uuid_sql(&file_id)?,
        generation: from_i64_sql(generation)?,
        deleted_at: row.get(4)?,
        marker_hash: ObjectHash::parse(marker_hash)
            .map_err(|error| metadata_to_sql(error.into()))?,
        marker_bytes: row.get(6)?,
        state: DeleteState::from_str(&state).map_err(metadata_to_sql)?,
    })
}

fn parse_uuid_sql(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn fixed_bytes<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N], MetadataError> {
    bytes
        .try_into()
        .map_err(|_| MetadataError::InvalidBinaryField(field.to_owned()))
}

fn fixed_bytes_sql<const N: usize>(bytes: &[u8], field: &str) -> rusqlite::Result<[u8; N]> {
    fixed_bytes(bytes, field).map_err(metadata_to_sql)
}

fn to_i64(value: u64) -> Result<i64, MetadataError> {
    i64::try_from(value).map_err(|_| MetadataError::NumericOverflow)
}

fn from_i64_sql(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| metadata_to_sql(MetadataError::NumericOverflow))
}

fn metadata_to_sql(error: MetadataError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
}

const BASE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS uploads (
    upload_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    request_fingerprint BLOB NOT NULL CHECK(length(request_fingerprint) = 32),
    file_id TEXT NOT NULL UNIQUE,
    file_name TEXT NOT NULL,
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    plaintext_size INTEGER NOT NULL CHECK(plaintext_size >= 0),
    storage_class TEXT NOT NULL,
    content_key_nonce BLOB NOT NULL CHECK(length(content_key_nonce) = 24),
    wrapped_content_key BLOB NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
        'staging', 'replicating_chunks', 'replicating_manifest', 'committed'
    )),
    generation INTEGER NOT NULL CHECK(generation > 0),
    manifest_hash TEXT,
    manifest_bytes BLOB,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((manifest_hash IS NULL) = (manifest_bytes IS NULL))
);

CREATE TABLE IF NOT EXISTS upload_chunks (
    upload_id TEXT NOT NULL REFERENCES uploads(upload_id),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    envelope_version INTEGER NOT NULL CHECK(envelope_version > 0),
    plaintext_len INTEGER NOT NULL CHECK(plaintext_len >= 0),
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    nonce BLOB NOT NULL CHECK(length(nonce) = 24),
    ciphertext_hash TEXT,
    ciphertext_len INTEGER,
    PRIMARY KEY(upload_id, ordinal),
    CHECK ((ciphertext_hash IS NULL) = (ciphertext_len IS NULL))
);

CREATE TABLE IF NOT EXISTS placements (
    object_kind TEXT NOT NULL CHECK(object_kind IN (
        'chunk', 'manifest', 'deletion_marker'
    )),
    object_hash TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    failure_domain TEXT NOT NULL,
    incarnation_id TEXT,
    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed', 'missing', 'corrupt')),
    confirmed_at INTEGER,
    last_verified_at INTEGER,
    last_verified_job TEXT,
    PRIMARY KEY(object_kind, object_hash, agent_id)
);

CREATE TABLE IF NOT EXISTS files (
    file_id TEXT PRIMARY KEY,
    current_generation INTEGER NOT NULL CHECK(current_generation > 0),
    state TEXT NOT NULL CHECK(state IN (
        'committed', 'deleted', 'recovery_blocked'
    )),
    name TEXT,
    plaintext_hash BLOB CHECK(plaintext_hash IS NULL OR length(plaintext_hash) = 32),
    plaintext_size INTEGER CHECK(plaintext_size IS NULL OR plaintext_size >= 0),
    current_manifest_hash TEXT,
    current_deletion_hash TEXT,
    source_upload_id TEXT UNIQUE REFERENCES uploads(upload_id),
    deleted_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (state != 'committed' OR (
        name IS NOT NULL AND plaintext_hash IS NOT NULL
        AND plaintext_size IS NOT NULL AND current_manifest_hash IS NOT NULL
        AND current_deletion_hash IS NULL
    )),
    CHECK (state != 'deleted' OR current_deletion_hash IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS file_manifests (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    manifest_hash TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    plaintext_size INTEGER NOT NULL CHECK(plaintext_size >= 0),
    PRIMARY KEY(file_id, generation)
);

CREATE TABLE IF NOT EXISTS file_chunks (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    envelope_version INTEGER NOT NULL CHECK(envelope_version > 0),
    plaintext_len INTEGER NOT NULL CHECK(plaintext_len >= 0),
    nonce BLOB NOT NULL CHECK(length(nonce) = 24),
    ciphertext_hash TEXT NOT NULL,
    ciphertext_len INTEGER NOT NULL CHECK(ciphertext_len >= 0),
    PRIMARY KEY(file_id, generation, ordinal),
    FOREIGN KEY(file_id, generation)
        REFERENCES file_manifests(file_id, generation)
);

CREATE TABLE IF NOT EXISTS deletion_markers (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    marker_hash TEXT NOT NULL UNIQUE,
    deleted_at INTEGER NOT NULL,
    PRIMARY KEY(file_id, generation)
);

CREATE TABLE IF NOT EXISTS delete_operations (
    operation_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    deleted_at INTEGER NOT NULL,
    marker_hash TEXT NOT NULL,
    marker_bytes BLOB NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('replicating_marker', 'committed')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS recovery_issues (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    issue_kind TEXT NOT NULL,
    detail TEXT NOT NULL,
    observed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    resolved_at INTEGER,
    PRIMARY KEY(file_id, generation, issue_kind)
);
"#;

const LIFECYCLE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_incarnations (
    incarnation_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    failure_domain TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('active', 'superseded', 'retired')),
    first_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
    superseded_by TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS agent_incarnations_one_active
    ON agent_incarnations(agent_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS reconcile_jobs (
    job_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('scrub')),
    mode TEXT NOT NULL CHECK(mode IN ('verify', 'repair')),
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    status TEXT NOT NULL CHECK(status IN (
        'running', 'complete', 'failed', 'interrupted'
    )),
    report TEXT
);

CREATE TABLE IF NOT EXISTS scan_observations (
    scan_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES reconcile_jobs(job_id),
    agent_id TEXT NOT NULL,
    incarnation_id TEXT NOT NULL,
    object_kind TEXT NOT NULL CHECK(object_kind IN (
        'chunk', 'manifest', 'deletion_marker'
    )),
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    status TEXT NOT NULL CHECK(status IN ('running', 'complete', 'failed')),
    final_cursor TEXT,
    object_count INTEGER,
    inventory_digest TEXT,
    detail TEXT,
    CHECK (status != 'complete' OR (object_count IS NOT NULL AND inventory_digest IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS object_health (
    object_kind TEXT NOT NULL,
    object_hash TEXT NOT NULL,
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    verified_copies INTEGER NOT NULL CHECK(verified_copies >= 0),
    verified_domains INTEGER NOT NULL CHECK(verified_domains >= 0),
    state TEXT NOT NULL CHECK(state IN ('durable', 'degraded', 'at_risk', 'lost')),
    job_id TEXT NOT NULL REFERENCES reconcile_jobs(job_id),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY(object_kind, object_hash)
);
"#;

fn apply_schema(connection: &Connection) -> Result<(), MetadataError> {
    connection.execute_batch(BASE_SCHEMA)?;
    connection.execute_batch(LIFECYCLE_SCHEMA)?;
    connection.execute_batch(META_SCHEMA)?;
    connection.execute_batch(BACKUP_SCHEMA)?;
    connection.execute_batch("PRAGMA user_version = 6;")?;
    Ok(())
}

/// Schema v5 added `portal_meta` (bound master-key identifier); v6 adds the
/// backup job, object, and snapshot tables. Both are additive, so applying the
/// full schema migrates either version.
const BACKUP_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS backup_jobs (
    job_id TEXT PRIMARY KEY,
    target_id TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    status TEXT NOT NULL CHECK(status IN (
        'running', 'complete', 'failed', 'interrupted'
    )),
    report TEXT
);

CREATE TABLE IF NOT EXISTS backup_objects (
    target_id TEXT NOT NULL,
    object_kind TEXT NOT NULL CHECK(object_kind IN (
        'chunk', 'manifest', 'deletion_marker'
    )),
    object_hash TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size >= 0),
    verified_at INTEGER NOT NULL,
    job_id TEXT NOT NULL REFERENCES backup_jobs(job_id),
    PRIMARY KEY(target_id, object_kind, object_hash)
);

CREATE TABLE IF NOT EXISTS backup_snapshots (
    target_id TEXT NOT NULL,
    name TEXT NOT NULL,
    snapshot_hash TEXT NOT NULL,
    content_generation INTEGER NOT NULL,
    size INTEGER NOT NULL CHECK(size >= 0),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    job_id TEXT NOT NULL REFERENCES backup_jobs(job_id),
    PRIMARY KEY(target_id, name)
);
"#;

const META_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS portal_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#;

/// Rebuild `placements` with incarnation and verification columns and the
/// expanded state set, then add the lifecycle tables. Existing rows keep a
/// NULL incarnation until their agent is observed and adopts them.
fn migrate_v3_to_v4(connection: &mut Connection) -> Result<(), MetadataError> {
    let migration: Result<(), MetadataError> = (|| {
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        if !table_has_column(connection, "placements", "incarnation_id")? {
            connection.execute_batch(
                r#"
ALTER TABLE placements RENAME TO placements_v3;

CREATE TABLE placements (
    object_kind TEXT NOT NULL CHECK(object_kind IN (
        'chunk', 'manifest', 'deletion_marker'
    )),
    object_hash TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    failure_domain TEXT NOT NULL,
    incarnation_id TEXT,
    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed', 'missing', 'corrupt')),
    confirmed_at INTEGER,
    last_verified_at INTEGER,
    last_verified_job TEXT,
    PRIMARY KEY(object_kind, object_hash, agent_id)
);

INSERT INTO placements
    (object_kind, object_hash, agent_id, failure_domain, state, confirmed_at)
SELECT object_kind, object_hash, agent_id, failure_domain, state, confirmed_at
FROM placements_v3;

DROP TABLE placements_v3;
"#,
            )?;
        }
        connection.execute_batch(LIFECYCLE_SCHEMA)?;
        connection.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if migration.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    migration?;
    apply_schema(connection)
}

fn migrate_v1_to_v3(connection: &mut Connection) -> Result<(), MetadataError> {
    migrate_projection_schema_to_v3(connection, true)
}

fn migrate_v2_to_v3(connection: &mut Connection) -> Result<(), MetadataError> {
    let upload_has_envelope = table_has_column(connection, "upload_chunks", "envelope_version")?;
    let has_recovery_history = table_exists(connection, "file_manifests")?;
    match (upload_has_envelope, has_recovery_history) {
        (true, false) => migrate_projection_schema_to_v3(connection, false),
        (false, true) => migrate_recovery_schema_to_v3(connection, true, true),
        (true, true) => {
            let file_has_envelope =
                table_has_column(connection, "file_chunks", "envelope_version")?;
            migrate_recovery_schema_to_v3(connection, false, !file_has_envelope)
        }
        (false, false) => Err(MetadataError::AmbiguousSchemaVersion(2)),
    }
}

fn migrate_projection_schema_to_v3(
    connection: &mut Connection,
    add_upload_envelope: bool,
) -> Result<(), MetadataError> {
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let migration: Result<(), MetadataError> = (|| {
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        if add_upload_envelope {
            connection.execute_batch(
                "ALTER TABLE upload_chunks
                 ADD COLUMN envelope_version INTEGER NOT NULL DEFAULT 1
                 CHECK(envelope_version > 0);",
            )?;
        }
        connection.execute_batch(
            r#"

ALTER TABLE files RENAME TO files_v1;
ALTER TABLE placements RENAME TO placements_v1;

CREATE TABLE placements (
    object_kind TEXT NOT NULL CHECK(object_kind IN (
        'chunk', 'manifest', 'deletion_marker'
    )),
    object_hash TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    failure_domain TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed')),
    confirmed_at INTEGER,
    PRIMARY KEY(object_kind, object_hash, agent_id)
);

INSERT INTO placements
SELECT object_kind, object_hash, agent_id, failure_domain, state, confirmed_at
FROM placements_v1;

CREATE TABLE files (
    file_id TEXT PRIMARY KEY,
    current_generation INTEGER NOT NULL CHECK(current_generation > 0),
    state TEXT NOT NULL CHECK(state IN (
        'committed', 'deleted', 'recovery_blocked'
    )),
    name TEXT,
    plaintext_hash BLOB CHECK(plaintext_hash IS NULL OR length(plaintext_hash) = 32),
    plaintext_size INTEGER CHECK(plaintext_size IS NULL OR plaintext_size >= 0),
    current_manifest_hash TEXT,
    current_deletion_hash TEXT,
    source_upload_id TEXT UNIQUE REFERENCES uploads(upload_id),
    deleted_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (state != 'committed' OR (
        name IS NOT NULL AND plaintext_hash IS NOT NULL
        AND plaintext_size IS NOT NULL AND current_manifest_hash IS NOT NULL
        AND current_deletion_hash IS NULL
    )),
    CHECK (state != 'deleted' OR current_deletion_hash IS NOT NULL)
);

CREATE TABLE file_manifests (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    manifest_hash TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    plaintext_size INTEGER NOT NULL CHECK(plaintext_size >= 0),
    PRIMARY KEY(file_id, generation)
);

CREATE TABLE file_chunks (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    envelope_version INTEGER NOT NULL CHECK(envelope_version > 0),
    plaintext_len INTEGER NOT NULL CHECK(plaintext_len >= 0),
    nonce BLOB NOT NULL CHECK(length(nonce) = 24),
    ciphertext_hash TEXT NOT NULL,
    ciphertext_len INTEGER NOT NULL CHECK(ciphertext_len >= 0),
    PRIMARY KEY(file_id, generation, ordinal),
    FOREIGN KEY(file_id, generation)
        REFERENCES file_manifests(file_id, generation)
);

CREATE TABLE deletion_markers (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    marker_hash TEXT NOT NULL UNIQUE,
    deleted_at INTEGER NOT NULL,
    PRIMARY KEY(file_id, generation)
);

CREATE TABLE delete_operations (
    operation_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    deleted_at INTEGER NOT NULL,
    marker_hash TEXT NOT NULL,
    marker_bytes BLOB NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('replicating_marker', 'committed')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE recovery_issues (
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    issue_kind TEXT NOT NULL,
    detail TEXT NOT NULL,
    observed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    resolved_at INTEGER,
    PRIMARY KEY(file_id, generation, issue_kind)
);

INSERT INTO files (
    file_id, current_generation, state, name, plaintext_hash, plaintext_size,
    current_manifest_hash, source_upload_id, created_at, updated_at
)
SELECT file_id, generation, 'committed', name, plaintext_hash, plaintext_size,
       manifest_hash, upload_id, created_at, created_at
FROM files_v1;

INSERT INTO file_manifests (
    file_id, generation, manifest_hash, name, plaintext_hash, plaintext_size
)
SELECT file_id, generation, manifest_hash, name, plaintext_hash, plaintext_size
FROM files_v1;

INSERT INTO file_chunks (
    file_id, generation, ordinal, envelope_version, plaintext_len, nonce,
    ciphertext_hash, ciphertext_len
)
SELECT f.file_id, f.generation, c.ordinal, c.envelope_version, c.plaintext_len, c.nonce,
       c.ciphertext_hash, c.ciphertext_len
FROM files_v1 AS f
JOIN upload_chunks AS c ON c.upload_id = f.upload_id
WHERE c.ciphertext_hash IS NOT NULL AND c.ciphertext_len IS NOT NULL;

DROP TABLE files_v1;
DROP TABLE placements_v1;
PRAGMA user_version = 3;
COMMIT;
"#,
        )?;
        Ok(())
    })();
    if migration.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migration?;
    connection.execute_batch(BASE_SCHEMA)?;
    Ok(())
}

fn migrate_recovery_schema_to_v3(
    connection: &mut Connection,
    add_upload_envelope: bool,
    add_file_envelope: bool,
) -> Result<(), MetadataError> {
    let migration: Result<(), MetadataError> = (|| {
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        if add_upload_envelope {
            connection.execute_batch(
                "ALTER TABLE upload_chunks
                 ADD COLUMN envelope_version INTEGER NOT NULL DEFAULT 1
                 CHECK(envelope_version > 0);",
            )?;
        }
        if add_file_envelope {
            connection.execute_batch(
                "ALTER TABLE file_chunks
                 ADD COLUMN envelope_version INTEGER NOT NULL DEFAULT 1
                 CHECK(envelope_version > 0);",
            )?;
        }
        connection.execute_batch("PRAGMA user_version = 3; COMMIT;")?;
        Ok(())
    })();
    if migration.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    migration?;
    connection.execute_batch(BASE_SCHEMA)?;
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, MetadataError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
             )",
            [table],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn table_has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, MetadataError> {
    let pragma = match table {
        "upload_chunks" => "PRAGMA table_info(upload_chunks)",
        "file_chunks" => "PRAGMA table_info(file_chunks)",
        "placements" => "PRAGMA table_info(placements)",
        _ => return Err(MetadataError::UnknownSchemaTable(table.to_owned())),
    };
    let mut statement = connection.prepare(pragma)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("invalid upload state: {0}")]
    InvalidState(String),
    #[error("invalid file state: {0}")]
    InvalidFileState(String),
    #[error("invalid delete-operation state: {0}")]
    InvalidDeleteState(String),
    #[error("invalid placement state: {0}")]
    InvalidPlacementState(String),
    #[error("invalid agent incarnation status: {0}")]
    InvalidIncarnationStatus(String),
    #[error("invalid reconcile job status: {0}")]
    InvalidJobStatus(String),
    #[error("invalid object health state: {0}")]
    InvalidHealthState(String),
    #[error("agent {agent_id} presented retired incarnation {incarnation_id}; a retired incarnation cannot rejoin")]
    RetiredIncarnation {
        agent_id: String,
        incarnation_id: String,
    },
    #[error("agent {agent_id} presented superseded incarnation {incarnation_id}; two incarnations cannot alternate under one agent id")]
    SupersededIncarnation {
        agent_id: String,
        incarnation_id: String,
    },
    #[error("deleted file has no deletion marker hash")]
    MissingDeletionMarker,
    #[error(
        "master key {presented} does not match the key {bound} this database was created with"
    )]
    MasterKeyMismatch { bound: String, presented: String },
    #[error("reconcile job is not running: {0}")]
    MissingJob(Uuid),
    #[error("scan observation is not running: {0}")]
    MissingScan(Uuid),
    #[error("illegal upload transition from {current:?} to {next:?}")]
    IllegalTransition {
        current: UploadState,
        next: UploadState,
    },
    #[error("persisted encryption or manifest plan conflicts with retry")]
    PlanConflict,
    #[error("placement row is missing")]
    MissingPlacement,
    #[error("manifest plan is missing")]
    MissingManifest,
    #[error("chunk object plan is incomplete")]
    MissingChunkObject,
    #[error("committed upload has no visible file record")]
    MissingCommittedFile,
    #[error("file does not exist: {0}")]
    MissingFile(Uuid),
    #[error("file is not committed: {0}")]
    FileNotCommitted(Uuid),
    #[error("file generation conflicts with persisted state: {0}")]
    GenerationConflict(Uuid),
    #[error("delete operation disappeared after commit")]
    MissingDeleteOperation,
    #[error("replica floor is not met for {kind} {hash}")]
    ReplicaFloorNotMet { kind: ObjectKind, hash: ObjectHash },
    #[error("invalid persisted binary field: {0}")]
    InvalidBinaryField(String),
    #[error("numeric value does not fit the SQLite representation")]
    NumericOverflow,
    #[error("unsupported SQLite schema version {0}")]
    UnsupportedSchemaVersion(i64),
    #[error("SQLite schema version {0} does not match a recognized layout")]
    AmbiguousSchemaVersion(i64),
    #[error("unsupported envelope version {0}")]
    UnsupportedEnvelopeVersion(u16),
    #[error("schema inspection does not allow table {0}")]
    UnknownSchemaTable(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Object(#[from] crate::object::ObjectError),
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_SCHEMA: &str = r#"
CREATE TABLE uploads (upload_id TEXT PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE,
request_fingerprint BLOB NOT NULL, file_id TEXT NOT NULL UNIQUE, file_name TEXT NOT NULL,
plaintext_hash BLOB NOT NULL, plaintext_size INTEGER NOT NULL, storage_class TEXT NOT NULL,
content_key_nonce BLOB NOT NULL, wrapped_content_key BLOB NOT NULL, state TEXT NOT NULL,
generation INTEGER NOT NULL, manifest_hash TEXT, manifest_bytes BLOB, created_at INTEGER NOT NULL DEFAULT (unixepoch()));
CREATE TABLE upload_chunks (upload_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
plaintext_len INTEGER NOT NULL, plaintext_hash BLOB NOT NULL, nonce BLOB NOT NULL,
ciphertext_hash TEXT, ciphertext_len INTEGER, PRIMARY KEY(upload_id, ordinal));
CREATE TABLE placements (object_kind TEXT NOT NULL, object_hash TEXT NOT NULL,
agent_id TEXT NOT NULL, failure_domain TEXT NOT NULL, state TEXT NOT NULL,
confirmed_at INTEGER, PRIMARY KEY(object_kind, object_hash, agent_id));
CREATE TABLE files (file_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, name TEXT NOT NULL,
plaintext_hash BLOB NOT NULL, plaintext_size INTEGER NOT NULL, manifest_hash TEXT NOT NULL,
upload_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()));
PRAGMA user_version = 1;
"#;

    #[test]
    fn state_machine_rejects_regression_and_skips() {
        assert!(UploadState::Staging.permits(UploadState::ReplicatingChunks));
        assert!(!UploadState::Staging.permits(UploadState::Committed));
        assert!(!UploadState::ReplicatingManifest.permits(UploadState::ReplicatingChunks));
        assert!(UploadState::Committed.permits(UploadState::Committed));
    }

    #[test]
    fn schema_v1_migrates_committed_uploads_without_rewriting_manifest_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(V1_SCHEMA).unwrap();
        let upload_id = Uuid::new_v4();
        let file_id = Uuid::new_v4();
        let manifest_hash = ObjectHash::digest(b"persisted manifest bytes");
        connection
            .execute(
                "INSERT INTO uploads (upload_id, idempotency_key, request_fingerprint, file_id,
             file_name, plaintext_hash, plaintext_size, storage_class, content_key_nonce,
             wrapped_content_key, state, generation, manifest_hash, manifest_bytes)
             VALUES (?1, 'stable-key', ?2, ?3, 'empty.bin', ?4, 0, 'regular-rf2',
                     ?5, ?6, 'committed', 1, ?7, ?8)",
                params![
                    upload_id.to_string(),
                    [1_u8; 32].as_slice(),
                    file_id.to_string(),
                    blake3::hash(b"").as_bytes().as_slice(),
                    [2_u8; NONCE_LEN].as_slice(),
                    vec![3_u8; 48],
                    manifest_hash.as_str(),
                    b"persisted manifest bytes".as_slice(),
                ],
            )
            .unwrap();
        connection.execute(
            "INSERT INTO files (file_id, generation, name, plaintext_hash, plaintext_size,
             manifest_hash, upload_id, state) VALUES (?1, 1, 'empty.bin', ?2, 0, ?3, ?4, 'committed')",
            params![
                file_id.to_string(),
                blake3::hash(b"").as_bytes().as_slice(),
                manifest_hash.as_str(),
                upload_id.to_string(),
            ],
        )
        .unwrap();
        drop(connection);

        let database = Database::open(&path).unwrap();
        let file = database.file_by_id(file_id).unwrap().unwrap();
        assert_eq!(file.manifest_hash, manifest_hash);
        let upload = database
            .upload_by_idempotency("stable-key")
            .unwrap()
            .unwrap();
        assert_eq!(upload.state, UploadState::Committed);
        assert_eq!(upload.manifest_bytes.unwrap(), b"persisted manifest bytes");
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            table_has_column(&database.connection, "upload_chunks", "envelope_version").unwrap()
        );
        assert!(table_has_column(&database.connection, "file_chunks", "envelope_version").unwrap());
    }

    #[test]
    fn audited_v2_migrates_to_v3_without_rewriting_manifest_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audited-v2.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(V1_SCHEMA).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE upload_chunks
                 ADD COLUMN envelope_version INTEGER NOT NULL DEFAULT 1
                 CHECK(envelope_version > 0);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        let upload_id = Uuid::new_v4();
        let file_id = Uuid::new_v4();
        let manifest_bytes = b"audited v2 manifest bytes";
        let manifest_hash = ObjectHash::digest(manifest_bytes);
        connection
            .execute(
                "INSERT INTO uploads (upload_id, idempotency_key, request_fingerprint, file_id,
                 file_name, plaintext_hash, plaintext_size, storage_class, content_key_nonce,
                 wrapped_content_key, state, generation, manifest_hash, manifest_bytes)
                 VALUES (?1, 'audited-v2', ?2, ?3, 'empty.bin', ?4, 0, 'regular-rf2',
                         ?5, ?6, 'committed', 1, ?7, ?8)",
                params![
                    upload_id.to_string(),
                    [1_u8; 32].as_slice(),
                    file_id.to_string(),
                    blake3::hash(b"").as_bytes().as_slice(),
                    [2_u8; NONCE_LEN].as_slice(),
                    vec![3_u8; 48],
                    manifest_hash.as_str(),
                    manifest_bytes.as_slice(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO files (file_id, generation, name, plaintext_hash, plaintext_size,
                 manifest_hash, upload_id, state)
                 VALUES (?1, 1, 'empty.bin', ?2, 0, ?3, ?4, 'committed')",
                params![
                    file_id.to_string(),
                    blake3::hash(b"").as_bytes().as_slice(),
                    manifest_hash.as_str(),
                    upload_id.to_string(),
                ],
            )
            .unwrap();
        drop(connection);

        let database = Database::open(&path).unwrap();
        assert_eq!(
            database
                .upload_by_idempotency("audited-v2")
                .unwrap()
                .unwrap()
                .manifest_bytes
                .unwrap(),
            manifest_bytes
        );
        assert_eq!(
            database.file_by_id(file_id).unwrap().unwrap().manifest_hash,
            manifest_hash
        );
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(table_has_column(&database.connection, "file_chunks", "envelope_version").unwrap());
    }

    #[test]
    fn recovery_v2_shape_migrates_to_v3_with_v1_envelope_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recovery-v2.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE upload_chunks (
                    upload_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    plaintext_len INTEGER NOT NULL,
                    plaintext_hash BLOB NOT NULL,
                    nonce BLOB NOT NULL,
                    ciphertext_hash TEXT,
                    ciphertext_len INTEGER,
                    PRIMARY KEY(upload_id, ordinal)
                 );
                 CREATE TABLE file_manifests (
                    file_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    manifest_hash TEXT NOT NULL,
                    name TEXT NOT NULL,
                    plaintext_hash BLOB NOT NULL,
                    plaintext_size INTEGER NOT NULL,
                    PRIMARY KEY(file_id, generation)
                 );
                 CREATE TABLE file_chunks (
                    file_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    plaintext_len INTEGER NOT NULL,
                    nonce BLOB NOT NULL,
                    ciphertext_hash TEXT NOT NULL,
                    ciphertext_len INTEGER NOT NULL,
                    PRIMARY KEY(file_id, generation, ordinal)
                 );
                 INSERT INTO upload_chunks
                    (upload_id, ordinal, plaintext_len, plaintext_hash, nonce)
                    VALUES ('upload', 0, 3, zeroblob(32), zeroblob(24));
                 INSERT INTO file_manifests
                    (file_id, generation, manifest_hash, name, plaintext_hash, plaintext_size)
                    VALUES ('file', 1, 'manifest', 'file.bin', zeroblob(32), 3);
                 INSERT INTO file_chunks
                    (file_id, generation, ordinal, plaintext_len, nonce,
                     ciphertext_hash, ciphertext_len)
                    VALUES ('file', 1, 0, 3, zeroblob(24), 'chunk', 19);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        drop(connection);

        let database = Database::open(&path).unwrap();
        let upload_version: u16 = database
            .connection
            .query_row(
                "SELECT envelope_version FROM upload_chunks WHERE upload_id = 'upload'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let file_version: u16 = database
            .connection
            .query_row(
                "SELECT envelope_version FROM file_chunks WHERE file_id = 'file'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(upload_version, ENVELOPE_VERSION);
        assert_eq!(file_version, ENVELOPE_VERSION);
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn schema_v3_placements_migrate_to_v4_and_adopt_the_first_incarnation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v3.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(BASE_SCHEMA).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE placements_shadow AS SELECT * FROM placements;
                 DROP TABLE placements;
                 CREATE TABLE placements (
                    object_kind TEXT NOT NULL,
                    object_hash TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    failure_domain TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed')),
                    confirmed_at INTEGER,
                    PRIMARY KEY(object_kind, object_hash, agent_id)
                 );
                 DROP TABLE placements_shadow;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        let hash = ObjectHash::digest(b"object");
        connection
            .execute(
                "INSERT INTO placements VALUES ('chunk', ?1, 'agent-a', 'host-a', 'confirmed', 7)",
                [hash.as_str()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO placements VALUES ('chunk', ?1, 'agent-b', 'host-b', 'confirmed', 8)",
                [hash.as_str()],
            )
            .unwrap();
        drop(connection);

        let mut database = Database::open(&path).unwrap();
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            database
                .confirmed_domains(ObjectKind::Chunk, &hash)
                .unwrap(),
            2
        );

        let outcome = database
            .observe_agent_incarnation("agent-a", "host-a", "inc-a1")
            .unwrap();
        assert_eq!(
            outcome,
            IncarnationObservation::Adopted {
                adopted_placements: 1
            }
        );
        assert_eq!(
            database
                .observe_agent_incarnation("agent-a", "host-a", "inc-a1")
                .unwrap(),
            IncarnationObservation::Known
        );
        assert_eq!(
            database
                .confirmed_domains(ObjectKind::Chunk, &hash)
                .unwrap(),
            2
        );

        let outcome = database
            .observe_agent_incarnation("agent-a", "host-a", "inc-a2")
            .unwrap();
        assert_eq!(
            outcome,
            IncarnationObservation::Superseded {
                previous: "inc-a1".to_owned()
            }
        );
        assert_eq!(
            database
                .confirmed_domains(ObjectKind::Chunk, &hash)
                .unwrap(),
            1
        );
        assert!(!database
            .placement_confirmed(ObjectKind::Chunk, &hash, "agent-a")
            .unwrap());
        assert!(matches!(
            database.observe_agent_incarnation("agent-a", "host-a", "inc-a1"),
            Err(MetadataError::SupersededIncarnation { .. })
        ));

        database
            .ensure_pending_placement(ObjectKind::Chunk, &hash, "agent-a", "host-a", "inc-a2")
            .unwrap();
        let states = database.placement_states(ObjectKind::Chunk, &hash).unwrap();
        assert_eq!(
            states[0],
            (
                "agent-a".to_owned(),
                PlacementState::Pending,
                Some("inc-a2".to_owned())
            )
        );
        database
            .confirm_placement(ObjectKind::Chunk, &hash, "agent-a")
            .unwrap();
        assert_eq!(
            database
                .confirmed_domains(ObjectKind::Chunk, &hash)
                .unwrap(),
            2
        );
    }

    #[test]
    fn interrupted_jobs_are_marked_on_open_and_scans_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jobs.sqlite");
        let mut database = Database::open(&path).unwrap();
        let job = database.create_reconcile_job(JobMode::Verify).unwrap();
        let scan = database
            .start_scan(job, "agent-a", "inc-a1", ObjectKind::Chunk)
            .unwrap();
        drop(database);

        let mut database = Database::open(&path).unwrap();
        let latest = database.latest_reconcile_job().unwrap().unwrap();
        assert_eq!(latest.job_id, job);
        assert_eq!(latest.status, JobStatus::Interrupted);
        assert!(matches!(
            database.complete_scan(&ScanCompletion {
                scan_id: scan,
                final_cursor: None,
                object_count: 0,
                inventory_digest: "digest".to_owned(),
            }),
            Err(MetadataError::MissingScan(_))
        ));
        assert!(matches!(
            database.finish_reconcile_job(job, JobStatus::Complete, None),
            Err(MetadataError::MissingJob(_))
        ));
        assert!(database.latest_complete_scans().unwrap().is_empty());
    }

    #[test]
    fn master_key_binding_adopts_then_refuses_other_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("meta.sqlite");
        let mut database = Database::open(&path).unwrap();
        assert_eq!(database.master_key_id().unwrap(), None);
        assert_eq!(
            database.bind_master_key_id("aa").unwrap(),
            KeyBinding::Bound
        );
        assert_eq!(
            database.bind_master_key_id("aa").unwrap(),
            KeyBinding::Matched
        );
        assert!(matches!(
            database.bind_master_key_id("bb"),
            Err(MetadataError::MasterKeyMismatch { .. })
        ));
        assert_eq!(database.master_key_id().unwrap().as_deref(), Some("aa"));
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn future_schema_versions_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("future.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .unwrap();
        drop(connection);
        assert!(matches!(
            Database::open(&path),
            Err(MetadataError::UnsupportedSchemaVersion(99))
        ));
    }
}

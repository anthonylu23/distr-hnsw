use std::{path::Path, str::FromStr};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{WrappedKey, ENVELOPE_VERSION, NONCE_LEN},
    durability::{ensure_directory, sync_directory},
    object::{ObjectHash, ObjectKind},
};

const SCHEMA_VERSION: i64 = 3;

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
            0 => connection.execute_batch(SCHEMA)?,
            1 => migrate_v1_to_v3(&mut connection)?,
            2 => migrate_v2_to_v3(&mut connection)?,
            SCHEMA_VERSION => connection.execute_batch(SCHEMA)?,
            _ => return Err(MetadataError::UnsupportedSchemaVersion(version)),
        }
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(Self { connection })
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

    pub fn ensure_pending_placement(
        &mut self,
        kind: ObjectKind,
        hash: &ObjectHash,
        agent_id: &str,
        failure_domain: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO placements
             (object_kind, object_hash, agent_id, failure_domain, state)
             VALUES (?1, ?2, ?3, ?4, 'pending')
             ON CONFLICT(object_kind, object_hash, agent_id) DO NOTHING",
            params![kind.as_str(), hash.as_str(), agent_id, failure_domain],
        )?;
        Ok(())
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
        let state: Option<String> = self
            .connection
            .query_row(
                "SELECT state FROM placements
                 WHERE object_kind = ?1 AND object_hash = ?2 AND agent_id = ?3",
                params![kind.as_str(), hash.as_str(), agent_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(state.as_deref() == Some("confirmed"))
    }

    pub fn confirmed_domains(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<usize, MetadataError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(DISTINCT failure_domain) FROM placements
             WHERE object_kind = ?1 AND object_hash = ?2 AND state = 'confirmed'",
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
        transaction.commit()?;
        self.delete_by_idempotency(&operation.idempotency_key)?
            .ok_or(MetadataError::MissingDeleteOperation)
    }

    pub fn confirmed_agents(
        &self,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> Result<Vec<String>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT agent_id FROM placements
             WHERE object_kind = ?1 AND object_hash = ?2 AND state = 'confirmed'
             ORDER BY agent_id",
        )?;
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
        transaction.commit()?;
        Ok(())
    }
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
        "SELECT COUNT(DISTINCT failure_domain) FROM placements
         WHERE object_kind = ?1 AND object_hash = ?2 AND state = 'confirmed'",
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

const SCHEMA: &str = r#"
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
    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed')),
    confirmed_at INTEGER,
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

PRAGMA user_version = 3;
"#;

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
    connection.execute_batch(SCHEMA)?;
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
    connection.execute_batch(SCHEMA)?;
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

use std::{path::Path, str::FromStr};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{WrappedKey, NONCE_LEN},
    durability::{ensure_directory, sync_directory},
    object::{ObjectHash, ObjectKind},
};

const SCHEMA_VERSION: i64 = 1;

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

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, MetadataError> {
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            return Err(MetadataError::UnsupportedSchemaVersion(version));
        }
        connection.execute_batch(SCHEMA)?;
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
            transaction.execute(
                "INSERT INTO upload_chunks
                 (upload_id, ordinal, plaintext_len, plaintext_hash, nonce)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    upload.upload_id.to_string(),
                    chunk.ordinal,
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
            "SELECT ordinal, plaintext_len, plaintext_hash, nonce,
                    ciphertext_hash, ciphertext_len
             FROM upload_chunks WHERE upload_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([upload_id.to_string()], |row| {
            let plaintext_hash: Vec<u8> = row.get(2)?;
            let nonce: Vec<u8> = row.get(3)?;
            let hash: Option<String> = row.get(4)?;
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, u32>(1)?,
                plaintext_hash,
                nonce,
                hash,
                row.get::<_, Option<u32>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (ordinal, plaintext_len, plaintext_hash, nonce, hash, ciphertext_len) = row?;
            Ok(ChunkPlan {
                ordinal,
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
            "SELECT ciphertext_hash FROM upload_chunks
             WHERE upload_id = ?1 ORDER BY ordinal",
        )?;
        let hashes = statement
            .query_map([upload_id.to_string()], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for hash in hashes {
            let hash = hash.ok_or(MetadataError::MissingChunkObject)?;
            let hash = ObjectHash::parse(hash)?;
            require_replica_floor(&transaction, ObjectKind::Chunk, &hash, minimum_replicas)?;
        }

        transaction.execute(
            "INSERT INTO files
             (file_id, generation, name, plaintext_hash, plaintext_size,
              manifest_hash, upload_id, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'committed')",
            params![
                upload.file_id.to_string(),
                to_i64(upload.generation)?,
                upload.file_name,
                upload.plaintext_hash.as_slice(),
                to_i64(upload.plaintext_size)?,
                manifest_hash.as_str(),
                upload.upload_id.to_string(),
            ],
        )?;
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
            "SELECT file_id, generation, name, plaintext_hash, plaintext_size, manifest_hash
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
            "SELECT file_id, generation, name, plaintext_hash, plaintext_size, manifest_hash
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
    plaintext_len INTEGER NOT NULL CHECK(plaintext_len >= 0),
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    nonce BLOB NOT NULL CHECK(length(nonce) = 24),
    ciphertext_hash TEXT,
    ciphertext_len INTEGER,
    PRIMARY KEY(upload_id, ordinal),
    CHECK ((ciphertext_hash IS NULL) = (ciphertext_len IS NULL))
);

CREATE TABLE IF NOT EXISTS placements (
    object_kind TEXT NOT NULL CHECK(object_kind IN ('chunk', 'manifest')),
    object_hash TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    failure_domain TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending', 'confirmed')),
    confirmed_at INTEGER,
    PRIMARY KEY(object_kind, object_hash, agent_id)
);

CREATE TABLE IF NOT EXISTS files (
    file_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL CHECK(generation > 0),
    name TEXT NOT NULL,
    plaintext_hash BLOB NOT NULL CHECK(length(plaintext_hash) = 32),
    plaintext_size INTEGER NOT NULL CHECK(plaintext_size >= 0),
    manifest_hash TEXT NOT NULL,
    upload_id TEXT NOT NULL UNIQUE REFERENCES uploads(upload_id),
    state TEXT NOT NULL CHECK(state = 'committed'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

PRAGMA user_version = 1;
"#;

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("invalid upload state: {0}")]
    InvalidState(String),
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
    #[error("replica floor is not met for {kind} {hash}")]
    ReplicaFloorNotMet { kind: ObjectKind, hash: ObjectHash },
    #[error("invalid persisted binary field: {0}")]
    InvalidBinaryField(String),
    #[error("numeric value does not fit the SQLite representation")]
    NumericOverflow,
    #[error("unsupported SQLite schema version {0}")]
    UnsupportedSchemaVersion(i64),
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

    #[test]
    fn state_machine_rejects_regression_and_skips() {
        assert!(UploadState::Staging.permits(UploadState::ReplicatingChunks));
        assert!(!UploadState::Staging.permits(UploadState::Committed));
        assert!(!UploadState::ReplicatingManifest.permits(UploadState::ReplicatingChunks));
        assert!(UploadState::Committed.permits(UploadState::Committed));
    }
}

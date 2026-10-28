//! Schema v11 (additive): users and their identities, browser sessions, API
//! keys, and the append-only audit log (DESIGN §9;
//! docs/m2-implementation-plan.md, decision 8). No secret is stored:
//! sessions and API keys keep only BLAKE3 verifiers of 256-bit tokens.

use rusqlite::{params, OptionalExtension, Row};
use uuid::Uuid;

use super::{Database, MetadataError};

pub(super) const IDENTITY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    user_id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('owner')),
    created_at INTEGER NOT NULL,
    disabled_at INTEGER
);

CREATE TABLE IF NOT EXISTS identities (
    provider TEXT NOT NULL CHECK(provider IN ('tailscale')),
    external_id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(user_id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(provider, external_id)
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    verifier BLOB NOT NULL UNIQUE CHECK(length(verifier) = 32),
    user_id TEXT NOT NULL REFERENCES users(user_id),
    node_stable_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoke_reason TEXT
);
CREATE INDEX IF NOT EXISTS sessions_live_by_user
    ON sessions(user_id) WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS api_keys (
    key_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(user_id),
    name TEXT NOT NULL,
    verifier BLOB NOT NULL CHECK(length(verifier) = 32),
    scopes TEXT NOT NULL,
    node_stable_id TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    revoked_at INTEGER,
    last_used_at INTEGER
);

CREATE TABLE IF NOT EXISTS audit_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    event TEXT NOT NULL,
    outcome TEXT NOT NULL
        CHECK(outcome IN ('allowed', 'completed', 'denied', 'failed', 'observed')),
    reason TEXT,
    peer TEXT,
    node_stable_id TEXT,
    node_name TEXT,
    user_login TEXT,
    user_id TEXT,
    session_id TEXT,
    api_key_id TEXT,
    method TEXT,
    path TEXT,
    target TEXT,
    detail TEXT
);
CREATE TRIGGER IF NOT EXISTS audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only');
END;
CREATE TRIGGER IF NOT EXISTS audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only');
END;
"#;

/// The only identity provider in v1: Tailscale WhoIs, keyed by login name
/// (the name tailnet ACLs use).
pub const TAILSCALE_PROVIDER: &str = "tailscale";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRow {
    pub user_id: Uuid,
    pub display_name: String,
    pub role: String,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
    /// Tailscale login names that resolve to this user.
    pub logins: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    pub session_id: Uuid,
    pub user_id: Uuid,
    /// The tailnet node the session was created from; it is valid only
    /// from that node.
    pub node_stable_id: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKeyRow {
    pub key_id: String,
    pub user_id: Uuid,
    pub name: String,
    pub verifier: [u8; 32],
    /// Collection scopes, `collection:right`, space-separated.
    pub scopes: String,
    /// When set, the key is accepted only from this tailnet node.
    pub node_stable_id: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

/// One audit-log entry. `seq` is assigned on append.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditRecord {
    pub seq: Option<i64>,
    pub at: i64,
    pub event: String,
    /// `allowed` (authorized; for a state change, written before it runs),
    /// `completed` or `failed` (how an allowed state change ended),
    /// `denied`, or `observed`.
    pub outcome: String,
    pub reason: Option<String>,
    pub peer: Option<String>,
    pub node_stable_id: Option<String>,
    pub node_name: Option<String>,
    pub user_login: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub api_key_id: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub target: Option<String>,
    pub detail: Option<String>,
}

fn uuid_column(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let text: String = row.get(index)?;
    Uuid::parse_str(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, error.into())
    })
}

fn verifier_column(row: &Row<'_>, index: usize) -> rusqlite::Result<[u8; 32]> {
    let bytes: Vec<u8> = row.get(index)?;
    bytes.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            "verifier is not 32 bytes".into(),
        )
    })
}

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: uuid_column(row, 0)?,
        user_id: uuid_column(row, 1)?,
        node_stable_id: row.get(2)?,
        created_at: row.get(3)?,
        last_seen_at: row.get(4)?,
        expires_at: row.get(5)?,
        revoked_at: row.get(6)?,
    })
}

const SESSION_COLUMNS: &str =
    "session_id, user_id, node_stable_id, created_at, last_seen_at, expires_at, revoked_at";

fn api_key_row(row: &Row<'_>) -> rusqlite::Result<ApiKeyRow> {
    Ok(ApiKeyRow {
        key_id: row.get(0)?,
        user_id: uuid_column(row, 1)?,
        name: row.get(2)?,
        verifier: verifier_column(row, 3)?,
        scopes: row.get(4)?,
        node_stable_id: row.get(5)?,
        created_at: row.get(6)?,
        expires_at: row.get(7)?,
        revoked_at: row.get(8)?,
        last_used_at: row.get(9)?,
    })
}

const API_KEY_COLUMNS: &str = "key_id, user_id, name, verifier, scopes, node_stable_id, \
     created_at, expires_at, revoked_at, last_used_at";

impl Database {
    /// Create a user with one Tailscale identity. A login that already
    /// belongs to a user is refused.
    pub fn add_user(
        &mut self,
        display_name: &str,
        login: &str,
        now: i64,
    ) -> Result<UserRow, MetadataError> {
        let transaction = self.connection.transaction()?;
        let taken: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM identities WHERE provider = ?1 AND external_id = ?2)",
            params![TAILSCALE_PROVIDER, login],
            |row| row.get(0),
        )?;
        if taken {
            return Err(MetadataError::InvalidState(format!(
                "tailscale login {login} already belongs to a user"
            )));
        }
        let user_id = Uuid::new_v4();
        transaction.execute(
            "INSERT INTO users(user_id, display_name, role, created_at) VALUES (?1, ?2, 'owner', ?3)",
            params![user_id.to_string(), display_name, now],
        )?;
        transaction.execute(
            "INSERT INTO identities(provider, external_id, user_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![TAILSCALE_PROVIDER, login, user_id.to_string(), now],
        )?;
        transaction.commit()?;
        Ok(UserRow {
            user_id,
            display_name: display_name.to_owned(),
            role: "owner".into(),
            created_at: now,
            disabled_at: None,
            logins: vec![login.to_owned()],
        })
    }

    pub fn users(&self) -> Result<Vec<UserRow>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT user_id, display_name, role, created_at, disabled_at
             FROM users ORDER BY created_at, user_id",
        )?;
        let mut users = statement
            .query_map([], |row| {
                Ok(UserRow {
                    user_id: uuid_column(row, 0)?,
                    display_name: row.get(1)?,
                    role: row.get(2)?,
                    created_at: row.get(3)?,
                    disabled_at: row.get(4)?,
                    logins: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut logins = self.connection.prepare(
            "SELECT external_id FROM identities
             WHERE provider = ?1 AND user_id = ?2 ORDER BY external_id",
        )?;
        for user in &mut users {
            user.logins = logins
                .query_map(
                    params![TAILSCALE_PROVIDER, user.user_id.to_string()],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(users)
    }

    pub fn user(&self, user_id: Uuid) -> Result<Option<UserRow>, MetadataError> {
        Ok(self
            .users()?
            .into_iter()
            .find(|user| user.user_id == user_id))
    }

    /// The user a Tailscale login resolves to, if any.
    pub fn user_by_login(&self, login: &str) -> Result<Option<UserRow>, MetadataError> {
        let user_id: Option<String> = self
            .connection
            .query_row(
                "SELECT user_id FROM identities WHERE provider = ?1 AND external_id = ?2",
                params![TAILSCALE_PROVIDER, login],
                |row| row.get(0),
            )
            .optional()?;
        match user_id {
            Some(user_id) => self.user(
                Uuid::parse_str(&user_id)
                    .map_err(|_| MetadataError::InvalidState("malformed user id".into()))?,
            ),
            None => Ok(None),
        }
    }

    /// Disable a user and revoke every session and API key they hold (a
    /// privilege change). Returns the number of sessions and keys revoked.
    pub fn disable_user(
        &mut self,
        user_id: Uuid,
        now: i64,
    ) -> Result<(usize, usize), MetadataError> {
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE users SET disabled_at = ?2 WHERE user_id = ?1 AND disabled_at IS NULL",
            params![user_id.to_string(), now],
        )?;
        if changed != 1 {
            return Err(MetadataError::InvalidState(format!(
                "user {user_id} does not exist or is already disabled"
            )));
        }
        let sessions = transaction.execute(
            "UPDATE sessions SET revoked_at = ?2, revoke_reason = 'user_disabled'
             WHERE user_id = ?1 AND revoked_at IS NULL",
            params![user_id.to_string(), now],
        )?;
        let keys = transaction.execute(
            "UPDATE api_keys SET revoked_at = ?2 WHERE user_id = ?1 AND revoked_at IS NULL",
            params![user_id.to_string(), now],
        )?;
        transaction.commit()?;
        Ok((sessions, keys))
    }

    pub fn insert_session(
        &mut self,
        session: &SessionRow,
        verifier: &[u8; 32],
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO sessions(session_id, verifier, user_id, node_stable_id, created_at,
                                  last_seen_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.session_id.to_string(),
                verifier.as_slice(),
                session.user_id.to_string(),
                session.node_stable_id,
                session.created_at,
                session.last_seen_at,
                session.expires_at,
            ],
        )?;
        Ok(())
    }

    /// The session whose token has this verifier, revoked or not.
    pub fn session_by_verifier(
        &self,
        verifier: &[u8; 32],
    ) -> Result<Option<SessionRow>, MetadataError> {
        Ok(self
            .connection
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE verifier = ?1"),
                params![verifier.as_slice()],
                session_row,
            )
            .optional()?)
    }

    pub fn touch_session(&mut self, session_id: Uuid, now: i64) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sessions SET last_seen_at = ?2 WHERE session_id = ?1 AND revoked_at IS NULL",
            params![session_id.to_string(), now],
        )?;
        Ok(())
    }

    /// Revoke a live session; false if it was already revoked or unknown.
    pub fn revoke_session(
        &mut self,
        session_id: Uuid,
        now: i64,
        reason: &str,
    ) -> Result<bool, MetadataError> {
        Ok(self.connection.execute(
            "UPDATE sessions SET revoked_at = ?2, revoke_reason = ?3
             WHERE session_id = ?1 AND revoked_at IS NULL",
            params![session_id.to_string(), now, reason],
        )? == 1)
    }

    /// Delete sessions that ended (expired or revoked) before `cutoff`.
    pub fn prune_sessions(&mut self, cutoff: i64) -> Result<usize, MetadataError> {
        Ok(self.connection.execute(
            "DELETE FROM sessions WHERE expires_at < ?1 OR revoked_at < ?1",
            params![cutoff],
        )?)
    }

    pub fn insert_api_key(&mut self, key: &ApiKeyRow) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO api_keys(key_id, user_id, name, verifier, scopes, node_stable_id,
                                  created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                key.key_id,
                key.user_id.to_string(),
                key.name,
                key.verifier.as_slice(),
                key.scopes,
                key.node_stable_id,
                key.created_at,
                key.expires_at,
            ],
        )?;
        Ok(())
    }

    pub fn api_key(&self, key_id: &str) -> Result<Option<ApiKeyRow>, MetadataError> {
        Ok(self
            .connection
            .query_row(
                &format!("SELECT {API_KEY_COLUMNS} FROM api_keys WHERE key_id = ?1"),
                params![key_id],
                api_key_row,
            )
            .optional()?)
    }

    /// API keys, newest first; only `user_id`'s when given.
    pub fn api_keys(&self, user_id: Option<Uuid>) -> Result<Vec<ApiKeyRow>, MetadataError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {API_KEY_COLUMNS} FROM api_keys
             WHERE ?1 IS NULL OR user_id = ?1 ORDER BY created_at DESC, key_id"
        ))?;
        let keys = statement
            .query_map(params![user_id.map(|id| id.to_string())], api_key_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    /// Revoke a live key, only `user_id`'s when given; false if no such
    /// live key.
    pub fn revoke_api_key(
        &mut self,
        key_id: &str,
        user_id: Option<Uuid>,
        now: i64,
    ) -> Result<bool, MetadataError> {
        Ok(self.connection.execute(
            "UPDATE api_keys SET revoked_at = ?3
             WHERE key_id = ?1 AND (?2 IS NULL OR user_id = ?2) AND revoked_at IS NULL",
            params![key_id, user_id.map(|id| id.to_string()), now],
        )? == 1)
    }

    pub fn touch_api_key(&mut self, key_id: &str, now: i64) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE api_keys SET last_used_at = ?2 WHERE key_id = ?1",
            params![key_id, now],
        )?;
        Ok(())
    }

    /// Append one record to the audit log and return its sequence number.
    pub fn append_audit(&mut self, record: &AuditRecord) -> Result<i64, MetadataError> {
        self.connection.execute(
            "INSERT INTO audit_log(at, event, outcome, reason, peer, node_stable_id, node_name,
                                   user_login, user_id, session_id, api_key_id, method, path,
                                   target, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                record.at,
                record.event,
                record.outcome,
                record.reason,
                record.peer,
                record.node_stable_id,
                record.node_name,
                record.user_login,
                record.user_id,
                record.session_id,
                record.api_key_id,
                record.method,
                record.path,
                record.target,
                record.detail,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Audit records with `seq > after`, oldest first.
    pub fn audit_records(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT seq, at, event, outcome, reason, peer, node_stable_id, node_name, user_login,
                    user_id, session_id, api_key_id, method, path, target, detail
             FROM audit_log WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let records = statement
            .query_map(params![after, limit as i64], |row| {
                Ok(AuditRecord {
                    seq: row.get(0)?,
                    at: row.get(1)?,
                    event: row.get(2)?,
                    outcome: row.get(3)?,
                    reason: row.get(4)?,
                    peer: row.get(5)?,
                    node_stable_id: row.get(6)?,
                    node_name: row.get(7)?,
                    user_login: row.get(8)?,
                    user_id: row.get(9)?,
                    session_id: row.get(10)?,
                    api_key_id: row.get(11)?,
                    method: row.get(12)?,
                    path: row.get(13)?,
                    target: row.get(14)?,
                    detail: row.get(15)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("portal.sqlite")).unwrap();
        (dir, database)
    }

    #[test]
    fn the_audit_log_refuses_updates_and_deletes() {
        let (_dir, mut database) = database();
        let seq = database
            .append_audit(&AuditRecord {
                at: 1,
                event: "login".into(),
                outcome: "allowed".into(),
                ..AuditRecord::default()
            })
            .unwrap();
        for statement in [
            "UPDATE audit_log SET outcome = 'denied'",
            "DELETE FROM audit_log",
        ] {
            let error = database.connection.execute(statement, []).unwrap_err();
            assert!(error.to_string().contains("append-only"), "{error}");
        }
        let records = database.audit_records(0, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, Some(seq));
        assert_eq!(records[0].outcome, "allowed");
    }

    #[test]
    fn a_login_belongs_to_one_user_and_disabling_revokes_everything() {
        let (_dir, mut database) = database();
        let user = database.add_user("Owner", "owner@example.com", 10).unwrap();
        assert!(database.add_user("Other", "owner@example.com", 11).is_err());
        assert_eq!(
            database.user_by_login("owner@example.com").unwrap(),
            Some(user.clone())
        );
        let session = SessionRow {
            session_id: Uuid::new_v4(),
            user_id: user.user_id,
            node_stable_id: "nA".into(),
            created_at: 10,
            last_seen_at: 10,
            expires_at: 100,
            revoked_at: None,
        };
        database.insert_session(&session, &[1; 32]).unwrap();
        database
            .insert_api_key(&ApiKeyRow {
                key_id: "00112233aabbccdd".into(),
                user_id: user.user_id,
                name: "app".into(),
                verifier: [2; 32],
                scopes: "files:read".into(),
                node_stable_id: None,
                created_at: 10,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
            })
            .unwrap();
        assert_eq!(database.disable_user(user.user_id, 20).unwrap(), (1, 1));
        assert_eq!(
            database
                .session_by_verifier(&[1; 32])
                .unwrap()
                .unwrap()
                .revoked_at,
            Some(20)
        );
        assert_eq!(
            database
                .api_key("00112233aabbccdd")
                .unwrap()
                .unwrap()
                .revoked_at,
            Some(20)
        );
        assert!(database.disable_user(user.user_id, 21).is_err());
    }
}

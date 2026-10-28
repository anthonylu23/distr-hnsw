//! The portal's grant-signing keys and join tokens
//! (docs/m2-implementation-plan.md, decisions 3 to 6).
//!
//! Signing-key seeds are stored sealed under the master key (DESIGN §10).
//! The first command that needs to talk to agents creates the first key.
//! The key set lists the active key and any retiring key and is signed by
//! all of them, so an agent that knows only the previous key still accepts
//! a rotated set.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{unwrap_key, wrap_key, CryptoError, MasterKey},
    grant::{KeyId, KeySet, KeyStatus, PortalSigner, SignedKeySet, TrustedKey},
    metadata::{Database, MetadataError, SigningKeyRow, SigningKeyStatus},
    trust::{generate_join_token, join_token_hash},
};

const SIGNING_KEY_PURPOSE: &[u8] = b"distr-hnsw grant signing key v1";
/// How long a join token stays usable.
pub const JOIN_TOKEN_LIFETIME_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum AccessError {
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("stored signing key {0} does not match its public key")]
    KeyMismatch(String),
    #[error("database has no active signing key")]
    NoActiveKey,
}

/// The signing keys, epoch, and key-set version in force.
pub struct GrantMaterial {
    /// Active key first, then retiring keys.
    signers: Vec<(PortalSigner, KeyStatus)>,
    epoch: u64,
    version: u64,
}

fn seal(
    master: &MasterKey,
    signer: &PortalSigner,
    status: SigningKeyStatus,
) -> Result<SigningKeyRow, AccessError> {
    let wrap_id = Uuid::new_v4();
    let wrapped_seed = wrap_key(master, SIGNING_KEY_PURPOSE, wrap_id, 1, &signer.seed())?;
    Ok(SigningKeyRow {
        key_id: signer.key_id().to_string(),
        wrap_id,
        public_key: signer.verifying_key().to_bytes(),
        wrapped_seed,
        status,
    })
}

fn unseal(master: &MasterKey, row: &SigningKeyRow) -> Result<PortalSigner, AccessError> {
    let seed = unwrap_key(
        master,
        SIGNING_KEY_PURPOSE,
        row.wrap_id,
        1,
        &row.wrapped_seed,
    )?;
    let signer = PortalSigner::from_seed(&seed);
    if signer.verifying_key().to_bytes() != row.public_key
        || signer.key_id().to_string() != row.key_id
    {
        return Err(AccessError::KeyMismatch(row.key_id.clone()));
    }
    Ok(signer)
}

impl GrantMaterial {
    /// Load the signing keys, creating the first one if there is none.
    pub fn load_or_create(
        database: &mut Database,
        master: &MasterKey,
    ) -> Result<Self, AccessError> {
        if database.signing_keys()?.is_empty() {
            let row = seal(master, &PortalSigner::generate(), SigningKeyStatus::Active)?;
            // A concurrent creator may win; either way one active key exists.
            database.insert_first_signing_key(&row)?;
        }
        Self::load(database, master)
    }

    pub fn load(database: &Database, master: &MasterKey) -> Result<Self, AccessError> {
        let state = database.grant_state()?;
        let mut signers = Vec::new();
        for row in database.signing_keys()? {
            let status = match row.status {
                SigningKeyStatus::Active => KeyStatus::Active,
                SigningKeyStatus::Retiring => KeyStatus::Retiring,
                SigningKeyStatus::Revoked => continue,
            };
            signers.push((unseal(master, &row)?, status));
        }
        if signers.first().map(|(_, status)| *status) != Some(KeyStatus::Active) {
            return Err(AccessError::NoActiveKey);
        }
        Ok(Self {
            signers,
            epoch: state.epoch,
            version: state.key_set_version,
        })
    }

    /// The key that signs new grants.
    pub fn active(&self) -> &PortalSigner {
        &self.signers[0].0
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The key set agents should hold, signed by every key in it.
    pub fn signed_key_set(&self) -> SignedKeySet {
        let key_set = KeySet {
            version: self.version,
            epoch: self.epoch,
            keys: self
                .signers
                .iter()
                .map(|(signer, status)| TrustedKey {
                    key_id: signer.key_id(),
                    public: signer.verifying_key(),
                    status: *status,
                })
                .collect(),
        };
        let signers: Vec<&PortalSigner> = self.signers.iter().map(|(signer, _)| signer).collect();
        key_set.sign(&signers)
    }
}

/// Create a single-use join token for `agent_id`, returned once in plain
/// text; the database keeps only its BLAKE3 hash.
pub fn create_join_token(
    database: &mut Database,
    agent_id: &str,
    now: i64,
) -> Result<String, AccessError> {
    let token = generate_join_token();
    database.insert_join_token(
        &join_token_hash(&token),
        agent_id,
        now + JOIN_TOKEN_LIFETIME_SECONDS,
    )?;
    Ok(token)
}

/// Make a new key active; the old one keeps verifying as `retiring`.
pub fn rotate_signing_key(
    database: &mut Database,
    master: &MasterKey,
) -> Result<KeyId, AccessError> {
    GrantMaterial::load_or_create(database, master)?;
    let signer = PortalSigner::generate();
    database.rotate_signing_key(&seal(master, &signer, SigningKeyStatus::Active)?)?;
    Ok(signer.key_id())
}

/// Drop a retiring key from the key set (`revoke = false`), or revoke it
/// and raise the grant epoch (`revoke = true`).
pub fn end_signing_key(
    database: &mut Database,
    key_id: &str,
    revoke: bool,
) -> Result<(), AccessError> {
    database.end_signing_key(key_id, revoke)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sealed_created_once_and_rotate_with_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let master = MasterKey::from_bytes([7; 32]);
        let mut database = Database::open(&dir.path().join("portal.sqlite")).unwrap();
        let first = GrantMaterial::load_or_create(&mut database, &master).unwrap();
        let again = GrantMaterial::load_or_create(&mut database, &master).unwrap();
        assert_eq!(first.active().key_id(), again.active().key_id());
        assert_eq!((first.epoch(), first.version()), (1, 1));
        assert!(first.signed_key_set().self_signed());

        // A different master key cannot unseal the seed.
        let wrong = MasterKey::from_bytes([8; 32]);
        assert!(GrantMaterial::load(&database, &wrong).is_err());

        let new_id = rotate_signing_key(&mut database, &master).unwrap();
        let rotated = GrantMaterial::load(&database, &master).unwrap();
        assert_eq!(rotated.active().key_id(), new_id);
        assert_eq!((rotated.epoch(), rotated.version()), (1, 2));
        let set = rotated.signed_key_set();
        assert_eq!(set.key_set.keys.len(), 2);
        assert!(
            set.signed_by(&first.signed_key_set().key_set),
            "old key vouches"
        );

        let old_id = first.active().key_id().to_string();
        assert!(
            end_signing_key(&mut database, &new_id.to_string(), false).is_err(),
            "active key"
        );
        end_signing_key(&mut database, &old_id, false).unwrap();
        let retired = GrantMaterial::load(&database, &master).unwrap();
        assert_eq!(retired.signed_key_set().key_set.keys.len(), 1);
        assert_eq!((retired.epoch(), retired.version()), (1, 3));

        let third = rotate_signing_key(&mut database, &master).unwrap();
        end_signing_key(&mut database, &new_id.to_string(), true).unwrap();
        let revoked = GrantMaterial::load(&database, &master).unwrap();
        assert_eq!(revoked.active().key_id(), third);
        assert_eq!(
            (revoked.epoch(), revoked.version()),
            (2, 5),
            "revocation raises the epoch"
        );
    }
}

//! An agent's trust state on its volume (docs/m2-implementation-plan.md,
//! decisions 5 and 6).
//!
//! Before join, the volume holds only the BLAKE3 hash of a single-use join
//! token. Join replaces it with the portal's signed key set and, on a
//! tailnet agent, the stable id of the portal's node. Later key sets are
//! accepted only if a key the agent already trusts signed them and their
//! version is newer. Files are replaced durably, and the token is removed
//! only after the trust files exist, so a crash between the two leaves a
//! joined agent whose leftover token is removed at the next start.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::RwLock,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;

use crate::{
    durability::{remove_file_durably, replace_file_durably},
    grant::{GrantError, SignedKeySet},
};

const TOKEN_FILE: &str = "join-token";
const KEY_SET_FILE: &str = "trust-key-set";
const PORTAL_NODE_FILE: &str = "trust-portal-node";

/// A fresh single-use join token: 256 random bits, base64url.
pub fn generate_join_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The stored verifier of a join token.
pub fn join_token_hash(token: &str) -> String {
    blake3::hash(token.trim().as_bytes()).to_hex().to_string()
}

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("agent is already joined")]
    AlreadyJoined,
    #[error("agent is not joined")]
    NotJoined,
    #[error("agent has no pending join token")]
    NoJoinToken,
    #[error("join token does not match")]
    BadToken,
    #[error("key set is not signed by a trusted key")]
    Untrusted,
    #[error("key set version {offered} is not newer than {current}")]
    NotNewer { offered: u64, current: u64 },
    #[error("key set epoch {offered} is older than {current}")]
    EpochRegression { offered: u64, current: u64 },
    #[error("malformed trust state in {0}: {1}")]
    Malformed(PathBuf, GrantError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub struct AgentTrust {
    pub key_set: SignedKeySet,
    /// The portal's tailnet node; `None` for an agent joined over loopback.
    pub portal_node: Option<String>,
}

#[derive(Debug)]
pub struct TrustStore {
    root: PathBuf,
    state: RwLock<Option<AgentTrust>>,
}

impl TrustStore {
    /// Load the trust state of the volume at `root`.
    pub fn open(root: &Path) -> Result<Self, TrustError> {
        let key_set_path = root.join(KEY_SET_FILE);
        let state = match fs::read(&key_set_path) {
            Ok(bytes) => {
                let key_set = SignedKeySet::from_bytes(&bytes)
                    .map_err(|error| TrustError::Malformed(key_set_path.clone(), error))?;
                let portal_node = match fs::read_to_string(root.join(PORTAL_NODE_FILE)) {
                    Ok(node) => Some(node.trim().to_owned()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                // A join that crashed after writing trust but before removing
                // the token: finish it.
                remove_file_durably(root, TOKEN_FILE)?;
                Some(AgentTrust {
                    key_set,
                    portal_node,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            root: root.to_owned(),
            state: RwLock::new(state),
        })
    }

    pub fn current(&self) -> Option<AgentTrust> {
        self.state.read().expect("trust lock").clone()
    }

    pub fn is_joined(&self) -> bool {
        self.state.read().expect("trust lock").is_some()
    }

    /// Remember the hash of the token an operator started this agent with.
    /// Ignored once joined; a new token replaces an unused one.
    pub fn arm_join_token(&self, token: &str) -> Result<(), TrustError> {
        if self.is_joined() {
            return Ok(());
        }
        replace_file_durably(&self.root, TOKEN_FILE, join_token_hash(token).as_bytes())?;
        Ok(())
    }

    /// Forget the current portal and arm `token` for a new join. For a
    /// portal rebuilt without its signing keys; an explicit operator
    /// action on the agent's host, which already controls the volume.
    pub fn reset_and_arm(&self, token: &str) -> Result<(), TrustError> {
        let mut state = self.state.write().expect("trust lock");
        replace_file_durably(&self.root, TOKEN_FILE, join_token_hash(token).as_bytes())?;
        remove_file_durably(&self.root, KEY_SET_FILE)?;
        remove_file_durably(&self.root, PORTAL_NODE_FILE)?;
        *state = None;
        Ok(())
    }

    /// Accept a first key set on the strength of the join token, and burn
    /// the token.
    pub fn join(
        &self,
        token: &str,
        key_set: SignedKeySet,
        portal_node: Option<String>,
    ) -> Result<(), TrustError> {
        let mut state = self.state.write().expect("trust lock");
        if state.is_some() {
            return Err(TrustError::AlreadyJoined);
        }
        let stored = match fs::read_to_string(self.root.join(TOKEN_FILE)) {
            Ok(stored) => stored,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(TrustError::NoJoinToken)
            }
            Err(error) => return Err(error.into()),
        };
        let stored = blake3::Hash::from_hex(stored.trim()).map_err(|_| TrustError::NoJoinToken)?;
        // `blake3::Hash` compares in constant time.
        if stored != blake3::hash(token.trim().as_bytes()) {
            return Err(TrustError::BadToken);
        }
        if !key_set.self_signed() {
            return Err(TrustError::Untrusted);
        }
        match &portal_node {
            Some(node) => replace_file_durably(&self.root, PORTAL_NODE_FILE, node.as_bytes())?,
            None => remove_file_durably(&self.root, PORTAL_NODE_FILE)?,
        }
        replace_file_durably(&self.root, KEY_SET_FILE, &key_set.to_bytes())?;
        remove_file_durably(&self.root, TOKEN_FILE)?;
        *state = Some(AgentTrust {
            key_set,
            portal_node,
        });
        Ok(())
    }

    /// Accept a newer key set signed by a key this agent already trusts.
    pub fn update(&self, offered: SignedKeySet) -> Result<(), TrustError> {
        let mut state = self.state.write().expect("trust lock");
        let Some(current) = state.as_mut() else {
            return Err(TrustError::NotJoined);
        };
        if !offered.signed_by(&current.key_set.key_set) {
            return Err(TrustError::Untrusted);
        }
        let (offered_set, current_set) = (&offered.key_set, &current.key_set.key_set);
        if offered_set.version <= current_set.version {
            return Err(TrustError::NotNewer {
                offered: offered_set.version,
                current: current_set.version,
            });
        }
        if offered_set.epoch < current_set.epoch {
            return Err(TrustError::EpochRegression {
                offered: offered_set.epoch,
                current: current_set.epoch,
            });
        }
        replace_file_durably(&self.root, KEY_SET_FILE, &offered.to_bytes())?;
        current.key_set = offered;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::{KeySet, KeyStatus, PortalSigner, TrustedKey};

    fn set(signers: &[(&PortalSigner, KeyStatus)], version: u64, epoch: u64) -> KeySet {
        KeySet {
            version,
            epoch,
            keys: signers
                .iter()
                .map(|(signer, status)| TrustedKey {
                    key_id: signer.key_id(),
                    public: signer.verifying_key(),
                    status: *status,
                })
                .collect(),
        }
    }

    #[test]
    fn join_needs_the_armed_token_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let signer = PortalSigner::generate();
        let first = set(&[(&signer, KeyStatus::Active)], 1, 1).sign(&[&signer]);
        let store = TrustStore::open(dir.path()).unwrap();
        assert!(matches!(
            store.join("t", first.clone(), None),
            Err(TrustError::NoJoinToken)
        ));
        let token = generate_join_token();
        store.arm_join_token(&token).unwrap();
        assert!(matches!(
            store.join("wrong", first.clone(), None),
            Err(TrustError::BadToken)
        ));
        let stranger = PortalSigner::generate();
        let unsigned = set(&[(&signer, KeyStatus::Active)], 1, 1).sign(&[&stranger]);
        assert!(matches!(
            store.join(&token, unsigned, None),
            Err(TrustError::Untrusted)
        ));
        store
            .join(&token, first.clone(), Some("nPORTAL".into()))
            .unwrap();
        assert!(!dir.path().join(TOKEN_FILE).exists(), "token burned");
        assert!(matches!(
            store.join(&token, first.clone(), None),
            Err(TrustError::AlreadyJoined)
        ));
        // Arming after join is ignored.
        store.arm_join_token(&generate_join_token()).unwrap();
        assert!(!dir.path().join(TOKEN_FILE).exists());

        let reopened = TrustStore::open(dir.path()).unwrap();
        let trust = reopened.current().unwrap();
        assert_eq!(trust.key_set, first);
        assert_eq!(trust.portal_node.as_deref(), Some("nPORTAL"));
    }

    #[test]
    fn a_reset_forgets_the_portal_and_accepts_a_new_join() {
        let dir = tempfile::tempdir().unwrap();
        let old = PortalSigner::generate();
        let store = TrustStore::open(dir.path()).unwrap();
        let first = generate_join_token();
        store.arm_join_token(&first).unwrap();
        store
            .join(
                &first,
                set(&[(&old, KeyStatus::Active)], 1, 1).sign(&[&old]),
                Some("nOLD".into()),
            )
            .unwrap();

        let rebuilt = PortalSigner::generate();
        let second = generate_join_token();
        store.reset_and_arm(&second).unwrap();
        assert!(!store.is_joined());
        let reopened = TrustStore::open(dir.path()).unwrap();
        assert!(!reopened.is_joined(), "the reset is durable");
        reopened
            .join(
                &second,
                set(&[(&rebuilt, KeyStatus::Active)], 1, 1).sign(&[&rebuilt]),
                None,
            )
            .unwrap();
        let trust = reopened.current().unwrap();
        assert_eq!(trust.key_set.key_set.keys[0].key_id, rebuilt.key_id());
        assert_eq!(trust.portal_node, None);
    }

    #[test]
    fn a_crash_after_writing_trust_burns_the_leftover_token_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let signer = PortalSigner::generate();
        let first = set(&[(&signer, KeyStatus::Active)], 1, 1).sign(&[&signer]);
        fs::write(dir.path().join(KEY_SET_FILE), first.to_bytes()).unwrap();
        fs::write(dir.path().join(TOKEN_FILE), join_token_hash("t")).unwrap();
        let store = TrustStore::open(dir.path()).unwrap();
        assert!(store.is_joined());
        assert!(!dir.path().join(TOKEN_FILE).exists());
    }

    #[test]
    fn updates_must_be_signed_by_a_trusted_key_newer_and_never_lower_the_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let old = PortalSigner::generate();
        let token = generate_join_token();
        let store = TrustStore::open(dir.path()).unwrap();
        store.arm_join_token(&token).unwrap();
        store
            .join(
                &token,
                set(&[(&old, KeyStatus::Active)], 1, 2).sign(&[&old]),
                None,
            )
            .unwrap();

        let new = PortalSigner::generate();
        let rotated = set(
            &[(&new, KeyStatus::Active), (&old, KeyStatus::Retiring)],
            2,
            2,
        );
        assert!(matches!(
            store.update(rotated.sign(&[&new])),
            Err(TrustError::Untrusted)
        ));
        store.update(rotated.sign(&[&new, &old])).unwrap();
        assert!(matches!(
            store.update(rotated.sign(&[&new, &old])),
            Err(TrustError::NotNewer { .. })
        ));
        let regressed = set(&[(&new, KeyStatus::Active)], 3, 1).sign(&[&new]);
        assert!(matches!(
            store.update(regressed),
            Err(TrustError::EpochRegression { .. })
        ));
        // After the old key is dropped, only the new key can vouch.
        let only_new = set(&[(&new, KeyStatus::Active)], 3, 3);
        store.update(only_new.sign(&[&new])).unwrap();
        let later = set(&[(&new, KeyStatus::Active)], 4, 3);
        assert!(matches!(
            store.update(later.sign(&[&old])),
            Err(TrustError::Untrusted)
        ));
        assert_eq!(
            TrustStore::open(dir.path())
                .unwrap()
                .current()
                .unwrap()
                .key_set
                .key_set
                .version,
            3
        );
    }
}

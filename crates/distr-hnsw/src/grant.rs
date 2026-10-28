//! Capability grants and key sets (DESIGN §3; docs/m2-implementation-plan.md,
//! decisions 3 to 6).
//!
//! The portal signs a short-lived grant for every internal request. A grant
//! names the agent, the operation, the object (or inventory kind), the GC
//! proof behind a delete, the grant epoch, and an expiry. Agents verify it
//! with the key set they received at join: they hold verification keys
//! only, so one agent cannot forge grants for another. Grants carry no
//! replay cache: each is bound to one idempotent operation on one object,
//! and it expires within minutes.
//!
//! Encodings are canonical little-endian binary with a domain-separation
//! prefix, so the signed bytes never depend on a serializer.

use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use uuid::Uuid;

use crate::object::{ObjectHash, ObjectKind};

pub const GRANT_VERSION: u8 = 1;
const GRANT_DOMAIN: &[u8] = b"distr-hnsw grant v1\0";
const KEY_SET_DOMAIN: &[u8] = b"distr-hnsw key set v1\0";
/// Longest lifetime an agent accepts.
pub const MAX_GRANT_LIFETIME_SECONDS: i64 = 300;
/// Lifetime the portal issues.
pub const GRANT_LIFETIME_SECONDS: i64 = 120;
/// Tolerated clock difference between portal and agent.
pub const CLOCK_SKEW_SECONDS: i64 = 60;
/// `Authorization` scheme carrying a grant.
pub const GRANT_SCHEME: &str = "DistrGrant";
const MAX_AGENT_ID_BYTES: usize = 255;
const MAX_KEYS: usize = 16;

/// First eight bytes of BLAKE3 over a verification key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyId(pub [u8; 8]);

impl KeyId {
    pub fn of(key: &VerifyingKey) -> Self {
        let digest = blake3::hash(key.as_bytes());
        Self(digest.as_bytes()[..8].try_into().expect("8 bytes"))
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Health,
    Inventory,
    Get,
    Put,
    Delete,
}

impl Operation {
    const fn code(self) -> u8 {
        match self {
            Self::Health => 1,
            Self::Inventory => 2,
            Self::Get => 3,
            Self::Put => 4,
            Self::Delete => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Health,
            2 => Self::Inventory,
            3 => Self::Get,
            4 => Self::Put,
            5 => Self::Delete,
            _ => return None,
        })
    }
}

/// What a grant covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Health: no object.
    Agent,
    /// Inventory of one object kind.
    Kind(ObjectKind),
    /// One object.
    Object(ObjectKind, ObjectHash),
}

fn kind_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Chunk => 1,
        ObjectKind::Manifest => 2,
        ObjectKind::DeletionMarker => 3,
    }
}

fn kind_from(code: u8) -> Option<ObjectKind> {
    Some(match code {
        1 => ObjectKind::Chunk,
        2 => ObjectKind::Manifest,
        3 => ObjectKind::DeletionMarker,
        _ => return None,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub key_id: KeyId,
    pub epoch: u64,
    pub agent_id: String,
    pub operation: Operation,
    pub scope: Scope,
    /// The applied GC proof a delete executes; required for `Delete` only.
    pub gc_proof: Option<Uuid>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub nonce: [u8; 16],
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum GrantError {
    #[error("no grant")]
    Missing,
    #[error("malformed grant: {0}")]
    Malformed(&'static str),
    #[error("grant signed by unknown key {0}")]
    UnknownKey(KeyId),
    #[error("grant signature does not verify")]
    BadSignature,
    #[error("grant epoch {grant} is older than the current epoch {current}")]
    StaleEpoch { grant: u64, current: u64 },
    #[error("grant epoch {grant} is newer than this agent's key set ({current})")]
    NewerEpoch { grant: u64, current: u64 },
    #[error("grant is for agent {0:?}")]
    WrongAgent(String),
    #[error("grant is for another operation")]
    WrongOperation,
    #[error("grant is for another object")]
    WrongObject,
    #[error("delete grant names no GC proof")]
    DeleteWithoutProof,
    #[error("grant expired")]
    Expired,
    #[error("grant is not valid yet")]
    NotYetValid,
    #[error("grant lifetime exceeds {MAX_GRANT_LIFETIME_SECONDS} s")]
    LifetimeTooLong,
}

impl Grant {
    fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.push(GRANT_VERSION);
        out.extend_from_slice(&self.key_id.0);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.push(self.agent_id.len() as u8);
        out.extend_from_slice(self.agent_id.as_bytes());
        out.push(self.operation.code());
        match &self.scope {
            Scope::Agent => out.push(0),
            Scope::Kind(kind) => {
                out.push(1);
                out.push(kind_code(*kind));
            }
            Scope::Object(kind, hash) => {
                out.push(2);
                out.push(kind_code(*kind));
                out.extend_from_slice(
                    &hex::decode(hash.as_str()).expect("object hashes are 32 hex bytes"),
                );
            }
        }
        match self.gc_proof {
            Some(proof) => {
                out.push(1);
                out.extend_from_slice(proof.as_bytes());
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.issued_at.to_le_bytes());
        out.extend_from_slice(&self.expires_at.to_le_bytes());
        out.extend_from_slice(&self.nonce);
        out
    }

    fn decode_body(bytes: &[u8]) -> Result<Self, GrantError> {
        let mut r = Reader(bytes);
        if r.u8()? != GRANT_VERSION {
            return Err(GrantError::Malformed("version"));
        }
        let key_id = KeyId(r.array()?);
        let epoch = u64::from_le_bytes(r.array()?);
        let len = r.u8()? as usize;
        let agent_id = std::str::from_utf8(r.take(len)?)
            .map_err(|_| GrantError::Malformed("agent id"))?
            .to_owned();
        let operation = Operation::from_code(r.u8()?).ok_or(GrantError::Malformed("operation"))?;
        let scope = match r.u8()? {
            0 => Scope::Agent,
            1 => Scope::Kind(kind_from(r.u8()?).ok_or(GrantError::Malformed("kind"))?),
            2 => {
                let kind = kind_from(r.u8()?).ok_or(GrantError::Malformed("kind"))?;
                let hash = ObjectHash::parse(hex::encode(r.take(32)?))
                    .map_err(|_| GrantError::Malformed("hash"))?;
                Scope::Object(kind, hash)
            }
            _ => return Err(GrantError::Malformed("scope")),
        };
        let gc_proof = match r.u8()? {
            0 => None,
            1 => Some(Uuid::from_bytes(r.array()?)),
            _ => return Err(GrantError::Malformed("gc proof")),
        };
        let issued_at = i64::from_le_bytes(r.array()?);
        let expires_at = i64::from_le_bytes(r.array()?);
        let nonce = r.array()?;
        if !r.0.is_empty() {
            return Err(GrantError::Malformed("trailing bytes"));
        }
        Ok(Self {
            key_id,
            epoch,
            agent_id,
            operation,
            scope,
            gc_proof,
            issued_at,
            expires_at,
            nonce,
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], GrantError> {
        if self.0.len() < n {
            return Err(GrantError::Malformed("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, GrantError> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], GrantError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
}

fn signed_message(domain: &[u8], body: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.len() + body.len());
    message.extend_from_slice(domain);
    message.extend_from_slice(body);
    message
}

/// The portal's grant-signing key.
pub struct PortalSigner {
    key: SigningKey,
    key_id: KeyId,
}

impl PortalSigner {
    pub fn generate() -> Self {
        Self::from_seed(&{
            let mut seed = [0_u8; 32];
            OsRng.fill_bytes(&mut seed);
            seed
        })
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let key_id = KeyId::of(&key.verifying_key());
        Self { key, key_id }
    }

    pub fn seed(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.key.to_bytes())
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Issue a grant valid for `GRANT_LIFETIME_SECONDS` from `now`, encoded
    /// for an `Authorization: DistrGrant <token>` header.
    pub fn issue(
        &self,
        epoch: u64,
        agent_id: &str,
        operation: Operation,
        scope: Scope,
        gc_proof: Option<Uuid>,
        now: i64,
    ) -> Result<String, GrantError> {
        if agent_id.len() > MAX_AGENT_ID_BYTES {
            return Err(GrantError::Malformed("agent id"));
        }
        let mut nonce = [0_u8; 16];
        OsRng.fill_bytes(&mut nonce);
        let grant = Grant {
            key_id: self.key_id,
            epoch,
            agent_id: agent_id.to_owned(),
            operation,
            scope,
            gc_proof,
            issued_at: now,
            expires_at: now + GRANT_LIFETIME_SECONDS,
            nonce,
        };
        Ok(self.sign_grant(&grant))
    }

    /// Sign an arbitrary grant (tests forge expired or mismatched ones).
    pub fn sign_grant(&self, grant: &Grant) -> String {
        let body = grant.encode_body();
        let signature = self.key.sign(&signed_message(GRANT_DOMAIN, &body));
        let mut token = body;
        token.extend_from_slice(&signature.to_bytes());
        URL_SAFE_NO_PAD.encode(token)
    }

    fn sign_raw(&self, domain: &[u8], body: &[u8]) -> Signature {
        self.key.sign(&signed_message(domain, body))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyStatus {
    /// Signs new grants.
    Active,
    /// Still accepted while agents move to the active key.
    Retiring,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedKey {
    pub key_id: KeyId,
    pub public: VerifyingKey,
    pub status: KeyStatus,
}

/// The verification keys and grant epoch an agent trusts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeySet {
    pub version: u64,
    pub epoch: u64,
    pub keys: Vec<TrustedKey>,
}

/// A key set with signatures from one or more portal keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedKeySet {
    pub key_set: KeySet,
    body: Vec<u8>,
    signatures: Vec<(KeyId, Signature)>,
}

/// What an agent expects a request's grant to say.
pub struct Expectation<'a> {
    pub agent_id: &'a str,
    pub operation: Operation,
    pub scope: &'a Scope,
}

impl KeySet {
    fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.push(self.keys.len() as u8);
        for key in &self.keys {
            out.extend_from_slice(&key.key_id.0);
            out.extend_from_slice(key.public.as_bytes());
            out.push(match key.status {
                KeyStatus::Active => 1,
                KeyStatus::Retiring => 2,
            });
        }
        out
    }

    fn decode_body(bytes: &[u8]) -> Result<Self, GrantError> {
        let mut r = Reader(bytes);
        let version = u64::from_le_bytes(r.array()?);
        let epoch = u64::from_le_bytes(r.array()?);
        let count = r.u8()? as usize;
        if count == 0 || count > MAX_KEYS {
            return Err(GrantError::Malformed("key count"));
        }
        let mut keys = Vec::with_capacity(count);
        for _ in 0..count {
            let key_id = KeyId(r.array()?);
            let public = VerifyingKey::from_bytes(&r.array()?)
                .map_err(|_| GrantError::Malformed("verification key"))?;
            if KeyId::of(&public) != key_id {
                return Err(GrantError::Malformed("key id"));
            }
            let status = match r.u8()? {
                1 => KeyStatus::Active,
                2 => KeyStatus::Retiring,
                _ => return Err(GrantError::Malformed("key status")),
            };
            keys.push(TrustedKey {
                key_id,
                public,
                status,
            });
        }
        if !r.0.is_empty() {
            return Err(GrantError::Malformed("trailing bytes"));
        }
        if keys
            .iter()
            .filter(|k| k.status == KeyStatus::Active)
            .count()
            != 1
        {
            return Err(GrantError::Malformed("exactly one active key"));
        }
        Ok(Self {
            version,
            epoch,
            keys,
        })
    }

    fn key(&self, key_id: KeyId) -> Option<&TrustedKey> {
        self.keys.iter().find(|key| key.key_id == key_id)
    }

    /// Sign this set with every given signer (during rotation, the old and
    /// new keys both sign, so agents that know either accept it).
    pub fn sign(&self, signers: &[&PortalSigner]) -> SignedKeySet {
        let body = self.encode_body();
        let signatures = signers
            .iter()
            .map(|signer| (signer.key_id, signer.sign_raw(KEY_SET_DOMAIN, &body)))
            .collect();
        SignedKeySet {
            key_set: self.clone(),
            body,
            signatures,
        }
    }

    /// Verify a grant token for a request.
    pub fn verify(
        &self,
        token: &str,
        expect: &Expectation<'_>,
        now: i64,
    ) -> Result<Grant, GrantError> {
        let raw = URL_SAFE_NO_PAD
            .decode(token.trim())
            .map_err(|_| GrantError::Malformed("base64"))?;
        if raw.len() < 64 {
            return Err(GrantError::Malformed("truncated"));
        }
        let (body, signature) = raw.split_at(raw.len() - 64);
        let grant = Grant::decode_body(body)?;
        let key = self
            .key(grant.key_id)
            .ok_or(GrantError::UnknownKey(grant.key_id))?;
        let signature = Signature::from_bytes(signature.try_into().expect("64 bytes"));
        key.public
            .verify(&signed_message(GRANT_DOMAIN, body), &signature)
            .map_err(|_| GrantError::BadSignature)?;
        if grant.epoch < self.epoch {
            return Err(GrantError::StaleEpoch {
                grant: grant.epoch,
                current: self.epoch,
            });
        }
        if grant.epoch > self.epoch {
            return Err(GrantError::NewerEpoch {
                grant: grant.epoch,
                current: self.epoch,
            });
        }
        if grant.agent_id != expect.agent_id {
            return Err(GrantError::WrongAgent(grant.agent_id));
        }
        if grant.operation != expect.operation {
            return Err(GrantError::WrongOperation);
        }
        if &grant.scope != expect.scope {
            return Err(GrantError::WrongObject);
        }
        if grant.operation == Operation::Delete && grant.gc_proof.is_none() {
            return Err(GrantError::DeleteWithoutProof);
        }
        if grant.expires_at - grant.issued_at > MAX_GRANT_LIFETIME_SECONDS
            || grant.expires_at < grant.issued_at
        {
            return Err(GrantError::LifetimeTooLong);
        }
        if now > grant.expires_at + CLOCK_SKEW_SECONDS {
            return Err(GrantError::Expired);
        }
        if grant.issued_at > now + CLOCK_SKEW_SECONDS {
            return Err(GrantError::NotYetValid);
        }
        Ok(grant)
    }
}

impl SignedKeySet {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.body.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.body);
        out.push(self.signatures.len() as u8);
        for (key_id, signature) in &self.signatures {
            out.extend_from_slice(&key_id.0);
            out.extend_from_slice(&signature.to_bytes());
        }
        out
    }

    /// Parse without trusting: check that the set is well formed and that
    /// each of its signatures comes from a key the set itself lists.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GrantError> {
        let mut r = Reader(bytes);
        let len = u32::from_le_bytes(r.array()?) as usize;
        let body = r.take(len)?.to_vec();
        let key_set = KeySet::decode_body(&body)?;
        let count = r.u8()? as usize;
        if count == 0 || count > MAX_KEYS {
            return Err(GrantError::Malformed("signature count"));
        }
        let mut signatures = Vec::with_capacity(count);
        for _ in 0..count {
            let key_id = KeyId(r.array()?);
            let signature = Signature::from_bytes(&r.array()?);
            signatures.push((key_id, signature));
        }
        if !r.0.is_empty() {
            return Err(GrantError::Malformed("trailing bytes"));
        }
        Ok(Self {
            key_set,
            body,
            signatures,
        })
    }

    /// True when at least one signature verifies under a key in `trusted`.
    pub fn signed_by(&self, trusted: &KeySet) -> bool {
        let message = signed_message(KEY_SET_DOMAIN, &self.body);
        self.signatures.iter().any(|(key_id, signature)| {
            trusted
                .key(*key_id)
                .is_some_and(|key| key.public.verify(&message, signature).is_ok())
        })
    }

    /// True when at least one signature verifies under a key the set itself
    /// lists (a first key set, accepted on the strength of a join token).
    pub fn self_signed(&self) -> bool {
        self.signed_by(&self.key_set)
    }
}

/// Parse `Authorization: DistrGrant <token>`.
pub fn token_from_header(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme == GRANT_SCHEME && !token.trim().is_empty()).then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn hash() -> ObjectHash {
        ObjectHash::digest(b"chunk")
    }

    fn key_set(signer: &PortalSigner, epoch: u64) -> KeySet {
        KeySet {
            version: 1,
            epoch,
            keys: vec![TrustedKey {
                key_id: signer.key_id(),
                public: signer.verifying_key(),
                status: KeyStatus::Active,
            }],
        }
    }

    fn expect<'a>(operation: Operation, scope: &'a Scope) -> Expectation<'a> {
        Expectation {
            agent_id: "a1",
            operation,
            scope,
        }
    }

    #[test]
    fn a_valid_grant_round_trips_for_every_operation() {
        let signer = PortalSigner::generate();
        let set = key_set(&signer, 3);
        let object = Scope::Object(ObjectKind::Chunk, hash());
        let cases = [
            (Operation::Health, Scope::Agent, None),
            (
                Operation::Inventory,
                Scope::Kind(ObjectKind::Manifest),
                None,
            ),
            (Operation::Get, object.clone(), None),
            (Operation::Put, object.clone(), None),
            (Operation::Delete, object, Some(Uuid::new_v4())),
        ];
        for (operation, scope, proof) in cases {
            let token = signer
                .issue(3, "a1", operation, scope.clone(), proof, NOW)
                .unwrap();
            let grant = set.verify(&token, &expect(operation, &scope), NOW).unwrap();
            assert_eq!(grant.gc_proof, proof);
            assert_eq!(grant.expires_at - grant.issued_at, GRANT_LIFETIME_SECONDS);
        }
    }

    #[test]
    fn every_mismatch_and_tamper_is_refused() {
        let signer = PortalSigner::generate();
        let set = key_set(&signer, 3);
        let object = Scope::Object(ObjectKind::Chunk, hash());
        let other = Scope::Object(ObjectKind::Chunk, ObjectHash::digest(b"other"));
        let token = signer
            .issue(3, "a1", Operation::Get, object.clone(), None, NOW)
            .unwrap();

        let wrong_agent = Expectation {
            agent_id: "a2",
            operation: Operation::Get,
            scope: &object,
        };
        assert_eq!(
            set.verify(&token, &wrong_agent, NOW),
            Err(GrantError::WrongAgent("a1".into()))
        );
        assert_eq!(
            set.verify(&token, &expect(Operation::Put, &object), NOW),
            Err(GrantError::WrongOperation)
        );
        assert_eq!(
            set.verify(&token, &expect(Operation::Get, &other), NOW),
            Err(GrantError::WrongObject)
        );
        assert_eq!(
            set.verify(&token, &expect(Operation::Get, &object), NOW + 300),
            Err(GrantError::Expired)
        );
        assert_eq!(
            set.verify(&token, &expect(Operation::Get, &object), NOW - 120),
            Err(GrantError::NotYetValid)
        );

        // Flip one bit anywhere in the token's bytes.
        let raw = URL_SAFE_NO_PAD.decode(&token).unwrap();
        for byte in 0..raw.len() {
            let mut altered = raw.clone();
            altered[byte] ^= 0x01;
            let altered = URL_SAFE_NO_PAD.encode(altered);
            assert!(
                set.verify(&altered, &expect(Operation::Get, &object), NOW)
                    .is_err(),
                "flipped byte {byte} was accepted"
            );
        }

        // Another key, a stale or newer epoch, a delete without a proof,
        // and an over-long lifetime.
        let stranger = PortalSigner::generate();
        let forged = stranger
            .issue(3, "a1", Operation::Get, object.clone(), None, NOW)
            .unwrap();
        assert_eq!(
            set.verify(&forged, &expect(Operation::Get, &object), NOW),
            Err(GrantError::UnknownKey(stranger.key_id()))
        );
        let stale = signer
            .issue(2, "a1", Operation::Get, object.clone(), None, NOW)
            .unwrap();
        assert_eq!(
            set.verify(&stale, &expect(Operation::Get, &object), NOW),
            Err(GrantError::StaleEpoch {
                grant: 2,
                current: 3
            })
        );
        let newer = signer
            .issue(4, "a1", Operation::Get, object.clone(), None, NOW)
            .unwrap();
        assert_eq!(
            set.verify(&newer, &expect(Operation::Get, &object), NOW),
            Err(GrantError::NewerEpoch {
                grant: 4,
                current: 3
            })
        );
        let unproven = signer
            .issue(3, "a1", Operation::Delete, object.clone(), None, NOW)
            .unwrap();
        assert_eq!(
            set.verify(&unproven, &expect(Operation::Delete, &object), NOW),
            Err(GrantError::DeleteWithoutProof)
        );
        let mut long =
            Grant::decode_body(&URL_SAFE_NO_PAD.decode(&token).unwrap()[..raw.len() - 64]).unwrap();
        long.expires_at = long.issued_at + MAX_GRANT_LIFETIME_SECONDS + 1;
        assert_eq!(
            set.verify(
                &signer.sign_grant(&long),
                &expect(Operation::Get, &object),
                NOW
            ),
            Err(GrantError::LifetimeTooLong)
        );
        assert_eq!(
            set.verify("", &expect(Operation::Get, &object), NOW),
            Err(GrantError::Malformed("truncated"))
        );
    }

    #[test]
    fn key_sets_round_trip_and_rotation_is_signed_by_the_old_key() {
        let old = PortalSigner::generate();
        let first = key_set(&old, 1).sign(&[&old]);
        let parsed = SignedKeySet::from_bytes(&first.to_bytes()).unwrap();
        assert_eq!(parsed, first);
        assert!(parsed.self_signed());

        let new = PortalSigner::generate();
        let rotated = KeySet {
            version: 2,
            epoch: 1,
            keys: vec![
                TrustedKey {
                    key_id: new.key_id(),
                    public: new.verifying_key(),
                    status: KeyStatus::Active,
                },
                TrustedKey {
                    key_id: old.key_id(),
                    public: old.verifying_key(),
                    status: KeyStatus::Retiring,
                },
            ],
        };
        let signed = rotated.sign(&[&new, &old]);
        assert!(
            signed.signed_by(&first.key_set),
            "old key vouches for the new set"
        );
        let only_new = rotated.sign(&[&new]);
        assert!(!only_new.signed_by(&first.key_set));

        // A tampered body no longer verifies.
        let mut bytes = signed.to_bytes();
        bytes[4] ^= 0x01; // first byte of the version
        let tampered = SignedKeySet::from_bytes(&bytes).unwrap();
        assert!(!tampered.signed_by(&first.key_set));
    }

    #[test]
    fn the_header_scheme_is_strict() {
        assert_eq!(token_from_header("DistrGrant abc"), Some("abc"));
        assert_eq!(token_from_header("Bearer abc"), None);
        assert_eq!(token_from_header("DistrGrant "), None);
        assert_eq!(token_from_header("DistrGrant"), None);
    }
}

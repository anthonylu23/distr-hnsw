//! Portal credentials: browser-session tokens with their CSRF tokens, and
//! application API keys with collection scopes (DESIGN §9;
//! docs/m2-implementation-plan.md, decision 8).
//!
//! Every secret is 256 random bits. The database keeps only a BLAKE3
//! verifier derived under a purpose-specific context, so a database copy
//! yields no usable credential, and verifiers are compared in constant time.

use std::{fmt, str::FromStr};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use zeroize::Zeroizing;

/// The session cookie. The `__Host-` prefix makes browsers refuse it unless
/// it is `Secure`, has `Path=/`, and names no domain.
pub const SESSION_COOKIE: &str = "__Host-distr-session";
/// Absolute session lifetime; a new login is a fresh session.
pub const SESSION_LIFETIME_SECONDS: i64 = 12 * 3600;
/// A session unused for this long ends.
pub const SESSION_IDLE_SECONDS: i64 = 3600;
/// Header that carries the session's CSRF token on state-changing requests.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// API keys are presented as `Authorization: Bearer dhk_<id>_<secret>`.
pub const API_KEY_PREFIX: &str = "dhk_";
/// The scope that covers the files product (`/api/files`).
pub const FILES_COLLECTION: &str = "files";

const SESSION_VERIFIER_CONTEXT: &str = "distr-hnsw 2026-10 session verifier v1";
const SESSION_CSRF_CONTEXT: &str = "distr-hnsw 2026-10 session csrf v1";
const API_KEY_VERIFIER_CONTEXT: &str = "distr-hnsw 2026-10 api key verifier v1";

fn random_32() -> Zeroizing<[u8; 32]> {
    let mut bytes = Zeroizing::new([0_u8; 32]);
    OsRng.fill_bytes(bytes.as_mut());
    bytes
}

fn decode_32(text: &str) -> Option<Zeroizing<[u8; 32]>> {
    // 32 bytes are exactly 43 unpadded base64url characters.
    if text.len() != 43 {
        return None;
    }
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(text).ok()?);
    let mut bytes = Zeroizing::new([0_u8; 32]);
    bytes.copy_from_slice(decoded.get(..32)?);
    (decoded.len() == 32).then_some(bytes)
}

/// Constant-time comparison of two 32-byte values.
pub fn verifiers_match(left: &[u8; 32], right: &[u8; 32]) -> bool {
    blake3::Hash::from_bytes(*left) == blake3::Hash::from_bytes(*right)
}

/// A browser-session token, held only by the browser (in the cookie).
pub struct SessionToken(Zeroizing<[u8; 32]>);

impl SessionToken {
    pub fn generate() -> Self {
        Self(random_32())
    }

    pub fn parse(text: &str) -> Option<Self> {
        decode_32(text).map(Self)
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.as_slice())
    }

    /// What the database stores and looks the session up by.
    pub fn verifier(&self) -> [u8; 32] {
        blake3::derive_key(SESSION_VERIFIER_CONTEXT, self.0.as_slice())
    }

    /// The CSRF token bound to this session. Only a holder of the session
    /// token can compute it, and a cross-site page cannot read it.
    pub fn csrf_token(&self) -> String {
        URL_SAFE_NO_PAD.encode(blake3::derive_key(SESSION_CSRF_CONTEXT, self.0.as_slice()))
    }

    pub fn csrf_matches(&self, presented: &str) -> bool {
        let expected = blake3::derive_key(SESSION_CSRF_CONTEXT, self.0.as_slice());
        decode_32(presented).is_some_and(|presented| verifiers_match(&presented, &expected))
    }
}

/// A newly minted or presented API key: a public id and a secret.
pub struct ApiKeySecret {
    key_id: String,
    secret: Zeroizing<[u8; 32]>,
}

impl ApiKeySecret {
    pub fn generate() -> Self {
        let mut id = [0_u8; 8];
        OsRng.fill_bytes(&mut id);
        Self {
            key_id: hex::encode(id),
            secret: random_32(),
        }
    }

    /// Parse `dhk_<16 hex>_<43 base64url>`.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(API_KEY_PREFIX)?;
        let (key_id, secret) = rest.split_once('_')?;
        let valid_id = key_id.len() == 16
            && key_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid_id {
            return None;
        }
        Some(Self {
            key_id: key_id.to_owned(),
            secret: decode_32(secret)?,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The full key, shown once at creation.
    pub fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(format!(
            "{API_KEY_PREFIX}{}_{}",
            self.key_id,
            URL_SAFE_NO_PAD.encode(self.secret.as_slice())
        ))
    }

    pub fn verifier(&self) -> [u8; 32] {
        blake3::derive_key(API_KEY_VERIFIER_CONTEXT, self.secret.as_slice())
    }

    pub fn matches(&self, stored: &[u8; 32]) -> bool {
        verifiers_match(&self.verifier(), stored)
    }
}

/// What a scope allows on its collection; each right includes the ones
/// below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Right {
    Read,
    Write,
    Admin,
}

impl Right {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

/// One `collection:right` grant on an API key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionScope {
    pub collection: String,
    pub right: Right,
}

impl fmt::Display for CollectionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.collection, self.right.as_str())
    }
}

/// Collection names: lowercase ASCII letters, digits, `-` and `_`, starting
/// with a letter or digit, at most 63 bytes.
pub fn valid_collection_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

impl FromStr for CollectionScope {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (collection, right) = text
            .split_once(':')
            .ok_or_else(|| format!("scope {text:?} is not collection:right"))?;
        if !valid_collection_name(collection) {
            return Err(format!("invalid collection name {collection:?}"));
        }
        let right = match right {
            "read" => Right::Read,
            "write" => Right::Write,
            "admin" => Right::Admin,
            _ => {
                return Err(format!(
                    "right must be read, write, or admin, got {right:?}"
                ))
            }
        };
        Ok(Self {
            collection: collection.to_owned(),
            right,
        })
    }
}

/// A key's scopes: at least one, at most one per collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scopes(Vec<CollectionScope>);

impl Scopes {
    pub fn new(mut scopes: Vec<CollectionScope>) -> Result<Self, String> {
        if scopes.is_empty() {
            return Err("an API key needs at least one scope".into());
        }
        scopes.sort_by(|a, b| a.collection.cmp(&b.collection));
        if scopes
            .windows(2)
            .any(|pair| pair[0].collection == pair[1].collection)
        {
            return Err("one scope per collection".into());
        }
        Ok(Self(scopes))
    }

    /// Parse the stored, space-separated form.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::new(
            text.split_whitespace()
                .map(str::parse)
                .collect::<Result<Vec<_>, _>>()?,
        )
    }

    pub fn allows(&self, collection: &str, right: Right) -> bool {
        self.0
            .iter()
            .any(|scope| scope.collection == collection && scope.right >= right)
    }

    pub fn as_strings(&self) -> Vec<String> {
        self.0.iter().map(ToString::to_string).collect()
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_strings().join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tokens_round_trip_and_bind_their_csrf_token() {
        let token = SessionToken::generate();
        let parsed = SessionToken::parse(&token.encode()).unwrap();
        assert_eq!(parsed.verifier(), token.verifier());
        assert!(token.csrf_matches(&token.csrf_token()));
        let other = SessionToken::generate();
        assert!(!token.csrf_matches(&other.csrf_token()));
        assert!(!token.csrf_matches(""));
        assert!(
            !token.csrf_matches(&token.encode()),
            "the token is not its CSRF token"
        );
        assert_ne!(
            token.verifier(),
            token.0.as_slice(),
            "verifier is not the secret"
        );
        assert!(SessionToken::parse("short").is_none());
        assert!(SessionToken::parse(&format!("{}=", token.encode())).is_none());
    }

    #[test]
    fn api_keys_parse_strictly_and_verify_in_constant_time() {
        let key = ApiKeySecret::generate();
        let text = key.encode();
        let parsed = ApiKeySecret::parse(&text).unwrap();
        assert_eq!(parsed.key_id(), key.key_id());
        assert!(parsed.matches(&key.verifier()));
        // A middle character: the last one also carries padding bits.
        let mut altered = text.as_bytes().to_vec();
        let middle = altered.len() - 10;
        altered[middle] = if altered[middle] == b'A' { b'B' } else { b'A' };
        let altered = String::from_utf8(altered).unwrap();
        let altered = ApiKeySecret::parse(&altered).unwrap();
        assert!(!altered.matches(&key.verifier()));
        for bad in [
            "",
            "dhk_",
            "dhk_0011_abc",
            &text.replacen("dhk_", "dhx_", 1),
            &text.to_uppercase(),
        ] {
            assert!(ApiKeySecret::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn scopes_enforce_collection_and_right() {
        let scopes = Scopes::parse("files:write notes:read").unwrap();
        assert!(scopes.allows("files", Right::Read));
        assert!(scopes.allows("files", Right::Write));
        assert!(!scopes.allows("files", Right::Admin));
        assert!(scopes.allows("notes", Right::Read));
        assert!(!scopes.allows("notes", Right::Write));
        assert!(!scopes.allows("other", Right::Read));
        assert_eq!(scopes.to_string(), "files:write notes:read");
        assert_eq!(Scopes::parse(&scopes.to_string()).unwrap(), scopes);
        for bad in [
            "",
            "files",
            "files:own",
            "Files:read",
            "-x:read",
            "files:read files:admin",
        ] {
            assert!(Scopes::parse(bad).is_err(), "{bad:?}");
        }
    }
}

//! Recovery bundle v1: the master key wrapped under a passphrase-derived key
//! so it can be recovered without any surviving cluster machine.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! magic "DHRB" | version u16 | key_id[16] | kdf u8 | m_cost_kib u32 | t_cost u32
//! | p_cost u32 | salt[16] | nonce[24] | ciphertext[48]
//! ```
//!
//! Everything before the nonce is authenticated as AAD. The bundle is emitted
//! as printable armor so it survives being written down. See DESIGN §10 and
//! `docs/m1-phase-1-decisions.md`, decision 1.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::crypto::{random_nonce, MasterKey, KEY_LEN, NONCE_LEN};

pub const BUNDLE_VERSION: u16 = 1;
const MAGIC: &[u8; 4] = b"DHRB";
const KDF_ARGON2ID: u8 = 1;
const SALT_LEN: usize = 16;
const KEY_ID_LEN: usize = 16;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 4 + 2 + KEY_ID_LEN + 1 + 4 + 4 + 4 + SALT_LEN;
const BUNDLE_LEN: usize = HEADER_LEN + NONCE_LEN + KEY_LEN + TAG_LEN;
const ARMOR_BEGIN: &str = "-----BEGIN DISTR-HNSW RECOVERY BUNDLE-----";
const ARMOR_END: &str = "-----END DISTR-HNSW RECOVERY BUNDLE-----";
const ARMOR_WIDTH: usize = 64;
pub const MIN_PASSPHRASE_LEN: usize = 12;

/// Argon2id cost parameters carried inside the bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl KdfParams {
    /// RFC 9106 §4 second recommended option (64 MiB, t = 3, p = 4).
    pub const DEFAULT: Self = Self {
        m_cost_kib: 64 * 1024,
        t_cost: 3,
        p_cost: 4,
    };
    /// OWASP minimum for Argon2id (19 MiB, t = 2, p = 1). Bundles below this
    /// floor are refused.
    pub const FLOOR: Self = Self {
        m_cost_kib: 19 * 1024,
        t_cost: 2,
        p_cost: 1,
    };

    /// Upper bounds so a corrupted or hostile header cannot demand unbounded
    /// memory or time before the AEAD check can reject it (4 GiB, t = 64,
    /// p = 64).
    pub const CEILING: Self = Self {
        m_cost_kib: 4 * 1024 * 1024,
        t_cost: 64,
        p_cost: 64,
    };

    fn validate(self) -> Result<(), BundleError> {
        if self.m_cost_kib < Self::FLOOR.m_cost_kib
            || self.t_cost < Self::FLOOR.t_cost
            || self.p_cost < Self::FLOOR.p_cost
        {
            return Err(BundleError::WeakKdfParameters(self));
        }
        if self.m_cost_kib > Self::CEILING.m_cost_kib
            || self.t_cost > Self::CEILING.t_cost
            || self.p_cost > Self::CEILING.p_cost
        {
            return Err(BundleError::ExcessiveKdfParameters(self));
        }
        Ok(())
    }
}

/// Non-secret header fields readable without the passphrase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleHeader {
    pub version: u16,
    pub key_id: [u8; KEY_ID_LEN],
    pub params: KdfParams,
}

impl BundleHeader {
    pub fn key_id_hex(&self) -> String {
        hex::encode(self.key_id)
    }
}

/// A successfully opened bundle.
pub struct OpenedBundle {
    pub header: BundleHeader,
    pub key: MasterKey,
}

/// Generate a passphrase: 24 Crockford base32 characters (120 bits) in six
/// groups, with no visually ambiguous glyphs.
pub fn generate_passphrase() -> Zeroizing<String> {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = Zeroizing::new([0_u8; 24]);
    OsRng.fill_bytes(bytes.as_mut());
    let mut out = String::with_capacity(29);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index % 4 == 0 {
            out.push('-');
        }
        out.push(ALPHABET[(byte % 32) as usize] as char);
    }
    Zeroizing::new(out)
}

/// Wrap the master key under `passphrase` and return the armored bundle.
pub fn seal(
    master: &MasterKey,
    passphrase: &[u8],
    params: KdfParams,
) -> Result<String, BundleError> {
    params.validate()?;
    if passphrase.len() < MIN_PASSPHRASE_LEN {
        return Err(BundleError::PassphraseTooShort(MIN_PASSPHRASE_LEN));
    }
    let mut salt = [0_u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let header = encode_header(master.key_id(), params, &salt);
    let kek = derive_kek(passphrase, &salt, params)?;
    let nonce = random_nonce();
    let cipher = XChaCha20Poly1305::new((&*kek).into());
    let ciphertext = cipher.encrypt(
        XNonce::from_slice(&nonce),
        Payload {
            msg: master.bytes(),
            aad: &header,
        },
    )?;
    let mut bytes = header;
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);
    debug_assert_eq!(bytes.len(), BUNDLE_LEN);
    Ok(armor(&bytes))
}

/// Read the header without a passphrase.
pub fn inspect(armored: &str) -> Result<BundleHeader, BundleError> {
    let bytes = dearmor(armored)?;
    Ok(parse_header(&bytes)?.0)
}

/// Unwrap the master key. Any tampering, wrong passphrase, unsupported
/// version, or sub-floor parameters fails without partial output.
pub fn open(armored: &str, passphrase: &[u8]) -> Result<OpenedBundle, BundleError> {
    let bytes = dearmor(armored)?;
    let (header, salt) = parse_header(&bytes)?;
    let nonce: [u8; NONCE_LEN] = bytes[HEADER_LEN..HEADER_LEN + NONCE_LEN]
        .try_into()
        .map_err(|_| BundleError::Malformed)?;
    let ciphertext = &bytes[HEADER_LEN + NONCE_LEN..];
    let kek = derive_kek(passphrase, &salt, header.params)?;
    let cipher = XChaCha20Poly1305::new((&*kek).into());
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &bytes[..HEADER_LEN],
                },
            )
            .map_err(|_| BundleError::WrongPassphraseOrTampered)?,
    );
    let key_bytes: [u8; KEY_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| BundleError::Malformed)?;
    let key = MasterKey::from_bytes(key_bytes);
    if key.key_id() != header.key_id {
        return Err(BundleError::KeyIdMismatch);
    }
    Ok(OpenedBundle { header, key })
}

fn encode_header(key_id: [u8; KEY_ID_LEN], params: KdfParams, salt: &[u8; SALT_LEN]) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&BUNDLE_VERSION.to_le_bytes());
    header.extend_from_slice(&key_id);
    header.push(KDF_ARGON2ID);
    header.extend_from_slice(&params.m_cost_kib.to_le_bytes());
    header.extend_from_slice(&params.t_cost.to_le_bytes());
    header.extend_from_slice(&params.p_cost.to_le_bytes());
    header.extend_from_slice(salt);
    header
}

fn parse_header(bytes: &[u8]) -> Result<(BundleHeader, [u8; SALT_LEN]), BundleError> {
    if bytes.len() != BUNDLE_LEN {
        return Err(BundleError::Malformed);
    }
    if &bytes[0..4] != MAGIC {
        return Err(BundleError::BadMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != BUNDLE_VERSION {
        return Err(BundleError::UnsupportedVersion(version));
    }
    let key_id: [u8; KEY_ID_LEN] = bytes[6..22]
        .try_into()
        .map_err(|_| BundleError::Malformed)?;
    if bytes[22] != KDF_ARGON2ID {
        return Err(BundleError::UnsupportedKdf(bytes[22]));
    }
    let word = |offset: usize| {
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("fixed-width header field"),
        )
    };
    let params = KdfParams {
        m_cost_kib: word(23),
        t_cost: word(27),
        p_cost: word(31),
    };
    params.validate()?;
    let salt: [u8; SALT_LEN] = bytes[35..HEADER_LEN]
        .try_into()
        .map_err(|_| BundleError::Malformed)?;
    Ok((
        BundleHeader {
            version,
            key_id,
            params,
        },
        salt,
    ))
}

fn derive_kek(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    params: KdfParams,
) -> Result<Zeroizing<[u8; KEY_LEN]>, BundleError> {
    let argon_params = Params::new(
        params.m_cost_kib,
        params.t_cost,
        params.p_cost,
        Some(KEY_LEN),
    )
    .map_err(|error| BundleError::Kdf(error.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut kek = Zeroizing::new([0_u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase, salt, kek.as_mut())
        .map_err(|error| BundleError::Kdf(error.to_string()))?;
    Ok(kek)
}

fn armor(bytes: &[u8]) -> String {
    let body = STANDARD.encode(bytes);
    let mut out = String::new();
    out.push_str(ARMOR_BEGIN);
    out.push('\n');
    for chunk in body.as_bytes().chunks(ARMOR_WIDTH) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str("= ");
    out.push_str(&blake3::hash(bytes).to_hex()[..8]);
    out.push('\n');
    out.push_str(ARMOR_END);
    out.push('\n');
    out
}

fn dearmor(text: &str) -> Result<Vec<u8>, BundleError> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut body = String::new();
    let mut check = None;
    let mut began = false;
    let mut ended = false;
    for line in &mut lines {
        if !began {
            if line == ARMOR_BEGIN {
                began = true;
                continue;
            }
            return Err(BundleError::Malformed);
        }
        if line == ARMOR_END {
            ended = true;
            break;
        }
        if let Some(rest) = line.strip_prefix("= ") {
            check = Some(rest.to_owned());
            continue;
        }
        body.push_str(line);
    }
    if !began || !ended || lines.next().is_some() {
        return Err(BundleError::Malformed);
    }
    let bytes = STANDARD
        .decode(body.as_bytes())
        .map_err(|_| BundleError::Malformed)?;
    if let Some(check) = check {
        if blake3::hash(&bytes).to_hex()[..8] != *check {
            return Err(BundleError::ChecksumMismatch);
        }
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("recovery bundle is malformed")]
    Malformed,
    #[error("recovery bundle magic is invalid")]
    BadMagic,
    #[error("unsupported recovery bundle version {0}")]
    UnsupportedVersion(u16),
    #[error("unsupported recovery bundle KDF {0}")]
    UnsupportedKdf(u8),
    #[error("recovery bundle KDF parameters are below the accepted floor: {0:?}")]
    WeakKdfParameters(KdfParams),
    #[error("recovery bundle KDF parameters exceed the accepted ceiling: {0:?}")]
    ExcessiveKdfParameters(KdfParams),
    #[error("recovery bundle checksum does not match; the text was corrupted in transcription")]
    ChecksumMismatch,
    #[error("passphrase must be at least {0} bytes")]
    PassphraseTooShort(usize),
    #[error("wrong passphrase or tampered recovery bundle")]
    WrongPassphraseOrTampered,
    #[error("recovered key does not match the bundle's key identifier")]
    KeyIdMismatch,
    #[error("key derivation failed: {0}")]
    Kdf(String),
    #[error("authenticated encryption failed")]
    Aead,
}

impl From<chacha20poly1305::Error> for BundleError {
    fn from(_: chacha20poly1305::Error) -> Self {
        Self::Aead
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> MasterKey {
        MasterKey::from_bytes([7_u8; KEY_LEN])
    }

    #[test]
    fn round_trip_with_floor_parameters() {
        let master = master();
        let armored = seal(&master, b"correct horse battery", KdfParams::FLOOR).unwrap();
        assert!(armored.starts_with(ARMOR_BEGIN));
        let header = inspect(&armored).unwrap();
        assert_eq!(header.key_id, master.key_id());
        assert_eq!(header.params, KdfParams::FLOOR);
        let opened = open(&armored, b"correct horse battery").unwrap();
        assert_eq!(opened.key.bytes(), master.bytes());
        assert_eq!(opened.header, header);
    }

    #[test]
    fn wrong_passphrase_tamper_and_transcription_errors_fail_closed() {
        let master = master();
        let armored = seal(&master, b"correct horse battery", KdfParams::FLOOR).unwrap();
        assert!(matches!(
            open(&armored, b"incorrect horse battery"),
            Err(BundleError::WrongPassphraseOrTampered)
        ));

        let bytes = dearmor(&armored).unwrap();
        for index in [0, 5, 10, 30, HEADER_LEN + 1, BUNDLE_LEN - 1] {
            let mut tampered = bytes.clone();
            tampered[index] ^= 0x01;
            let Err(error) = open(&armor(&tampered), b"correct horse battery") else {
                panic!("tampering byte {index} was not detected");
            };
            assert!(
                matches!(
                    error,
                    BundleError::WrongPassphraseOrTampered
                        | BundleError::BadMagic
                        | BundleError::UnsupportedVersion(_)
                        | BundleError::WeakKdfParameters(_)
                        | BundleError::ExcessiveKdfParameters(_)
                ),
                "index {index}: {error}"
            );
        }

        let mut transcribed = armored.clone();
        let position = transcribed.find("\n").unwrap() + 3;
        let original = transcribed.as_bytes()[position];
        let replacement = if original == b'A' { b'B' } else { b'A' };
        transcribed.replace_range(
            position..position + 1,
            std::str::from_utf8(&[replacement]).unwrap(),
        );
        assert!(matches!(
            open(&transcribed, b"correct horse battery"),
            Err(BundleError::ChecksumMismatch)
        ));

        let mut truncated = bytes.clone();
        truncated.pop();
        assert!(matches!(
            open(&armor(&truncated), b"correct horse battery"),
            Err(BundleError::Malformed)
        ));
    }

    #[test]
    fn weak_parameters_and_short_passphrases_are_refused() {
        let master = master();
        let weak = KdfParams {
            m_cost_kib: 1024,
            ..KdfParams::FLOOR
        };
        assert!(matches!(
            seal(&master, b"correct horse battery", weak),
            Err(BundleError::WeakKdfParameters(_))
        ));
        assert!(matches!(
            seal(&master, b"short", KdfParams::FLOOR),
            Err(BundleError::PassphraseTooShort(_))
        ));
        let excessive = KdfParams {
            t_cost: 1 << 24,
            ..KdfParams::FLOOR
        };
        assert!(matches!(
            seal(&master, b"correct horse battery", excessive),
            Err(BundleError::ExcessiveKdfParameters(_))
        ));
    }

    #[test]
    fn key_ids_are_stable_and_generated_passphrases_are_long() {
        let master = master();
        assert_eq!(
            master.key_id(),
            MasterKey::from_bytes([7_u8; KEY_LEN]).key_id()
        );
        assert_ne!(
            master.key_id(),
            MasterKey::from_bytes([8_u8; KEY_LEN]).key_id()
        );
        let passphrase = generate_passphrase();
        assert_eq!(passphrase.len(), 29);
        assert!(passphrase.bytes().filter(|byte| *byte == b'-').count() == 5);
        assert_ne!(*generate_passphrase(), *passphrase);
    }
}

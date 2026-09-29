use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::durability::{ensure_directory, sync_directory, sync_regular_file};

pub const ENVELOPE_VERSION: u16 = 1;
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const KEY_ID_LEN: usize = 16;
const KEY_ID_CONTEXT: &str = "distr-hnsw:master-key-id:v1";

/// The server master key. Zeroized on drop.
#[derive(Clone)]
pub struct MasterKey([u8; KEY_LEN]);

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl MasterKey {
    /// Generate a fresh key and persist it durably with owner-only permissions.
    pub fn create(path: &Path) -> Result<Self, CryptoError> {
        let mut bytes = [0_u8; KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        let key = Self(bytes);
        key.write_new(path)?;
        Ok(key)
    }

    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Persist this key to a path that must not already exist.
    pub fn write_new(&self, path: &Path) -> Result<(), CryptoError> {
        if path.exists() {
            return Err(CryptoError::KeyAlreadyExists(path.to_owned()));
        }
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&self.0)?;
        sync_regular_file(&file)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }

    /// Non-secret identifier derived from the key. It lets the portal refuse a
    /// wrong key before decrypting anything and lets a recovery bundle name
    /// the key it wraps without revealing it.
    pub fn key_id(&self) -> [u8; KEY_ID_LEN] {
        let derived = blake3::derive_key(KEY_ID_CONTEXT, &self.0);
        derived[..KEY_ID_LEN]
            .try_into()
            .expect("derive_key yields 32 bytes")
    }

    pub fn key_id_hex(&self) -> String {
        hex::encode(self.key_id())
    }

    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        let metadata = fs::metadata(path)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(CryptoError::InsecureKeyPermissions(path.to_owned()));
        }
        let mut file = File::open(path)?;
        let mut bytes = [0_u8; KEY_LEN];
        file.read_exact(&mut bytes)?;
        let mut trailing = [0_u8; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(CryptoError::InvalidKeyLength);
        }
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrappedKey {
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

pub fn random_key() -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    OsRng.fill_bytes(key.as_mut());
    key
}

pub fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0_u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

pub fn wrap_key(
    master: &MasterKey,
    purpose: &[u8],
    file_id: Uuid,
    generation: u64,
    key: &[u8; KEY_LEN],
) -> Result<WrappedKey, CryptoError> {
    let nonce = random_nonce();
    let aad = key_aad(purpose, file_id, generation);
    let cipher = XChaCha20Poly1305::new(master.bytes().into());
    let ciphertext = cipher.encrypt(
        XNonce::from_slice(&nonce),
        Payload {
            msg: key,
            aad: &aad,
        },
    )?;
    Ok(WrappedKey { nonce, ciphertext })
}

pub fn unwrap_key(
    master: &MasterKey,
    purpose: &[u8],
    file_id: Uuid,
    generation: u64,
    wrapped: &WrappedKey,
) -> Result<Zeroizing<[u8; KEY_LEN]>, CryptoError> {
    let aad = key_aad(purpose, file_id, generation);
    let cipher = XChaCha20Poly1305::new(master.bytes().into());
    let plaintext = Zeroizing::new(cipher.decrypt(
        XNonce::from_slice(&wrapped.nonce),
        Payload {
            msg: &wrapped.ciphertext,
            aad: &aad,
        },
    )?);
    let key: [u8; KEY_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::InvalidKeyLength)?;
    Ok(Zeroizing::new(key))
}

pub fn encrypt_chunk(
    key: &[u8; KEY_LEN],
    envelope_version: u16,
    file_id: Uuid,
    ordinal: u32,
    plaintext_len: u32,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() != plaintext_len as usize {
        return Err(CryptoError::PlaintextLengthMismatch);
    }
    if envelope_version != ENVELOPE_VERSION {
        return Err(CryptoError::UnsupportedEnvelopeVersion(envelope_version));
    }
    let aad = chunk_aad(envelope_version, file_id, ordinal, plaintext_len);
    let cipher = XChaCha20Poly1305::new(key.into());
    Ok(cipher.encrypt(
        XNonce::from_slice(nonce),
        Payload {
            msg: plaintext,
            aad: &aad,
        },
    )?)
}

pub fn decrypt_chunk(
    key: &[u8; KEY_LEN],
    envelope_version: u16,
    file_id: Uuid,
    ordinal: u32,
    plaintext_len: u32,
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if envelope_version != ENVELOPE_VERSION {
        return Err(CryptoError::UnsupportedEnvelopeVersion(envelope_version));
    }
    let aad = chunk_aad(envelope_version, file_id, ordinal, plaintext_len);
    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = cipher.decrypt(
        XNonce::from_slice(nonce),
        Payload {
            msg: ciphertext,
            aad: &aad,
        },
    )?;
    if plaintext.len() != plaintext_len as usize {
        return Err(CryptoError::PlaintextLengthMismatch);
    }
    Ok(plaintext)
}

fn key_aad(purpose: &[u8], file_id: Uuid, generation: u64) -> Vec<u8> {
    let mut aad = b"distr-hnsw:key:v1:".to_vec();
    aad.extend_from_slice(purpose);
    aad.extend_from_slice(file_id.as_bytes());
    aad.extend_from_slice(&generation.to_le_bytes());
    aad
}

fn chunk_aad(envelope_version: u16, file_id: Uuid, ordinal: u32, plaintext_len: u32) -> Vec<u8> {
    let mut aad = match envelope_version {
        1 => b"distr-hnsw:chunk:v1".to_vec(),
        _ => unreachable!("unsupported versions are rejected before AAD construction"),
    };
    aad.extend_from_slice(file_id.as_bytes());
    aad.extend_from_slice(&ordinal.to_le_bytes());
    aad.extend_from_slice(&plaintext_len.to_le_bytes());
    aad
}

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("master key already exists: {0}")]
    KeyAlreadyExists(std::path::PathBuf),
    #[error("master key permissions must not grant group or other access: {0}")]
    InsecureKeyPermissions(std::path::PathBuf),
    #[error("invalid key length")]
    InvalidKeyLength,
    #[error("plaintext length does not match authenticated chunk metadata")]
    PlaintextLengthMismatch,
    #[error("unsupported envelope version {0}")]
    UnsupportedEnvelopeVersion(u16),
    #[error("authenticated encryption or decryption failed")]
    Aead,
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<chacha20poly1305::Error> for CryptoError {
    fn from(_: chacha20poly1305::Error) -> Self {
        Self::Aead
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_v1_aad_remains_byte_compatible() {
        let file_id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let aad = chunk_aad(1, file_id, 7, 11);
        let mut expected = b"distr-hnsw:chunk:v1".to_vec();
        expected.extend_from_slice(file_id.as_bytes());
        expected.extend_from_slice(&7_u32.to_le_bytes());
        expected.extend_from_slice(&11_u32.to_le_bytes());
        assert_eq!(aad, expected);
    }
}

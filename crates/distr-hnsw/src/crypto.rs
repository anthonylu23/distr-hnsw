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

use crate::durability::{ensure_directory, sync_directory};

pub const ENVELOPE_VERSION: u16 = 1;
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct MasterKey([u8; KEY_LEN]);

impl MasterKey {
    pub fn create(path: &Path) -> Result<Self, CryptoError> {
        if path.exists() {
            return Err(CryptoError::KeyAlreadyExists(path.to_owned()));
        }
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        let mut bytes = [0_u8; KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(Self(bytes))
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

pub fn random_key() -> [u8; KEY_LEN] {
    let mut key = [0_u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
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
) -> Result<[u8; KEY_LEN], CryptoError> {
    let aad = key_aad(purpose, file_id, generation);
    let cipher = XChaCha20Poly1305::new(master.bytes().into());
    let plaintext = cipher.decrypt(
        XNonce::from_slice(&wrapped.nonce),
        Payload {
            msg: &wrapped.ciphertext,
            aad: &aad,
        },
    )?;
    plaintext
        .try_into()
        .map_err(|_| CryptoError::InvalidKeyLength)
}

pub fn encrypt_chunk(
    key: &[u8; KEY_LEN],
    file_id: Uuid,
    ordinal: u32,
    plaintext_len: u32,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() != plaintext_len as usize {
        return Err(CryptoError::PlaintextLengthMismatch);
    }
    let aad = chunk_aad(file_id, ordinal, plaintext_len);
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
    file_id: Uuid,
    ordinal: u32,
    plaintext_len: u32,
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let aad = chunk_aad(file_id, ordinal, plaintext_len);
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

fn chunk_aad(file_id: Uuid, ordinal: u32, plaintext_len: u32) -> Vec<u8> {
    let mut aad = b"distr-hnsw:chunk:v1".to_vec();
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

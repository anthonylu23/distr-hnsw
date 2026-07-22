use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    crypto::{
        random_key, random_nonce, unwrap_key, wrap_key, CryptoError, MasterKey, WrappedKey,
        ENVELOPE_VERSION, NONCE_LEN,
    },
    object::ObjectHash,
};

const MAGIC: &[u8; 4] = b"DHMF";
const MAX_FIELD_LEN: usize = 16 * 1024 * 1024;
const MANIFEST_KEY_PURPOSE: &[u8] = b"manifest";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkRecord {
    pub ordinal: u32,
    pub plaintext_len: u32,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext_hash: ObjectHash,
    pub ciphertext_len: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPayload {
    pub name: String,
    pub plaintext_len: u64,
    pub plaintext_hash: [u8; 32],
    pub content_key: WrappedKey,
    pub chunks: Vec<ChunkRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedManifest {
    pub file_id: Uuid,
    pub generation: u64,
    pub payload: ManifestPayload,
}

pub fn encode_manifest(
    master: &MasterKey,
    file_id: Uuid,
    generation: u64,
    payload: &ManifestPayload,
) -> Result<Vec<u8>, FormatError> {
    let manifest_key = random_key();
    let wrapped_manifest_key = wrap_key(
        master,
        MANIFEST_KEY_PURPOSE,
        file_id,
        generation,
        &manifest_key,
    )?;
    let payload_nonce = random_nonce();
    let clear_header = clear_header(file_id, generation);
    let payload_bytes = encode_payload(payload)?;
    let cipher = XChaCha20Poly1305::new((&manifest_key).into());
    let encrypted_payload = cipher.encrypt(
        XNonce::from_slice(&payload_nonce),
        Payload {
            msg: &payload_bytes,
            aad: &clear_header,
        },
    )?;

    let mut output = clear_header;
    output.extend_from_slice(&wrapped_manifest_key.nonce);
    push_bytes(&mut output, &wrapped_manifest_key.ciphertext)?;
    output.extend_from_slice(&payload_nonce);
    push_bytes(&mut output, &encrypted_payload)?;
    Ok(output)
}

pub fn decode_manifest(master: &MasterKey, bytes: &[u8]) -> Result<DecodedManifest, FormatError> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != MAGIC {
        return Err(FormatError::BadMagic);
    }
    let version = reader.u16()?;
    if version != ENVELOPE_VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let file_id = Uuid::from_slice(reader.take(16)?).map_err(|_| FormatError::InvalidUuid)?;
    let generation = reader.u64()?;
    let clear_header_len = reader.position();
    let wrapped_manifest_key = WrappedKey {
        nonce: reader.array()?,
        ciphertext: reader.bytes()?.to_vec(),
    };
    let payload_nonce: [u8; NONCE_LEN] = reader.array()?;
    let encrypted_payload = reader.bytes()?;
    reader.finish()?;

    let manifest_key = unwrap_key(
        master,
        MANIFEST_KEY_PURPOSE,
        file_id,
        generation,
        &wrapped_manifest_key,
    )?;
    let cipher = XChaCha20Poly1305::new((&manifest_key).into());
    let plaintext = cipher.decrypt(
        XNonce::from_slice(&payload_nonce),
        Payload {
            msg: encrypted_payload,
            aad: &bytes[..clear_header_len],
        },
    )?;
    let payload = decode_payload(&plaintext)?;
    Ok(DecodedManifest {
        file_id,
        generation,
        payload,
    })
}

fn clear_header(file_id: Uuid, generation: u64) -> Vec<u8> {
    let mut output = Vec::with_capacity(30);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&ENVELOPE_VERSION.to_le_bytes());
    output.extend_from_slice(file_id.as_bytes());
    output.extend_from_slice(&generation.to_le_bytes());
    output
}

fn encode_payload(payload: &ManifestPayload) -> Result<Vec<u8>, FormatError> {
    let mut output = Vec::new();
    push_bytes(&mut output, payload.name.as_bytes())?;
    output.extend_from_slice(&payload.plaintext_len.to_le_bytes());
    output.extend_from_slice(&payload.plaintext_hash);
    output.extend_from_slice(&payload.content_key.nonce);
    push_bytes(&mut output, &payload.content_key.ciphertext)?;
    let count = u32::try_from(payload.chunks.len()).map_err(|_| FormatError::FieldTooLarge)?;
    output.extend_from_slice(&count.to_le_bytes());
    for chunk in &payload.chunks {
        output.extend_from_slice(&chunk.ordinal.to_le_bytes());
        output.extend_from_slice(&chunk.plaintext_len.to_le_bytes());
        output.extend_from_slice(&chunk.nonce);
        output.extend_from_slice(&hex::decode(chunk.ciphertext_hash.as_str())?);
        output.extend_from_slice(&chunk.ciphertext_len.to_le_bytes());
    }
    Ok(output)
}

fn decode_payload(bytes: &[u8]) -> Result<ManifestPayload, FormatError> {
    let mut reader = Reader::new(bytes);
    let name = String::from_utf8(reader.bytes()?.to_vec()).map_err(|_| FormatError::InvalidUtf8)?;
    let plaintext_len = reader.u64()?;
    let plaintext_hash = reader.array()?;
    let content_key = WrappedKey {
        nonce: reader.array()?,
        ciphertext: reader.bytes()?.to_vec(),
    };
    let count = reader.u32()? as usize;
    if count > MAX_FIELD_LEN / 64 {
        return Err(FormatError::FieldTooLarge);
    }
    let mut chunks = Vec::with_capacity(count);
    for expected_ordinal in 0..count {
        let ordinal = reader.u32()?;
        if ordinal as usize != expected_ordinal {
            return Err(FormatError::NonCanonicalOrdinal);
        }
        let plaintext_len = reader.u32()?;
        let nonce = reader.array()?;
        let hash_bytes: [u8; 32] = reader.array()?;
        let ciphertext_hash = ObjectHash::parse(hex::encode(hash_bytes))?;
        let ciphertext_len = reader.u32()?;
        chunks.push(ChunkRecord {
            ordinal,
            plaintext_len,
            nonce,
            ciphertext_hash,
            ciphertext_len,
        });
    }
    reader.finish()?;
    Ok(ManifestPayload {
        name,
        plaintext_len,
        plaintext_hash,
        content_key,
        chunks,
    })
}

fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), FormatError> {
    if bytes.len() > MAX_FIELD_LEN {
        return Err(FormatError::FieldTooLarge);
    }
    let length = u32::try_from(bytes.len()).map_err(|_| FormatError::FieldTooLarge)?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], FormatError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(FormatError::Truncated)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(FormatError::Truncated)?;
        self.position = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        self.take(N)?.try_into().map_err(|_| FormatError::Truncated)
    }

    fn bytes(&mut self) -> Result<&'a [u8], FormatError> {
        let count = self.u32()? as usize;
        if count > MAX_FIELD_LEN {
            return Err(FormatError::FieldTooLarge);
        }
        self.take(count)
    }

    fn finish(self) -> Result<(), FormatError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(FormatError::TrailingBytes)
        }
    }
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("manifest magic is invalid")]
    BadMagic,
    #[error("unsupported manifest version {0}")]
    UnsupportedVersion(u16),
    #[error("manifest is truncated")]
    Truncated,
    #[error("manifest contains trailing bytes")]
    TrailingBytes,
    #[error("manifest field exceeds the format limit")]
    FieldTooLarge,
    #[error("manifest contains invalid UTF-8")]
    InvalidUtf8,
    #[error("manifest contains an invalid file id")]
    InvalidUuid,
    #[error("manifest chunk ordinals are not canonical")]
    NonCanonicalOrdinal,
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("authenticated manifest encryption or decryption failed")]
    Aead,
    #[error(transparent)]
    Hex(#[from] hex::FromHexError),
    #[error(transparent)]
    Object(#[from] crate::object::ObjectError),
}

impl From<chacha20poly1305::Error> for FormatError {
    fn from(_: chacha20poly1305::Error) -> Self {
        Self::Aead
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::crypto::{random_nonce, wrap_key, KEY_LEN};

    #[test]
    fn manifest_round_trip_and_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("master.key");
        let master = MasterKey::create(&key_path).unwrap();
        assert_eq!(
            std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let file_id = Uuid::new_v4();
        let content_key = [7_u8; KEY_LEN];
        let payload = ManifestPayload {
            name: "example.bin".to_owned(),
            plaintext_len: 3,
            plaintext_hash: *blake3::hash(b"abc").as_bytes(),
            content_key: wrap_key(&master, b"content", file_id, 1, &content_key).unwrap(),
            chunks: vec![ChunkRecord {
                ordinal: 0,
                plaintext_len: 3,
                nonce: random_nonce(),
                ciphertext_hash: ObjectHash::digest(b"ciphertext"),
                ciphertext_len: 10,
            }],
        };

        let encoded = encode_manifest(&master, file_id, 1, &payload).unwrap();
        let decoded = decode_manifest(&master, &encoded).unwrap();
        assert_eq!(decoded.file_id, file_id);
        assert_eq!(decoded.generation, 1);
        assert_eq!(decoded.payload, payload);

        let mut tampered = encoded;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(decode_manifest(&master, &tampered).is_err());
    }

    #[test]
    fn payload_encoding_matches_the_v1_golden_bytes() {
        let payload = ManifestPayload {
            name: "a".to_owned(),
            plaintext_len: 3,
            plaintext_hash: [1; 32],
            content_key: WrappedKey {
                nonce: [2; NONCE_LEN],
                ciphertext: vec![3, 4, 5],
            },
            chunks: vec![ChunkRecord {
                ordinal: 0,
                plaintext_len: 3,
                nonce: [6; NONCE_LEN],
                ciphertext_hash: ObjectHash::parse("07".repeat(32)).unwrap(),
                ciphertext_len: 19,
            }],
        };
        let expected = "01000000610300000000000000010101010101010101010101010101010101010101010101010101010101010102020202020202020202020202020202020202020202020203000000030405010000000000000003000000060606060606060606060606060606060606060606060606070707070707070707070707070707070707070707070707070707070707070713000000";
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(hex::encode(&encoded), expected);
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn decode_rejects_bad_magic_unsupported_version_and_trailing_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let master = MasterKey::create(&directory.path().join("master.key")).unwrap();
        let file_id = Uuid::new_v4();
        let payload = ManifestPayload {
            name: "example.bin".to_owned(),
            plaintext_len: 0,
            plaintext_hash: [0; 32],
            content_key: wrap_key(&master, b"content", file_id, 1, &[9_u8; KEY_LEN]).unwrap(),
            chunks: Vec::new(),
        };
        let encoded = encode_manifest(&master, file_id, 1, &payload).unwrap();

        let mut bad_magic = encoded.clone();
        bad_magic[0] ^= 1;
        assert!(matches!(
            decode_manifest(&master, &bad_magic),
            Err(FormatError::BadMagic)
        ));

        let mut unsupported = encoded.clone();
        unsupported[4] = 2;
        unsupported[5] = 0;
        assert!(matches!(
            decode_manifest(&master, &unsupported),
            Err(FormatError::UnsupportedVersion(2))
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            decode_manifest(&master, &trailing),
            Err(FormatError::TrailingBytes)
        ));
    }

    #[test]
    fn decode_payload_rejects_unreasonable_lengths() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&((MAX_FIELD_LEN as u32) + 1).to_le_bytes());
        encoded.extend_from_slice(&[0_u8; 8]);
        assert!(matches!(
            decode_payload(&encoded),
            Err(FormatError::FieldTooLarge)
        ));
    }
}

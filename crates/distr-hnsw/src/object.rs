use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Chunk,
    Manifest,
}

impl ObjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::Manifest => "manifest",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ObjectKind {
    type Err = ObjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "chunk" => Ok(Self::Chunk),
            "manifest" => Ok(Self::Manifest),
            _ => Err(ObjectError::InvalidKind(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectHash(String);

impl ObjectHash {
    pub fn digest(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ObjectError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(ObjectError::InvalidHash(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ObjectHash {
    type Err = ObjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Debug, Error)]
pub enum ObjectError {
    #[error("unsupported object kind: {0}")]
    InvalidKind(String),
    #[error("invalid lowercase BLAKE3 object hash: {0}")]
    InvalidHash(String),
}

pub mod agent;
pub mod backup;
pub mod crypto;
pub mod durability;
pub mod format;
pub mod index_archive;
pub mod lifecycle;
pub mod metadata;
pub mod object;
pub mod portal;
pub mod reconcile;
pub mod recovery;
pub mod recovery_bundle;
pub mod tailscale;

pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Largest object body an agent accepts: one chunk plus envelope slack. A
/// manifest must fit too, which bounds the chunk count of one file.
pub const MAX_OBJECT_BYTES: usize = CHUNK_SIZE + 1024 * 1024;

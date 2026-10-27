//! Durable directory creation and the per-partition directory lock.
//!
//! Directory entries are only durable once their parent is synced, so every
//! directory the engine creates is followed by an `fsync` of its parent (the
//! M1 `ensure_child_directory` rule, `docs/m1-filesystem-qualification.md`).

use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

/// `fsync` a directory so the entries created in it are durable.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Create `path` and any missing ancestors, syncing each new directory's
/// parent so the new entries survive power loss. A no-op when `path` exists.
pub(crate) fn create_dir_durable(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_owned());
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => break,
        }
    }
    fs::create_dir_all(path)?;
    for directory in missing.iter().rev() {
        sync_dir(parent_of(directory))?;
    }
    Ok(())
}

/// Name of the lock file inside a partition directory.
pub const LOCK_NAME: &str = "LOCK";

/// An exclusive advisory lock (`flock`) on a partition directory. A
/// partition has at most one owner at a time: a serving process, an
/// archive, or a restore. The lock is released when this value is dropped.
#[derive(Debug)]
pub struct PartitionLock {
    directory: PathBuf,
    _file: File,
}

impl PartitionLock {
    /// Take the lock without waiting; `Ok(None)` when another owner holds it.
    pub(crate) fn try_acquire(directory: &Path) -> io::Result<Option<Self>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(LOCK_NAME))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                directory: directory.to_owned(),
                _file: file,
            })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

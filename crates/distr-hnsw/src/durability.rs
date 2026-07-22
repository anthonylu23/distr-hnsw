use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;

use crate::object::{ObjectHash, ObjectKind};

#[derive(Clone, Debug)]
pub struct DurableStore {
    root: PathBuf,
}

impl DurableStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        ensure_directory(&root)?;
        let objects = root.join("objects");
        ensure_child_directory(&root, &objects)?;
        ensure_child_directory(&objects, &objects.join("chunk"))?;
        ensure_child_directory(&objects, &objects.join("manifest"))?;
        Ok(Self { root })
    }

    pub fn put(
        &self,
        kind: ObjectKind,
        expected: &ObjectHash,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        let actual = ObjectHash::digest(bytes);
        if &actual != expected {
            return Err(StoreError::HashMismatch {
                expected: expected.clone(),
                actual,
            });
        }

        let final_path = self.object_path(kind, expected);
        let parent = final_path
            .parent()
            .ok_or_else(|| StoreError::InvalidPath(final_path.clone()))?;
        let namespace = self.root.join("objects").join(kind.as_str());
        let first_prefix = namespace.join(&expected.as_str()[0..2]);
        ensure_child_directory(&namespace, &first_prefix)?;
        ensure_child_directory(&first_prefix, parent)?;

        if final_path.exists() {
            match self.get(kind, expected) {
                Ok(_) => return Ok(()),
                Err(StoreError::HashMismatch { .. }) => {}
                Err(error) => return Err(error),
            }
        }

        let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            sync_regular_file(&file)?;
            drop(file);
            fs::rename(&temporary, &final_path)?;
            sync_directory(parent)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn get(&self, kind: ObjectKind, expected: &ObjectHash) -> Result<Vec<u8>, StoreError> {
        let path = self.object_path(kind, expected);
        let bytes = fs::read(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                StoreError::NotFound(expected.clone())
            } else {
                StoreError::Io(error)
            }
        })?;
        let actual = ObjectHash::digest(&bytes);
        if &actual != expected {
            return Err(StoreError::HashMismatch {
                expected: expected.clone(),
                actual,
            });
        }
        Ok(bytes)
    }

    pub fn object_path(&self, kind: ObjectKind, hash: &ObjectHash) -> PathBuf {
        let value = hash.as_str();
        self.root
            .join("objects")
            .join(kind.as_str())
            .join(&value[0..2])
            .join(&value[2..4])
            .join(value)
    }
}

fn sync_regular_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;

        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if result == 0 {
            return Ok(());
        }
    }
    file.sync_all()
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    File::open(path)?.sync_all()
}

pub(crate) fn ensure_directory(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if path.is_dir() {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("directory has no parent: {}", path.display()),
        )
    })?;
    ensure_directory(parent)?;
    ensure_child_directory(parent, path)
}

fn ensure_child_directory(parent: &Path, child: &Path) -> io::Result<()> {
    match fs::create_dir(child) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if child.is_dir() {
                Ok(())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("object {0} was not found")]
    NotFound(ObjectHash),
    #[error("object hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        expected: ObjectHash,
        actual: ObjectHash,
    },
    #[error("invalid object path: {0}")]
    InvalidPath(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_idempotent_and_get_verifies_hash() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableStore::open(directory.path()).unwrap();
        let bytes = b"durable bytes";
        let hash = ObjectHash::digest(bytes);

        store.put(ObjectKind::Chunk, &hash, bytes).unwrap();
        store.put(ObjectKind::Chunk, &hash, bytes).unwrap();
        assert_eq!(store.get(ObjectKind::Chunk, &hash).unwrap(), bytes);

        fs::write(store.object_path(ObjectKind::Chunk, &hash), b"corrupt").unwrap();
        assert!(matches!(
            store.get(ObjectKind::Chunk, &hash),
            Err(StoreError::HashMismatch { .. })
        ));
    }
}

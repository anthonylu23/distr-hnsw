use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;
use uuid::Uuid;

use crate::object::{ObjectHash, ObjectKind};

pub const MAX_INVENTORY_PAGE: usize = 1_000;
const INCARNATION_FILE: &str = "incarnation";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct InventoryObject {
    pub hash: ObjectHash,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct InventoryPage {
    pub objects: Vec<InventoryObject>,
    pub next_after: Option<ObjectHash>,
}

#[derive(Debug)]
pub struct DurableStore {
    root: PathBuf,
    /// Bytes held by stored objects, computed on open and maintained by put
    /// and delete. Temporary files are not counted.
    used_bytes: AtomicU64,
}

impl DurableStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        ensure_directory(&root)?;
        let objects = root.join("objects");
        ensure_child_directory(&root, &objects)?;
        for kind in ObjectKind::ALL {
            ensure_child_directory(&objects, &objects.join(kind.as_str()))?;
        }
        let store = Self {
            root,
            used_bytes: AtomicU64::new(0),
        };
        let mut used = 0_u64;
        for kind in ObjectKind::ALL {
            let mut after = None;
            loop {
                let page = store.inventory(kind, after.as_ref(), MAX_INVENTORY_PAGE)?;
                used = used.saturating_add(page.objects.iter().map(|object| object.size).sum());
                match page.next_after {
                    Some(cursor) => after = Some(cursor),
                    None => break,
                }
            }
        }
        store.used_bytes.store(used, Ordering::Relaxed);
        Ok(store)
    }

    pub fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed)
    }

    /// Free bytes reported by the filesystem holding the volume.
    pub fn filesystem_free_bytes(&self) -> Result<(u64, u64), StoreError> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(self.root.as_os_str().as_bytes())
            .map_err(|_| StoreError::InvalidPath(self.root.clone()))?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::statvfs(path.as_ptr(), &mut stats) };
        if result != 0 {
            return Err(StoreError::Io(io::Error::last_os_error()));
        }
        let fragment = stats.f_frsize.max(1);
        Ok((
            stats.f_bavail.saturating_mul(fragment),
            stats.f_blocks.saturating_mul(fragment),
        ))
    }

    /// Physically remove an object. Idempotent: a missing object is not an
    /// error. Only garbage-collection proofs may call this (see
    /// `docs/m1-lifecycle-contract.md`).
    pub fn delete(&self, kind: ObjectKind, hash: &ObjectHash) -> Result<bool, StoreError> {
        let path = self.object_path(kind, hash);
        let size = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(StoreError::Io(error)),
        };
        fs::remove_file(&path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        self.used_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(size))
            })
            .ok();
        Ok(true)
    }

    /// Load the immutable incarnation identity of this volume, creating it
    /// durably on first use. Reusing an agent name on a fresh volume yields a
    /// new incarnation; a malformed identity fails closed.
    pub fn load_or_create_incarnation(&self) -> Result<String, StoreError> {
        let path = self.root.join(INCARNATION_FILE);
        match fs::read_to_string(&path) {
            Ok(contents) => {
                let value = contents.trim();
                Uuid::parse_str(value)
                    .map(|_| value.to_owned())
                    .map_err(|_| StoreError::MalformedIncarnation(path))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let value = Uuid::new_v4().to_string();
                let temporary = self.root.join(format!(".{INCARNATION_FILE}.{value}.tmp"));
                let result = (|| {
                    let mut file = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&temporary)?;
                    file.write_all(value.as_bytes())?;
                    file.write_all(b"\n")?;
                    sync_regular_file(&file)?;
                    drop(file);
                    // A concurrent creator wins; never overwrite an identity.
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            let _ = fs::remove_file(&temporary);
                            return self.load_or_create_incarnation();
                        }
                        Err(error) => return Err(StoreError::Io(error)),
                    }
                    fs::remove_file(&temporary)?;
                    sync_directory(&self.root)?;
                    Ok(value.clone())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                result
            }
            Err(error) => Err(StoreError::Io(error)),
        }
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

        let mut replaced_bytes = 0_u64;
        if final_path.exists() {
            match self.get(kind, expected) {
                Ok(_) => return Ok(()),
                Err(StoreError::HashMismatch { .. }) => {
                    replaced_bytes = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
                }
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
        } else {
            self.used_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                    Some(used.saturating_sub(replaced_bytes) + bytes.len() as u64)
                })
                .ok();
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

    pub fn inventory(
        &self,
        kind: ObjectKind,
        after: Option<&ObjectHash>,
        limit: usize,
    ) -> Result<InventoryPage, StoreError> {
        if limit == 0 {
            return Err(StoreError::InvalidInventoryLimit(limit));
        }
        let limit = limit.min(MAX_INVENTORY_PAGE);
        let namespace = self.root.join("objects").join(kind.as_str());
        let mut objects = Vec::with_capacity(limit);
        let mut has_more = false;

        'prefixes: for first in sorted_entries(&namespace)? {
            if !first.path().is_dir() || !is_hex_prefix(&first.file_name()) {
                return Err(StoreError::MalformedInventoryEntry(first.path()));
            }
            for second in sorted_entries(&first.path())? {
                if !second.path().is_dir() || !is_hex_prefix(&second.file_name()) {
                    return Err(StoreError::MalformedInventoryEntry(second.path()));
                }
                for entry in sorted_entries(&second.path())? {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') && name.ends_with(".tmp") {
                        continue;
                    }
                    if !entry.path().is_file() {
                        return Err(StoreError::MalformedInventoryEntry(entry.path()));
                    }
                    let hash = ObjectHash::parse(name.as_ref())
                        .map_err(|_| StoreError::MalformedInventoryEntry(entry.path()))?;
                    let expected_first = first.file_name();
                    let expected_second = second.file_name();
                    if expected_first.to_string_lossy() != hash.as_str()[0..2]
                        || expected_second.to_string_lossy() != hash.as_str()[2..4]
                    {
                        return Err(StoreError::MalformedInventoryEntry(entry.path()));
                    }
                    if after.is_some_and(|cursor| hash.as_str() <= cursor.as_str()) {
                        continue;
                    }
                    if objects.len() == limit {
                        has_more = true;
                        break 'prefixes;
                    }
                    objects.push(InventoryObject {
                        hash,
                        size: entry.metadata()?.len(),
                    });
                }
            }
        }
        let next_after = has_more
            .then(|| objects.last().map(|object| object.hash.clone()))
            .flatten();
        Ok(InventoryPage {
            objects,
            next_after,
        })
    }
}

fn sorted_entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn is_hex_prefix(value: &std::ffi::OsStr) -> bool {
    value.to_str().is_some_and(|value| {
        value.len() == 2
            && value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    })
}

pub(crate) fn sync_regular_file(file: &File) -> io::Result<()> {
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
    #[error("inventory limit must be greater than zero, got {0}")]
    InvalidInventoryLimit(usize),
    #[error("malformed object-store inventory entry: {0}")]
    MalformedInventoryEntry(PathBuf),
    #[error("malformed agent incarnation identity: {0}")]
    MalformedIncarnation(PathBuf),
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
        assert_eq!(store.used_bytes(), bytes.len() as u64);
        assert_eq!(
            DurableStore::open(directory.path()).unwrap().used_bytes(),
            bytes.len() as u64
        );
        assert!(store.delete(ObjectKind::Chunk, &hash).unwrap());
        assert!(!store.delete(ObjectKind::Chunk, &hash).unwrap());
        assert_eq!(store.used_bytes(), 0);
        store.put(ObjectKind::Chunk, &hash, bytes).unwrap();

        fs::write(store.object_path(ObjectKind::Chunk, &hash), b"corrupt").unwrap();
        assert!(matches!(
            store.get(ObjectKind::Chunk, &hash),
            Err(StoreError::HashMismatch { .. })
        ));
    }

    #[test]
    fn inventory_paginates_exclusively_without_gaps_or_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableStore::open(directory.path()).unwrap();
        let mut expected = Vec::new();
        for bytes in [b"first".as_slice(), b"second", b"third"] {
            let hash = ObjectHash::digest(bytes);
            store.put(ObjectKind::Manifest, &hash, bytes).unwrap();
            expected.push(hash);
        }
        expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));

        let first = store.inventory(ObjectKind::Manifest, None, 2).unwrap();
        assert_eq!(first.objects.len(), 2);
        assert_eq!(first.next_after.as_ref(), Some(&first.objects[1].hash));
        let second = store
            .inventory(ObjectKind::Manifest, first.next_after.as_ref(), 2)
            .unwrap();
        assert_eq!(second.objects.len(), 1);
        assert!(second.next_after.is_none());
        let actual: Vec<_> = first
            .objects
            .into_iter()
            .chain(second.objects)
            .map(|object| object.hash)
            .collect();
        assert_eq!(actual, expected);
        assert!(matches!(
            store.inventory(ObjectKind::Manifest, None, 0),
            Err(StoreError::InvalidInventoryLimit(0))
        ));
        assert!(store
            .inventory(ObjectKind::Manifest, None, MAX_INVENTORY_PAGE + 1)
            .is_ok());
    }

    #[test]
    fn incarnation_is_created_once_and_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableStore::open(directory.path()).unwrap();
        let first = store.load_or_create_incarnation().unwrap();
        assert!(Uuid::parse_str(&first).is_ok());
        let reopened = DurableStore::open(directory.path()).unwrap();
        assert_eq!(reopened.load_or_create_incarnation().unwrap(), first);
        assert!(!directory.path().read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));

        fs::write(directory.path().join(INCARNATION_FILE), "not-a-uuid\n").unwrap();
        assert!(matches!(
            store.load_or_create_incarnation(),
            Err(StoreError::MalformedIncarnation(_))
        ));
    }

    #[test]
    fn wiped_volume_yields_a_new_incarnation() {
        let directory = tempfile::tempdir().unwrap();
        let first = DurableStore::open(directory.path())
            .unwrap()
            .load_or_create_incarnation()
            .unwrap();
        fs::remove_dir_all(directory.path()).unwrap();
        let second = DurableStore::open(directory.path())
            .unwrap()
            .load_or_create_incarnation()
            .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn malformed_inventory_entries_fail_the_page() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableStore::open(directory.path()).unwrap();
        let malformed = directory.path().join("objects/manifest/not-hex");
        fs::create_dir(&malformed).unwrap();
        assert!(matches!(
            store.inventory(ObjectKind::Manifest, None, 10),
            Err(StoreError::MalformedInventoryEntry(_))
        ));
    }
}

//! Archive a vector partition's durable state (manifest, newest snapshot,
//! WAL segments) through the M1 blob plane and restore it into an empty
//! directory (engine contract §4 rule 5, §5; roadmap M3 "archive snapshots
//! and WAL segments through the M1 blob plane").
//!
//! Each file is committed as a regular blob-plane file (4 MiB encrypted
//! chunks, RF2, immutable manifest) under a deterministic idempotency key
//! derived from the partition id, the file's role, its name, and its BLAKE3,
//! so re-archiving an unchanged file is a no-op and a changed WAL segment
//! (it grows until rotation) is committed as a new file. The portal records
//! which file id holds each archived part in `index_archives`.
//!
//! Archive requires the partition to be offline: it takes the partition's
//! directory lock (refusing with `Locked` if a server holds it), verifies
//! the partition without repairing anything, and keeps the lock until it
//! finishes, so no file changes while it is read and uploaded. Restore
//! assembles the partition in a staging directory, verifies every file and a
//! full recovery there, and only then renames it into place.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use distr_hnsw_index::partition::Partition;

use crate::{
    metadata::{IndexArchiveRow, MetadataError},
    portal::{Portal, PortalError},
};

const SNAP_DIR: &str = "snap";
const WAL_DIR: &str = "wal";
const MANIFEST_NAME: &str = "partition.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePart {
    Manifest,
    Snapshot,
    WalSegment,
}

impl ArchivePart {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Snapshot => "snapshot",
            Self::WalSegment => "wal_segment",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "manifest" => Some(Self::Manifest),
            "snapshot" => Some(Self::Snapshot),
            "wal_segment" => Some(Self::WalSegment),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchivedFile {
    pub part: ArchivePart,
    pub name: String,
    pub blake3: String,
    pub bytes: u64,
    pub file_id: Uuid,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchiveReportV1 {
    pub report_type: &'static str,
    pub version: u16,
    pub partition_id: Uuid,
    pub files: Vec<ArchivedFile>,
    pub uploaded: usize,
    pub unchanged: usize,
    pub wal_segments_truncated: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RestoreReportV1 {
    pub report_type: &'static str,
    pub version: u16,
    pub partition_id: Uuid,
    pub destination: PathBuf,
    pub files: Vec<ArchivedFile>,
    pub recovery_high_water: u64,
    pub wal_entries_replayed: u64,
    pub snapshots_rejected: usize,
}

struct Candidate {
    part: ArchivePart,
    name: String,
    path: PathBuf,
}

/// Parts in commit order: manifest, WAL segments, then the snapshot, so an
/// interrupted archive can leave an older snapshot with newer segments
/// (recoverable) but never a newer snapshot with stale segments.
fn candidates(
    partition_dir: &Path,
    snapshot: Option<&Path>,
) -> Result<Vec<Candidate>, IndexArchiveError> {
    let mut out = vec![Candidate {
        part: ArchivePart::Manifest,
        name: MANIFEST_NAME.to_owned(),
        path: partition_dir.join(MANIFEST_NAME),
    }];
    let wal_dir = partition_dir.join(WAL_DIR);
    if wal_dir.exists() {
        let mut segments: Vec<(String, PathBuf)> = fs::read_dir(&wal_dir)?
            .filter_map(|e| e.ok())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(name, _)| name.ends_with(".wal"))
            .collect();
        segments.sort();
        for (name, path) in segments {
            out.push(Candidate {
                part: ArchivePart::WalSegment,
                name,
                path,
            });
        }
    }
    // Newest verifiable snapshot only: older ones are superseded.
    if let Some(path) = snapshot {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or(IndexArchiveError::Corrupt("snapshot name"))?;
        out.push(Candidate {
            part: ArchivePart::Snapshot,
            name,
            path: path.to_owned(),
        });
    }
    Ok(out)
}

fn digest(path: &Path) -> Result<(String, u64), IndexArchiveError> {
    let bytes = fs::read(path)?;
    Ok((
        blake3::hash(&bytes).to_hex().to_string(),
        bytes.len() as u64,
    ))
}

/// The sequence number in a `<20 digits>.<extension>` name.
fn numbered(name: &str, extension: &str) -> Option<u64> {
    let stem = name.strip_suffix(extension)?;
    (stem.len() == 20 && stem.bytes().all(|b| b.is_ascii_digit()))
        .then(|| stem.parse().ok())
        .flatten()
}

/// Archive the partition's manifest, every WAL segment, and its newest
/// snapshot. A part counts as unchanged only when its recorded hash matches
/// and the recorded file is still visible; otherwise it is uploaded again.
/// With `truncate_wal`, segments wholly covered by the archived snapshot are
/// removed locally after their archive copy is verified (copy-first).
pub async fn archive(
    portal: &mut Portal,
    partition_dir: &Path,
    truncate_wal: bool,
) -> Result<ArchiveReportV1, IndexArchiveError> {
    let lock = Partition::lock(partition_dir)?;
    let report = Partition::inspect(&lock)?;
    let partition_id = report.partition_id;
    let snapshot_high_water = report.snapshot_high_water;

    let mut files = Vec::new();
    let mut uploaded = 0;
    let mut unchanged = 0;
    for candidate in candidates(partition_dir, report.snapshot_used.as_deref())? {
        let (hash, bytes) = digest(&candidate.path)?;
        let key = format!(
            "index:{partition_id}:{}:{}:{hash}",
            candidate.part.as_str(),
            candidate.name
        );
        let existing = portal.database.index_archive_lookup(
            partition_id,
            candidate.part.as_str(),
            &candidate.name,
        )?;
        let recorded = match existing {
            Some((file_id, existing_hash)) if existing_hash == hash => {
                portal.is_visible(file_id)?.then_some(file_id)
            }
            _ => None,
        };
        let (file_id, status) = match recorded {
            Some(file_id) => {
                unchanged += 1;
                (file_id, "unchanged")
            }
            None => {
                let mut file_id = portal.upload(&candidate.path, &key).await?;
                if !portal.is_visible(file_id)? {
                    // The key's earlier file was deleted since it was
                    // archived; the same key would return it again, so
                    // commit a fresh file under a key naming the lost one.
                    file_id = portal
                        .upload(&candidate.path, &format!("{key}:replacing:{file_id}"))
                        .await?;
                }
                if !portal.is_visible(file_id)? {
                    return Err(IndexArchiveError::Corrupt(
                        "archived part is not visible after upload",
                    ));
                }
                portal.database.index_archive_record(
                    partition_id,
                    candidate.part.as_str(),
                    &candidate.name,
                    &hash,
                    bytes,
                    file_id,
                )?;
                uploaded += 1;
                (file_id, "uploaded")
            }
        };
        files.push(ArchivedFile {
            part: candidate.part,
            name: candidate.name,
            blake3: hash,
            bytes,
            file_id,
            status: status.to_owned(),
        });
    }

    let mut truncated = 0;
    if truncate_wal {
        // Only segments archived in this run (each verified visible above,
        // under the hash of the bytes on disk) and wholly below the archived
        // snapshot's high-water mark may go; the newest segment never does.
        let mut segments: Vec<(u64, String)> = files
            .iter()
            .filter(|f| f.part == ArchivePart::WalSegment)
            .filter_map(|f| numbered(&f.name, ".wal").map(|first| (first, f.name.clone())))
            .collect();
        segments.sort();
        for window in segments.windows(2) {
            let (_, name) = &window[0];
            let (next_first, _) = &window[1];
            if *next_first <= snapshot_high_water + 1 {
                fs::remove_file(partition_dir.join(WAL_DIR).join(name))?;
                truncated += 1;
            }
        }
        if truncated > 0 {
            fs::File::open(partition_dir.join(WAL_DIR))?.sync_all()?;
        }
    }
    drop(lock);

    Ok(ArchiveReportV1 {
        report_type: "IndexArchiveReportV1",
        version: 1,
        partition_id,
        files,
        uploaded,
        unchanged,
        wal_segments_truncated: truncated,
    })
}

/// The rows a restore needs: the manifest, the newest snapshot, and every
/// segment not wholly covered by it. Names are validated before any of them
/// becomes a path.
fn restore_set(
    rows: Vec<IndexArchiveRow>,
) -> Result<Vec<(ArchivePart, IndexArchiveRow)>, IndexArchiveError> {
    let mut manifest = None;
    let mut snapshot: Option<(u64, IndexArchiveRow)> = None;
    let mut segments: Vec<(u64, IndexArchiveRow)> = Vec::new();
    for row in rows {
        match ArchivePart::parse(&row.part).ok_or(IndexArchiveError::Corrupt("archive part"))? {
            ArchivePart::Manifest => {
                if row.name != MANIFEST_NAME {
                    return Err(IndexArchiveError::Corrupt("manifest name"));
                }
                manifest = Some(row);
            }
            ArchivePart::Snapshot => {
                let high_water = numbered(&row.name, ".snap")
                    .ok_or(IndexArchiveError::Corrupt("snapshot name"))?;
                if snapshot
                    .as_ref()
                    .is_none_or(|(newest, _)| high_water > *newest)
                {
                    snapshot = Some((high_water, row));
                }
            }
            ArchivePart::WalSegment => {
                let first = numbered(&row.name, ".wal")
                    .ok_or(IndexArchiveError::Corrupt("segment name"))?;
                segments.push((first, row));
            }
        }
    }
    let manifest = manifest.ok_or(IndexArchiveError::Corrupt("no archived manifest"))?;
    segments.sort_by_key(|(first, _)| *first);
    let covered = snapshot.as_ref().map_or(0, |(high_water, _)| *high_water);
    let firsts: Vec<u64> = segments.iter().map(|(first, _)| *first).collect();
    let mut out = vec![(ArchivePart::Manifest, manifest)];
    for (index, (_, row)) in segments.into_iter().enumerate() {
        let wholly_covered = firsts
            .get(index + 1)
            .is_some_and(|next| *next <= covered + 1);
        if !wholly_covered {
            out.push((ArchivePart::WalSegment, row));
        }
    }
    if let Some((_, row)) = snapshot {
        out.push((ArchivePart::Snapshot, row));
    }
    Ok(out)
}

/// Restore a partition into `destination`, which must not exist or be an
/// empty directory. Everything is assembled and verified (each file's
/// BLAKE3, then a full recovery) in a sibling staging directory, which is
/// renamed into place only when it opens; a failure leaves nothing behind.
pub async fn restore(
    portal: &Portal,
    partition_id: Uuid,
    destination: &Path,
) -> Result<RestoreReportV1, IndexArchiveError> {
    if destination.exists() && fs::read_dir(destination)?.next().is_some() {
        return Err(IndexArchiveError::DestinationNotEmpty(
            destination.to_owned(),
        ));
    }
    let rows = portal.database.index_archive_list(partition_id)?;
    if rows.is_empty() {
        return Err(IndexArchiveError::NothingArchived(partition_id));
    }
    let set = restore_set(rows)?;
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    };
    let name = destination
        .file_name()
        .ok_or(IndexArchiveError::Corrupt("destination name"))?
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&parent)?;
    let staging = parent.join(format!(".{name}.restoring-{}", Uuid::new_v4()));
    let result = restore_into(portal, set, &staging, destination).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let (files, report) = result?;
    Ok(RestoreReportV1 {
        report_type: "IndexRestoreReportV1",
        version: 1,
        partition_id: report.partition_id,
        destination: destination.to_owned(),
        files,
        recovery_high_water: report.high_water,
        wal_entries_replayed: report.wal_entries_replayed,
        snapshots_rejected: report.snapshots_rejected.len(),
    })
}

async fn restore_into(
    portal: &Portal,
    set: Vec<(ArchivePart, IndexArchiveRow)>,
    staging: &Path,
    destination: &Path,
) -> Result<
    (
        Vec<ArchivedFile>,
        distr_hnsw_index::partition::RecoveryReport,
    ),
    IndexArchiveError,
> {
    fs::create_dir_all(staging.join(SNAP_DIR))?;
    fs::create_dir_all(staging.join(WAL_DIR))?;
    let mut files = Vec::new();
    for (part, row) in set {
        let (name, hash, bytes, file_id) = (row.name, row.blake3, row.bytes, row.file_id);
        let target = match part {
            ArchivePart::Manifest => staging.join(&name),
            ArchivePart::Snapshot => staging.join(SNAP_DIR).join(&name),
            ArchivePart::WalSegment => staging.join(WAL_DIR).join(&name),
        };
        portal.download(file_id, &target).await?;
        let (got, _) = digest(&target)?;
        if got != hash {
            return Err(IndexArchiveError::Corrupt("restored file hash"));
        }
        files.push(ArchivedFile {
            part,
            name,
            blake3: hash,
            bytes,
            file_id,
            status: "restored".to_owned(),
        });
    }
    // A full recovery in the staging directory proves the set is complete
    // (snapshot verified, WAL continuous and reaching it) before it is
    // published under the destination name.
    let (partition, report) = Partition::open(staging)?;
    drop(partition);
    for directory in [
        staging.join(SNAP_DIR),
        staging.join(WAL_DIR),
        staging.to_owned(),
    ] {
        fs::File::open(&directory)?.sync_all()?;
    }
    if destination.exists() {
        fs::remove_dir(destination)?;
    }
    fs::rename(staging, destination)?;
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::File::open(parent)?.sync_all()?;
    Ok((files, report))
}

#[derive(Debug, Error)]
pub enum IndexArchiveError {
    #[error("restore destination is not empty: {0}")]
    DestinationNotEmpty(PathBuf),
    #[error("no archived parts recorded for partition {0}")]
    NothingArchived(Uuid),
    #[error("archive record is corrupt: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Partition(#[from] distr_hnsw_index::partition::PartitionError),
    #[error(transparent)]
    Portal(#[from] PortalError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

//! Archive a vector partition's durable state (manifest, newest snapshot,
//! WAL segments) through the M1 blob plane and restore it into an empty
//! directory (engine contract §4 rule 5, §5; roadmap M3 "archive snapshots
//! and WAL segments through the M1 blob plane").
//!
//! Each file is committed as a regular blob-plane file (4 MiB encrypted
//! chunks, RF2, immutable manifest) under a deterministic idempotency key
//! derived from the partition id, the file's role, its name, and its BLAKE3,
//! so re-archiving an unchanged file is a no-op and a changed WAL segment
//! (it grows until rotation) becomes a new file version. The portal records
//! which file id holds each archived part in `index_archives`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    metadata::MetadataError,
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

fn candidates(
    partition_dir: &Path,
    snapshot_high_water: u64,
) -> Result<Vec<Candidate>, IndexArchiveError> {
    let mut out = vec![Candidate {
        part: ArchivePart::Manifest,
        name: MANIFEST_NAME.to_owned(),
        path: partition_dir.join(MANIFEST_NAME),
    }];
    // Newest snapshot only: older ones are superseded.
    let snapshot_name = format!("{snapshot_high_water:020}.snap");
    let snapshot_path = partition_dir.join(SNAP_DIR).join(&snapshot_name);
    if snapshot_path.exists() {
        out.push(Candidate {
            part: ArchivePart::Snapshot,
            name: snapshot_name,
            path: snapshot_path,
        });
    }
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
    Ok(out)
}

fn digest(path: &Path) -> Result<(String, u64), IndexArchiveError> {
    let bytes = fs::read(path)?;
    Ok((
        blake3::hash(&bytes).to_hex().to_string(),
        bytes.len() as u64,
    ))
}

/// Archive the partition's manifest, newest snapshot, and every WAL segment.
/// With `truncate_wal`, segments wholly covered by the archived snapshot are
/// removed locally after their archive copy is committed (copy-first).
pub async fn archive(
    portal: &mut Portal,
    partition_dir: &Path,
    truncate_wal: bool,
) -> Result<ArchiveReportV1, IndexArchiveError> {
    let (partition, report) = distr_hnsw_index::partition::Partition::open(partition_dir)?;
    let partition_id = partition.id();
    let snapshot_high_water = report.snapshot_high_water;
    drop(partition);

    let mut files = Vec::new();
    let mut uploaded = 0;
    let mut unchanged = 0;
    for candidate in candidates(partition_dir, snapshot_high_water)? {
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
        let (file_id, status) = match existing {
            Some((file_id, existing_hash)) if existing_hash == hash => {
                unchanged += 1;
                (file_id, "unchanged")
            }
            _ => {
                let file_id = portal.upload(&candidate.path, &key).await?;
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
        // Only segments that are both archived and wholly below the archived
        // snapshot's high-water mark may go; the newest segment never does.
        let mut segments: Vec<(u64, String)> = files
            .iter()
            .filter(|f| f.part == ArchivePart::WalSegment)
            .filter_map(|f| {
                f.name
                    .strip_suffix(".wal")
                    .and_then(|s| s.parse().ok())
                    .map(|first| (first, f.name.clone()))
            })
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

/// Download every archived part of a partition into an empty directory and
/// open it, returning the recovery outcome.
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
    fs::create_dir_all(destination.join(SNAP_DIR))?;
    fs::create_dir_all(destination.join(WAL_DIR))?;
    let mut files = Vec::new();
    for row in rows {
        let part =
            ArchivePart::parse(&row.part).ok_or(IndexArchiveError::Corrupt("archive part"))?;
        let (name, hash, bytes, file_id) = (row.name, row.blake3, row.bytes, row.file_id);
        let target = match part {
            ArchivePart::Manifest => destination.join(&name),
            ArchivePart::Snapshot => destination.join(SNAP_DIR).join(&name),
            ArchivePart::WalSegment => destination.join(WAL_DIR).join(&name),
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
    let (partition, report) = distr_hnsw_index::partition::Partition::open(destination)?;
    Ok(RestoreReportV1 {
        report_type: "IndexRestoreReportV1",
        version: 1,
        partition_id: partition.id(),
        destination: destination.to_owned(),
        files,
        recovery_high_water: report.high_water,
        wal_entries_replayed: report.wal_entries_replayed,
        snapshots_rejected: report.snapshots_rejected.len(),
    })
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

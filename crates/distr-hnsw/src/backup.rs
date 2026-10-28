//! Backup set v1: offsite copies of committed encrypted objects and SQLite
//! snapshots, and the restore paths that rebuild a portal from them.
//!
//! Layout inside a target (decided 2026-09-29, DESIGN §11.1):
//!
//! ```text
//! backup/v1/objects/<kind>/<hash>        immutable ciphertext, never overwritten
//! backup/v1/sqlite/<utc>-<hash8>.db      VACUUM INTO snapshots
//! backup/v1/catalog/<utc>-<job>.json     inventory written after each job
//! ```
//!
//! Objects are self-verifying by content address, so the catalog is advisory;
//! authenticity of the set rests on the target's versioning and immutability.
//! The backup job copies ciphertext and decrypts nothing; it needs the master
//! key only to unseal the key that signs its agent grants.
//!
//! Targets: a versioned directory (`dir:<path>`) and an S3-compatible bucket
//! (`s3:<bucket>[/<prefix>]`, see [`s3`]). Both are dispatched through
//! [`BackupTargetKind`]; adapters never overwrite.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod s3;

pub use s3::S3Target;

use axum::body::Bytes;

use crate::{
    agent_client::AgentClient,
    durability::{ensure_directory, sync_directory, sync_regular_file},
    metadata::{Database, HistoryObject, JobStatus, MetadataError, ReconcileJob},
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
};

const LAYOUT_ROOT: &str = "backup/v1";
const MINIMUM_REPLICAS: usize = 2;

/// Operator-facing target specification: `dir:<path>` or
/// `s3:<bucket>[/<prefix>]`. The S3 form takes its endpoint and credentials
/// from the environment (see [`s3`]); neither is part of the target id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupTargetSpec {
    Directory(PathBuf),
    S3 { bucket: String, prefix: String },
}

impl BackupTargetSpec {
    /// Stable identifier persisted with the backup bookkeeping.
    pub fn id(&self) -> String {
        match self {
            Self::Directory(path) => format!("dir:{}", path.display()),
            Self::S3 { bucket, prefix } => s3::target_id(bucket, prefix),
        }
    }

    /// Open the target, verifying its preconditions (an S3 bucket must have
    /// versioning enabled).
    pub async fn open(&self) -> Result<BackupTargetKind, BackupError> {
        match self {
            Self::Directory(path) => Ok(BackupTargetKind::Directory(DirectoryTarget::open(path)?)),
            Self::S3 { bucket, prefix } => Ok(BackupTargetKind::S3(
                S3Target::open_from_env(bucket, prefix).await?,
            )),
        }
    }
}

impl FromStr for BackupTargetSpec {
    type Err = BackupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.split_once(':') {
            Some(("dir", path)) if !path.is_empty() => Ok(Self::Directory(PathBuf::from(path))),
            Some(("s3", rest)) if !rest.is_empty() => {
                let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
                let prefix = prefix.trim_matches('/');
                if bucket.is_empty() || (!prefix.is_empty() && validate_path(prefix).is_err()) {
                    return Err(BackupError::InvalidTarget(value.to_owned()));
                }
                Ok(Self::S3 {
                    bucket: bucket.to_owned(),
                    prefix: prefix.to_owned(),
                })
            }
            _ => Err(BackupError::InvalidTarget(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PutOutcome {
    Created,
    /// The path already held byte-identical content.
    Identical,
}

/// A versioned object-store-like backup destination. Adapters never
/// overwrite: an existing path with different bytes is a conflict. Paths are
/// `/`-separated and relative to the layout root (`backup/v1/...`) for every
/// adapter; the S3 adapter maps them under its prefix and strips it again
/// when listing. An enum rather than a trait object so the S3 adapter can be
/// async without boxed futures.
pub enum BackupTargetKind {
    Directory(DirectoryTarget),
    S3(S3Target),
}

impl BackupTargetKind {
    pub fn id(&self) -> String {
        match self {
            Self::Directory(target) => target.id(),
            Self::S3(target) => target.id(),
        }
    }

    pub async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<PutOutcome, BackupError> {
        match self {
            Self::Directory(target) => target.put_new(path, bytes),
            Self::S3(target) => target.put_new(path, bytes).await,
        }
    }

    pub async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, BackupError> {
        match self {
            Self::Directory(target) => target.get(path),
            Self::S3(target) => target.get(path).await,
        }
    }

    /// Paths under `prefix`, sorted, with sizes.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, BackupError> {
        match self {
            Self::Directory(target) => target.list(prefix),
            Self::S3(target) => target.list(prefix).await,
        }
    }
}

/// Reject empty, absolute, and dot-segment paths before they reach a target.
fn validate_path(path: &str) -> Result<(), BackupError> {
    if path.is_empty()
        || path.starts_with('/')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(BackupError::InvalidPath(path.to_owned()));
    }
    Ok(())
}

/// Versioned-directory adapter: durable writes under one root. Immutability
/// is layout-level only; offsite protection needs a rotated or remote disk.
pub struct DirectoryTarget {
    root: PathBuf,
}

impl DirectoryTarget {
    pub fn open(root: &Path) -> Result<Self, BackupError> {
        ensure_directory(root)?;
        Ok(Self {
            root: root.to_owned(),
        })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, BackupError> {
        validate_path(path)?;
        Ok(self.root.join(path))
    }

    pub fn id(&self) -> String {
        format!("dir:{}", self.root.display())
    }

    pub fn put_new(&self, path: &str, bytes: &[u8]) -> Result<PutOutcome, BackupError> {
        let final_path = self.resolve(path)?;
        if final_path.exists() {
            let existing = fs::read(&final_path)?;
            return if existing == bytes {
                Ok(PutOutcome::Identical)
            } else {
                Err(BackupError::Conflict(path.to_owned()))
            };
        }
        let parent = final_path
            .parent()
            .ok_or_else(|| BackupError::InvalidPath(path.to_owned()))?;
        ensure_directory(parent)?;
        let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            sync_regular_file(&file)?;
            drop(file);
            // link(2) fails if the final path appeared meanwhile: never overwrite.
            match fs::hard_link(&temporary, &final_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    fs::remove_file(&temporary)?;
                    let existing = fs::read(&final_path)?;
                    return if existing == bytes {
                        Ok(PutOutcome::Identical)
                    } else {
                        Err(BackupError::Conflict(path.to_owned()))
                    };
                }
                Err(error) => return Err(BackupError::Io(error)),
            }
            fs::remove_file(&temporary)?;
            sync_directory(parent)?;
            Ok(PutOutcome::Created)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn get(&self, path: &str) -> Result<Option<Vec<u8>>, BackupError> {
        match fs::read(self.resolve(path)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(BackupError::Io(error)),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, BackupError> {
        let base = self.resolve(prefix)?;
        let mut out = Vec::new();
        if !base.exists() {
            return Ok(out);
        }
        let mut stack = vec![base];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path
                        .strip_prefix(&self.root)
                        .map_err(|_| BackupError::InvalidPath(path.display().to_string()))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((relative, entry.metadata()?.len()));
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

pub fn object_path(kind: ObjectKind, hash: &ObjectHash) -> String {
    format!("{LAYOUT_ROOT}/objects/{}/{hash}", kind.as_str())
}

fn snapshot_path(name: &str) -> String {
    format!("{LAYOUT_ROOT}/sqlite/{name}")
}

fn catalog_path(name: &str) -> String {
    format!("{LAYOUT_ROOT}/catalog/{name}")
}

fn utc_stamp(seconds: i64) -> String {
    // Seconds since the epoch formatted as YYYYMMDDTHHMMSSZ without pulling in
    // a calendar crate: civil-from-days per Howard Hinnant.
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Inverse of [`utc_stamp`]: `YYYYMMDDTHHMMSSZ` to seconds since the epoch.
fn parse_utc_stamp(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    let field = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (y, m, d) = (field(0..4)?, field(4..6)?, field(6..8)?);
    let (hh, mm, ss) = (field(9..11)?, field(11..13)?, field(13..15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // days-from-civil per Howard Hinnant.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn now_seconds() -> Result<i64, BackupError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BackupError::Clock)?
        .as_secs();
    i64::try_from(now).map_err(|_| BackupError::Clock)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogObject {
    pub kind: ObjectKind,
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogV1 {
    pub catalog_type: String,
    pub version: u16,
    pub job_id: Uuid,
    pub created_at: i64,
    pub master_key_id: Option<String>,
    pub objects: Vec<CatalogObject>,
    pub snapshot: Option<SnapshotRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub name: String,
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BackupTotals {
    pub history_objects: usize,
    pub verified_before: usize,
    pub copied_now: usize,
    pub identical_on_target: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupReportV1 {
    pub report_type: String,
    pub version: u16,
    pub job_id: Uuid,
    pub target_id: String,
    pub totals: BackupTotals,
    pub snapshot: Option<SnapshotRecord>,
    pub snapshot_skipped_identical: bool,
    pub catalog: String,
    pub backup_lag_seconds: i64,
    /// Result of re-downloading and hashing a sample of previously verified
    /// objects from the target, when requested.
    pub integrity_sample: Option<IntegritySample>,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntegritySample {
    pub sampled_at: i64,
    pub requested: usize,
    pub sampled: usize,
    pub valid: usize,
    pub corrupt: Vec<String>,
    pub missing: Vec<String>,
}

impl BackupReportV1 {
    pub fn exit_code(&self) -> u8 {
        if self.totals.failed == 0 && self.issues.is_empty() {
            0
        } else {
            2
        }
    }
}

/// Copy every historical object to the target, verify each copy by reading
/// it back, ship a SQLite snapshot if the database changed, and write a
/// catalog. Restartable: verified objects are skipped on the next run.
pub async fn backup(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &AgentClient,
    spec: &BackupTargetSpec,
    snapshot_path_hint: &Path,
) -> Result<BackupReportV1, BackupError> {
    backup_with_sample(database, agents, client, spec, snapshot_path_hint, 0).await
}

/// Like [`backup`], additionally re-downloading and hashing `sample`
/// previously verified objects spread across the set (integrity sampling,
/// DESIGN §11.1). A corrupt or missing offsite copy is an issue, not a
/// silent re-copy: the target is expected to be immutable.
pub async fn backup_with_sample(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &AgentClient,
    spec: &BackupTargetSpec,
    snapshot_path_hint: &Path,
    sample: usize,
) -> Result<BackupReportV1, BackupError> {
    let target = spec.open().await?;
    let target_id = spec.id();
    let job_id = database.create_backup_job(&target_id)?;
    let result = run_backup(
        database,
        agents,
        client,
        &target,
        &target_id,
        job_id,
        snapshot_path_hint,
        sample,
    )
    .await;
    match result {
        Ok(report) => {
            let json = serde_json::to_string(&report)?;
            database.finish_backup_job(job_id, JobStatus::Complete, Some(&json))?;
            Ok(report)
        }
        Err(error) => {
            let detail = error.to_string();
            database.finish_backup_job(job_id, JobStatus::Failed, Some(&detail))?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_backup(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &AgentClient,
    target: &BackupTargetKind,
    target_id: &str,
    job_id: Uuid,
    snapshot_path_hint: &Path,
    sample: usize,
) -> Result<BackupReportV1, BackupError> {
    // Snapshot first, objects second: history only grows, so every object
    // the snapshot references is in the history read below, and the snapshot
    // is shipped only once all of them are verified on the target. A snapshot
    // taken after the copy could reference files committed meanwhile whose
    // objects the set lacks.
    let generation = database.content_generation()?;
    let latest = database.latest_backup_snapshot(target_id)?;
    let pending_snapshot = match &latest {
        Some(previous) if previous.content_generation == generation => None,
        _ => {
            let temporary = snapshot_path_hint.with_extension(format!("snapshot-{job_id}.tmp"));
            let _ = fs::remove_file(&temporary);
            database.snapshot_into(&temporary)?;
            let snapshot_bytes = fs::read(&temporary);
            let _ = fs::remove_file(&temporary);
            Some(snapshot_bytes?)
        }
    };
    let history = database.history_objects()?;
    let mut totals = BackupTotals {
        history_objects: history.len(),
        ..BackupTotals::default()
    };
    let mut issues = Vec::new();
    let mut catalog_objects = Vec::with_capacity(history.len());
    let mut oldest_unverified: Option<i64> = None;

    for object in &history {
        if database.backup_object_verified(target_id, object.kind, &object.hash)? {
            totals.verified_before += 1;
            catalog_objects.push(CatalogObject {
                kind: object.kind,
                hash: object.hash.to_string(),
                size: object.expected_len.unwrap_or(0),
            });
            continue;
        }
        let Some(bytes) = fetch_from_agents(database, client, agents, object).await else {
            totals.failed += 1;
            oldest_unverified = Some(oldest_unverified.map_or(object.committed_at, |current| {
                current.min(object.committed_at)
            }));
            issues.push(format!(
                "no valid live copy of {} {} to back up",
                object.kind, object.hash
            ));
            continue;
        };
        let path = object_path(object.kind, &object.hash);
        let mut unverified = |totals: &mut BackupTotals, issue: String| {
            totals.failed += 1;
            oldest_unverified = Some(oldest_unverified.map_or(object.committed_at, |current| {
                current.min(object.committed_at)
            }));
            issues.push(issue);
        };
        match target.put_new(&path, &bytes).await {
            Ok(PutOutcome::Created) => totals.copied_now += 1,
            Ok(PutOutcome::Identical) => totals.identical_on_target += 1,
            Err(error) => {
                unverified(&mut totals, format!("target refused {path}: {error}"));
                continue;
            }
        }
        let verified = target
            .get(&path)
            .await?
            .is_some_and(|got| ObjectHash::digest(&got) == object.hash);
        if !verified {
            unverified(
                &mut totals,
                format!("target copy of {path} did not read back intact"),
            );
            continue;
        }
        database.record_backup_object(
            target_id,
            object.kind,
            &object.hash,
            bytes.len() as u64,
            job_id,
        )?;
        catalog_objects.push(CatalogObject {
            kind: object.kind,
            hash: object.hash.to_string(),
            size: bytes.len() as u64,
        });
    }

    // SQLite snapshot, skipped when no file-visible content changed since the
    // last shipped one (backup bookkeeping alone does not warrant a snapshot),
    // and withheld while any object it may reference is not yet verified.
    let now = now_seconds()?;
    let previous = latest.map(|previous| SnapshotRecord {
        name: previous.name,
        hash: previous.snapshot_hash,
        size: previous.size,
    });
    let (snapshot, skipped) = match pending_snapshot {
        None => (previous, true),
        Some(_) if totals.failed > 0 => {
            issues.push(format!(
                "SQLite snapshot not shipped: {} object(s) it may reference are not verified on the target",
                totals.failed
            ));
            (previous, false)
        }
        Some(snapshot_bytes) => {
            let snapshot_hash = blake3::hash(&snapshot_bytes).to_hex().to_string();
            let name = format!("{}-{}.db", utc_stamp(now), &snapshot_hash[..8]);
            target
                .put_new(&snapshot_path(&name), &snapshot_bytes)
                .await?;
            let verified = target
                .get(&snapshot_path(&name))
                .await?
                .is_some_and(|got| blake3::hash(&got).to_hex().to_string() == snapshot_hash);
            if !verified {
                return Err(BackupError::SnapshotVerification(name));
            }
            database.record_backup_snapshot(
                target_id,
                &name,
                &snapshot_hash,
                generation,
                snapshot_bytes.len() as u64,
                job_id,
            )?;
            (
                Some(SnapshotRecord {
                    name,
                    hash: snapshot_hash,
                    size: snapshot_bytes.len() as u64,
                }),
                false,
            )
        }
    };

    let catalog = CatalogV1 {
        catalog_type: "BackupCatalogV1".to_owned(),
        version: 1,
        job_id,
        created_at: now,
        master_key_id: database.master_key_id()?,
        objects: catalog_objects,
        snapshot: snapshot.clone(),
    };
    // Integrity sampling: every k-th verified object by hash order, offset by
    // the job id so successive runs cover different objects.
    let integrity_sample = if sample > 0 {
        let verified: Vec<&CatalogObject> = catalog.objects.iter().collect();
        let mut record = IntegritySample {
            sampled_at: now,
            requested: sample,
            ..IntegritySample::default()
        };
        if !verified.is_empty() {
            let stride = (verified.len() / sample).max(1);
            let offset = (job_id.as_u128() % stride as u128) as usize;
            for object in verified.iter().skip(offset).step_by(stride).take(sample) {
                let hash = ObjectHash::parse(object.hash.clone())
                    .map_err(|error| BackupError::S3(format!("catalog hash: {error}")))?;
                let path = object_path(object.kind, &hash);
                record.sampled += 1;
                match target.get(&path).await? {
                    Some(bytes) if ObjectHash::digest(&bytes) == hash => record.valid += 1,
                    Some(_) => record.corrupt.push(path),
                    None => record.missing.push(path),
                }
            }
        }
        for path in record.corrupt.iter().chain(record.missing.iter()) {
            issues.push(format!("offsite copy {path} failed integrity sampling"));
        }
        Some(record)
    } else {
        None
    };

    let catalog_name = format!("{}-{job_id}.json", utc_stamp(now));
    target
        .put_new(
            &catalog_path(&catalog_name),
            serde_json::to_string_pretty(&catalog)?.as_bytes(),
        )
        .await?;

    Ok(BackupReportV1 {
        report_type: "BackupReportV1".to_owned(),
        version: 1,
        job_id,
        target_id: target_id.to_owned(),
        totals,
        snapshot,
        snapshot_skipped_identical: skipped,
        catalog: catalog_name,
        backup_lag_seconds: oldest_unverified.map_or(0, |oldest| (now - oldest).max(0)),
        integrity_sample,
        issues,
    })
}

async fn fetch_from_agents(
    database: &Database,
    client: &AgentClient,
    agents: &[AgentTarget],
    object: &HistoryObject,
) -> Option<Vec<u8>> {
    let confirmed = database
        .confirmed_agents(object.kind, &object.hash)
        .unwrap_or_default();
    let ordered = agents
        .iter()
        .filter(|agent| confirmed.contains(&agent.id))
        .chain(agents.iter().filter(|agent| !confirmed.contains(&agent.id)));
    for agent in ordered {
        if let Ok(response) = client.get(agent, object.kind, &object.hash).await {
            if response.status().is_success() {
                if let Ok(bytes) = response.bytes().await {
                    if ObjectHash::digest(&bytes) == object.hash
                        && object
                            .expected_len
                            .is_none_or(|expected| expected == bytes.len() as u64)
                    {
                        return Some(bytes.to_vec());
                    }
                }
            }
        }
    }
    None
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupTargetStatus {
    pub target_id: String,
    pub last_job: Option<ReconcileJob>,
    pub objects_verified: usize,
    pub objects_pending: usize,
    pub backup_lag_seconds: i64,
    pub last_snapshot: Option<SnapshotRecord>,
    pub last_snapshot_at: Option<i64>,
    pub last_integrity_sample: Option<IntegritySample>,
}

#[derive(Deserialize)]
struct ReportSampleOnly {
    integrity_sample: Option<IntegritySample>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupStatusV1 {
    pub targets: Vec<BackupTargetStatus>,
    /// The last restore drill recorded with `portal drill record`.
    pub last_restore_drill: Option<RestoreDrillRecord>,
    /// True only when a passing empty-infrastructure restore drill within
    /// the drill window covers a configured target whose backup lag is
    /// within the drill's declared blob RPO (DESIGN §11.1).
    pub recovery_ready: bool,
    pub recovery_ready_reason: String,
}

const RESTORE_DRILL_META: &str = "last_restore_drill";
/// A drill is required at least quarterly (DESIGN §11.1); 92 days covers it.
pub const RESTORE_DRILL_WINDOW_SECONDS: i64 = 92 * 24 * 3600;

/// The facts of one restore drill that recovery readiness depends on,
/// extracted from a `RestoreDrillReportV1` (scripts/restore-drill.sh).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RestoreDrillRecord {
    pub stamp: String,
    pub completed_at: i64,
    pub recorded_at: i64,
    pub source_revision: String,
    pub target: String,
    pub verdict: String,
    pub within_targets: bool,
    pub declared_blob_rpo_seconds: i64,
    pub declared_portal_rto_seconds: i64,
    pub actual_recovery_point_lag_ms: i64,
    pub actual_recovery_time_ms: i64,
}

/// Record a restore drill report so `recovery_ready` can be derived from it.
/// The report is validated, not trusted blindly: it must be a
/// `RestoreDrillReportV1` with a parseable stamp and objectives.
pub fn record_restore_drill(
    database: &mut Database,
    report_json: &str,
) -> Result<RestoreDrillRecord, BackupError> {
    let invalid = |detail: &str| BackupError::InvalidDrillReport(detail.to_owned());
    let report: serde_json::Value = serde_json::from_str(report_json)?;
    if report["report_type"] != "RestoreDrillReportV1" {
        return Err(invalid("not a RestoreDrillReportV1"));
    }
    let text = |pointer: &str| {
        report
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(pointer))
    };
    let number = |pointer: &str| {
        report
            .pointer(pointer)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| invalid(pointer))
    };
    let stamp = text("/stamp")?;
    let completed_at = parse_utc_stamp(&stamp).ok_or_else(|| invalid("/stamp"))?;
    let record = RestoreDrillRecord {
        completed_at,
        recorded_at: now_seconds()?,
        source_revision: text("/source_revision")?,
        target: text("/target")?,
        verdict: text("/verdict")?,
        within_targets: report
            .pointer("/objectives/within_targets")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| invalid("/objectives/within_targets"))?,
        declared_blob_rpo_seconds: number("/objectives/declared_blob_rpo_seconds")? as i64,
        declared_portal_rto_seconds: number("/objectives/declared_portal_loss_rto_seconds")? as i64,
        actual_recovery_point_lag_ms: (number("/objectives/actual_recovery_point_lag_seconds")?
            * 1000.0)
            .round() as i64,
        actual_recovery_time_ms: (number("/objectives/actual_recovery_time_seconds")? * 1000.0)
            .round() as i64,
        stamp,
    };
    database.set_meta(RESTORE_DRILL_META, &serde_json::to_string(&record)?)?;
    Ok(record)
}

/// Whether a drill run against `drill_target` exercised `target_id`: the
/// same S3 bucket (any prefix, so a drill may use its own), or the same
/// directory.
fn drill_covers(drill_target: &str, target_id: &str) -> bool {
    match (
        drill_target.strip_prefix("s3:"),
        target_id.strip_prefix("s3:"),
    ) {
        (Some(drill), Some(target)) => drill.split('/').next() == target.split('/').next(),
        (None, None) => drill_target == target_id,
        _ => false,
    }
}

fn recovery_readiness(
    drill: Option<&RestoreDrillRecord>,
    targets: &[BackupTargetStatus],
    now: i64,
) -> (bool, String) {
    let Some(drill) = drill else {
        return (
            false,
            "no restore drill recorded; run scripts/restore-drill.sh against this deployment's target and `portal drill record`".to_owned(),
        );
    };
    if drill.verdict != "pass" || !drill.within_targets {
        return (
            false,
            format!(
                "last restore drill ({}) did not pass within its declared RPO/RTO",
                drill.stamp
            ),
        );
    }
    if now - drill.completed_at > RESTORE_DRILL_WINDOW_SECONDS {
        return (
            false,
            format!(
                "last passing restore drill ({}) is older than the quarterly window",
                drill.stamp
            ),
        );
    }
    let Some(target) = targets
        .iter()
        .find(|target| drill_covers(&drill.target, &target.target_id))
    else {
        return (
            false,
            format!(
                "restore drill target {} is not a configured backup target",
                drill.target
            ),
        );
    };
    if target.backup_lag_seconds > drill.declared_blob_rpo_seconds {
        return (
            false,
            format!(
                "backup lag on {} is {}s, above the declared blob RPO of {}s",
                target.target_id, target.backup_lag_seconds, drill.declared_blob_rpo_seconds
            ),
        );
    }
    (
        true,
        format!(
            "restore drill {} passed against {}; backup lag {}s within RPO {}s",
            drill.stamp,
            target.target_id,
            target.backup_lag_seconds,
            drill.declared_blob_rpo_seconds
        ),
    )
}

pub fn backup_status(database: &Database) -> Result<BackupStatusV1, BackupError> {
    let history = database.history_objects()?;
    let now = now_seconds()?;
    let mut targets = Vec::new();
    for target_id in database.backup_target_ids()? {
        let mut pending = 0;
        let mut oldest: Option<i64> = None;
        for object in &history {
            if !database.backup_object_verified(&target_id, object.kind, &object.hash)? {
                pending += 1;
                oldest = Some(oldest.map_or(object.committed_at, |current| {
                    current.min(object.committed_at)
                }));
            }
        }
        let snapshot = database.latest_backup_snapshot(&target_id)?;
        let last_job = database.latest_backup_job(&target_id)?;
        let last_integrity_sample = last_job
            .as_ref()
            .filter(|job| job.status == JobStatus::Complete)
            .and_then(|job| job.report.as_deref())
            .and_then(|report| serde_json::from_str::<ReportSampleOnly>(report).ok())
            .and_then(|report| report.integrity_sample);
        targets.push(BackupTargetStatus {
            last_job,
            last_integrity_sample,
            objects_verified: database.backup_object_count(&target_id)?,
            objects_pending: pending,
            backup_lag_seconds: oldest.map_or(0, |oldest| (now - oldest).max(0)),
            last_snapshot: snapshot.as_ref().map(|previous| SnapshotRecord {
                name: previous.name.clone(),
                hash: previous.snapshot_hash.clone(),
                size: previous.size,
            }),
            last_snapshot_at: snapshot.map(|previous| previous.created_at),
            target_id,
        });
    }
    let last_restore_drill = database
        .meta(RESTORE_DRILL_META)?
        .map(|json| serde_json::from_str::<RestoreDrillRecord>(&json))
        .transpose()?;
    let (recovery_ready, recovery_ready_reason) =
        recovery_readiness(last_restore_drill.as_ref(), &targets, now);
    Ok(BackupStatusV1 {
        targets,
        last_restore_drill,
        recovery_ready,
        recovery_ready_reason,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RestoreMetadataReport {
    pub report_type: String,
    pub snapshot: SnapshotRecord,
    pub database: PathBuf,
    pub catalog: Option<String>,
}

/// Restore the latest (or named) SQLite snapshot from the target into a path
/// that must not exist. The snapshot's hash is checked against the newest
/// catalog that names it when one exists.
pub async fn restore_metadata(
    spec: &BackupTargetSpec,
    database_path: &Path,
    snapshot_name: Option<&str>,
) -> Result<RestoreMetadataReport, BackupError> {
    if database_path.exists() {
        return Err(BackupError::DestinationExists(database_path.to_owned()));
    }
    let target = spec.open().await?;
    let snapshots = target.list(&format!("{LAYOUT_ROOT}/sqlite")).await?;
    let chosen = match snapshot_name {
        Some(name) => snapshots
            .iter()
            .find(|(path, _)| path.ends_with(&format!("/{name}")))
            .cloned(),
        None => snapshots.last().cloned(),
    }
    .ok_or(BackupError::NoSnapshot)?;
    let name = chosen.0.rsplit('/').next().unwrap_or(&chosen.0).to_owned();
    let bytes = target
        .get(&chosen.0)
        .await?
        .ok_or_else(|| BackupError::NoSnapshot)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    // Snapshot names end in the first eight hex digits of their BLAKE3 hash.
    let named_prefix = name
        .strip_suffix(".db")
        .and_then(|stem| stem.rsplit('-').next())
        .unwrap_or_default();
    if named_prefix.len() != 8 || !hash.starts_with(named_prefix) {
        return Err(BackupError::SnapshotVerification(name));
    }

    let mut catalog_name = None;
    for (path, _) in target
        .list(&format!("{LAYOUT_ROOT}/catalog"))
        .await?
        .into_iter()
        .rev()
    {
        let Some(raw) = target.get(&path).await? else {
            continue;
        };
        let Ok(catalog) = serde_json::from_slice::<CatalogV1>(&raw) else {
            continue;
        };
        if let Some(snapshot) = catalog.snapshot {
            if snapshot.name == name {
                if snapshot.hash != hash {
                    return Err(BackupError::SnapshotVerification(name));
                }
                catalog_name = Some(path);
                break;
            }
        }
    }

    if let Some(parent) = database_path.parent() {
        ensure_directory(parent)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(database_path)?;
    file.write_all(&bytes)?;
    sync_regular_file(&file)?;
    drop(file);
    if let Some(parent) = database_path.parent() {
        sync_directory(parent)?;
    }
    // Opening validates the schema and migrates if needed. A snapshot that
    // does not open is removed so the restore can be retried.
    let opened = Database::open(database_path).and_then(|mut database| {
        // The snapshot can predate objects already on the agents; GC, drain,
        // and retire refuse until `recover --apply` reconciles them.
        database.set_recovery_pending(now_seconds().unwrap_or(0))
    });
    if let Err(error) = opened {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = database_path.as_os_str().to_owned();
            path.push(suffix);
            let _ = fs::remove_file(PathBuf::from(path));
        }
        return Err(error.into());
    }
    Ok(RestoreMetadataReport {
        report_type: "RestoreMetadataReportV1".to_owned(),
        snapshot: SnapshotRecord {
            name,
            hash,
            size: bytes.len() as u64,
        },
        database: database_path.to_owned(),
        catalog: catalog_name,
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RestoreObjectsReport {
    pub report_type: String,
    pub objects: usize,
    pub placed: usize,
    pub already_present: usize,
    pub failed: usize,
    pub issues: Vec<String>,
}

impl RestoreObjectsReport {
    pub fn exit_code(&self) -> u8 {
        if self.failed == 0 {
            0
        } else {
            2
        }
    }
}

/// Copy every object in the backup set into the agents until each has copies
/// in `MINIMUM_REPLICAS` distinct failure domains, verifying each copy by
/// reading it back. Needs no master key and no database; `recover --apply`
/// then rebuilds metadata from the restored objects.
pub async fn restore_objects(
    spec: &BackupTargetSpec,
    agents: &[AgentTarget],
    client: &AgentClient,
) -> Result<RestoreObjectsReport, BackupError> {
    let target = spec.open().await?;
    let mut report = RestoreObjectsReport {
        report_type: "RestoreObjectsReportV1".to_owned(),
        ..RestoreObjectsReport::default()
    };
    for (path, _) in target.list(&format!("{LAYOUT_ROOT}/objects")).await? {
        let mut parts = path.rsplit('/');
        let (Some(hash_text), Some(kind_text)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(kind), Ok(hash)) = (
            kind_text.parse::<ObjectKind>(),
            ObjectHash::parse(hash_text.to_owned()),
        ) else {
            report.failed += 1;
            report
                .issues
                .push(format!("unrecognized backup path {path}"));
            continue;
        };
        report.objects += 1;
        let Some(bytes) = target.get(&path).await? else {
            report.failed += 1;
            report
                .issues
                .push(format!("{path} vanished during restore"));
            continue;
        };
        if ObjectHash::digest(&bytes) != hash {
            report.failed += 1;
            report
                .issues
                .push(format!("{path} does not match its content address"));
            continue;
        }
        let mut domains: BTreeSet<String> = BTreeSet::new();
        for agent in agents {
            if domains.len() >= MINIMUM_REPLICAS {
                break;
            }
            if domains.contains(&agent.failure_domain) {
                continue;
            }
            let present = match client.get(agent, kind, &hash).await {
                Ok(response) if response.status().is_success() => response
                    .bytes()
                    .await
                    .is_ok_and(|got| ObjectHash::digest(&got) == hash),
                _ => false,
            };
            if present {
                report.already_present += 1;
                domains.insert(agent.failure_domain.clone());
                continue;
            }
            let accepted = matches!(
                client
                    .put(agent, kind, &hash, Bytes::copy_from_slice(&bytes))
                    .await,
                Ok(response) if response.status().is_success()
            );
            let read_back = accepted
                && match client.get(agent, kind, &hash).await {
                    Ok(response) if response.status().is_success() => response
                        .bytes()
                        .await
                        .is_ok_and(|got| ObjectHash::digest(&got) == hash),
                    _ => false,
                };
            if read_back {
                report.placed += 1;
                domains.insert(agent.failure_domain.clone());
            } else {
                report
                    .issues
                    .push(format!("agent {} did not accept {path} intact", agent.id));
            }
        }
        if domains.len() < MINIMUM_REPLICAS {
            report.failed += 1;
            report.issues.push(format!(
                "{path} reached only {} of {MINIMUM_REPLICAS} failure domains",
                domains.len()
            ));
        }
    }
    Ok(report)
}

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("backup target must be dir:<path> or s3:<bucket>[/<prefix>]: {0}")]
    InvalidTarget(String),
    #[error("restore drill report is invalid or incomplete: {0}")]
    InvalidDrillReport(String),
    #[error(
        "S3 bucket {0} does not have versioning enabled; enable versioning (and Object Lock) \
         before using it as a backup target"
    )]
    TargetUnversioned(String),
    #[error("S3 credentials missing: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY")]
    S3Credentials,
    #[error("S3 {0}")]
    S3(String),
    #[error("invalid backup path: {0}")]
    InvalidPath(String),
    #[error("backup target already holds different bytes at {0}; refusing to overwrite")]
    Conflict(String),
    #[error("snapshot {0} did not verify against its recorded hash")]
    SnapshotVerification(String),
    #[error("no SQLite snapshot found in the backup target")]
    NoSnapshot,
    #[error("restore destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn directory_target_never_overwrites_and_lists_sorted() {
        let directory = tempfile::tempdir().unwrap();
        let target = BackupTargetKind::Directory(DirectoryTarget::open(directory.path()).unwrap());
        assert_eq!(target.id(), format!("dir:{}", directory.path().display()));
        assert_eq!(
            target
                .put_new("backup/v1/objects/chunk/aa", b"one")
                .await
                .unwrap(),
            PutOutcome::Created
        );
        assert_eq!(
            target
                .put_new("backup/v1/objects/chunk/aa", b"one")
                .await
                .unwrap(),
            PutOutcome::Identical
        );
        assert!(matches!(
            target.put_new("backup/v1/objects/chunk/aa", b"two").await,
            Err(BackupError::Conflict(_))
        ));
        target
            .put_new("backup/v1/objects/chunk/ab", b"three")
            .await
            .unwrap();
        target
            .put_new("backup/v1/sqlite/x.db", b"db")
            .await
            .unwrap();
        assert_eq!(
            target.list("backup/v1/objects").await.unwrap(),
            vec![
                ("backup/v1/objects/chunk/aa".to_owned(), 3),
                ("backup/v1/objects/chunk/ab".to_owned(), 5),
            ]
        );
        assert_eq!(
            target.get("backup/v1/sqlite/x.db").await.unwrap().unwrap(),
            b"db"
        );
        assert!(target.get("backup/v1/missing").await.unwrap().is_none());
        assert!(matches!(
            target.put_new("../escape", b"x").await,
            Err(BackupError::InvalidPath(_))
        ));
        assert!(matches!(
            target.put_new("/abs", b"x").await,
            Err(BackupError::InvalidPath(_))
        ));
        assert!(!directory
            .path()
            .join("backup/v1/objects/chunk")
            .read_dir()
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
    }

    #[test]
    fn utc_stamps_are_civil_dates() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_790_704_638), "20260929T175718Z");
        for seconds in [0, 951_782_400, 1_790_704_638, 4_102_444_799] {
            assert_eq!(parse_utc_stamp(&utc_stamp(seconds)), Some(seconds));
        }
        assert_eq!(parse_utc_stamp("20261325T000000Z"), None);
        assert_eq!(parse_utc_stamp("not a stamp"), None);
    }

    #[test]
    fn drills_cover_same_bucket_or_same_directory() {
        assert!(drill_covers("s3:offsite/drill-1", "s3:offsite/prod"));
        assert!(drill_covers("s3:offsite", "s3:offsite/prod"));
        assert!(!drill_covers("s3:other/prod", "s3:offsite/prod"));
        assert!(drill_covers("dir:/mnt/backup", "dir:/mnt/backup"));
        assert!(!drill_covers("dir:/tmp/drill/offsite", "dir:/mnt/backup"));
        assert!(!drill_covers("dir:/mnt/backup", "s3:mnt"));
    }

    #[test]
    fn target_specs_parse() {
        assert_eq!(
            "dir:/mnt/backup".parse::<BackupTargetSpec>().unwrap(),
            BackupTargetSpec::Directory(PathBuf::from("/mnt/backup"))
        );
        let bare = "s3:bucket".parse::<BackupTargetSpec>().unwrap();
        assert_eq!(
            bare,
            BackupTargetSpec::S3 {
                bucket: "bucket".to_owned(),
                prefix: String::new(),
            }
        );
        assert_eq!(bare.id(), "s3:bucket");
        assert_eq!("s3:bucket/".parse::<BackupTargetSpec>().unwrap(), bare);
        let nested = "s3:bucket/site/a/".parse::<BackupTargetSpec>().unwrap();
        assert_eq!(
            nested,
            BackupTargetSpec::S3 {
                bucket: "bucket".to_owned(),
                prefix: "site/a".to_owned(),
            }
        );
        assert_eq!(nested.id(), "s3:bucket/site/a");
        for invalid in [
            "dir:",
            "s3:",
            "s3:/x",
            "s3:bucket/a//b",
            "s3:bucket/../x",
            "ftp:x",
        ] {
            assert!(
                invalid.parse::<BackupTargetSpec>().is_err(),
                "{invalid} should be rejected"
            );
        }
    }
}

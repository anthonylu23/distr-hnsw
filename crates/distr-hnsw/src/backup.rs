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
//! The backup job needs no master key: it copies ciphertext.

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

use crate::{
    durability::{ensure_directory, sync_directory, sync_regular_file},
    metadata::{Database, HistoryObject, JobStatus, MetadataError, ReconcileJob},
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
};

const LAYOUT_ROOT: &str = "backup/v1";
const MINIMUM_REPLICAS: usize = 2;

/// Operator-facing target specification, e.g. `dir:/mnt/backup`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupTargetSpec {
    Directory(PathBuf),
}

impl BackupTargetSpec {
    pub fn id(&self) -> String {
        match self {
            Self::Directory(path) => format!("dir:{}", path.display()),
        }
    }

    pub fn open(&self) -> Result<Box<dyn BackupTarget>, BackupError> {
        match self {
            Self::Directory(path) => Ok(Box::new(DirectoryTarget::open(path)?)),
        }
    }
}

impl FromStr for BackupTargetSpec {
    type Err = BackupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.split_once(':') {
            Some(("dir", path)) if !path.is_empty() => Ok(Self::Directory(PathBuf::from(path))),
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

/// A versioned object-store-like backup destination. Implementations must
/// never overwrite: an existing path with different bytes is a conflict.
pub trait BackupTarget {
    fn id(&self) -> String;
    fn put_new(&self, path: &str, bytes: &[u8]) -> Result<PutOutcome, BackupError>;
    fn get(&self, path: &str) -> Result<Option<Vec<u8>>, BackupError>;
    /// Paths under `prefix`, sorted, with sizes.
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, BackupError>;
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
        if path.is_empty()
            || path.starts_with('/')
            || path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(BackupError::InvalidPath(path.to_owned()));
        }
        Ok(self.root.join(path))
    }
}

impl BackupTarget for DirectoryTarget {
    fn id(&self) -> String {
        format!("dir:{}", self.root.display())
    }

    fn put_new(&self, path: &str, bytes: &[u8]) -> Result<PutOutcome, BackupError> {
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

    fn get(&self, path: &str) -> Result<Option<Vec<u8>>, BackupError> {
        match fs::read(self.resolve(path)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(BackupError::Io(error)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, BackupError> {
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
    pub issues: Vec<String>,
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
    client: &reqwest::Client,
    spec: &BackupTargetSpec,
    snapshot_path_hint: &Path,
) -> Result<BackupReportV1, BackupError> {
    let target = spec.open()?;
    let target_id = spec.id();
    let job_id = database.create_backup_job(&target_id)?;
    let result = run_backup(
        database,
        agents,
        client,
        target.as_ref(),
        &target_id,
        job_id,
        snapshot_path_hint,
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

async fn run_backup(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    target: &dyn BackupTarget,
    target_id: &str,
    job_id: Uuid,
    snapshot_path_hint: &Path,
) -> Result<BackupReportV1, BackupError> {
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
        match target.put_new(&path, &bytes) {
            Ok(PutOutcome::Created) => totals.copied_now += 1,
            Ok(PutOutcome::Identical) => totals.identical_on_target += 1,
            Err(error) => {
                totals.failed += 1;
                issues.push(format!("target refused {path}: {error}"));
                continue;
            }
        }
        let verified = target
            .get(&path)?
            .is_some_and(|got| ObjectHash::digest(&got) == object.hash);
        if !verified {
            totals.failed += 1;
            issues.push(format!("target copy of {path} did not read back intact"));
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
    // last shipped one (backup bookkeeping alone does not warrant a snapshot).
    let now = now_seconds()?;
    let generation = database.content_generation()?;
    let latest = database.latest_backup_snapshot(target_id)?;
    let (snapshot, skipped) = match latest {
        Some(previous) if previous.content_generation == generation => (
            Some(SnapshotRecord {
                name: previous.name,
                hash: previous.snapshot_hash,
                size: previous.size,
            }),
            true,
        ),
        _ => {
            let temporary = snapshot_path_hint.with_extension(format!("snapshot-{job_id}.tmp"));
            let _ = fs::remove_file(&temporary);
            database.snapshot_into(&temporary)?;
            let snapshot_bytes = fs::read(&temporary)?;
            let _ = fs::remove_file(&temporary);
            let snapshot_hash = blake3::hash(&snapshot_bytes).to_hex().to_string();
            let name = format!("{}-{}.db", utc_stamp(now), &snapshot_hash[..8]);
            target.put_new(&snapshot_path(&name), &snapshot_bytes)?;
            let verified = target
                .get(&snapshot_path(&name))?
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
    let catalog_name = format!("{}-{job_id}.json", utc_stamp(now));
    target.put_new(
        &catalog_path(&catalog_name),
        serde_json::to_string_pretty(&catalog)?.as_bytes(),
    )?;

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
        issues,
    })
}

async fn fetch_from_agents(
    database: &Database,
    client: &reqwest::Client,
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
        let url = format!(
            "{}/v1/objects/{}/{}",
            agent.base_url,
            object.kind.as_str(),
            object.hash
        );
        if let Ok(response) = client.get(url).send().await {
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupStatusV1 {
    pub targets: Vec<BackupTargetStatus>,
    /// Always false until the empty-infrastructure restore drill has passed
    /// for this deployment (DESIGN §11.1).
    pub recovery_ready: bool,
    pub recovery_ready_reason: String,
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
        targets.push(BackupTargetStatus {
            last_job: database.latest_backup_job(&target_id)?,
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
    Ok(BackupStatusV1 {
        targets,
        recovery_ready: false,
        recovery_ready_reason:
            "empty-infrastructure restore drill has not passed for this deployment".to_owned(),
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
pub fn restore_metadata(
    spec: &BackupTargetSpec,
    database_path: &Path,
    snapshot_name: Option<&str>,
) -> Result<RestoreMetadataReport, BackupError> {
    if database_path.exists() {
        return Err(BackupError::DestinationExists(database_path.to_owned()));
    }
    let target = spec.open()?;
    let snapshots = target.list(&format!("{LAYOUT_ROOT}/sqlite"))?;
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
        .get(&chosen.0)?
        .ok_or_else(|| BackupError::NoSnapshot)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();

    let mut catalog_name = None;
    for (path, _) in target
        .list(&format!("{LAYOUT_ROOT}/catalog"))?
        .into_iter()
        .rev()
    {
        let Some(raw) = target.get(&path)? else {
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
    // Opening validates the schema and migrates if needed.
    Database::open(database_path)?;
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
    client: &reqwest::Client,
) -> Result<RestoreObjectsReport, BackupError> {
    let target = spec.open()?;
    let mut report = RestoreObjectsReport {
        report_type: "RestoreObjectsReportV1".to_owned(),
        ..RestoreObjectsReport::default()
    };
    for (path, _) in target.list(&format!("{LAYOUT_ROOT}/objects"))? {
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
        let Some(bytes) = target.get(&path)? else {
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
            let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
            let present = match client.get(&url).send().await {
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
                client.put(&url).body(bytes.clone()).send().await,
                Ok(response) if response.status().is_success()
            );
            let read_back = accepted
                && match client.get(&url).send().await {
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
    #[error("backup target must be dir:<path>: {0}")]
    InvalidTarget(String),
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

    #[test]
    fn directory_target_never_overwrites_and_lists_sorted() {
        let directory = tempfile::tempdir().unwrap();
        let target = DirectoryTarget::open(directory.path()).unwrap();
        assert_eq!(
            target
                .put_new("backup/v1/objects/chunk/aa", b"one")
                .unwrap(),
            PutOutcome::Created
        );
        assert_eq!(
            target
                .put_new("backup/v1/objects/chunk/aa", b"one")
                .unwrap(),
            PutOutcome::Identical
        );
        assert!(matches!(
            target.put_new("backup/v1/objects/chunk/aa", b"two"),
            Err(BackupError::Conflict(_))
        ));
        target
            .put_new("backup/v1/objects/chunk/ab", b"three")
            .unwrap();
        target.put_new("backup/v1/sqlite/x.db", b"db").unwrap();
        assert_eq!(
            target.list("backup/v1/objects").unwrap(),
            vec![
                ("backup/v1/objects/chunk/aa".to_owned(), 3),
                ("backup/v1/objects/chunk/ab".to_owned(), 5),
            ]
        );
        assert_eq!(target.get("backup/v1/sqlite/x.db").unwrap().unwrap(), b"db");
        assert!(target.get("backup/v1/missing").unwrap().is_none());
        assert!(matches!(
            target.put_new("../escape", b"x"),
            Err(BackupError::InvalidPath(_))
        ));
        assert!(matches!(
            target.put_new("/abs", b"x"),
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
    }

    #[test]
    fn target_specs_parse() {
        assert_eq!(
            "dir:/mnt/backup".parse::<BackupTargetSpec>().unwrap(),
            BackupTargetSpec::Directory(PathBuf::from("/mnt/backup"))
        );
        assert!("s3:bucket".parse::<BackupTargetSpec>().is_err());
        assert!("dir:".parse::<BackupTargetSpec>().is_err());
    }
}

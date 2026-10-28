//! Regression tests for the defects found in the 2026-10-25 audit that
//! reopened M1 (`docs/hardening-2026-10.md`). Each test reproduces the
//! original failure sequence and asserts the corrected behavior.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use distr_hnsw::{
    access::create_join_token,
    agent::{serve_agent_with_capacity, AgentIdentity, CapacityConfig},
    agent_client::{join_agent, AgentClient},
    backup::{self, BackupTargetSpec},
    crypto::MasterKey,
    durability::DurableStore,
    format::encode_deletion_marker,
    lifecycle::{gc, retire, GcOptions, LifecycleError},
    metadata::{Database, FileState, JobMode, JobStatus},
    object::{ObjectHash, ObjectKind},
    portal::{AgentTarget, Failpoint, FailpointAction, Portal},
    reconcile::scrub,
    tailscale::LocalApi,
    CHUNK_SIZE,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

struct TestAgent {
    target: AgentTarget,
    volume: PathBuf,
    _holder: TempDir,
    task: JoinHandle<()>,
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Cluster {
    workspace: TempDir,
    key_path: PathBuf,
    database_path: PathBuf,
    agents: Vec<TestAgent>,
}

impl Cluster {
    async fn new(count: usize) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let key_path = workspace.path().join("master.key");
        MasterKey::create(&key_path).unwrap();
        let mut cluster = Self {
            database_path: workspace.path().join("portal.sqlite"),
            key_path,
            workspace,
            agents: Vec::new(),
        };
        for (id, domain) in [
            ("agent-a", "host-a"),
            ("agent-b", "host-b"),
            ("agent-c", "host-c"),
        ]
        .into_iter()
        .take(count)
        {
            cluster.start_agent(id, domain).await;
        }
        cluster
    }

    /// Start an agent on a fresh volume with a fresh join token and join it.
    async fn start_agent(&mut self, id: &str, domain: &str) {
        let holder = tempfile::tempdir().unwrap();
        let volume = holder.path().to_owned();
        let key = self.key();
        let mut database = self.database();
        database.bind_master_key_id(&key.key_id_hex()).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let token = create_join_token(&mut database, id, now).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let identity = AgentIdentity {
            id: id.to_owned(),
            failure_domain: domain.to_owned(),
        };
        let (volume_path, agent_token) = (volume.clone(), token.clone());
        let task = tokio::spawn(async move {
            serve_agent_with_capacity(
                listener,
                volume_path,
                identity,
                CapacityConfig::default(),
                &agent_token,
            )
            .await
            .unwrap();
        });
        let agent = TestAgent {
            target: AgentTarget {
                id: id.to_owned(),
                failure_domain: domain.to_owned(),
                base_url: format!("http://{address}"),
            },
            volume,
            _holder: holder,
            task,
        };
        join_agent(
            &mut database,
            &key,
            &agent.target,
            &token,
            &LocalApi::new("/nonexistent/tailscaled.sock"),
        )
        .await
        .unwrap();
        self.agents.push(agent);
    }

    fn targets(&self) -> Vec<AgentTarget> {
        self.agents
            .iter()
            .map(|agent| agent.target.clone())
            .collect()
    }

    fn key(&self) -> MasterKey {
        MasterKey::load(&self.key_path).unwrap()
    }

    fn portal(&self) -> Portal {
        Portal::open(&self.database_path, self.key(), self.targets()).unwrap()
    }

    fn database(&self) -> Database {
        Database::open(&self.database_path).unwrap()
    }

    fn client(&self) -> AgentClient {
        AgentClient::open(&mut self.database(), &self.key()).unwrap()
    }

    fn source(&self, name: &str, size: usize) -> PathBuf {
        let path = self.workspace.path().join(name);
        let bytes: Vec<u8> = (0..size)
            .map(|index| ((index * 13 + 5) % 251) as u8)
            .collect();
        fs::write(&path, bytes).unwrap();
        path
    }

    async fn scrub(&self) {
        let report = scrub(
            &mut self.database(),
            &self.targets(),
            &self.client(),
            JobMode::Verify,
        )
        .await
        .unwrap();
        assert!(report.complete_observation, "{report:?}");
    }

    async fn gc_apply(&self) -> distr_hnsw::lifecycle::GcReportV1 {
        gc(
            &mut self.database(),
            &self.targets(),
            &self.client(),
            GcOptions {
                retention_seconds: 0,
                staging_grace_seconds: 0,
                apply: true,
            },
        )
        .await
        .unwrap()
    }

    fn marker_objects(&self) -> usize {
        self.agents
            .iter()
            .map(|agent| {
                DurableStore::open(&agent.volume)
                    .unwrap()
                    .inventory(ObjectKind::DeletionMarker, None, 1000)
                    .unwrap()
                    .objects
                    .len()
            })
            .sum()
    }
}

/// A recovery that applies an in-flight upload must leave a same-key retry
/// returning that file, not colliding with it on `files.file_id`.
#[tokio::test]
async fn recovered_upload_is_committed_for_same_key_retries() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("crash.bin", 2 * CHUNK_SIZE + 17);
    let mut portal = cluster
        .portal()
        .with_failpoint(Failpoint::BeforeCommit, FailpointAction::ReturnError);
    assert!(portal.upload(&source, "crash-key").await.is_err());
    drop(portal);

    let report = cluster.portal().recover(true).await.unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");

    let file_id = cluster.portal().upload(&source, "crash-key").await.unwrap();
    let upload = cluster
        .database()
        .upload_by_idempotency("crash-key")
        .unwrap()
        .unwrap();
    assert_eq!(upload.file_id, file_id);
    assert_eq!(upload.state, distr_hnsw::metadata::UploadState::Committed);
    let out = cluster.workspace.path().join("out.bin");
    cluster.portal().download(file_id, &out).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), fs::read(source).unwrap());
}

/// A delete abandoned after its first marker replica and retried under a new
/// idempotency key reuses the pending marker; recovery then converges.
#[tokio::test]
async fn delete_retried_under_a_new_key_reuses_the_pending_marker() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("doomed.bin", 64);
    let file_id = cluster.portal().upload(&source, "up").await.unwrap();

    let mut first = cluster.portal().with_failpoint(
        Failpoint::AfterFirstMarkerReplica,
        FailpointAction::ReturnError,
    );
    assert!(first.delete(file_id, "del-1").await.is_err());
    drop(first);

    let second = cluster.portal().delete(file_id, "del-2").await.unwrap();
    assert_eq!(second.generation, 2);
    // One marker, one copy per agent.
    assert_eq!(cluster.marker_objects(), 2);
    // The abandoned key now completes onto the same marker.
    let first_retry = cluster.portal().delete(file_id, "del-1").await.unwrap();
    assert_eq!(first_retry.marker_hash, second.marker_hash);

    let report = cluster.portal().recover(true).await.unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let state = cluster
        .database()
        .file_projection(file_id)
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state, FileState::Deleted);
}

/// Agents that already hold two distinct markers for one generation (as an
/// earlier release could produce) recover to a deleted file without blocking,
/// both with the original metadata and from a blank database.
#[tokio::test]
async fn redundant_deletion_markers_recover_deterministically() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("twice.bin", 64);
    let file_id = cluster.portal().upload(&source, "up").await.unwrap();
    cluster.portal().delete(file_id, "del").await.unwrap();

    // Plant a second, distinct marker for the same generation on both agents.
    let extra = encode_deletion_marker(&cluster.key(), file_id, 2, 1).unwrap();
    let extra_hash = ObjectHash::digest(&extra);
    for agent in &cluster.agents {
        DurableStore::open(&agent.volume)
            .unwrap()
            .put(ObjectKind::DeletionMarker, &extra_hash, &extra)
            .unwrap();
    }

    let report = cluster.portal().recover(true).await.unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let decision = report
        .files
        .iter()
        .find(|file| file.file_id == file_id.to_string())
        .unwrap();
    assert!(!decision.notes.is_empty(), "{decision:?}");

    // Joined agents accept grants only from the portal's signing key, which
    // is sealed in the database, so the blank database starts with it.
    let signing_keys = cluster.database().signing_keys().unwrap();
    assert_eq!(signing_keys.len(), 1);
    fs::remove_file(&cluster.database_path).unwrap();
    cluster
        .database()
        .insert_first_signing_key(&signing_keys[0])
        .unwrap();
    let blank = cluster.portal().recover(true).await.unwrap();
    assert_eq!(blank.exit_code(), 0, "{blank:?}");
    assert_eq!(
        cluster
            .database()
            .file_projection(file_id)
            .unwrap()
            .unwrap()
            .state,
        FileState::Deleted
    );
}

/// A same-key retry heals a replica corrupted after it was confirmed.
#[tokio::test]
async fn upload_retry_heals_a_corrupt_confirmed_replica() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("inflight.bin", 4096);
    let mut portal = cluster
        .portal()
        .with_failpoint(Failpoint::AfterChunksDurable, FailpointAction::ReturnError);
    assert!(portal.upload(&source, "k").await.is_err());
    drop(portal);

    let database = cluster.database();
    let upload = database.upload_by_idempotency("k").unwrap().unwrap();
    let chunk = database.chunks(upload.upload_id).unwrap()[0]
        .ciphertext_hash
        .clone()
        .unwrap();
    drop(database);
    let store = DurableStore::open(&cluster.agents[0].volume).unwrap();
    fs::write(store.object_path(ObjectKind::Chunk, &chunk), b"bitrot").unwrap();

    cluster.portal().upload(&source, "k").await.unwrap();
    assert!(store.get(ObjectKind::Chunk, &chunk).is_ok());
}

async fn deleted_file_cluster() -> Cluster {
    let cluster = Cluster::new(3).await;
    let source = cluster.source("doomed.bin", CHUNK_SIZE + 64);
    let file_id = cluster.portal().upload(&source, "gc-doomed").await.unwrap();
    cluster.portal().delete(file_id, "gc-delete").await.unwrap();
    cluster
}

/// Objects GC physically deleted no longer count against retirement.
#[tokio::test]
async fn retirement_ignores_collected_objects() {
    let cluster = deleted_file_cluster().await;
    cluster.scrub().await;
    let applied = cluster.gc_apply().await;
    assert_eq!(applied.totals.applied, 3, "{applied:?}");

    let report = retire(
        &mut cluster.database(),
        &cluster.targets(),
        &cluster.client(),
        "agent-c",
    )
    .await
    .unwrap();
    assert!(report.retired_incarnation.is_some(), "{report:?}");
}

/// A backup target added after GC succeeds instead of failing forever on
/// objects that no longer exist.
#[tokio::test]
async fn backup_after_gc_skips_collected_objects() {
    let cluster = deleted_file_cluster().await;
    cluster.scrub().await;
    assert_eq!(cluster.gc_apply().await.totals.applied, 3);

    let spec = BackupTargetSpec::Directory(cluster.workspace.path().join("offsite"));
    let mut database = cluster.database();
    for _ in 0..2 {
        let report = backup::backup(
            &mut database,
            &cluster.targets(),
            &cluster.client(),
            &spec,
            &cluster.database_path,
        )
        .await
        .unwrap();
        assert_eq!(report.exit_code(), 0, "{report:?}");
        assert_eq!(report.totals.failed, 0);
    }
    let status = backup::backup_status(&database).unwrap();
    assert_eq!(status.targets[0].objects_pending, 0);
}

/// Once a backup target exists, a deleted generation is collected only after
/// that target has verified it.
#[tokio::test]
async fn gc_waits_for_backup_coverage_of_deleted_generations() {
    let cluster = Cluster::new(3).await;
    let spec = BackupTargetSpec::Directory(cluster.workspace.path().join("offsite"));
    let client = cluster.client();
    let run_backup = || async {
        backup::backup(
            &mut cluster.database(),
            &cluster.targets(),
            &client,
            &spec,
            &cluster.database_path,
        )
        .await
        .unwrap()
    };
    let keep = cluster.source("keep.bin", 64);
    cluster.portal().upload(&keep, "keep").await.unwrap();
    assert_eq!(run_backup().await.exit_code(), 0);

    let doomed = cluster.source("doomed.bin", 64);
    let file_id = cluster.portal().upload(&doomed, "doomed").await.unwrap();
    cluster
        .portal()
        .delete(file_id, "doomed-delete")
        .await
        .unwrap();
    cluster.scrub().await;
    let blocked = cluster.gc_apply().await;
    assert_eq!(blocked.totals.applied, 0, "{blocked:?}");
    assert!(blocked.candidates.iter().all(|candidate| candidate
        .blockers
        .iter()
        .any(|blocker| blocker.contains("backup target"))));

    assert_eq!(run_backup().await.exit_code(), 0);
    cluster.scrub().await;
    let applied = cluster.gc_apply().await;
    assert_eq!(applied.totals.applied, 2, "{applied:?}");
}

/// Restoring a snapshot taken mid-upload, after that upload was acknowledged,
/// must not let GC collect the acknowledged file's objects.
#[tokio::test]
async fn gc_after_a_stale_metadata_restore_keeps_acknowledged_files() {
    let cluster = Cluster::new(3).await;
    let source = cluster.source("late.bin", 8192);
    let mut portal = cluster
        .portal()
        .with_failpoint(Failpoint::AfterChunksDurable, FailpointAction::ReturnError);
    assert!(portal.upload(&source, "late-key").await.is_err());
    drop(portal);
    let snapshot = cluster.workspace.path().join("snap.sqlite");
    cluster.database().snapshot_into(&snapshot).unwrap();
    let file_id = cluster.portal().upload(&source, "late-key").await.unwrap();

    // Portal loss: the database is replaced by the stale snapshot.
    for suffix in ["", "-wal", "-shm"] {
        let mut path = cluster.database_path.as_os_str().to_owned();
        path.push(suffix);
        let _ = fs::remove_file(PathBuf::from(path));
    }
    fs::copy(&snapshot, &cluster.database_path).unwrap();

    cluster.scrub().await;
    let report = cluster.gc_apply().await;
    assert_eq!(report.totals.applied, 0, "{report:?}");
    assert!(report.candidates.iter().all(|candidate| candidate
        .blockers
        .iter()
        .any(|blocker| blocker.contains("unknown to the metadata"))));

    let mut portal = cluster.portal();
    assert_eq!(portal.recover(true).await.unwrap().exit_code(), 0);
    let out = cluster.workspace.path().join("out.bin");
    portal.download(file_id, &out).await.unwrap();
    assert_eq!(fs::read(out).unwrap(), fs::read(&source).unwrap());
    // Recovery adopted the upload, so GC has no staging candidate left.
    drop(portal);
    assert_eq!(cluster.gc_apply().await.totals.candidates, 0);
}

/// `restore_metadata` marks the database recovery-pending; GC and retire
/// refuse until `recover --apply` clears it.
#[tokio::test]
async fn restored_metadata_blocks_lifecycle_until_recovered() {
    let cluster = Cluster::new(3).await;
    let source = cluster.source("kept.bin", 64);
    cluster.portal().upload(&source, "kept").await.unwrap();
    let spec = BackupTargetSpec::Directory(cluster.workspace.path().join("offsite"));
    backup::backup(
        &mut cluster.database(),
        &cluster.targets(),
        &cluster.client(),
        &spec,
        &cluster.database_path,
    )
    .await
    .unwrap();

    let restored = cluster.workspace.path().join("restored.sqlite");
    backup::restore_metadata(&spec, &restored, None)
        .await
        .unwrap();
    let mut database = Database::open(&restored).unwrap();
    assert!(database.recovery_pending().unwrap().is_some());
    let refused = gc(
        &mut database,
        &cluster.targets(),
        &cluster.client(),
        GcOptions {
            retention_seconds: 0,
            staging_grace_seconds: 0,
            apply: true,
        },
    )
    .await;
    assert!(matches!(refused, Err(LifecycleError::RecoveryPending(_))));
    let refused = retire(
        &mut database,
        &cluster.targets(),
        &cluster.client(),
        "agent-c",
    )
    .await;
    assert!(matches!(refused, Err(LifecycleError::RecoveryPending(_))));
    drop(database);

    let mut portal = Portal::open(&restored, cluster.key(), cluster.targets()).unwrap();
    assert_eq!(portal.recover(true).await.unwrap().exit_code(), 0);
    drop(portal);
    assert!(Database::open(&restored)
        .unwrap()
        .recovery_pending()
        .unwrap()
        .is_none());
}

/// Opening the database from a second handle (another process, such as
/// `portal health` beside `scrub --interval`) leaves a live job running.
#[tokio::test]
async fn opening_the_database_does_not_interrupt_a_live_job() {
    let cluster = Cluster::new(2).await;
    let mut owner = cluster.database();
    let job = owner.create_reconcile_job(JobMode::Verify).unwrap();
    let backup_job = owner.create_backup_job("dir:/elsewhere").unwrap();
    let _other = cluster.database();
    owner
        .finish_reconcile_job(job, JobStatus::Complete, Some("{}"))
        .unwrap();
    owner
        .finish_backup_job(backup_job, JobStatus::Complete, Some("{}"))
        .unwrap();
}

/// A backup whose target refuses writes reports lag rather than zero.
#[tokio::test]
async fn backup_lag_is_reported_when_the_target_refuses_writes() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("lag.bin", 2048);
    cluster.portal().upload(&source, "lag-key").await.unwrap();
    let target_dir = cluster.workspace.path().join("offsite");
    let mut database = cluster.database();
    // Conflicting bytes at every object path make the target refuse.
    for object in database.history_objects().unwrap() {
        let path = target_dir.join(backup::object_path(object.kind, &object.hash));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"conflict").unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let report = backup::backup(
        &mut database,
        &cluster.targets(),
        &cluster.client(),
        &BackupTargetSpec::Directory(target_dir),
        &cluster.database_path,
    )
    .await
    .unwrap();
    assert!(report.totals.failed > 0);
    assert!(report.backup_lag_seconds > 0, "{report:?}");
    // The snapshot would reference objects the set lacks, so it is withheld.
    assert!(report.snapshot.is_none(), "{report:?}");
}

fn drill_report(target: &str, verdict: &str, stamp: &str, within: bool) -> String {
    serde_json::json!({
        "report_type": "RestoreDrillReportV1",
        "version": 1,
        "stamp": stamp,
        "source_revision": "abc1234",
        "target": target,
        "verdict": verdict,
        "objectives": {
            "declared_metadata_rpo_seconds": 300,
            "declared_blob_rpo_seconds": 900,
            "declared_portal_loss_rto_seconds": 600,
            "actual_recovery_point_lag_seconds": 1.5,
            "actual_recovery_time_seconds": 12.25,
            "within_targets": within
        }
    })
    .to_string()
}

fn utc_now_stamp() -> String {
    // Seconds-precision UTC stamp without a calendar crate.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (days, secs) = (now.div_euclid(86_400), now.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `recovery_ready` turns true only for a recent passing drill against a
/// configured target whose lag is within the declared RPO.
#[tokio::test]
async fn recovery_ready_follows_the_recorded_restore_drill() {
    let cluster = Cluster::new(2).await;
    let source = cluster.source("ready.bin", 64);
    cluster.portal().upload(&source, "ready").await.unwrap();
    let spec = BackupTargetSpec::Directory(cluster.workspace.path().join("offsite"));
    let mut database = cluster.database();
    backup::backup(
        &mut database,
        &cluster.targets(),
        &cluster.client(),
        &spec,
        &cluster.database_path,
    )
    .await
    .unwrap();
    let status = backup::backup_status(&database).unwrap();
    assert!(!status.recovery_ready);
    assert!(status.last_restore_drill.is_none());

    let now = utc_now_stamp();
    let cases = [
        (spec.id(), "fail", now.clone(), true, false),
        (spec.id(), "pass", now.clone(), false, false),
        (
            "dir:/elsewhere".to_owned(),
            "pass",
            now.clone(),
            true,
            false,
        ),
        (
            spec.id(),
            "pass",
            "20200101T000000Z".to_owned(),
            true,
            false,
        ),
        (spec.id(), "pass", now.clone(), true, true),
    ];
    for (target, verdict, stamp, within, ready) in cases {
        backup::record_restore_drill(
            &mut database,
            &drill_report(&target, verdict, &stamp, within),
        )
        .unwrap();
        let status = backup::backup_status(&database).unwrap();
        assert_eq!(
            status.recovery_ready, ready,
            "{target} {verdict} {stamp} {within}: {}",
            status.recovery_ready_reason
        );
    }
    assert!(backup::record_restore_drill(&mut database, "{\"report_type\":\"Other\"}").is_err());
}

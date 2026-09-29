//! Lifecycle pass: incarnations, complete-scan observations, scrub health,
//! and copy-first repair. Nothing here ever deletes an object, and every
//! conclusion about absence requires a complete scan of an active incarnation.

use std::{collections::BTreeSet, fs, path::PathBuf};

use distr_hnsw::{
    agent::{serve_agent, AgentIdentity},
    crypto::MasterKey,
    durability::DurableStore,
    metadata::{
        Database, IncarnationObservation, IncarnationStatus, JobMode, JobStatus, MetadataError,
        ObjectHealthState, PlacementState, RequiredObject,
    },
    object::ObjectKind,
    portal::{AgentTarget, Portal},
    reconcile::{health_report, scrub, ScanSummary, ScrubError, ScrubReportV1},
    CHUNK_SIZE,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use walkdir_free::snapshot;

/// Minimal recursive listing so tests can prove a volume was not mutated.
mod walkdir_free {
    use std::{collections::BTreeSet, fs, path::Path};

    pub fn snapshot(root: &Path) -> BTreeSet<(String, u64)> {
        let mut out = BTreeSet::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path.strip_prefix(root).unwrap().display().to_string();
                    out.insert((relative, entry.metadata().unwrap().len()));
                }
            }
        }
        out
    }
}

struct TestAgent {
    target: AgentTarget,
    volume: PathBuf,
    _holder: Option<TempDir>,
    task: JoinHandle<()>,
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_agent_on(
    id: &str,
    domain: &str,
    volume: PathBuf,
    holder: Option<TempDir>,
) -> TestAgent {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: domain.to_owned(),
    };
    let volume_path = volume.clone();
    let task = tokio::spawn(async move {
        serve_agent(listener, volume_path, identity).await.unwrap();
    });
    TestAgent {
        target: AgentTarget {
            id: id.to_owned(),
            failure_domain: domain.to_owned(),
            base_url: format!("http://{address}"),
        },
        volume,
        _holder: holder,
        task,
    }
}

async fn start_agent(id: &str, domain: &str) -> TestAgent {
    let holder = tempfile::tempdir().unwrap();
    let volume = holder.path().to_owned();
    start_agent_on(id, domain, volume, Some(holder)).await
}

struct Cluster {
    workspace: TempDir,
    key_path: PathBuf,
    database_path: PathBuf,
    agents: Vec<TestAgent>,
}

impl Cluster {
    fn targets(&self) -> Vec<AgentTarget> {
        self.agents
            .iter()
            .map(|agent| agent.target.clone())
            .collect()
    }

    fn portal(&self) -> Portal {
        let key = MasterKey::load(&self.key_path).unwrap();
        Portal::open(&self.database_path, key, self.targets()).unwrap()
    }

    fn database(&self) -> Database {
        Database::open(&self.database_path).unwrap()
    }

    async fn scrub(&self, mode: JobMode) -> ScrubReportV1 {
        let mut database = self.database();
        scrub(
            &mut database,
            &self.targets(),
            &reqwest::Client::new(),
            mode,
        )
        .await
        .unwrap()
    }

    fn object_path(&self, agent_index: usize, object: &RequiredObject) -> PathBuf {
        DurableStore::open(&self.agents[agent_index].volume)
            .unwrap()
            .object_path(object.kind, &object.hash)
    }
}

async fn committed_cluster() -> (Cluster, uuid::Uuid) {
    let agent_a = start_agent("agent-a", "host-a").await;
    let agent_b = start_agent("agent-b", "host-b").await;
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    MasterKey::create(&key_path).unwrap();
    let database_path = workspace.path().join("portal.sqlite");
    let cluster = Cluster {
        workspace,
        key_path,
        database_path,
        agents: vec![agent_a, agent_b],
    };
    let source = cluster.workspace.path().join("source.bin");
    let bytes: Vec<u8> = (0..CHUNK_SIZE + 4096)
        .map(|index| ((index * 13 + 5) % 251) as u8)
        .collect();
    fs::write(&source, bytes).unwrap();
    let file_id = cluster
        .portal()
        .upload(&source, "lifecycle-upload")
        .await
        .unwrap();
    (cluster, file_id)
}

fn digests(scans: &[ScanSummary]) -> BTreeSet<(String, String, String)> {
    scans
        .iter()
        .map(|scan| {
            (
                scan.agent_id.clone(),
                scan.kind.to_string(),
                scan.inventory_digest.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn required(cluster: &Cluster) -> Vec<RequiredObject> {
    cluster.database().required_objects().unwrap()
}

fn placement(cluster: &Cluster, object: &RequiredObject, agent_id: &str) -> PlacementState {
    cluster
        .database()
        .placement_states(object.kind, &object.hash)
        .unwrap()
        .into_iter()
        .find(|(agent, _, _)| agent == agent_id)
        .map(|(_, state, _)| state)
        .expect("placement row exists")
}

async fn download_matches(cluster: &Cluster, file_id: uuid::Uuid, name: &str) {
    let destination = cluster.workspace.path().join(name);
    cluster
        .portal()
        .download(file_id, &destination)
        .await
        .unwrap();
    let source = fs::read(cluster.workspace.path().join("source.bin")).unwrap();
    assert_eq!(fs::read(destination).unwrap(), source);
}

#[tokio::test]
async fn healthy_cluster_is_durable_with_reproducible_observations() {
    let (cluster, _) = committed_cluster().await;
    let required = required(&cluster);
    assert_eq!(required.len(), 3, "manifest plus two chunks");

    let first = cluster.scrub(JobMode::Verify).await;
    assert_eq!(first.exit_code(), 0, "{first:?}");
    assert!(first.complete_observation);
    assert!(first.issues.is_empty());
    assert_eq!(first.health.durable, 3);
    assert_eq!(first.totals.valid_copies, 6);
    assert_eq!(first.scans.len(), 6);
    assert!(first.findings.is_empty());
    for agent in &first.agents {
        assert!(agent.reachable);
        // The upload already verified and adopted both incarnations.
        assert_eq!(agent.incarnation, Some(IncarnationObservation::Known));
    }

    let second = cluster.scrub(JobMode::Verify).await;
    assert_eq!(second.exit_code(), 0);
    assert_eq!(digests(&first.scans), digests(&second.scans));
    for agent in &second.agents {
        assert_eq!(agent.incarnation, Some(IncarnationObservation::Known));
    }

    let health = health_report(&cluster.database()).unwrap();
    assert_eq!(health.exit_code(), 0);
    let job = health.latest_job.unwrap();
    assert_eq!(job.job_id, second.job_id);
    assert_eq!(job.status, JobStatus::Complete);
    assert_eq!(health.incarnations.len(), 2);
    assert!(health
        .incarnations
        .iter()
        .all(|incarnation| incarnation.status == IncarnationStatus::Active));
    assert_eq!(health.latest_complete_scans.len(), 6);
    assert!(health.unhealthy_objects.is_empty());
}

#[tokio::test]
async fn corrupt_copy_is_reported_then_repaired_in_place_without_deleting() {
    let (cluster, file_id) = committed_cluster().await;
    let chunk = required(&cluster)
        .into_iter()
        .find(|object| object.kind == ObjectKind::Chunk)
        .unwrap();
    let path = cluster.object_path(0, &chunk);
    fs::write(&path, b"bit rot").unwrap();
    let before_a = snapshot(&cluster.agents[0].volume);
    let before_b = snapshot(&cluster.agents[1].volume);

    let report = cluster.scrub(JobMode::Verify).await;
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.health.at_risk, 1, "{report:?}");
    assert_eq!(report.health.durable, 2);
    assert_eq!(report.totals.corrupt_copies, 1);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].state, ObjectHealthState::AtRisk);
    assert_eq!(
        placement(&cluster, &chunk, "agent-a"),
        PlacementState::Corrupt
    );
    assert_eq!(
        placement(&cluster, &chunk, "agent-b"),
        PlacementState::Confirmed
    );
    assert_eq!(
        snapshot(&cluster.agents[0].volume),
        before_a,
        "verify never writes"
    );
    assert_eq!(snapshot(&cluster.agents[1].volume), before_b);
    download_matches(&cluster, file_id, "after-corruption.bin").await;

    let repair = cluster.scrub(JobMode::Repair).await;
    assert_eq!(repair.exit_code(), 0, "{repair:?}");
    assert_eq!(repair.repairs.len(), 1);
    assert_eq!(repair.repairs[0].status, "applied");
    assert_eq!(repair.repairs[0].target_agent, "agent-a");
    assert_eq!(repair.repairs[0].source_agent, "agent-b");
    assert_eq!(repair.health.durable, 3);
    assert_eq!(
        placement(&cluster, &chunk, "agent-a"),
        PlacementState::Confirmed
    );
    let after_a = snapshot(&cluster.agents[0].volume);
    assert_eq!(
        after_a.len(),
        before_a.len(),
        "repair replaced bytes, removed nothing"
    );
    assert_ne!(after_a, before_a);
    assert!(DurableStore::open(&cluster.agents[0].volume)
        .unwrap()
        .get(ObjectKind::Chunk, &chunk.hash)
        .is_ok());
    assert_eq!(snapshot(&cluster.agents[1].volume), before_b);
    download_matches(&cluster, file_id, "after-repair.bin").await;
}

#[tokio::test]
async fn missing_copy_is_proven_by_a_complete_scan_and_restored() {
    let (cluster, _) = committed_cluster().await;
    let manifest = required(&cluster)
        .into_iter()
        .find(|object| object.kind == ObjectKind::Manifest)
        .unwrap();
    fs::remove_file(cluster.object_path(1, &manifest)).unwrap();

    let report = cluster.scrub(JobMode::Verify).await;
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.totals.missing_copies, 1);
    assert_eq!(
        placement(&cluster, &manifest, "agent-b"),
        PlacementState::Missing
    );
    assert_eq!(report.findings[0].state, ObjectHealthState::AtRisk);
    assert!(report.findings[0]
        .placements
        .iter()
        .any(|placement| placement.agent_id == "agent-b"
            && placement.state == PlacementState::Missing
            && placement.verified_in_this_job));

    let repair = cluster.scrub(JobMode::Repair).await;
    assert_eq!(repair.exit_code(), 0, "{repair:?}");
    assert_eq!(repair.totals.repairs_applied, 1);
    assert_eq!(
        placement(&cluster, &manifest, "agent-b"),
        PlacementState::Confirmed
    );
    assert!(cluster.object_path(1, &manifest).is_file());
}

#[tokio::test]
async fn wiped_volume_supersedes_the_incarnation_and_old_copies_stop_counting() {
    let (mut cluster, file_id) = committed_cluster().await;
    let mut old_b = cluster.agents.remove(1);
    let old_volume = old_b.volume.clone();
    let old_holder = old_b._holder.take();
    drop(old_b);
    let fresh = tempfile::tempdir().unwrap();
    let fresh_volume = fresh.path().to_owned();
    cluster
        .agents
        .push(start_agent_on("agent-b", "host-b", fresh_volume, Some(fresh)).await);

    let report = cluster.scrub(JobMode::Verify).await;
    assert_eq!(report.exit_code(), 2);
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.contains("replacing")));
    assert!(matches!(
        report.agents[1].incarnation,
        Some(IncarnationObservation::Superseded { .. })
    ));
    assert_eq!(report.health.at_risk, 3, "{report:?}");
    // The complete scan of the fresh incarnation proves the old copies are
    // gone, so the rows flip to missing under the new incarnation.
    for object in required(&cluster) {
        assert_eq!(
            placement(&cluster, &object, "agent-b"),
            PlacementState::Missing
        );
        assert_eq!(
            cluster
                .database()
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            1
        );
    }
    let health = health_report(&cluster.database()).unwrap();
    let statuses: Vec<_> = health
        .incarnations
        .iter()
        .filter(|incarnation| incarnation.agent_id == "agent-b")
        .map(|incarnation| incarnation.status)
        .collect();
    assert_eq!(
        statuses,
        vec![IncarnationStatus::Superseded, IncarnationStatus::Active]
    );
    download_matches(&cluster, file_id, "one-domain.bin").await;

    let repair = cluster.scrub(JobMode::Repair).await;
    assert_eq!(repair.exit_code(), 0, "{repair:?}");
    assert_eq!(repair.totals.repairs_applied, 3);
    for object in required(&cluster) {
        assert_eq!(
            cluster
                .database()
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            2
        );
    }

    // The superseded incarnation returning under the same name fails closed.
    let _keep = old_holder;
    let returned = start_agent_on("agent-b", "host-b", old_volume, None).await;
    let mut targets = cluster.targets();
    targets[1] = returned.target.clone();
    let mut database = cluster.database();
    let error = scrub(
        &mut database,
        &targets,
        &reqwest::Client::new(),
        JobMode::Repair,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            ScrubError::Metadata(MetadataError::SupersededIncarnation { .. })
        ),
        "{error}"
    );
    let latest = database.latest_reconcile_job().unwrap().unwrap();
    assert_eq!(latest.status, JobStatus::Failed);
    for object in required(&cluster) {
        assert_eq!(
            database
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            2
        );
    }
    let error = Portal::open(
        &cluster.database_path,
        MasterKey::load(&cluster.key_path).unwrap(),
        targets,
    )
    .unwrap()
    .delete(file_id, "delete-with-stale-node")
    .await
    .unwrap_err();
    assert!(error.to_string().contains("superseded"), "{error}");
}

#[tokio::test]
async fn failed_inventory_proves_nothing_and_defers_repair() {
    let (cluster, _) = committed_cluster().await;
    let malformed = cluster.agents[0].volume.join("objects/chunk/not-hex");
    fs::create_dir(&malformed).unwrap();
    let before = snapshot(&cluster.agents[0].volume);

    let report = cluster.scrub(JobMode::Repair).await;
    assert_eq!(report.exit_code(), 2);
    assert!(!report.complete_observation);
    assert!(report.scans.iter().any(|scan| scan.agent_id == "agent-a"
        && scan.kind == ObjectKind::Chunk
        && scan.inventory_digest.is_none()));
    assert_eq!(report.totals.unverified_copies, 2);
    assert_eq!(report.totals.missing_copies, 0);
    assert_eq!(report.totals.repairs_applied, 0);
    assert_eq!(report.totals.repairs_deferred, 2);
    assert!(report.repairs.is_empty());
    for object in required(&cluster) {
        assert_eq!(
            placement(&cluster, &object, "agent-a"),
            PlacementState::Confirmed
        );
    }
    assert_eq!(snapshot(&cluster.agents[0].volume), before);
    assert_eq!(
        snapshot(&cluster.agents[1].volume).len(),
        4,
        "incarnation plus three objects"
    );

    fs::remove_dir(&malformed).unwrap();
    let healed = cluster.scrub(JobMode::Verify).await;
    assert_eq!(healed.exit_code(), 0, "{healed:?}");
}

#[tokio::test]
async fn unreachable_agent_leaves_placements_unverified() {
    let (cluster, _) = committed_cluster().await;
    cluster.agents[1].task.abort();
    tokio::task::yield_now().await;

    let report = cluster.scrub(JobMode::Repair).await;
    assert_eq!(report.exit_code(), 2);
    assert!(!report.agents[1].reachable);
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.contains("unreachable")));
    assert_eq!(report.totals.reachable_agents, 1);
    assert_eq!(report.totals.unverified_copies, 3);
    assert_eq!(report.health.at_risk, 3);
    assert_eq!(report.totals.repairs_deferred, 3);
    for object in required(&cluster) {
        assert_eq!(
            placement(&cluster, &object, "agent-b"),
            PlacementState::Confirmed
        );
        assert_eq!(
            cluster
                .database()
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            2
        );
    }
    let health = health_report(&cluster.database()).unwrap();
    assert_eq!(health.exit_code(), 2);
    assert_eq!(health.unhealthy_objects.len(), 3);
}

#[tokio::test]
async fn deleted_files_require_only_their_marker() {
    let (cluster, file_id) = committed_cluster().await;
    cluster
        .portal()
        .delete(file_id, "lifecycle-delete")
        .await
        .unwrap();
    let required = required(&cluster);
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].kind, ObjectKind::DeletionMarker);

    let report = cluster.scrub(JobMode::Verify).await;
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(report.health.durable, 1);
    for agent in &report.agents {
        assert_eq!(agent.listed_objects, 4);
        assert_eq!(agent.unreferenced_objects, 3, "old generation is retained");
    }
    let path = cluster.object_path(0, &required[0]);
    assert!(path.is_file());
}

#[tokio::test]
async fn scrub_does_not_need_the_master_key() {
    let (cluster, _) = committed_cluster().await;
    fs::remove_file(&cluster.key_path).unwrap();
    let report = cluster.scrub(JobMode::Verify).await;
    assert_eq!(report.exit_code(), 0);
}

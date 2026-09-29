//! Movement, retirement, capacity admission, and proof-based garbage
//! collection. Physical deletion happens only through an applied proof, and
//! every gate in `docs/m1-lifecycle-contract.md` has a negative test here.

use std::{fs, path::PathBuf};

use distr_hnsw::{
    agent::{serve_agent_with_capacity, AgentIdentity, CapacityConfig},
    crypto::MasterKey,
    durability::DurableStore,
    lifecycle::{drain, gc, retire, GcOptions},
    metadata::{Database, IncarnationStatus, JobMode, PlacementState},
    object::ObjectKind,
    portal::{AgentTarget, Portal, PortalError},
    reconcile::scrub,
    CHUNK_SIZE,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

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

async fn start_agent_with(
    id: &str,
    domain: &str,
    volume: PathBuf,
    holder: Option<TempDir>,
    capacity: CapacityConfig,
) -> TestAgent {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: domain.to_owned(),
    };
    let volume_path = volume.clone();
    let task = tokio::spawn(async move {
        serve_agent_with_capacity(listener, volume_path, identity, capacity)
            .await
            .unwrap();
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
    start_agent_with(id, domain, volume, Some(holder), CapacityConfig::default()).await
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
        Portal::open(
            &self.database_path,
            MasterKey::load(&self.key_path).unwrap(),
            self.targets(),
        )
        .unwrap()
    }

    fn database(&self) -> Database {
        Database::open(&self.database_path).unwrap()
    }

    async fn scrub(&self) {
        let mut database = self.database();
        let report = scrub(
            &mut database,
            &self.targets(),
            &reqwest::Client::new(),
            JobMode::Verify,
        )
        .await
        .unwrap();
        assert!(report.complete_observation, "{report:?}");
    }

    fn holds(&self, index: usize, kind: ObjectKind, hash: &distr_hnsw::object::ObjectHash) -> bool {
        DurableStore::open(&self.agents[index].volume)
            .unwrap()
            .get(kind, hash)
            .is_ok()
    }

    async fn upload(&self, name: &str, size: usize, key: &str) -> uuid::Uuid {
        let source = self.workspace.path().join(name);
        let bytes: Vec<u8> = (0..size)
            .map(|index| ((index * 11 + 7) % 251) as u8)
            .collect();
        fs::write(&source, bytes).unwrap();
        self.portal().upload(&source, key).await.unwrap()
    }
}

async fn three_agent_cluster() -> Cluster {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    MasterKey::create(&key_path).unwrap();
    Cluster {
        database_path: workspace.path().join("portal.sqlite"),
        key_path,
        workspace,
        agents: vec![
            start_agent("agent-a", "host-a").await,
            start_agent("agent-b", "host-b").await,
            start_agent("agent-c", "host-c").await,
        ],
    }
}

fn placement(
    cluster: &Cluster,
    kind: ObjectKind,
    hash: &distr_hnsw::object::ObjectHash,
    agent: &str,
) -> Option<PlacementState> {
    cluster
        .database()
        .all_placements(kind, hash)
        .unwrap()
        .into_iter()
        .find(|(id, _, _)| id == agent)
        .map(|(_, _, state)| state)
}

#[tokio::test]
async fn drain_moves_copies_first_and_only_then_orphans_the_source() {
    let cluster = three_agent_cluster().await;
    let file_id = cluster
        .upload("file.bin", CHUNK_SIZE + 512, "drain-file")
        .await;
    let required = cluster.database().required_objects().unwrap();
    assert_eq!(required.len(), 3);
    // Uploads replicate to every configured agent, so drain a, then prove
    // the interesting case: only two copies remain and one must move.
    let client = reqwest::Client::new();
    let mut database = cluster.database();
    let dry = drain(&mut database, &cluster.targets(), &client, "agent-a", true)
        .await
        .unwrap();
    assert_eq!(dry.exit_code(), 0, "{dry:?}");
    assert_eq!(
        dry.totals.already_redundant, 3,
        "b and c already hold copies"
    );
    for object in &required {
        assert_eq!(
            placement(&cluster, object.kind, &object.hash, "agent-a"),
            Some(PlacementState::Confirmed),
            "dry run changes nothing"
        );
    }

    // Remove c's copies so draining a requires real moves onto c.
    for object in &required {
        fs::remove_file(
            DurableStore::open(&cluster.agents[2].volume)
                .unwrap()
                .object_path(object.kind, &object.hash),
        )
        .unwrap();
    }
    let report = drain(&mut database, &cluster.targets(), &client, "agent-a", false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(report.totals.moves_applied, 3);
    assert_eq!(report.totals.orphaned, 3);
    assert!(report
        .moves
        .iter()
        .all(|m| m.target_agent == "agent-c" && m.status == "applied"));
    for object in &required {
        assert_eq!(
            placement(&cluster, object.kind, &object.hash, "agent-a"),
            Some(PlacementState::Orphaned)
        );
        assert_eq!(
            placement(&cluster, object.kind, &object.hash, "agent-c"),
            Some(PlacementState::Confirmed)
        );
        assert!(
            cluster.holds(0, object.kind, &object.hash),
            "drain never deletes the source copy"
        );
        assert!(cluster.holds(2, object.kind, &object.hash));
        assert_eq!(
            database
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            2
        );
    }
    let destination = cluster.workspace.path().join("after-drain.bin");
    cluster
        .portal()
        .download(file_id, &destination)
        .await
        .unwrap();
    assert_eq!(
        fs::read(destination).unwrap(),
        fs::read(cluster.workspace.path().join("file.bin")).unwrap()
    );
}

#[tokio::test]
async fn drain_blocks_when_no_other_failure_domain_can_take_the_copy() {
    let cluster = three_agent_cluster().await;
    cluster.upload("file.bin", 4096, "drain-blocked").await;
    let required = cluster.database().required_objects().unwrap();
    let mut database = cluster.database();
    let two = vec![
        cluster.agents[0].target.clone(),
        cluster.agents[1].target.clone(),
    ];
    for object in &required {
        fs::remove_file(
            DurableStore::open(&cluster.agents[1].volume)
                .unwrap()
                .object_path(object.kind, &object.hash),
        )
        .unwrap();
    }
    let report = drain(
        &mut database,
        &two,
        &reqwest::Client::new(),
        "agent-a",
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.totals.blocked, required.len());
    assert_eq!(report.totals.orphaned, 0);
    for object in &required {
        assert_eq!(
            placement(&cluster, object.kind, &object.hash, "agent-a"),
            Some(PlacementState::Confirmed)
        );
    }
}

#[tokio::test]
async fn retire_refuses_until_the_floor_holds_without_the_agent_and_then_locks_it_out() {
    let cluster = three_agent_cluster().await;
    cluster.upload("file.bin", 4096, "retire-file").await;
    let required = cluster.database().required_objects().unwrap();
    let mut database = cluster.database();
    let client = reqwest::Client::new();
    // Every agent holds every object after upload, so retirement of a is
    // immediately safe. Damage c first to prove the refusal path.
    let chunk = required
        .iter()
        .find(|o| o.kind == ObjectKind::Chunk)
        .unwrap();
    let path = DurableStore::open(&cluster.agents[2].volume)
        .unwrap()
        .object_path(chunk.kind, &chunk.hash);
    fs::write(&path, b"rot").unwrap();
    let refused = retire(&mut database, &cluster.targets(), &client, "agent-a")
        .await
        .unwrap();
    assert_eq!(refused.exit_code(), 2);
    assert!(refused.retired_incarnation.is_none());
    assert_eq!(refused.objects_at_risk_without_agent, 1);

    // Repair c, then retirement succeeds even though a is unreachable.
    let mut repaired = cluster.database();
    scrub(&mut repaired, &cluster.targets(), &client, JobMode::Repair)
        .await
        .unwrap();
    cluster.agents[0].task.abort();
    tokio::task::yield_now().await;
    let report = retire(&mut database, &cluster.targets(), &client, "agent-a")
        .await
        .unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let retired = report.retired_incarnation.clone().unwrap();
    let statuses: Vec<_> = database
        .agent_incarnations()
        .unwrap()
        .into_iter()
        .filter(|i| i.agent_id == "agent-a")
        .map(|i| i.status)
        .collect();
    assert_eq!(statuses, vec![IncarnationStatus::Retired]);
    for object in &required {
        assert_eq!(
            placement(&cluster, object.kind, &object.hash, "agent-a"),
            Some(PlacementState::Orphaned)
        );
        assert_eq!(
            database
                .confirmed_domains(object.kind, &object.hash)
                .unwrap(),
            2
        );
    }
    // The retired incarnation cannot rejoin under its old identity.
    assert!(matches!(
        database.observe_agent_incarnation("agent-a", "host-a", &retired),
        Err(distr_hnsw::metadata::MetadataError::RetiredIncarnation { .. })
    ));
}

#[tokio::test]
async fn capacity_admission_refuses_chunks_keeps_deletes_and_converges_after_growth() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    MasterKey::create(&key_path).unwrap();
    let tiny = CapacityConfig {
        quota_bytes: Some(64 * 1024),
        reserve_bytes: Some(8 * 1024),
        hard_floor_bytes: Some(1024),
    };
    let holder_b = tempfile::tempdir().unwrap();
    let volume_b = holder_b.path().to_owned();
    let mut cluster = Cluster {
        database_path: workspace.path().join("portal.sqlite"),
        key_path,
        workspace,
        agents: vec![
            start_agent("agent-a", "host-a").await,
            start_agent_with("agent-b", "host-b", volume_b.clone(), Some(holder_b), tiny).await,
        ],
    };
    // A small file fits under the quota and reserve.
    let small = cluster.upload("small.bin", 4096, "cap-small").await;
    // A large one cannot be admitted: only host-a can hold it.
    let big_source = cluster.workspace.path().join("big.bin");
    fs::write(&big_source, vec![1_u8; 200 * 1024]).unwrap();
    let error = cluster
        .portal()
        .upload(&big_source, "cap-big")
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PortalError::InsufficientCapacity {
                domains_eligible: 1,
                ..
            }
        ),
        "{error}"
    );
    assert!(cluster.portal().is_visible(small).unwrap());
    assert_eq!(
        cluster.database().required_objects().unwrap().len(),
        2,
        "nothing partial is visible"
    );
    // No chunk of the refused upload was written to the constrained agent.
    let store_b = DurableStore::open(&volume_b).unwrap();
    assert!(store_b.used_bytes() < 64 * 1024);

    // Deletes still commit: markers may use the reserve.
    let deleted = cluster.portal().delete(small, "cap-delete").await.unwrap();
    assert!(cluster.holds(1, ObjectKind::DeletionMarker, &deleted.marker_hash));

    // Raise the quota by restarting b on the same volume (same incarnation);
    // the same idempotency key then converges.
    let mut old_b = cluster.agents.pop().unwrap();
    let _keep = old_b._holder.take();
    drop(old_b);
    let mut grown = cluster;
    grown.agents.push(
        start_agent_with(
            "agent-b",
            "host-b",
            volume_b.clone(),
            None,
            CapacityConfig {
                quota_bytes: Some(4 * 1024 * 1024),
                reserve_bytes: Some(8 * 1024),
                hard_floor_bytes: Some(1024),
            },
        )
        .await,
    );
    let big = grown.portal().upload(&big_source, "cap-big").await.unwrap();
    assert!(grown.portal().is_visible(big).unwrap());
    let incarnations = grown.database().agent_incarnations().unwrap();
    assert_eq!(
        incarnations
            .iter()
            .filter(|i| i.agent_id == "agent-b")
            .count(),
        1,
        "same volume keeps its incarnation"
    );
}

async fn deleted_file_cluster() -> (
    Cluster,
    Vec<(ObjectKind, distr_hnsw::object::ObjectHash)>,
    distr_hnsw::object::ObjectHash,
) {
    let cluster = three_agent_cluster().await;
    let file_id = cluster
        .upload("doomed.bin", CHUNK_SIZE + 64, "gc-doomed")
        .await;
    let old = cluster
        .database()
        .required_objects()
        .unwrap()
        .into_iter()
        .map(|o| (o.kind, o.hash))
        .collect::<Vec<_>>();
    let marker = cluster
        .portal()
        .delete(file_id, "gc-delete")
        .await
        .unwrap()
        .marker_hash;
    (cluster, old, marker)
}

#[tokio::test]
async fn gc_requires_observation_after_deletion_and_retention_then_deletes_only_proven_objects() {
    let (cluster, old, marker) = deleted_file_cluster().await;
    let client = reqwest::Client::new();
    let options = |apply: bool, retention: i64| GcOptions {
        retention_seconds: retention,
        staging_grace_seconds: 0,
        apply,
    };

    // No complete observation since the deletion: every candidate is blocked.
    let mut database = cluster.database();
    let report = gc(&mut database, &cluster.targets(), &client, options(true, 0))
        .await
        .unwrap();
    assert_eq!(report.totals.candidates, 3, "{report:?}");
    assert_eq!(report.totals.blocked, 3);
    assert_eq!(report.totals.applied, 0);
    assert!(report
        .candidates
        .iter()
        .all(|c| c.blockers.iter().any(|b| b.contains("observation"))));
    for (kind, hash) in &old {
        assert!(
            cluster.holds(0, *kind, hash),
            "nothing deleted while blocked"
        );
    }

    // Observe, but retention has not elapsed.
    cluster.scrub().await;
    let report = gc(
        &mut database,
        &cluster.targets(),
        &client,
        options(true, 3600),
    )
    .await
    .unwrap();
    assert_eq!(report.totals.blocked, 3);
    assert!(report
        .candidates
        .iter()
        .all(|c| c.blockers.iter().any(|b| b.contains("retention"))));

    // Plan only: proofs recorded, nothing deleted.
    let plan = gc(
        &mut database,
        &cluster.targets(),
        &client,
        options(false, 0),
    )
    .await
    .unwrap();
    assert_eq!(plan.exit_code(), 0, "{plan:?}");
    assert_eq!(plan.totals.proven, 3);
    assert!(plan
        .candidates
        .iter()
        .all(|c| c.status == "planned" && c.proof_id.is_some()));
    for (kind, hash) in &old {
        assert!(cluster.holds(0, *kind, hash));
    }
    assert!(!plan
        .candidates
        .iter()
        .any(|c| c.kind == ObjectKind::DeletionMarker));

    // Apply: proven objects are deleted everywhere; the marker survives.
    let apply = gc(&mut database, &cluster.targets(), &client, options(true, 0))
        .await
        .unwrap();
    assert_eq!(apply.exit_code(), 0, "{apply:?}");
    assert_eq!(apply.totals.applied, 3);
    for (kind, hash) in &old {
        for index in 0..3 {
            assert!(
                !cluster.holds(index, *kind, hash),
                "{kind} {hash} still on agent {index}"
            );
        }
        assert!(database.all_placements(*kind, hash).unwrap().is_empty());
    }
    for index in 0..3 {
        assert!(cluster.holds(index, ObjectKind::DeletionMarker, &marker));
    }
    let again = gc(&mut database, &cluster.targets(), &client, options(true, 0))
        .await
        .unwrap();
    assert_eq!(
        again.totals.candidates, 0,
        "collected objects are no longer candidates"
    );
    let health = scrub(&mut database, &cluster.targets(), &client, JobMode::Verify)
        .await
        .unwrap();
    assert_eq!(health.exit_code(), 0, "{health:?}");
}

#[tokio::test]
async fn gc_proof_goes_stale_when_membership_or_content_changes_before_apply() {
    let (mut cluster, old, _) = deleted_file_cluster().await;
    let client = reqwest::Client::new();
    cluster.scrub().await;
    let options = GcOptions {
        retention_seconds: 0,
        staging_grace_seconds: 0,
        apply: false,
    };
    let mut database = cluster.database();
    let plan = gc(&mut database, &cluster.targets(), &client, options)
        .await
        .unwrap();
    assert_eq!(plan.totals.proven, 3);
    let proofs: Vec<_> = plan
        .candidates
        .iter()
        .map(|c| c.proof_id.unwrap())
        .collect();

    // A wiped volume returns under the same name: new incarnation, no
    // observation from it yet, so every proof is stale and blocked.
    let old_c = cluster.agents.pop().unwrap();
    drop(old_c);
    let fresh = tempfile::tempdir().unwrap();
    let path = fresh.path().to_owned();
    cluster.agents.push(
        start_agent_with(
            "agent-c",
            "host-c",
            path,
            Some(fresh),
            CapacityConfig::default(),
        )
        .await,
    );
    let replan = gc(
        &mut database,
        &cluster.targets(),
        &client,
        GcOptions {
            apply: true,
            ..options
        },
    )
    .await
    .unwrap();
    assert_eq!(replan.totals.applied, 0, "{replan:?}");
    assert_eq!(replan.totals.blocked, 3);
    assert!(
        database.planned_gc_proofs().unwrap().is_empty()
            || database
                .planned_gc_proofs()
                .unwrap()
                .iter()
                .all(|p| !proofs.contains(&p.proof_id)),
        "old proofs are no longer planned"
    );
    for (kind, hash) in &old {
        assert!(cluster.holds(0, *kind, hash));
    }

    // Observe the new incarnation, plan again, then commit new content
    // before applying: the proof re-derivation notices and refuses.
    cluster.scrub().await;
    let plan = gc(&mut database, &cluster.targets(), &client, options)
        .await
        .unwrap();
    assert_eq!(plan.totals.proven, 3, "{plan:?}");
    cluster.upload("newer.bin", 2048, "gc-newer").await;
    // Apply in one call re-derives inside; simulate a stale planned proof by
    // checking the recorded generation differs from the current one.
    let current = database.content_generation().unwrap();
    assert!(database
        .planned_gc_proofs()
        .unwrap()
        .iter()
        .all(|p| p.content_generation < current));
    let apply = gc(
        &mut database,
        &cluster.targets(),
        &client,
        GcOptions {
            apply: true,
            ..options
        },
    )
    .await
    .unwrap();
    // The stale proofs are superseded by fresh ones that pass, since the new
    // upload does not reference the old objects; what matters is that no
    // proof recorded before the change was executed.
    assert_eq!(apply.totals.stale, 3, "{apply:?}");
    assert_eq!(apply.totals.applied, 3);
}

#[tokio::test]
async fn gc_defers_when_an_agent_is_unreachable_and_never_deletes_live_objects() {
    let (cluster, old, _) = deleted_file_cluster().await;
    let live = cluster.upload("live.bin", 1024, "gc-live").await;
    let client = reqwest::Client::new();
    cluster.scrub().await;
    let mut database = cluster.database();
    cluster.agents[2].task.abort();
    tokio::task::yield_now().await;
    let options = GcOptions {
        retention_seconds: 0,
        staging_grace_seconds: 0,
        apply: true,
    };
    let report = gc(&mut database, &cluster.targets(), &client, options)
        .await
        .unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.totals.applied, 0, "{report:?}");
    assert_eq!(report.totals.deferred, 3);
    for (kind, hash) in &old {
        assert!(cluster.holds(0, *kind, hash));
    }
    let live_objects = cluster.database().required_objects().unwrap();
    assert!(live_objects.iter().any(|o| o.file_id == live));
    assert!(!report
        .candidates
        .iter()
        .any(|c| live_objects.iter().any(|o| o.hash.to_string() == c.hash)));
}

#[tokio::test]
async fn expired_staging_uploads_become_candidates_only_after_the_grace_period() {
    let cluster = three_agent_cluster().await;
    let source = cluster.workspace.path().join("abandoned.bin");
    fs::write(&source, vec![5_u8; 8192]).unwrap();
    let mut portal = cluster.portal().with_failpoint(
        distr_hnsw::portal::Failpoint::AfterChunksDurable,
        distr_hnsw::portal::FailpointAction::ReturnError,
    );
    assert!(portal.upload(&source, "abandoned").await.is_err());
    drop(portal);
    cluster.scrub().await;
    let client = reqwest::Client::new();
    let mut database = cluster.database();
    let fresh = gc(
        &mut database,
        &cluster.targets(),
        &client,
        GcOptions {
            retention_seconds: 0,
            staging_grace_seconds: 3600,
            apply: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(fresh.totals.candidates, 0, "{fresh:?}");
    let expired = gc(
        &mut database,
        &cluster.targets(),
        &client,
        GcOptions {
            retention_seconds: 0,
            staging_grace_seconds: 0,
            apply: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(expired.totals.candidates, 1, "{expired:?}");
    assert_eq!(expired.totals.applied, 1);
    let chunk_hash = expired.candidates[0].hash.clone();
    let hash = distr_hnsw::object::ObjectHash::parse(chunk_hash).unwrap();
    for index in 0..3 {
        assert!(!cluster.holds(index, ObjectKind::Chunk, &hash));
    }
}

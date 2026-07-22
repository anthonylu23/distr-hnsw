use std::{fs, path::Path};

use axum::{extract::DefaultBodyLimit, http::StatusCode, routing::get, Json, Router};
use distr_hnsw::{
    agent::{serve_agent, AgentIdentity},
    crypto::MasterKey,
    durability::DurableStore,
    metadata::Database,
    object::ObjectKind,
    portal::{AgentTarget, Failpoint, FailpointAction, Portal, PortalError},
    CHUNK_SIZE,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

struct TestAgent {
    target: AgentTarget,
    volume: TempDir,
    task: JoinHandle<()>,
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_agent(id: &str, failure_domain: &str) -> TestAgent {
    let volume = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: failure_domain.to_owned(),
    };
    let volume_path = volume.path().to_owned();
    let task = tokio::spawn(async move {
        serve_agent(listener, volume_path, identity).await.unwrap();
    });
    TestAgent {
        target: AgentTarget {
            id: id.to_owned(),
            failure_domain: failure_domain.to_owned(),
            base_url: format!("http://{address}"),
        },
        volume,
        task,
    }
}

async fn start_rejecting_agent(id: &str, failure_domain: &str) -> (AgentTarget, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: failure_domain.to_owned(),
    };
    let router = Router::new()
        .route(
            "/v1/health",
            get({
                let identity = identity.clone();
                move || async move { Json(identity) }
            }),
        )
        .route(
            "/v1/objects/{kind}/{hash}",
            axum::routing::put(|_body: axum::body::Bytes| async {
                StatusCode::INSUFFICIENT_STORAGE
            }),
        )
        .layer(DefaultBodyLimit::max(CHUNK_SIZE + 1024 * 1024));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        AgentTarget {
            id: id.to_owned(),
            failure_domain: failure_domain.to_owned(),
            base_url: format!("http://{address}"),
        },
        task,
    )
}

fn write_source(path: &Path) -> Vec<u8> {
    let bytes: Vec<_> = (0..CHUNK_SIZE + 37)
        .map(|index| ((index * 31 + 17) % 251) as u8)
        .collect();
    fs::write(path, &bytes).unwrap();
    bytes
}

#[tokio::test]
async fn every_commit_failpoint_retries_to_one_visible_file() {
    let agent_a = start_agent("agent-a", "host-a").await;
    let agent_b = start_agent("agent-b", "host-b").await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    MasterKey::create(&key_path).unwrap();
    let source = workspace.path().join("source.bin");
    let expected = write_source(&source);

    let failpoints = [
        Failpoint::AfterPlan,
        Failpoint::AfterFirstChunkReplica,
        Failpoint::AfterChunksDurable,
        Failpoint::AfterFirstManifestReplica,
        Failpoint::AfterManifestDurable,
        Failpoint::BeforeCommit,
        Failpoint::AfterCommit,
    ];

    for failpoint in failpoints {
        let database_path = workspace
            .path()
            .join(format!("{}.sqlite", failpoint.as_str()));
        let key = MasterKey::load(&key_path).unwrap();
        let mut portal = Portal::open(&database_path, key, agents.clone())
            .unwrap()
            .with_failpoint(failpoint, FailpointAction::ReturnError);
        let idempotency_key = format!("test-{}", failpoint.as_str());
        let error = portal.upload(&source, &idempotency_key).await.unwrap_err();
        assert!(matches!(
            error,
            PortalError::InjectedFailure(actual) if actual == failpoint
        ));
        drop(portal);

        let database = Database::open(&database_path).unwrap();
        let upload = database
            .upload_by_idempotency(&idempotency_key)
            .unwrap()
            .unwrap();
        let visible_before_retry = database.file_by_id(upload.file_id).unwrap().is_some();
        assert_eq!(visible_before_retry, failpoint == Failpoint::AfterCommit);
        drop(database);

        let key = MasterKey::load(&key_path).unwrap();
        let mut portal = Portal::open(&database_path, key, agents.clone()).unwrap();
        let file_id = portal.upload(&source, &idempotency_key).await.unwrap();
        assert_eq!(file_id, upload.file_id);
        assert!(portal.is_visible(file_id).unwrap());
        assert_eq!(
            portal.upload(&source, &idempotency_key).await.unwrap(),
            file_id
        );

        let destination = workspace
            .path()
            .join(format!("{}.download", failpoint.as_str()));
        portal.download(file_id, &destination).await.unwrap();
        assert_eq!(fs::read(destination).unwrap(), expected);
    }
}

#[tokio::test]
async fn conflicting_idempotency_and_corrupt_replicas_fail_closed() {
    let agent_a = start_agent("agent-a", "host-a").await;
    let agent_b = start_agent("agent-b", "host-b").await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    MasterKey::create(&key_path).unwrap();
    let source = workspace.path().join("source.bin");
    let expected = write_source(&source);

    let key = MasterKey::load(&key_path).unwrap();
    let mut portal = Portal::open(&database_path, key, agents.clone()).unwrap();
    let file_id = portal.upload(&source, "stable-request").await.unwrap();

    let mut changed = expected.clone();
    changed[0] ^= 1;
    fs::write(&source, &changed).unwrap();
    assert!(matches!(
        portal.upload(&source, "stable-request").await,
        Err(PortalError::IdempotencyConflict)
    ));
    fs::write(&source, &expected).unwrap();

    let database = Database::open(&database_path).unwrap();
    let upload = database
        .upload_by_idempotency("stable-request")
        .unwrap()
        .unwrap();
    let first_chunk = database.chunks(upload.upload_id).unwrap().remove(0);
    let chunk_hash = first_chunk.ciphertext_hash.unwrap();
    drop(database);

    let store_a = DurableStore::open(agent_a.volume.path()).unwrap();
    fs::write(
        store_a.object_path(ObjectKind::Chunk, &chunk_hash),
        b"corrupt-a",
    )
    .unwrap();
    let destination = workspace.path().join("one-good-replica.bin");
    portal.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(&destination).unwrap(), expected);
    assert!(matches!(
        portal.download(file_id, &destination).await,
        Err(PortalError::DestinationExists(_))
    ));

    let store_b = DurableStore::open(agent_b.volume.path()).unwrap();
    fs::write(
        store_b.object_path(ObjectKind::Chunk, &chunk_hash),
        b"corrupt-b",
    )
    .unwrap();
    let destination = workspace.path().join("no-good-replica.bin");
    assert!(matches!(
        portal.download(file_id, &destination).await,
        Err(PortalError::NoValidReplica { .. })
    ));
    assert!(!destination.exists());
}

#[tokio::test]
async fn rf2_never_commits_when_the_second_agent_rejects_writes() {
    let agent_a = start_agent("agent-a", "host-a").await;
    let (agent_b, rejecting_task) = start_rejecting_agent("agent-b", "host-b").await;
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    MasterKey::create(&key_path).unwrap();
    let source = workspace.path().join("source.bin");
    write_source(&source);

    let key = MasterKey::load(&key_path).unwrap();
    let mut portal =
        Portal::open(&database_path, key, vec![agent_a.target.clone(), agent_b]).unwrap();
    assert!(matches!(
        portal.upload(&source, "rf2-refusal").await,
        Err(PortalError::AgentRejected { status: 507, .. })
    ));

    let database = Database::open(&database_path).unwrap();
    let upload = database
        .upload_by_idempotency("rf2-refusal")
        .unwrap()
        .unwrap();
    assert!(database.file_by_id(upload.file_id).unwrap().is_none());
    rejecting_task.abort();
}

#[tokio::test]
async fn empty_file_commits_as_a_manifest_without_chunks() {
    let agent_a = start_agent("agent-a", "host-a").await;
    let agent_b = start_agent("agent-b", "host-b").await;
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    MasterKey::create(&key_path).unwrap();

    let key = MasterKey::load(&key_path).unwrap();
    let mut portal = Portal::open(
        &database_path,
        key,
        vec![agent_a.target.clone(), agent_b.target.clone()],
    )
    .unwrap();
    let file_id = portal.upload(&source, "empty-file").await.unwrap();
    let destination = workspace.path().join("empty.download");
    portal.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), Vec::<u8>::new());
}

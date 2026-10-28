use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use distr_hnsw::{
    access::create_join_token,
    agent::{serve_agent, AgentIdentity},
    agent_client::join_agent,
    crypto::{
        encrypt_chunk, random_key, random_nonce, unwrap_key, wrap_key, MasterKey, ENVELOPE_VERSION,
    },
    durability::DurableStore,
    format::{
        decode_manifest, encode_deletion_marker, encode_manifest, ChunkRecord, ManifestPayload,
    },
    metadata::{Database, FileState},
    object::{ObjectHash, ObjectKind},
    portal::{AgentTarget, Failpoint, FailpointAction, Portal, PortalError},
    recovery::RecoveryError,
    tailscale::LocalApi,
};
use rusqlite::{params, Connection};
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

/// Start an agent with a fresh join token from the portal database at
/// `database_path` (bound to the key at `key_path`) and join it there. An
/// agent accepts grants only from the portal that joined it.
async fn start_agent(id: &str, domain: &str, database_path: &Path, key_path: &Path) -> TestAgent {
    let key = MasterKey::load(key_path).unwrap();
    let mut database = Database::open(database_path).unwrap();
    database.bind_master_key_id(&key.key_id_hex()).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let token = create_join_token(&mut database, id, now).unwrap();
    let volume = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: domain.to_owned(),
    };
    let volume_path = volume.path().to_owned();
    let agent_token = token.clone();
    let task = tokio::spawn(async move {
        serve_agent(listener, volume_path, identity, &agent_token)
            .await
            .unwrap();
    });
    let target = AgentTarget {
        id: id.to_owned(),
        failure_domain: domain.to_owned(),
        base_url: format!("http://{address}"),
    };
    join_agent(
        &mut database,
        &key,
        &target,
        &token,
        &LocalApi::new("/nonexistent/tailscaled.sock"),
    )
    .await
    .unwrap();
    TestAgent {
        target,
        volume,
        task,
    }
}

fn create_key(path: &Path) {
    MasterKey::create(path).unwrap();
}

/// Agent ids and failure domains for the legacy-schema tests, which record
/// placements before the agents can be joined to the migrated database.
const AGENTS: [(&str, &str); 2] = [("agent-a", "host-a"), ("agent-b", "host-b")];

/// Snapshot the portal database at `from` into `to`, as a metadata backup
/// does. Taken after the agents joined but before any upload, the copy holds
/// the signing key they trust and no file records: a stale SQLite that
/// recovery must rebuild from the agents (DESIGN §11).
fn stale_snapshot(from: &Path, to: &Path) {
    Database::open(from).unwrap().snapshot_into(to).unwrap();
}

#[tokio::test]
async fn every_delete_boundary_is_retryable_and_never_removes_objects() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    create_key(&key_path);
    let source = workspace.path().join("source.bin");
    fs::write(&source, b"delete payload").unwrap();

    for failpoint in [
        Failpoint::AfterDeletePlan,
        Failpoint::AfterFirstMarkerReplica,
        Failpoint::AfterMarkerDurable,
        Failpoint::BeforeDeleteCommit,
        Failpoint::AfterDeleteCommit,
    ] {
        let database_path = workspace
            .path()
            .join(format!("{}.sqlite", failpoint.as_str()));
        // Each failpoint gets a fresh portal database, and agents trust only
        // the portal that joined them, so each also gets a fresh pair.
        let agent_a = start_agent("agent-a", "host-a", &database_path, &key_path).await;
        let agent_b = start_agent("agent-b", "host-b", &database_path, &key_path).await;
        let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
        let mut portal = Portal::open(
            &database_path,
            MasterKey::load(&key_path).unwrap(),
            agents.clone(),
        )
        .unwrap();
        let file_id = portal
            .upload(&source, &format!("upload-{}", failpoint.as_str()))
            .await
            .unwrap();
        let database = Database::open(&database_path).unwrap();
        let projection = database.file_projection(file_id).unwrap().unwrap();
        let manifest_hash = projection.manifest_hash.unwrap();
        let upload = database
            .upload_by_idempotency(&format!("upload-{}", failpoint.as_str()))
            .unwrap()
            .unwrap();
        let chunk_hash = database
            .chunks(upload.upload_id)
            .unwrap()
            .remove(0)
            .ciphertext_hash
            .unwrap();
        drop(database);
        drop(portal);

        let mut portal = Portal::open(
            &database_path,
            MasterKey::load(&key_path).unwrap(),
            agents.clone(),
        )
        .unwrap()
        .with_failpoint(failpoint, FailpointAction::ReturnError);
        assert!(matches!(
            portal.delete(file_id, &format!("delete-{}", failpoint.as_str())).await,
            Err(PortalError::InjectedFailure(actual)) if actual == failpoint
        ));
        drop(portal);

        let database = Database::open(&database_path).unwrap();
        assert_eq!(
            database.file_by_id(file_id).unwrap().is_some(),
            failpoint != Failpoint::AfterDeleteCommit
        );
        drop(database);

        let mut portal = Portal::open(
            &database_path,
            MasterKey::load(&key_path).unwrap(),
            agents.clone(),
        )
        .unwrap();
        let key = format!("delete-{}", failpoint.as_str());
        let first = portal.delete(file_id, &key).await.unwrap();
        let second = portal.delete(file_id, &key).await.unwrap();
        assert_eq!(first.marker_hash, second.marker_hash);
        assert!(!portal.is_visible(file_id).unwrap());
        assert!(matches!(
            portal.delete(file_id, "new-delete-key").await,
            Err(PortalError::AlreadyDeleted(actual)) if actual == file_id
        ));
        assert!(DurableStore::open(agent_a.volume.path())
            .unwrap()
            .get(ObjectKind::Manifest, &manifest_hash)
            .is_ok());
        assert!(DurableStore::open(agent_b.volume.path())
            .unwrap()
            .get(ObjectKind::Manifest, &manifest_hash)
            .is_ok());
        for agent in [&agent_a, &agent_b] {
            assert!(DurableStore::open(agent.volume.path())
                .unwrap()
                .get(ObjectKind::Chunk, &chunk_hash)
                .is_ok());
        }
    }
}

#[tokio::test]
async fn delete_refuses_to_commit_when_confirmed_marker_replicas_are_gone() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    let source = workspace.path().join("source.bin");
    fs::write(&source, b"delete durability").unwrap();
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &database_path, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &database_path, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];

    let mut portal = Portal::open(
        &database_path,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let file_id = portal.upload(&source, "delete-source").await.unwrap();
    drop(portal);

    let mut portal = Portal::open(
        &database_path,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap()
    .with_failpoint(Failpoint::AfterMarkerDurable, FailpointAction::ReturnError);
    assert!(matches!(
        portal.delete(file_id, "delete-missing-marker").await,
        Err(PortalError::InjectedFailure(Failpoint::AfterMarkerDurable))
    ));
    drop(portal);

    let marker_hash = Database::open(&database_path)
        .unwrap()
        .delete_by_idempotency("delete-missing-marker")
        .unwrap()
        .unwrap()
        .marker_hash;
    for agent in [&agent_a, &agent_b] {
        fs::remove_file(
            DurableStore::open(agent.volume.path())
                .unwrap()
                .object_path(ObjectKind::DeletionMarker, &marker_hash),
        )
        .unwrap();
    }
    // A retry re-PUTs confirmed placements, which would heal agent B; make B
    // refuse the write so only its stale `confirmed` row remains. That row
    // must not satisfy the commit's live replica check.
    use std::os::unix::fs::PermissionsExt;
    let marker_parent = DurableStore::open(agent_b.volume.path())
        .unwrap()
        .object_path(ObjectKind::DeletionMarker, &marker_hash)
        .parent()
        .unwrap()
        .to_owned();
    fs::set_permissions(&marker_parent, fs::Permissions::from_mode(0o555)).unwrap();

    let mut portal =
        Portal::open(&database_path, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    let result = portal.delete(file_id, "delete-missing-marker").await;
    fs::set_permissions(&marker_parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            result,
            Err(PortalError::NoValidReplica {
                kind: ObjectKind::DeletionMarker,
                ..
            })
        ),
        "{result:?}"
    );
    assert!(portal.is_visible(file_id).unwrap());

    // With agent B writable again the same-key retry heals and commits.
    portal
        .delete(file_id, "delete-missing-marker")
        .await
        .unwrap();
    assert!(!portal.is_visible(file_id).unwrap());
}

#[tokio::test]
async fn recovery_plans_then_repairs_one_copy_and_rebuilds_stale_sqlite() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let original_db = workspace.path().join("original.sqlite");
    let recovered_db = workspace.path().join("recovered.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &original_db, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &original_db, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    stale_snapshot(&original_db, &recovered_db);

    let mut portal = Portal::open(
        &original_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let file_id = portal.upload(&source, "recover-upload").await.unwrap();
    let manifest_hash = Database::open(&original_db)
        .unwrap()
        .file_projection(file_id)
        .unwrap()
        .unwrap()
        .manifest_hash
        .unwrap();
    drop(portal);
    fs::write(
        DurableStore::open(agent_b.volume.path())
            .unwrap()
            .object_path(ObjectKind::Manifest, &manifest_hash),
        b"corrupt manifest replica",
    )
    .unwrap();

    let mut recovery = Portal::open(
        &recovered_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let plan = recovery.recover(false).await.unwrap();
    assert_eq!(plan.totals.repairs_planned, 1);
    assert!(Database::open(&recovered_db)
        .unwrap()
        .file_by_id(file_id)
        .unwrap()
        .is_none());

    let applied = recovery.recover(true).await.unwrap();
    assert_eq!(applied.totals.repairs_applied, 1);
    assert_eq!(applied.totals.blocked, 0);
    assert!(recovery.is_visible(file_id).unwrap());
    let destination = workspace.path().join("restored.bin");
    recovery.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), Vec::<u8>::new());

    let repeated = recovery.recover(true).await.unwrap();
    assert_eq!(repeated.totals.repairs_applied, 0);
    assert_eq!(
        repeated.inventory_digest,
        recovery.recover(false).await.unwrap().inventory_digest
    );
}

#[tokio::test]
async fn recovery_keeps_marker_only_tombstones_unreadable_and_blocks_conflicts() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let original_db = workspace.path().join("original.sqlite");
    let recovered_db = workspace.path().join("recovered.sqlite");
    let conflict_db = workspace.path().join("conflict.sqlite");
    let blank_db = workspace.path().join("blank.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &original_db, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &original_db, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    for database in [&recovered_db, &conflict_db, &blank_db] {
        stale_snapshot(&original_db, database);
    }

    let mut portal = Portal::open(
        &original_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let deleted_file = portal.upload(&source, "deleted-upload").await.unwrap();
    portal.delete(deleted_file, "delete-key").await.unwrap();
    drop(portal);

    let mut recovery = Portal::open(
        &recovered_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let report = recovery.recover(true).await.unwrap();
    assert_eq!(report.totals.blocked, 0);
    let projection = Database::open(&recovered_db)
        .unwrap()
        .file_projection(deleted_file)
        .unwrap()
        .unwrap();
    assert_eq!(projection.state, FileState::Deleted);
    assert!(!recovery.is_visible(deleted_file).unwrap());

    let mut original = Portal::open(
        &conflict_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let conflicted_file = original.upload(&source, "conflict-upload").await.unwrap();
    drop(original);
    let bytes = encode_deletion_marker(
        &MasterKey::load(&key_path).unwrap(),
        conflicted_file,
        1,
        123,
    )
    .unwrap();
    let hash = ObjectHash::digest(&bytes);
    for agent in [&agent_a, &agent_b] {
        DurableStore::open(agent.volume.path())
            .unwrap()
            .put(ObjectKind::DeletionMarker, &hash, &bytes)
            .unwrap();
    }
    let mut conflict_recovery =
        Portal::open(&blank_db, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    let report = conflict_recovery.recover(true).await.unwrap();
    assert!(report.totals.blocked >= 1);
    assert!(!conflict_recovery.is_visible(conflicted_file).unwrap());
}

#[tokio::test]
async fn wrong_master_key_aborts_recovery_without_mutation() {
    let workspace = tempfile::tempdir().unwrap();
    let right_path = workspace.path().join("right.key");
    let wrong_path = workspace.path().join("wrong.key");
    let source_db = workspace.path().join("source.sqlite");
    let destination_db = workspace.path().join("destination.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    create_key(&right_path);
    create_key(&wrong_path);
    let agent_a = start_agent("agent-a", "host-a", &source_db, &right_path).await;
    let agent_b = start_agent("agent-b", "host-b", &source_db, &right_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    let mut portal =
        Portal::open(&source_db, MasterKey::load(&right_path).unwrap(), agents).unwrap();
    let file_id = portal.upload(&source, "wrong-key-source").await.unwrap();
    drop(portal);
    let manifest_hash = Database::open(&source_db)
        .unwrap()
        .file_projection(file_id)
        .unwrap()
        .unwrap()
        .manifest_hash
        .unwrap();

    // Agents refuse grants from a portal that did not join them, which would
    // fail recovery before it reads any evidence. Give the wrong-key portal
    // agents of its own holding the same objects, as `restore objects` would,
    // so recovery must fail on the evidence itself.
    let recovering_a = start_agent("agent-a", "host-a", &destination_db, &wrong_path).await;
    let recovering_b = start_agent("agent-b", "host-b", &destination_db, &wrong_path).await;
    for (from, to) in [(&agent_a, &recovering_a), (&agent_b, &recovering_b)] {
        let bytes = DurableStore::open(from.volume.path())
            .unwrap()
            .get(ObjectKind::Manifest, &manifest_hash)
            .unwrap();
        DurableStore::open(to.volume.path())
            .unwrap()
            .put(ObjectKind::Manifest, &manifest_hash, &bytes)
            .unwrap();
    }
    let mut recovery = Portal::open(
        &destination_db,
        MasterKey::load(&wrong_path).unwrap(),
        vec![recovering_a.target.clone(), recovering_b.target.clone()],
    )
    .unwrap();
    let result = recovery.recover(true).await;
    assert!(matches!(result, Err(RecoveryError::Trust(_))), "{result:?}");
    assert!(Database::open(&destination_db)
        .unwrap()
        .all_file_projections()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn recovery_applies_safe_files_while_missing_chunks_block_only_their_file() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let source_db = workspace.path().join("source.sqlite");
    let recovered_db = workspace.path().join("recovered.sqlite");
    let good_source = workspace.path().join("good.bin");
    let blocked_source = workspace.path().join("blocked.bin");
    fs::write(&good_source, []).unwrap();
    fs::write(&blocked_source, b"required chunk").unwrap();
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &source_db, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &source_db, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    stale_snapshot(&source_db, &recovered_db);

    let mut portal = Portal::open(
        &source_db,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let good_file = portal.upload(&good_source, "good").await.unwrap();
    let blocked_file = portal.upload(&blocked_source, "blocked").await.unwrap();
    drop(portal);
    let database = Database::open(&source_db).unwrap();
    let upload = database.upload_by_idempotency("blocked").unwrap().unwrap();
    let chunk_hash = database
        .chunks(upload.upload_id)
        .unwrap()
        .remove(0)
        .ciphertext_hash
        .unwrap();
    drop(database);
    for agent in [&agent_a, &agent_b] {
        fs::remove_file(
            DurableStore::open(agent.volume.path())
                .unwrap()
                .object_path(ObjectKind::Chunk, &chunk_hash),
        )
        .unwrap();
    }

    let mut recovery =
        Portal::open(&recovered_db, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    let report = recovery.recover(true).await.unwrap();
    assert_eq!(report.totals.converged, 1);
    assert_eq!(report.totals.blocked, 1);
    assert!(recovery.is_visible(good_file).unwrap());
    assert!(!recovery.is_visible(blocked_file).unwrap());
    assert_eq!(
        Database::open(&recovered_db)
            .unwrap()
            .file_projection(blocked_file)
            .unwrap()
            .unwrap()
            .state,
        FileState::RecoveryBlocked
    );
}

#[tokio::test]
async fn malformed_or_unavailable_agent_inventory_aborts_without_mutation() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &database_path, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &database_path, &key_path).await;
    DurableStore::open(agent_a.volume.path()).unwrap();
    fs::create_dir(
        agent_a
            .volume
            .path()
            .join("objects/manifest/malformed-prefix"),
    )
    .unwrap();
    let mut portal = Portal::open(
        &database_path,
        MasterKey::load(&key_path).unwrap(),
        vec![agent_a.target.clone(), agent_b.target.clone()],
    )
    .unwrap();
    assert!(portal.recover(true).await.is_err());
    assert!(Database::open(&database_path)
        .unwrap()
        .all_file_projections()
        .unwrap()
        .is_empty());

    fs::remove_dir(
        agent_a
            .volume
            .path()
            .join("objects/manifest/malformed-prefix"),
    )
    .unwrap();
    let mut unavailable = agent_b.target.clone();
    unavailable.base_url = "http://127.0.0.1:9".to_owned();
    let mut portal = Portal::open(
        &database_path,
        MasterKey::load(&key_path).unwrap(),
        vec![agent_a.target.clone(), unavailable],
    )
    .unwrap();
    assert!(portal.recover(true).await.is_err());
    assert!(Database::open(&database_path)
        .unwrap()
        .all_file_projections()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn newer_manifest_wins_over_stale_sqlite_and_remains_downloadable() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("stale.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    create_key(&key_path);
    let agent_a = start_agent("agent-a", "host-a", &database_path, &key_path).await;
    let agent_b = start_agent("agent-b", "host-b", &database_path, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];

    let mut portal = Portal::open(
        &database_path,
        MasterKey::load(&key_path).unwrap(),
        agents.clone(),
    )
    .unwrap();
    let file_id = portal.upload(&source, "generation-one").await.unwrap();
    drop(portal);
    let generation_one_hash = Database::open(&database_path)
        .unwrap()
        .file_projection(file_id)
        .unwrap()
        .unwrap()
        .manifest_hash
        .unwrap();
    let generation_one_bytes = DurableStore::open(agent_a.volume.path())
        .unwrap()
        .get(ObjectKind::Manifest, &generation_one_hash)
        .unwrap();
    let master = MasterKey::load(&key_path).unwrap();
    let mut decoded = decode_manifest(&master, &generation_one_bytes).unwrap();
    let content_key = unwrap_key(
        &master,
        b"content",
        file_id,
        1,
        &decoded.payload.content_key,
    )
    .unwrap();
    decoded.payload.content_key = wrap_key(&master, b"content", file_id, 2, &content_key).unwrap();
    let generation_two_bytes = encode_manifest(&master, file_id, 2, &decoded.payload).unwrap();
    let generation_two_hash = ObjectHash::digest(&generation_two_bytes);
    for agent in [&agent_a, &agent_b] {
        DurableStore::open(agent.volume.path())
            .unwrap()
            .put(
                ObjectKind::Manifest,
                &generation_two_hash,
                &generation_two_bytes,
            )
            .unwrap();
    }

    let mut recovery =
        Portal::open(&database_path, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    let report = recovery.recover(true).await.unwrap();
    assert_eq!(report.totals.blocked, 0);
    assert_eq!(
        Database::open(&database_path)
            .unwrap()
            .file_projection(file_id)
            .unwrap()
            .unwrap()
            .generation,
        2
    );
    let destination = workspace.path().join("generation-two.bin");
    recovery.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), Vec::<u8>::new());
}

#[tokio::test]
async fn migrated_v1_upload_remains_downloadable_and_idempotent() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("legacy.sqlite");
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();
    create_key(&key_path);
    let master = MasterKey::load(&key_path).unwrap();
    let file_id = uuid::Uuid::new_v4();
    let upload_id = uuid::Uuid::new_v4();
    let content_key = random_key();
    let wrapped_content_key = wrap_key(&master, b"content", file_id, 1, &content_key).unwrap();
    let plaintext_hash = *blake3::hash(b"").as_bytes();
    let payload = distr_hnsw::format::ManifestPayload {
        name: "empty.bin".to_owned(),
        plaintext_len: 0,
        plaintext_hash,
        content_key: wrapped_content_key.clone(),
        chunks: Vec::new(),
    };
    let manifest_bytes = encode_manifest(&master, file_id, 1, &payload).unwrap();
    let manifest_hash = ObjectHash::digest(&manifest_bytes);

    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE uploads (upload_id TEXT PRIMARY KEY, idempotency_key TEXT UNIQUE,
             request_fingerprint BLOB, file_id TEXT UNIQUE, file_name TEXT, plaintext_hash BLOB,
             plaintext_size INTEGER, storage_class TEXT, content_key_nonce BLOB,
             wrapped_content_key BLOB, state TEXT, generation INTEGER, manifest_hash TEXT,
             manifest_bytes BLOB, created_at INTEGER DEFAULT (unixepoch()));
             CREATE TABLE upload_chunks (upload_id TEXT, ordinal INTEGER, plaintext_len INTEGER,
             plaintext_hash BLOB, nonce BLOB, ciphertext_hash TEXT, ciphertext_len INTEGER,
             PRIMARY KEY(upload_id, ordinal));
             CREATE TABLE placements (object_kind TEXT, object_hash TEXT, agent_id TEXT,
             failure_domain TEXT, state TEXT, confirmed_at INTEGER,
             PRIMARY KEY(object_kind, object_hash, agent_id));
             CREATE TABLE files (file_id TEXT PRIMARY KEY, generation INTEGER, name TEXT,
             plaintext_hash BLOB, plaintext_size INTEGER, manifest_hash TEXT, upload_id TEXT UNIQUE,
             state TEXT, created_at INTEGER DEFAULT (unixepoch()));
             PRAGMA user_version = 1;",
        )
        .unwrap();
    let mut fingerprint = blake3::Hasher::new();
    fingerprint.update(b"distr-hnsw:idempotency:v1");
    fingerprint.update(&plaintext_hash);
    fingerprint.update(&0_u64.to_le_bytes());
    fingerprint.update(&("empty.bin".len() as u64).to_le_bytes());
    fingerprint.update(b"empty.bin");
    fingerprint.update(&("regular-rf2".len() as u64).to_le_bytes());
    fingerprint.update(b"regular-rf2");
    connection
        .execute(
            "INSERT INTO uploads VALUES (?1, 'legacy-key', ?2, ?3, 'empty.bin', ?4, 0,
             'regular-rf2', ?5, ?6, 'committed', 1, ?7, ?8, unixepoch())",
            params![
                upload_id.to_string(),
                fingerprint.finalize().as_bytes().as_slice(),
                file_id.to_string(),
                plaintext_hash.as_slice(),
                wrapped_content_key.nonce.as_slice(),
                wrapped_content_key.ciphertext,
                manifest_hash.as_str(),
                manifest_bytes,
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO files VALUES (?1, 1, 'empty.bin', ?2, 0, ?3, ?4, 'committed', unixepoch())",
            params![
                file_id.to_string(),
                plaintext_hash.as_slice(),
                manifest_hash.as_str(),
                upload_id.to_string(),
            ],
        )
        .unwrap();
    for (agent_id, domain) in AGENTS {
        connection
            .execute(
                "INSERT INTO placements VALUES ('manifest', ?1, ?2, ?3, 'confirmed', unixepoch())",
                params![manifest_hash.as_str(), agent_id, domain],
            )
            .unwrap();
    }
    drop(connection);

    // Joining opens, and so migrates, the legacy database.
    let agent_a = start_agent(AGENTS[0].0, AGENTS[0].1, &database_path, &key_path).await;
    let agent_b = start_agent(AGENTS[1].0, AGENTS[1].1, &database_path, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    for agent in [&agent_a, &agent_b] {
        DurableStore::open(agent.volume.path())
            .unwrap()
            .put(ObjectKind::Manifest, &manifest_hash, &manifest_bytes)
            .unwrap();
    }

    let mut portal =
        Portal::open(&database_path, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    assert_eq!(portal.upload(&source, "legacy-key").await.unwrap(), file_id);
    let destination = workspace.path().join("legacy-download.bin");
    portal.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), Vec::<u8>::new());
}

#[tokio::test]
async fn audited_v2_chunk_remains_downloadable_after_v3_migration() {
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("audited-v2.sqlite");
    create_key(&key_path);
    let master = MasterKey::load(&key_path).unwrap();
    let file_id = uuid::Uuid::new_v4();
    let upload_id = uuid::Uuid::new_v4();
    let content_key = random_key();
    let nonce = random_nonce();
    let plaintext = b"audited v2 chunk payload";
    let plaintext_hash = *blake3::hash(plaintext).as_bytes();
    let ciphertext = encrypt_chunk(
        &content_key,
        ENVELOPE_VERSION,
        file_id,
        0,
        plaintext.len() as u32,
        &nonce,
        plaintext,
    )
    .unwrap();
    let chunk_hash = ObjectHash::digest(&ciphertext);
    let wrapped_content_key = wrap_key(&master, b"content", file_id, 1, &content_key).unwrap();
    let payload = ManifestPayload {
        name: "legacy.bin".to_owned(),
        plaintext_len: plaintext.len() as u64,
        plaintext_hash,
        content_key: wrapped_content_key.clone(),
        chunks: vec![ChunkRecord {
            ordinal: 0,
            plaintext_len: plaintext.len() as u32,
            nonce,
            ciphertext_hash: chunk_hash.clone(),
            ciphertext_len: ciphertext.len() as u32,
        }],
    };
    let manifest_bytes = encode_manifest(&master, file_id, 1, &payload).unwrap();
    let manifest_hash = ObjectHash::digest(&manifest_bytes);

    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE uploads (
                upload_id TEXT PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE,
                request_fingerprint BLOB NOT NULL, file_id TEXT NOT NULL UNIQUE,
                file_name TEXT NOT NULL, plaintext_hash BLOB NOT NULL,
                plaintext_size INTEGER NOT NULL, storage_class TEXT NOT NULL,
                content_key_nonce BLOB NOT NULL, wrapped_content_key BLOB NOT NULL,
                state TEXT NOT NULL, generation INTEGER NOT NULL, manifest_hash TEXT,
                manifest_bytes BLOB, created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE upload_chunks (
                upload_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
                envelope_version INTEGER NOT NULL, plaintext_len INTEGER NOT NULL,
                plaintext_hash BLOB NOT NULL, nonce BLOB NOT NULL,
                ciphertext_hash TEXT, ciphertext_len INTEGER,
                PRIMARY KEY(upload_id, ordinal)
             );
             CREATE TABLE placements (
                object_kind TEXT NOT NULL, object_hash TEXT NOT NULL,
                agent_id TEXT NOT NULL, failure_domain TEXT NOT NULL,
                state TEXT NOT NULL, confirmed_at INTEGER,
                PRIMARY KEY(object_kind, object_hash, agent_id)
             );
             CREATE TABLE files (
                file_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, name TEXT NOT NULL,
                plaintext_hash BLOB NOT NULL, plaintext_size INTEGER NOT NULL,
                manifest_hash TEXT NOT NULL, upload_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             PRAGMA user_version = 2;",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO uploads
             (upload_id, idempotency_key, request_fingerprint, file_id, file_name,
              plaintext_hash, plaintext_size, storage_class, content_key_nonce,
              wrapped_content_key, state, generation, manifest_hash, manifest_bytes)
             VALUES (?1, 'audited-v2-key', ?2, ?3, 'legacy.bin', ?4, ?5,
                     'regular-rf2', ?6, ?7, 'committed', 1, ?8, ?9)",
            params![
                upload_id.to_string(),
                [1_u8; 32].as_slice(),
                file_id.to_string(),
                plaintext_hash.as_slice(),
                plaintext.len() as u64,
                wrapped_content_key.nonce.as_slice(),
                wrapped_content_key.ciphertext,
                manifest_hash.as_str(),
                manifest_bytes,
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO upload_chunks
             (upload_id, ordinal, envelope_version, plaintext_len, plaintext_hash,
              nonce, ciphertext_hash, ciphertext_len)
             VALUES (?1, 0, 1, ?2, ?3, ?4, ?5, ?6)",
            params![
                upload_id.to_string(),
                plaintext.len() as u32,
                plaintext_hash.as_slice(),
                nonce.as_slice(),
                chunk_hash.as_str(),
                ciphertext.len() as u32,
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO files
             (file_id, generation, name, plaintext_hash, plaintext_size,
              manifest_hash, upload_id, state)
             VALUES (?1, 1, 'legacy.bin', ?2, ?3, ?4, ?5, 'committed')",
            params![
                file_id.to_string(),
                plaintext_hash.as_slice(),
                plaintext.len() as u64,
                manifest_hash.as_str(),
                upload_id.to_string(),
            ],
        )
        .unwrap();
    for (agent_id, domain) in AGENTS {
        for (kind, hash) in [
            (ObjectKind::Chunk, &chunk_hash),
            (ObjectKind::Manifest, &manifest_hash),
        ] {
            connection
                .execute(
                    "INSERT INTO placements
                     (object_kind, object_hash, agent_id, failure_domain, state, confirmed_at)
                     VALUES (?1, ?2, ?3, ?4, 'confirmed', unixepoch())",
                    params![kind.as_str(), hash.as_str(), agent_id, domain],
                )
                .unwrap();
        }
    }
    drop(connection);

    // Joining opens, and so migrates, the legacy database.
    let agent_a = start_agent(AGENTS[0].0, AGENTS[0].1, &database_path, &key_path).await;
    let agent_b = start_agent(AGENTS[1].0, AGENTS[1].1, &database_path, &key_path).await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    for agent in [&agent_a, &agent_b] {
        let store = DurableStore::open(agent.volume.path()).unwrap();
        store
            .put(ObjectKind::Chunk, &chunk_hash, &ciphertext)
            .unwrap();
        store
            .put(ObjectKind::Manifest, &manifest_hash, &manifest_bytes)
            .unwrap();
    }

    let mut portal =
        Portal::open(&database_path, MasterKey::load(&key_path).unwrap(), agents).unwrap();
    let destination = workspace.path().join("legacy.download");
    portal.download(file_id, &destination).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), plaintext);
    assert_eq!(
        portal
            .upload(&workspace.path().join("missing-source"), "audited-v2-key")
            .await
            .unwrap(),
        file_id
    );
}

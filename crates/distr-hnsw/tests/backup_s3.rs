//! S3-compatible backup target against a live endpoint (MinIO through
//! `scripts/minio-test.sh`). Every test returns early with a notice unless
//! `DISTR_HNSW_S3_TEST_ENDPOINT` is set, because the suite needs a running
//! server and the right to create buckets. Credentials come from
//! `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` like the product code.

use std::{fs, path::PathBuf};

use aws_sdk_s3::{
    types::{BucketVersioningStatus, VersioningConfiguration},
    Client,
};
use distr_hnsw::{
    agent::{serve_agent, AgentIdentity},
    backup::{self, s3, BackupError, BackupTargetSpec, PutOutcome, S3Target},
    crypto::MasterKey,
    metadata::Database,
    portal::{AgentTarget, Portal},
    CHUNK_SIZE,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

const TEST_ENDPOINT_ENV: &str = "DISTR_HNSW_S3_TEST_ENDPOINT";

/// The live client, or `None` after printing why the test is skipped.
fn live_client() -> Option<Client> {
    let Ok(endpoint) = std::env::var(TEST_ENDPOINT_ENV) else {
        eprintln!("skipping: {TEST_ENDPOINT_ENV} is not set (see scripts/minio-test.sh)");
        return None;
    };
    // The product code reads its endpoint from this variable; every test in
    // this binary sets the same value, so the race between test threads is
    // benign.
    std::env::set_var(s3::ENDPOINT_ENV, endpoint);
    Some(s3::client_from_env().expect("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are set"))
}

async fn create_bucket(client: &Client, object_lock: bool) -> String {
    let name = format!("distr-hnsw-test-{}", uuid::Uuid::new_v4().simple());
    client
        .create_bucket()
        .bucket(&name)
        .object_lock_enabled_for_bucket(object_lock)
        .send()
        .await
        .unwrap();
    name
}

async fn enable_versioning(client: &Client, bucket: &str) {
    client
        .put_bucket_versioning()
        .bucket(bucket)
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .unwrap();
}

struct TestAgent {
    target: AgentTarget,
    _holder: TempDir,
    task: JoinHandle<()>,
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_agent(id: &str, domain: &str) -> TestAgent {
    let holder = tempfile::tempdir().unwrap();
    let volume: PathBuf = holder.path().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let identity = AgentIdentity {
        id: id.to_owned(),
        failure_domain: domain.to_owned(),
    };
    let task = tokio::spawn(async move {
        serve_agent(listener, volume, identity).await.unwrap();
    });
    TestAgent {
        target: AgentTarget {
            id: id.to_owned(),
            failure_domain: domain.to_owned(),
            base_url: format!("http://{address}"),
        },
        _holder: holder,
        task,
    }
}

#[tokio::test]
async fn s3_target_never_overwrites_and_lists_under_its_prefix() {
    let Some(client) = live_client() else {
        return;
    };
    let bucket = create_bucket(&client, true).await;
    let target = S3Target::open(client.clone(), &bucket, "/site/a/")
        .await
        .unwrap()
        .with_list_page_size(2);
    assert_eq!(target.id(), format!("s3:{bucket}/site/a"));
    let capabilities = target.capabilities();
    assert!(capabilities.versioning_enabled);
    assert!(capabilities.object_lock_enabled, "{capabilities:?}");
    assert!(capabilities.default_retention.is_none());

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
    let conflict = target
        .put_new("backup/v1/objects/chunk/aa", b"two")
        .await
        .unwrap_err();
    assert!(matches!(conflict, BackupError::Conflict(_)), "{conflict}");
    assert_eq!(
        target.get("backup/v1/objects/chunk/aa").await.unwrap(),
        Some(b"one".to_vec()),
        "the conflicting write left the original bytes in place"
    );
    let head = client
        .head_object()
        .bucket(&bucket)
        .key("site/a/backup/v1/objects/chunk/aa")
        .send()
        .await
        .unwrap();
    assert_eq!(head.content_length(), Some(3), "keys carry the prefix");

    for name in ["ab", "ac", "ad", "ae"] {
        target
            .put_new(&format!("backup/v1/objects/chunk/{name}"), b"three")
            .await
            .unwrap();
    }
    target
        .put_new("backup/v1/sqlite/x.db", b"db")
        .await
        .unwrap();
    // A neighbour outside the prefix and a sibling of the listed directory
    // stay invisible.
    client
        .put_object()
        .bucket(&bucket)
        .key("other/backup/v1/objects/chunk/zz")
        .body(b"zz".to_vec().into())
        .send()
        .await
        .unwrap();
    target
        .put_new("backup/v1/objects-extra/chunk/zz", b"zz")
        .await
        .unwrap();
    // Five keys with a page size of two takes three pages.
    assert_eq!(
        target.list("backup/v1/objects").await.unwrap(),
        vec![
            ("backup/v1/objects/chunk/aa".to_owned(), 3),
            ("backup/v1/objects/chunk/ab".to_owned(), 5),
            ("backup/v1/objects/chunk/ac".to_owned(), 5),
            ("backup/v1/objects/chunk/ad".to_owned(), 5),
            ("backup/v1/objects/chunk/ae".to_owned(), 5),
        ]
    );
    assert_eq!(
        target.list("backup/v1/sqlite").await.unwrap(),
        vec![("backup/v1/sqlite/x.db".to_owned(), 2)]
    );
    assert!(target.list("backup/v1/catalog").await.unwrap().is_empty());
    assert!(target.get("backup/v1/missing").await.unwrap().is_none());
    assert!(matches!(
        target.put_new("../escape", b"x").await,
        Err(BackupError::InvalidPath(_))
    ));
    assert!(matches!(
        target.get("/abs").await,
        Err(BackupError::InvalidPath(_))
    ));
}

#[tokio::test]
async fn unversioned_bucket_is_refused_and_missing_object_lock_only_warns() {
    let Some(client) = live_client() else {
        return;
    };
    let plain = create_bucket(&client, false).await;
    let error = S3Target::open(client.clone(), &plain, "")
        .await
        .err()
        .expect("unversioned bucket must be refused");
    assert!(
        matches!(&error, BackupError::TargetUnversioned(bucket) if *bucket == plain),
        "{error}"
    );
    let spec = BackupTargetSpec::S3 {
        bucket: plain.clone(),
        prefix: String::new(),
    };
    assert!(matches!(
        spec.open().await,
        Err(BackupError::TargetUnversioned(_))
    ));

    enable_versioning(&client, &plain).await;
    let target = S3Target::open(client, &plain, "").await.unwrap();
    assert_eq!(target.id(), format!("s3:{plain}"));
    let capabilities = target.capabilities();
    assert!(capabilities.versioning_enabled);
    assert!(!capabilities.object_lock_enabled);
    assert!(
        capabilities
            .warnings
            .iter()
            .any(|warning| warning.contains("Object Lock")),
        "{capabilities:?}"
    );
}

/// The directory drill (`commit_spine_process.rs`) through the S3 adapter:
/// back up twice, then rebuild metadata and objects into fresh agents and let
/// recovery converge.
#[tokio::test]
async fn backup_and_restore_round_trip_through_s3() {
    let Some(client) = live_client() else {
        return;
    };
    let bucket = create_bucket(&client, true).await;
    let spec: BackupTargetSpec = format!("s3:{bucket}/offsite").parse().unwrap();
    assert_eq!(spec.id(), format!("s3:{bucket}/offsite"));

    let agent_a = start_agent("agent-a", "host-a").await;
    let agent_b = start_agent("agent-b", "host-b").await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    let database_path = workspace.path().join("portal.sqlite");
    let kept_source = workspace.path().join("kept.bin");
    let kept_bytes: Vec<u8> = (0..CHUNK_SIZE + 4096)
        .map(|index| ((index * 7 + 3) % 251) as u8)
        .collect();
    fs::write(&kept_source, &kept_bytes).unwrap();
    let deleted_source = workspace.path().join("deleted.bin");
    fs::write(&deleted_source, vec![9_u8; 2048]).unwrap();
    let (kept_id, deleted_id) = {
        let key = MasterKey::create(&key_path).unwrap();
        let mut portal = Portal::open(&database_path, key, agents.clone()).unwrap();
        let kept_id = portal.upload(&kept_source, "s3-kept").await.unwrap();
        let deleted_id = portal.upload(&deleted_source, "s3-deleted").await.unwrap();
        portal.delete(deleted_id, "s3-delete").await.unwrap();
        (kept_id, deleted_id)
    };

    let http = reqwest::Client::new();
    let mut database = Database::open(&database_path).unwrap();
    let first = backup::backup(&mut database, &agents, &http, &spec, &database_path)
        .await
        .unwrap();
    assert_eq!(first.exit_code(), 0, "{first:?}");
    assert_eq!(first.target_id, spec.id());
    assert_eq!(
        first.totals.history_objects, 6,
        "two manifests, three chunks, one marker"
    );
    assert_eq!(first.totals.copied_now, 6);
    assert_eq!(first.totals.identical_on_target, 0);
    assert_eq!(first.totals.failed, 0);
    assert!(!first.snapshot_skipped_identical);
    let snapshot = first.snapshot.clone().expect("snapshot shipped");

    let second = backup::backup(&mut database, &agents, &http, &spec, &database_path)
        .await
        .unwrap();
    assert_eq!(second.exit_code(), 0, "{second:?}");
    assert_eq!(second.totals.verified_before, 6);
    assert_eq!(second.totals.copied_now, 0);
    assert!(second.snapshot_skipped_identical);
    assert_eq!(second.snapshot, first.snapshot);

    let status = backup::backup_status(&database).unwrap();
    assert_eq!(status.targets.len(), 1);
    assert_eq!(status.targets[0].target_id, spec.id());
    assert_eq!(status.targets[0].objects_verified, 6);
    assert_eq!(status.targets[0].objects_pending, 0);
    assert!(!status.recovery_ready);

    let listed = spec.open().await.unwrap();
    assert_eq!(listed.list("backup/v1/objects").await.unwrap().len(), 6);
    let raw = client
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("offsite/backup/v1/")
        .send()
        .await
        .unwrap();
    assert_eq!(
        raw.key_count(),
        Some(6 + 1 + 2),
        "objects, one snapshot, two catalogs, all under the prefix"
    );

    // Destroy the cluster: agents and their volumes go away; only the backup
    // set and the key remain (the bundle ceremony is drilled elsewhere).
    drop(agent_a);
    drop(agent_b);
    let restored = workspace.path().join("restored");
    let new_database = restored.join("portal.sqlite");
    let metadata = backup::restore_metadata(&spec, &new_database, None)
        .await
        .unwrap();
    assert_eq!(metadata.snapshot, snapshot);
    assert!(metadata.catalog.is_some(), "{metadata:?}");
    assert!(matches!(
        backup::restore_metadata(&spec, &new_database, None).await,
        Err(BackupError::DestinationExists(_))
    ));

    let agent_c = start_agent("agent-a", "host-a").await;
    let agent_d = start_agent("agent-b", "host-b").await;
    let new_agents = vec![agent_c.target.clone(), agent_d.target.clone()];
    let objects = backup::restore_objects(&spec, &new_agents, &http)
        .await
        .unwrap();
    assert_eq!(objects.exit_code(), 0, "{objects:?}");
    assert_eq!(objects.objects, 6);
    assert_eq!(objects.placed, 12);
    assert_eq!(objects.already_present, 0);
    let again = backup::restore_objects(&spec, &new_agents, &http)
        .await
        .unwrap();
    assert_eq!(again.placed, 0);
    assert_eq!(again.already_present, 12);

    let key = MasterKey::load(&key_path).unwrap();
    let mut portal = Portal::open(&new_database, key, new_agents).unwrap();
    let recovery = portal.recover(true).await.unwrap();
    assert_eq!(recovery.exit_code(), 0, "{recovery:?}");
    assert_eq!(recovery.totals.files, 2);
    assert_eq!(recovery.totals.blocked, 0);
    let destination = restored.join("kept.bin");
    portal.download(kept_id, &destination).await.unwrap();
    assert_eq!(fs::read(&destination).unwrap(), kept_bytes);
    assert!(portal
        .download(deleted_id, &restored.join("deleted.bin"))
        .await
        .is_err());
}

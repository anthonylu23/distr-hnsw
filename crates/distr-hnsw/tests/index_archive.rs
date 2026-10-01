//! M3 pass 7: a vector partition's manifest, newest snapshot, and WAL
//! segments round-trip through the blob plane and recover to the same
//! logical state on another machine.

use distr_hnsw::{
    agent::{serve_agent, AgentIdentity},
    crypto::MasterKey,
    index_archive::{archive, restore, ArchivePart, IndexArchiveError},
    portal::{AgentTarget, Portal},
};
use distr_hnsw_index::{
    hnsw::HnswParams,
    partition::{Partition, PartitionConfig},
    Metric,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

struct TestAgent {
    target: AgentTarget,
    _volume: TempDir,
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
        _volume: volume,
        task,
    }
}

fn vector(i: u32, dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|d| ((i as f32) * 0.37 + d as f32 * 1.3).sin())
        .collect()
}

fn op(n: u64) -> [u8; 16] {
    let mut id = [0_u8; 16];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id
}

fn state(partition: &Partition, dims: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for q in 0..5_u32 {
        for hit in partition
            .search(&vector(q * 7, dims), 10, 100, None)
            .unwrap()
        {
            out.push((hit.key, hit.payload));
        }
    }
    out
}

#[tokio::test]
async fn partition_state_round_trips_through_the_blob_plane() {
    let agent_a = start_agent("agent-a", "domain-a").await;
    let agent_b = start_agent("agent-b", "domain-b").await;
    let agents = vec![agent_a.target.clone(), agent_b.target.clone()];
    let workspace = tempfile::tempdir().unwrap();
    let key_path = workspace.path().join("master.key");
    MasterKey::create(&key_path).unwrap();
    let database_path = workspace.path().join("portal.sqlite");

    let dims = 16;
    let partition_dir = workspace.path().join("partition");
    let partition_id = Uuid::new_v4();
    let mut partition = Partition::create(
        &partition_dir,
        PartitionConfig::new(partition_id, dims, Metric::L2, HnswParams::default()),
    )
    .unwrap();
    for i in 0..300_u32 {
        partition
            .upsert(
                op(i as u64),
                format!("key-{i}").as_bytes(),
                &vector(i, dims),
                format!("payload-{i}").as_bytes(),
            )
            .unwrap();
    }
    partition.snapshot().unwrap();
    // Entries after the snapshot live only in the WAL and must be archived too.
    for i in 300..340_u32 {
        partition
            .upsert(
                op(i as u64),
                format!("key-{i}").as_bytes(),
                &vector(i, dims),
                format!("payload-{i}").as_bytes(),
            )
            .unwrap();
    }
    partition.delete(op(1_000), b"key-5").unwrap();
    let expected = state(&partition, dims);
    let expected_high_water = partition.high_water();
    drop(partition);

    let key = MasterKey::load(&key_path).unwrap();
    let mut portal = Portal::open(&database_path, key, agents.clone()).unwrap();
    let first = archive(&mut portal, &partition_dir, false).await.unwrap();
    assert_eq!(first.unchanged, 0);
    assert_eq!(first.uploaded, first.files.len());
    assert!(first
        .files
        .iter()
        .any(|f| f.part == ArchivePart::Manifest && f.name == "partition.json"));
    assert_eq!(
        first
            .files
            .iter()
            .filter(|f| f.part == ArchivePart::Snapshot)
            .count(),
        1
    );
    assert!(first
        .files
        .iter()
        .any(|f| f.part == ArchivePart::WalSegment));

    // Re-archiving an unchanged partition uploads nothing.
    let second = archive(&mut portal, &partition_dir, false).await.unwrap();
    assert_eq!(second.uploaded, 0);
    assert_eq!(second.unchanged, first.files.len());

    // Restore into an empty directory and compare logical state.
    let destination = workspace.path().join("restored");
    let report = restore(&portal, partition_id, &destination).await.unwrap();
    assert_eq!(report.partition_id, partition_id);
    assert_eq!(report.recovery_high_water, expected_high_water);
    assert_eq!(report.snapshots_rejected, 0);
    assert!(report.wal_entries_replayed >= 41);
    let (restored, _) = Partition::open(&destination).unwrap();
    assert_eq!(state(&restored, dims), expected);
    assert!(restored
        .search(&vector(5, dims), 1, 100, None)
        .unwrap()
        .iter()
        .all(|hit| hit.key != b"key-5"));

    // A second restore into the same, now non-empty, directory is refused.
    assert!(matches!(
        restore(&portal, partition_id, &destination).await,
        Err(IndexArchiveError::DestinationNotEmpty(_))
    ));
    assert!(matches!(
        restore(&portal, Uuid::new_v4(), &workspace.path().join("none")).await,
        Err(IndexArchiveError::NothingArchived(_))
    ));
}

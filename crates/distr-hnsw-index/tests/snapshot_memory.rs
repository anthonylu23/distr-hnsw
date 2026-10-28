//! Writing a snapshot streams it to the file. An earlier writer built the
//! whole file in memory several times over, which at 1M × 512 needed more RAM
//! than the partition itself and was killed by the OOM killer. This binary
//! counts heap allocations, so it holds exactly one test.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use distr_hnsw_index::{
    hnsw::HnswParams,
    partition::{Partition, PartitionConfig},
    vector::FlatVectors,
    Metric,
};
use uuid::Uuid;

struct PeakTracking;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for PeakTracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOCATOR: PeakTracking = PeakTracking;

fn vector(seed: u64, dims: usize) -> Vec<f32> {
    let mut state = seed.wrapping_add(1);
    (0..dims)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        })
        .collect()
}

#[test]
fn snapshot_write_holds_no_copy_of_the_partition() {
    let (rows, dims) = (20_000, 64);
    let directory = tempfile::tempdir().unwrap();
    let mut data = Vec::with_capacity(rows * dims);
    for row in 0..rows {
        data.extend(vector(row as u64, dims));
    }
    let vectors = FlatVectors::from_vec(dims, data, Metric::Cosine).unwrap();
    // A small graph: this test is about the writer, not search quality.
    let params = HnswParams {
        m: 4,
        m0: 8,
        ef_construction: 16,
        seed: 1,
    };
    let config = PartitionConfig::new(Uuid::new_v4(), dims, Metric::Cosine, params);
    let (mut partition, _) = Partition::bulk_load(
        directory.path(),
        config,
        vectors,
        (0..rows as u64).map(|i| i.to_le_bytes().to_vec()),
    )
    .unwrap();
    // Rows on the heap tail as well as in the mapped base.
    for row in 0..100_u64 {
        let mut op = [0_u8; 16];
        op[..8].copy_from_slice(&row.to_le_bytes());
        partition
            .upsert(
                op,
                format!("tail-{row}").as_bytes(),
                &vector(rows as u64 + row, dims),
                b"{}",
            )
            .unwrap();
    }

    let before = CURRENT.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let path = partition.snapshot().unwrap();
    let growth = PEAK.load(Ordering::SeqCst) - before;
    let file_bytes = fs::metadata(&path).unwrap().len() as usize;

    // Scratch is two 1 MiB write buffers plus the key and idempotency
    // listings; an in-memory encoding would need several times the file.
    assert!(
        growth < file_bytes / 2,
        "snapshot write grew the heap by {growth} bytes for a {file_bytes}-byte file"
    );
    drop(partition);
    let (reopened, _) = Partition::open(directory.path()).unwrap();
    assert_eq!(reopened.live_count(), rows + 100);
}

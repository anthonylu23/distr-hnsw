//! A single partition: the state machine that ties keys, slots, versions,
//! the idempotency window, the WAL, snapshots, and recovery together
//! (`docs/m3-engine-contract.md` §1, §3, §4, §5, §6, §12).
//!
//! Every mutation is one WAL entry: appended, synced, then applied, and only
//! then acknowledged. Recovery loads the newest verifiable snapshot and
//! replays the WAL tail exactly once in order. An open partition holds an
//! exclusive lock on its directory.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    distance::normalize,
    fsutil::{self, PartitionLock},
    hnsw::{default_ef_search, Hnsw, HnswParams, SearchMode, Slot},
    quant::QuantizedVectors,
    snapshot::{self, LoadedSnapshot, SlotRecord, SnapshotError, SnapshotInput},
    vector::VectorError,
    wal::{self, Entry, Operation, WalError, WalWriter},
    Metric,
};

const MANIFEST_NAME: &str = "partition.json";
const WAL_DIR: &str = "wal";
const SNAP_DIR: &str = "snap";
pub const DEFAULT_IDEMPOTENCY_WINDOW: usize = 1_000_000;
/// Largest `k` a search accepts (contract §3).
pub const MAX_K: usize = 10_000;
/// Largest `ef_search` or `ef_construction` a partition accepts.
pub const MAX_EF: usize = 100_000;

/// Static configuration persisted in `partition.json` at creation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartitionConfig {
    pub id: Uuid,
    pub dims: usize,
    pub metric: String,
    pub m: usize,
    pub m0: usize,
    pub ef_construction: usize,
    pub seed: u64,
    pub idempotency_window: usize,
    /// RAM budget for the hot set (contract §11); `None` = unlimited.
    #[serde(default)]
    pub ram_budget_bytes: Option<u64>,
    /// Fraction of the budget reserved for compaction and recovery headroom.
    #[serde(default = "default_headroom")]
    pub headroom_fraction: f64,
    /// Tombstone ratio at which compaction is recommended (contract §10).
    #[serde(default = "default_compaction_ratio")]
    pub compaction_tombstone_ratio: f64,
    /// Per-collection `ef_search` default; `None` = the metric's measured
    /// default ([`default_ef_search`]).
    #[serde(default)]
    pub ef_search: Option<usize>,
    /// History begins with a snapshot that is not in the WAL (a bulk load).
    /// Such a partition refuses to open without a verifiable snapshot, since
    /// the WAL alone cannot reproduce it (contract §6).
    #[serde(default)]
    pub base_snapshot: bool,
}

fn default_headroom() -> f64 {
    0.35
}

fn default_compaction_ratio() -> f64 {
    0.2
}

impl PartitionConfig {
    pub fn new(id: Uuid, dims: usize, metric: Metric, params: HnswParams) -> Self {
        Self {
            id,
            dims,
            metric: metric.as_str().to_owned(),
            m: params.m,
            m0: params.m0,
            ef_construction: params.ef_construction,
            seed: params.seed,
            idempotency_window: DEFAULT_IDEMPOTENCY_WINDOW,
            ram_budget_bytes: None,
            headroom_fraction: default_headroom(),
            compaction_tombstone_ratio: default_compaction_ratio(),
            ef_search: None,
            base_snapshot: false,
        }
    }

    /// The measured per-metric defaults (contract §8): `M` and `ef_search`
    /// from [`HnswParams::for_metric`] and [`default_ef_search`], with the
    /// level seed derived from the partition id so partitions do not share
    /// level sequences.
    pub fn for_metric(id: Uuid, dims: usize, metric: Metric) -> Self {
        let mut params = HnswParams::for_metric(metric);
        params.seed = u64::from_le_bytes(id.as_bytes()[..8].try_into().expect("8"));
        Self::new(id, dims, metric, params)
    }

    /// The `ef_search` used when a query does not choose one.
    pub fn ef_search(&self) -> usize {
        self.ef_search.unwrap_or_else(|| {
            self.metric()
                .map(default_ef_search)
                .unwrap_or(default_ef_search(Metric::L2))
        })
    }

    /// Reject configurations the formats or the graph cannot represent:
    /// list lengths are stored as `u8`, the WAL caps dims, and `M` must be at
    /// least 2 for the level distribution to be finite.
    pub fn validate(&self) -> Result<(), PartitionError> {
        self.metric()?;
        if self.dims == 0 || self.dims > wal::MAX_DIMS {
            return Err(PartitionError::Manifest("dims must be between 1 and 65536"));
        }
        if !(2..=255).contains(&self.m) {
            return Err(PartitionError::Manifest("m must be between 2 and 255"));
        }
        if self.m0 < self.m || self.m0 > 255 {
            return Err(PartitionError::Manifest("m0 must be between m and 255"));
        }
        if self.ef_construction == 0 || self.ef_construction > MAX_EF {
            return Err(PartitionError::Manifest(
                "ef_construction must be between 1 and 100000",
            ));
        }
        if self.ef_search.is_some_and(|ef| ef == 0 || ef > MAX_EF) {
            return Err(PartitionError::Manifest(
                "ef_search must be between 1 and 100000",
            ));
        }
        if self.idempotency_window == 0 {
            return Err(PartitionError::Manifest(
                "idempotency window must be positive",
            ));
        }
        if !(0.0..=0.9).contains(&self.headroom_fraction) {
            return Err(PartitionError::Manifest(
                "headroom fraction must be between 0 and 0.9",
            ));
        }
        if !(self.compaction_tombstone_ratio > 0.0 && self.compaction_tombstone_ratio <= 1.0) {
            return Err(PartitionError::Manifest(
                "compaction tombstone ratio must be in (0, 1]",
            ));
        }
        Ok(())
    }

    /// Bytes an upsert of one vector with `payload_len` bytes adds to the
    /// hot set under the published model (contract §11): int8 copy and
    /// scalars, graph links for this M, slot and key bookkeeping. `graph(M)`
    /// is the line through the two measured points, 182 bytes at M = 16 and
    /// 310 at M = 32 (`docs/bench/README.md`).
    pub fn hot_bytes_per_insert(&self, payload_len: usize) -> usize {
        let graph = 54 + 8 * self.m;
        self.dims + 8 + graph + 80 + payload_len
    }

    fn params(&self) -> HnswParams {
        HnswParams {
            m: self.m,
            m0: self.m0,
            ef_construction: self.ef_construction,
            seed: self.seed,
        }
    }

    fn metric(&self) -> Result<Metric, PartitionError> {
        Metric::parse(&self.metric).ok_or(PartitionError::Manifest("unknown metric"))
    }
}

/// Named crash boundaries (contract §12). The partition returns
/// [`PartitionError::Injected`] at the boundary; dropping it and reopening
/// the directory models the crash. A failpoint between the WAL append and
/// the apply poisons the writer, as a real failure there would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failpoint {
    AfterWalAppend,
    AfterWalSync,
    AfterApply,
    SnapshotBeforeRename,
    SnapshotAfterRename,
    /// Compaction: the rebuilt state's snapshot is durable but the served
    /// state has not been swapped.
    CompactionBeforeSwap,
}

/// Live state captured at the rebuild point by
/// [`Partition::capture_compaction`]. It owns copies of everything the
/// rebuild needs and borrows nothing, so [`build`](Self::build) can run on
/// another thread while the partition keeps serving reads and writes.
pub struct CompactionInput {
    rebuild_point: u64,
    metric: Metric,
    params: HnswParams,
    vectors: crate::vector::FlatVectors,
    /// Old slot of each new slot, in slot order.
    old_slots: Vec<Slot>,
    slots: Vec<SlotRecord>,
    key_table: Vec<Vec<u8>>,
    keys: HashMap<Vec<u8>, (u32, Option<Slot>)>,
    idempotency: Vec<([u8; 16], u64)>,
}

impl CompactionInput {
    pub fn rebuild_point(&self) -> u64 {
        self.rebuild_point
    }

    /// Phase 1b: rebuild the graph over the captured live slots. This is
    /// the long part of compaction and touches no partition state.
    pub fn build(self) -> CompactionPlan {
        let old_to_new: HashMap<Slot, Slot> = self
            .old_slots
            .iter()
            .enumerate()
            .map(|(new, old)| (*old, new as Slot))
            .collect();
        let index = Hnsw::build_parallel(self.vectors, self.metric, self.params);
        let keys = self
            .keys
            .into_iter()
            .map(|(key, (index_in_table, live))| {
                let live = live.and_then(|old| old_to_new.get(&old).copied());
                (key, (index_in_table, live))
            })
            .collect();
        CompactionPlan {
            rebuild_point: self.rebuild_point,
            index,
            key_table: self.key_table,
            keys,
            slots: self.slots,
            idempotency: self.idempotency,
        }
    }
}

/// The frozen rebuild produced by [`CompactionInput::build`].
pub struct CompactionPlan {
    rebuild_point: u64,
    index: Hnsw,
    key_table: Vec<Vec<u8>>,
    keys: HashMap<Vec<u8>, (u32, Option<Slot>)>,
    slots: Vec<SlotRecord>,
    idempotency: Vec<([u8; 16], u64)>,
}

impl CompactionPlan {
    pub fn rebuild_point(&self) -> u64 {
        self.rebuild_point
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionReport {
    pub rebuild_point: u64,
    pub caught_up_entries: u64,
    pub slots_before: usize,
    pub tombstones_before: usize,
    pub slots_after: usize,
    pub snapshot: PathBuf,
}

/// Outcome of a mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub seq: u64,
    /// True when the operation id was already in the window and nothing was
    /// re-applied.
    pub deduplicated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub key: Vec<u8>,
    pub distance: f32,
    pub payload: Vec<u8>,
    pub slot: Slot,
}

/// What `open` (or `inspect`) found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub partition_id: Uuid,
    pub snapshot_used: Option<PathBuf>,
    pub snapshot_high_water: u64,
    pub snapshots_rejected: Vec<(PathBuf, String)>,
    pub wal_segments_read: usize,
    pub wal_entries_replayed: u64,
    pub wal_entries_skipped: u64,
    pub torn_tails_truncated: Vec<(PathBuf, u64)>,
    /// A torn tail found by [`Partition::inspect`], which repairs nothing.
    pub torn_tail_found: Option<(PathBuf, u64)>,
    pub high_water: u64,
}

struct IdempotencyWindow {
    capacity: usize,
    order: VecDeque<[u8; 16]>,
    seqs: HashMap<[u8; 16], u64>,
}

impl IdempotencyWindow {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            seqs: HashMap::new(),
        }
    }

    fn get(&self, op_id: &[u8; 16]) -> Option<u64> {
        self.seqs.get(op_id).copied()
    }

    fn record(&mut self, op_id: [u8; 16], seq: u64) {
        if self.seqs.insert(op_id, seq).is_none() {
            self.order.push_back(op_id);
        }
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seqs.remove(&old);
            }
        }
    }

    fn entries(&self) -> Vec<([u8; 16], u64)> {
        self.order
            .iter()
            .filter_map(|id| self.seqs.get(id).map(|seq| (*id, *seq)))
            .collect()
    }
}

pub struct Partition {
    directory: PathBuf,
    config: PartitionConfig,
    metric: Metric,
    index: Hnsw,
    /// Every key ever seen, in first-seen order; slot records reference it.
    key_table: Vec<Vec<u8>>,
    /// Key -> (key_index, live slot if any).
    keys: HashMap<Vec<u8>, (u32, Option<Slot>)>,
    slots: Vec<SlotRecord>,
    idempotency: IdempotencyWindow,
    /// Absent only while `open` replays history; set before serving.
    wal: Option<WalWriter>,
    high_water: u64,
    snapshot_high_water: u64,
    failpoint: Option<Failpoint>,
    /// Held while the partition is open; `None` only for the staged state
    /// inside `finish_compaction` and during recovery.
    lock: Option<PartitionLock>,
}

impl Partition {
    /// Create an empty partition directory. Fails if the directory already
    /// holds a manifest.
    pub fn create(directory: &Path, config: PartitionConfig) -> Result<Self, PartitionError> {
        config.validate()?;
        let metric = config.metric()?;
        fsutil::create_dir_durable(directory)?;
        let lock = Self::lock(directory)?;
        let manifest = directory.join(MANIFEST_NAME);
        if manifest.exists() {
            return Err(PartitionError::AlreadyExists(directory.to_owned()));
        }
        // Both subdirectories exist before the directory sync that makes the
        // manifest durable, so neither entry can be lost after creation.
        for sub in [WAL_DIR, SNAP_DIR] {
            if !directory.join(sub).is_dir() {
                fs::create_dir(directory.join(sub))?;
            }
        }
        let temporary = directory.join(format!(".{MANIFEST_NAME}.tmp"));
        fs::write(&temporary, serde_json::to_vec_pretty(&config)?)?;
        fs::File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, &manifest)?;
        fsutil::sync_dir(directory)?;
        let wal = WalWriter::open(&directory.join(WAL_DIR), config.id, 0)?;
        Ok(Self {
            directory: directory.to_owned(),
            index: Hnsw::new(config.dims, metric, config.params()),
            key_table: Vec::new(),
            keys: HashMap::new(),
            slots: Vec::new(),
            idempotency: IdempotencyWindow::new(config.idempotency_window),
            wal: Some(wal),
            high_water: 0,
            snapshot_high_water: 0,
            failpoint: None,
            lock: Some(lock),
            metric,
            config,
        })
    }

    /// Take the exclusive directory lock, failing with
    /// [`PartitionError::Locked`] when another owner holds it.
    pub fn lock(directory: &Path) -> Result<PartitionLock, PartitionError> {
        PartitionLock::try_acquire(directory)?
            .ok_or_else(|| PartitionError::Locked(directory.to_owned()))
    }

    /// Create a partition from a fixed set of vectors using the parallel
    /// bulk builder (contract §8, "Bulk build") and persist it as a snapshot
    /// at high-water mark 0. Keys are supplied per row; payloads are empty.
    /// Used for bulk loads and benchmarks; ordinary writes go through
    /// [`upsert`](Self::upsert). The manifest records that history starts
    /// with this snapshot (`base_snapshot`), so the partition fails closed if
    /// it is ever lost, including when the load itself is interrupted.
    pub fn bulk_load(
        directory: &Path,
        mut config: PartitionConfig,
        vectors: crate::vector::FlatVectors,
        keys: impl Iterator<Item = Vec<u8>>,
    ) -> Result<(Self, PathBuf), PartitionError> {
        config.base_snapshot = true;
        let mut partition = Self::create(directory, config)?;
        if vectors.dims() != partition.config.dims {
            return Err(PartitionError::Vector(VectorError::WrongDims {
                expected: partition.config.dims,
                actual: vectors.dims(),
            }));
        }
        let count = vectors.len();
        let mut key_table = Vec::with_capacity(count);
        let mut key_map = HashMap::with_capacity(count);
        let mut slots = Vec::with_capacity(count);
        for (slot, key) in keys.take(count).enumerate() {
            if key_map.contains_key(&key) {
                return Err(PartitionError::Manifest("bulk load keys must be unique"));
            }
            key_map.insert(key.clone(), (slot as u32, Some(slot as Slot)));
            key_table.push(key);
            slots.push(SlotRecord {
                version: 0,
                tombstone: false,
                key_index: slot as u32,
                payload: Vec::new(),
            });
        }
        if key_table.len() != count {
            return Err(PartitionError::Manifest(
                "bulk load needs one key per vector",
            ));
        }
        partition.index =
            Hnsw::build_parallel(vectors, partition.metric, partition.config.params());
        partition.key_table = key_table;
        partition.keys = key_map;
        partition.slots = slots;
        let path = partition.snapshot()?;
        Ok((partition, path))
    }

    /// Recover a partition from its directory (contract §6) and open it for
    /// writes. The directory lock is held until the partition is dropped.
    /// A torn tail is truncated only after the whole log has been validated.
    pub fn open(directory: &Path) -> Result<(Self, RecoveryReport), PartitionError> {
        let lock = Self::lock(directory)?;
        let (mut partition, mut report) = Self::recover(directory)?;
        if let Some((path, offset)) = report.torn_tail_found.take() {
            wal::truncate_torn_tail(&path, offset)?;
            report.torn_tails_truncated.push((path, offset));
        }
        partition.wal = Some(WalWriter::open(
            &directory.join(WAL_DIR),
            partition.config.id,
            partition.high_water,
        )?);
        partition.lock = Some(lock);
        Ok((partition, report))
    }

    /// Verify a partition without changing it: the full recovery (snapshot
    /// verification, WAL validation, replay into memory) runs, but nothing
    /// is repaired and no writer is opened. The caller holds the directory
    /// lock, so nothing can be appending meanwhile.
    pub fn inspect(lock: &PartitionLock) -> Result<RecoveryReport, PartitionError> {
        let (_, report) = Self::recover(lock.directory())?;
        Ok(report)
    }

    fn recover(directory: &Path) -> Result<(Self, RecoveryReport), PartitionError> {
        let config: PartitionConfig =
            serde_json::from_slice(&fs::read(directory.join(MANIFEST_NAME))?)?;
        config.validate()?;
        let metric = config.metric()?;
        let mut report = RecoveryReport {
            partition_id: config.id,
            ..RecoveryReport::default()
        };

        // Newest verifiable snapshot.
        let mut loaded: Option<LoadedSnapshot> = None;
        for (_, path) in snapshot::list(&directory.join(SNAP_DIR))? {
            match snapshot::load(&path, config.id) {
                Ok(snapshot) => {
                    loaded = Some(snapshot);
                    break;
                }
                Err(error) => report.snapshots_rejected.push((path, error.to_string())),
            }
        }
        if loaded.is_none() && config.base_snapshot {
            return Err(PartitionError::BaseSnapshotUnavailable {
                rejected: report.snapshots_rejected.len(),
            });
        }

        let (index, key_table, keys, slots, idempotency, high_water) = match loaded {
            Some(snapshot) => {
                if snapshot.dims != config.dims || snapshot.metric != metric {
                    return Err(PartitionError::Manifest("snapshot disagrees with manifest"));
                }
                if snapshot.params != config.params() {
                    return Err(PartitionError::Manifest(
                        "snapshot graph parameters disagree with manifest",
                    ));
                }
                report.snapshot_used = Some(snapshot.path.clone());
                report.snapshot_high_water = snapshot.high_water;
                let index = Hnsw::from_parts(
                    snapshot.params,
                    metric,
                    snapshot.vectors,
                    snapshot.quantized,
                    snapshot.graph,
                )
                .ok_or(PartitionError::Snapshot(SnapshotError::Malformed(
                    "graph shape",
                )))?;
                let mut key_table = Vec::with_capacity(snapshot.keys.len());
                let mut keys = HashMap::with_capacity(snapshot.keys.len());
                for (index_in_table, (key, live)) in snapshot.keys.into_iter().enumerate() {
                    let live = (live != u32::MAX).then_some(live);
                    keys.insert(key.clone(), (index_in_table as u32, live));
                    key_table.push(key);
                }
                let mut window = IdempotencyWindow::new(config.idempotency_window);
                for (op_id, seq) in snapshot.idempotency {
                    window.record(op_id, seq);
                }
                (
                    index,
                    key_table,
                    keys,
                    snapshot.slots,
                    window,
                    snapshot.high_water,
                )
            }
            None => (
                Hnsw::new(config.dims, metric, config.params()),
                Vec::new(),
                HashMap::new(),
                Vec::new(),
                IdempotencyWindow::new(config.idempotency_window),
                0,
            ),
        };
        let mut partition = Self {
            directory: directory.to_owned(),
            index,
            key_table,
            keys,
            slots,
            idempotency,
            wal: None,
            high_water,
            snapshot_high_water: high_water,
            failpoint: None,
            lock: None,
            metric,
            config: config.clone(),
        };

        // Validate every segment before anything is repaired, and replay the
        // tail exactly once, in order. Only the newest segment may end in a
        // torn tail.
        let segments = wal::list_segments(&directory.join(WAL_DIR))?;
        // A snapshot cannot prove that nothing followed it unless the WAL
        // continues from it: some segment must start at or before the next
        // sequence and the log must reach the snapshot's high-water mark.
        // Only an empty partition (or a bulk base with no writes yet) may
        // have no history at all.
        let mut saw_history = partition.high_water == 0 && segments.is_empty();
        let mut wal_end: Option<u64> = None;
        let newest = segments.len().saturating_sub(1);
        for (position, (first_seq, path)) in segments.iter().enumerate() {
            report.wal_segments_read += 1;
            let read = wal::read_segment(path, config.id, position == newest)?;
            if let Some((offset, _)) = read.torn_tail {
                report.torn_tail_found = Some((path.clone(), offset));
            }
            if *first_seq <= partition.high_water + 1 {
                saw_history = true;
            }
            let end = read.last_seq().unwrap_or(first_seq.saturating_sub(1));
            wal_end = Some(wal_end.map_or(end, |seen| seen.max(end)));
            for entry in read.entries {
                if entry.seq <= partition.high_water {
                    report.wal_entries_skipped += 1;
                    continue;
                }
                if entry.seq != partition.high_water + 1 {
                    return Err(PartitionError::Wal(WalError::SequenceGap {
                        path: path.clone(),
                        expected: partition.high_water + 1,
                        found: entry.seq,
                    }));
                }
                partition.apply(&entry)?;
                report.wal_entries_replayed += 1;
            }
        }
        if !saw_history || wal_end.is_some_and(|end| end < report.snapshot_high_water) {
            return Err(PartitionError::MissingHistory {
                snapshot_high_water: report.snapshot_high_water,
                first_wal_seq: segments.first().map(|s| s.0),
            });
        }
        report.high_water = partition.high_water;
        Ok((partition, report))
    }

    pub fn with_failpoint(mut self, failpoint: Failpoint) -> Self {
        self.failpoint = Some(failpoint);
        self
    }

    pub fn clear_failpoint(&mut self) {
        self.failpoint = None;
    }

    fn hit(&self, point: Failpoint) -> Result<(), PartitionError> {
        if self.failpoint == Some(point) {
            return Err(PartitionError::Injected(point));
        }
        Ok(())
    }

    pub fn id(&self) -> Uuid {
        self.config.id
    }

    pub fn dims(&self) -> usize {
        self.config.dims
    }

    pub fn metric(&self) -> Metric {
        self.metric
    }

    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    pub fn snapshot_high_water(&self) -> u64 {
        self.snapshot_high_water
    }

    pub fn live_count(&self) -> usize {
        self.index.live_len()
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn index(&self) -> &Hnsw {
        &self.index
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Live slot for an external key.
    pub fn slot_for_key(&self, key: &[u8]) -> Option<Slot> {
        self.keys.get(key).and_then(|(_, live)| *live)
    }

    /// Upsert a record. Normalizes for cosine, rejects zero vectors there,
    /// deduplicates by operation id, and acknowledges only after the WAL
    /// entry is synced and applied.
    pub fn upsert(
        &mut self,
        op_id: [u8; 16],
        key: &[u8],
        vector: &[f32],
        payload: &[u8],
    ) -> Result<Applied, PartitionError> {
        if let Some(seq) = self.idempotency.get(&op_id) {
            return Ok(Applied {
                seq,
                deduplicated: true,
            });
        }
        if vector.len() != self.config.dims {
            return Err(PartitionError::Vector(VectorError::WrongDims {
                expected: self.config.dims,
                actual: vector.len(),
            }));
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(PartitionError::Vector(VectorError::NonFinite));
        }
        let mut stored = vector.to_vec();
        if self.metric.normalizes() && !normalize(&mut stored) {
            return Err(PartitionError::ZeroVector);
        }
        self.admit(payload.len())?;
        self.mutate(
            op_id,
            Operation::Upsert {
                key: key.to_vec(),
                payload: payload.to_vec(),
                vector: stored,
            },
        )
    }

    /// Delete a record. Deleting an absent key succeeds and is logged.
    pub fn delete(&mut self, op_id: [u8; 16], key: &[u8]) -> Result<Applied, PartitionError> {
        if let Some(seq) = self.idempotency.get(&op_id) {
            return Ok(Applied {
                seq,
                deduplicated: true,
            });
        }
        self.mutate(op_id, Operation::Delete { key: key.to_vec() })
    }

    /// Admission against the RAM budget (contract §11): refuse an upsert
    /// that would push the hot set past `budget − headroom`. Deletes are
    /// always admitted; nothing is ever dropped to make room.
    fn admit(&self, payload_len: usize) -> Result<(), PartitionError> {
        let Some(budget) = self.config.ram_budget_bytes else {
            return Ok(());
        };
        let headroom = (budget as f64 * self.config.headroom_fraction.clamp(0.0, 0.9)) as u64;
        let limit = budget.saturating_sub(headroom);
        let projected =
            self.resident_bytes() as u64 + self.config.hot_bytes_per_insert(payload_len) as u64;
        if projected > limit {
            return Err(PartitionError::OverBudget {
                resident_bytes: self.resident_bytes() as u64,
                projected_bytes: projected,
                admissible_bytes: limit,
                budget_bytes: budget,
                headroom_bytes: headroom,
            });
        }
        Ok(())
    }

    /// Change the RAM budget at runtime (the manifest keeps the configured
    /// default; the balancer owns live budgets in M4).
    pub fn set_ram_budget(&mut self, budget_bytes: Option<u64>) {
        self.config.ram_budget_bytes = budget_bytes;
    }

    pub fn tombstone_ratio(&self) -> f64 {
        if self.slots.is_empty() {
            0.0
        } else {
            self.index.tombstone_count() as f64 / self.slots.len() as f64
        }
    }

    pub fn compaction_recommended(&self) -> bool {
        self.tombstone_ratio() >= self.config.compaction_tombstone_ratio
    }

    /// Phase 1a of compaction (contract §10): freeze the rebuild point and
    /// copy the live slots, in slot order, with the key table and the
    /// idempotency window. This is the only part of phase 1 that borrows the
    /// partition (a memory copy of the live rows); the rebuild itself runs in
    /// [`CompactionInput::build`], which owns its inputs, so a caller that
    /// keeps the partition behind a lock releases it for the rebuild.
    pub fn capture_compaction(&self) -> Result<CompactionInput, PartitionError> {
        let live = self.index.live_len();
        let mut vectors = crate::vector::FlatVectors::with_capacity(self.config.dims, live);
        let mut slots = Vec::with_capacity(live);
        let mut old_slots = Vec::with_capacity(live);
        for (old_slot, record) in self.slots.iter().enumerate() {
            if record.tombstone {
                continue;
            }
            let row = self
                .index
                .vectors()
                .get(old_slot as u64)
                .ok_or(PartitionError::Manifest("slot without vector"))?;
            // Rows are already normalized; store as given.
            vectors.push(row, Metric::L2)?;
            old_slots.push(old_slot as Slot);
            slots.push(record.clone());
        }
        Ok(CompactionInput {
            rebuild_point: self.high_water,
            metric: self.metric,
            params: self.config.params(),
            vectors,
            old_slots,
            slots,
            key_table: self.key_table.clone(),
            keys: self.keys.clone(),
            idempotency: self.idempotency.entries(),
        })
    }

    /// Phase 1 in one call: [`capture_compaction`](Self::capture_compaction)
    /// then [`CompactionInput::build`]. Holding `&self` for the whole rebuild,
    /// it blocks writers for that long; use the two steps to avoid that.
    pub fn begin_compaction(&self) -> Result<CompactionPlan, PartitionError> {
        Ok(self.capture_compaction()?.build())
    }

    /// Phase 2 of compaction: apply every entry acknowledged after the
    /// rebuild point to the new state from the WAL, write its snapshot at
    /// the current high-water mark, and swap. A crash before the swap leaves
    /// the old state authoritative on disk until the new snapshot is the
    /// newest verifiable one, which represents the same logical state.
    pub fn finish_compaction(
        &mut self,
        plan: CompactionPlan,
    ) -> Result<CompactionReport, PartitionError> {
        let CompactionPlan {
            rebuild_point,
            index,
            key_table,
            keys,
            slots,
            idempotency,
        } = plan;
        let mut staged = Partition {
            directory: self.directory.clone(),
            config: self.config.clone(),
            metric: self.metric,
            index,
            key_table,
            keys,
            slots,
            idempotency: IdempotencyWindow::new(self.config.idempotency_window),
            wal: None,
            high_water: rebuild_point,
            snapshot_high_water: self.snapshot_high_water,
            failpoint: None,
            lock: None,
        };
        for (op_id, seq) in idempotency {
            staged.idempotency.record(op_id, seq);
        }
        // Catch up from the durable log: every entry after the rebuild point.
        let mut replayed = 0_u64;
        let segments = wal::list_segments(&self.directory.join(WAL_DIR))?;
        let newest = segments.len().saturating_sub(1);
        for (position, (first_seq, path)) in segments.into_iter().enumerate() {
            if first_seq > self.high_water {
                continue;
            }
            let read = wal::read_segment(&path, self.config.id, position == newest)?;
            for entry in read.entries {
                if entry.seq <= rebuild_point || entry.seq > self.high_water {
                    continue;
                }
                if entry.seq != staged.high_water + 1 {
                    return Err(PartitionError::Wal(WalError::SequenceGap {
                        path: path.clone(),
                        expected: staged.high_water + 1,
                        found: entry.seq,
                    }));
                }
                staged.apply(&entry)?;
                replayed += 1;
            }
        }
        if staged.high_water != self.high_water {
            return Err(PartitionError::MissingHistory {
                snapshot_high_water: staged.high_water,
                first_wal_seq: Some(self.high_water),
            });
        }
        let before = (self.slots.len(), self.index.tombstone_count());
        let path = staged.snapshot()?;
        self.hit(Failpoint::CompactionBeforeSwap)?;
        // Swap: the staged state becomes the served state; the writer stays.
        self.index = staged.index;
        self.key_table = staged.key_table;
        self.keys = staged.keys;
        self.slots = staged.slots;
        self.idempotency = staged.idempotency;
        self.snapshot_high_water = self.high_water;
        Ok(CompactionReport {
            rebuild_point,
            caught_up_entries: replayed,
            slots_before: before.0,
            tombstones_before: before.1,
            slots_after: self.slots.len(),
            snapshot: path,
        })
    }

    fn mutate(&mut self, op_id: [u8; 16], operation: Operation) -> Result<Applied, PartitionError> {
        let seq = self
            .wal
            .as_mut()
            .expect("writer is open after recovery")
            .append_unsynced(op_id, operation.clone())?;
        // From here the log may hold an entry the in-memory state lacks. Any
        // early exit poisons the writer, so no later entry is applied or
        // logged behind it until a reopen replays the log.
        if let Err(error) = self.sync_and_apply(seq, op_id, operation) {
            if let Some(wal) = self.wal.as_mut() {
                wal.poison();
            }
            return Err(error);
        }
        self.hit(Failpoint::AfterApply)?;
        Ok(Applied {
            seq,
            deduplicated: false,
        })
    }

    fn sync_and_apply(
        &mut self,
        seq: u64,
        op_id: [u8; 16],
        operation: Operation,
    ) -> Result<(), PartitionError> {
        self.hit(Failpoint::AfterWalAppend)?;
        self.wal
            .as_mut()
            .expect("writer is open after recovery")
            .sync()?;
        self.hit(Failpoint::AfterWalSync)?;
        self.apply(&Entry {
            seq,
            op_id,
            operation,
        })
    }

    /// Apply an entry to in-memory state. Shared by the live path and
    /// replay; never writes the WAL. Vectors arrive already normalized.
    fn apply(&mut self, entry: &Entry) -> Result<(), PartitionError> {
        match &entry.operation {
            Operation::Upsert {
                key,
                payload,
                vector,
            } => {
                if vector.len() != self.config.dims {
                    return Err(PartitionError::Vector(VectorError::WrongDims {
                        expected: self.config.dims,
                        actual: vector.len(),
                    }));
                }
                let slot = self.index.insert(vector)?;
                let key_index = match self.keys.get(key) {
                    Some((index, _)) => *index,
                    None => {
                        self.key_table.push(key.clone());
                        (self.key_table.len() - 1) as u32
                    }
                };
                if let Some((_, Some(previous))) = self.keys.get(key) {
                    let previous = *previous;
                    self.index.tombstone(previous);
                    self.slots[previous as usize].tombstone = true;
                }
                self.keys.insert(key.clone(), (key_index, Some(slot)));
                self.slots.push(SlotRecord {
                    version: entry.seq,
                    tombstone: false,
                    key_index,
                    payload: payload.clone(),
                });
            }
            Operation::Delete { key } => {
                if let Some((index, Some(previous))) = self.keys.get(key).cloned() {
                    self.index.tombstone(previous);
                    self.slots[previous as usize].tombstone = true;
                    self.keys.insert(key.clone(), (index, None));
                }
            }
        }
        self.idempotency.record(entry.op_id, entry.seq);
        self.high_water = entry.seq;
        Ok(())
    }

    /// Approximate top-k over live records with the default int8 mode and,
    /// if given, a filter over slots (contract §9 cutover applies). Slots
    /// are internal and change on upsert and compaction, so a filter is only
    /// meaningful against the state it was computed from.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        filter: Option<&RoaringBitmap>,
    ) -> Result<Vec<SearchHit>, PartitionError> {
        if query.len() != self.config.dims {
            return Err(PartitionError::Vector(VectorError::WrongDims {
                expected: self.config.dims,
                actual: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(PartitionError::Vector(VectorError::NonFinite));
        }
        if k > MAX_K {
            return Err(PartitionError::QueryLimit {
                what: "k",
                value: k,
                max: MAX_K,
            });
        }
        if ef > MAX_EF {
            return Err(PartitionError::QueryLimit {
                what: "ef",
                value: ef,
                max: MAX_EF,
            });
        }
        let mut q = query.to_vec();
        if self.metric.normalizes() && !normalize(&mut q) {
            return Err(PartitionError::ZeroVector);
        }
        let mode = SearchMode::int8_default(k);
        let hits = match filter {
            Some(allowed) => {
                self.index
                    .search_filtered(&q, k, ef, mode, allowed, Hnsw::filter_cutover(ef))
            }
            None => self.index.search_with(&q, k, ef, mode, None),
        };
        Ok(hits
            .into_iter()
            .map(|hit| {
                let slot = hit.id as Slot;
                let record = &self.slots[slot as usize];
                SearchHit {
                    key: self.key_table[record.key_index as usize].clone(),
                    distance: hit.distance,
                    payload: record.payload.clone(),
                    slot,
                }
            })
            .collect())
    }

    /// [`search`](Self::search) with the collection's `ef_search` default.
    pub fn search_default(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&RoaringBitmap>,
    ) -> Result<Vec<SearchHit>, PartitionError> {
        self.search(query, k, self.config.ef_search(), filter)
    }

    /// Write a snapshot at the current high-water mark (contract §5), then
    /// serve the `f32` originals from it, so rows appended since the last
    /// snapshot (or copied by a bulk load or compaction) leave the heap
    /// (contract §7, §11).
    pub fn snapshot(&mut self) -> Result<PathBuf, PartitionError> {
        let keys: Vec<(Vec<u8>, Slot)> = self
            .key_table
            .iter()
            .map(|key| {
                let live = self.keys.get(key).and_then(|(_, live)| *live);
                (key.clone(), live.unwrap_or(u32::MAX))
            })
            .collect();
        let idempotency = self.idempotency.entries();
        let input = SnapshotInput {
            partition_id: self.config.id,
            dims: self.config.dims,
            metric: self.metric,
            params: self.index.params(),
            high_water: self.high_water,
            keys: &keys,
            slots: &self.slots,
            vectors: self.index.vectors(),
            quantized: self.index.quantized(),
            graph: self.index.graph_parts(),
            idempotency: &idempotency,
        };
        let abort = self.failpoint == Some(Failpoint::SnapshotBeforeRename);
        let path = snapshot::write_with_hook(&self.directory.join(SNAP_DIR), &input, &|| !abort)?;
        self.snapshot_high_water = self.high_water;
        let mapped =
            snapshot::map_vectors(&path, self.config.id, self.config.dims, self.slots.len())?;
        if !self.index.replace_vectors(mapped) {
            return Err(PartitionError::Snapshot(SnapshotError::Malformed(
                "f32 section shape",
            )));
        }
        self.hit(Failpoint::SnapshotAfterRename)?;
        Ok(path)
    }

    /// Remove WAL segments wholly covered by the latest snapshot. The caller
    /// archives them first (M1 copy-first rule; pass 7).
    pub fn truncate_wal_below_snapshot(&mut self) -> Result<usize, PartitionError> {
        let segments = wal::list_segments(&self.directory.join(WAL_DIR))?;
        let mut removed = 0;
        for window in segments.windows(2) {
            let (_, path) = &window[0];
            let (next_first, _) = &window[1];
            // Segment ends at next_first - 1; covered if that is <= snapshot HWM.
            let current = self.wal.as_ref().map(|w| w.current_segment().to_owned());
            if *next_first <= self.snapshot_high_water + 1 && Some(path) != current.as_ref() {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            fsutil::sync_dir(&self.directory.join(WAL_DIR))?;
        }
        Ok(removed)
    }

    /// Heap-resident bytes per the contract §11 model: int8, graph, slot,
    /// key, payload, and idempotency bookkeeping, plus any `f32` rows not yet
    /// served from a snapshot (appended since the last one).
    pub fn resident_bytes(&self) -> usize {
        let payloads: usize = self.slots.iter().map(|s| s.payload.len() + 32).sum();
        let keys: usize = self.key_table.iter().map(|k| k.len() + 48).sum();
        self.index.accounted_bytes() + payloads + keys + self.idempotency.order.len() * 40
    }

    /// Number of f32 bytes served from the memory-mapped snapshot.
    pub fn mapped_bytes(&self) -> usize {
        self.index.vectors().mapped_bytes()
    }

    pub fn quantized(&self) -> &QuantizedVectors {
        self.index.quantized()
    }
}

#[derive(Debug, Error)]
pub enum PartitionError {
    #[error("partition already exists at {0}")]
    AlreadyExists(PathBuf),
    #[error("partition at {0} is locked by another owner")]
    Locked(PathBuf),
    #[error("{what} = {value} exceeds the maximum of {max}")]
    QueryLimit {
        what: &'static str,
        value: usize,
        max: usize,
    },
    #[error("partition history starts with a bulk-load snapshot and no verifiable snapshot remains ({rejected} rejected); refusing to serve without it")]
    BaseSnapshotUnavailable { rejected: usize },
    #[error("partition manifest is invalid: {0}")]
    Manifest(&'static str),
    #[error("cosine collections reject zero-norm vectors")]
    ZeroVector,
    #[error("injected failure at {0:?}")]
    Injected(Failpoint),
    #[error("over RAM budget: resident {resident_bytes} + insert would reach {projected_bytes} bytes, admissible {admissible_bytes} of {budget_bytes} (headroom {headroom_bytes}); refusing without dropping data")]
    OverBudget {
        resident_bytes: u64,
        projected_bytes: u64,
        admissible_bytes: u64,
        budget_bytes: u64,
        headroom_bytes: u64,
    },
    #[error("no WAL history covers sequence {snapshot_high_water}+1 (first WAL sequence {first_wal_seq:?}); refusing to serve an incomplete partition")]
    MissingHistory {
        snapshot_high_water: u64,
        first_wal_seq: Option<u64>,
    },
    #[error(transparent)]
    Vector(#[from] VectorError),
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> HnswParams {
        HnswParams {
            m: 8,
            m0: 16,
            ef_construction: 64,
            seed: 9,
        }
    }

    fn config(id: Uuid) -> PartitionConfig {
        let mut config = PartitionConfig::new(id, 8, Metric::Cosine, params());
        config.idempotency_window = 64;
        config
    }

    fn vector(i: usize) -> Vec<f32> {
        (0..8)
            .map(|d| ((i * (d + 3)) as f32 * 0.37).sin() + 0.1)
            .collect()
    }

    fn op(i: u64) -> [u8; 16] {
        let mut id = [0_u8; 16];
        id[..8].copy_from_slice(&i.to_le_bytes());
        id
    }

    fn fill(partition: &mut Partition, range: std::ops::Range<usize>) {
        for i in range {
            partition
                .upsert(
                    op(i as u64),
                    format!("key-{i}").as_bytes(),
                    &vector(i),
                    format!("p{i}").as_bytes(),
                )
                .unwrap();
        }
    }

    fn state(partition: &Partition) -> (u64, usize, usize, Vec<SearchHit>, Vec<SearchHit>) {
        let q1: Vec<f32> = (0..8).map(|d| (d as f32 * 0.3).cos()).collect();
        let q2: Vec<f32> = (0..8).map(|d| (d as f32 * 0.9).sin() - 0.2).collect();
        (
            partition.high_water(),
            partition.live_count(),
            partition.slot_count(),
            partition.search(&q1, 5, 32, None).unwrap(),
            partition.search(&q2, 5, 32, None).unwrap(),
        )
    }

    #[test]
    fn snapshot_plus_tail_reproduces_state_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..300);
        for i in (0..300).step_by(7) {
            partition
                .delete(op(10_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        for i in 100..140 {
            partition
                .upsert(
                    op(20_000 + i as u64),
                    format!("key-{i}").as_bytes(),
                    &vector(i + 1000),
                    b"v2",
                )
                .unwrap();
        }
        let before_snapshot = state(&partition);
        let path = partition.snapshot().unwrap();
        assert!(path.exists());
        fill(&mut partition, 300..360);
        let before = state(&partition);
        assert_eq!(before.0, 300 + 43 + 40 + 60);
        drop(partition);

        let (reopened, report) = Partition::open(directory.path()).unwrap();
        assert_eq!(report.snapshot_used.as_ref(), Some(&path));
        assert_eq!(report.snapshot_high_water, before_snapshot.0);
        assert_eq!(report.wal_entries_replayed, 60);
        assert_eq!(report.wal_entries_skipped, before_snapshot.0);
        assert!(report.snapshots_rejected.is_empty());
        assert_eq!(state(&reopened), before);
        assert!(
            reopened.mapped_bytes() > 0,
            "originals served from the snapshot map"
        );
        let hit = &reopened.search(&vector(1120), 1, 32, None).unwrap()[0];
        assert_eq!(hit.key, b"key-120");
        assert_eq!(hit.payload, b"v2");
        assert!(reopened.slot_for_key(b"key-7").is_none());
        // The directory lock admits one owner at a time.
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::Locked(_))
        ));
        drop(reopened);

        // Nothing but the WAL (no snapshot) also recovers exactly.
        fs::remove_dir_all(directory.path().join(SNAP_DIR)).unwrap();
        let (replayed, report) = Partition::open(directory.path()).unwrap();
        assert!(report.snapshot_used.is_none());
        assert_eq!(report.wal_entries_replayed, before.0);
        assert_eq!(state(&replayed), before);
    }

    #[test]
    fn idempotency_survives_reopen_and_window_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        let first = partition.upsert(op(1), b"k", &vector(1), b"a").unwrap();
        let again = partition.upsert(op(1), b"k", &vector(2), b"b").unwrap();
        assert_eq!(
            again,
            Applied {
                seq: first.seq,
                deduplicated: true
            }
        );
        assert_eq!(partition.slot_count(), 1, "retry applied nothing");
        partition.snapshot().unwrap();
        drop(partition);
        let (mut reopened, _) = Partition::open(directory.path()).unwrap();
        let after = reopened.upsert(op(1), b"k", &vector(3), b"c").unwrap();
        assert!(after.deduplicated);
        // Beyond the window an id is applied again (window = 64).
        fill(&mut reopened, 10..90);
        let late = reopened.upsert(op(1), b"k", &vector(4), b"d").unwrap();
        assert!(!late.deduplicated);
    }

    #[test]
    fn every_write_failpoint_recovers_to_exactly_one_application() {
        for point in [
            Failpoint::AfterWalAppend,
            Failpoint::AfterWalSync,
            Failpoint::AfterApply,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let mut partition = Partition::create(directory.path(), config(id)).unwrap();
            fill(&mut partition, 0..20);
            partition.snapshot().unwrap();
            fill(&mut partition, 20..30);
            let mut crashing = partition.with_failpoint(point);
            let error = crashing
                .upsert(op(99), b"crash-key", &vector(99), b"x")
                .unwrap_err();
            assert!(matches!(error, PartitionError::Injected(p) if p == point));
            drop(crashing);

            let (mut reopened, report) = Partition::open(directory.path()).unwrap();
            assert!(report.wal_entries_replayed >= 10, "{point:?}: {report:?}");
            let present = reopened.slot_for_key(b"crash-key").is_some();
            match point {
                Failpoint::AfterWalAppend => {} // unsynced: either outcome is legal
                _ => assert!(present, "{point:?}: synced entry must replay"),
            }
            let retry = reopened
                .upsert(op(99), b"crash-key", &vector(99), b"x")
                .unwrap();
            assert_eq!(retry.deduplicated, present, "{point:?}");
            assert!(reopened.slot_for_key(b"crash-key").is_some());
            let live_for_key = reopened
                .search(&vector(99), 3, 32, None)
                .unwrap()
                .into_iter()
                .filter(|h| h.key == b"crash-key")
                .count();
            assert_eq!(live_for_key, 1, "{point:?}: exactly one live version");
            assert_eq!(
                reopened.slot_count(),
                31,
                "{point:?}: no double application"
            );
        }
    }

    #[test]
    fn snapshot_failpoints_leave_a_recoverable_directory() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..50);
        let first = partition.snapshot().unwrap();
        fill(&mut partition, 50..80);
        let expected = state(&partition);

        let mut crashing = partition.with_failpoint(Failpoint::SnapshotBeforeRename);
        assert!(crashing.snapshot().is_err());
        let temps = fs::read_dir(directory.path().join(SNAP_DIR))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(temps, 1, "temporary left behind");
        crashing.clear_failpoint();
        let mut crashing = crashing.with_failpoint(Failpoint::SnapshotAfterRename);
        assert!(matches!(
            crashing.snapshot(),
            Err(PartitionError::Injected(Failpoint::SnapshotAfterRename))
        ));
        drop(crashing);

        let (reopened, report) = Partition::open(directory.path()).unwrap();
        assert_ne!(
            report.snapshot_used.as_ref(),
            Some(&first),
            "newest complete snapshot wins"
        );
        assert_eq!(report.snapshot_high_water, expected.0);
        assert_eq!(report.wal_entries_replayed, 0);
        assert_eq!(state(&reopened), expected);
    }

    #[test]
    fn corrupt_snapshot_is_rejected_and_older_history_is_used() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..40);
        let first = partition.snapshot().unwrap();
        fill(&mut partition, 40..70);
        let second = partition.snapshot().unwrap();
        let expected = state(&partition);
        drop(partition);

        // Damage a byte inside the second snapshot's body.
        let mut bytes = fs::read(&second).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x5A;
        fs::write(&second, &bytes).unwrap();
        let (reopened, report) = Partition::open(directory.path()).unwrap();
        assert_eq!(report.snapshot_used.as_ref(), Some(&first));
        assert_eq!(report.snapshots_rejected.len(), 1);
        assert!(
            report.snapshots_rejected[0].1.contains("hash"),
            "{report:?}"
        );
        assert_eq!(report.wal_entries_replayed, 30);
        assert_eq!(state(&reopened), expected);
        drop(reopened);

        // A snapshot for another partition id is rejected too.
        let other = Uuid::new_v4();
        assert!(matches!(
            snapshot::load(&first, other),
            Err(SnapshotError::WrongPartition { .. })
        ));
        // Header CRC and footer damage are named.
        let mut header = fs::read(&first).unwrap();
        header[10] ^= 1;
        fs::write(directory.path().join("x.snap"), &header).unwrap();
        assert!(matches!(
            snapshot::load(&directory.path().join("x.snap"), id),
            Err(SnapshotError::HeaderCrc)
        ));
    }

    #[test]
    fn wal_damage_and_missing_history_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..30);
        let segment = partition.wal.as_ref().unwrap().current_segment().to_owned();
        drop(partition);

        // Torn tail: truncated and recovered to the last good entry.
        let full = fs::read(&segment).unwrap();
        fs::write(&segment, &full[..full.len() - 10]).unwrap();
        let (recovered, report) = Partition::open(directory.path()).unwrap();
        assert_eq!(report.torn_tails_truncated.len(), 1);
        assert_eq!(recovered.high_water(), 29);
        assert_eq!(recovered.slot_count(), 29);
        drop(recovered);

        // Mid-segment damage fails closed.
        let mut damaged = full.clone();
        damaged[wal::HEADER_LEN + 70] ^= 0x01;
        fs::write(&segment, &damaged).unwrap();
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::Wal(WalError::Corrupt { .. }))
        ));

        // A snapshot whose WAL history was lost refuses to serve: it cannot
        // prove that nothing followed it.
        fs::write(&segment, &full).unwrap();
        let (mut again, _) = Partition::open(directory.path()).unwrap();
        again.snapshot().unwrap();
        fill(&mut again, 30..40);
        drop(again);
        fs::remove_file(&segment).unwrap();
        let result = Partition::open(directory.path());
        assert!(
            matches!(result, Err(PartitionError::MissingHistory { .. })),
            "{:?}",
            result.err()
        );
        // An empty partition with neither snapshot nor WAL is simply empty.
        let empty = tempfile::tempdir().unwrap();
        drop(Partition::create(empty.path(), config(Uuid::new_v4())).unwrap());
        fs::remove_dir_all(empty.path().join(WAL_DIR)).unwrap();
        let (fresh, _) = Partition::open(empty.path()).unwrap();
        assert_eq!(fresh.high_water(), 0);
    }

    #[test]
    fn bulk_loaded_partition_round_trips_every_section() {
        let directory = tempfile::tempdir().unwrap();
        let mut rows = crate::vector::FlatVectors::new(32);
        for i in 0..6000 {
            let v: Vec<f32> = (0..32)
                .map(|d| ((i * (d + 7)) as f32 * 0.013).sin() * 3.0)
                .collect();
            rows.push(&v, Metric::L2).unwrap();
        }
        let params = HnswParams {
            m: 16,
            m0: 32,
            ef_construction: 100,
            seed: 21,
        };
        let config = PartitionConfig::new(Uuid::new_v4(), 32, Metric::L2, params);
        let (partition, _) = Partition::bulk_load(
            directory.path(),
            config,
            rows.clone(),
            (0..6000_u64).map(|i| i.to_le_bytes().to_vec()),
        )
        .unwrap();
        let mut partition = partition;
        for i in 0..100 {
            let v: Vec<f32> = (0..32)
                .map(|d| ((i * (d + 11)) as f32 * 0.021).cos() * 2.0)
                .collect();
            partition
                .upsert(
                    op(900_000 + i as u64),
                    format!("tail-{i}").as_bytes(),
                    &v,
                    b"t",
                )
                .unwrap();
        }
        let query: Vec<f32> = (0..32).map(|d| (d as f32 * 0.11).cos()).collect();
        let before = partition.search(&query, 10, 100, None).unwrap();
        let before_f32 = partition.index().search(&query, 10, 100, None);
        let before_parts = {
            let g = partition.index().graph_parts();
            (
                g.levels.to_vec(),
                g.level0.to_vec(),
                g.level0_len.to_vec(),
                g.upper.to_vec(),
                g.upper_len.to_vec(),
                g.entry,
                g.max_level,
            )
        };
        let before_q = {
            let (c, s, n) = partition.index().quantized().parts();
            (c.to_vec(), s.to_vec(), n.to_vec())
        };
        drop(partition);

        let (recovered, report) = Partition::open(directory.path()).unwrap();
        assert!(report.snapshots_rejected.is_empty(), "{report:?}");
        let g = recovered.index().graph_parts();
        assert_eq!(g.levels, before_parts.0.as_slice(), "levels");
        assert_eq!(g.level0_len, before_parts.2.as_slice(), "level0 lengths");
        assert_eq!(g.level0, before_parts.1.as_slice(), "level0 links");
        assert_eq!(g.upper, before_parts.3.as_slice(), "upper links");
        assert_eq!(g.upper_len, before_parts.4.as_slice(), "upper lengths");
        assert_eq!((g.entry, g.max_level), (before_parts.5, before_parts.6));
        let (c, s, n) = recovered.index().quantized().parts();
        assert_eq!(
            (c, s, n),
            (
                before_q.0.as_slice(),
                before_q.1.as_slice(),
                before_q.2.as_slice()
            ),
            "int8"
        );
        for slot in [0_u64, 1, 2999, 5999] {
            assert_eq!(
                recovered.index().vectors().get(slot).unwrap(),
                rows.get(slot).unwrap(),
                "f32 row {slot}"
            );
        }
        let after_f32 = recovered.index().search(&query, 10, 100, None);
        assert_eq!(after_f32, before_f32, "f32 traversal after recovery");
        let after = recovered.search(&query, 10, 100, None).unwrap();
        assert_eq!(after, before, "int8 traversal after recovery");
    }

    #[test]
    fn compaction_preserves_live_records_hides_deleted_ones_and_survives_crash_before_swap() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..400);
        for i in (0..400).step_by(3) {
            partition
                .delete(op(50_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        assert!(partition.compaction_recommended());
        let plan = partition.begin_compaction().unwrap();
        // Writes continue after the rebuild point: delete a live record,
        // re-upsert a deleted one, add new ones.
        partition.delete(op(60_000), b"key-1").unwrap();
        partition
            .upsert(op(60_001), b"key-3", &vector(3), b"back")
            .unwrap();
        fill(&mut partition, 400..430);
        let expected = state(&partition);
        let live_keys: Vec<Vec<u8>> = (0..430)
            .map(|i| format!("key-{i}").into_bytes())
            .filter(|k| partition.slot_for_key(k).is_some())
            .collect();

        // Crash before swap: the new snapshot exists; the live state is old.
        let mut crashing = partition.with_failpoint(Failpoint::CompactionBeforeSwap);
        assert!(matches!(
            crashing.finish_compaction(plan),
            Err(PartitionError::Injected(Failpoint::CompactionBeforeSwap))
        ));
        drop(crashing);
        let (reopened, report) = Partition::open(directory.path()).unwrap();
        assert_eq!(
            report.snapshot_high_water, expected.0,
            "compacted snapshot is the newest"
        );
        assert_eq!(report.wal_entries_replayed, 0);
        assert_eq!(reopened.live_count(), expected.1);
        // The rebuild had no tombstones; the one delete caught up after the
        // rebuild point (key-1) is the only tombstone in the new state.
        assert_eq!(reopened.index().tombstone_count(), 1);
        assert!(reopened.slot_for_key(b"key-1").is_none());
        assert_eq!(
            reopened.search(&vector(3), 1, 32, None).unwrap()[0].payload,
            b"back"
        );
        for key in &live_keys {
            assert!(reopened.slot_for_key(key).is_some(), "{key:?}");
        }
        assert_eq!(reopened.search(&vector(3), 5, 32, None).unwrap().len(), 5);

        // Normal compaction swaps in place and keeps serving the same keys.
        let mut partition = reopened;
        fill(&mut partition, 430..440);
        for i in 430..436 {
            partition
                .delete(op(70_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        let plan = partition.begin_compaction().unwrap();
        partition
            .upsert(op(80_000), b"key-999", &vector(999), b"n")
            .unwrap();
        let report = partition.finish_compaction(plan).unwrap();
        assert_eq!(report.caught_up_entries, 1);
        // Six new deletes plus the one tombstone carried from the previous
        // compaction's catch-up.
        assert_eq!(report.tombstones_before, 7);
        assert_eq!(report.slots_after, partition.live_count());
        assert_eq!(partition.index().tombstone_count(), 0);
        assert!(partition.slot_for_key(b"key-999").is_some());
        assert!(partition.slot_for_key(b"key-430").is_none());
        // Idempotency survives compaction.
        assert!(
            partition
                .upsert(op(80_000), b"key-999", &vector(1), b"x")
                .unwrap()
                .deduplicated
        );
        // Order is preserved: the oracle agrees on the top results.
        let q: Vec<f32> = (0..8).map(|d| (d as f32 * 0.5).sin()).collect();
        let mut nq = q.clone();
        normalize(&mut nq);
        let hits = partition.search(&q, 5, 400, None).unwrap();
        let exact =
            crate::oracle::search(partition.index().vectors(), Metric::Cosine, &nq, 5, |id| {
                !partition.index().is_tombstoned(id as Slot)
            });
        assert_eq!(
            hits.iter().map(|h| h.slot as u64).collect::<Vec<_>>(),
            exact.iter().map(|h| h.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ram_budget_refuses_upserts_but_admits_deletes() {
        let directory = tempfile::tempdir().unwrap();
        let mut budgeted = config(Uuid::new_v4());
        budgeted.ram_budget_bytes = Some(64 * 1024);
        budgeted.headroom_fraction = 0.25;
        let mut partition = Partition::create(directory.path(), budgeted).unwrap();
        let mut admitted = 0;
        let refusal = loop {
            match partition.upsert(
                op(admitted),
                format!("k{admitted}").as_bytes(),
                &vector(admitted as usize),
                b"",
            ) {
                Ok(_) => admitted += 1,
                Err(error) => break error,
            }
            assert!(admitted < 10_000, "budget never bit");
        };
        assert!(
            matches!(refusal, PartitionError::OverBudget { .. }),
            "{refusal}"
        );
        assert!(admitted > 10);
        assert_eq!(
            partition.high_water(),
            admitted,
            "a refused upsert consumes no sequence"
        );
        for i in 0..admitted / 2 {
            partition
                .delete(op(99_000 + i), format!("k{i}").as_bytes())
                .unwrap();
        }
        assert!(partition.slot_for_key(b"k0").is_none());
        // Deletes do not free hot-set bytes until compaction; compaction does.
        assert!(matches!(
            partition.upsert(op(100_000), b"again", &vector(5), b""),
            Err(PartitionError::OverBudget { .. })
        ));
        let plan = partition.begin_compaction().unwrap();
        partition.finish_compaction(plan).unwrap();
        assert!(partition
            .upsert(op(100_000), b"again", &vector(5), b"")
            .is_ok());
        // Operators may change the budget at runtime.
        partition.set_ram_budget(Some(1));
        assert!(matches!(
            partition.upsert(op(100_001), b"tiny", &vector(6), b""),
            Err(PartitionError::OverBudget { .. })
        ));
        partition.set_ram_budget(None);
        assert!(partition
            .upsert(op(100_001), b"tiny", &vector(6), b"")
            .is_ok());
    }

    #[test]
    fn zero_vectors_are_rejected_on_cosine_and_deletes_are_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let mut partition = Partition::create(directory.path(), config(Uuid::new_v4())).unwrap();
        assert!(matches!(
            partition.upsert(op(1), b"z", &[0.0; 8], b""),
            Err(PartitionError::ZeroVector)
        ));
        assert!(matches!(
            partition.search(&[0.0; 8], 3, 16, None),
            Err(PartitionError::ZeroVector)
        ));
        let d1 = partition.delete(op(2), b"absent").unwrap();
        assert!(!d1.deduplicated);
        assert_eq!(partition.high_water(), 1);
        partition.upsert(op(3), b"k", &vector(1), b"").unwrap();
        partition.delete(op(4), b"k").unwrap();
        assert!(partition.slot_for_key(b"k").is_none());
        assert_eq!(partition.live_count(), 0);
        assert!(partition
            .search(&vector(1), 3, 16, None)
            .unwrap()
            .is_empty());
    }

    fn wal_header(id: Uuid, first_seq: u64) -> Vec<u8> {
        let mut header = vec![0_u8; wal::HEADER_LEN];
        header[0..4].copy_from_slice(wal::WAL_MAGIC);
        header[4..6].copy_from_slice(&wal::WAL_VERSION.to_le_bytes());
        header[8..24].copy_from_slice(id.as_bytes());
        header[24..32].copy_from_slice(&first_seq.to_le_bytes());
        header
    }

    /// Offsets of every frame in a segment file.
    fn frame_starts(bytes: &[u8]) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut at = wal::HEADER_LEN;
        while at < bytes.len() {
            starts.push(at);
            at += 4 + u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        }
        starts
    }

    #[test]
    fn records_inserted_after_mass_deletion_stay_reachable() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..600);
        for i in 0..600 {
            partition
                .delete(op(10_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        assert_eq!(partition.live_count(), 0);
        for i in 600..700 {
            partition
                .upsert(
                    op(i as u64),
                    format!("key-{i}").as_bytes(),
                    &vector(i),
                    b"new",
                )
                .unwrap();
        }
        let found = |p: &Partition| {
            (600..700)
                .filter(|&i| {
                    p.search(&vector(i), 1, 64, None)
                        .unwrap()
                        .first()
                        .is_some_and(|hit| hit.key == format!("key-{i}").as_bytes())
                })
                .count()
        };
        assert_eq!(
            found(&partition),
            100,
            "every new record is its own nearest neighbour"
        );
        drop(partition);
        let (reopened, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(found(&reopened), 100, "replay rebuilds the same links");
    }

    #[test]
    fn a_bulk_load_without_its_snapshot_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut rows = crate::vector::FlatVectors::new(8);
        for i in 0..300 {
            rows.push(&vector(i), Metric::Cosine).unwrap();
        }
        let (mut partition, snapshot) = Partition::bulk_load(
            directory.path(),
            config(Uuid::new_v4()),
            rows,
            (0..300_u64).map(|i| i.to_le_bytes().to_vec()),
        )
        .unwrap();
        fill(&mut partition, 1000..1003);
        assert_eq!(partition.live_count(), 303);
        drop(partition);

        // A damaged base snapshot: the WAL alone holds only three entries.
        let good = fs::read(&snapshot).unwrap();
        let mut bad = good.clone();
        let middle = bad.len() / 2;
        bad[middle] ^= 0x10;
        fs::write(&snapshot, &bad).unwrap();
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::BaseSnapshotUnavailable { rejected: 1 })
        ));
        // A missing one, likewise.
        fs::remove_file(&snapshot).unwrap();
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::BaseSnapshotUnavailable { rejected: 0 })
        ));
        // Restored, it opens with every record.
        fs::write(&snapshot, &good).unwrap();
        let (reopened, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(reopened.live_count(), 303);
    }

    #[test]
    fn a_wal_that_stops_short_of_the_snapshot_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..10);
        let segment = partition.wal.as_ref().unwrap().current_segment().to_owned();
        let stale = fs::read(&segment).unwrap();
        fill(&mut partition, 10..20);
        partition.snapshot().unwrap();
        drop(partition);
        // An older copy of the segment (for example from an archive set that
        // recorded the snapshot but not the grown segment) ends at 10.
        fs::write(&segment, &stale).unwrap();
        let result = Partition::open(directory.path());
        assert!(
            matches!(
                result,
                Err(PartitionError::MissingHistory {
                    snapshot_high_water: 20,
                    ..
                })
            ),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn damage_in_a_sealed_segment_fails_closed_without_touching_any_file() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..10);
        let segment = partition.wal.as_ref().unwrap().current_segment().to_owned();
        drop(partition);
        // Split the log into a sealed segment (1..=5) and an active one (6..=10).
        let full = fs::read(&segment).unwrap();
        let starts = frame_starts(&full);
        let mut second = wal_header(id, 6);
        second.extend_from_slice(&full[starts[5]..]);
        let second_path = directory
            .path()
            .join(WAL_DIR)
            .join(format!("{:020}.wal", 6));
        fs::write(&second_path, &second).unwrap();
        fs::write(&segment, &full[..starts[5]]).unwrap();
        let (split, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(split.high_water(), 10);
        drop(split);

        // Cut the sealed segment's last entry: corruption, and nothing on
        // disk may change, so the evidence survives for the operator.
        let sealed = fs::read(&segment).unwrap();
        fs::write(&segment, &sealed[..sealed.len() - 10]).unwrap();
        let before = (fs::read(&segment).unwrap(), fs::read(&second_path).unwrap());
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::Wal(WalError::Corrupt { .. }))
        ));
        assert_eq!(
            (fs::read(&segment).unwrap(), fs::read(&second_path).unwrap()),
            before
        );
    }

    #[test]
    fn a_damaged_length_field_fails_closed_instead_of_dropping_acknowledged_entries() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..10);
        let segment = partition.wal.as_ref().unwrap().current_segment().to_owned();
        drop(partition);
        let full = fs::read(&segment).unwrap();
        let starts = frame_starts(&full);
        let mut damaged = full.clone();
        damaged[starts[1] + 3] ^= 0x01; // len of entry 2 grows by 16 MiB
        fs::write(&segment, &damaged).unwrap();
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::Wal(WalError::Corrupt { .. }))
        ));
        assert_eq!(fs::read(&segment).unwrap(), damaged, "nothing truncated");
    }

    #[test]
    fn inspect_repairs_nothing_and_open_truncates_after_validation() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut partition = Partition::create(directory.path(), config(id)).unwrap();
        fill(&mut partition, 0..12);
        let segment = partition.wal.as_ref().unwrap().current_segment().to_owned();
        drop(partition);
        let full = fs::read(&segment).unwrap();
        fs::write(&segment, &full[..full.len() - 7]).unwrap();

        let lock = Partition::lock(directory.path()).unwrap();
        let report = Partition::inspect(&lock).unwrap();
        assert_eq!(report.partition_id, id);
        assert_eq!(report.high_water, 11);
        assert!(report.torn_tail_found.is_some());
        assert!(report.torn_tails_truncated.is_empty());
        assert_eq!(fs::read(&segment).unwrap().len(), full.len() - 7);
        assert!(matches!(
            Partition::open(directory.path()),
            Err(PartitionError::Locked(_))
        ));
        drop(lock);

        let (mut reopened, report) = Partition::open(directory.path()).unwrap();
        assert_eq!(report.torn_tails_truncated.len(), 1);
        assert!(report.torn_tail_found.is_none());
        assert_eq!(reopened.high_water(), 11);
        fill(&mut reopened, 100..101);
        drop(reopened);
        let (again, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(again.high_water(), 12);
    }

    #[test]
    fn the_writer_is_poisoned_between_append_and_apply() {
        for point in [Failpoint::AfterWalAppend, Failpoint::AfterWalSync] {
            let directory = tempfile::tempdir().unwrap();
            let mut partition =
                Partition::create(directory.path(), config(Uuid::new_v4())).unwrap();
            fill(&mut partition, 0..5);
            let mut failing = partition.with_failpoint(point);
            assert!(matches!(
                failing.upsert(op(50), b"lost?", &vector(50), b""),
                Err(PartitionError::Injected(p)) if p == point
            ));
            failing.clear_failpoint();
            // The log holds seq 6 that memory lacks; appending seq 7 behind
            // it would make replicas of this log diverge.
            assert!(matches!(
                failing.upsert(op(51), b"next", &vector(51), b""),
                Err(PartitionError::Wal(WalError::Poisoned))
            ));
            assert!(matches!(
                failing.delete(op(52), b"key-1"),
                Err(PartitionError::Wal(WalError::Poisoned))
            ));
            assert_eq!(failing.high_water(), 5);
            drop(failing);
            let (reopened, _) = Partition::open(directory.path()).unwrap();
            assert_eq!(
                reopened.high_water(),
                6,
                "{point:?}: the logged entry replays"
            );
            assert!(reopened.slot_for_key(b"next").is_none());
        }
    }

    #[test]
    fn non_finite_queries_and_oversized_requests_are_rejected_on_every_metric() {
        for metric in [Metric::Cosine, Metric::Dot, Metric::L2] {
            let directory = tempfile::tempdir().unwrap();
            let mut partition = Partition::create(
                directory.path(),
                PartitionConfig::new(Uuid::new_v4(), 8, metric, params()),
            )
            .unwrap();
            fill(&mut partition, 0..20);
            for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut query = vector(3);
                query[2] = bad;
                assert!(
                    matches!(
                        partition.search(&query, 3, 16, None),
                        Err(PartitionError::Vector(VectorError::NonFinite))
                    ),
                    "{metric:?} {bad}"
                );
                assert!(matches!(
                    partition.upsert(op(99), b"bad", &query, b""),
                    Err(PartitionError::Vector(VectorError::NonFinite))
                ));
            }
            assert!(matches!(
                partition.search(&vector(3), 1 << 40, 16, None),
                Err(PartitionError::QueryLimit { what: "k", .. })
            ));
            assert!(matches!(
                partition.search(&vector(3), 3, usize::MAX, None),
                Err(PartitionError::QueryLimit { what: "ef", .. })
            ));
            assert!(matches!(
                partition.search(&vector(3)[..7], 3, 16, None),
                Err(PartitionError::Vector(VectorError::WrongDims { .. }))
            ));
            // k beyond the live count returns every live record, once.
            assert_eq!(
                partition.search(&vector(3), MAX_K, 16, None).unwrap().len(),
                20
            );
        }
    }

    #[test]
    fn unrepresentable_configurations_are_refused_at_create() {
        let directory = tempfile::tempdir().unwrap();
        let base = config(Uuid::new_v4());
        let cases: Vec<(&str, PartitionConfig)> = vec![
            (
                "m=1",
                PartitionConfig {
                    m: 1,
                    ..base.clone()
                },
            ),
            (
                "m0>255",
                PartitionConfig {
                    m0: 256,
                    ..base.clone()
                },
            ),
            (
                "m0<m",
                PartitionConfig {
                    m: 16,
                    m0: 8,
                    ..base.clone()
                },
            ),
            (
                "dims=0",
                PartitionConfig {
                    dims: 0,
                    ..base.clone()
                },
            ),
            (
                "dims>max",
                PartitionConfig {
                    dims: wal::MAX_DIMS + 1,
                    ..base.clone()
                },
            ),
            (
                "ef_construction=0",
                PartitionConfig {
                    ef_construction: 0,
                    ..base.clone()
                },
            ),
            (
                "window=0",
                PartitionConfig {
                    idempotency_window: 0,
                    ..base.clone()
                },
            ),
            (
                "headroom",
                PartitionConfig {
                    headroom_fraction: 1.5,
                    ..base.clone()
                },
            ),
            (
                "ratio",
                PartitionConfig {
                    compaction_tombstone_ratio: 0.0,
                    ..base.clone()
                },
            ),
            (
                "ef_search",
                PartitionConfig {
                    ef_search: Some(0),
                    ..base.clone()
                },
            ),
            (
                "metric",
                PartitionConfig {
                    metric: "hamming".into(),
                    ..base.clone()
                },
            ),
        ];
        for (name, bad) in cases {
            assert!(
                matches!(
                    Partition::create(&directory.path().join(name), bad),
                    Err(PartitionError::Manifest(_))
                ),
                "{name}"
            );
            assert!(!directory.path().join(name).join(MANIFEST_NAME).exists());
        }
    }

    #[test]
    fn per_metric_defaults_come_from_one_place() {
        let id = Uuid::new_v4();
        let cosine = PartitionConfig::for_metric(id, 8, Metric::Cosine);
        assert_eq!((cosine.m, cosine.m0, cosine.ef_search()), (32, 64, 400));
        let l2 = PartitionConfig::for_metric(id, 8, Metric::L2);
        assert_eq!((l2.m, l2.m0, l2.ef_search()), (16, 32, 100));
        assert_eq!(HnswParams::default(), HnswParams::for_metric(Metric::Dot));
        assert_ne!(
            PartitionConfig::for_metric(Uuid::new_v4(), 8, Metric::L2).seed,
            l2.seed,
            "level seeds differ between partitions"
        );
        let directory = tempfile::tempdir().unwrap();
        let mut partition = Partition::create(directory.path(), cosine).unwrap();
        fill(&mut partition, 0..50);
        assert_eq!(
            partition.search_default(&vector(7), 3, None).unwrap(),
            partition.search(&vector(7), 3, 400, None).unwrap()
        );
    }

    #[test]
    fn f32_originals_leave_the_heap_at_every_snapshot_and_accounting_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let mut partition = Partition::create(directory.path(), config(Uuid::new_v4())).unwrap();
        fill(&mut partition, 0..500);
        let heap_with_rows = partition.resident_bytes();
        partition.snapshot().unwrap();
        let after_snapshot = partition.resident_bytes();
        assert_eq!(
            heap_with_rows - after_snapshot,
            500 * 8 * 4,
            "rows remapped"
        );
        assert_eq!(partition.mapped_bytes(), 500 * 8 * 4);
        let expected = state(&partition);
        drop(partition);
        let (mut reopened, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(reopened.resident_bytes(), after_snapshot);
        assert_eq!(state(&reopened), expected);

        // Compaction copies live rows to the heap to rebuild; the swapped-in
        // state serves them from its new snapshot.
        for i in (0..500).step_by(2) {
            reopened
                .delete(op(70_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        let plan = reopened.begin_compaction().unwrap();
        reopened.finish_compaction(plan).unwrap();
        assert_eq!(reopened.index().vectors().resident_bytes(), 0);
        assert_eq!(reopened.mapped_bytes(), 250 * 8 * 4);
        let compacted = reopened.resident_bytes();
        drop(reopened);
        let (again, _) = Partition::open(directory.path()).unwrap();
        assert_eq!(again.resident_bytes(), compacted);
    }

    #[test]
    fn compaction_rebuilds_on_another_thread_while_reads_and_writes_continue() {
        use std::sync::{mpsc, Arc, RwLock};

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Partition>();
        assert_send_sync::<CompactionInput>();

        let directory = tempfile::tempdir().unwrap();
        let mut partition = Partition::create(directory.path(), config(Uuid::new_v4())).unwrap();
        fill(&mut partition, 0..400);
        for i in (0..400).step_by(3) {
            partition
                .delete(op(50_000 + i as u64), format!("key-{i}").as_bytes())
                .unwrap();
        }
        let shared = Arc::new(RwLock::new(partition));
        // Only the capture holds the partition; the rebuild owns its input.
        let input = shared.read().unwrap().capture_compaction().unwrap();
        let rebuild_point = input.rebuild_point();
        let (started, wait) = mpsc::channel();
        let builder = std::thread::spawn(move || {
            started.send(()).unwrap();
            input.build()
        });
        wait.recv().unwrap();
        let reader = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                for i in 0..40 {
                    let hits = shared
                        .read()
                        .unwrap()
                        .search(&vector(i), 3, 32, None)
                        .unwrap();
                    assert!(!hits.is_empty());
                }
            })
        };
        for i in 400..440 {
            let mut writer = shared.write().unwrap();
            writer
                .upsert(
                    op(i as u64),
                    format!("key-{i}").as_bytes(),
                    &vector(i),
                    b"w",
                )
                .unwrap();
        }
        shared
            .write()
            .unwrap()
            .delete(op(60_000), b"key-1")
            .unwrap();
        reader.join().unwrap();
        let plan = builder.join().unwrap();
        let mut partition = Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
        let report = partition.finish_compaction(plan).unwrap();
        assert_eq!(report.rebuild_point, rebuild_point);
        assert_eq!(report.caught_up_entries, 41);
        for i in 400..440 {
            assert!(partition
                .slot_for_key(format!("key-{i}").as_bytes())
                .is_some());
        }
        assert!(partition.slot_for_key(b"key-1").is_none());
        assert!(partition.slot_for_key(b"key-3").is_none());
        assert_eq!(partition.index().tombstone_count(), 1);
    }
}

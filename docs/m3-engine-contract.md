# M3 engine contract

Status: **Proposed** (2026-09-30, pass 2 of
[m3-implementation-plan.md](m3-implementation-plan.md); revised 2026-10-25
by the hardening pass that followed the project audit, which tightened the
WAL torn-tail rule, recovery's history checks, and several claims the code
did not meet). This document pins
the semantics, persistent formats, parameters, and crash boundaries of the
single-partition vector engine before graph code is written. It is
subordinate to `DESIGN.md` §6 and `roadmap.md` M3; later passes may extend
it but must not weaken a rule without a documented decision. Anything the
exact oracle in `crates/distr-hnsw-index/src/oracle.rs` already defines
(distance functions, result order) is restated here as the authority.

## 1. Identity and ordering

| Term | Definition |
|---|---|
| Partition id | UUID assigned by the portal; every WAL segment and snapshot carries it and refuses to load under another. |
| External key | The record id the API sees: 1 to 512 bytes, opaque, compared bytewise. Unique among live records. |
| Slot | Internal dense index (`u32`) of one stored vector version. Slots are append-only and immutable once written; upsert creates a new slot. Graph neighbour lists hold slots. At most 2^32 − 1 slots per partition; the size-triggered split (DESIGN §6.2) keeps partitions far below that. |
| Version | The WAL sequence number that created a slot. |
| Sequence (`seq`) | Monotonic `u64` per partition, assigned on WAL append, starting at 1. |
| Committed high-water mark | Highest `seq` durably synced and applied. Queries observe a state at some high-water mark and every hit's distance comes from that state. |

**Result order** for every search path, exact or approximate, filtered or
not: distance ascending, then slot ascending. Slots are assigned in WAL
order, so ties resolve to the earlier insertion and the order is identical on
every replica that applied the same log. NaN is impossible: vectors with a
non-finite component are rejected at insert and query time
(`VectorError::NonFinite` on every metric).

## 2. Metrics

| Metric | Stored form | Distance reported |
|---|---|---|
| `cosine` | unit-normalized on insert and query | `1 − dot(a, b)`, range 0 to 2 |
| `dot` | as given | `−dot(a, b)` |
| `l2` | as given | squared Euclidean distance |

Cosine rejects zero-norm vectors at the public API (insert and query) with a
distinct error, since their direction is undefined. The engine core accepts
them for benchmark comparability with the defined semantics `normalize(0) =
0`, so a zero vector is at distance 1 from everything. `l2` reports the
squared distance; ranking is unchanged and callers that need the root apply
it.

## 3. Operations and semantics

Two operations exist. Each is one WAL entry, applied atomically.

- **Upsert(key, vector, payload)**: if `key` is live, its slot is
  tombstoned; a new slot is appended with the vector, an int8 copy, and the
  payload (opaque bytes, at most 64 KiB). The new slot's version is the
  entry's `seq`.
- **Delete(key)**: the live slot for `key`, if any, is tombstoned. Deleting an
  absent key succeeds and is logged, so replay is idempotent.

Tombstoned slots are masked from every result but remain in the graph until
compaction (DESIGN §6.4). A query never returns a tombstoned slot, a slot
newer than the high-water mark it observes, or a distance computed from a
different slot than the one it returns.

**Idempotency.** Every entry carries an `op_id` (16 bytes, portal- or
client-assigned). The partition keeps a window of the last `W` applied
`op_id`s with their `seq` (default `W` = 1,000,000), persisted in snapshots.
An entry whose `op_id` is in the window is not re-applied; the original
`seq` is returned. Two limits follow, and callers must respect them:

- Deduplication is by `op_id` alone. A retry that reuses an `op_id` with a
  different key or vector is reported as `deduplicated` and changes
  nothing; the engine does not compare request contents. The portal must
  derive `op_id`s so that one id always names one request.
- An `op_id` older than the window is applied again. That is only harmless
  when no other write touched the same key in between: a delayed retry of
  `upsert(k, v1)` after `upsert(k, v2)` would silently restore `v1`, and a
  stale delete would remove a re-inserted record. Retries must therefore be
  bounded well inside the window (`W` operations on this partition). M4
  enforces that with epochs and the committed high-water mark (DESIGN
  §6.3), or by storing a request fingerprint per `op_id`; until then the
  window is the only guard.

**Limits.** A search accepts `k ≤ 10,000` (`MAX_K`) and `ef ≤ 100,000`
(`MAX_EF`); larger values return `PartitionError::QueryLimit` rather than
reserving memory for them. A partition's configuration is validated at
create and open: `1 ≤ dims ≤ 65,536`, `2 ≤ M ≤ 255`, `M ≤ M0 ≤ 255`
(neighbour-list lengths are stored as `u8`), `1 ≤ ef_construction ≤
100,000`, a positive idempotency window, headroom in `[0, 0.9]`, and a
compaction ratio in `(0, 1]`.

## 4. Write-ahead log

Segments live at `wal/<first_seq, 20 decimal digits>.wal` and rotate at
64 MiB. Integers are little-endian.

```text
segment header (32 bytes)
  magic "DHWL" | version u16 = 1 | reserved u16 | partition_id [16] | first_seq u64
entry
  len u32          bytes after this field, including crc
  crc32c u32       over everything after this field
  seq u64
  op_id [16]
  op u8            1 = upsert, 2 = delete
  key_len u16 | key
  upsert only: payload_len u32 | payload | dims u32 | vector f32le[dims]
```

Rules:

1. An entry is acknowledged only after its segment is `fdatasync`ed. Group
   commit may batch entries; the acknowledgement of any entry implies all
   earlier entries in the batch are durable.
2. `seq` must increase by exactly one across entries and segments. A gap or
   repeat is corruption.
3. On open, every segment is read to its end before anything is repaired.
   A legal `len` lies between 32 (a delete with a 1-byte key) and 328,231
   (an upsert with the largest key, payload, and vector). The `len` field is
   not covered by the CRC, so a damaged frame is classified by what follows
   it, never by the length it claims:
   - In the **newest** segment, a frame that is short, has a `len` outside
     the legal range or past the end of the file, or fails its CRC is a
     **torn tail** only if no CRC-valid frame starts anywhere after it. Then
     it and everything after it (for example a zero-filled region left by a
     crash between `write` and `fdatasync`) were never acknowledged; the
     file is truncated to the last good entry, and that is reported. If a
     valid frame does follow, the damage is in the middle of the log and the
     partition **fails closed**: a corrupted length can never discard the
     acknowledged entries behind it.
   - A newest segment shorter than its header, or all zeros, was created by
     an interrupted rotation (the header is synced before any entry is
     appended) and holds nothing; it is removed.
   - **Sealed** segments (all but the newest) are synced before their
     successor is created, so any damage in them is corruption and fails
     closed. Nothing is truncated: the bytes are left for diagnosis.
   - A frame whose CRC matches but whose body is invalid, a bad header, or a
     header whose first sequence disagrees with the file name fails closed.

   A failure names the segment and offset; the partition does not serve.
   These rules (2026-10-25) changed the reader, not the format: WAL version
   1 segments written before them read the same way.
   Group commit (rule 1) will batch unsynced frames, so a torn batch can
   leave a valid frame after a torn one; that case currently fails closed
   and needs its own rule when group commit lands.
4. Entries with `seq` at or below the loaded snapshot's high-water mark are
   skipped during replay; every later entry is applied exactly once in
   order.
5. Segments wholly covered by a durable snapshot are eligible for archival
   to the blob plane (M1 object class `wal_segment`) and local truncation,
   in that order; a segment is never deleted before its archive copy is
   verified (M1 copy-first rule).
6. The writer is **poisoned** after any failed write, `fdatasync`, or
   rotation, and after an injected failure between append and apply: the
   file may hold a partial frame, or an entry the in-memory state lacks.
   A poisoned writer refuses every later append (`WalError::Poisoned`)
   until the partition is reopened and recovery re-establishes the end of
   the log. A failed `fdatasync` is never retried (Linux may report a
   writeback error once and drop the dirty pages). The writer extends the
   newest segment only when that segment ends exactly at the recovered
   high-water mark; otherwise it refuses to open rather than append behind
   a stale or damaged tail.

## 5. Snapshot

A snapshot is one file `snap/<high_water_seq, 20 digits>.snap`, written to a
temporary name, synced, renamed, with the directory synced (the M1 durable
write sequence). The partition directory, `wal/`, and `snap/` are created
with their parents synced, so no directory entry the engine relies on can
vanish after power loss. Sections are 64-byte aligned so the file can be
memory mapped and served from the page cache when a partition is demoted
from RAM.

```text
header (128 bytes)
  magic "DHSN" | version u16 = 1 | flags u16 | partition_id [16]
  dims u32 | metric u8 | quant u8 (1 = int8 per-vector) | reserved
  high_water_seq u64 | slot_count u32 | live_count u32 | key_count u32
  idempotency_window u32 | section_count u32 | section_table_offset u64
  header_crc32c u32
section table: for each section
  kind u16 | reserved u16 | offset u64 | len u64 | blake3 [32]
sections (kinds)
  1 keys        key_count × (key_len u16 | key | live_slot u32)
  2 slots       slot_count × (version u64 | tombstone u8 | key_index u32 | payload_offset u64 | payload_len u32)
  3 payloads    concatenated payload bytes
  4 vectors_f32 slot_count × dims × f32
  5 vectors_i8  scales f32 × slot_count | norms_sq f32 × slot_count | int8 × dims × slot_count
  6 graph       m u32 | m0 u32 | ef_construction u32 | seed u64 | entry_slot u32 (u32::MAX = none)
                | max_level u8 | tombstone_count u64 | levels u8 × slot_count | level0_len u8 × slot_count
                | level0 u32 × (slot_count × m0) | tombstone words u64 × ceil(slot_count / 64)
                | per slot with level > 0: upper_len u8 × level | upper u32 × (level × m)
  7 idempotency window × (op_id [16] | seq u64)
footer
  file_blake3 [32] over everything before the footer
```

The keys section lists every key ever seen (live slot `u32::MAX` when none
is live) so slot records can reference keys by index. Beside the snapshot
and WAL directories, `partition.json` records the static configuration
(partition id, dims, metric, graph parameters, idempotency window) and is
written durably at creation; a snapshot that disagrees with it is refused.
Two snapshots at the same high-water mark share a name and the later
atomically replaces the earlier. They describe the same logical state but
need not be byte-identical (a compaction renumbers slots), so archives
identify a snapshot by its BLAKE3, never by its name. An aborted write leaves
its temporary file, which loading ignores. `partition.json` also records
`base_snapshot` when history begins with a bulk-load snapshot that is not in
the WAL (§6).

Loading verifies the header CRC, every section's BLAKE3, and the footer; a
mismatch fails closed with the section named. It then cross-checks every
reference the snapshot holds (neighbour targets and list lengths on every
level, the entry point's level, key-to-slot and slot-to-key links, and the
tombstone bitmap against the per-slot flags and count), and the graph
parameters must equal `partition.json`; any disagreement is refused at open
rather than discovered as a panic at query time. The newest snapshot whose
checks pass is used; an older one is used only when the newer fails and the
operator is told. A snapshot is content-addressed by its BLAKE3 when archived
(M1 object class `snapshot`).

## 6. Recovery

`open(partition_dir)`:

1. Load the newest verifiable snapshot (or start empty at `seq` 0). Every
   rejected snapshot is reported with the check that failed.
2. Replay WAL entries with `seq` greater than the snapshot high-water mark,
   in order, exactly once, rebuilding slots, vectors, graph insertions,
   tombstones, and the idempotency window. Stop at a torn tail.
3. The committed high-water mark becomes the last replayed `seq`. Serving
   begins only after replay completes.

The WAL must continue from the snapshot: some segment must start at or
before `high_water + 1`, and the log must reach the snapshot's high-water
mark. Otherwise recovery fails closed (`MissingHistory`): the partition
cannot prove that nothing followed the snapshot, and a writer appending
after a stale segment would brick the next restart. Only an empty partition
may have no history at all. A partition created by a bulk load
(`base_snapshot` in `partition.json`) holds data that is in no WAL entry, so
it refuses to open without a verifiable snapshot
(`BaseSnapshotUnavailable`) instead of starting empty; the flag is written
before the load begins, so an interrupted load fails closed too.

`open` takes an exclusive advisory lock (`flock` on `LOCK` in the partition
directory) for as long as the partition is open, so a server, an archive,
and a restore can never touch one directory at once (`Locked`).
`Partition::inspect` runs the whole recovery in memory under that lock but
repairs nothing and opens no writer; it reports a torn tail instead of
truncating it.
The recovery report (snapshot used, snapshots rejected, segments read,
entries replayed and skipped, torn tails truncated, final high-water mark)
is returned to the caller for logging.

Recovery time at the 1M × 512 scale is a published benchmark number
(threshold in the plan). Snapshot cadence follows DESIGN §6.3: when the WAL
tail exceeds 25% of the snapshot size or an absolute cap (default 256 MiB).

## 7. Quantization and rescoring

Each slot stores its full-precision `f32` vector and an int8 copy:

- `scale = max_i |x_i| / 127` (per vector, symmetric); `q_i = round(x_i /
  scale)` clamped to [−127, 127]; a zero vector has `scale = 0`.
- Stored beside the int8 vector: `scale` and `norm_sq = Σ x_i²` of the
  original, so L2 can use `‖a‖² + ‖b‖² − 2·a·b`.
- Int8 distance: `dot ≈ scale_a · scale_b · Σ q_a,i · q_b,i` with `i32`
  accumulation.

Search runs on the int8 graph with `ef_search`, collects candidates, then
**rescoring** recomputes the exact `f32` distance for the top
`max(4·k, 100)` candidates (configurable) and returns the best `k`. The
returned distance is always the `f32` distance of the returned slot. The
canonical re-run (2026-10-27) measured the recall cost of int8 at no more
than 0.0014 on the public sets (threshold 0.01; `docs/bench/README.md`), so int8 traversal with rescoring
is the default search mode and the `f32` mode remains available for
diagnosis. If the graph itself ever needs re-quantization, the `f32`
originals make that a rebuild, not a data loss.

The `f32` originals are **not** part of the RAM hot set: traversal touches
only int8 codes and graph links, and rescoring reads at most the rescoring
depth of originals per query. Originals therefore live in the memory-mapped
snapshot (§5) and are served through the page cache. Rows appended since the
last snapshot, and rows a bulk load or compaction has just built, are on the
heap until the next snapshot; every snapshot write maps its `f32` section
(after checking that section's BLAKE3) and serves the originals from it, so
they leave the heap. `resident_bytes` counts those heap rows while they
exist (§11).

## 8. HNSW parameters and construction

| Parameter | Default | Notes |
|---|---|---|
| `M` | 16 for `l2` and `dot`, 32 for `cosine` | max neighbours per node on upper levels; set from the pass 3 measurements (`docs/bench/README.md`) |
| `M0` | `2 · M` | max neighbours on level 0 |
| `ef_construction` | 200 | candidate list during insert |
| `ef_search` | 100 for `l2` and `dot`, 400 for `cosine` | per query, at least `k`; per-collection default (`PartitionConfig::ef_search`), per-query override |
| level multiplier | `1 / ln(M)` | levels drawn from a geometric distribution |
| level RNG | splitmix64 over `(seed, slot)` | `seed` is configured; `PartitionConfig::for_metric` derives it from the partition id. Deterministic levels, so the same log yields the same levels on every replica and in tests |

The defaults live in code, in one place: `HnswParams::for_metric`,
`hnsw::default_ef_search`, and `PartitionConfig::for_metric`, which the
portal should use to create collections. `HnswParams::default()` is the
`l2`/`dot` row. The cosine `ef_search` of 400 clears 0.97 recall on nytimes
(0.974) but not on glove (0.966; 0.974 at 500, 0.985 at 800), so the default
is a recorded open decision for the owner (raise it, or accept glove as the
hard case), not a met threshold.
| neighbour selection | heuristic (HNSW Algorithm 4) with pruned-candidate backfill | |
| entry point | the slot with the highest level; updated on insert | |

Inserts follow the standard greedy descent then `ef_construction` search per
level with bidirectional linking and pruning. Tombstoned slots stay linked
and are traversed but never scored into results. Construction treats them
as ordinary candidates and neighbours (as hnswlib does): excluding them
would leave a record inserted after mass deletion with no neighbours and
unreachable until compaction. Concurrency: `Partition` is a single-writer
`&mut self` API with no internal lock; a server keeps it behind a
read-write lock, queries take the read guard, and writes take the write
guard. A query never observes a partially linked slot because linking
completes before the high-water mark advances.

**Bulk build.** Bulk loads, compaction rebuilds, and benchmarks may use the
parallel builder: levels and the entry point are the same deterministic
function of `(seed, slot)`, but neighbour selection depends on thread
scheduling, so two parallel builds of one log are not byte-identical (recall
agreed within 0.001 in pass 3). A slot's own list is merged with the
back-links other threads added to it rather than overwritten, and a final
pass gives every level-0 slot without an in-link one (replacing only a link
whose target keeps another in-link), so no live record is unreachable after
a bulk load or compaction. The WAL apply path inserts sequentially and stays
deterministic. Because compaction uses the parallel builder, replicas that
compact independently hold different graphs; M4 must either ship the
primary's compacted snapshot or make the build deterministic before relying
on identical approximate results across replicas.

## 9. Filtered search

The allowed set is a roaring bitmap of **slots** in the partition's current
state (`Partition::search(.., Some(&bitmap))`). Slots are internal: an upsert
moves a key to a new slot, compaction renumbers every slot, and replicas
number slots differently, so a slot bitmap is valid only against the state
it was computed from, and `SearchHit::slot` is not a stable identifier. The
original intent, a bitmap of external keys translated by the partition, is
not representable (keys are byte strings) and is deferred: M4/M5 must add a
stable, portal-assigned `u32` record id to each slot (WAL and snapshot), have
the partition translate a record-id bitmap to slots under the same state it
searches, and remove slot-space filters from the public API before filters
carry authorization (M2 grants). Two modes, chosen per query:

- **Selective**: if `|allowed| ≤ T`, exact distances over the allowed slots
  only (the oracle path) with the same tie rule. **`T = 200 · ef_search`**,
  set from the pass 5 measurements (`docs/bench/README.md`): the latency
  crossover between the two routes was 294, 152, and 215 times `ef_search`
  on the three public sets, and masked traversal cost scales with `ef`, not
  `k` (the plan's `50 · k` hypothesis was two orders of magnitude low).
  `Hnsw::filter_cutover(ef)` implements the rule.
- **Broad**: masked traversal; excluded and tombstoned slots are routed
  through but never scored or returned. One mask type serves both purposes.

Filtered recall is measured against the oracle over the same allowed set at
every selectivity level (threshold ≥ 0.95). Pass 5 measured masked-route
mean recall between 0.983 and 1.000 at 0.1% to 50% selectivity; below the
cutover the exact route has recall 1.0 by construction.

## 10. Compaction

Recommended when `tombstones / slots ≥ 0.2` (`compaction_tombstone_ratio`,
exposed as `Partition::compaction_recommended`) or on operator request. The
engine never compacts on its own: the caller owns the trigger so it can be
scheduled against load (pass 7).

Compaction is two-phase (`begin_compaction` / `finish_compaction`):

1. **Begin**, in two steps. *Capture* (`capture_compaction(&self)`) freezes
   the rebuild point `S` = current high-water mark and copies the live slots
   at `S` in slot order, with the key table and the idempotency window; it
   holds the partition only for that memory copy (writers behind the lock
   wait for it). *Build* (`CompactionInput::build`) owns its input and
   borrows nothing, so it runs on another thread while readers and writers
   continue on the old state and the WAL. It builds a new graph assigning
   new dense slots in the same relative order, so tie order is preserved;
   external keys are unchanged. `begin_compaction(&self)` does both in one
   call and therefore blocks writers for the whole rebuild.
2. **Finish** (`&mut self`, an exclusive section). Replay WAL entries
   `(S, now]` into the staged state from the segments on disk; the replay
   must reach exactly the current high-water mark or the finish fails with
   `MissingHistory` and the old state stays authoritative. Write the staged
   state's snapshot at the current high-water mark (sync, rename). Then swap
   the served state atomically. The exclusive section is bounded by the
   catch-up replay plus one snapshot write, not by the rebuild.
3. Older snapshots and covered WAL segments become collectible per §4 rule 5.

Because the snapshot name is the high-water mark, a compaction that finishes
with no concurrent writes rewrites the snapshot the previous state already
had at that mark. The two files describe the same logical state (§5), so the
rename is a same-name atomic replace, never a loss of history.

Compaction is not admitted against the RAM budget: the rebuild holds a
second index (int8, graph, and a heap copy of the live `f32` rows) beside the
served one, which at 512 dims exceeds the 35% headroom. Until a measured
peak model gates `begin` (deferred), the caller must schedule compaction
only when the host has that memory.

A crash before the swap (`CompactionBeforeSwap`) leaves the old state
authoritative; on recovery the compacted snapshot is simply the newest
verifiable snapshot and recovers the same logical state. No acknowledged
write is lost because every acknowledged entry is in the WAL regardless of
which state it was applied to; no deleted record becomes visible because
tombstones are replayed into the staged state too.

**Archival.** `distr-hnsw index archive` commits a partition's manifest, WAL
segments, and newest verifiable snapshot, in that order, as ordinary
blob-plane files (M1 contract: 4 MiB encrypted chunks, RF2, immutable
manifests) under the idempotency key `index:<partition>:<part>:<name>:<blake3>`;
the portal records the part-to-file mapping in `index_archives` (schema v8).
The snapshot goes last, so an interrupted archive can leave an older
snapshot with newer segments (recoverable) but never a newer snapshot with
stale segments. Archive is **offline only**: it takes the partition lock
(failing with `Locked` while a server holds it), verifies the partition with
`inspect` (no repair, no writer), and keeps the lock until it finishes, so no
file changes between hashing and upload. A part counts as unchanged only when
its recorded hash matches and the recorded file is still visible in the blob
plane; otherwise it is committed again (as a new file, since a grown segment
or a re-upload after deletion is a different file). With `--truncate-wal`,
segments wholly below the archived snapshot are removed locally only after
their archive copy was verified in that run (§4 rule 5).

`index restore` takes the manifest, the newest archived snapshot, and the
segments it does not wholly cover (names validated against the expected
patterns), downloads them into a sibling staging directory, verifies each
BLAKE3, and runs a full recovery there; only a set that opens is renamed into
the destination (which must be absent or empty), so a failed restore leaves
nothing behind. Superseded archive files are never deleted: retention of
archived parts (deleting files covered by a newer committed snapshot through
the logical-delete path, and marking them as system files) is deferred to
M4, and GC and `portal delete` do not yet treat archived parts as
referenced.

## 11. Memory accounting and admission

Hot set per slot (RAM): `dims + 8` (int8, scale, norm) + graph links and
bookkeeping, **measured** in pass 4 as 182 bytes at M = 16 and 310 bytes at
M = 32 (`docs/bench/README.md`), + slot metadata (32) + key map (≈ 48) +
payload bytes when payloads are RAM-resident. This section is the single
source of the published formula:

    hot_bytes_per_vector = dims + 8 + graph(M) + 80 + payload
    graph(M) = 54 + 8·M      (the line through 182 at M = 16 and 310 at M = 32)

At 512 dims and M = 32 that is 910 bytes before payload. Each partition also
holds its idempotency window, about 40 bytes per entry (up to 40 MB at the
default `W`), counted in `resident_bytes` but not per vector. The `f32`
originals (`dims × 4`) and the snapshot file count against the **disk**
budget and the page cache, not the RAM budget, except while they are on the
heap between a write and the next snapshot (§7). Admission uses these
measured values, not the estimate.

A partition has an optional RAM budget (`ram_budget_bytes`; none means
unlimited) and reserves compaction headroom (`headroom_fraction`, default
35%). An upsert is refused with `PartitionError::OverBudget` (which reports
resident, projected, admissible, and budget bytes) when
`resident_bytes + hot_bytes_per_insert(payload) > budget × (1 − headroom)`,
where `resident_bytes` is the accounted int8, graph, key, slot, payload, and
idempotency footprint plus any `f32` rows not yet served from a snapshot,
and `hot_bytes_per_insert` is the published formula. `resident_bytes` is the
same before and after a reopen once a snapshot has been written. It is
recomputed by a scan on every budgeted upsert (O(n)); a running counter is a
deferred optimization that should land with group commit. Deletes are
always admitted because they only add a tombstone bit and must stay possible
when the partition is full; compaction then reclaims the space. Refusals
never lower durability or drop data, mirroring the M1 rule. The budget can
be changed at run time (`set_ram_budget`) without restarting the partition.
Disk budgeting is the host's job: the engine reports snapshot and WAL bytes
and the portal's capacity admission (M1) governs the archive copies.

## 12. Named crash points

The failpoint harness covers the points below. It is an **in-process**
model: the partition returns `Injected` at the boundary, the test drops it
without a clean close, and a new `Partition::open` recovers the directory.
That proves exactly-once replay and the recovery logic, but not durability:
the page cache keeps unsynced bytes across a drop, so it cannot catch a
missing or misplaced `fdatasync`. A subprocess kill harness and a
`dm-log-writes` run of the WAL and snapshot paths (reusing
`scripts/power-loss-drill.sh`) are still owed; until then host power loss
for the engine is an explicit M3 limitation.

| Point | Expected outcome after recovery |
|---|---|
| after WAL append, before sync | entry may be a torn tail; if so it was never acknowledged and is discarded |
| after sync, before apply | entry replays and is applied exactly once |
| after apply, before ack | same; the client retry with the same `op_id` deduplicates |
| snapshot temp written, before rename | temp ignored; recovery from the previous snapshot plus WAL |
| after snapshot rename, before WAL truncation | new snapshot used; older segments still present and harmless |
| compaction: new snapshot written, before swap | recovery uses the newest verifiable snapshot; the same logical state either way |
| compaction: after swap, before old-state cleanup | old files harmless; collected later |

Corruption cases that must fail closed: bad segment header, bad mid-segment
entry (including a damaged `len` field anywhere before the last entry), any
damage in a sealed segment (with nothing truncated), `seq` gap, a WAL that
ends below the snapshot's high-water mark, a missing or unverifiable
bulk-load base snapshot, snapshot header CRC, any section BLAKE3, footer
hash, a snapshot that disagrees with `partition.json` or holds a dangling
reference, a snapshot for another partition id, an unsupported version.
Cases that must recover as torn tails: a short or CRC-failing final entry, a
zero-filled tail, and a newest segment whose header was never written.

## 13. Out of scope for this contract

Replication, epochs, promotion, routing, and multi-partition queries (M4);
the portal API surface and payload JSON schema (M5); the archive schedule
through the blob plane beyond the object classes named above.

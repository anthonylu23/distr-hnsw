# M3 engine contract

Status: **Proposed** (2026-09-30, pass 2 of
[m3-implementation-plan.md](m3-implementation-plan.md)). This document pins
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
non-finite component are rejected at insert and query time.

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
`seq` is returned. An `op_id` older than the window is applied again, which
is safe because upsert and delete are idempotent in effect and only the
version changes.

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
3. On open, a segment is read to its end. A final entry that is short, has a
   wrong `len`, or fails its CRC is a **torn tail**: it is discarded and the
   file is truncated to the last good entry, which is logged. Any bad entry
   that is not the last one, or a bad header, **fails closed** with a
   diagnosis naming the segment and offset; the partition does not serve.
4. Entries with `seq` at or below the loaded snapshot's high-water mark are
   skipped during replay; every later entry is applied exactly once in
   order.
5. Segments wholly covered by a durable snapshot are eligible for archival
   to the blob plane (M1 object class `wal_segment`) and local truncation,
   in that order; a segment is never deleted before its archive copy is
   verified (M1 copy-first rule).

## 5. Snapshot

A snapshot is one file `snap/<high_water_seq, 20 digits>.snap`, written to a
temporary name, synced, renamed, with the directory synced (the M1 durable
write sequence). Sections are 64-byte aligned so the file can be memory
mapped and served from the page cache when a partition is demoted from RAM.

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
  5 vectors_i8  slot_count × (scale f32 | norm_sq f32 | int8 × dims)
  6 graph       entry_slot u32 | max_level u8 | per slot: level u8, then per level: count u16 | neighbours u32 × count
  7 idempotency window × (op_id [16] | seq u64)
footer
  file_blake3 [32] over everything before the footer
```

Loading verifies the header CRC, every section's BLAKE3, and the footer; a
mismatch fails closed with the section named. The newest snapshot whose
checks pass is used; an older one is used only when the newer fails and the
operator is told. A snapshot is content-addressed by its BLAKE3 when archived
(M1 object class `snapshot`).

## 6. Recovery

`open(partition_dir)`:

1. Load the newest verifiable snapshot (or start empty at `seq` 0).
2. Replay WAL entries with `seq` greater than the snapshot high-water mark,
   in order, exactly once, rebuilding slots, vectors, graph insertions,
   tombstones, and the idempotency window. Stop at a torn tail.
3. The committed high-water mark becomes the last replayed `seq`. Serving
   begins only after replay completes.

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
recall cost of int8 (threshold: within 0.01 of the `f32` graph) is measured
in pass 4; if the graph itself ever needs re-quantization, the `f32`
originals make that a rebuild, not a data loss.

## 8. HNSW parameters and construction

| Parameter | Default | Notes |
|---|---|---|
| `M` | 16 for `l2` and `dot`, 32 for `cosine` | max neighbours per node on upper levels; set from the pass 3 measurements (`docs/bench/README.md`) |
| `M0` | `2 · M` | max neighbours on level 0 |
| `ef_construction` | 200 | candidate list during insert |
| `ef_search` | 100 for `l2` and `dot`, 400 for `cosine` | per query, at least `k`; per-collection default, per-query override |
| level multiplier | `1 / ln(M)` | levels drawn from a geometric distribution |
| level RNG | seeded by `(partition_id, slot)` | deterministic levels, so the same log yields the same graph on every replica and in tests |
| neighbour selection | heuristic (HNSW Algorithm 4) with pruned-candidate backfill | |
| entry point | the slot with the highest level; updated on insert | |

Inserts follow the standard greedy descent then `ef_construction` search per
level with bidirectional linking and pruning. Tombstoned slots stay linked
and are traversed but never scored into results. Concurrency: queries take a
read guard on the partition state; inserts serialize on the writer; a query
never observes a partially linked slot because linking completes before the
high-water mark advances.

**Bulk build.** Bulk loads, compaction rebuilds, and benchmarks may use the
parallel builder: levels and the entry point are the same deterministic
function of `(seed, slot)`, but neighbour selection depends on thread
scheduling, so two parallel builds of one log are not byte-identical (recall
agreed within 0.001 in pass 3). The WAL apply path inserts sequentially and
stays deterministic.

## 9. Filtered search

The portal supplies the allowed set as a roaring bitmap of external keys,
translated to live slots by the partition. Two modes, chosen per query:

- **Selective**: if `|allowed| ≤ T`, exact distances over the allowed slots
  only (the oracle path) with the same tie rule. `T = c · k`, with `c` set
  from the pass 5 measurements at 0.1%, 1%, 10%, and 50% selectivity;
  hypothesis `c ≈ 50`.
- **Broad**: masked traversal; excluded and tombstoned slots are routed
  through but never scored or returned. One mask type serves both purposes.

Filtered recall is measured against the oracle over the same allowed set at
every selectivity level (threshold ≥ 0.95).

## 10. Compaction

Triggered when `tombstones / slots ≥ 0.2` (configurable) or by an operator.

1. Freeze a rebuild point `S` (current high-water mark) and write a
   snapshot at `S` if none exists.
2. Build a new state from the live slots at `S` in slot order, assigning new
   dense slots in the same relative order so tie order is preserved;
   external keys are unchanged. Writes continue to the old state and the WAL.
3. Pause writes briefly; apply entries `(S, now]` to the new state; write
   the new state's snapshot at the current high-water mark; sync; rename.
4. Swap the served state atomically (readers holding the old guard finish
   on the old state). Resume writes.
5. Older snapshots and covered WAL segments become collectible per §4 rule 5.

A crash before step 4 leaves the old state authoritative and the new
snapshot file is ignored unless its high-water mark is the newest verifiable
one, in which case it is simply the recovery point. No acknowledged write is
lost because every acknowledged entry is in the WAL regardless of which
state it was applied to; no deleted record becomes visible because
tombstones are replayed into the new state too.

## 11. Memory accounting and admission

Per slot: `dims × 4` (f32) + `dims + 8` (int8, scale, norm) + key bytes +
payload bytes + graph links (`M0 × 4` at level 0 plus `M × 4` per upper
level, expected `≈ (M0 + M/(ln M − 1)) × 4`) + slot metadata (32) + key map
(≈ 48). The overhead factor over `vectors × dims × bytes_per_dim` is
**measured** in pass 4 and published; admission uses the measured value.

A partition has a RAM budget and a disk budget. It reserves compaction
headroom (default 35% of the budget, revised from measurement) and refuses
new upserts with a distinct capacity error when the accounted size plus the
next insert would exceed `budget − headroom`. Deletes are always admitted.
Refusals never lower durability or drop data, mirroring the M1 rule.

## 12. Named crash points

The failpoint harness (M1 style: the process exits at the boundary and a
fresh process recovers) covers:

| Point | Expected outcome after recovery |
|---|---|
| after WAL append, before sync | entry may be a torn tail; if so it was never acknowledged and is discarded |
| after sync, before apply | entry replays and is applied exactly once |
| after apply, before ack | same; the client retry with the same `op_id` deduplicates |
| snapshot temp written, before rename | temp ignored; recovery from the previous snapshot plus WAL |
| after snapshot rename, before WAL truncation | new snapshot used; older segments still present and harmless |
| compaction: new snapshot written, before swap | recovery uses the newest verifiable snapshot; content identical either way |
| compaction: after swap, before old-state cleanup | old files harmless; collected later |

Corruption cases that must fail closed: bad segment header, bad mid-segment
entry, `seq` gap, snapshot header CRC, any section BLAKE3, footer hash, a
snapshot for another partition id, an unsupported version.

## 13. Out of scope for this contract

Replication, epochs, promotion, routing, and multi-partition queries (M4);
the portal API surface and payload JSON schema (M5); the archive schedule
through the blob plane beyond the object classes named above.

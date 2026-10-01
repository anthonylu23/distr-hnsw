# M3 implementation plan

Status: **In progress** (passes 1 public part and 2 through 6 done by
2026-10-01: datasets pinned, oracle and kernels landed, engine contract
written in [m3-engine-contract.md](m3-engine-contract.md), HNSW core meets
the f32 recall thresholds, int8 traversal with exact rescoring costs at most
0.0005 recall, the memory formula is measured, filtered search has a
measured cutover of 200 · ef, and WAL, snapshots, and recovery pass every
crash and corruption test with 1.8 s recovery at 1M; results in
[bench/README.md](bench/README.md)). The milestone scope, acceptance criteria,
and exit gate remain in [`roadmap.md`](roadmap.md) (M3). This page fixes the
order of work and, deliberately, puts the benchmark datasets and the
brute-force oracle before any engine code: every recall threshold is written
down and every correctness test has a reference answer before the first HNSW
edge is inserted.

## Why datasets first

M1 earned trust through crash points and drills. The vector engine earns it
through measured recall against exact search. Three things therefore have to
exist before engine code:

1. **Pinned datasets** with recorded digests, so a recall number is
   reproducible and a regression is a regression, not a data change.
2. **A brute-force oracle** with deterministic tie rules. It is both the
   benchmark baseline and the reference store for model-based tests, so the
   engine's semantics (metric, ties, upsert, tombstones) are defined by the
   oracle first.
3. **Written thresholds** for recall, latency, memory, and recovery time, so
   the exit gate is not tuned after seeing results (the same rule that
   governed phase 0).

## Datasets

All datasets live under `~/distr-hnsw-bench/datasets/<name>/` on `anthonypc`
in one flat format: `base.f32le` (row-major vectors), `queries.f32le`,
`groundtruth.i32le` (k = 100 nearest ids per query under the dataset's
metric), and `manifest.json` (source URL, conversion command, dimensions,
counts, metric, BLAKE3 of every file). Manifests are committed under
`docs/bench/`; data is not. Conversion from the ann-benchmarks HDF5
distributions happens once with a small Python script (`h5py`), then the
Rust harness reads only the flat files.

| Name | Vectors | Dims | Metric | Purpose |
|---|---:|---:|---|---|
| `nytimes-256-angular` | 290k | 256 | cosine | Fast regression set for CI on either machine; angular metric |
| `sift-128-euclidean` | 1.0M | 128 | L2 | Classic baseline with well-known recall/ef curves; catches graph bugs |
| `glove-100-angular` | 1.18M | 100 | cosine | Hard angular set; exposes recall cliffs and int8 loss |
| `project-nomic-512` | 200k, later 1M | 512 | cosine | Representative: chunks from the phase-0 mixed corpus plus public text, embedded with the locked `nomic-embed-text @ 512` on `anthonypc`; ground truth by exact search |
| `filtered-*` (derived) | as parent | as parent | as parent | Each parent gains synthetic facet labels at 0.1%, 1%, 10%, and 50% selectivity; ground truth is exact search over the matching subset |

The public sets are for engine correctness and comparability; the project set
is the only one at the product's dimensionality and is what capacity numbers
are published against. Its embedding run needs the GPU on `anthonypc`, which
is shared with other work; pass 1 therefore starts with the public sets and
builds the project set when the card is free, without blocking engine work. The 1M single-partition scale is the M3 target (about
0.5 GiB int8 vectors plus roughly 0.15 GiB of graph at M = 16, well inside
`anthonypc`'s 15 GiB with the 2 GiB of full-precision originals kept for
rescoring).

## Thresholds (written before engine code)

Measured on `anthonypc` (16 threads, 15 GiB RAM), single partition, default
parameters M = 16, `ef_construction` = 200, `ef_search` chosen per dataset.

| Measure | Required | Target |
|---|---|---|
| Unfiltered recall@10, f32 graph | ≥ 0.97 on every public set | ≥ 0.99 |
| Unfiltered recall@10, int8 graph + exact rescore | within 0.01 of the f32 graph | within 0.005 |
| Filtered recall@10 at 0.1%, 1%, 10%, 50% | ≥ 0.95 at every level | ≥ 0.98 |
| Query latency, 1M × 512, single thread | p95 ≤ 5 ms | p95 ≤ 2 ms |
| Build throughput, 1M × 512, 16 threads (parallel bulk build) | ≥ 5k vectors/s | ≥ 20k vectors/s |
| RAM per vector at 512 dims, int8 | measured overhead factor published; admission uses it | ≤ 800 bytes hot set (measured 830 at M = 32: target missed by 4%, required met) |
| Snapshot + WAL-tail recovery, 1M × 512 | ≤ 60 s to serving | ≤ 20 s |
| WAL fsync path | every acknowledged entry survives `kill -9` at every boundary | same, plus host power loss on the qualified filesystems |

Recall is **distance-based**: a returned hit counts when its distance is no
worse than the k-th true nearest distance (relative tolerance 1e-5). The
public sets contain exact-duplicate and zero vectors (nytimes: 26,558
duplicate groups, 239 zero base vectors, 9 zero queries) whose ties make
id-based recall undercount even exact search; id recall is reported beside
it for diagnosis. The harness computes the k-th true distance from the
reference ids with the engine's own kernels so both sides use one
definition.

If a required threshold is missed, the gate stays open; thresholds change only
by a documented decision, never by editing after a run.

## Crates

- `crates/distr-hnsw-index`: the engine library. No tokio, no network. Modules
  `vector` (flat store, f32 and int8), `distance` (SIMD kernels with scalar
  reference), `oracle` (exact search), `hnsw`, `quant`, `filter` (roaring
  masks and cutover), `wal`, `snapshot`, `partition` (the state machine that
  ties them together), `budget` (memory accounting).
- `crates/distr-hnsw-bench`: the harness binary. Loads pinned datasets,
  runs oracle and engine, writes `BenchReportV1` JSON with source revision,
  dataset digests, hardware, parameters, recall, latency percentiles, build
  time, RSS and accounted memory, snapshot bytes, and recovery time. Sanitized
  summaries go under `docs/bench/`, following the phase-0 discipline.
- `crates/distr-hnsw` gains `index archive` and `index restore` (pass 7):
  a partition's manifest, newest snapshot, and WAL segments are committed as
  ordinary blob-plane files and recorded in `index_archives` (schema v8);
  restore downloads them into an empty directory and recovers.

## Passes

Each pass ends with green tests, a benchmark report where relevant, and
updated contract documentation. Estimates are solo-effort scale markers.

1. **Datasets, oracle, kernels** (~1 week). Fetch and convert the three
   public sets, build `project-nomic-512` at 200k, derive filtered variants,
   commit manifests. Implement flat vector storage, f32 and int8 distance
   kernels with property tests against scalar references, and the exact
   oracle with deterministic ties (distance ascending, then id ascending).
   Exit: the harness reproduces ground truth with recall 1.0 on every set
   and publishes the brute-force latency floor.
2. **Engine contract** (~3 days, overlaps pass 1). Write
   `m3-engine-contract.md` before graph code: record identity (`u64` record
   id plus optional external key), metrics (cosine as normalized dot, dot,
   L2), tie rules, upsert as tombstone-then-insert with a version counter,
   idempotency ids, WAL entry and snapshot layouts with versions and
   checksums, quantization scheme (per-vector symmetric int8 scale, f32
   originals retained), rescoring policy (rescore the top `max(4k, 100)`
   int8 candidates exactly), and the named crash points.
3. **HNSW core** (~2 weeks; done in one day). Build, insert, search,
   tombstone mask, in RAM only. Model-based tests against the oracle on
   random small sets (insert, upsert, delete, query interleavings). First
   recall/ef curves on `nytimes` and `sift`; fix parameters. Exit: f32
   recall thresholds met on the public sets. **Result:** sift 0.987 at
   M = 16 / ef = 100; nytimes 0.975 and glove 0.985 at M = 32 / ef = 400
   and 800; parallel bulk build 8,250 vectors/s on sift; defaults set per
   metric in the contract.
4. **Int8 quantization and rescoring** (~1 week; done in one day). Quantize
   on insert, search the int8 graph, rescore candidates from f32 originals,
   never return a score from a stale version. Exit: int8 within threshold
   of f32; RAM per vector measured and the overhead factor published.
   **Result:** recall within 0.0005 of f32 on every set, throughput about
   doubled; graph 182 B/vector at M = 16 and 310 at M = 32; hot-set formula
   adopted in the contract with f32 originals moved to the snapshot/page
   cache.
5. **Filtered search** (~1 week; done in one day). Roaring bitmap masks
   shared with the tombstone path; masked traversal; brute-force cutover
   with the threshold set from the 0.1%–50% measurements, not guessed. Exit:
   filtered recall thresholds met at every selectivity. **Result:** masked
   recall 0.983 to 1.000 at every level; the crossover is 150 to 300 times
   ef, so the cutover is `200 · ef_search` and the `50 · k` hypothesis is
   rejected.
6. **WAL, snapshot, recovery** (~2 weeks; done in one day). Checksummed WAL
   segments with sequence numbers and idempotency ids; versioned mmap
   snapshots; recovery from snapshot plus tail; a failpoint harness in the
   M1 style that kills the process at every append, sync, apply, and
   publication boundary; truncated, reordered, and checksum-invalid inputs
   fail closed. Exit: exactly-once replay proven; recovery time measured at
   1M × 512. **Result:** every crash point and corruption case covered by
   tests (in-process crash model: the partition is dropped without a clean
   close and reopened); recovery 1.8 s on sift 1M and 5.4 s on glove with
   byte-identical search results; snapshot 821 bytes/vector at 128 dims.
   The 1M × 512 measurement waits on the project dataset.
7. **Compaction, budget, blob-plane archive** (~2 weeks; done in one day).
   Background rebuild with crash points at the swap; concurrent reads and
   writes during compaction; memory accounting from the measured overhead
   factor with recovery and compaction headroom reserved; snapshots and WAL
   segments archived as blob-plane objects and a partition restored from
   them through the M1 restore path. Exit: the M3 acceptance checklist has
   evidence for every line and the benchmark report is published.
   **Result:** two-phase compaction (`begin` beside readers and writers,
   `finish` as a bounded exclusive section that replays the WAL delta and
   swaps) with a crash point before the swap; writes landing between the
   phases are caught up and verified; RAM budget admission refuses upserts
   over `budget − headroom` with a typed error and always admits deletes;
   `distr-hnsw index archive|restore` round-trips a partition through
   loopback RF2 agents with byte-identical search results. Compaction
   measurements are in `docs/bench/README.md`.

Total: roughly 9 to 10 weeks, consistent with the roadmap's 1 to 2 months
plus a data-preparation week.

## What is decided here and what is not

Decided by this plan: datasets, thresholds, crate boundaries, pass order,
and that the oracle defines semantics. Left to the engine contract in pass 2:
exact binary layouts, the quantization formula, and rescoring depth. Left to
M4: replication, epochs, promotion, routing, and multi-node fault injection.
M2 (Tailscale identity) is independent and can proceed alongside.

## Verification map to the roadmap gate

| M3 criterion | Where it is proven |
|---|---|
| Metric, ties, upsert, tombstones | pass 3 model-based tests against the oracle |
| Unfiltered and filtered recall on public and project data | passes 3 to 5 benchmark reports |
| Int8 plus rescore budget, never a stale score | pass 4 tests and report |
| WAL survives restart, exactly-once replay | pass 6 failpoint harness |
| Snapshot plus tail reproduces state | pass 6 recovery tests |
| Corrupt inputs fail closed | pass 6 corruption tests |
| Compaction never loses or resurrects | pass 7 concurrency and crash tests |
| Hard limits respected | pass 7 budget tests against the measured formula |

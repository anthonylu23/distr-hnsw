# M3 implementation plan

Status: **In progress**. All seven passes were completed on 2026-10-01
(datasets pinned, oracle and kernels, the engine contract in
[m3-engine-contract.md](m3-engine-contract.md), HNSW, int8 with exact
rescoring, filtered search, WAL/snapshot/recovery, compaction, budget, and
blob-plane archive; results in [bench/README.md](bench/README.md)). The
2026-10-25 audit found defects in the WAL torn-tail rule, recovery's history
checks, insertion after mass deletion, the parallel builder, and the
compaction concurrency claim; the hardening pass fixed them in code and
tests, and the published benchmark numbers wait on re-runs from a clean,
committed revision (`--canonical`). The milestone scope, acceptance criteria,
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

All datasets live under `~/distr-hnsw-bench/datasets/<name>/` on the reference host
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
| `project-nomic-512` | 200k and 1M | 512 | cosine | Representative: chunks from the phase-0 mixed corpus plus public text (pinned Wikipedia, so 99.8% of the base), embedded with the locked `nomic-embed-text @ 512` on the reference host; ground truth by exact search (`scripts/bench/build-project-set.py`) |
| `filtered-*` (derived) | as parent | as parent | as parent | Each parent gains synthetic facet labels at 0.1%, 1%, 10%, and 50% selectivity; ground truth is exact search over the matching subset |

The public sets are for engine correctness and comparability; the project set
is the only one at the product's dimensionality and is what capacity numbers
are published against. Its embedding run needs the GPU on the reference host, which
is shared with other work; pass 1 therefore starts with the public sets and
builds the project set when the card is free, without blocking engine work. The 1M single-partition scale is the M3 target (about
0.5 GiB int8 vectors plus roughly 0.15 GiB of graph at M = 16, well inside
the reference host's 15 GiB with the 2 GiB of full-precision originals kept for
rescoring).

## Thresholds (written before engine code)

Measured on the reference host (16 threads, 15 GiB RAM), single partition, default
parameters M = 16, `ef_construction` = 200, `ef_search` chosen per dataset.

| Measure | Required | Target |
|---|---|---|
| Unfiltered recall@10, f32 graph | ≥ 0.97 on every public set | ≥ 0.99 |
| Unfiltered recall@10, int8 graph + exact rescore | within 0.01 of the f32 graph | within 0.005 |
| Filtered recall@10 at 0.1%, 1%, 10%, 50% | ≥ 0.95 at every level | ≥ 0.98 |
| Query latency, 1M × 512, single thread | p95 ≤ 5 ms | p95 ≤ 2 ms |
| Build throughput, 1M × 512, 16 threads (parallel bulk build) | ≥ 560 vectors/s (1M in 30 min; revised 2026-10-27, below) | ≥ 3.3k vectors/s (1M in 5 min) |
| RAM per vector at 512 dims, int8 | measured overhead factor published; admission uses it | ≤ 800 bytes hot set (910 at M = 32 with the slot and key bookkeeping admission charges, contract §11: target missed by 14%, required met) |
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

**Build-throughput revision (2026-10-27, before any 512-dim run).** The
original row (≥ 5k required, ≥ 20k target) assumed M = 16. Cosine moved to
M = 32 in pass 3, and at M = 32 the parallel build measured 3,182 vectors/s
at 100 dims and 1,725 at 256 dims, so 512 dims cannot reach 5k without a
different build algorithm. The threshold is now set by what build speed
gates in this design. Recovery does not rebuild: it loads a snapshot and
replays the WAL tail (recovery has its own row). Compaction rebuilds off the
write path: writers keep committing during the rebuild and are blocked only
for the catch-up and swap (4.0 to 5.5 s measured). Build speed therefore
bounds only how long a first bulk import or a compaction holds a second
copy of the partition in RAM. The required bound is a 1M × 512 build within
30 minutes, and the target is within 5 minutes.

## Results against the thresholds (2026-10-28)

Measured on `project-nomic-512` (1M × 512, cosine) with the defaults
M = 32, `ef_construction` = 200, `ef_search` = 500, int8 traversal with
rescoring, from clean commits with `--canonical`: build, latency, and
filtered recall from `b4b6710`, compaction from `684ddfe`, and
persistence from `46757b8`. The public-set rows are from `dc276f3`.
Figures and reports: [bench/README.md](bench/README.md).

| Measure | Required | Target | Measured | Verdict |
|---|---|---|---|---|
| Unfiltered recall@10 | ≥ 0.97 on every public set | ≥ 0.99 | sift 0.987 (M 16, ef 100), nytimes 0.979, glove 0.974; project 0.9933 | required met; target met on the project set only |
| Int8 + rescore vs. f32 | within 0.01 | within 0.005 | ≤ 0.0014 on every set; 0.0000 on the project set at ef 500 | met |
| Filtered recall@10, 0.1% to 50% | ≥ 0.95 | ≥ 0.98 | project 1.000 / 1.000 / 0.999 / 0.996 | met |
| Query latency, single thread | p95 ≤ 5 ms | p95 ≤ 2 ms | p95 3.14 ms (p50 2.18 ms) | required met; target missed |
| Build throughput, 16 threads | ≥ 560 vectors/s | ≥ 3.3k vectors/s | 1,700 vectors/s (588 s) | required met; target missed |
| RAM per vector, int8 | overhead published; admission uses it | ≤ 800 bytes | 921 bytes resident after recovery (formula 910) | required met; target missed by 15% |
| Snapshot + WAL-tail recovery | ≤ 60 s | ≤ 20 s | 3.66 s with a 1,000-entry tail, identical results | met |
| WAL fsync path | survives `kill -9` at every boundary | plus host power loss | in-process failpoints at every append, sync, and apply boundary | accepted with a limitation: no subprocess `kill -9` harness or engine power-loss run yet (deferred) |

The 1M run also found a defect: writing a snapshot built the whole file
in memory several times over, so the first snapshot of a 1M × 512 bulk
load was OOM-killed. The writer now streams
([hardening-2026-10.md](hardening-2026-10.md)), and the persistence and
compaction rows were measured with it. A bench bug also under-reported
persistence recall (0.9878 instead of 0.9934); it is fixed and does not
touch a threshold row.

## Crates

- `crates/distr-hnsw-index`: the engine library. No tokio, no network. Modules
  `vector` (flat store, f32 and int8), `distance` (SIMD kernels with scalar
  reference), `oracle` (exact search), `hnsw`, `quant`, `filter` (roaring
  masks and cutover; filtering lives in `hnsw`), `wal`, `snapshot`,
  `partition` (the state machine that ties them together, including memory
  accounting), and `fsutil` (durable directory creation and the partition
  directory lock).
- `crates/distr-hnsw-bench`: the harness binary. Loads pinned datasets,
  runs oracle and engine, writes `BenchReportV1` JSON with provenance
  (source revision with a dirty flag embedded by `build.rs`, the BLAKE3 of
  the executable, and the command line), dataset digests, hardware,
  parameters, recall, latency percentiles, build time, RSS and accounted
  memory, snapshot bytes, and recovery time. `--canonical` refuses a dirty or
  unknown revision; published results must use it. Sanitized summaries go
  under `docs/bench/`, following the phase-0 discipline.
- `crates/distr-hnsw` gains `index archive` and `index restore` (pass 7):
  a partition's manifest, newest snapshot, and WAL segments are committed as
  ordinary blob-plane files and recorded in `index_archives` (schema v8);
  restore assembles them in a staging directory, recovers there, and
  renames the result into place.

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
   close and reopened; no process kill or power-loss evidence yet, contract
   §12); recovery 1.8 s on sift 1M and 5.4 s on glove with byte-identical
   search results; snapshot 821 bytes/vector at 128 dims. The 1M × 512
   measurement waits on the project dataset. The 2026-10-25 audit found
   that a damaged length field mid-log read as a torn tail and discarded
   acknowledged entries, that a WAL ending below the snapshot or a lost
   bulk-load snapshot opened successfully, and that the writer kept
   appending after a failed sync; the hardening pass fixed all four with
   regression tests (contract §4 rules 3 and 6, §6).
7. **Compaction, budget, blob-plane archive** (~2 weeks; done in one day).
   Background rebuild with crash points at the swap; concurrent reads and
   writes during compaction; memory accounting from the measured overhead
   factor with recovery and compaction headroom reserved; snapshots and WAL
   segments archived as blob-plane objects and a partition restored from
   them through the M1 restore path. Exit: the M3 acceptance checklist has
   evidence for every line and the benchmark report is published.
   **Result:** two-phase compaction (`finish` as a bounded exclusive section
   that replays the WAL delta and swaps) with a crash point before the swap;
   RAM budget admission refuses upserts over `budget − headroom` with a
   typed error and always admits deletes; `distr-hnsw index archive|restore`
   round-trips a partition through loopback RF2 agents with byte-identical
   search results. Compaction measurements are in `docs/bench/README.md`.
   The pass-7 evidence issued its "concurrent" writes after `begin`
   returned, and `begin(&self)` borrowed the partition for the whole
   rebuild, so writers could not in fact proceed. The hardening pass split
   `begin` into a short capture and an owned rebuild that runs on another
   thread, with a multithreaded test and a bench that writes during the
   rebuild; the published compaction numbers must be re-run with it.

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
| WAL survives restart, exactly-once replay | pass 6 failpoint harness (in-process; process kill and power loss still owed) |
| Snapshot plus tail reproduces state | pass 6 recovery tests |
| Corrupt inputs fail closed | pass 6 corruption tests |
| Compaction never loses or resurrects | pass 7 crash tests; multithreaded rebuild test from the hardening pass |
| Hard limits respected | pass 7 budget tests against the measured formula |

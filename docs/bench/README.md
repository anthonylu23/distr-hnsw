# Benchmark datasets and baselines

Manifests for the pinned datasets the M3 engine is measured against
([m3-implementation-plan.md](../m3-implementation-plan.md)). Data files live
under `~/distr-hnsw-bench/datasets/<name>/` on `anthonypc`; each manifest
records the source, conversion command, shape, metric, and BLAKE3 of every
file, and the harness refuses a dataset whose bytes do not match.

| Dataset | Vectors | Dims | Metric | Queries | Ground truth |
|---|---:|---:|---|---:|---|
| `nytimes-256-angular` | 290,000 | 256 | cosine | 10,000 | 100-NN |
| `sift-128-euclidean` | 1,000,000 | 128 | L2 | 10,000 | 100-NN |
| `glove-100-angular` | 1,183,514 | 100 | cosine | 10,000 | 100-NN |
| `project-nomic-512-200k` | 200,000 | 512 | cosine | 10,000 | 100-NN |
| `project-nomic-512` | 1,000,000 | 512 | cosine | 10,000 | 100-NN |

## Provenance

Every table below was measured on `anthonypc` (16 threads, 15 GiB RAM)
with `--canonical` builds from clean commits. The public sets ran on
2026-10-27 from `dc276f3`, except nytimes compaction, which comes from
`835f055` (that commit changes only the persistence and compaction
benches, so they skip zero-norm rows and queries). Every persistence row
was re-run on 2026-10-28 from `46757b8`, after a recall-verification fix.
The project set's provenance is given in its section. Each report records `source_revision`,
`source_dirty: false`, the executable's BLAKE3, and the command line.
Reports: `~/distr-hnsw-bench/reports/canonical-<rev>/`.

The 2026-09-30 and 2026-10-01 figures that these replace carried no usable
source revision (19 of 20 said `unknown`). Where a figure is comparable,
the canonical run reproduces it within noise.

Latency is single-thread over a 200-query sample, quoted as p50 / p95 in
milliseconds unless noted. Batch throughput uses all 16 threads. Recall is
distance-based and quoted with its `ef`.

## Oracle baseline

Exact search with the engine's own kernels, k = 10, first 1,000 queries.
Id recall quantifies tie effects from duplicate vectors.

| Dataset | Recall@10 | Min per query | Id recall | p50 / p95 (ms) | Batch queries/s |
|---|---:|---:|---:|---:|---:|
| `nytimes-256-angular` | 1.000 | 1.000 | 0.985 | 14.5 / 16.8 | 351 |
| `sift-128-euclidean` | 1.000 | 1.000 | 0.999 | 26.7 / 30.6 | 101 |
| `glove-100-angular` | 1.000 | 1.000 | 1.000 | 24.6 / 27.0 | 137 |

Batch throughput is 17% to 25% below the pass 1 run (466 / 121 / 166),
while single-thread latency matches within 2%. The cause was not
investigated. The batch scan is memory-bound and sensitive to other load on
the host.

## HNSW, f32 graph

k = 10, 2,000 queries, `ef_construction` = 200. Bytes per vector are the
engine's accounted hot set plus the f32 originals (memory table below).

| Dataset | M, build | Build | Bytes/vector | ef | Recall@10 | p50 / p95 (ms) | Batch q/s |
|---|---|---:|---:|---:|---|---|---:|
| `sift-128-euclidean` | 16, parallel | 123 s, 8,114 v/s | 830 | 50 / 100 / 200 | 0.958 / 0.987 / 0.997 | 0.20 / 0.26, 0.34 / 0.41, 0.63 / 0.84 | 23,600 / 13,400 / 7,900 |
| `sift-128-euclidean` | 16, sequential | 641 s, 1,561 v/s | 830 | 50 / 100 / 200 | 0.959 / 0.988 / 0.998 | 0.20 / 0.24, 0.38 / 0.50, 0.61 / 0.74 | 24,400 / 13,600 / 6,900 |
| `nytimes-256-angular` | 16, sequential | 319 s, 908 v/s | 1,470 | 50 / 100 / 200 | 0.849 / 0.887 / 0.916 | 0.22 / 0.35, 0.53 / 0.74, 0.76 / 0.97 | 13,900 / 10,200 / 5,600 |
| `nytimes-256-angular` | 32, parallel | 168 s, 1,725 v/s | 1,598 | 100 / 200 / 400 / 500 / 600 / 800 | 0.929 / 0.954 / 0.974 / 0.979 / 0.984 / 0.990 | 0.70 / 0.96, 1.31 / 1.70, 2.64 / 3.21, 3.42 / 4.03, 3.93 / 4.60, 5.18 / 6.00 | 5,700 / 3,100 / 1,600 / 1,200 / 1,100 / 770 |
| `glove-100-angular` | 16, sequential | 857 s, 1,381 v/s | 690 | 50 / 100 / 200 / 400 | 0.758 / 0.829 / 0.881 / 0.922 | 0.18 / 0.26, 0.32 / 0.44, 0.58 / 0.72, 1.07 / 1.33 | 27,600 / 16,500 / 9,300 / 5,100 |
| `glove-100-angular` | 32, parallel | 372 s, 3,182 v/s | 818 | 100 / 200 / 400 / 500 / 600 / 800 | 0.894 / 0.937 / 0.966 / 0.974 / 0.979 / 0.985 | 0.58 / 0.80, 0.99 / 1.25, 1.81 / 2.35, 2.17 / 2.86, 2.60 / 3.52, 3.29 / 4.38 | 8,100 / 5,100 / 2,800 / 2,100 / 1,900 / 1,500 |

Reading:

- The L2 set clears the 0.97 required recall at M = 16 with ef = 100
  (0.987). Sequential and parallel builds agree within 0.001 recall, and
  the parallel build is 5.2 times faster.
- The angular sets need M = 32. nytimes clears 0.97 at ef = 400 (0.974) and
  glove at ef = 500 (0.974). At ef = 400, glove reaches 0.966, which is
  below the target.
- Every configuration is one to two orders of magnitude faster than the
  exact oracle at the same recall band.

Defaults (contract §8): `M = 16` for L2 and dot, `M = 32` for cosine;
`ef_search` 100 for L2/dot and 500 for cosine, per-query overridable, in
`HnswParams::for_metric` and `hnsw::default_ef_search`. The cosine default
was raised from 400 on 2026-10-27 because 400 misses 0.97 on glove (0.966);
500 clears both angular sets (0.979 and 0.974) at about 25% more latency
(nytimes int8 p50 2.00 → 2.46 ms, glove 1.63 → 2.04 ms). The persistence
and compaction rows below ran at ef = 400, the default at the time.

**Build throughput.** The plan's 1M × 512 threshold assumed M = 16 and
required ≥ 5k vectors/s. At M = 32 the parallel build reaches 3,182
vectors/s at 100 dims and 1,725 at 256 dims. On 2026-10-27, before any
512-dim run, the threshold was revised to ≥ 560 vectors/s required (1M in
30 minutes) and ≥ 3.3k targeted, because recovery never rebuilds and
compaction rebuilds off the write path
([m3-implementation-plan.md](../m3-implementation-plan.md)). At 512 dims
the 1M project set builds at 1,700 vectors/s (588 s), which meets the
requirement and misses the target ([Project set](#project-set-project-nomic-512)).

## Int8 traversal with exact rescoring

Same graphs as the parallel rows above, 2,000 queries, k = 10, rescoring
the best `max(4k, 100)` = 100 int8 candidates with exact f32 distances.
Every returned distance is the exact f32 distance of the returned slot
(asserted by test).

| Dataset | M | ef | Recall@10 f32 → int8 | p50 / p95 ms, f32 → int8 | Batch q/s f32 → int8 |
|---|---:|---:|---|---|---|
| `sift-128-euclidean` | 16 | 50 / 100 / 200 | 0.9580 → 0.9582 / 0.9874 → 0.9867 / 0.9971 → 0.9971 | 0.20 / 0.26 → 0.17 / 0.22, 0.34 / 0.41 → 0.28 / 0.33, 0.63 / 0.84 → 0.50 / 0.59 | 23,600 → 48,700 / 13,400 → 27,500 / 7,900 → 16,500 |
| `nytimes-256-angular` | 32 | 200 / 400 / 800 | 0.9539 → 0.9540 / 0.9741 → 0.9738 / 0.9900 → 0.9902 | 1.31 / 1.70 → 0.99 / 1.27, 2.64 / 3.21 → 2.00 / 2.38, 5.18 / 6.00 → 3.87 / 4.45 | 3,100 → 8,700 / 1,600 → 4,500 / 770 → 2,400 |
| `glove-100-angular` | 32 | 200 / 400 / 800 | 0.9371 → 0.9356 / 0.9660 → 0.9658 / 0.9851 → 0.9853 | 0.99 / 1.25 → 0.93 / 1.15, 1.81 / 2.35 → 1.63 / 2.09, 3.29 / 4.38 → 2.90 / 3.78 | 5,100 → 9,500 / 2,800 → 5,000 / 1,500 → 2,700 |

Int8 traversal costs at most 0.0014 recall (glove, ef 200) against the
required 0.01 budget and the 0.005 target. Single-thread latency drops 6%
to 25%. Parallel throughput roughly doubles, because the hot set per vector
shrinks about fourfold and more of it stays in cache.

### Measured memory per vector

| Dataset | dims | M | f32 originals | int8 + scalars | graph and bookkeeping | Total accounted | RSS after build |
|---|---:|---:|---:|---:|---:|---:|---:|
| `sift-128-euclidean` | 128 | 16 | 512 | 136 | 182 | 830 | 1.57 GiB / 1M |
| `nytimes-256-angular` | 256 | 32 | 1,024 | 264 | 310 | 1,598 | 0.96 GiB / 290k |
| `glove-100-angular` | 100 | 32 | 400 | 108 | 310 | 818 | 1.96 GiB / 1.18M |

Graph bytes per vector are a function of M alone: about 182 at M = 16 and
310 at M = 32, close to the contract's `(M0 + M/(ln M − 1)) × 4` estimate
plus 40 bytes of levels, lengths, and list headers. The int8 copy is
`dims + 8`. Admission adds 80 bytes of slot and key bookkeeping per vector;
the published formula is contract §11, quoted here:

    hot_bytes_per_vector = dims + 8 + graph(M) + 80 + payload
    graph(M) = 54 + 8·M      (182 at M = 16, 310 at M = 32)

At the product's 512 dims with M = 32 that is 910 bytes before payload,
against the plan's target of 800 (missed by 14%; required, a measured and
published factor, met). Each partition also holds its idempotency window
(about 40 bytes per entry, up to 40 MB at the default window). The f32
originals (`dims × 4`) are not part of the hot set: they are read only to
rescore 100 candidates per query and are served from the memory-mapped
snapshot through the page cache (contract §7, §11).

## Filtered search

Same graphs, k = 10, 1,000 queries, int8 traversal with rescoring. Filters
are deterministic pseudo-random subsets of the base at four selectivities;
ground truth is exact search over the allowed set. Two routes are timed per
level: exact brute force over the allowed slots, and masked traversal at
the dataset's ef (recall is for the masked route; brute force is exact by
construction).

| Dataset (M, ef) | Selectivity | Allowed | Brute p50 / p95 (ms) | Masked recall@10 (min) | Masked p50 / p95 (ms) |
|---|---:|---:|---|---|---|
| sift (16, 100) | 0.1% / 1% / 10% / 50% | 1,000 / 9,988 / 100,015 / 500,046 | 0.03 / 0.04, 0.33 / 0.35, 12.9 / 13.3, 38.0 / 38.7 | 1.000 (1.0) / 0.9999 (0.9) / 0.9996 (0.9) / 0.9952 (0.8) | 59.9 / 70.8, 10.1 / 13.2, 1.62 / 2.14, 0.49 / 0.61 |
| nytimes (32, 400) | 0.1% / 1% / 10% / 50% | 285 / 2,900 / 28,979 / 144,996 | 0.01 / 0.01, 0.14 / 0.14, 4.09 / 4.25, 17.8 / 18.2 | 0.999 (0.0) / 0.999 (0.0) / 0.996 (0.8) / 0.9833 (0.6) | 117 / 123, 50.1 / 52.7, 15.0 / 16.3, 3.87 / 4.73 |
| glove (32, 800) | 0.1% / 1% / 10% / 50% | 1,182 / 11,838 / 118,344 / 591,790 | 0.03 / 0.04, 0.30 / 0.31, 12.7 / 13.6, 32.2 / 32.8 | 1.000 (1.0) / 1.000 (1.0) / 0.9994 (0.9) / 0.9925 (0.7) | 442 / 471, 113 / 125, 20.6 / 25.8, 5.18 / 6.94 |

Reading:

- Masked traversal collapses under selective filters: at 0.1% it is slower
  than an unfiltered brute-force scan of the whole base, because the graph
  routes through thousands of excluded nodes to find a few allowed ones, and
  on nytimes some queries find none at all (per-query minimum 0.0). This is
  the regime the contract reserves for the exact route, which costs
  microseconds there.
- Above the crossover the masked route is 4.6 to 78 times faster than brute
  force and its mean recall stays at or above 0.983, clearing the 0.95
  threshold at every level; raising ef four times lifts the 50% level to
  0.998 or better.
- The latency crossover is 29k allowed slots on sift (ef 100), 59k on
  nytimes (ef 400), and 171k on glove (ef 800): 291, 147, and 213 times ef.
  The cost of masked traversal scales with ef, not k; the plan's
  `c ≈ 50 · k` hypothesis was wrong by two orders of magnitude.

Cutover adopted (contract §9): `T = 200 · ef_search`. At every measured
level this picks the faster route, and near the boundary it prefers the
exact one. A future refinement is an int8 brute-force route with rescoring,
which would move the crossover higher.

## Persistence and recovery

Bulk-load a dataset into an on-disk partition, write the snapshot, append a
1,000-entry WAL tail with per-entry `fdatasync`, drop the partition, and
recover it. Search results before and after recovery are compared bit for
bit (int8 traversal with rescoring, 1,000 queries; recall is recomputed from
the dataset rows each hit's key names).

| Dataset (M, ef) | Snapshot | Bytes/vector | Snapshot write | WAL tail (1,000 upserts) | Recovery | Replayed | Results identical | RAM resident / mapped after recovery |
|---|---:|---:|---:|---:|---:|---:|---|---|
| sift 1M (16, 100) | 0.77 GiB | 821 | 1.8 s | 1.46 s, 565 KB | 1.76 s | 1,000 | yes (recall 0.9893 both) | 389 MiB / 488 MiB |
| nytimes 290k (32, 400) | 0.43 GiB | 1,589 | 0.9 s | 4.31 s, 1.08 MB | 4.03 s | 1,000 | yes (recall 0.9767 both) | 185 MiB / 283 MiB |
| glove 1.18M (32, 800) | 0.89 GiB | 809 | 1.9 s | 2.94 s, 453 KB | 3.24 s | 1,000 | yes (recall 0.9860 both) | 572 MiB / 451 MiB |
| project-nomic-512 1M (32, 500) | 2.67 GiB | 2,869 | 4.4 s | 2.32 s, 2.10 MB | 3.66 s | 1,000 | yes (recall 0.9934 both) | 879 MiB / 1,953 MiB |

Reading:

- Recovery is dominated by hashing the file (BLAKE3 over every byte through
  the map, originals included), copying the int8 and graph sections into
  RAM, and replaying the tail; afterwards the f32 originals are served from
  the map rather than copied. These runs recovered right after writing,
  from a warm page cache; a cold-cache recovery is still to be measured.
  At 1M × 512 recovery takes 3.66 s against the plan's 60 s required and
  20 s target: of the 2.67 GiB file only the int8 and graph sections
  (0.8 GiB) are copied.
- These rows ran from `46757b8`. They replace the 2026-10-27 rows, which
  under-reported recall where a duplicate under a `tail-100` to `tail-999`
  key was returned (sift 0.9880 and nytimes 0.9716 then; see
  [Project set](#project-set-project-nomic-512)). Snapshot writes now
  stream and take about half as long as the earlier in-memory encoding
  (sift 3.4 → 1.8 s).
- Recovered search is byte-identical to pre-recovery search, and no hit's
  reported distance disagrees with the distance recomputed from its row
  (`score_mismatches` 0).
- The WAL tail runs at about 230 to 690 synced entries per second with one
  `fdatasync` per entry. Group commit (contract §4 rule 1) will batch these;
  the per-entry number is the floor, not the throughput target.
- nytimes holds 239 zero-norm base rows and 9 zero-norm queries. Bulk load
  keeps them, but the public upsert and search reject them on cosine
  (contract §2), so the persistence and compaction benches use the first
  1,000 admissible rows and queries.

## Compaction

Bulk-load, delete every fifth key (tombstone ratio 0.200, the contract's
trigger), search, then run two-phase compaction: a short capture under the
writer, a rebuild on another thread while 1,000 upserts are written, and the
exclusive finish. Recall is measured against exact search over the surviving
base; 1,000 queries, k = 10, int8 traversal with rescoring.

| Measure | sift 1M (M 16, ef 100) | nytimes 290k (M 32, ef 400) | glove 1.18M (M 32, ef 800) |
|---|---:|---:|---:|
| Deletes (one `fdatasync` each) | 200,000 in 92.0 s | 58,000 in 26.1 s | 236,703 in 111.9 s |
| Capture (writer held) | 0.38 s | 0.15 s | 0.36 s |
| Rebuild (writers running) | 87.4 s | 129.4 s | 285.8 s |
| Upserts acknowledged during the rebuild, slowest | 1,000, 44.4 ms | 1,000, 36.9 ms | 1,000, 17.6 ms |
| Finish (exclusive: catch-up, snapshot, swap) | 3.95 s | 5.28 s | 5.53 s |
| Slots | 1,000,000 → 801,000 | 290,000 → 233,000 | 1,183,514 → 947,811 |
| Snapshot bytes | 821 → 666 MB | 461 → 373 MB | 958 → 776 MB |
| Accounted resident bytes | 414 → 345 MB | 194 → 160 MB | 609 → 503 MB |
| Query p50 | 353 → 275 µs | 2,468 → 1,882 µs | 3,630 → 2,894 µs |
| Recall@10 vs. surviving base | 0.9929 → 0.9918 | 0.9779 → 0.9758 | 0.9889 → 0.9891 |
| Concurrent keys present after swap | 1,000 | 1,000 | 1,000 |
| Deleted keys returned, score mismatches | 0, 0 | 0, 0 | 0, 0 |

Reading:

- Writers proceed during the rebuild: every concurrent upsert was
  acknowledged before the rebuild finished, and the only exclusive section is
  the finish phase (4 to 6 s, dominated by writing the snapshot). The
  2026-10-01 run could not show this, because its API held the partition for
  the whole rebuild.
- Compaction removes the tombstoned slots from the graph, the int8 store,
  and the snapshot, and p50 latency drops by a fifth or more because
  traversal no longer walks dead nodes.
- Recall after compaction is within 0.002 of the masked search over the
  uncompacted graph on every set, and above the 0.97 required recall.
- No deleted key appears in any result after compaction, the catch-up
  replayed every concurrent upsert (`caught_up_entries` = 1,000), and every
  concurrent key is present after the swap.

## Project set (`project-nomic-512`)

The only set at the product's dimension, built by
[`scripts/bench/build-project-set.py`](../../scripts/README.md) with the
locked phase-0 model and recipe: `nomic-embed-text` (Ollama digest
`0a109f42…`), task prefixes, truncated from 768 to 512 dimensions and
L2-normalized. Text is chunked with the phase-0 chunker (2,000 characters,
200 overlap). Sources in stream order: the phase-0 mixed corpus (347 text
files, 1,817 chunks), then Wikipedia `20231101.en` pinned to revision
`b04c8d1c`. Every 100th chunk is held out and its first 256 characters
become a query. The base is therefore 99.8% encyclopedic prose: it matches
the product's vector distribution at 512 dimensions, not the mix of
personal files a deployment would hold. Manifests:
[project-nomic-512-200k.manifest.json](project-nomic-512-200k.manifest.json)
and [project-nomic-512.manifest.json](project-nomic-512.manifest.json); the
200k set is the first 200,000 base chunks of the 1M set, and both share
the same 10,000 queries.

Both sets were measured on 2026-10-28 with the same procedure as the
tables above, M = 32, and the cosine default ef = 500 unless noted. The
200k set (the first 200,000 base chunks) ran from `30fc26f`. The 1M set
ran oracle, HNSW, and filtered from `b4b6710`, compaction from `684ddfe`
(after the snapshot-writer fix below), and persistence from `46757b8`
(after the recall-verification fix below).

| Measure | 200k × 512 | 1M × 512 |
|---|---|---|
| Oracle | recall 1.000, p50 / p95 18.1 / 19.4 ms, 393 queries/s | recall 1.000, p50 / p95 91.3 / 98.4 ms, 23 queries/s |
| Parallel build | 80 s, 2,498 vectors/s | 588 s, 1,700 vectors/s |
| Recall@10, f32 → int8, ef 100 / 200 / 500 / 800 | 0.9841 → 0.9841 / 0.9926 → 0.9926 / 0.9973 → 0.9972 / 0.9988 → 0.9987 | 0.9682 → 0.9680 / 0.9839 → 0.9841 / 0.9933 → 0.9933 / 0.9956 → 0.9956 |
| p50 / p95 ms, int8, ef 100 / 500 / 800 | 0.42 / 0.64, 1.46 / 2.17, 2.13 / 3.14 | 0.63 / 0.89, 2.18 / 3.14, 3.25 / 4.65 |
| p50 / p95 ms, f32, ef 500 | — | 3.52 / 5.22 |
| Batch queries/s, int8, ef 500 | 3,979 | 2,659 |
| Filtered recall, masked route, 0.1% / 1% / 10% / 50% | 1.000 / 1.000 / 0.9997 / 0.9989; crossover at about 29,500 allowed slots | 1.000 / 1.000 / 0.9990 / 0.9963; crossover at about 85,500 allowed slots |
| Filtered p50 ms, brute / masked, 0.1% / 1% / 10% / 50% | — | 0.07 / 348, 1.57 / 86.8, 20.8 / 14.5, 99.9 / 3.83 |
| Accounted bytes per vector (f32 + int8 + graph) | 2,878 | 2,878 (2,048 + 520 + 310) |
| Snapshot | 574 MB (2,869 bytes per vector including f32 originals) | 2.87 GB (2,869 bytes per vector), written in 4.4 s |
| Recovery with a 1,000-entry WAL tail | 1.74 s, results identical | 3.66 s, results identical (recall 0.9934 both) |
| RAM resident / mapped after recovery | — | 879 MiB / 1,953 MiB |
| Compaction at 20% tombstones | 1,000 of 1,000 upserts accepted during the 61 s rebuild (slowest 15 ms), finish 3.2 s, no deleted key returned, recall 0.9984 → 0.9986 | 1,000 of 1,000 upserts accepted during the 478 s rebuild (slowest 32 ms), finish 6.0 s, no deleted key returned, recall 0.9938 → 0.9941, p50 2.68 → 2.33 ms |
| Peak RSS of the bench process | — | persistence 7.7 GiB, compaction 9.8 GiB |

Reading (1M, against the plan's thresholds at 1M × 512):

- Every required threshold is met at the defaults: recall 0.9933 (≥ 0.97),
  single-thread p95 3.14 ms (≤ 5 ms), build 1,700 vectors/s (≥ 560),
  recovery 3.66 s (≤ 60 s), and a published hot-set factor of
  about 920 bytes per vector. Three targets are missed: p95 ≤ 2 ms (met at
  ef = 200, recall 0.984), build ≥ 3.3k vectors/s, and ≤ 800 bytes per
  vector. The f32 traversal mode exceeds 5 ms at p95 (5.22 ms); int8 is
  the default and f32 is kept for diagnosis.
- Recall falls with scale at a fixed ef: 0.9972 at 200k and 0.9933 at 1M
  for ef = 500, and ef = 100 drops below 0.97 at 1M (0.968). The default
  ef = 500 keeps a margin at the M3 target scale.
- The cutover rule `T = 200 · ef_search` (contract §9) puts the boundary at
  100,000 allowed slots for ef = 500; the measured crossover is about
  85,500 (171 × ef, inside the 147 to 291 × ef of the public sets), so the
  rule picks the faster route at every measured level. Near the boundary
  both routes cost 15 to 21 ms at p50, five to seven times an unfiltered
  query.
- The oracle's batch rate falls from 393 to 23 queries/s, 17 times for a
  base 5 times larger. At 1M each query streams the 2 GB base, an
  effective 47 GB/s across 16 threads. The cause of the super-linear drop
  was not investigated; the oracle is a reference, not a serving path.
- Peak RSS includes the bench's own copy of the dataset (1.9 GiB) and the
  snapshot pages mapped from the page cache. Compaction holds a second
  copy of the live rows while it rebuilds, which is why it peaks highest;
  compaction is not yet admitted against the RAM budget (contract §11).

Two defects surfaced on the 1M set and were fixed before the published
runs:

- **Snapshot writes built the file in memory.** The first 1M persistence
  run was OOM-killed at its first snapshot (8.1 GB anonymous memory, swap
  exhausted): the writer held about four times the 2.9 GB file on top of
  the partition. It now streams in two passes, and the file is unchanged
  byte for byte (`684ddfe`, [hardening-2026-10.md](../hardening-2026-10.md)).
  Benches now run in a `systemd-run --user --scope -p MemoryMax=10G`
  scope so an overrun kills only the bench.
- **Persistence recall was under-reported.** Recall verification read any
  8-byte key as a row number first, and the WAL-tail keys `tail-100` to
  `tail-999` are 8 bytes long, so those duplicate hits mapped to no row,
  were dropped, and counted as misses. It showed on this set because base
  rows 0 to 999 are the corpus chunks its first queries retrieve: the
  1M run read 0.9878 where the compaction bench read 0.9938 for the same
  queries. Fixed in `46757b8`; an unmapped key now counts as a score
  mismatch. The public sets' persistence rows were re-run from the same
  commit.


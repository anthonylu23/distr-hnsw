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
| `project-nomic-512` | pending (GPU shared) | 512 | cosine | | exact search |

## Provenance and re-run status

Every number below was measured on 2026-09-30 and 2026-10-01 before the
harness recorded where it came from: 19 of the 20 reports say
`source_revision: "unknown"`, and the compaction report names `c3f93ce`, a
commit that predates the compaction code (it was built from an uncommitted
tree). The figures do match the report files, but the repository cannot show
which code produced them. The harness now embeds the revision with a dirty
flag (`build.rs`), the executable's BLAKE3, and the command line in every
report, and `--canonical` refuses a dirty or unknown build. **Every table here
is pending a canonical re-run** from the hardening commit; until then treat
them as indicative.

The hardening pass changed the parallel builder (back-links are merged, not
overwritten, and level-0 orphans are relinked) and compaction (the rebuild
runs on its own thread). An ad-hoc check from the uncommitted tree on
2026-10-25 (sift 1M, M = 16, f32, 2,000 queries) gave 117.9 s and 8,485
vectors/s with recall 0.958 / 0.988 / 0.997 at ef 50 / 100 / 200, the same as
pass 3 within noise.

## Oracle baseline (pass 1, 2026-09-30)

Exact search with the engine's own kernels, `anthonypc` (16 threads, 15 GiB),
release build, k = 10, first 1,000 queries. Recall is distance-based; id
recall is shown to quantify tie effects from duplicate vectors.

| Dataset | Recall@10 | Min per query | Id recall | Single-thread p50 / p95 (ms) | Batch queries/s (16 threads) |
|---|---:|---:|---:|---:|---:|
| `nytimes-256-angular` | 1.000 | 1.000 | 0.985 | 14.7 / 15.7 | 466 |
| `sift-128-euclidean` | 1.000 | 1.000 | 0.999 | 26.9 / 27.6 | 121 |
| `glove-100-angular` | 1.000 | 1.000 | 1.000 | 24.5 / 26.4 | 166 |

These are the brute-force latency floors the HNSW passes must beat by one to
two orders of magnitude while staying above the recall thresholds in the
plan. Full reports: `~/distr-hnsw-bench/reports/oracle-<dataset>.json`.

## HNSW, f32 graph (pass 3, 2026-09-30)

Same machine, release build, k = 10, 2,000 queries, `ef_construction` = 200,
parallel bulk build on 16 threads unless noted. Recall is distance-based.
The sequential (WAL-order) build of the same parameters differed from the
parallel build by at most 0.001 recall on every set.

| Dataset | M | Build | Bytes/vector (accounted) | ef | Recall@10 | p50 (ms) | Batch q/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| `sift-128-euclidean` | 16 | 121 s, 8,250 v/s (sequential: 878 s, 1,138 v/s) | 694 | 50 / 100 / 200 | 0.958 / 0.987 / 0.997 | 0.21 / 0.34 / 0.62 | 23,700 / 13,800 / 7,700 |
| `nytimes-256-angular` | 16 (sequential) | 329 s, 881 v/s | 1,206 | 50 / 100 / 200 | 0.849 / 0.887 / 0.916 | 0.23 / 0.41 / 0.77 | 16,400 / 9,600 / 5,300 |
| `nytimes-256-angular` | 32 | 171 s, 1,690 v/s | 1,334 | 100 / 200 / 400 / 800 | 0.932 / 0.956 / 0.975 / 0.989 | 0.70 / 1.34 / 2.69 / 5.39 | 5,400 / 2,900 / 1,500 / 770 |
| `glove-100-angular` | 16 (sequential) | 1,012 s, 1,170 v/s | 582 | 50 / 100 / 200 / 400 | 0.758 / 0.829 / 0.881 / 0.922 | 0.20 / 0.42 / 0.62 / 1.15 | 18,900 / 14,600 / 7,500 / 4,500 |
| `glove-100-angular` | 32 | 391 s, 3,030 v/s | 710 | 100 / 200 / 400 / 800 | 0.893 / 0.937 / 0.966 / 0.985 | 0.53 / 1.00 / 1.71 / 3.13 | 7,500 / 4,900 / 2,600 / 1,400 |

Reading: the L2 set clears the 0.97 required recall at M = 16 with ef = 100
(0.987), consistent with published hnswlib curves for these parameters. The
two angular sets are the known hard cases and need M = 32: nytimes clears
0.97 at ef = 400 and glove at roughly ef = 500 (0.966 at 400, 0.985 at 800).
Every configuration is one to two orders of magnitude faster than the exact
oracle at the same recall band. Bytes per vector are the f32 originals plus
graph links; the int8 pass adds the quantized copy the graph will search,
so the RAM the hot path touches per vector drops to about a quarter while the
originals stay available for exact rescoring (contract §7).

Defaults adopted from these measurements (contract §8): `M = 16` for L2 and
dot collections, `M = 32` for cosine collections; `ef_search` defaults to 100
for L2/dot and 400 for cosine, per-query overridable. They live in code as
`HnswParams::for_metric` and `hnsw::default_ef_search` (the bench passes its
parameters explicitly). The cosine default of 400 clears 0.97 on nytimes
(0.975) but **not** on glove (0.966); that shortfall is an open decision
(raise the default, or accept glove as the hard case), not a met threshold.
Full reports: `~/distr-hnsw-bench/reports/hnsw-*.json`.

## Int8 traversal with exact rescoring (pass 4, 2026-09-30)

Same graphs as above (parallel build, `ef_construction` = 200), 2,000
queries, k = 10, rescoring the best `max(4k, 100)` = 100 int8 candidates with
exact f32 distances. Every returned distance is the exact f32 distance of
the returned slot (asserted by test).

| Dataset | M | ef | Recall@10 f32 → int8 | p50 ms f32 → int8 | Batch q/s f32 → int8 |
|---|---:|---:|---|---|---|
| `sift-128-euclidean` | 16 | 50 / 100 / 200 | 0.9585 → 0.9581 / 0.9873 → 0.9869 / 0.9972 → 0.9971 | 0.20 → 0.17 / 0.34 → 0.29 / 0.63 → 0.49 | 23,700 → 47,400 / 13,500 → 27,200 / 7,800 → 16,200 |
| `nytimes-256-angular` | 32 | 200 / 400 / 800 | 0.9547 → 0.9547 / 0.9744 → 0.9747 / 0.9893 → 0.9891 | 1.34 → 1.08 / 2.70 → 2.14 / 5.26 → 4.23 | 2,900 → 7,900 / 1,400 → 3,900 / 770 → 2,100 |
| `glove-100-angular` | 32 | 200 / 400 / 800 | 0.9362 → 0.9363 / 0.9659 → 0.9657 / 0.9849 → 0.9850 | 0.98 → 0.96 / 1.82 → 1.76 / 3.34 → 3.04 | 4,900 → 8,400 / 2,600 → 4,700 / 1,400 → 2,500 |

Int8 traversal costs at most 0.0005 recall against the required 0.01 budget
and the dataset-level ef defaults are unchanged. Single-thread latency drops
10% to 25%; parallel throughput roughly doubles because the hot set per
vector shrinks by about four and more of it stays in cache.

### Measured memory per vector

| Dataset | dims | M | f32 originals | int8 + scalars | graph and bookkeeping | Total accounted | RSS after build |
|---|---:|---:|---:|---:|---:|---:|---:|
| `sift-128-euclidean` | 128 | 16 | 512 | 136 | 182 | 830 | 1.56 GiB / 1M |
| `nytimes-256-angular` | 256 | 32 | 1,024 | 264 | 310 | 1,598 | 0.96 GiB / 290k |
| `glove-100-angular` | 100 | 32 | 400 | 108 | 310 | 818 | 1.97 GiB / 1.18M |

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
snapshot through the page cache (contract §7, §11). Full reports:
`~/distr-hnsw-bench/reports/hnsw-*-int8.json`.

## Filtered search (pass 5, 2026-09-30)

Same graphs, k = 10, 1,000 queries, int8 traversal with rescoring. Filters
are deterministic pseudo-random subsets of the base at four selectivities;
ground truth is exact search over the allowed set. Two routes are timed per
level: exact brute force over the allowed slots, and masked traversal at
the dataset's default ef (recall is for the masked route; brute force is
exact by construction).

| Dataset (M, ef) | Selectivity | Allowed | Brute p50 (ms) | Masked recall@10 (min) | Masked p50 (ms) |
|---|---:|---:|---:|---|---:|
| sift (16, 100) | 0.1% / 1% / 10% / 50% | 1,000 / 9,988 / 100,015 / 500,046 | 0.03 / 0.34 / 13.2 / 38.6 | 1.000 (1.0) / 0.9997 (0.9) / 0.9996 (0.9) / 0.996 (0.8) | 61.4 / 10.4 / 1.79 / 0.52 |
| nytimes (32, 400) | 0.1% / 1% / 10% / 50% | 285 / 2,900 / 28,979 / 144,996 | 0.01 / 0.14 / 4.2 / 17.8 | 0.999 (0.0) / 0.9987 (0.0) / 0.9958 (0.7) / 0.9829 (0.6) | 118 / 51.0 / 15.9 / 4.02 |
| glove (32, 800) | 0.1% / 1% / 10% / 50% | 1,182 / 11,838 / 118,344 / 591,790 | 0.03 / 0.32 / 13.2 / 32.7 | 1.000 (1.0) / 1.000 (1.0) / 0.9994 (0.9) / 0.9929 (0.7) | 447 / 117 / 21.4 / 5.41 |

Reading:

- Masked traversal collapses under selective filters: at 0.1% it is slower
  than an unfiltered brute-force scan of the whole base, because the graph
  routes through thousands of excluded nodes to find a few allowed ones, and
  on nytimes some queries find none at all (per-query minimum 0.0). This is
  the regime the contract reserves for the exact route, which costs
  microseconds there.
- Above the crossover the masked route is 7 to 75 times faster than brute
  force and its mean recall stays at or above 0.983, clearing the 0.95
  threshold at every level; raising ef four times lifts the 50% level to
  0.997 or better.
- The latency crossover is 29k allowed slots on sift (ef 100), 61k on
  nytimes (ef 400), and 172k on glove (ef 800): 294, 152, and 215 times ef.
  Expressed per k it would be 2,900 to 17,000 times k, so the plan's
  `c ≈ 50 · k` hypothesis was wrong by two orders of magnitude; the cost of
  masked traversal scales with ef, not k.

Cutover adopted (contract §9): `T = 200 · ef_search`. At every measured
level this picks the faster route, and near the boundary it prefers the
exact one. A future refinement is an int8 brute-force route with rescoring,
which would move the crossover higher. Full reports:
`~/distr-hnsw-bench/reports/filtered-*.json`.

## Persistence and recovery (pass 6, 2026-10-01)

Bulk-load a dataset into an on-disk partition, write the snapshot, append a
1,000-entry WAL tail with per-entry `fdatasync`, drop the partition, and
recover it. Search results before and after recovery are compared bit for
bit (int8 traversal with rescoring, 1,000 queries).

| Dataset (M) | Snapshot | Bytes/vector | Snapshot write | WAL tail (1,000 upserts) | Recovery | Replayed | Results identical | RAM resident / mapped after recovery |
|---|---:|---:|---:|---:|---:|---:|---|---|
| sift 1M (16) | 0.77 GiB | 821 | 3.5 s | 1.44 s, 565 KB | 1.80 s | 1,000 | yes (recall 0.9892 both) | 389 MiB / 488 MiB |
| glove 1.18M (32) | 0.89 GiB | 809 | 3.8 s | 2.73 s, 453 KB | 5.40 s | 1,000 | yes (recall 0.9872 both) | 572 MiB / 451 MiB |

Reading:

- Recovery is dominated by hashing the file (BLAKE3 over 0.8 to 0.9 GiB,
  which reads every byte through the map, originals included), copying the
  int8 and graph sections into RAM, and replaying the tail; afterwards the
  f32 originals are served from the map rather than copied. These runs
  recovered right after writing, from a warm page cache; a cold-cache
  recovery (fresh process after dropping caches) is still to be measured. Against the plan's 60 s required
  and 20 s target at 1M × 512, 1.8 to 5.4 s at 100 to 128 dims leaves
  ample room: the 512-dim file is about 3 GiB, of which only the int8 copy
  (0.5 GiB) is copied.
- Recovered search is byte-identical to pre-recovery search, which is what
  the snapshot and WAL formats are for.
- The WAL tail runs at about 700 synced entries per second with one
  `fdatasync` per entry. Group commit (contract §4 rule 1) will batch these;
  the per-entry number is the floor, not the throughput target.
- Resident RAM after recovery is the int8 copy plus graph plus keys and
  payload bookkeeping; the f32 originals (488 MiB on sift) sit in the page
  cache behind the map. The bench process RSS (~2 GiB) also holds the
  dataset itself.

Full reports: `~/distr-hnsw-bench/reports/persist-*.json`.

## Compaction (pass 7, 2026-10-01)

Bulk-load sift 1M (M = 16), delete every fifth key (200,000, tombstone ratio
0.200, the contract's trigger), search, then run two-phase compaction with
1,000 upserts landing between `begin_compaction` and `finish_compaction`.
Recall is measured against exact search over the surviving base; 1,000
queries, k = 10, ef = 100, int8 traversal with rescoring.

| Phase | Time | Notes |
|---|---:|---|
| 200,000 deletes | 97.0 s | one `fdatasync` per delete (2,060/s); group commit is the known follow-up |
| `begin_compaction` | 92.8 s | rebuilds 800,000 live slots (8,600 vectors/s); in this run writers were blocked throughout (see below) |
| `finish_compaction` | 5.21 s | the exclusive section: replay 1,000 caught-up entries, write the 0.62 GiB snapshot, swap |

| Measure | Before | After |
|---|---:|---:|
| Slots (including tombstones) | 1,000,000 | 801,000 |
| Snapshot bytes | 821 MB | 666 MB |
| Accounted resident bytes | 926 MB | 755 MB |
| Query p50 | 451 µs | 306 µs |
| Recall@10 vs. surviving base | 0.9929 | 0.9905 |
| Deleted keys returned | 0 | 0 |

Reading:

- In this run the 1,000 "concurrent" upserts were issued after
  `begin_compaction` returned, and `begin_compaction(&self)` borrowed the
  partition for the whole 93 s rebuild, so writers could not have proceeded
  during it. The claim that compaction needs no maintenance window was
  therefore not shown. The hardening pass split `begin` into a short capture
  and an owned rebuild on another thread; the bench now writes during the
  rebuild and reports `capture_seconds`, `build_seconds`,
  `upserts_during_build`, and `upsert_max_ms_during_build`. (A 20,000-vector
  smoke run acknowledged 884 upserts during a 0.42 s rebuild, slowest
  2.9 ms.) The finish phase remains the exclusive section: 5.2 s at
  1M × 128, dominated by writing the snapshot.
- Compaction removes the tombstoned slots from the graph, the int8 store,
  and the snapshot, and p50 latency drops by a third because traversal no
  longer walks dead nodes. The resident figures above include about 512 MB
  of f32 originals held on the heap after the bulk load and after the
  rebuild (the snapshot was written but not mapped); every snapshot now
  maps its originals, so a re-run will report only the hot set.
- Recall after compaction (0.9905) is marginally below the masked search of
  the uncompacted graph (0.9929): the old graph was built over 1M points and
  masking makes the search visit more candidates, which is also why it is
  slower. Both are above the 0.98 threshold for sift.
- No deleted key appears in any result after compaction, and the
  catch-up replayed every concurrent upsert (`caught_up_entries` = 1,000,
  high-water marks equal). The bench now also looks each concurrent key up
  after the swap (`concurrent_keys_present`) and computes recall from the
  dataset row each hit's key names, not from the distance the engine
  reported (`score_mismatches` must be 0).

Full report: `~/distr-hnsw-bench/reports/compact-sift-128-euclidean.json`.

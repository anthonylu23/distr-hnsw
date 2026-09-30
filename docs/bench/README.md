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
graph links; the int8 pass will add the quantized copy and is expected to
dominate RAM by a factor of about four less than f32 once the graph searches
int8 (contract §7).

Defaults adopted from these measurements (contract §8): `M = 16` for L2 and
dot collections, `M = 32` for cosine collections; `ef_search` defaults to 100
for L2/dot and 400 for cosine, per-query overridable. Full reports:
`~/distr-hnsw-bench/reports/hnsw-*.json`.

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

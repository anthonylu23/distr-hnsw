# Phase-0 validation prototype

Status: **tooling hardened** (FTS isolation, failure-safe corpus replacement,
embedding-time model identity, reproducible provenance, controlled latency,
baseline metrics, dim recommendation, embed concurrency). Bake-off regenerated on
`anthonypc` against the Obsidian vault. Product go/no-go remains **no-go** on
this small corpus; dims are **not locked**.

## Purpose

Per [DESIGN.md](../DESIGN.md) §14.0 / §15: empirically validate that semantic
retrieval beats name/date baselines on a real corpus, and lock the default
local embedding model and dimensionality (≤768) before building distributed
machinery.

## Implementation

Disposable Rust CLI: [`prototype/`](../prototype/) (`distr-hnsw-validate`).

| Piece | Choice |
|---|---|
| Store | SQLite (run-scoped under `~/distr-hnsw-proto/runs/<utc>/`) |
| Search | Brute-force cosine on L2-normalized vectors |
| Embed | Ollama `/v1/embeddings`; concurrent batches; digest persisted per model/dims |
| Nomic prefixes | `search_document:` / `search_query:` applied automatically |
| Extract | In-process text/HTML/PDF; empty/broken PDF → `needs-ocr` |
| Prepare | Fingerprint; prune/root switch only after a clean walk; `--fresh` |
| Name FTS | Column filter grouped: `{name} : (t1 OR t2 …)` |
| Baselines judged | vs name, vs recency, vs keyword (go gate uses vs name) |
| Report | JSON/Markdown/HTML with query/source/executable/model provenance |

## Success criteria

- Semantic win-rate vs name baseline ≥ ~60% of decided (non-tie) queries, **or**
  clear qualitative dominance on meaning queries
- Usable provider cold start and warmed p50/p95 `query` latency on the GPU box
- Prefer dims 512 if within ~0.03 nDCG of the best config for that model;
  **do not lock dims** when judged queries &lt; 40 or nDCG spread across dims &lt; 0.03

## anthonypc bake-off (corrected, 2026-07-18)

Hardware: Fedora desktop, RTX 3060 Ti 8GB, Ollama `nomic-embed-text`.

| Field | Value |
|---|---|
| Corpus | `~/Documents/Obsidian Vault` (14 markdown notes, 27 chunks) |
| Query set | 10 labeled queries (`prototype/testdata/anthonypc-vault-queries.json`) |
| Work dir | `~/distr-hnsw-proto/runs/20260718T215901Z` |
| Suggested (not locked) | `nomic-embed-text` @ **512** |
| Strict go/no-go | **no-go** (vs name 16.7%); **dims not locked** |
| Mean recall@10 | **1.00** |
| Recency mtime collision | 35.7% of files share max mtime (5/14) |

### Config summary (grouped name FTS)

| model | dims | judged | vs name (W/L/T) | vs recency | vs keyword | mean recall | mean nDCG | cold / warm p50 / p95 ms |
|---|---:|---:|---|---|---|---:|---:|---:|
| nomic-embed-text | 768 | 10 | 16.7% (1/5/4) | 100% | 62.5% | 1.000 | 0.725 | 998.1 / 14.0 / 14.6 |
| nomic-embed-text | 512 | 10 | 16.7% (1/5/4) | 100% | 62.5% | 1.000 | 0.710 | — / 13.9 / 15.9 |
| nomic-embed-text | 384 | 10 | 16.7% (1/5/4) | 100% | 50.0% | 1.000 | 0.725 | — / 13.7 / 15.5 |

Cold start is recorded once per distinct model (on the first listed config),
not once per truncation dimension. Before scoring, eval completes an unreported
full sweep of all model/dimension/query combinations. A second eval against the
same database reproduced every retrieval metric exactly; warmed latency varied
slightly as expected.

Earlier (buggy) headline was 28.6% vs name because the name FTS column filter
was not parenthesized and leaked body-text matches. Corrected: **16.7%**.

Full report (local copy, gitignored):
`prototype/testdata/reports/bakeoff-20260718T215901Z.{md,json,html}`

Remote source of truth:
`anthonypc:~/distr-hnsw-proto/runs/20260718T215901Z/reports/`

The full report contains private vault filenames and remains gitignored. A
sanitized aggregate with hashes and headline metrics is tracked at
[`phase-0-bakeoff-summary.json`](phase-0-bakeoff-summary.json).

### Interpretation

- Pipeline works end-to-end on the GPU box.
- Semantic **beats recency** cleanly on this set; **loses to name** when queries
  echo filenames (expected; hybrid keyword is phase 5).
- Corpus and query set are too small to lock model/dims. Next bake-off needs a
  larger mixed file tree and ≥40 meaning-oriented queries.

### Defaults (provisional, explicitly unlocked)

| Setting | Value | Notes |
|---|---|---|
| Candidate local model | `nomic-embed-text` | Measured on anthonypc |
| Candidate dims | 512 (within 0.03 nDCG of the measured best) | **Not locked** — judged=10 |
| Embed runtime | Ollama on GPU box | `OLLAMA_HOST` normalized to `http://…` |

## Non-goals (confirmed)

HNSW, int8, WAL, Tailscale auth in-app, RRF hybrid, out-of-process extractors,
OCR, dashboard, replication.

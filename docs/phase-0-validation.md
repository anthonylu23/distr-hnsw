# Phase-0 validation prototype

Status: **M0 In progress — semantic go, dimensions unresolved**. A frozen
40-query independent holdout on `anthonypc` (run
`20260719T202526Z-holdout`) produced **32/0/8** at 512d versus name, exact
repeat retrieval evidence, and complete provenance. The semantic product bet
passes. Eligible nDCG spread is only **0.014**, below the documented **0.03**
dimension-lock threshold, so DESIGN §15 and M1 remain gated.

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
| Embed | Ollama `/v1/embeddings`; bounded concurrent batches with immediate failure context; digest persisted per model/dims |
| Nomic prefixes | `search_document:` / `search_query:` applied automatically; BGE-M3 unprefixed |
| Extract | In-process text/HTML/PDF; empty/broken PDF → `needs-ocr` |
| Prepare | Fingerprint; prune/root switch only after a clean walk; `--fresh` |
| Name FTS | Column filter grouped: `{name} : (t1 OR t2 …)` |
| Baselines judged | vs name, vs recency, vs keyword (go gate uses vs name) |
| Selection | Choose globally among configs at ≤768 dims; for `nomic-embed-text`, prefer and lock 512d as the documented capacity/quality tie-break when it is non-inferior to the best eligible dimension |
| Report | JSON/Markdown/HTML with query/source/executable/model/corpus hashes, category strata, and explicit dim-lock state |
| Repeat gate | Two evaluations from the retained binary; `compare-eval-reports.py` requires exact provenance/ranking/metric equality and records latency separately |
| Corpus assembly | [`prototype/scripts/assemble-mixed-corpus.sh`](../prototype/scripts/assemble-mixed-corpus.sh) (copy-only, unique stage, supported-extension allowlist, content-deduplicated PDFs, manifest) |

## Success criteria

- Semantic win-rate vs name baseline ≥ ~60% of decided (non-tie) queries, **or**
  clear qualitative dominance on meaning queries
- Usable provider cold start and warmed p50/p95 query latency on the GPU box
- For `nomic-embed-text`, prefer and lock 512d when at least two eligible
  dimensions were evaluated on at least 40 judged queries, 512d is within 0.03
  nDCG of the best eligible dimension, and the semantic go gate passes. This is
  a documented capacity/quality tie-break, not a claim of statistical
  superiority. Otherwise require an eligible nDCG spread of at least 0.03.
- **Do not lock dims** with fewer than 40 judged queries, only one eligible
  dimension, or a product no-go.
- Evaluate native dimensions or model-documented truncations only. Arbitrary
  vector slicing is not a valid model configuration.
- Oversized configs are diagnostics only: they do not choose the model, affect
  dimension spread, produce a go verdict, or lock dimensions.

## Independent holdout — canonical quality evidence (2026-07-19)

The holdout was curated from document content, structurally validated, and
hashed before any retrieval run. It contains 40 new queries: 8 code, 10 PDF, 6
personal-note, 6 public, and 10 study-note queries. Every relevance selector
resolves to exactly one extracted document.

| Field | Value |
|---|---|
| Work dir | `~/distr-hnsw-proto/runs/20260719T202526Z-holdout` |
| Evaluator revision | `3e18096757e426c06c567a78dc8f3ee8112783c1` |
| Query SHA-256 before retrieval | `9ee8153dea157c823cb7a1f84416ba9d554691682d4b276f87cb6762448b5ec7` |
| Query BLAKE3 in reports | `d37e55d895b974fce20fcf3498a831b47935dec2bde0c9fdddf36cff914dfb47` |
| Retained executable SHA-256 | `33efe3f04417bb9763f635d951449c52ab1927f6900e23cbed3290f97fc8ffdf` |
| Repeat comparison | **pass** — provenance, rankings, and retrieval metrics identical |
| Semantic decision | **go** |
| Dimension decision | **not locked** — eligible nDCG spread **0.014** |

### Holdout config summary

| model | dims | judged | vs name (W/L/T) | vs keyword (W/L/T) | mean recall | mean nDCG | cold / warm p50 / p95 ms |
|---|---:|---:|---|---|---:|---:|---:|
| nomic-embed-text | 768 | 40 | 100% (32/0/8) | 28.6% (4/10/26) | 0.800 | 0.678 | 1051.6 / 26.6 / 31.1 |
| nomic-embed-text | **512** | 40 | 100% (32/0/8) | 30.8% (4/9/27) | 0.800 | 0.691 | — / 25.7 / 28.9 |
| nomic-embed-text | 384 | 40 | 100% (33/0/7) | 30.8% (4/9/27) | 0.825 | 0.692 | — / 22.9 / 24.8 |

At 512d, 32 comparisons are decided and none are losses; the 95% Wilson lower
bound is 89.3%. Code, PDFs, personal notes, and study notes have recall@10 of
0.875, 1.000, 0.833, and 0.900 respectively. Public code-fragment queries are
the clear limitation at 0.167 recall and 0.056 nDCG: five of six are ties where
both semantic and name retrieval miss. Keyword search also wins more decided
comparisons than semantic search, reinforcing the phase-5 hybrid requirement.

The private database passes SQLite integrity and foreign-key checks and holds
392 files, 2625 chunks, and 7875 embeddings. Full reports, the retained binary,
and the repeat comparison remain under the private run directory. The
sanitized aggregate is
[`phase-0-bakeoff-summary.json`](phase-0-bakeoff-summary.json).

## anthonypc bake-off — mixed-v4b development set (2026-07-19)

Hardware: Fedora desktop, RTX 3060 Ti 8GB, Ollama `nomic-embed-text`.

| Field | Value |
|---|---|
| Corpus tree | `~/distr-hnsw-proto/corpora/mixed-v4-20260719` (unchanged stage) |
| Query set | `~/distr-hnsw-proto/corpora/mixed-v4b-20260719-queries.json` (beside stage) |
| Staged / evaluated files | **398 / 392** |
| Chunks | **2625** |
| Work dir | `~/distr-hnsw-proto/runs/20260719T192138Z` |
| Candidate default | `nomic-embed-text` @ **512** |
| Development-set result | **candidate go** (512d vs name **72.2%**); dimensions not yet locked |
| Mean recall@10 | **0.793** (768/512) |
| Mean nDCG@10 | **0.681** (512; slightly above 768) |
| Prepare fingerprint | `extractor=v0-inprocess;max_file_bytes=8388608;chunk_chars=2000;chunk_overlap=200` |
| Query-set BLAKE3 | `025a6ff4423709f0be1b78425b7c35bf219e8ee177c515f33d194181e74ecb01` |
| Indexed-corpus BLAKE3 | `a25ae7a3df7b345d310dc67c8606b5c5752104486bac1eb4a63d4aef9cc27c66` |
| Staged manifest SHA-256 | `54087630f0bf8445853afc00e316d831bc8592124af71ea5be296cf18777e43e` |

### What changed vs mixed-v4 no-go

Same corpus and chunk dials. Query-side only:

- Paraphrased title-token / filename-echo queries toward note/PDF **content**
- Expanded grades only where sibling docs were content-justified (and covered
  by `relevant_path_globs`)
- Did **not** add easy public fillers or filename-echo queries
- Offline hybrid RRF was not evaluated; semantic-only remains the M0 gate

### Config summary

| model | dims | judged | vs name (W/L/T) | vs recency | vs keyword | mean recall | mean nDCG | cold / warm p50 / p95 ms |
|---|---:|---:|---|---|---|---:|---:|---:|
| nomic-embed-text | 768 | 50 | 70.3% (26/11/13) | 100% | 41.7% | 0.793 | 0.674 | 4612.5 / 26.3 / 30.1 |
| nomic-embed-text | **512** | 50 | **72.2% (26/10/14)** | 100% | 45.5% | 0.793 | **0.681** | — / 24.7 / 26.9 |
| nomic-embed-text | 384 | 50 | 70.3% (26/11/13) | 100% | 39.1% | 0.753 | 0.634 | — / 22.1 / 24.2 |

### Category summary for the candidate 512d configuration

| Category | judged | vs name (W/L/T) | vs keyword | mean recall | mean nDCG |
|---|---:|---:|---:|---:|---:|
| code | 10 | 77.8% (7/2/1) | 40.0% | 0.717 | 0.636 |
| PDF | 13 | 60.0% (6/4/3) | 55.6% | 0.962 | 0.673 |
| personal notes | 8 | 100.0% (5/0/3) | 50.0% | 0.750 | 0.785 |
| public | 8 | 100.0% (3/0/5) | 0.0% (all ties) | 1.000 | 1.000 |
| study notes | 11 | 55.6% (5/4/2) | 25.0% | 0.545 | 0.421 |

Category strata remain diagnostics. Study notes stay the weakest stratum and
should stay visible in later quality work; they do not reopen the M0 gate.

Full reports remain private/gitignored. Remote source of truth:
`anthonypc:~/distr-hnsw-proto/runs/20260719T192138Z/reports/`.
Sanitized aggregate: [`phase-0-bakeoff-summary.json`](phase-0-bakeoff-summary.json).

### Candidate defaults

| Setting | Value | Notes |
|---|---|---|
| Local model | `nomic-embed-text` | Semantic quality validated on the independent holdout |
| Dims | **512 candidate** | Not locked; holdout nDCG spread is below 0.03 |
| Embed runtime | Ollama on GPU box | `OLLAMA_HOST` normalized to `http://…` |

## Remaining M0 decision

All empirical gates except dimensionality are complete. The current rule says
not to lock when eligible nDCG spread is below 0.03; the holdout spread is
0.014. M0 therefore remains open until the dimension policy is resolved without
retuning the frozen holdout. The evidence supports `nomic-embed-text`; it does
not support claiming that 512d is measurably better than 384d or 768d.

Mixed-v4b remains useful development evidence. It is not discarded, but it is
not independent confirmation because its corrections followed review of the
prior no-go losses.

## Prior mixed-v4 no-go (superseded as gate evidence)

Run `20260719T045711Z` on the same corpus with the original query set:
selected 512d **51.3%**, best 768d **55.6%** — below the ≥60% gate. Loss audit
of personal-notes + PDF vs name (sanitized):

| Mode | Count | Examples (query ids) |
|---|---:|---|
| Title-token / name-FTS advantage (relevant retrieved, name ranked higher) | 8 | q01, q03, q06, q08, q31, q35–q38 |
| Ranking failure (in top-10 but buried) | 3 | q04, q30, q32 |
| Complete miss (recall 0; extract OK) | 2 | q02 (query/content mismatch), q35/q36 style PDF misses |

That audit drove the mixed-v4b query fixes above.

## BGE-M3 diagnostic — contaminated, not gate evidence

The Ollama BGE-M3 artifact returns native 1024-dimensional vectors. BGE-M3
does not document the 768/512 Matryoshka configurations attempted by slicing
the native vectors, so those configurations are not eligible under this
prototype's evidence rules.

Run `~/distr-hnsw-proto/runs/20260719T045506Z` used the superseded `mixed-v1`
corpus. Ollama reproducibly returned JSON `NaN` for a 68-character chunk while
adjacent chunks embedded normally. A later agent completed the run by padding
individual failing inputs and reported 63.6% at sliced 512d. Both the input
mutation and undocumented slicing invalidate that result as product gate
evidence. It does not amend the nomic@512 candidate or unlock dimensions.

## Prior vault bake-off (2026-07-18)

Small Obsidian vault (14 notes / 27 chunks / 10 queries): **no-go** at 16.7% vs
name. Superseded by mixed-v4 / mixed-v4b for the M0 gate.

## Non-goals (confirmed)

HNSW, int8, WAL, Tailscale auth in-app, product-grade RRF hybrid as an M0
requirement, out-of-process extractors, OCR, dashboard, replication. The
holdout confirms semantic value, while keyword results preserve hybrid fusion
as a phase-5 product requirement rather than an M0 fallback.

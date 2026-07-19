# Phase-0 validation prototype

Status: **mixed-v4 bake-off complete** on `anthonypc`. Product go/no-go remains
**no-go** (selected 512d vs name **51.3%**; gate ≥60%). The best observed valid
configuration was 768d at **55.6%**, also below the gate. Dims are **not
locked** and M1 stays blocked.

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
| Selection | Choose globally among configs at ≤768 dims; oversized configs are diagnostic only |
| Report | JSON/Markdown/HTML with query/source/executable/model/corpus hashes, category strata, and explicit dim-lock state |
| Corpus assembly | [`prototype/scripts/assemble-mixed-corpus.sh`](../prototype/scripts/assemble-mixed-corpus.sh) (copy-only, unique stage, supported-extension allowlist, content-deduplicated PDFs, manifest) |

## Success criteria

- Semantic win-rate vs name baseline ≥ ~60% of decided (non-tie) queries, **or**
  clear qualitative dominance on meaning queries
- Usable provider cold start and warmed p50/p95 query latency on the GPU box
- Prefer dims 512 if within ~0.03 nDCG of the best eligible config for that
  model; **do not lock dims** when judged queries &lt; 40 or eligible nDCG spread
  across dims &lt; 0.03
- Evaluate native dimensions or model-documented truncations only. Arbitrary
  vector slicing is not a valid model configuration.
- Oversized configs are diagnostics only: they do not choose the model, affect
  dimension spread, produce a go verdict, or lock dimensions.

## anthonypc bake-off — mixed-v4 (2026-07-19)

Hardware: Fedora desktop, RTX 3060 Ti 8GB, Ollama `nomic-embed-text`.

| Field | Value |
|---|---|
| Corpus | `~/distr-hnsw-proto/corpora/mixed-v4-20260719` (copy-only: notes + code + PDFs + public fillers) |
| Staged / evaluated files | **398 / 392** (six unsupported-extension metadata files are not indexed) |
| Extracted / chunks | **388 / 2625** (three over-size exclusions; one empty HTML failure) |
| Query set | 50 categorized meaning-oriented queries (`~/distr-hnsw-proto/corpora/mixed-v4-20260719-queries.json`, beside stage) |
| Work dir | `~/distr-hnsw-proto/runs/20260719T045711Z` |
| Suggested (not locked) | `nomic-embed-text` @ **512** (within 0.03 nDCG of 768) |
| Strict go/no-go | **no-go** (selected 512d vs name **51.3%**; best observed 768d **55.6%**); **dims not locked** |
| Mean recall@10 | **0.783** (768/512) |
| Recency mtime collision | 0.8% (3/392) |
| Duplicate content groups | **0** in both the staged manifest and indexed DB |
| Staged manifest SHA-256 | `54087630f0bf8445853afc00e316d831bc8592124af71ea5be296cf18777e43e` |
| Indexed-corpus BLAKE3 | `a25ae7a3df7b345d310dc67c8606b5c5752104486bac1eb4a63d4aef9cc27c66` |

### Config summary

| model | dims | judged | vs name (W/L/T) | vs recency | vs keyword | mean recall | mean nDCG | cold / warm p50 / p95 ms |
|---|---:|---:|---|---|---|---:|---:|---:|
| nomic-embed-text | 768 | 50 | 55.6% (20/16/14) | 100% | 53.6% | 0.783 | 0.628 | 829.7 / 27.7 / 34.4 |
| nomic-embed-text | 512 | 50 | 51.3% (20/19/11) | 100% | 53.8% | 0.783 | 0.606 | — / 26.6 / 30.5 |
| nomic-embed-text | 384 | 50 | 51.3% (20/19/11) | 100% | 46.2% | 0.763 | 0.570 | — / 23.8 / 28.3 |

### Category summary for the selected 512d configuration

| Category | judged | vs name (W/L/T) | vs keyword | mean recall | mean nDCG |
|---|---:|---:|---:|---:|---:|
| code | 10 | 77.8% (7/2/1) | 40.0% | 0.717 | 0.636 |
| PDF | 13 | 36.4% (4/7/2) | 54.5% | 0.846 | 0.537 |
| personal notes | 8 | 14.3% (1/6/1) | 83.3% | 0.875 | 0.543 |
| public | 8 | 100.0% (3/0/5) | 0.0% (all ties) | 1.000 | 1.000 |
| study notes | 11 | 55.6% (5/4/2) | 25.0% | 0.545 | 0.421 |

The aggregate comparison from DESIGN §15 remains the formal gate. Category
strata are diagnostics and human-review guardrails, not post-hoc thresholds.
Repeated evaluation reproduced every retrieval metric exactly; provider
latency varied slightly as expected and stored/current model digests matched.

Full reports remain private/gitignored. Remote source of truth:
`anthonypc:~/distr-hnsw-proto/runs/20260719T045711Z/reports/`.
Sanitized aggregate: [`phase-0-bakeoff-summary.json`](phase-0-bakeoff-summary.json).

### Interpretation

- Scale and reproducibility gates are met: hundreds of mixed files, 50 judged
  queries, valid database integrity, matching model digests, and exact repeat
  retrieval fields.
- Semantic beats recency but does not beat filename search often enough. The
  valid best is 55.6% at 768d; the rule-selected 512d config is 51.3%.
- Code is strong while personal notes, PDFs, and study notes remain the product
  risk. Public filler is easy and should not drive a go decision.
- Next work should target retrieval quality: inspect losses, correct judgments
  and chunking, then test another model with native or documented ≤768 output.

### Defaults (provisional, explicitly unlocked)

| Setting | Value | Notes |
|---|---|---|
| Candidate local model | `nomic-embed-text` | Valid measured candidate on anthonypc |
| Candidate dims | 512 (within 0.03 nDCG of 768) | **Not locked** — product no-go |
| Embed runtime | Ollama on GPU box | `OLLAMA_HOST` normalized to `http://…` |

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
evidence. It is retained only as a diagnostic hypothesis; it does not amend the
mixed-v4 no-go, unlock dimensions, or authorize M1.

The corrected embed scheduler surfaces the failing chunk ID and length, limits
in-flight work, persists provider identity before the first vector, resumes
matching partial runs, and clears a configuration before a forced rebuild. It
does not silently pad or rewrite failed inputs.

## Prior vault bake-off (2026-07-18)

Small Obsidian vault (14 notes / 27 chunks / 10 queries): **no-go** at 16.7% vs
name. Superseded by mixed-v4 for the M0 gate.

## Non-goals (confirmed)

HNSW, int8, WAL, Tailscale auth in-app, product-grade RRF hybrid,
out-of-process extractors, OCR, dashboard, replication. A bounded offline
fusion experiment is permissible only as evidence for an explicit M0 gate
revision, not as production implementation.

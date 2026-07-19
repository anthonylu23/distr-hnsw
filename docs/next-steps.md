# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work needed to advance the current milestone.

## Phase 0 follow-through (after mixed-v4 no-go)

Mixed-v4 on anthonypc (`20260719T045711Z`): **392 evaluated files / 2625
chunks / 50 queries**; selected 512d vs name **51.3%**, best observed 768d
**55.6%** — both below the ≥60% gate. Dims are **not locked**. Corpus and
retrieval metrics are reproducible, so the remaining blocker is retrieval
quality. See [phase-0-validation.md](phase-0-validation.md).

1. Improve the product bet evidence before M1:
   - Audit the personal-note and PDF losses first. Personal notes score 14.3%
     and PDFs 36.4% vs name at selected 512d despite high recall, pointing to
     ranking/judgment quality rather than simple absence from the top 10.
   - Test one retrieval change at a time: judgment/chunking corrections first,
     then another model whose native or documented truncation dimension is
     ≤768. Keep the current categorized query set frozen as the comparison set.
     BGE-M3 may remain in the report at its native 1024d as a diagnostic, but it
     cannot satisfy the current product cap and must not be sliced to invented
     768/512 configurations.
   - If semantic-only retrieval still fails, run a bounded offline hybrid-fusion
     experiment and explicitly decide whether passing hybrid quality is enough
     to revise the M0 product gate; do not silently substitute a new gate.
   - Re-run via `assemble-mixed-corpus.sh` + `run-bakeoff-anthonypc.sh` with
     queries kept **beside** the unique stage, never inside it.
2. If a subsequent bake-off clears ≥60% vs **name** (or clear qualitative go)
   **and** judged ≥ 40: lock model/dims into DESIGN §15 and proceed to phase 1.
3. If still no-go after model/chunking/query fixes: document a deliberate
   product decision (e.g. hybrid-first) before any distributed work — do not
   start M1 on hope.

## Phase 1 (after go)

Blob plane + recovery foundation per DESIGN.md §14. Keep `prototype/` disposable.
Do not begin the milestone until the phase-0 exit gate in
[roadmap.md](roadmap.md#m0--validate-the-semantic-product-bet) passes.

## Ops notes

- `ssh anthonylu@anthonypc` may require a one-time Tailscale SSH browser check.
- Remote `OLLAMA_HOST` is `127.0.0.1:11434` (no scheme); the CLI normalizes this.
- Canonical stage: `~/distr-hnsw-proto/corpora/mixed-v4-20260719`
- Queries / SOURCES / INVENTORY / manifest: `~/distr-hnsw-proto/corpora/mixed-v4-20260719-{queries.json,SOURCES.md,INVENTORY.txt,MANIFEST.sha256,MANIFEST.digest}`
- Canonical bake-off run: `~/distr-hnsw-proto/runs/20260719T045711Z` (selected 512d 51.3%; best 768d 55.6%; dims not locked).
- Earlier `mixed-v2` and `mixed-v3` stages/runs are diagnostic artifacts, not
  evidence for the M0 decision.
- BGE-M3 diagnostic run: `~/distr-hnsw-proto/runs/20260719T045506Z` against
  superseded `mixed-v1`. Its reported 768/512 results used undocumented vector
  slicing and per-input padding to bypass provider NaNs, so they are
  contaminated diagnostics rather than decision evidence.

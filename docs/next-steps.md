# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work needed to advance the current milestone.

## Close M0 before M1

M0 is **In progress**. Current candidate evidence is mixed-v4b on `anthonypc`
(`20260719T192138Z`): same `mixed-v4-20260719` corpus tree, corrected query
set, candidate **`nomic-embed-text` @ 512** at **72.2%** vs name. The query
corrections followed a loss audit, so this is development-set evidence rather
than the final gate. See [phase-0-validation.md](phase-0-validation.md) and
[phase-0-bakeoff-summary.json](phase-0-bakeoff-summary.json).

Before M1:

1. Build from an exact source revision and retain the evaluated executable.
2. Freeze at least 40 independent, stratified meaning queries before retrieval.
3. Run the nomic 768/512/384 matrix twice against unchanged inputs.
4. Compare retrieval metrics and rankings exactly; report latency variance.
5. Audit private remote artifacts and publish the sanitized final decision.

Keep `prototype/` disposable. Hybrid RRF is not part of the semantic-only gate;
use it only as a separate fallback experiment if the holdout fails.

## Phase 1 (after M0 passes)

Start the blob plane + recovery foundation per DESIGN.md §14 and
[roadmap.md](roadmap.md#m1--build-the-blob-plane-and-recovery-foundation).
Before implementation, pin object formats, SQLite transitions, fsync
boundaries, master-key custody, backup defaults, and the crash-test matrix.
Then build the smallest recovery-first slice:

1. One regular-file class with 4 MiB encrypted chunks.
2. RF2 across two local agents with a file-backed master key.
3. Durable portal commit state machine and immutable recovery objects.
4. Failure injection and an empty-infrastructure restore drill.

## Ops notes

- `ssh anthonylu@anthonypc` may require a one-time Tailscale SSH browser check.
- Remote `OLLAMA_HOST` is `127.0.0.1:11434` (no scheme); the CLI normalizes this.
- Canonical corpus stage: `~/distr-hnsw-proto/corpora/mixed-v4-20260719`
- Development queries: `~/distr-hnsw-proto/corpora/mixed-v4b-20260719-queries.json`
  (beside stage; blake3 `025a6ff4423709f0be1b78425b7c35bf219e8ee177c515f33d194181e74ecb01`)
- Development candidate run: `~/distr-hnsw-proto/runs/20260719T192138Z`
- Prior mixed-v4 no-go run `20260719T045711Z` and BGE-M3 diagnostic
  `20260719T045506Z` are historical only, not lock evidence.

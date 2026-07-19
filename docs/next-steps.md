# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work needed to advance the current milestone.

## Close M0 before M1

M0 is **In progress — semantic go, dimensions unresolved**. The frozen
independent holdout (`anthonypc` run `20260719T202526Z-holdout`) scores
**32/0/8** at 512d versus name with exact repeat retrieval evidence and complete
provenance. See [phase-0-validation.md](phase-0-validation.md) and
[phase-0-bakeoff-summary.json](phase-0-bakeoff-summary.json).

Only one gate remains before M1:

1. Resolve the dimension policy without retuning the frozen holdout. Its nDCG
   spread is 0.014, so the existing rule correctly leaves dimensions unlocked.
2. Record the selected policy and dimension in DESIGN §15.
3. Perform the M0 exit review and then unblock M1.

Keep `prototype/` disposable. Public code fragments remain weak and keyword
search wins more decided comparisons than semantic search, so hybrid fusion
remains a phase-5 product requirement rather than more M0 prototype work.

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
- Frozen holdout queries: `~/distr-hnsw-proto/corpora/mixed-v4-holdout-20260719-queries.json`
  (pre-run SHA-256 `9ee8153dea157c823cb7a1f84416ba9d554691682d4b276f87cb6762448b5ec7`)
- Holdout run: `~/distr-hnsw-proto/runs/20260719T202526Z-holdout`
- Prior mixed-v4 no-go run `20260719T045711Z` and BGE-M3 diagnostic
  `20260719T045506Z` are historical only, not lock evidence.

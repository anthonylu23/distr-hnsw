# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work needed to advance the current milestone.

## M0 closed

M0 is **Accepted**. The frozen independent holdout (`anthonypc` policy run
`20260720T032226Z-holdout-policy`) scores **32/0/8** at 512d versus name with
exact repeat retrieval evidence and complete provenance. The documented
non-inferiority tie-break locks `nomic-embed-text @ 512`; its nDCG is 0.001
below the best eligible dimension and inside the 0.03 band. See
[phase-0-validation.md](phase-0-validation.md) and
[phase-0-bakeoff-summary.json](phase-0-bakeoff-summary.json).

Keep `prototype/` disposable and do not spend M1 on further phase-0 tuning.
Public code fragments remain weak and keyword search wins more decided
comparisons than semantic search, so hybrid fusion remains a phase-5 product
requirement.

## Phase 1 — start here

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
- Original holdout run: `~/distr-hnsw-proto/runs/20260719T202526Z-holdout`
- Accepted policy run: `~/distr-hnsw-proto/runs/20260720T032226Z-holdout-policy`
- Prior mixed-v4 no-go run `20260719T045711Z` and BGE-M3 diagnostic
  `20260719T045506Z` are historical only, not lock evidence.

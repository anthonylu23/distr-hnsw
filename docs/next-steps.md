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

## Phase 1 — in progress

M1 Pass 1 (commit spine) is implemented under `crates/distr-hnsw/`. Continue
the blob plane + recovery foundation per DESIGN.md §14 and
[roadmap.md](roadmap.md#m1--build-the-blob-plane-and-recovery-foundation).

The implemented first-pass contract and implementation dependency graph are in
[m1-storage-contract.md](m1-storage-contract.md) and
[m1-implementation-plan.md](m1-implementation-plan.md). Pass 1 provides the
commit spine through restart-safe RF2 upload and byte-identical download;
M1 remains **In progress**. The next pass should pin and implement deletion
markers plus inventory-driven reconstruction before repair/GC. Backup,
independent key recovery, supported-filesystem review, and the
blank-infrastructure restore remain later M1 gates.

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

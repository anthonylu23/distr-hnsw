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

## Phase 1 — next pass

The implemented contracts and dependency graph are in
[m1-storage-contract.md](m1-storage-contract.md) and
[m1-implementation-plan.md](m1-implementation-plan.md). Passes 1 through 3
provide restart-safe RF2 upload/download, durable logical deletion, strict
agent inventories, canonical schema-v4 migration, explicit plan/apply
recovery, agent incarnations, complete-scan observations, durability health,
and copy-first scrub/repair. M1 remains **In progress**.

The three phase-1 decisions were ratified on 2026-09-29 and recorded in
DESIGN §10, §11, §11.1, §15; see
[m1-phase-1-decisions.md](m1-phase-1-decisions.md) for the tests each must
pass. Lane B (key custody) is implemented: bound key identifier, recovery
bundle v1, `portal key` commands. Lane C's first target is implemented:
backup set v1, directory adapter, `VACUUM INTO` snapshot shipping, catalogs,
backup status, and `restore metadata` / `restore objects`, proven by a light
local empty-infrastructure drill.

The lifecycle contract is fully implemented (pass 4: drain, retire,
capacity admission, proof-based GC), and the `dm-log-writes` power-loss
drill passed on btrfs, ext4, and XFS (`scripts/power-loss-drill.sh`). What
remains for M1:

1. Land the S3-compatible backup target with Object Lock (MinIO test
   double), then offsite retention/expiry and integrity sampling.
2. Rerun `scripts/restore-drill.sh --target s3:...` against the S3 target
   and attach the report to [m1-restore-drill.md](m1-restore-drill.md); the
   directory-target run already passed.
3. Inject a real `ENOSPC` on a small loop-device filesystem and confirm the
   507 mapping and admission behavior end to end; extend the power-loss
   drill to deletion markers and concurrent writers.
4. Run large storage, corruption, movement, and GC matrices on `anthonypc`.

The representative drill remains the M1 exit gate.

## Ops notes

- Sessions may run directly on `anthonypc`; the MacBook is reachable as
  `ssh macbook`. Both checkouts track `origin/main`.
- Tests write temporary data under `target/` (see `.cargo/config.toml`), not
  tmpfs, so durability tests exercise the real filesystem.
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

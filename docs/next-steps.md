# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work needed to advance the current milestone.

## M0 and M1 closed

M0 was accepted on 2026-07-19 (`nomic-embed-text @ 512`, see
[phase-0-validation.md](phase-0-validation.md)). M1 was accepted on
2026-09-30 with recorded limitations; the evidence is indexed in
[roadmap.md](roadmap.md) and the accepted limitations are listed there.
Keep `prototype/` disposable.

## M3 exit review passed

The single-partition engine completed its seven passes on 2026-10-01
([m3-implementation-plan.md](m3-implementation-plan.md)); the roadmap holds
the per-criterion evidence and proposes acceptance with one limitation.

Immediate tasks:

1. **Owner decision:** accept M3 with the recorded limitation, or hold
   acceptance until the project dataset is measured.
2. Build `project-nomic-512` when the GPU is free (`scripts/bench/` plus the
   M0 prototype embedder), then run `oracle`, `hnsw`, `filtered`,
   `persist`, and `compact` against it and append the rows to
   [bench/README.md](bench/README.md). No code change is expected.
3. Start M2 (Tailscale identity and authorization): decide the test tailnet
   (a second machine or VM as the unauthorized node) and the portal
   certificate approach.
4. Write the deployment guide that carries the M1 accepted limitations and
   requires `scripts/restore-drill.sh --target s3:...` against the real
   offsite bucket before a deployment is called recovery ready.
5. Engine follow-ups that M4 will want: WAL group commit (contract §4 rule
   1), and a scheduler that calls `begin_compaction` when
   `compaction_recommended()` is true.

## Ops notes

- Sessions may run directly on `anthonypc`; the MacBook is reachable as
  `ssh macbook`. Both checkouts track `origin/main`.
- Tests write temporary data under `target/` (see `.cargo/config.toml`), not
  tmpfs, so durability tests exercise the real filesystem.
- Drills live in `scripts/` (`power-loss-drill.sh` and `enospc-drill.sh` need
  sudo; `restore-drill.sh`, `lifecycle-matrix.sh`, and `minio-test.sh` do
  not); reports land under `~/distr-hnsw-drill/` and are not committed.
- Phase-0 artifacts: canonical corpus `~/distr-hnsw-proto/corpora/mixed-v4-20260719`;
  accepted policy run `~/distr-hnsw-proto/runs/20260720T032226Z-holdout-policy`.

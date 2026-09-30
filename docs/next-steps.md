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

## Choosing the next milestone

Both M2 (Tailscale identity and authorization) and M3 (single-partition
vector engine) depend only on M1 and can start. The design's build order
puts M2 first because it is small (2 to 4 weeks) and every later tailnet
deployment needs it; M3 is the larger engineering risk and can proceed in
parallel as isolated engine work.

Immediate tasks regardless of order:

1. Write the deployment guide that carries the M1 accepted limitations and
   requires `scripts/restore-drill.sh --target s3:...` against the real
   offsite bucket before a deployment is called recovery ready.
2. Decide the M2 test tailnet (a second machine or VM as the unauthorized
   node) and the certificate approach for the portal.
3. Pin the M3 public benchmark datasets and recall thresholds before writing
   engine code.

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

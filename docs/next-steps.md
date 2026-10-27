# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work queue.

## Current position

- M0 accepted on 2026-07-19 (`nomic-embed-text @ 512`, see
  [phase-0-validation.md](phase-0-validation.md)). Keep `prototype/`
  disposable.
- M1 accepted on 2026-09-30 and **reopened on 2026-10-25**. M3 exit review
  withdrawn the same day. Both result from the audit in
  [hardening-2026-10.md](hardening-2026-10.md).
- M2 starts after the hardening pass.

## Hardening pass (now)

1. Done: fixes committed (`f6381d3` blob plane, `a7e0371` engine) with
   regression tests ([hardening-2026-10.md](hardening-2026-10.md)).
2. Done 2026-10-27 from the clean revision `dc276f3`:
   - the restore drill on the directory and MinIO targets, both recorded
     with `portal drill record` ([m1-restore-drill.md](m1-restore-drill.md));
   - the lifecycle matrix with the GC-then-retire and
     GC-after-metadata-restore orders
     ([m1-lifecycle-matrix.md](m1-lifecycle-matrix.md));
   - `scripts/minio-test.sh`;
   - every M3 bench with `--canonical` on all three pinned sets, with p95.
     nytimes persistence and compaction came from `835f055`, after a bench
     fix for zero-norm rows ([bench/README.md](bench/README.md)).
3. Optional: `scripts/power-loss-drill.sh` (sudo, run by the owner) for the
   changed durable-write paths (re-PUT directory sync, master key write).
4. Owner decisions:
   - the cosine default ef: 400 gives glove 0.966, below 0.97; 500 clears
     both angular sets at about 25% more latency;
   - the 1M × 512 build-throughput threshold (≥ 5k vectors/s required;
     3,182 at 100 dims and 1,725 at 256 dims with M = 32);
   - re-accepting M1 and accepting M3. `project-nomic-512` still waits on
     the GPU and remains a recorded M3 limitation.

## After hardening

1. **M2 (Tailscale identity and authorization).** Choose the test tailnet
   (a second machine or VM as the unauthorized node) and the portal
   certificate approach. The authorization matrix must cover agent `DELETE`
   and inventory first, bind agent ids to node keys, and refuse a DELETE
   without a portal-signed GC proof.
2. **M4 design prerequisites** (design before code):
   - WAL and snapshot entries carry no epoch; promotion needs `(epoch, seq)`
     or a hash chain, which is a format v2 change.
   - Parallel build is scheduling-dependent, so replicas diverge after
     compaction; decide what replicas must agree on.
   - The engine needs a concurrency model for searches during compaction
     finish and snapshot writes.
   - Snapshot cadence, compaction scheduling, and WAL group commit.
3. **Open-source hygiene.** Remove the personal vault fixture from the tree
   (history rewrite is the owner's call). Replace host and user names in
   public docs with roles. Add CI running fmt, clippy, and the non-sudo
   tests.

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

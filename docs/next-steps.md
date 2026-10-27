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

1. Blob-plane and engine fixes are in the working tree with regression
   tests ([hardening-2026-10.md](hardening-2026-10.md)). Review them.
2. Commit them as scoped commits (blob plane, engine, docs) so the drills
   and benches below run from a clean tree.
3. From that clean revision on `anthonypc`:
   - `scripts/restore-drill.sh` (directory) and
     `scripts/restore-drill.sh --target s3:…` against MinIO, then
     `portal drill record` to exercise `recovery_ready`;
   - `scripts/lifecycle-matrix.sh`, extended with GC-then-retire and
     GC-after-metadata-restore orders;
   - `scripts/minio-test.sh`;
   - optionally `scripts/power-loss-drill.sh` (sudo) for the changed
     durable-write paths.
4. Re-run the M3 benches with `--canonical` from the same clean revision:
   `oracle`, `hnsw`, int8, `filtered`, `persist`, and `compact` on all three
   pinned sets. Publish p95 next to p50 and quote ef next to every recall
   figure. Record two explicit decisions: the 1M × 512 build-throughput
   threshold, and the cosine default ef (400 gives glove 0.966, below the
   0.97 target).
5. Update the evidence documents and the roadmap. Then decide whether to
   re-accept M1 and accept M3 (owner decision). `project-nomic-512` still
   waits on the GPU and remains a recorded M3 limitation.

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

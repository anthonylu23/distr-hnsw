# Next steps

The full gated implementation sequence, milestone acceptance criteria, and
verification requirements are maintained in [roadmap.md](roadmap.md). This page
tracks only the immediate work queue.

## Current position

- M0 accepted on 2026-07-19 (`nomic-embed-text @ 512`, see
  [phase-0-validation.md](phase-0-validation.md)). Keep `prototype/`
  disposable.
- M1 accepted on 2026-09-30, reopened on 2026-10-25, and **re-accepted on
  2026-10-28**. M3's exit review was withdrawn on 2026-10-25 and M3 was
  **accepted on 2026-10-28**. Both reopenings came from the audit in
  [hardening-2026-10.md](hardening-2026-10.md), which is now closed.
- M2 is in progress ([m2-implementation-plan.md](m2-implementation-plan.md)).

## Hardening pass (closed 2026-10-28)

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
3. Decided 2026-10-27:
   - the cosine default `ef_search` is 500 (400 gave glove 0.966, below
     0.97; 500 clears both angular sets for about 25% more latency);
   - the 1M × 512 build-throughput threshold is ≥ 560 vectors/s required
     (1M in 30 minutes) and ≥ 3.3k targeted, because recovery never rebuilds
     and compaction rebuilds off the write path
     ([m3-implementation-plan.md](m3-implementation-plan.md));
   - the license copyright line stays "The distr-hnsw authors";
   - the personal vault fixture leaves the tree without a history rewrite;
     the repository is already public, so a rewrite would not withdraw it.
4. Done 2026-10-28: the power-loss drill covers the master-key write and a
   re-PUT after a crash between rename and directory sync; it passed on
   btrfs, ext4, and XFS from `10d7ed4`, and M1 was re-accepted.
5. Done 2026-10-28: `project-nomic-512` at 200k (`30fc26f`) and 1M
   (oracle, HNSW, and filtered from `b4b6710`; compaction from `684ddfe`;
   persistence from `46757b8`). Every required 1M × 512 threshold is met
   ([m3-implementation-plan.md](m3-implementation-plan.md)), and M3 was
   accepted. The 1M run found that snapshot writes built the whole file in
   memory (OOM-killed at the first 1M snapshot; the writer now streams,
   `684ddfe`) and that the persistence bench under-reported recall
   (`46757b8`).

## M2 (now)

Plan: [m2-implementation-plan.md](m2-implementation-plan.md). Work packages
1 to 3 (LocalAPI client and bind discipline, signed capability grants on
every agent route with `DELETE` only for an applied GC proof, join tokens
and key rotation) run first, because they close the agents' open `DELETE`
and inventory surface. The development laptop serves as the unauthorized
tailnet node. The owner's tailnet HTTPS is off-limits: certificates are
tested against a fake LocalAPI, and live certificate evidence waits on a
dedicated test tailnet.

## After M2

1. **M4 design prerequisites** (design before code):
   - WAL and snapshot entries carry no epoch; promotion needs `(epoch, seq)`
     or a hash chain, which is a format v2 change.
   - Parallel build is scheduling-dependent, so replicas diverge after
     compaction; decide what replicas must agree on.
   - The engine needs a concurrency model for searches during compaction
     finish and snapshot writes.
   - Snapshot cadence, compaction scheduling, and WAL group commit.
2. Done 2026-10-28: **open-source hygiene.** Public docs and scripts name
   hosts by role ([test-hosts.md](test-hosts.md)) and take SSH destinations
   from environment variables; CI runs fmt, clippy, and the non-sudo tests.
3. **Engine follow-ups carried from M3** (none blocks M2): a subprocess
   `kill -9` harness and a power-loss run for the engine; RAM admission
   for compaction (a 1M × 512 compaction holds a second copy of the live
   rows); cold-cache recovery; an int8 brute-force route to cheapen
   filtered queries near the cutover (15 to 21 ms at p50 at 1M).

## Ops notes

- Sessions may run directly on the reference host or on the development
  laptop ([test-hosts.md](test-hosts.md)). Both checkouts track
  `origin/main`.
- Tests write temporary data under `target/` (see `.cargo/config.toml`), not
  tmpfs, so durability tests exercise the real filesystem.
- Drills live in `scripts/` (`power-loss-drill.sh` and `enospc-drill.sh` need
  sudo; `restore-drill.sh`, `lifecycle-matrix.sh`, and `minio-test.sh` do
  not); reports land under `~/distr-hnsw-drill/` and are not committed.
- Phase-0 artifacts: canonical corpus `~/distr-hnsw-proto/corpora/mixed-v4-20260719`;
  accepted policy run `~/distr-hnsw-proto/runs/20260720T032226Z-holdout-policy`.

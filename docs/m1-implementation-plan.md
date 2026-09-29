# M1 implementation plan

The canonical milestone scope and acceptance gate remain in
[`roadmap.md`](roadmap.md). This page records the implemented passes and the
next dependency boundary.

## Current status

Passes 1 and 2 plus the compatibility-hardening pass are implemented locally.
The product crate provides loopback agents plus portal `init`, `put`, `get`,
`delete`, and `recover` commands. Pass 2 adds deletion-marker v1, paginated
inventories, plan/apply recovery reports, highest-generation selection,
recovery-only RF2 repair, per-file convergence, and fail-closed recovery
issues. The hardening pass integrates the audited commit spine, introduces
canonical SQLite schema v3, and migrates both prior schema-v2 layouts.

This is not M1 acceptance. Physical lifecycle work, continuous reconciliation,
supported-filesystem review, offsite backup, independent key recovery, portal
metadata restore, and the blank-infrastructure drill remain open.

## Pass 1 — commit spine

1. Pin the storage contract, persistent formats, state transitions, durability
   boundaries, and named crash points.
2. Implement the durable opaque-object agent and its loopback HTTP surface.
3. Implement regular-file chunk encryption, the immutable manifest, and the
   SQLite upload state machine.
4. Integrate RF2 upload and download across two agent processes.
5. Exercise idempotent retry, RF2 refusal, partial-visibility prevention,
   corruption detection, and every named crash boundary.

Pass 1 exits when a multi-chunk file survives injected failure at every commit
boundary and downloads with the original plaintext hash.

## Pass 2 — deletion and inventory recovery

1. Add authenticated immutable deletion markers and restartable keyed delete
   operations without physical object removal.
2. Atomically migrate SQLite v1 to the versioned v2 projection/history schema.
3. Add strict, cursor-based inventories for all object namespaces.
4. Implement deterministic plan/apply recovery, highest-generation conflict
   handling, recovery-only RF2 repair, and per-file blocked state.
5. Exercise marker format attacks, delete process crashes, inventory failures,
   stale/blank SQLite, one-copy repair, marker tombstones, conflicts, missing
   objects, wrong keys, repeat apply, and partial convergence.

Pass 2 exits when newer immutable evidence can safely rebuild stale SQLite,
required objects can be restored to RF2, and no blocked or deleted file becomes
downloadable. Those gates now pass in the light local matrix.

## Compatibility hardening — integrated baseline

1. Persist the chunk envelope version while retaining byte-identical v1 AAD.
2. Treat RF2 as a floor when extra agents reject writes or are unavailable.
3. Live-revalidate required chunks, manifests, and deletion markers before the
   committing transaction.
4. Preserve committed retries without live agents or the original source.
5. Migrate schema v1, audited commit-spine v2, and recovery-history v2 into one
   canonical v3 layout without rewriting immutable objects.
6. Exercise exact chunk download after migration and refuse file or tombstone
   commit when confirmed physical copies disappeared.

This pass exits when all Pass 1 and Pass 2 regression suites run against the
same source tree and every supported historical schema fixture converges on v3.

### Validation

Run on the development machine:

```bash
cargo fmt --all -- --check
cargo clippy -p distr-hnsw --all-targets -- -D warnings
cargo test -p distr-hnsw
```

The process-level test launches two real agent children and abruptly exits the
portal at each crash boundary. Larger future storage matrices belong on
`anthonypc`.

## Parallel lanes for the next pass

- Supported-filesystem qualification and the reconciliation observation model
  can proceed independently.
- Backup target evaluation and master-key recovery design can proceed in
  parallel, but the restore drill waits for both.
- Node observation/retirement, movement, and GC share one lifecycle owner;
  physical deletion cannot precede the observation and retention proofs.

The safety model for that owner is pinned in
[`m1-lifecycle-contract.md`](m1-lifecycle-contract.md).

## Remaining M1 passes

- continuous scrub/reconciliation and explicit degraded-state reporting;
- safe movement, quotas, observation, retirement, and garbage collection;
- versioned offsite backup, SQLite history replication, and independent key
  recovery;
- portal-loss and empty-infrastructure restore drills.

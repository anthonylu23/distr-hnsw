# M1 implementation plan

The canonical milestone scope and acceptance gate remain in
[`roadmap.md`](roadmap.md). This page records implementation dependencies for
the current first pass.

## Current status

Pass 1 is implemented locally. The product crate under `crates/distr-hnsw`
provides loopback agent processes plus portal `init`, `put`, and `get` commands.
Tests cover canonical authenticated manifests, durable object idempotency and
hash verification, state-transition legality, conflicting upload idempotency,
RF2 refusal, partial visibility, corrupt-replica fallback/fail-closed behavior,
and abrupt portal exit at every named boundary.

This is not M1 acceptance. Linux filesystem review, delete/lifecycle work,
inventory reconstruction, independent recovery, and the blank-infrastructure
drill remain open.

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

## Parallel lanes after contract review

- Agent durable storage can proceed alongside SQLite and pure state-machine
  work once object identifiers and the durable-PUT contract are fixed.
- Format/crypto work can proceed alongside the agent because agents store
  opaque bytes.
- Failure-harness scaffolding can proceed alongside both once CLI commands and
  failpoint names are fixed.
- Portal integration waits for all three lanes.

Persistent formats and state transitions have one authoritative owner. Delete
and garbage-collection semantics must not be developed independently because
they share generation, observation, reference, and node-retirement rules.

## Deferred M1 passes

- deletion markers and stale-metadata precedence;
- inventory-driven reconstruction, scrub, and repair;
- safe movement, quotas, retirement, and garbage collection;
- versioned offsite backup, SQLite history replication, and independent key
  recovery;
- portal-loss and empty-infrastructure restore drills.

# M1 lifecycle contract

This document pins the safety model that must exist before the blob plane gains
physical DELETE, placement movement, node retirement, or garbage collection. It
extends the recovery contract in
[`m1-storage-contract.md`](m1-storage-contract.md) without weakening the
invariants in `DESIGN.md`.

## Safety boundary

The current implementation may create, verify, inventory, and repair immutable
objects. It may not physically delete them. A placement row, an absent object
in one scan, an expired upload, or a SQLite reference count is not sufficient
deletion evidence.

Durability and placement policy are separate:

- the durability floor is the minimum number of live, hash-verified copies in
  distinct failure domains required for acknowledgement;
- desired placement includes replica count, tier, volume, and policy
  constraints;
- an object can meet the durability floor while remaining policy-degraded, and
  that state must stay observable until reconciliation fixes it.

## Node identity and observation

Every agent incarnation must have an immutable identity. Reusing a human name
does not reuse the incarnation. A full inventory observation records the agent
incarnation, scan id, start and completion time, namespace, final cursor, and
inventory digest. Partial, failed, or cursor-inconsistent scans prove nothing
about absence.

Deletion observation is generation-based. A node has observed a file deletion
only when a complete scan from the same active incarnation finishes after the
deletion marker was committed and the scan contains no object that would make
an older live generation authoritative. Long-absent nodes block collection
until they return and are reconciled or are formally retired.

Retirement is an explicit durable portal transition. A retired incarnation
cannot rejoin. Its storage must be wiped or fully reconciled before the machine
joins under a new incarnation.

## Repair and movement

Scrub reads stored bytes and verifies their content address. Corrupt or missing
copies never count toward durability or desired placement.

Repair and movement use one copy-first protocol:

1. select a valid source and an eligible destination;
2. PUT the immutable bytes to the destination;
3. GET and hash-verify the destination copy;
4. persist the confirmed destination placement;
5. re-evaluate the live durability floor and desired placement;
6. only then mark an obsolete placement `orphaned`.

An interrupted move is safe to retry. It leaves either an extra durable copy or
an unconfirmed destination; it never removes the source first.

## Physical deletion proof

An object can be submitted to agent DELETE only when one transaction records a
proof containing all applicable facts:

- the object is not required by the highest authoritative live generation;
- every active node incarnation has a complete qualifying observation;
- the configured live-retention horizon has elapsed;
- no committed manifest references a candidate chunk;
- deletion-marker and older-generation retention rules are satisfied;
- shared-object and staging references have been checked against the canonical
  projection and immutable history;
- removing the placement cannot violate the durability floor or an in-flight
  movement;
- the node is active, or its retirement procedure explicitly owns cleanup;
- offsite retention is treated separately and is not represented as
  live-cluster deletion.

Proof inputs are immutable or versioned. If policy, node membership, generation,
or inventory state changes before execution, the proof is stale and DELETE must
be replanned.

## Capacity and failure behavior

Admission uses effective free space: the lesser of configured quota headroom
and filesystem free space, after reserving repair and compaction headroom.
ENOSPC or global budget exhaustion rejects new writes with actionable health;
it never lowers the durability floor or triggers emergency deletion that skips
the proof above.

## Implementation order

1. Persist agent incarnations, complete scan observations, desired-placement
   health, and restartable reconcile jobs. **Implemented** (pass 3): see
   `m1-storage-contract.md`, "Agent incarnations" and "Scrub and durability
   health". A newly observed incarnation supersedes the active one
   automatically; the superseded one is refused if it returns. Legacy
   placement rows are adopted by the first incarnation observed for their
   agent, an accepted migration limitation that live revalidation and scrub
   correct.
2. Add continuous inventory comparison and scrub/repair without DELETE.
   **Implemented** (pass 3): `portal scrub [--repair] [--interval]`.
   Unreferenced objects (older generations, staging garbage) are counted but
   not verified or persisted; GC planning owns them.
3. Add copy-first movement and formal retirement.
4. Add quota/headroom admission and ENOSPC failure injection.
5. Add proof-producing GC planning; keep it dry-run.
6. Add agent DELETE only after stale-node, interrupted-move, retention, and
   proof-invalidation tests pass.

Large corruption, movement, retirement, and storage-pressure matrices run on
`anthonypc`. This pass does not provide backup, independent key recovery, or
the empty-infrastructure restore required for M1 acceptance.

# M1 storage contract

This document pins the implemented M1 blob-plane contract through pass 2. It
is subordinate to `DESIGN.md` and `docs/roadmap.md`; later M1 work may extend
these rules but must not weaken their durability or recovery invariants.

## Implemented boundary

The product supports one regular-file class, seekable local input, fixed 4 MiB
plaintext chunks, a file-backed master key, and RF2 across two agents in
distinct configured failure domains. It now includes restart-safe logical
deletion, paginated agent inventories, deterministic recovery planning, and
explicit recovery application with recovery-only repair.

Agents remain loopback-only and unauthenticated for M1 development. Physical
deletion, continuous reconciliation, movement, node retirement, quotas, GC,
offsite backup, independent key recovery, and blank-infrastructure restore are
not implemented. Until the full M1 exit gate passes, the service must not hold
the only copy of a file or claim recovery readiness.

## Immutable object namespaces

Every stored object is immutable and addressed by the lowercase hexadecimal
BLAKE3 digest of its complete stored bytes. Agents maintain distinct `chunk`,
`manifest`, and `deletion_marker` namespaces:

```text
objects/<kind>/<hash[0..2]>/<hash[2..4]>/<hash>
```

Agents treat bytes as opaque. PUT verifies the supplied digest before
acknowledgement; GET verifies stored bytes. Acknowledgement follows a
same-directory temporary write, file sync, atomic rename, and parent-directory
sync. Linux uses `fsync`; macOS requests `F_FULLFSYNC` for regular files and
uses directory `fsync` for rename persistence.

`GET /v1/inventory/{kind}?after=<hash>&limit=<n>` returns hash-sorted
`{objects:[{hash,size}],next_after}` pages. The cursor is exclusive and the
default and maximum page size are 1,000. Malformed fanout entries fail the
page; recovery never silently omits them.

## Upload and manifest commit

Before dispatch, the portal prehashes the source and durably records the file
and upload identifiers, request fingerprint, wrapped per-file content key,
plaintext digest and length, chunk nonces, and storage class. The fingerprint
covers plaintext digest, size, display name, and storage class. Reusing a key
for another request conflicts. Each chunk is rehashed immediately before
encryption to prevent nonce reuse if the source changes.

The manifest v1 envelope retains its existing format: a clear magic, version,
file id, and generation header authenticates an encrypted payload containing
file metadata, wrapped content key, and ordered chunk records. Decoders reject
trailing bytes, unsupported versions, invalid lengths, tampering, and wrong
keys.

The portal persists exact manifest bytes and hash before dispatch. A file is
visible only after its manifest and all referenced chunks have confirmed RF2
placements in distinct failure domains and SQLite commits the projection.

## Logical deletion

`portal delete --idempotency-key <key> <file-id>` creates generation
`current_generation + 1`. Deletion marker v1 uses a clear magic, version, file
id, and generation header plus an authenticated encrypted `deleted_at`
payload. Exact bytes and hash are persisted before replication.

The marker must reach RF2 before one SQLite transaction inserts its immutable
generation record, changes the current file projection to `deleted`, and marks
the operation committed. Downloads remain available before that transaction
and are denied afterward. Reusing the same key resumes or returns the same
success. A key reused for another file conflicts; a new key for an already
deleted file returns `AlreadyDeleted`. Logical deletion never calls agent
DELETE and never removes older manifests or chunks.

## SQLite schema v2

Schema v2 stores `files` as the current projection with generation and
`committed`, `deleted`, or `recovery_blocked` state. It adds immutable
`file_manifests`, `file_chunks`, `deletion_markers`, restartable
`delete_operations`, and `recovery_issues`; placements accept all three object
kinds. Opening a v1 database migrates it in one transaction, preserving
committed uploads, chunks, placements, and the exact existing manifest bytes.
Unknown future schema versions are rejected.

## Recovery contract

`portal recover` is read-only planning; `portal recover --apply` is the only
recovery mutation path. Both emit deterministic `RecoveryReportV1` JSON with
mode, inventory digest, agent summaries, per-file decisions, repairs, issues,
conflicts, and totals. Exit status is 0 when converged, 2 when files remain
blocked, and 1 for a global operational or trust failure.

Recovery inventories every configured agent, verifies listed sizes and object
hashes, decrypts manifests and markers, merges immutable evidence with SQLite,
and chooses the highest generation per file. Different immutable objects or a
manifest/marker collision at the same winning generation block the file and
are never exposed.

Apply converges per file. A winning manifest requires itself and every
referenced chunk at RF2; a winning marker requires the marker at RF2. A valid
source copy is repaired to deterministic missing agents in distinct failure
domains before SQLite exposure. Older generations remain untouched. Missing
or corrupt required objects persist a recovery issue and make a live winner
`recovery_blocked`; an existing or winning deletion remains unreadable even
when its marker cannot regain RF2. Wrong keys, malformed inventories,
unavailable agents, or another untrustworthy global scan abort without
mutation.

## Named crash boundaries

Upload tests abruptly stop after plan persistence, first chunk replica, chunks
at RF2, first manifest replica, manifest at RF2, before commit, and after
commit. Delete tests stop after plan persistence, first marker replica, marker
at RF2, before tombstone commit, and after commit. Retrying with the same
idempotency key must converge without premature visibility changes.

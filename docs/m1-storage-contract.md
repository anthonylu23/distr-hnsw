# M1 storage contract

This document pins the implemented M1 blob-plane contract through the
lifecycle observation and scrub pass (pass 3), the master-key custody lane,
and the backup-set lane. It is subordinate to `DESIGN.md` and
`docs/roadmap.md`; later M1 work may extend these rules but must not weaken
their durability or recovery invariants.

## Implemented boundary

The product supports one regular-file class, seekable local input, fixed 4 MiB
plaintext chunks, a file-backed master key, and RF2 across two agents in
distinct configured failure domains. It now includes restart-safe logical
deletion, paginated agent inventories, deterministic recovery planning, and
explicit recovery application with recovery-only repair.

The integrated baseline also preserves chunk-envelope v1 byte compatibility,
treats RF2 as a floor when extra agents fail, live-revalidates every required
object before file or tombstone commit, and lets already-committed idempotent
retries succeed without a source file or live agents.

Pass 3 adds immutable agent incarnations, complete-scan observations,
placement verification states, per-object durability health, and a
copy-first scrub/repair job that never deletes. The custody and backup lanes
add the bound key identifier, recovery bundle, backup set v1 with a
versioned-directory target, and the restore path that rebuilt a cluster
from backup alone in the light local drill. Agents remain loopback-only and
unauthenticated for M1 development. Physical deletion, movement, node
retirement, quotas, GC, the S3-compatible target, and the representative
restore drill on `anthonypc` are not implemented. Until the full M1 exit gate passes, the service must not hold
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
sync. Linux uses `fsync`. On macOS both the file and directory syncs issue
`F_FULLFSYNC` (the standard library's `sync_all` does so on Apple targets);
whether a directory `F_FULLFSYNC` persists a rename on APFS is unverified, and
no filesystem is yet qualified for power loss. See
[m1-filesystem-qualification.md](m1-filesystem-qualification.md).

## Master-key custody

The master key is a 32-byte file with owner-only permissions, zeroized in
memory on drop. Its non-secret identifier is the first 16 bytes of
`BLAKE3.derive_key("distr-hnsw:master-key-id:v1", key)`. `portal init`
binds that identifier into `portal_meta`; every later `Portal::open`
(upload, download, delete, recover) rebinds and fails closed with
`MasterKeyMismatch` before any object is decrypted if a different key is
presented. A database created before schema v5 is bound by the first key
that opens it.

Recovery bundle v1 wraps the key for recovery without any surviving cluster
machine. Layout: magic `DHRB`, version `u16 = 1`, key id, KDF id
(`1` = Argon2id), `m_cost_kib`, `t_cost`, `p_cost`, 16-byte salt, 24-byte
nonce, and the XChaCha20-Poly1305 ciphertext of the key; everything before
the nonce is authenticated data. Defaults are RFC 9106's 64 MiB / t = 3 /
p = 4; decoding refuses parameters below the OWASP floor (19 MiB / 2 / 1)
or above a ceiling (4 GiB / 64 / 64) so a corrupted header cannot demand
unbounded work. The bundle is printed as armored base64 with a short
checksum line so transcription errors are reported as such rather than as a
wrong passphrase. Passphrases are generated as 24 Crockford base32
characters (120 bits) unless supplied on stdin (minimum 12 bytes).

`portal init` emits the bundle once for a newly created key and verifies the
round-trip before printing; `--no-recovery-bundle` skips it for development.
`portal key export-recovery` re-emits one; `portal key restore --verify`
proves a bundle and passphrase recover the key bound to a database without
writing; `portal key restore` writes the key to a path that must not exist.
Wrong passphrase, any tampering, unsupported version, or out-of-range
parameters fail without partial output.

## Agent incarnations

Each volume carries an immutable incarnation identity, a UUID created durably
on first open and reported by `GET /v1/health` as `incarnation_id` beside the
configured `id` and `failure_domain`. A wiped or replaced volume yields a new
incarnation even under the same agent name; a malformed identity file fails
the agent closed.

The portal records every reported incarnation in `agent_incarnations` with
status `active`, `superseded`, or `retired`, and at most one active
incarnation per agent id. On contact:

- the first incarnation ever seen for an agent id becomes active and adopts
  that agent's legacy placement rows (rows without an incarnation);
- the same incarnation refreshes `last_seen_at`;
- a different incarnation supersedes the active one; the previous
  incarnation's placements no longer count toward the durability floor and
  are reset to `pending` when the object is next written to that agent;
- a superseded or retired incarnation presenting itself again is refused, so
  two volumes cannot alternate under one name and a retired node cannot
  rejoin. Formal retirement is a later pass.

Every placement counted toward RF2 must be `confirmed` on an active
incarnation (or a not-yet-adopted legacy row). Upload, delete, recovery, and
scrub all verify identity and incarnation before recording any placement.

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
visible only after its manifest and all referenced chunks have live
GET-and-hash-verified RF2 placements in distinct failure domains and SQLite
commits the projection. Persisted placement rows alone cannot authorize commit.

## Logical deletion

`portal delete --idempotency-key <key> <file-id>` creates generation
`current_generation + 1`. Deletion marker v1 uses a clear magic, version, file
id, and generation header plus an authenticated encrypted `deleted_at`
payload. Exact bytes and hash are persisted before replication.

The marker must reach live, hash-verified RF2 before one SQLite transaction
inserts its immutable generation record, changes the current file projection
to `deleted`, and marks the operation committed. Downloads remain available
before that transaction and are denied afterward. Reusing the same key resumes
or returns the same success. A key reused for another file conflicts; a new key
for an already deleted file returns `AlreadyDeleted`. Logical deletion never
calls agent DELETE and never removes older manifests or chunks.

## Backup set v1

`portal backup --target dir:<path>` needs no master key. It copies every
immutable object any generation in history references (manifests, chunks,
deletion markers), so deleted generations stay in the set until offsite
retention expires. Each object is fetched from an agent with a hash-verified
live copy, written to `backup/v1/objects/<kind>/<hash>` with `create_new`
semantics (identical bytes are accepted, different bytes are a conflict),
read back from the target and hash-verified, and only then recorded in
`backup_objects`. A verified object is skipped on later runs; the job is
restartable.

A `VACUUM INTO` snapshot of the database is shipped to
`backup/v1/sqlite/<utc>-<hash8>.db`, read back and verified, whenever the
content generation changed since the last shipped snapshot. The content
generation is a `portal_meta` counter bumped by file commit, delete commit,
and recovery apply; backup bookkeeping alone does not trigger a snapshot.
Each job then writes `backup/v1/catalog/<utc>-<job>.json` listing the
verified objects, the snapshot, and the bound key identifier. Objects are
self-verifying by content address; the catalog is advisory and the set's
authenticity rests on the target's versioning and immutability.

`portal health` reports per target the last job, verified and pending object
counts, backup lag (age of the oldest unverified object's file change), and
the last snapshot. `recovery_ready` is always false until the representative
empty-infrastructure drill has passed for the deployment.

The versioned-directory adapter provides layout-level immutability only and
durable writes through the same temp-write, sync, link, directory-sync
sequence as agents. Offsite protection requires a rotated or remote disk; the
S3-compatible adapter with Object Lock is the first supported network target
and is not yet implemented.

### Restore

`portal restore metadata --target … --database <new>` copies the latest (or
named) snapshot into a path that must not exist and checks its hash against
the newest catalog that names it. `portal restore objects --target … --agent
…` copies every backed-up object into the agents until two distinct failure
domains hold read-back-verified copies; it needs neither key nor database.
`portal recover --apply` then rebinds placements to the new incarnations and
converges file records. The light local drill (`commit_spine_process.rs`)
destroys agents, volumes, database, and key file, then rebuilds from the
backup set, the recovery bundle, and an off-cluster passphrase, and
downloads the committed file byte for byte while the deleted file stays
unreadable.

## SQLite schema v6

Schema v3 stores `files` as the current projection with generation and
`committed`, `deleted`, or `recovery_blocked` state. It adds immutable
`file_manifests`, `file_chunks`, `deletion_markers`, restartable
`delete_operations`, and `recovery_issues`; placements accept all three object
kinds. Per-chunk envelope versions are persisted in upload and recovered-file
history; manifest v1 implies chunk envelope v1.

Schema v4 rebuilds `placements` with `incarnation_id`, `last_verified_at`,
and `last_verified_job` columns and the state set `pending`, `confirmed`,
`missing`, `corrupt`. It adds `agent_incarnations`, `reconcile_jobs`,
`scan_observations`, and `object_health`. Migrated placement rows keep a NULL
incarnation until their agent is observed. Opening a database marks any job
still `running` from a previous process as `interrupted` and fails its open
scans; interrupted scans prove nothing. Schema v5 adds `portal_meta`, which
holds the bound master-key identifier and the content generation. Schema v6
adds `backup_jobs`, `backup_objects`, and `backup_snapshots`.

The two earlier development lines both used schema version 2 for incompatible
layouts. Opening a v2 database inspects its table shape and atomically migrates
either the audited commit-spine layout or the recovery-history layout to
canonical v3, then v4, then v6 (v5 and v6 are additive). V1 also migrates
through the same chain. Existing manifest
bytes, ciphertext hashes, and chunk-v1 AAD remain unchanged. Unknown or
unrecognized schema layouts fail closed.

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

## Scrub and durability health

`portal scrub` needs no master key. It verifies each configured agent's
identity and incarnation, then lists every namespace with strict paginated
inventories. A namespace is observed only when every page arrived in
hash-sorted order and the final page carried no cursor; each observation is
persisted in `scan_observations` with its incarnation, final cursor, object
count, and BLAKE3 inventory digest. A malformed page fails that scan and
proves nothing.

The required set is derived from the projection: the manifest and every
chunk of each `committed` file and the marker of each `deleted` file. Files
in `recovery_blocked` belong to `recover`. For every required object and
every reachable agent, scrub reads the listed copy and checks its hash and
size, recording `confirmed` or `corrupt`; a copy absent from a complete scan
of an active incarnation is recorded `missing` if a placement row existed.
Copies on unreachable agents or behind failed scans stay in their recorded
state and are reported as unverified. Verification never removes rows or
objects.

Per-object health is persisted in `object_health` from the copies verified in
this job: `lost` (no valid copy), `at_risk` (fewer than two verified failure
domains), `degraded` (floor met but a configured copy is missing or corrupt,
or fewer copies than desired), or `durable`. Only `confirmed` placements on
active incarnations serve downloads and count toward commit.

`portal scrub --repair` restores non-durable objects copy-first: PUT the
verified bytes to a destination, read them back and hash-verify, and only
then confirm the placement. A corrupt copy is replaced in place by the
agent's atomic rename; nothing is deleted first. Repair runs only under a
complete observation (every agent reachable, every scan complete); otherwise
repairs are deferred and reported. `--interval` repeats the job
continuously. `portal health` prints the persisted job, incarnation, scan,
and health state without contacting agents. Exit status is 0 only for a
complete observation with every required object durable, 2 otherwise, and 1
for an identity, incarnation, or operational failure.

## Named crash boundaries

Upload tests abruptly stop after plan persistence, first chunk replica, chunks
at RF2, first manifest replica, manifest at RF2, before commit, and after
commit. Delete tests stop after plan persistence, first marker replica, marker
at RF2, before tombstone commit, and after commit. Retrying with the same
idempotency key must converge without premature visibility changes.

Physical lifecycle behavior is governed by
[`m1-lifecycle-contract.md`](m1-lifecycle-contract.md). Agent DELETE remains
unimplemented.

# M1 storage contract

This document pins the first implementation pass for the M1 blob-plane commit
spine. It is subordinate to `DESIGN.md` and `docs/roadmap.md`; later M1 work
extends these rules without weakening them.

## First-pass boundary

The first pass supports one regular-file class, seekable local input, fixed
4 MiB plaintext chunks, a file-backed master key, and RF2 across two agents in
distinct configured failure domains. Agents and the portal bind to loopback for
development. Tailscale identity and production authorization remain M2 work.

Delete markers, repair, movement, garbage collection, offsite backup, and
empty-infrastructure restore are later M1 passes. Until the full M1 exit gate
passes, the service must not hold the only copy of a file or claim recovery
readiness.

## Object identities and namespaces

Every stored object is immutable and addressed by the lowercase hexadecimal
BLAKE3 digest of its complete stored bytes. Agents maintain distinct `chunk`
and `manifest` namespaces, each fanned out by the first four hash digits:

```text
objects/<kind>/<hash[0..2]>/<hash[2..4]>/<hash>
```

Agents treat bytes as opaque. They verify the supplied digest before a PUT is
acknowledged and verify stored bytes on GET. Acknowledgement follows a
same-directory temporary write, file sync, atomic rename, and parent-directory
sync. Linux uses `fsync`; macOS requests `F_FULLFSYNC` for regular files and
uses directory `fsync` for rename persistence.

## Encryption plan

Before any chunk is dispatched, the portal prehashes the seekable source and
persists:

- file and upload identifiers;
- plaintext BLAKE3 digest and length;
- the idempotency request fingerprint;
- the wrapped per-file content key;
- envelope version, plaintext digest, and one random nonce per chunk;
- the selected storage class.

The request fingerprint covers the plaintext digest, size, display name, and
storage class. Reusing an idempotency key with another fingerprint is rejected
before encryption. Each chunk is rehashed and compared with its persisted plan
immediately before encryption, preventing key/nonce reuse if the source changes
between the initial prehash and dispatch.

Chunk AAD authenticates the envelope version, file id, ordinal, and plaintext
length. Ciphertext, including the Poly1305 tag, is the stored chunk object.

## Manifest format and commit decision

The manifest is a versioned binary envelope with a minimal cleartext header
containing magic, envelope version, file id, and generation. Its encrypted
payload contains the display name, original length and digest, wrapped content
key, and the ordered chunk nonce/hash/length records. Integer fields are
little-endian and strings/byte arrays are length-prefixed; decoders reject
trailing bytes, unsupported versions, and unreasonable lengths.

After every chunk reaches RF2, the portal constructs and persists the exact
manifest bytes and hash before manifest dispatch. From that point the upload is
irrevocable commit intent: restart recovery must retry the same manifest to RF2
and finish the SQLite commit. Chunks without a persisted manifest remain
uncommitted staging objects. This first pass does not reconstruct an upload
from agent inventories alone; that is required before the full M1 recovery
gate.

Only the SQLite `committed` state is visible or downloadable. A transaction
that exposes the file is permitted only after the manifest and every referenced
chunk have two confirmed placements in distinct failure domains.

## State transitions

```text
staging -> replicating_chunks -> replicating_manifest -> committed
```

Retries may remain in the same state or advance. Regression is illegal.
Placements independently transition from `pending` to `confirmed`; a failed
attempt stays pending and can be retried. The later lifecycle pass adds
`orphaned` after its cleanup and retirement rules are pinned.

## First-pass crash points

The test harness injects process-equivalent failure after:

1. encryption-plan persistence;
2. the first confirmed chunk replica;
3. all chunks reach RF2;
4. the first confirmed manifest replica;
5. the manifest reaches RF2;
6. immediately before the committing transaction;
7. immediately after that transaction.

Restart with the same idempotency key must converge on one file id and one set
of ciphertext objects. No pre-commit state may be read through the file API.

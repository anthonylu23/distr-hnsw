# Architecture overview

This page orients a new reader. The authoritative specification is
[DESIGN.md](../DESIGN.md); persistent formats and safety rules are pinned in
the contracts linked below. Milestone status lives in
[roadmap.md](roadmap.md).

## What distr-hnsw is

A self-hosted storage service for a Tailscale network. Files are chunked,
encrypted, and replicated across machines you already own (the **blob
plane**). Their contents are embedded and indexed with distributed HNSW
graphs for semantic search (the **vector plane**). One portal coordinates
both planes; one SQLite database holds control-plane metadata.

## What exists today

| Component | Crate | State |
|---|---|---|
| Blob plane: agents, portal commit spine, deletion, recovery, scrub and repair, drain and retire, capacity admission, proof-based GC, key recovery, backup and restore | `crates/distr-hnsw` | Implemented (M1, accepted) |
| Single-partition vector engine: flat and int8 vectors, HNSW, filtered search, WAL and snapshots, compaction, RAM admission | `crates/distr-hnsw-index` | Implemented (M3, accepted) |
| Benchmark harness on pinned public datasets and the 512-dim project set | `crates/distr-hnsw-bench` | Implemented |
| Tailscale identity and authorization: tailnet binds, agent grants and joins, the portal service with WhoIs identity, sessions, API keys, audit log, and certificates | `crates/distr-hnsw` | In progress (M2) |
| Distributed vector plane, extraction and embedding pipeline, dashboard, vault | — | Not started (M4–M7) |
| Phase-0 validation prototype | `prototype/` | Disposable; not product code |

Today everything runs as one binary, `distr-hnsw`, with three command
groups: `agent` (a storage daemon), `portal` (coordinator commands over the
SQLite database, plus `portal serve`, the HTTPS API), and `index` (moving
engine state through the blob plane). Agents bind to their node's tailnet
address (or to loopback with an explicit insecure flag) and accept only
requests that carry a grant signed by the portal.

## Blob plane

```text
   browsers, apps ── HTTPS on the tailnet (WhoIs + session or API key)
             │
             portal (CLI commands and `portal serve`)
             ├─ SQLite metadata (schema v11): uploads, files, generations,
             │  placements, incarnations, scans, GC proofs, backup state,
             │  grant-signing keys, joined agents, join tokens, users,
             │  sessions, API keys, audit log
             ├─ master key (file) ── recovery bundle (offline, passphrase)
             │
   PUT/GET/inventory/DELETE over HTTP, each with a signed grant
             │
   ┌─────────┴─────────┐
 agent A (domain 1)  agent B (domain 2)  …   immutable content-addressed
 objects/chunk|manifest|deletion_marker/…    objects on a local volume
```

- **Objects.** Every stored object is immutable and addressed by the BLAKE3
  hash of its bytes. Files are split into 4 MiB chunks encrypted with
  XChaCha20-Poly1305 under a per-file content key. An encrypted **manifest**
  per file generation lists the chunks. An encrypted **deletion marker**
  records a delete as a new generation.
- **Agents** store opaque bytes durably: a temporary write, file sync,
  atomic rename, and directory sync before acknowledging. They verify hashes
  on PUT and GET, list inventories, report capacity, and make no placement
  decisions.
- **Agent authorization.** An agent joins once with a single-use token and
  then trusts the portal's key set (Ed25519 verification keys and a grant
  epoch). Every request carries a grant naming the agent, the operation,
  the object, and an expiry of two minutes; a `DELETE` grant also names the
  GC proof it applies. On a tailnet address the agent also checks with
  Tailscale WhoIs that the caller is the portal node it pinned at join.
  Key rotation and revocation reach an agent on its next request: it
  answers 409, and the portal pushes the new key set, which the old key
  vouches for.
- **Portal service.** `portal serve` listens only on its node's tailnet
  address and only over HTTPS, with the node's certificate from
  `tailscaled`, renewed and swapped in without a restart. Each connection
  is identified by Tailscale WhoIs on its socket address; identity headers
  are stripped and audited. Requests authenticate with a browser session
  (bound to its node and user, CSRF-checked) or an API key (a stored
  verifier with collection scopes), and every sign-in, refusal, and state
  change is appended to an audit log.
- **Commit spine.** The portal persists an idempotent upload plan, places
  every chunk and then the manifest on agents in at least two failure
  domains (RF2), re-verifies them live, and only then commits the file in
  SQLite. Deletes replicate the marker to RF2 before the tombstone commits.
  A crash at any boundary leaves either a committed file or collectible
  garbage.
- **Recovery.** Manifests and markers on the agents are the durable truth.
  `portal recover --apply` rebuilds file records from agent inventories,
  choosing the highest generation per file, so a stale SQLite restore cannot
  resurrect a deleted file or lose a newer one.
- **Lifecycle.** Scrub verifies stored bytes and repairs copy-first. Drain
  moves copies off an agent and retire removes its incarnation permanently.
  GC deletes physically only through a recorded proof: complete
  observations, retention elapsed, no unknown control objects, backup
  coverage, and no pending recovery.
- **Backup and restore.** Objects are copied to a versioned directory or an
  S3-compatible bucket with never-overwrite semantics. `VACUUM INTO` SQLite
  snapshots are shipped once their objects are verified. Restore rebuilds a
  cluster from the backup set, the recovery bundle, and the passphrase.
  `recovery_ready` is derived from a recorded restore drill.

Contracts: [m1-storage-contract.md](m1-storage-contract.md) (formats,
schema, commit, recovery, backup),
[m1-lifecycle-contract.md](m1-lifecycle-contract.md) (deletion proof,
movement, retirement),
[m1-filesystem-qualification.md](m1-filesystem-qualification.md)
(durable-write review), and
[m1-phase-1-decisions.md](m1-phase-1-decisions.md) (key custody, backup
defaults, admission).

## Vector engine (one partition)

- **Storage.** Flat `f32` originals and int8 quantized copies with a scale
  and norm per vector. The hot set in RAM is about 910 bytes per vector at
  512 dims and M=32; originals are served from the memory-mapped snapshot.
- **Graph.** In-repository HNSW with per-metric defaults, deterministic
  level assignment, and parallel bulk build. Tombstoned records stay
  traversable and are never returned.
- **Filtered search.** Masked traversal for broad filters; exact brute force
  when the allowed set is at most `200 · ef_search`.
- **Persistence.** A CRC-framed WAL with sequence numbers and an idempotency
  window, plus versioned snapshots verified per section. Recovery replays
  the newest verifiable snapshot and the WAL tail. Only an interrupted
  append at the end of the newest segment is repaired; all other damage
  fails closed. Snapshots are streamed to disk, so writing one needs no
  second copy of the partition in memory.
- **Compaction.** A short capture step, then a rebuild that writers do not
  wait on, then a swap.
- **Archive.** `distr-hnsw index archive|restore` stores a partition's
  manifest, snapshot, and WAL segments as ordinary blob-plane files.
- **Measured scale.** On one 16-thread host with 1M × 512 vectors
  (`project-nomic-512`): recall@10 0.993 at the default `ef_search` 500,
  single-thread p95 3.1 ms, bulk build 1,700 vectors/s, recovery 3.7 s
  ([bench/README.md](bench/README.md)). Compaction holds a second copy of
  the live rows and is not yet admitted against the RAM budget.

Contract: [m3-engine-contract.md](m3-engine-contract.md). Benchmarks:
[bench/README.md](bench/README.md).

## Tech stack

Rust (tokio, axum, reqwest and tokio-rustls with rustls), SQLite via
rusqlite, BLAKE3, Ed25519 (ed25519-dalek),
XChaCha20-Poly1305 and Argon2id from RustCrypto, roaring bitmaps, and the
AWS SDK for S3-compatible backup targets. Tests use real temporary volumes
on disk (`TMPDIR=target/`); drills and large matrices live in
[`scripts/`](../scripts/README.md).

## Invariants worth knowing before changing code

- No acknowledgement before two live, hash-verified copies in distinct
  failure domains; persisted placement rows alone never authorize a commit.
- Objects are immutable; one file generation has exactly one manifest or one
  deletion marker.
- Nothing is physically deleted except through an applied GC proof.
- Corrupt persistent data fails closed with a diagnosis; only a provably
  interrupted tail write is repaired.
- A deployment is not "durable" or "recovery ready" until its own restore
  drill has passed against its own backup target.

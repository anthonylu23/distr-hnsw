# M1 implementation plan

The canonical milestone scope and acceptance gate remain in
[`roadmap.md`](roadmap.md). This page records the implemented passes and the
next dependency boundary.

## Current status

Passes 1 through 4 (commit spine, deletion and recovery, lifecycle
observation and scrub, and movement/retirement/admission/GC), lane B
(master-key custody), and lane C's first target (backup set v1 with a
versioned directory) are implemented and pushed. The product crate provides
loopback agents with capacity policy plus portal `init`, `put`, `get`,
`delete`, `recover`, `scrub`, `health`, `key`, `backup`, `restore`, `drain`,
`retire`, and `gc` commands. Pass 2 adds deletion-marker v1, paginated
inventories, plan/apply recovery reports, highest-generation selection,
recovery-only RF2 repair, per-file convergence, and fail-closed recovery
issues. The hardening pass integrates the audited commit spine, introduces
canonical SQLite schema v3, and migrates both prior schema-v2 layouts. Pass 3
adds agent incarnations, complete-scan observations, placement verification
states, per-object durability health, and copy-first scrub/repair on schema
v4. Lane B adds the bound key identifier (schema v5) and recovery bundle v1
with its `init` ceremony and `key` commands. Lane C adds backup set v1
(schema v6), the directory target, `VACUUM INTO` snapshot shipping, catalogs,
backup status in `health`, and `restore metadata` / `restore objects`.

M1 was accepted on 2026-09-30 with recorded limitations (`roadmap.md`).
The S3-compatible target, integrity sampling, the power-loss, capacity,
lifecycle-matrix, and restore drills all have evidence documents under
`docs/`. The light local drill passes through the binary. The three phase-1 design questions
were ratified on 2026-09-29; see
[`m1-phase-1-decisions.md`](m1-phase-1-decisions.md).

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

## Pass 3 — lifecycle observation and scrub

1. Give every volume an immutable incarnation identity and report it from
   agent health; record, adopt, supersede, and refuse incarnations in the
   portal (`docs/m1-lifecycle-contract.md`, "Node identity and observation").
2. Persist reconcile jobs and per-namespace scan observations with final
   cursor, count, and inventory digest; mark interrupted jobs on open.
3. Verify every required copy by reading it back; record `confirmed`,
   `corrupt`, or `missing` on placement rows without deleting anything.
4. Derive and persist per-object health; expose it with `portal health`.
5. Repair copy-first under a complete observation only; defer otherwise.
6. Exercise healthy repeatability, corruption, deletion of a copy, a wiped
   volume returning under the same name, a superseded incarnation returning,
   a malformed inventory, an unreachable agent, deleted-file requirements,
   and the CLI end to end.

Pass 3 exits when every required object's durability is observable from
persisted state and a lost or corrupt copy is restored without any physical
deletion. Those gates pass in the light local matrix on btrfs.

## Pass 4 — movement, retirement, admission, and garbage collection

1. Rebuild placements with the `orphaned` state (schema v7) and add GC
   proof records.
2. Implement copy-first drain and floor-proving retirement.
3. Add agent capacity policy (quota, reserve, hard floor), HTTP 507
   refusals with ENOSPC mapping, and two-domain portal admission (exit 3).
4. Implement proof-producing GC planning and proof-revalidating apply with
   agent DELETE; retain deletion markers; defer when any agent is
   unreachable.
5. Exercise drain success and blocking, retirement refusal and lockout,
   capacity refusal with deletes still committing and convergence after
   growth, and every GC gate.

Pass 4 exits when a node can be drained and retired without any object
dropping below RF2, an over-budget cluster refuses new chunks without
touching the floor, and physical deletion happens only through a proof that
re-derives unchanged. Those gates pass in the light local matrix.

## Lane B — master-key custody

1. Derive a non-secret key identifier, bind it at `init`, and refuse a
   mismatching key at every portal open before any decryption.
2. Define recovery bundle v1 (Argon2id-wrapped XChaCha20-Poly1305, armored,
   floor and ceiling on KDF parameters, transcription checksum).
3. Emit the bundle once at `init` after an internal round-trip check; add
   `key show-id`, `key export-recovery`, and `key restore [--verify]`.
4. Zeroize the master key and unwrapped content keys on drop.
5. Exercise round-trip, wrong passphrase, every tampered header and body
   byte, transcription error, truncation, out-of-range parameters, wrong-key
   refusal against a bound database, verify-never-writes, and
   restore-never-overwrites, through the binary.

Lane B exits when a key lost with every cluster disk is recovered from the
bundle and an off-cluster passphrase alone, and the recovered key downloads
a committed file byte for byte. Those gates pass locally; the
empty-infrastructure drill will repeat them from blank infrastructure.

## Lane C — backup set (first target)

1. Define backup-set layout v1 behind a `BackupTarget` trait and implement
   the versioned-directory adapter with never-overwrite semantics.
2. Copy every historical object with read-back verification and persistent
   per-object state, so the job is restartable and idempotent.
3. Ship `VACUUM INTO` snapshots only when the content generation changed;
   write a catalog per job.
4. Expose backup status (last job, verified/pending, lag, last snapshot,
   `recovery_ready = false`) in `portal health`.
5. Implement `restore metadata` and `restore objects`, then prove the full
   sequence in a process test: total loss, key from bundle, metadata from
   snapshot, objects into fresh agents, `recover --apply`, byte-for-byte
   download, deleted file still unreadable, scrub durable.

Lane C also delivered the S3-compatible adapter (`backup/s3.rs`, MinIO test
double via `scripts/minio-test.sh`) and integrity sampling
(`--verify-sample`). Retention and expiry are delegated to the target's
lifecycle and Object Lock configuration.

### Validation

Run on the development machine:

```bash
cargo fmt --all -- --check
cargo clippy -p distr-hnsw --all-targets -- -D warnings
cargo test -p distr-hnsw
```

`.cargo/config.toml` points `TMPDIR` at `target/` so tests exercise the real
filesystem rather than tmpfs. The process-level tests launch two real agent
children, abruptly exit the portal at each crash boundary, and drive `scrub`
and `health` through the binary. Larger storage matrices belong on
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

## After M1

- Deployment documentation must carry the accepted limitations from
  `roadmap.md` and require each deployment to pass `scripts/restore-drill.sh`
  against its own offsite bucket before claiming recovery readiness.
- Power-loss drill extensions (deletion markers, concurrent writers, a
  physical volume) and an APFS qualification remain optional hardening.
- M2 (Tailscale identity) and M3 (single-partition vector engine) are ready
  to start; see `roadmap.md`.

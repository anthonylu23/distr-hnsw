# M1 phase-1 design decisions

Date: 2026-09-29. Status of every decision below: **Proposed (awaiting owner
ratification)**.

`docs/roadmap.md` (M1, "Before the milestone exits") requires three phase-1
questions from `DESIGN.md` §15 to be closed as documented decisions with tested
recovery/failure behavior. This page recommends an answer for each so the
owner can ratify or amend. Once ratified, update `DESIGN.md` §10, §11, §11.1,
and §15 first, then realign `roadmap.md` and the M1 contracts (roadmap rule
7). Nothing here weakens `DESIGN.md` §2.1 or the M1 contracts.

---

## Decision 1 — Master-key custody and independent recovery ceremony

### Decision needed

`DESIGN.md` §10 defers custody ("file with tight permissions in v1,
keychain/passphrase-unlock later") and requires a tested backup and rotation
procedure; §11 requires recovery without any surviving cluster machine. Today
(`crates/distr-hnsw/src/crypto.rs`) `MasterKey` is a raw 32-byte file created
by `portal init`, loaded only with mode `0600`-class permissions, with no
identifier, backup path, zeroization, or rotation seam.

### Recommendation

Keep the file-backed data-encryption key for operation; add a
passphrase-wrapped **recovery bundle** as the independent custody artifact.

1. **Operational custody stays file-backed**, so unattended reboots keep
   working. Add a non-secret
   `key_id = BLAKE3.derive_key("distr-hnsw:master-key-id:v1", key)[..16]`
   persisted in SQLite metadata at `init` and checked at every portal start
   and before any recovery scan. Zeroize `MasterKey` and unwrapped content
   keys on drop.
2. **Recovery bundle v1** wraps the master key under a KEK derived with
   Argon2id from a passphrase kept off-cluster (password manager and/or a
   printed sheet). All fields are authenticated:

   | Field | Value |
   |---|---|
   | magic, version | `distr-hnsw-recovery-bundle`, `u16 = 1` |
   | key_id | 16 bytes; must equal the SQLite value on restore |
   | kdf | `argon2id`, `m_kib`, `t`, `p`, 16-byte salt |
   | nonce, ciphertext | 24-byte XChaCha20-Poly1305 nonce; 32-byte key + 16-byte tag |
   | AAD | magic ‖ version ‖ key_id ‖ kdf parameters |

   Emitted as an armored text block (header, base64 body, checksum trailer)
   so it survives printing; the shape follows age's versioned header and
   passphrase stanza ([age spec](https://github.com/C2SP/C2SP/blob/main/age.md)).
3. **KDF parameters.** Default to the RFC 9106 memory-constrained option,
   `m = 64 MiB, t = 3, p = 4`, 128-bit salt
   ([RFC 9106 §4](https://www.rfc-editor.org/rfc/rfc9106.html)); refuse
   bundles below the OWASP floor `m = 19 MiB, t = 2, p = 1`
   ([OWASP](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)).
   Parameters travel in the bundle and can be raised without a format bump.
   The tool generates the passphrase by default (eight words from a
   7,776-word list, about 103 bits); a user-supplied one needs an explicit
   flag and a minimum length.
4. **Ceremony.** `portal init` prints the bundle once and refuses to finish
   until the operator re-enters the passphrase and the bundle round-trips.
   The bundle is never written to SQLite, the object store, or the backup
   set. Reserved seams: bundle v2 may carry Shamir `k`-of-`n` shares of the
   KEK; rotation re-wraps content keys under a new `key_id` (post-M1).
5. **CLI surface.**

   ```text
   distr-hnsw portal key export-recovery --master-key <path> [--out <file>] [--passphrase-stdin]
   distr-hnsw portal key restore --bundle <file> --master-key <path> [--verify] [--database <db>]
   distr-hnsw portal key show-id --master-key <path>
   ```

   `restore` uses `create_new` semantics and never overwrites a key file;
   `--verify` decrypts and compares without writing.

**Fails closed.** Missing key file, insecure permissions, wrong length or
trailing bytes (existing); loaded key whose `key_id` differs from SQLite (new
`MasterKeyMismatch`, raised before any object is decrypted); wrong passphrase
or tampered bundle (AEAD failure, no partial output); unknown bundle version
or sub-floor KDF parameters; recovery scans still abort without mutation on
undecryptable manifests (`m1-storage-contract.md`).

### Alternatives considered

| Option | Unattended reboot | Independent recovery | Notes |
|---|---|---|---|
| File only (status quo) | Yes | No | Key dies with the portal disk; fails DESIGN §11. |
| **File + passphrase bundle (recommended)** | Yes | Yes | Smallest change; printable artifact; Shamir later. |
| Passphrase unlock at portal start | No | Yes | Strongest at rest, but every reboot blocks on a human; can become an opt-in mode over the same bundle. |
| OS keychain / secret-service | Partial | No | Headless servers often lack an unlocked keyring; still needs a bundle. |
| systemd-creds / TPM sealing | Yes | No | Binds the key to one machine; possible at-rest hardening later. |

### Consequences and tested behavior required

- Unit/property: bundle round-trip; wrong passphrase, any flipped bit, wrong
  version, truncation/trailing bytes, and sub-floor parameters fail; `key_id`
  is stable and reveals nothing about the key.
- Process integration: `init` → `put` → `export-recovery` → delete key file →
  `key restore` → `get` matches the original hash; a wrong bundle stops at
  `MasterKeyMismatch` with no decrypt attempt.
- Drill input: the empty-infrastructure restore (Decision 2) obtains the key
  only from the bundle plus an off-cluster passphrase.
- Roadmap criterion closed: master key recoverable without a surviving
  cluster machine; wrong or missing key material fails closed.
- Code: `crypto.rs` gains `key_id`, `RecoveryBundleV1`, zeroize; `main.rs`
  gains `key` subcommands; the next schema version (v5; v4 is the lifecycle
  pass) adds `meta(key_id)`; crates `argon2`, `zeroize`.

**Status: Proposed (awaiting owner ratification).**

---

## Decision 2 — First versioned backup target and safe RPO/retention defaults

### Decision needed

`DESIGN.md` §11.1 and §14 (item 1) forbid distr-hnsw from holding the only
copy of a file until an offsite job continuously copies committed encrypted
objects into versioned object storage and SQLite history plus key material are
recoverable independently of every cluster machine; the roadmap M1 exit gate
repeats this. §11 names Litestream. Nothing is implemented yet
(`m1-storage-contract.md`, "Implemented boundary").

### Recommendation

Define one **backup-set layout v1** behind a small `BackupTarget` trait with
two adapters: a **versioned directory** adapter (lands first; CI, the
`anthonypc` drill rig, attached or rotated disks) and an **S3-compatible**
adapter as the first supported network offsite target, with Backblaze B2 as the
reference deployment and MinIO as the test double. SFTP/WebDAV are deferred.

- **Layout.** `backup/v1/objects/<kind>/<hash>` (immutable, never
  overwritten), `backup/v1/sqlite/<utc>-<txid>.db` (daily `VACUUM INTO`
  snapshots), `backup/v1/litestream/` (replica), `backup/v1/catalog/<utc>.json`
  (signed inventory with hashes and counts). Hash-addressed chunks need no
  server-side versioning; bucket versioning still protects the catalog and
  SQLite history from accidental overwrite or delete.
- **Immutability.** Enable bucket versioning and, where offered, Object Lock
  in governance mode with default retention equal to the offsite window. AWS
  requires versioning for Object Lock and governance bypass needs
  `s3:BypassGovernanceRetention`
  ([AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html));
  B2 offers governance/compliance/legal hold over the S3 API, on new or
  existing buckets, and lifecycle rules cannot delete locked files
  ([Backblaze](https://www.backblaze.com/docs/cloud-storage-object-lock));
  MinIO requires versioning and allows locking on existing buckets since
  `RELEASE.2025-05-20`
  ([MinIO](https://docs.min.io/enterprise/aistor-object-store/administration/object-locking-and-immutability/)).
  Compliance mode is not the default because it cannot be shortened.
- **SQLite history: Litestream plus periodic snapshots.** Run Litestream
  0.5.x for continuous replication and point-in-time restore. It holds a read
  transaction, takes over checkpointing, and ships LTX files with
  `sync-interval` 1s, snapshots every 24h, and 24h default retention
  ([how it works](https://litestream.io/how-it-works/),
  [config](https://litestream.io/reference/config/)). Set retention
  explicitly: 0.5 allows one replica per database and silently ignores legacy
  per-replica retention keys
  ([migration guide](https://litestream.io/docs/migration/),
  [release post](https://fly.io/blog/litestream-v050-is-here/)).
  Independently, the portal writes a daily in-process `VACUUM INTO` snapshot,
  which is transactional, does not block writers, and is fsynced under
  `synchronous=NORMAL/FULL` ([SQLite](https://www.sqlite.org/lang_vacuum.html)).
  Litestream restores are version-sensitive (LTX is new in 0.5), so the
  snapshot is the format-independent fallback and the drill verifies one
  against the other. The portal must leave checkpointing to Litestream;
  confirm the exact interaction during implementation. `sqlite3_rsync`
  ([SQLite](https://www.sqlite.org/rsync.html)) suits manual portal-loss
  rehearsals only.
- **Defaults** (operator-visible and editable, DESIGN §11.1):

  | Setting | Default | Rationale |
  |---|---|---|
  | Metadata RPO target | 60 s | Litestream sync 1s plus upload; "replicated" never means zero RPO. |
  | Blob RPO target | 15 min | Continuous copy-and-verify; warn > 15 min, critical > 60 min. |
  | Portal-loss RTO target | 10 min once recovery material is at hand | DESIGN §11. |
  | Total-cluster-loss RTO | Measured by drill, no default claim | Published per release (DESIGN §8.5). |
  | Live deletion grace | 30 days | Lifecycle GC proof horizon. |
  | Offsite retention of deleted generations | 90 days after the deletion marker | Never shorter than live grace; equals Object Lock default retention. |
  | Litestream snapshot interval / retention | 24 h / 14 days | Overrides the 24 h default. |
  | `VACUUM INTO` snapshots | daily, keep 30 | Format-independent fallback. |
  | Integrity verification | daily catalog listing; weekly 1% download-and-hash sample | Detects silent loss before a drill. |
  | Restore drill | before every release and at least quarterly | Required for "recovery ready". |

- **Observability.** `distr-hnsw portal status --json` (later
  `GET /v1/health/backup`) reports `BackupStatusV1`: target kind/id, declared
  targets, `backup_lag_seconds` (age of the oldest committed object not yet
  verified offsite), `objects_pending`, `metadata_replication_lag_seconds`
  (a heartbeat row observed in the replica, independent of Litestream
  metrics), `last_backup_completed_at`, `last_integrity_verification_at` and
  result, `last_restore_drill_at` with measured recovery point/time, and
  `recovery_ready` (gate passed, lag within target, drill within window).
  Per-file `offsite_state: pending | copied | verified` is persisted.

**Why the only-copy rule stands.** In-tailnet RF2 protects against a machine
or disk, not a lost portal, bad deploy, ransomware, or fire. Until backup set,
SQLite history, and key bundle restore into empty infrastructure with matching
bytes, durability is undemonstrated and `DESIGN.md` §11.1 forbids claiming it.

### Alternatives considered

| Target | Versioning / immutability | Testability | Verdict |
|---|---|---|---|
| **Versioned directory** (attached disk, mounted share, rclone-able) | Layout-level only | Best; laptop and `anthonypc` | First adapter and drill rig; offsite only on a rotated or remote disk. |
| **S3-compatible** (B2, MinIO, AWS) | Bucket versioning + Object Lock | MinIO on `anthonypc` | First supported network target; B2 reference. |
| SFTP / WebDAV | None native (Litestream supports both as replicas) | Easy | Defer; no immutability story for objects. |
| Litestream only | PITR, second-level RPO | Good | Adopt, paired with `VACUUM INTO` for format independence. |
| `VACUUM INTO` only | RPO = interval | Simple | Hours of RPO; keep as fallback, not primary. |

### Consequences and tested behavior required

- Unit/property: deterministic layout paths; canonical signed catalog;
  adapters refuse to overwrite an existing key.
- Process integration: commit → backup job → `offsite_state = verified`; a
  kill mid-copy never leaves `verified` without a hash-verified offsite
  object; `backup_lag_seconds` rises when the target is unreachable.
- `anthonypc` matrix: directory and MinIO targets with Object Lock; deleting
  a locked object fails; Litestream restore to a declared point and the
  `VACUUM INTO` fallback both reconcile against the object set.
- Empty-infrastructure drill: blank metadata and volumes; inputs are only the
  backup set, SQLite history, and the Decision 1 bundle; bytes match original
  hashes; record declared vs. actual RPO/RTO, hashes, missing control
  metadata, and operator steps.
- Roadmap criteria closed: empty-infrastructure restore; backup lag,
  verification, RPO/RTO, retention, and last drill observable.
- Note: the Rust `object_store` crate is a candidate for both adapters;
  confirm conditional-put support before adopting it.

**Status: Proposed (awaiting owner ratification).**

---

## Decision 3 — Admission control when the whole cluster is over budget

### Decision needed

`DESIGN.md` §15 records a working default ("files-first") and §2.1 requires
reserved headroom with admission control ahead of any hard durability
invariant. `m1-lifecycle-contract.md` ("Capacity and failure behavior")
defines effective free space and rules out lowering the floor or emergency
deletion. This makes that concrete for the M1 blob plane.

### Recommendation

- **Arithmetic.** Per volume,
  `effective_free = max(0, min(quota_bytes - used_bytes, fs_free_bytes) - reserve_bytes)`,
  with `used_bytes` from the last complete inventory plus confirmed placements
  since, `fs_free_bytes` from `statvfs`, and
  `reserve_bytes = max(10% of quota, 1 GiB)` (configurable). Per failure
  domain, `effective_free` is the maximum over its volumes. A write of
  ciphertext size `S` (plaintext plus a 16-byte tag per 4 MiB chunk plus the
  manifest) is admitted only if at least `minimum_write_replicas` (2) distinct
  failure domains each have `effective_free >= S`. "Globally over budget"
  means no such placement exists; a cluster-wide sum is not used because it
  can be positive while no RF2 placement is possible.
- **Reserve consumers.** Regular-file chunks never consume the reserve.
  Deletion markers and manifests of in-flight commits may, so a full cluster
  can still record deletions. Repair and movement may use it down to a hard
  floor (`fs_free < max(1%, 256 MiB)`), below which repair reports itself
  blocked rather than writing.
- **Two-layer enforcement.** The portal pre-checks admission from its
  possibly stale view and chooses placement; each agent mechanically enforces
  its configured quota and reserve on PUT and maps `ENOSPC` to the same
  refusal (portal-supplied configuration, not an agent policy decision). A
  mid-upload refusal fails that placement; the portal tries another eligible
  agent in the same domain, else fails the upload. Staged objects become
  collectible garbage; nothing becomes visible.
- **Error semantics.**
  `PortalError::InsufficientCapacity { required_bytes, replicas_required, domains_eligible, limiting: Quota | Filesystem | Reserve | Enospc }`.
  CLI exit status 3 (distinct from 1 operational failure and 2 blocked
  files). Agent PUT refusal is HTTP 507 Insufficient Storage with
  `{ code: "insufficient_capacity", reason, volume_id }`; the future portal
  API returns 507 with the same body and `Retry-After`. Refusals are
  idempotent: retrying the same key re-evaluates capacity and may succeed
  later without conflict.
- **Health state.** `CapacityReportV1` in `portal status --json`, per volume,
  failure domain, and cluster: `quota_bytes`, `used_bytes`, `fs_free_bytes`,
  `reserve_bytes`, `effective_free_bytes`,
  `capacity_state ∈ { ok, warning, admission_paused, enospc_observed }`
  (warning below 20% of quota; `admission_paused` when a reference 4 MiB
  object cannot be placed at RF2; `enospc_observed` when an agent reported
  ENOSPC in the last 15 minutes), `rf2_eligible_domains`,
  `durability_floor_intact` (always true), `emergency_deletion: never`, and
  `last_refusal_at`. GC continues only through the lifecycle proof; pressure
  never shortens a retention horizon or skips a gate.
- **Extension to vector collections (M4+).** Blob volumes and
  `index.ram_budget`/`index.disk_quota` (DESIGN §8.1) are separate budgets;
  the vector plane competes for blob bytes only via snapshot and WAL archive
  objects. Storage classes carry an `admission_priority` seam ordered control
  objects → repair to floor → regular files → file-collection archives →
  app-collection archives ("files-first"); M1 persists the field but
  exercises one value. The error gains
  `resource ∈ { blob_bytes, index_disk, index_ram }` so M4 adds no new type.

### Alternatives considered

| Option | Behavior when over budget | Verdict |
|---|---|---|
| **Refuse new file writes; keep deletes, repair, and GC-by-proof running** | Predictable, invariant-preserving | Adopt. |
| Cluster-wide byte sum as the signal | Accepts writes that cannot be placed at RF2 | Reject; contradicts the floor. |
| Degrade to RF1 under pressure | Silent durability loss | Already rejected by the roadmap. |
| Emergency GC or early tombstone expiry | Reclaims space by skipping proofs | Reject; violates the lifecycle contract. |
| Portal-only enforcement | Stale usage view overruns disks | Insufficient alone; keep the agent-side limit. |
| Fixed-byte reserve | Wrong on both small and large volumes | Use percentage with a floor. |

### Consequences and tested behavior required

- Unit/property: `effective_free` on quota-, filesystem-, and reserve-bound
  volumes; chunks never breach the reserve; markers and manifests may use it;
  the hard floor blocks repair.
- Process integration (tmpfs or loop device on `anthonypc`): fill a volume;
  `put` exits 3 with `InsufficientCapacity`; `delete` still commits its marker
  at RF2; `recover --apply` repairs into the reserve; inventory digests prove
  nothing was removed; `status` shows `admission_paused`, then `ok`.
- Failure injection: `ENOSPC` between chunk PUTs and before the manifest; the
  file is never visible; the same key converges after space is freed.
- Roadmap criterion closed: ENOSPC and global budget exhaustion produce
  admission-control errors and actionable health without violating the floor.

**Status: Proposed (awaiting owner ratification).**

---

## Implementation order

Parallel lanes beside the lifecycle pass in `m1-implementation-plan.md`; only
the drill serializes on them.

1. **Lane B, key custody (Decision 1).** Smallest change; land first:
   `key_id`, `RecoveryBundleV1`, zeroize, `key` subcommands, schema v4.
2. **Lane C, backup set (Decision 2).** Layout v1 and the directory adapter
   with the copy-and-verify job and `BackupStatusV1`; then the S3-compatible
   adapter against MinIO on `anthonypc`; then Litestream with explicit
   retention plus daily `VACUUM INTO`.
3. **Lane A, lifecycle (existing plan).** Decision 3 is step 4 of the
   lifecycle contract's order (quota/headroom admission, ENOSPC injection),
   after observations and scrub/repair, before GC planning.
4. **Empty-infrastructure restore drill.** Waits for lanes B and C and for
   lane A's reconciliation observations; runs on `anthonypc` with blank
   metadata and volumes and records the roadmap evidence package. Its pass
   closes the M1 exit gate.
5. **Ratification bookkeeping.** On approval, update `DESIGN.md` §10, §11,
   §11.1, §15 and the roadmap M1 checklist; on amendment, revise this page and
   keep its status lines accurate until the tests above are green.

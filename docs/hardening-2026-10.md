# Hardening pass (October 2026)

**Status:** Fixes done and re-verified. Opened 2026-10-25 after a full code
and documentation audit. Every item below marked *required* is done. The
drills and benches were re-run from clean commits on 2026-10-27. M1 stays
**Reopened** and M3 **In progress** until the owner decides on
re-acceptance and on the open thresholds below. M2 starts after this pass.

## Why

A multi-reviewer audit of the code and documentation found 74 issues. An
independent verification pass confirmed 70, rated 4 plausible, and refuted
none. Most high-severity findings were reproduced with probe tests. Seven
were high severity, with six distinct causes (retire and backup share one).
They violate invariants that M1 and M3 claim:

| # | Finding | Plane | Invariant broken |
|---|---|---|---|
| H1 | GC deletes an acknowledged file's chunks after the portal is restored from a snapshot taken mid-upload | blob | GC never removes a required object (lifecycle contract, "Physical deletion proof") |
| H2 | Retirement is refused forever once any GC proof is applied; backup fails on every run after GC | blob | retirement gate; backup convergence |
| H3 | An abandoned delete retried under a new idempotency key mints a second deletion marker for the same generation | blob | one authoritative record per generation |
| H4 | `recover --apply` aborts with a SQLite CHECK violation on that conflict, after applying earlier files | blob | recovery converges or blocks only the affected file |
| H5 | A corrupt WAL length field is read as a torn tail, so acknowledged entries are silently truncated | engine | WAL corruption fails closed (engine contract §4) |
| H6 | Inserts never link through tombstoned nodes; records inserted after mass deletion are unreachable | engine | tombstones stay traversable (engine contract §8) |

Bulk-load snapshot loss and torn-tail repair of non-final segments are
listed among the engine items below.

The audit also found that several evidence claims were stronger than the
repository supports: a ticked M1 observability criterion with
`recovery_ready` hard-coded false, M3 benchmark reports with no usable source
revision, and S3 tests that passed without running.

## Blob plane (`crates/distr-hnsw`)

Regression tests for every item below live in `tests/hardening.rs` unless
noted.

| Item | Change | Status |
|---|---|---|
| H1 GC after a stale restore | GC refuses every candidate while any agent holds a manifest or deletion marker the metadata does not know. `portal restore metadata` sets a *recovery pending* flag; GC, drain, and retire refuse until `recover --apply` clears it. A staging upload whose file the projection already knows is never staging garbage. | Done (required) |
| H2 Collected objects in history | `history_objects` excludes objects with an applied GC proof, which fixes retire, backup, and backup status. GC now also requires every known backup target to hold a verified copy of a deleted generation before collecting it. | Done (required) |
| H3 Two markers per generation | A delete under a new key reuses the pending marker of the same generation. Schema v9 adds a trigger that rejects a second distinct marker. | Done (required) |
| H4 Recovery abort on conflict | Blocking a deleted file keeps its committed marker. Distinct deletion markers at one generation (which earlier releases could produce) resolve deterministically with a note instead of blocking. Re-running recovery on an unchanged blocked file no longer bumps the content generation. | Done (required) |
| Recovered uploads | `recover --apply` marks a matching in-flight upload committed and links it to the file, so same-key retries return the file and GC never sees it as staging. | Done |
| Concurrent jobs | Jobs record a process lease (schema v9). Opening the database interrupts only jobs whose owner has exited, so `portal health` beside `scrub --interval` no longer kills the loop. | Done |
| Backup consistency | The SQLite snapshot is taken before objects are copied and shipped only when every object is verified. Lag is reported when the target refuses writes. | Done |
| Restore hardening | The snapshot name's hash prefix is checked. A snapshot that fails to open is removed, so the restore can be retried. | Done |
| Re-PUT durability | Acknowledging an already-present object now syncs its directory chain. | Done |
| Healing retries | Upload and delete retries re-PUT confirmed placements; agents keep intact copies and replace corrupt ones. | Done |
| Recovery memory | Recovery verifies chunks without retaining their bytes and re-reads a source only for repair. | Done |
| Plan-mode purity | `recover` without `--apply` no longer records agent incarnations. | Done |
| Inventory paging | Pages skip prefix directories before the cursor, so a full listing is no longer quadratic. | Done |
| Init bundle | `portal init` tracks an owed recovery bundle durably, so an init that failed before emitting it emits it on the next run. `key export-recovery --out` never overwrites a file. | Done |
| Master key write | Written to a temporary, fsynced, then hard-linked into place, never overwritten. | Done |
| Leaked temporaries | Agents sweep interrupted-PUT temporaries older than one hour at startup. | Done |
| Oversized files | Refused before any chunk is written when the manifest could not fit one object. | Done |
| Failpoints | `DISTR_HNSW_FAILPOINT` is honored only by debug builds or the `failpoints` feature; release builds refuse to run with it set. | Done |
| Capacity label | The portal pre-check reports `effective_free_space` instead of a hard-coded `quota`. | Done |
| Recovery readiness | `portal drill record --report` stores a `RestoreDrillReportV1`. `recovery_ready` is true only for a passing drill within its declared objectives, recorded within the last 92 days, that covers a configured target whose lag is within the declared blob RPO. | Done (required) |
| S3 tests | Marked `#[ignore]` and run with `--ignored` by `scripts/minio-test.sh`, so `cargo test` no longer counts them as passing. | Done |

### Deferred (recorded, not blocking re-acceptance)

- Delta catalogs: every backup job still writes a full catalog.
- Drain keeps no durable state, so scrub or new writes can re-confirm copies
  on a drained agent before retirement. Retirement re-checks RF2 without the
  agent, so this costs convergence, not safety.
- A placement's `failure_domain` is not refreshed when an agent's configured
  domain changes. Model domain changes as an explicit migration.
- S3 `PutObject` sends no `Content-MD5`. AWS requires an integrity header for
  writes under Object Lock default retention. The SDK's default CRC32
  checksum may satisfy it; verify against the real offsite provider during
  that deployment's restore drill.
- A corrupt orphan control object with no valid replica still aborts
  recovery. This is fail-closed by design: it could be any file's winning
  generation.
- Per-file `offsite_state` and `metadata_replication_lag_seconds` (Decision 2)
  are not implemented. Per-target `objects_pending` and
  `last_snapshot_at` carry the same information.
- Agents are unauthenticated on loopback: any local process can call
  `DELETE`. The portal deletes only through applied proofs; agent-side
  authorization is M2's first requirement.
- `portal delete` does not refuse a file that holds an archived index part
  (`index_archives`). GC collects only files an operator deleted, and the
  archive path re-uploads a part whose file was deleted. Retention of
  archive sets as a unit belongs to M4.
- `init` proves the recovery bundle round-trips with the in-memory secret;
  interactive passphrase re-entry (Decision 1) is open for M6.

## Vector engine (`crates/distr-hnsw-index`)

Engine formats are unchanged (WAL v1, snapshot v1). `partition.json` gained
two optional fields (`ef_search`, `base_snapshot`) that older binaries
ignore. Details are in [m3-engine-contract.md](m3-engine-contract.md).

| Item | Change | Status |
|---|---|---|
| H5 WAL integrity | A damaged frame is a torn tail only in the newest segment and only when no valid frame follows it. `len` must lie in 32..=328,231 bytes. A zero-filled tail or a newest segment with an unwritten header is torn. Any damage in a sealed segment fails closed. Every segment is validated before the newest is truncated. Recovery refuses a WAL that stops short of the snapshot. The writer extends a segment only at the recovered high-water mark and refuses appends after any failed write, sync, or rotation. Partition, `wal/`, and `snap/` directories are created with parent syncs (`fsutil.rs`). Tests flip every bit of every non-final length field. | Done (required) |
| H6 Tombstone traversal | Construction routes through and links to tombstoned nodes (never scores them into results). Delete-all-then-upsert and 99%-tombstoned tests find every insert, including after reopen. | Done (required) |
| Parallel-build race | A node writing its own neighbor list overwrote back-links other threads had added, which could strand a live record after bulk load or compaction (a test failed about half the time on unmodified `HEAD`). Links now merge, and a final pass gives every node an in-link. | Done (required) |
| Bulk-load base | `partition.json` records `base_snapshot` before the load. Opening without a verifiable base fails with `BaseSnapshotUnavailable`. | Done (required) |
| Input validation | Non-finite queries rejected on every metric; `k` ≤ 10,000, `ef` ≤ 100,000; `Hnsw::search_with` panics on a wrong-length query; `PartitionConfig::validate()` at create and open; snapshots cross-checked against `partition.json` on load. | Done |
| Per-metric defaults | One source in code: `HnswParams::for_metric`, `default_ef_search`, and `PartitionConfig::for_metric` (which derives the level seed from the partition id). Per-collection `ef_search` through `search_default`. | Done; cosine `ef_search` raised to 500 on 2026-10-27 |
| Archive and restore | Exclusive directory lock; archive verifies without repairing (`Partition::inspect`) and archives the snapshot last; an unchanged part must still be visible; restore stages in a sibling directory and renames only after a full recovery succeeds there. | Done |
| Concurrent compaction | A short capture step, then a rebuild that owns its input, so writers continue during the rebuild; multithreaded test; the bench writes during the rebuild. | Done (required) |
| RAM accounting | Each snapshot serves the f32 originals from the file just written; resident bytes match before and after reopen. One hot-set formula (contract §11). | Done |
| Bench provenance | `build.rs` embeds the commit with a `-dirty` flag; reports record the executable's BLAKE3 and the command line; `--canonical` refuses dirty or unknown builds; persist and compact recall is recomputed from the dataset rows (`score_mismatches` must be 0). | Done (required) |
| Contract honesty | Idempotency: deduplication is by `op_id` alone within the window. §12: the crash harness is in-process. Filters are slot-space and unstable until stable record ids exist. | Done |

Engine items deferred, each recorded in the contract:
- a request fingerprint per `op_id`;
- stable record ids for filters;
- a subprocess kill harness and a power-loss run for the engine;
- cold-cache recovery measurement;
- compaction RAM admission and a running resident-bytes counter;
- group commit;
- compaction and snapshot scheduling;
- retention of superseded archive parts;
- deterministic parallel build across replicas;
- a narrower public API.

## Evidence and documentation

| Item | Status |
|---|---|
| Re-run `scripts/restore-drill.sh` (directory and MinIO) and `scripts/lifecycle-matrix.sh` from a clean committed revision; add GC-then-retire and GC-after-restore orders to the matrix | Done 2026-10-27 from `dc276f3`. Restore drill passed on both targets (56/56 verified, 0 resurrected, recovery 3.9 s and 3.75 s), and both reports were recorded with `portal drill record` (`recovery_ready` true; false for a target the deployment does not use). The matrix passed, including retire after GC and stale-restore GC (0 collected, acknowledged file intact). `scripts/minio-test.sh` passed 3/3. See [m1-restore-drill.md](m1-restore-drill.md) and [m1-lifecycle-matrix.md](m1-lifecycle-matrix.md). |
| Re-run `scripts/power-loss-drill.sh` (needs sudo) for the changed durable-write paths (re-PUT directory sync, master key write) | Partly done. Re-run from `27e9bf9` on 2026-10-28: 8/8 marks on btrfs, ext4, and XFS, negative control lost, so the fresh-PUT path did not regress ([m1-filesystem-qualification.md](m1-filesystem-qualification.md)). The drill does not exercise either changed path (the master key is written off the drilled volume, and no object is re-PUT), so this item stays open until the drill covers them. Required before M1 re-acceptance. |
| Re-run the M3 benches from a clean revision; publish p95; decide the 1M × 512 build-throughput threshold openly | Re-run done 2026-10-27 with `--canonical` from `dc276f3` (nytimes persistence and compaction from `835f055`). p95 is published next to p50 ([bench/README.md](bench/README.md)). Decided 2026-10-27: the cosine default ef is 500, and the build-throughput threshold is ≥ 560 vectors/s required, ≥ 3.3k targeted ([m3-implementation-plan.md](m3-implementation-plan.md)). |
| Drill reports record `git describe --dirty` (restore, lifecycle, ENOSPC, power loss) | Done |
| Roadmap, next steps, AGENTS, contracts, and DESIGN (v2.2) brought in line with the code | Done |
| Architecture overview and deployment guide | Done ([architecture.md](architecture.md), [deployment.md](deployment.md)) |
| Root README, license texts, security policy, repository metadata | Done (copyright line "The distr-hnsw authors", kept on 2026-10-27) |
| Persistence and compaction benches on nytimes | Fixed in `835f055`. Both benches re-upserted and queried the first 1,000 rows, but nytimes holds 239 zero-norm base rows and 9 zero-norm queries, which the public cosine API rejects (contract §2), so the WAL tail failed and compaction panicked. They now use the first admissible rows and queries and record `queries_evaluated`. These runs had never covered nytimes before. |
| CI running fmt, clippy, and the non-sudo tests | Open |

## Open-source hygiene (owner decisions)

- `prototype/testdata/anthonypc-vault-queries.json` listed personal
  knowledge-base filenames and queries. It left the tree on 2026-10-27. Its
  history was not rewritten: the repository is already public, the file
  holds note titles rather than note contents, and a rewrite would not
  withdraw copies already fetched.
- Host and user names (`anthonypc`, `anthonylu@…`, home paths) appear in
  tracked docs and scripts. Replace them with roles and environment variables
  before publishing.
- The public name is kept for now (decided 2026-10-25). The BLAKE3 key-id
  context `distr-hnsw:master-key-id:v1` embeds it; changing the context
  after release breaks key-id binding.

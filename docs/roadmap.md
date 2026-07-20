# Milestones and roadmap

This document turns the build order in [DESIGN.md](../DESIGN.md) into gated,
verifiable milestones. It describes what must be true before work advances; it
is not a promise of calendar dates. The design remains authoritative for system
behavior and invariants. If this roadmap and the design disagree, update the
design first and then bring this document back into alignment.

## Current position

| Milestone | State | Exit gate |
|---|---|---|
| M0 — Semantic validation | **Accepted** | Larger representative bake-off produces a documented go decision and locks the default local model and dimensions |
| M1 — Blob plane and recovery foundation | Not started | Blob durability and an empty-infrastructure restore drill pass |
| M2 — Tailscale identity and authorization | Not started | Network identity, sessions, grants, and API-key boundaries pass adversarial tests |
| M3 — Single-partition vector engine | Not started | Persistence, recovery, recall, filtering, and compaction gates pass against brute force |
| M4 — Distributed vector plane | Not started | Quorum, fencing, promotion, movement, and balancing survive failure injection |
| M5 — File extraction and semantic retrieval | Not started | Files flow safely from extraction through reproducible hybrid search |
| M6 — Dashboard and operations | Not started | User and operator workflows are complete, truthful, and accessible |
| M7 — Vault, sharing, and v1 release | Not started | Security, deployment, recovery, and operating-envelope qualification pass |

The phase-0 implementation is intentionally disposable. A frozen independent
40-query holdout (`anthonypc` policy run
`20260720T032226Z-holdout-policy`) validates the semantic product bet: 512d
scores 32/0/8 versus name, and an unchanged-input repeat reproduces rankings
and retrieval metrics exactly with complete provenance. The documented
non-inferiority tie-break locks `nomic-embed-text @ 512` with a 0.001 nDCG gap
to the best eligible dimension. M0 is accepted and M1 is unblocked. See
[phase-0-validation.md](phase-0-validation.md) and
[next-steps.md](next-steps.md).

## How milestones are governed

### Status vocabulary

- **Not started**: no milestone implementation is relied upon.
- **In progress**: implementation or validation is active, but the exit gate
  has not passed.
- **Blocked**: a required predecessor or explicit decision is missing.
- **Accepted**: every acceptance criterion has evidence and the milestone exit
  review found no unresolved release-blocking defect.
- **Reopened**: later evidence invalidated an accepted criterion.

Only evidence changes milestone state. A feature being demonstrable is not the
same as its milestone being accepted.

### Rules that apply to every milestone

1. Preserve the system invariants in DESIGN §2.1. Any intentional change is a
   design change and must be documented before implementation.
2. Version persistent formats and network contracts. Document compatibility,
   migration, and downgrade behavior before those formats hold unique data.
3. Add focused unit, property, integration, and failure-injection tests with
   the implementation. A happy-path demo is never sufficient for a durability
   or epoch boundary.
4. Make retries idempotent and make partial work observable. Recovery must
   converge without an operator editing SQLite or storage directories.
5. Record benchmark and drill inputs, software versions, hashes, hardware, and
   outcomes so results can be reproduced.
6. Run development and small fixture tests on the laptop. Run large-corpus,
   large-artifact, GPU, sustained-load, and destructive storage-flow tests on
   `anthonypc` or purpose-built disposable infrastructure.
7. Update this roadmap, the relevant `docs/` pages, and operator documentation
   as behavior changes. Do not let a green test encode a contract that the
   documentation does not explain.

### Verification layers

Each milestone uses the smallest applicable layers below, escalating when the
claim crosses a machine or durability boundary.

| Layer | Purpose | Typical environment |
|---|---|---|
| Static | Formatting, linting, dependency and unsafe-code review | Laptop and CI |
| Unit/property | Formats, state machines, placement, crypto envelopes, index invariants | Laptop and CI |
| Process integration | Real roles on temporary volumes; restart and crash boundaries | Laptop for small fixtures; `anthonypc` for larger artifacts |
| Multi-node fault | Loss, partition, stale return, ENOSPC, bit flips, movement under load | Disposable processes/VMs across distinct failure domains |
| Performance/quality | Retrieval recall, latency, memory, throughput, recovery time | Pinned datasets and recorded hardware |
| Disaster recovery | Independently restore keys, metadata, blobs, and vector state into empty infrastructure | Isolated restore environment |
| Manual product/security | Browser flows, accessibility, threat-boundary checks, operator runbooks | Supported browsers and a real tailnet |

For an accepted milestone, its evidence package must identify the exact test or
drill, source revision, environment, result, and any accepted limitation.

---

## M0 — Validate the semantic product bet

**Purpose.** Prove on representative data that meaning-based retrieval adds
enough value over filename and date retrieval to justify the distributed
system. Select the default local embedding model and dimensionality before
their formats and capacity costs become embedded in later milestones.

**Dependencies.** None.

**Estimated effort.** Days for tooling, plus corpus/query curation and review.

### Scope and deliverables

- Keep `prototype/` as a disposable validation CLI; do not evolve it into the
  product service.
- Build a representative mixed corpus of at least hundreds of files, including
  PDFs, notes, and code.
- Curate at least 40 meaning-oriented labeled queries. Minimize queries that
  merely repeat filenames, and record relevance judgments.
- Compare semantic retrieval with name, recency, and keyword baselines using
  reproducible metrics and qualitative review.
- Measure provider cold start separately from warmed p50/p95 query latency.
- Persist embedding-provider identity and model digest with indexed vectors;
  refuse evaluation if the query model no longer matches.
- Publish a sanitized aggregate report with corpus/query/source/executable
  provenance while keeping private filenames and full reports untracked.
- Record the selected model and dimensions in DESIGN §15, or explicitly record
  a no-go and the next hypothesis to test.

### Acceptance criteria

- [x] The evaluated corpus has at least hundreds of mixed-format files and the
  judged set contains at least 40 meaning-oriented queries.
- [x] Semantic search wins at least approximately 60% of decided comparisons
  against the name baseline, or a documented human review establishes clear
  qualitative dominance on meaning queries.
  *(independent holdout at 512d: 100% vs name, 32/0/8)*
- [x] Recall/nDCG, baseline comparisons, cold start, and warmed p50/p95 latency
  are reported for every candidate configuration.
- [x] Re-running evaluation against unchanged inputs reproduces retrieval
  metrics; latency variance is reported rather than treated as determinism.
- [x] The stored and current provider/model identities match for every query.
- [x] Dimensionality is not locked with fewer than 40 judged queries, one
  eligible dimension, or a product no-go; the documented `nomic-embed-text`
  tie-break locks 512d only when it is within 0.03 nDCG of the best eligible
  dimension.
  *(40-query holdout: 512d is 0.001 below the best eligible nDCG)*
- [x] The go/no-go decision and its limitations are reviewed and reflected in
  the design and phase documentation.
  *(semantic go; public-fragment and keyword limitations documented)*

### Verification and evidence

Run the fixture test locally, then execute the full bake-off on `anthonypc`
with a fresh run-scoped work directory via
`prototype/scripts/run-bakeoff-anthonypc.sh`. Verify the sanitized report
contains hashes for the query set, source tree, executable, and model identity.
Repeat evaluation without rebuilding the corpus and compare metrics. Retain the
sanitized aggregate under `docs/`; keep private full reports in the ignored run
directory. Because mixed-v4b was corrected after inspecting the prior losses,
freeze and evaluate an independent holdout of at least 40 meaning-oriented
queries before the final decision; do not revise it based on retrieval output.

**Exit gate.** **Passed.** The independent holdout and exact repeat validate
`nomic-embed-text` and the semantic product bet. The documented
non-inferiority tie-break locks 512d in DESIGN §15 without treating the 0.001
gap to 384d as evidence of statistical superiority. M1 is unblocked.

**Explicit non-goals.** Distribution, HNSW, WALs, Tailscale auth, replication,
the dashboard, and production extraction. Holdout keyword results preserve
hybrid fusion as a phase-5 product requirement, not an M0 implementation task.

---

## M1 — Build the blob plane and recovery foundation

**Purpose.** Establish the trustworthy byte-storage substrate used by regular
files, vector snapshots, WAL archives, and source documents. Recovery is part
of the write design, not a later operational add-on.

**Dependencies.** M0 accepted.

**Estimated effort.** Roughly 2–3 months of solo implementation.

### Scope and deliverables

Before service code, document and test the versioned object formats, SQLite
state transitions, platform-specific durable-write contract, recovery
precedence rules, and crash-point matrix. The first vertical slice is one
regular-file class, 4 MiB chunks, RF2 across two local agents, a file-backed
master key, and no dashboard or user auth.

Implement the milestone in this recovery-first sequence:

1. **Durable opaque-object agent.** Implement PUT, GET, DELETE, inventory,
   scrub, and health over configured volumes. A successful PUT requires a
   temporary write, file sync, atomic rename, and parent-directory sync.
   Agents do not make placement or policy decisions.
2. **Portal commit state machine.** Persist an idempotent staging/encryption
   plan before dispatch; encrypt 4 MiB chunks with XChaCha20-Poly1305; address
   ciphertext using BLAKE3; track pending/confirmed/orphaned placements; and
   acknowledge only after the configured durability floor is confirmed across
   distinct failure domains.
3. **Immutable recovery objects.** Replicate versioned encrypted manifests
   before marking a file committed. Replicate deletion markers before the
   SQLite tombstone. Resolve recovery using the highest generation so stale
   metadata cannot resurrect deleted files.
4. **Reconciliation and lifecycle.** Add inventory comparison, scrub,
   corruption repair, safe movement, staging cleanup, retention-aware garbage
   collection, volume quotas, node retirement, and policy-degraded health.
5. **Independent recovery.** Back up encrypted objects to versioned offsite
   storage; replicate SQLite history; protect master-key recovery material
   independently of cluster nodes; automate portal loss and total-cluster
   restore.

Before the milestone exits, close the phase-1 design questions for production
master-key custody and recovery, the first supported versioned backup target
and safe RPO/retention defaults, and admission behavior when the entire cluster
is over budget. The file-backed key and files-first admission policy are
working defaults, not unreviewed permanent contracts.

### Acceptance criteria

#### Durable storage and commit

- [ ] Acknowledged object writes survive immediate process and host restart on
  each supported storage platform.
- [ ] Identical retries converge on the same ciphertext objects and one file
  record; conflicting reuse of an idempotency key is rejected.
- [ ] No staging or partially replicated file is visible or downloadable.
- [ ] A committed file has a durable replicated manifest and every referenced
  chunk meets `minimum_write_replicas` across distinct failure domains.
- [ ] RF2 stops accepting new writes when both required durable copies cannot
  be established; the system never silently falls back to one copy.
- [ ] Policy shortfalls remain visible as degraded even when the durability
  floor permits acknowledgement.

#### Delete, repair, and capacity safety

- [ ] Delete makes a file unreadable only after its deletion marker is durable;
  a stale SQLite restore or returning stale node cannot resurrect it.
- [ ] Repair, rebalance, quota changes, and garbage collection never remove an
  old copy before a replacement is durable and confirmed.
- [ ] Bit flips are detected by ciphertext hash verification and repaired from
  a valid replica without serving corrupt plaintext.
- [ ] Staging garbage and unreferenced chunks are collected only after the
  documented grace, reference, observation, and node-retirement gates.
- [ ] ENOSPC and global budget exhaustion produce admission-control errors and
  actionable health state without violating the replica floor.

#### Recovery

- [ ] A portal crash at every boundary in the commit and delete protocols
  recovers to either one complete committed state or harmless collectible
  garbage—never a visible partial file.
- [ ] Core file records newer than an older SQLite restore point can be
  reconstructed from manifests, deletion markers, and inventories.
- [ ] The master key can be recovered without relying on any surviving cluster
  machine, and wrong or missing key material fails closed.
- [ ] A representative cluster restores into empty infrastructure from the
  versioned offsite objects, SQLite history, and independent key material;
  regular-file bytes match their original hashes.
- [ ] Backup lag, last integrity verification, RPO/RTO targets, retention, and
  last restore-drill result are externally observable.
- [ ] Master-key custody, backup defaults, and globally-over-budget admission
  behavior are documented decisions with tested recovery/failure behavior.

### Verification and evidence

- Unit/property tests cover canonical serialization, envelope versions,
  authenticated metadata, generation ordering, placement selection, and every
  legal/illegal state transition.
- A process harness kills the portal or an agent between every durable boundary,
  truncates writes, retries requests, flips stored bits, fills volumes, and
  returns retired/stale nodes.
- Multi-agent tests use distinct temporary volumes and failure domains. Larger
  artifact matrices run on `anthonypc`, never against irreplaceable data.
- The restore drill starts with blank metadata and object directories. It
  records declared RPO/RTO, actual recovery point/time, source hashes, restored
  hashes, missing control metadata, and operator steps.
- Review the durable-write implementation on every supported filesystem; do
  not infer parent-directory durability from a passing process-restart test.

**Exit gate.** All acceptance criteria pass, including the empty-infrastructure
restore. Until then, distr-hnsw must not hold the only copy of any file and must
not describe a deployment as durable or recovery ready.

**Explicit non-goals.** Dashboard UX, browser vault, semantic search, vector
indexes, public sharing, and portal high availability.

---

## M2 — Add Tailscale identity and authorization

**Purpose.** Turn the trusted storage core into a tailnet-native service with a
defense-in-depth identity and authorization boundary suitable for later browser
and application clients.

**Dependencies.** M1 accepted. Auth work may be prototyped earlier, but it must
not distract from the M1 recovery gate.

**Estimated effort.** Roughly 2–4 weeks.

### Scope and deliverables

- Discover the host tailnet IP through Tailscale LocalAPI and bind only to it.
- Resolve peer identity using LocalAPI `WhoIs(remoteAddr)` on the actual socket;
  do not trust forwarded identity headers.
- Obtain, renew, and hot-reload HTTPS certificates for the portal.
- Add stable internal users/identities, secure browser sessions, CSRF
  protection, and audit logging.
- Add hashed application API keys with collection-scoped rights.
- Add short-lived portal-signed internal capability grants bound to operation,
  object/partition, placement epoch, and expiry.
- Add single-use join tokens, signing-key rotation, revocation, and documented
  tailnet ACL policy.

### Acceptance criteria

- [ ] Portal and internal roles refuse to bind to wildcard or non-tailnet
  addresses, including after `tailscaled` restart or address change.
- [ ] A forged identity header from a local or tailnet process grants no access;
  only socket-derived WhoIs identity is accepted.
- [ ] Certificate issuance, renewal, reload, and failure behavior work without
  exposing plaintext browser traffic.
- [ ] Session cookies are secure, HTTP-only, same-site, rotated as documented,
  and state-changing routes reject CSRF attempts.
- [ ] API keys are stored only as non-reversible verifiers and enforce collection
  and action scopes.
- [ ] Internal roles reject missing, expired, altered, wrong-operation,
  wrong-object, and stale-epoch grants.
- [ ] Join tokens cannot be reused, and signing-key rotation has a tested overlap
  and revocation procedure.
- [ ] ACL and certificate-transparency implications are explained in deployment
  documentation.

### Verification and evidence

Use a real test tailnet for WhoIs, address-change, certificate, and ACL tests.
Add request-level authorization matrices for every route and property tests for
grant parsing/expiry. Run adversarial tests from an unauthorized tailnet node
and a local process capable of forging headers. Record key-rotation and
node-join drills in the milestone evidence.

**Exit gate.** Every external and internal operation has an authenticated,
authorized principal; spoofing, replay, stale grants, and key rotation pass
their negative-path tests.

**Explicit non-goals.** OIDC, passkeys, public-internet ingress, multi-user
product UX, and non-Tailscale mTLS mode.

---

## M3 — Build a correct single-partition vector engine

**Purpose.** Prove the local ANN engine's correctness, persistence, recovery,
quality, and resource model before adding replication and distributed routing.

**Dependencies.** M1 accepted. M2 is not required for isolated engine work.

**Estimated effort.** Roughly 1–2 months.

### Scope and deliverables

- Implement the in-repository HNSW build, insert/upsert, delete/tombstone,
  query, persistence, and compaction paths.
- Define versioned binary snapshot and checksummed WAL formats with high-water
  marks and idempotency IDs.
- Implement int8 scalar quantization with exact rescoring of candidates.
- Implement filtered search using masked traversal plus a measured brute-force
  cutover for selective filters.
- Add memory accounting and admission control using the published capacity
  formula; reserve recovery and compaction headroom.
- Archive snapshots and WAL segments through the M1 blob plane and restore a
  partition from snapshot plus WAL tail.

### Acceptance criteria

- [ ] Exact and ANN query results obey the configured distance metric,
  deterministic tie rules, upsert semantics, and tombstones.
- [ ] Unfiltered and filtered recall meet documented thresholds against brute
  force on fixed public datasets and representative project data.
- [ ] Int8 retrieval plus exact rescoring meets its documented recall/latency
  budget and never returns a score computed from the wrong vector/version.
- [ ] Acknowledged WAL entries survive process/host restart and replay exactly
  once in sequence.
- [ ] Snapshot plus WAL-tail restore reproduces logical records, tombstones,
  idempotency state, and the committed high-water mark.
- [ ] Truncated, reordered, or checksum-invalid WAL/snapshot data fails closed
  with a recoverable diagnosis.
- [ ] Compaction under concurrent reads/writes neither loses acknowledged
  operations nor makes deleted records visible.
- [ ] Hard RAM/disk limits reject or defer work before recovery/compaction
  headroom would be consumed.

### Verification and evidence

Build model-based/property tests around a brute-force reference store. Crash at
each WAL append/sync/apply, snapshot publication, and compaction swap boundary.
Benchmark recall, p50/p95/p99 latency, build time, memory, disk amplification,
snapshot bytes, and recovery time on pinned datasets and hardware. Include
filter selectivities that exercise both masked traversal and brute-force
cutover. Run large benchmarks on `anthonypc`.

**Exit gate.** The engine meets published correctness, recall, resource, and
recovery thresholds on a single partition; all persistent-format corruption
tests fail safely.

**Explicit non-goals.** Cross-node replication, leader election, semantic
routing, multiple query coordinators, and file extraction.

---

## M4 — Distribute and replicate the vector plane

**Purpose.** Add partition placement, synchronous WAL replication, fencing,
promotion, movement, and capacity-aware balancing without allowing two
acknowledged primary histories.

**Dependencies.** M1 and M3 accepted; M2 capability grants are required before
a tailnet deployment is accepted.

**Estimated effort.** Roughly 2–3 months.

### Scope and deliverables

- Map record IDs into fixed logical hash ranges owned by partitions; split a
  range without recomputing `hash % partition_count`.
- Maintain primary/replica placement and a monotonically increasing epoch in
  the portal routing table.
- Replicate ordered WAL entries synchronously to the configured quorum and
  preserve idempotency state across promotion.
- Fence old primaries, promote only candidates that contain the portal's last
  quorum-committed high-water mark, and reject writes during unsafe ambiguity.
- Scatter queries across relevant partitions and merge globally correct top-k
  results with freshness/high-water requirements.
- Implement safe partition copy, catch-up, epoch cutover, old-copy retirement,
  join, split, and capacity-driven balancing.
- Publish the measured v1 operating envelope and apply admission control before
  portal, memory, disk, recovery, or compaction budgets are exceeded.

### Acceptance criteria

- [ ] An acknowledged primary write exists on the configured durable quorum and
  remains recoverable after any allowed single-node failure.
- [ ] RF2 becomes unavailable for writes when either copy cannot durably
  acknowledge; RF3 majority behavior matches its configured contract.
- [ ] No test history contains two epochs that both acknowledge committed writes
  for the same partition.
- [ ] A stale primary returning after partition or promotion rejects work and
  cannot overwrite newer state.
- [ ] Promotion never selects a replica below the last recorded committed
  high-water mark; ambiguous state fails closed.
- [ ] Scatter-gather returns the correct global top-k and honors read-your-writes
  high-water requirements.
- [ ] Move and split operations preserve service or fail safely while concurrent
  writes, queries, snapshots, and node loss occur.
- [ ] Rebalancing never deletes an old partition copy before its replacement is
  durable, caught up, and committed in the new epoch.
- [ ] Budget reduction, machine addition/removal, ENOSPC, and hot partitions
  converge or produce explicit blocked/degraded state.
- [ ] A primary raw-vector collection restores from blob-plane snapshot and WAL
  archive with no acknowledged data loss beyond the declared RPO contract.

### Verification and evidence

Use deterministic model tests for routing, quorum, placement, and epoch state
machines, then a real multi-process fault harness. Kill or partition a primary
before/after every WAL quorum boundary; delay/reorder messages; return a stale
primary; corrupt one replica; move and split under load; fill a machine; and
restart the portal from older metadata. Compare final logical state with a
linearizable reference history. Benchmark fan-out latency, portal saturation,
movement/recovery time, and snapshot bandwidth at the proposed v1 envelope.

**Exit gate.** The failure matrix produces no acknowledged split-brain history,
lost committed raw-vector write, or unsafe old-copy deletion, and the measured
operating envelope is documented.

**Explicit non-goals.** Consensus-based portal HA, centroid/hierarchical query
routing, direct client-to-node reads, and beyond-RAM/DiskANN-style indexes.

---

## M5 — Add extraction, embeddings, and file semantics

**Purpose.** Connect committed file bytes to a safe, reproducible indexing
pipeline and expose the product's hybrid semantic file behaviors.

**Dependencies.** M0, M1, M3, and M4 accepted. M2 is required for exposed APIs.

**Estimated effort.** Roughly 1–2 months.

### Scope and deliverables

- Add a versioned extractor trait and out-of-process workers with time, memory,
  output-size, and file-size limits.
- Support the agreed v1 text formats; classify empty, unsupported, broken, and
  OCR-required inputs without blocking byte durability.
- Add local and OpenAI-compatible embedding-provider implementations with
  persisted model/dimension/provider identity and controlled retry/backoff.
- Record chunking, extractor, and embedding provenance so re-indexing is
  deliberate and reproducible.
- Build `files-text`, SQLite FTS5 name/body indexes, and hybrid RRF search.
- Validate the v1 chunk-to-file score aggregation rule on representative data;
  start with max similarity and document evidence before choosing otherwise.
- Add file ingest pipeline status, retry/rebuild controls, similar files,
  filters/facets, tags, smart collections, and manual collections.
- Preserve full file access by ID/name/tag/recency while semantic indexing is
  delayed, unavailable, or rebuilding.

### Acceptance criteria

- [ ] A committed file remains downloadable regardless of extraction,
  embedding, FTS, or ANN failure.
- [ ] Malformed or adversarial documents cannot crash the portal, escape the
  worker, exceed configured resource caps, or poison another job.
- [ ] Extraction and embedding retries are idempotent and expose terminal versus
  retryable states.
- [ ] Every indexed vector can be traced to file generation, source chunk,
  extractor/version, chunker/version, model identity/digest, dimensions, and
  embedding configuration.
- [ ] A changed file generation, extraction version, or model configuration
  cannot silently mix incompatible vectors in one collection.
- [ ] Hybrid search deterministically fuses keyword and semantic candidates and
  respects owner/tag/type/date/manual-collection filters.
- [ ] Chunk-level hits aggregate to file-level results using a versioned,
  documented rule whose quality has been measured against alternatives.
- [ ] Deletes and new generations stop stale chunks from appearing in search,
  similar-file, or smart-collection results.
- [ ] Local-provider outage, API throttling, and partial batch failure drain
  through bounded retry queues without losing committed files.
- [ ] Quality evaluation shows the integrated product preserves the M0 semantic
  decision on a representative corpus; regressions have an explicit threshold.

### Verification and evidence

Maintain a fixture corpus spanning supported, unsupported, malformed, huge,
encrypted, empty, and scanned documents. Run workers under enforced limits and
inject crashes/timeouts between pipeline transitions. Compare hybrid/filter
results with deterministic expected rankings and audit provenance round trips.
Run representative corpus quality and GPU/provider load tests on `anthonypc`,
including provider restart and digest-change scenarios.

**Exit gate.** End-to-end file ingest, degraded operation, re-indexing, delete,
hybrid search, and provenance tests pass without coupling byte durability to
derived-index availability.

**Explicit non-goals.** OCR, image/multimodal embeddings, learned score fusion,
and post-v1 chunk-to-file aggregation experiments beyond the documented v1
rule.

---

## M6 — Build the dashboard and operator experience

**Purpose.** Make semantic storage useful to an operator and make health,
degradation, capacity, backup, and recovery state understandable without direct
database or filesystem inspection.

**Dependencies.** M1–M5 accepted for their respective production surfaces.

**Estimated effort.** Roughly 1–2 months.

### Scope and deliverables

- Build the embedded React/TypeScript dashboard and browser session flow.
- Add search-first browsing, file detail/download, similar-file neighborhoods,
  tags/facets, smart/manual collections, upload progress, and pipeline status.
- Add cluster views for nodes, volumes, budgets, blob replicas, partitions,
  epochs, degraded policies, queues, backup lag, alerts, and restore readiness.
- Add safe operator actions for retry, repair, rebalance, drain/retire, key
  rotation, and restore drills with previews and confirmation boundaries.
- Expose the same health semantics through the JSON API and Prometheus metrics;
  the dashboard must not invent a second source of truth.
- Document accessibility, supported browsers, responsive behavior, and common
  incident workflows.

### Acceptance criteria

- [ ] Core upload, search, filter, inspect, download, tag, collection, and delete
  flows work with keyboard navigation and supported screen sizes/browsers.
- [ ] Progress and retry behavior survive refresh, reconnect, duplicate submit,
  and long-running operations without creating conflicting work.
- [ ] The UI distinguishes healthy, durability-floor satisfied but
  policy-degraded, unavailable, rebuilding, and recovery-not-ready states.
- [ ] No screen calls in-tailnet replication a backup or labels the deployment
  durable/recovery ready before the M1/M7 gates pass.
- [ ] Destructive and capacity-changing actions show affected objects,
  invariants, and rollback/irreversibility before confirmation.
- [ ] UI, API, and metrics report consistent high-water marks, queue depth,
  replica state, backup lag, and alert status.
- [ ] Unauthorized users/keys cannot discover metadata through pages, APIs,
  autocomplete, error messages, or background requests.

### Verification and evidence

Add component tests for state rendering and browser tests for complete user and
operator stories. Drive the UI against injected degraded states, slow uploads,
provider outages, lost nodes, stale epochs, backup lag, and restore-in-progress.
Run accessibility checks plus manual keyboard/screen-reader review. Reconcile
sample dashboard values against API, metrics, and direct test-harness state.

**Exit gate.** The primary product and operations stories pass end to end, and
the dashboard communicates durability and recovery truth without requiring
tribal knowledge.

**Explicit non-goals.** Native sync clients, multi-user administration, mobile
apps, and public-internet access.

---

## M7 — Add vault, sharing, deployment, and v1 qualification

**Purpose.** Complete the security-sensitive product surfaces and prove a
third party can deploy, operate, upgrade, recover, and understand the v1 system
within a published operating envelope.

**Dependencies.** M1–M6 accepted.

**Estimated effort.** Roughly 1 month, plus the observation time required by
release qualification and restore drills.

### Scope and deliverables

- Add the vault flow using a pinned, audited browser/WASM implementation of
  Argon2id and XChaCha20-Poly1305 with bounded streaming and benchmarked KDF
  parameters. The server never receives the vault passphrase or plaintext.
- Add expiring, revocable, tailnet-only share links for regular files; keep
  vault sharing deferred.
- Add `distr-hnsw init`/`join`, role configuration, systemd units, upgrades,
  rollbacks where compatible, node drain/retire, signing-key rotation, and
  backup/restore workflows.
- Publish installation, architecture, threat model, capacity planning,
  observability, incident response, format compatibility, and recovery docs.
- Resolve the public project name before publication and update binaries,
  packages, examples, and documentation consistently if it changes.
- Measure and publish the v1 envelope: vectors/partitions, write throughput,
  p50/p95 query latency and recall at documented filter selectivities,
  snapshot bytes/day, portal CPU/network, largest-partition recovery/movement,
  and full-cluster restore time.
- Run release qualification and an empty-infrastructure recovery drill using
  the same artifacts intended for release.

### Acceptance criteria

#### Vault and sharing

- [ ] Vault passphrase and plaintext are absent from portal requests, logs,
  metrics, crash reports, SQLite, and agent/index storage.
- [ ] Chunked vault encryption/decryption is authenticated, streaming, bounded,
  versioned, and interoperable across every supported browser.
- [ ] Wrong passphrase, altered ciphertext/AAD, reordered chunks, and truncated
  uploads fail closed without exposing partial plaintext.
- [ ] The UI and threat-model documentation clearly state that a malicious
  portal can serve passphrase-stealing JavaScript.
- [ ] Share tokens are unguessable, scoped to one regular file and permission,
  expire, revoke immediately by generation, and reveal no vault content.

#### Deployment and lifecycle

- [ ] A new operator can initialize a portal, join roles, apply documented ACLs,
  configure budgets/classes/backups, and reach healthy state using only the
  release artifacts and documentation.
- [ ] The public name, package/binary names, licensing, and repository metadata
  are consistent and ready for publication.
- [ ] A supported rolling upgrade preserves committed bytes, acknowledged
  vector writes, epochs, and format compatibility; incompatible downgrade is
  rejected with a recovery path.
- [ ] Backup, signing-key rotation, master-key recovery, node retirement,
  re-indexing, and restore runbooks have each been rehearsed.
- [ ] Observability alerts before durability, capacity, backup RPO, or recovery
  headroom contracts are breached.

#### Release qualification

- [ ] The published v1 operating envelope is backed by reproducible load and
  failure tests, and admission control protects its hard limits.
- [ ] An isolated empty-infrastructure drill restores regular files byte-for-
  byte, rebuilds a file collection from source, and restores a primary raw-
  vector collection from snapshot plus WAL.
- [ ] Actual metadata/blob RPO and portal/cluster RTO are within the configured
  targets, or the release remains blocked with the miss documented.
- [ ] Security review covers identity, authorization, key custody, browser
  crypto, dependency provenance, secrets in logs, and the documented threat
  boundary.
- [ ] All milestone evidence is linked from a release checklist; no critical
  defect, unresolved data-loss path, or undocumented invariant remains.

### Verification and evidence

Use supported-browser interoperability and negative crypto fixtures, network
capture/log scanning, token replay/revocation tests, clean-machine deployment,
rolling upgrade/rollback tests, soak/load testing at and beyond the proposed
envelope, and the full disaster-recovery drill. A person who did not implement
the feature should execute the public setup and restore documentation and file
all ambiguities as release blockers or documented corrections.

**Exit gate.** A tagged v1 candidate passes the security, operating-envelope,
fresh-install, upgrade, and empty-infrastructure restore checklists. Only then
may the configured deployment claim it can safely hold the only copy of a file.

**Explicit non-goals.** Portal HA, public sharing, multi-user deployments,
OIDC/passkeys, non-Tailscale transport, hierarchical semantic routing,
beyond-RAM indexes, OCR, and multimodal collections.

---

## Post-v1 horizon

Post-v1 work is intentionally not on the critical path to the v1 gate. Each
item requires its own design update, acceptance criteria, and migration plan:

- centroid/hierarchical routing and multiple query coordinators;
- direct blob reads using the reserved capability path;
- consensus-backed portal HA;
- OIDC/passkeys and multi-user authorization/product UX;
- deliberate public ingress and abuse controls for sharing;
- non-Tailscale mTLS deployments;
- DiskANN-style beyond-RAM partitions;
- OCR and image/CLIP multimodal collections;
- native sync or path/WebDAV compatibility views.

## Milestone evidence checklist

Use this checklist at every exit review:

- [ ] Scope and design references are current.
- [ ] Every acceptance criterion links to reproducible evidence.
- [ ] Persistent/network format versions and migration behavior are documented.
- [ ] Unit, property, integration, failure, and performance suites applicable to
  the milestone pass from a clean checkout.
- [ ] Failure tests cover crashes at durable boundaries, retry/idempotency, and
  recovery convergence—not only steady-state availability.
- [ ] Security and privacy changes have a threat-boundary review.
- [ ] Capacity and latency results name inputs, source revision, hardware, and
  configured limits.
- [ ] Operator documentation and observability cover degraded and recovery
  states.
- [ ] Open risks, deferred work, and accepted limitations are explicit.
- [ ] The milestone status table at the top of this document is updated.

# distr-hnsw — Design Document (v2.1)

**A unified distributed semantic storage service, self-hosted on a Tailscale
network.**

Files are stored replicated across always-on machines on a tailnet and indexed
by *meaning*: every searchable file is embedded into vector space, and search,
browsing, and grouping are built on nearest-neighbor structure. The blob and
vector planes are parts of one storage system, with one control plane, one
durability model, and one operator experience. The collection API exposes the
same indexing substrate to advanced applications; it is an extension of the
storage service, not a second product. The system is designed so that other
people can deploy it on their own tailnets.

Status: pre-implementation design, v2.1 (2026-07-17). Supersedes the v1
file-system-only spec after the pivot to vector-native organization. v2.1
folds in design-review feedback: a phase-0 validation prototype, hybrid
keyword+semantic search, filtered-ANN execution modes, a first-class
extraction pipeline, int8 quantization with a published capacity formula,
adaptive snapshot cadence, and phase-tagged open questions. Project renamed
from `filedrop` to `distr-hnsw` (2026-07-17); public name remains open (§15).

---

## 1. Product definition

**One product, two cooperating data planes.** distr-hnsw is a distributed
semantic storage service. Users upload files through one portal endpoint; the
system chunks, encrypts, and replicates their bytes, then builds distributed
HNSW indexes for semantic search, "more like this," and dynamic collections.
The blob plane is the durable source of truth and the vector plane is the
native organization and retrieval layer.

Applications on the tailnet may use a collections API to store documents or
raw vectors against the same cluster, with per-collection embedding config and
API keys, sharing the same placement, reconciliation, observability, and
recovery machinery as file storage.

### Goals

- Durable distributed storage: some files exist *only* here. Blob durability
  is first-class.
- Vector-native organization as the primary UX, with manual collections,
  tags, and classical facets as stable user-controlled structure; HNSW
  collections are also available to advanced applications.
- The semantic bet is validated empirically *before* the distributed build:
  the phase-0 prototype (§14) must show semantic search beating name/date
  retrieval on the operator's real corpus, and it picks the default
  embedding model and dimensionality.
- Customizable storage provisioning per machine (disk quotas per volume, RAM
  budgets per machine) with **auto-balancing** when a machine exceeds its
  budget or when machines are added.
- Tailscale-native networking, auth, and identity.
- Embedding via **both** a local model (GPU box on the tailnet) and
  API-based providers, selectable per collection.
- Designed with explicit migration seams from 3 machines and thousands of
  vectors toward much larger deployments. v1 has a measured operating
  envelope; growth beyond it may replace implementations, but must preserve
  collection, placement, and data-format boundaries rather than require a
  product rewrite.
- Deployable by other users on their own tailnets: single binary, bootstrap
  flow, centrally managed config, real docs.
- Learning vehicle for distributed-systems *design* (replication, WAL,
  rebalancing, routing) — the operator understands every moving part, even
  where implementation is assisted.

### Non-goals (v1)

- Multi-user *within* one deployment — deferred; schema and auth designed for
  it (stable `user_id`s everywhere). Multi-deployment (each person runs their
  own cluster) is the supported model instead.
- Portal high availability — fast-rebuild recovery, not failover (§11).
- Semantic *routing* of queries (the hierarchical top layer) — v1 fans out to
  all partitions; the routing layer is a planned later optimization with an
  explicit seam reserved for it (§8.4).
- Image/multimodal embedding — v1 file search is text-only; CLIP-family
  image collections are a reserved post-v1 seam (§4, §14).
- Erasure coding, native sync client, non-Tailscale transport (mTLS mode is a
  designed-for-later seam, §15).
- Operating or positioning the vector plane as a standalone service separate
  from distr-hnsw's storage and control planes.
- Nodes that sleep or leave the tailnet: storage machines are always-on;
  outages are faults to heal, not steady state.

---

## 2. System overview

```
                            tailnet (WireGuard)
 ┌─────────┐ HTTPS ┌───────────────────────────────┐
 │ browser │──────▶│ PORTAL (one machine)           │
 │ or app  │       │  dashboard SPA · JSON API      │
 └─────────┘       │  auth: WhoIs + sessions + keys │
                   │  ingest: chunker · crypto      │
                   │  embed dispatch                │
                   │  query: scatter-gather merge   │
                   │  placement + routing table     │
                   │  reconciler · balancer         │
                   │  metadata (SQLite)             │
                   └──┬──────────┬──────────┬───────┘
        blob plane    │          │          │     vector plane
   PUT/GET chunks     ▼          ▼          ▼     WAL append / ANN query
                 ┌────────┐ ┌────────┐ ┌────────┐
                 │machine A│ │machine B│ │machine C│
                 │ agent   │ │ agent   │ │ agent   │  chunk store (dumb)
                 │ index   │ │ index   │ │ index   │  partitions + HNSW
                 └────────┘ └────────┘ └────┬───┘
                                            │
                                   ┌────────▼───────┐
                                   │ GPU box: local  │
                                   │ embedding server│
                                   └────────────────┘
```

**One binary, multiple roles**: `distr-hnsw portal`, `distr-hnsw agent` (blob
plane), `distr-hnsw index` (vector plane). Agent and index typically colocate
on each machine but are separate processes with separate failure and rebuild
stories. The GPU box runs an off-the-shelf embedding server (TEI/Ollama);
distr-hnsw talks to it, it is not part of the binary.

**Two data planes, one placement philosophy.**

- The **blob plane** stores immutable, content-addressed, encrypted chunks —
  placement by *policy* (space, tiers, failure domains). It is the source of
  truth for file bytes, and the internal substrate for the vector plane's
  snapshots and WAL archives.
- The **vector plane** stores mutable partitioned HNSW indexes — placement by
  *capacity packing* (disk + RAM budgets), moved by the balancer.
- Both planes are governed the same way: **declarative config in the portal
  DB, enforced by reconciliation loops.** Policy edits, quota changes, and
  machine additions create reconciliation work, never manual migrations.

**Proxy model.** All client traffic terminates at the portal; machines are
unreachable except from the portal (tailnet ACLs). Bytes flow through the
portal both ways. The internal chunk/partition APIs use capability-style
signed grants bound to operation, object/partition, placement epoch, and a short
expiry. This also reserves a safe direct-read data path without rearchitecture.
"One endpoint" is the permanent promise; "one machine implementing it" is v1.

### 2.1 System invariants

These statements are release contracts, not aspirations:

- A visible `committed` file has a replicated immutable manifest containing its
  wrapped content key, and that manifest plus every referenced chunk meets the
  file class's `minimum_write_replicas`.
- No repair, rebalance, compaction, quota change, or garbage-collection action
  deletes an old copy before replacement copies are durable and confirmed.
- The vector plane may delay or lose derived file indexes without affecting
  committed file-byte retrieval.
- An acknowledged primary vector write is present in the configured durable
  quorum and can be recovered from WAL and/or a snapshot plus WAL tail.
- At most one primary epoch for a partition may have writes acknowledged as
  committed. Writes stranded on an old primary are discarded during rebuild;
  stale epochs are rejected when an old node returns after a partition.
- Capacity estimates reserve recovery and compaction headroom; admission
  control runs before an operation would violate a hard durability invariant.
- In-tailnet replication is not called backup. "Recovery ready" requires the
  independently restorable offsite state defined in §11.1.

---

## 3. Tailscale integration

Machines are multi-purpose and already tailnet nodes; distr-hnsw rides the host
`tailscaled` — no tsnet, no extra tailnet nodes.

- **Bind discipline**: daemons listen only on the machine's tailnet IP
  (100.x.y.z), discovered via LocalAPI at startup; never `0.0.0.0`. Handle
  tailscaled-not-ready (systemd `After=` + retry) and restarts.
- **Identity via WhoIs on the socket**: portal resolves every connection with
  LocalAPI `WhoIs(remoteAddr)` → verified node + user. `tailscale serve`
  identity *headers* are rejected — any local process on a multi-purpose
  machine could forge them.
- **HTTPS via tailscaled certs**: LocalAPI-minted Let's Encrypt certs for
  `<machine>.<tailnet>.ts.net`, wired into the TLS config. Required: browsers
  need a secure context for browser cryptography (vault). The implementation
  owns renewal and reload behavior in Rust; deployments document that issuing
  public certificates places the machine and tailnet DNS names in Certificate
  Transparency logs. Internal traffic skips TLS — the tailnet is already
  encrypted and mutually identified.
- **ACLs as the outer wall**: agent/index ports accept only the portal
  machine; portal HTTPS accepts the operator's devices. Enforced at the
  WireGuard layer.
- **Process authorization as defense-in-depth**: WhoIs names the *node*, not
  the *process*. Internal operations additionally require short-lived
  capability grants signed by a portal-held asymmetric key; agents receive
  only the verification key and therefore cannot forge grants for one another.
  Join tokens are single-use, and signing-key rotation is part of cluster ops.

---

## 4. Semantic organization model

The core UX bet: vector-space structure is the primary way to retrieve and
browse files. It does not remove stable, user-controlled organization.

- **Identity**: every file has a stable `file_id`, a display name, and
  free-form tags. There is a flat namespace, not a path tree. (WebDAV/path
  compatibility, if ever added, is a *view*, not storage structure.)
  File bytes are immutable in v1; rename/tag edits create a new manifest
  generation, while content replacement creates a new file id linked as a
  version successor. Search, similarity, and neighborhoods are
  **latest-only**: on replacement the superseded file's vectors are
  tombstoned in the file collections; old versions stay reachable from the
  file page's version history and via an `include_versions` API flag.
- **Search is hybrid**: one endpoint runs a keyword leg (SQLite FTS5 BM25
  over file names and extracted text, with trigram/prefix tokenization on
  names so partial filename matches work) and a semantic leg (query embedded
  → ANN over the file collection) in parallel, merged with reciprocal rank
  fusion; exact name hits rank above everything. This is the primary "open a
  file" gesture. The keyword leg covers exact-token queries that embeddings
  miss ("invoice 4021", an error code), and search degrades to keyword-only
  when no embedding provider is reachable (§7).
- **Neighborhoods**: every file page shows "more like this" (its ANN
  neighbors). Browsing is walking the graph.
- **Smart collections**: saved semantic queries and/or cluster-derived groups
  ("tax documents," "screenshots of dashboards") that update as files arrive.
  Clusters are computed from the vector space (e.g., periodic k-means over
  file vectors) and are *suggestions* the user can name and pin — the system
  proposes structure instead of demanding it up front.
- **Manual collections**: explicit user-curated groups that never change unless
  the user changes them. Smart and manual collections can be nested in the UI
  without becoming the physical storage layout.
- **Facets that stay classical**: time, file type, size, tags. Semantic
  organization complements rather than replaces boring filters.
- **Multi-vector files**: large documents embed as multiple text chunks;
  file-level search aggregates chunk hits (max-sim in v1). v1 is text-only:
  image/multimodal embedding (CLIP-family, in its own collection/space) is a
  reserved post-v1 phase — deferring it removes an entire modality's
  extraction, embedding, and RAM cost from the critical path.
- **The vault is structurally unsearchable**: the portal cannot read vault
  plaintext, so vault files are never embedded — and that is correct, since
  embeddings leak content. Vault items are reachable by name/tag only.

Files remain fully retrievable without the vector plane (by id, name, tag,
recency). Semantic features degrade gracefully if the index is rebuilding —
the blob plane never depends on the vector plane.

---

## 5. Blob plane

(Condensed from the v1 spec; durability invariants tightened in v2.)

- **Chunks**: files split into 4 MiB chunks, encrypted, then named by the
  BLAKE3 hash of the ciphertext. Content addressing gives integrity checks and
  idempotent storage/replication of the same ciphertext. Per-file random keys
  intentionally prevent cross-file plaintext deduplication. Agents store
  chunks fanned out by hash prefix under volume paths.
- **File manifests**: after chunk encryption, the portal creates an immutable,
  versioned manifest containing file identity, a monotonically increasing
  generation, display metadata, ordered ciphertext chunk hashes and sizes,
  encryption parameters, and the wrapped content key. Except for a minimal
  type/version/file-id/generation header, the manifest payload is encrypted
  under a per-manifest key wrapped by the server master
  key; the file content key remains separately wrapped under the regular-file
  or vault scheme. Canonical manifest bytes are content-addressed and replicated
  through the blob plane under the file's storage class. Manifests use a
  distinct object namespace so inventory can identify them during recovery.
  SQLite indexes the manifest; it is not the only copy of the information
  required to reconstruct a file.
- **Volumes**: one directory on one physical disk; attributes `tier`
  (`ssd`/`hdd`/…), `quota` (max bytes distr-hnsw may use; reported free =
  `min(quota − used, disk free)`), `role` (`standard` | `redundancy-only`),
  failure domain (machine). Centrally configured in the portal DB, editable
  in the dashboard; machines keep only bootstrap config.
- **Storage classes**: named placement policies per file/collection with three
  separate contracts: `desired_replicas`, `minimum_write_replicas`, and tier /
  failure-domain constraints. A write may satisfy the durability floor while
  remaining policy-degraded. `strict` classes require both the durability floor
  and placement policy before acknowledgement; default classes require the
  durability floor, then alert and reconcile tier or placement violations.
  The dashboard never describes a policy-degraded object as fully healthy.
- **Write quorum**: uploads are acknowledged only after
  `minimum_write_replicas` copies are durably written and confirmed on distinct
  failure domains. The default class desires RF3 and requires at least two
  durable copies; a single-copy acknowledgement is an explicit opt-in unsafe
  class, never an implicit degraded mode. Placements are `pending` /
  `confirmed` / `orphaned` — an explicit state machine.
- **Scrub + reconcile + move**: agents re-hash locally; the portal audits
  replica counts and policy conformance and schedules rate-limited repairs.
- **Agents stay dumb.** PUT/GET/DELETE/list/health. All intelligence lives
  in the portal. This plane is where torture tests concentrate: kill an agent
  mid-upload, flip bits on disk, partition a node during scrub — the system
  must heal.

### 5.1 File commit protocol

Every upload has an idempotency key and an explicit state:

1. Portal creates a `staging` file record and persists an encryption plan with
   its wrapped content key, envelope version, per-chunk nonces, and class before
   dispatching any chunk. A retry of this upload reuses the plan.
2. Portal encrypts and streams chunks to candidate volumes. Agents acknowledge
   a chunk only after the file and its containing directory are durably synced
   according to the platform-specific storage contract.
3. Portal records confirmed placements transactionally. Retries may create
   duplicate physical writes but converge on the same ciphertext hash.
4. Once every chunk meets `minimum_write_replicas`, the portal constructs the
   immutable file manifest and replicates the manifest object to the same
   durability floor.
5. One SQLite transaction records the manifest hash and marks the file
   `committed`. Only committed files are visible or downloadable. Policy
   shortfalls remain separately visible as degraded.
6. A sweeper expires abandoned staging uploads and deletes unreferenced chunks
   only after a grace period. It must tolerate a portal crash at every boundary.

The implementation test matrix enumerates every crash point between these
steps and proves that recovery produces either one committed file or harmless
garbage eligible for later collection—never a visible partial file.

### 5.2 Delete protocol

Delete first replicates an immutable deletion marker `(file_id, generation,
deleted_at)` to the file's durability floor, then commits the SQLite tombstone
and revokes active shares. Reads and index results stop immediately. During
recovery, the highest manifest/deletion generation wins, so stale metadata does
not resurrect a deleted file.

Physical chunks are collected only after a retention window, after all
non-retired nodes have observed the deletion generation, and only when no live
file manifest references the ciphertext hash. A long-absent node must be
formally retired before collection; if it later returns, it rejoins with a new
identity and a wiped/reconciled object store. Offsite backups follow their own
documented retention policy; "deleted from the live cluster" and "expired from
backup history" are distinct states.

The blob plane additionally stores file **manifests**, vector-partition
**snapshots**, **WAL archives**, and **source documents** pushed by apps that
want re-embeddability.

---

## 6. Vector plane

### 6.1 Collections

The unit of indexing and API surface inside the unified storage service. Each
collection has: dimensionality, distance
metric, embedding config (§7) or `raw` (bring-your-own-vectors), durability
config, and API-key grants. System collections: `files-text` (v1) and,
post-v1, `files-image` (§4). Advanced apps may create their own.

Records are `(id, vector, payload JSON, [source-doc ref])` with upsert and
delete.

### 6.2 Partitions

Each collection is divided into **partitions** — the unit of placement,
replication, and movement. v1 maps the hash of each record id into a fixed
logical hash space whose ranges are owned by partitions. A size-triggered split
divides one range on the next hash bit; only that range's records move. It does
not recompute `hash % partition_count` and reshuffle the collection. Each
partition has a **primary** machine and `R−1` **replica** machines (distinct
failure domains), recorded in the portal's routing table with a monotonically
increasing placement epoch.

### 6.3 Durability: WAL replication

Vectors from raw-vector collections are **primary state** — nothing upstream
to rebuild from. The write path treats them accordingly:

1. Portal routes the operation with the current partition epoch and a stable
   client/idempotency id. A primary rejects stale epochs and previously applied
   operation ids. Idempotency ids are part of WAL/snapshot state so promotion
   does not forget recent acknowledgements.
2. The primary serializes accepted writes, assigns the next monotonically
   increasing WAL sequence, appends the complete operation (including
   vector/payload or tombstone) to its checksummed **write-ahead log**, durably
   syncs it, and forwards the same ordered entry to replicas. Replicas durably
   sync before acknowledging. "Written" always means recoverable after process
   and host restart, not merely present in a page cache.
3. Ack to the client after the configured write quorum is durable. RF2 requires
   both copies and therefore stops accepting writes when either copy is
   unavailable; RF3 uses a majority and can tolerate one unavailable replica.
4. Index nodes apply committed WAL entries in sequence to their HNSW. The
   acknowledged consistency contract is read-your-writes through the portal;
   stale replicas do not serve a read requiring a newer high-water mark.
5. **Snapshots** (a consistent serialized index + high-water mark) are
   written to the blob plane; the WAL truncates behind them. WAL segments
   also archive to the blob plane between snapshots. Cadence is adaptive —
   snapshot when the WAL tail exceeds ~25% of index size or an absolute byte
   cap — and uploads are rate-limited under the same IOWeight discipline as
   repairs. `rebuildable` collections snapshot on a looser schedule since
   their recovery story does not depend on it. Snapshot bytes/day is a
   published envelope metric (§8.5).
6. Recovery of a partition replica = fetch latest snapshot + replay WAL tail.

On primary failure, the portal stops issuing grants for the old epoch and waits
for outstanding short-lived grants to expire. Promotion uses the highest
high-water mark the portal recorded as quorum committed, verifies that the
candidate contains it, increments the epoch, and rejects writes until the new
primary has replayed through that point. Unacknowledged WAL tails are never
silently promoted as committed state. A node from an older epoch may rejoin
only as a rebuilding replica. This is a single-control-plane protocol, not a
claim that index nodes run consensus; the portal's availability and
metadata-recovery limits remain explicit in §11.

Per-collection durability modes:

- `primary` (default for raw collections): as above, RF ≥ 2.
- `rebuildable` (default for file collections and any collection whose
  source docs live in the blob plane): RF1 index + the ability to re-embed
  from source. Cheaper; recovery is slower. The blob plane's RF3 is doing
  the real durability work.

`primary` mode is the sole reason the vector plane needs quorums, epochs,
fencing, and promotion at all — `rebuildable` collections could recover from
snapshots and re-embedding alone. It stays in v1 deliberately: replicated-WAL
correctness is a core learning goal of this project (§1), and raw-vector
collections are the only data with no upstream source. The documented default
for `primary` collections is **RF3 majority**. RF2 remains selectable, but the
dashboard warns at creation time that it halts writes whenever either copy is
unavailable — on a small cluster that is *less* write-available than a single
node.

Snapshot, split, compaction, and movement protocols all operate at a declared
WAL high-water mark and placement epoch. Implementations must specify which
phase accepts writes, how catch-up proceeds, and the atomic cutover condition;
no operation may have two writable primaries for one epoch.

### 6.4 HNSW, deletes, compaction

- In-repo HNSW implementation (see §13 for why): in-RAM graph per partition,
  serialized to memory-mapped snapshot files; SIMD distance kernels
  (cosine / dot / L2).
- **Deletes are tombstones**: the graph node is masked from results but not
  unlinked (HNSW connectivity degrades under true deletion). Background
  **compaction** rebuilds a partition's graph when tombstone ratio exceeds a
  threshold — scheduled by the reconciler, rate-limited, replica-first then
  cutover, so queries never see a rebuilding primary.
- Query executes on the primary; replicas serve reads on primary failure
  (and, later, for load spreading).

### 6.5 Memory as a first-class budget

HNSW serves from RAM. Every machine's config declares a **RAM budget** for
the index role alongside its disk quotas; the placement engine packs
partitions against *both* dimensions. Cold partitions (by query traffic) may
be demoted to disk-resident (mmap, page-cache-served) under RAM pressure —
the balancer treats "RAM residency" as a placement property with the same
degrade-and-reconcile behavior as everything else. True beyond-RAM designs
(DiskANN-style graph-on-SSD) are a reserved later phase, not v1.

The RAM budget is an admission and placement estimate based on measured vector,
graph, payload, and allocator overhead—not a promise that `mmap` page-cache
residency can be controlled exactly. Nodes retain OS and compaction headroom;
they do not pack nominal partition estimates to 100% of physical RAM or disk.

Vectors in the v1 graph and snapshot format are stored **int8
scalar-quantized** (~4× RAM reduction, typically ~1% recall cost);
full-precision originals remain in the WAL/snapshot stream so top candidates
can be exactly rescored and requantization stays possible. File collections
default to **≤768 dimensions** (natively or via Matryoshka truncation).
Capacity planning and admission control share one published formula —
`vectors × dims × bytes_per_dim × overhead_factor` against aggregate cluster
budgets, with the overhead factor (graph links ≈ 100–150 bytes/node at M=16,
payload, allocator slack) *measured* by the benchmark suite, not assumed. A
collection's ceiling is the aggregate RAM budget across machines — RAM scales
linearly with added machines via partition placement — but a single
partition's graph must stay RAM-local to one node, which the size-triggered
splits (§6.2) enforce.

### 6.6 Filtered queries

Facet filters (type, size, time, tags) compose with ANN per partition in one
of two modes chosen by measured filter selectivity. Naive post-filtering of an
unfiltered top-k silently destroys recall under selective filters and is never
used.

- The portal evaluates classical facets in SQLite — the source of truth for
  mutable metadata, which avoids double-writing tags into vector payloads —
  and ships the matching-id set to index nodes as a roaring bitmap (a 1M-file
  id space compresses to KBs–low MBs; fine over the tailnet).
- **Selective filter** (candidates below a threshold on the order of 50×k):
  skip the graph entirely and brute-force exact distances over just the
  matching vectors. Perfect recall, trivially cheap — precisely the regime
  where graph traversal collapses.
- **Broad filter**: masked HNSW traversal — excluded nodes are routed
  *through* but never scored or returned. This is the tombstone mask (§6.4)
  with a different bitmap source; one code path serves both.

The recall benchmark measures filtered queries at multiple selectivity levels
(0.1%, 1%, 10%, 50%); the brute-force cutover threshold is set from those
measurements, not guessed.

---

## 7. Embedding pipeline

- **Provider trait** per collection:
  - `local`: HTTP to a self-hosted inference server on the tailnet (the GPU
    box) — text-embeddings-inference or Ollama; model name in config.
  - `api`: OpenAI-compatible embeddings endpoint (key stored on portal,
    encrypted). Explicit tradeoff, chosen per collection: convenience vs.
    file contents leaving the tailnet. The file store's system collections
    default to `local` when a local provider is configured.
  - `raw`: no embedding; clients supply vectors.
- **File ingest flow**: upload → blob plane commit (durability first) →
  async: text extraction (or image decode) → chunking → embed via provider →
  upsert vectors into the file collection. Pipeline status tracked per file
  (`pending` / `extracted` / `needs-ocr` / `indexed` / `failed` /
  `excluded`); failures retry with backoff and surface in the dashboard. A file is safe the moment blobs
  commit — searchability lags by seconds and that is acceptable and explicit.
- **Model identity is part of the collection.** Vectors from different
  models/versions never mix in one collection. Changing a model = creating a
  new collection version and re-embedding (from stored source docs) in the
  background, then atomically switching reads — the reconciler pattern again.
  This is why the file store keeps source text extractions cached in the
  blob plane.
- The GPU box is *not* special in the cluster: it runs a stock inference
  server. If no embedding provider is reachable, ingest queues and **search
  degrades to keyword-only** (§4) — text queries must be embedded at query
  time, so semantic search is only as available as a provider serving the
  collection's exact model. Provider redundancy therefore means multiple
  endpoints serving the *same model*; a fallback running a different model is
  not redundancy, it is a different collection.

### 7.1 Extraction pipeline

Extraction is modeled exactly like embedding: a pluggable **extractor trait**
with `(extractor_id, extractor_version)` recorded per file and extractions
cached in the blob plane. Upgrading an extractor is the same
re-derive-and-swap reconciler pattern as an embedding-model change, not a
special event. Extraction quality affects perceived search quality more than
ANN recall does, and is benchmarked accordingly.

- **v1 formats, local Rust workers**: plain text/markdown/code, HTML, Office
  formats (docx/xlsx/pptx — zip+XML), and PDF via pdfium bindings
  (`pdfium-render`); pdfium handles real-world PDFs meaningfully better than
  the pure-Rust PDF crates. PDFs without a text layer are marked
  `needs-ocr`, never silently indexed as empty.
- **OCR is a later provider, not a v1 blocker**: PaddleOCR or a small VLM on
  the GPU box behind the same trait; `needs-ocr` files are already queued
  for it.
- **Extraction runs out-of-process.** Parsers for untrusted file formats are
  the classic crash-and-CVE surface and never run inside the portal daemon:
  a worker subprocess with per-file memory and time caps, killed on hang,
  with failures recorded per file and surfaced in the dashboard.
- Extracted text also feeds the FTS5 keyword index (§4), so extraction is on
  the critical path for *both* search legs.

---

## 8. Placement, budgets, auto-balancing

### 8.1 Config surface (per machine, central)

```yaml
machines:
  hermes:
    volumes:
      - { path: /mnt/nvme/distr-hnsw, tier: ssd, quota: 500GB, role: standard }
      - { path: /mnt/tank/distr-hnsw, tier: hdd, quota: 4TB, role: redundancy-only }
    index:
      ram_budget: 8GB
      disk_quota: 200GB        # partitions, WALs, snapshots (on ssd tier)
```

Stored in the portal DB, edited in the dashboard, versioned in the audit log.
Adding a machine = install binary, `distr-hnsw join <token>`, then assign
volumes/budgets in the dashboard.

### 8.2 The balancer

One reconciliation loop, two planes:

- **Blob plane** (from v1): replica counts, policy conformance, quota
  shrinkage → mover re-replicates/migrates chunks, rate-limited.
- **Vector plane**: watches per-machine disk *and* RAM utilization against
  budgets and per-partition size/traffic. Triggers:
  - **Machine over budget** ("a layer of the network exceeds its storage
    budget") → move partitions to machines with headroom.
  - **New machine joins** → gradual rebalance toward capacity-weighted
    spread (no thundering herd; movement is rate-limited and preemptible).
  - **Partition too big** → split (hash partitions split cleanly by id bit;
    semantic partitions, later, split by sub-clustering).
  - **Cluster over budget globally** → nothing to move; admission control
    (reject writes for affected collections with a clear error) + loud
    dashboard/alert. Full disks must degrade the *service*, not the *data*.

**Partition movement protocol** (the heart of auto-balancing): snapshot-ship
to the destination → stream WAL catch-up → brief write pause on the
partition (sub-second) → atomic routing-table cutover → old copy demoted to
garbage, collected after grace period. Same protocol serves rebalancing,
machine decommission, and replica placement — one mechanism, many policies.

### 8.3 Query path (v1: scatter-gather)

Facet filters resolve in SQLite first (§6.6); the keyword leg (FTS5) and the
semantic leg then run in parallel. Semantic leg: embed the query → fan out to
**all** partitions of the collection with the filter bitmap → each index node
returns its local top-k (masked traversal or brute-force per §6.6) → portal
merges to global top-k → fuse with the keyword leg via reciprocal rank fusion.
This introduces no *routing-induced* recall loss: every partition is searched.
HNSW remains approximate, so end-to-end recall is measured against brute force
and governed by an explicit target. Full fanout is entirely adequate at the v1
operating envelope. The portal's routing table is already the only place that
knows partition locations — that indirection is the seam the later phase uses.

### 8.4 Later scale phase: the hierarchical routing layer

The "machines as the top layer of the graph" structure, shipped as a
measured optimization once fan-out-everywhere is the proven baseline:

- Portal maintains a small in-RAM **routing index**: centroids summarizing
  each partition's region of vector space (SPANN-style coarse layer).
- Queries probe only the top-`nprobe` partitions by centroid distance;
  `nprobe` trades recall vs. cost, tuned against the v1 baseline as the
  recall oracle.
- Requires semantic (not hash) partition assignment, which brings the real
  research-grade problems: centroid drift, boundary effects, hot-region load
  skew, semantic splitting. The balancer and movement protocol are designed
  to be reused, but assignment, split, insert routing, and recall evaluation all
  change together. This is a deliberate migration behind the collection and
  placement boundaries, not merely an assignment-function swap.

### 8.5 v1 operating envelope

The single portal, proxy data path, SQLite control plane, and full-partition
fanout are deliberate v1 limits. The initial benchmark target is 3–8 always-on
machines, up to 1 million files, and up to 10 million vectors in one collection
on documented reference hardware. These numbers become supported limits only
after the failure-injection and benchmark suites establish them.

Every release publishes, for its reference deployment: maximum tested logical
blob bytes, files, vectors, partitions, and machines; ingest throughput; p50/p95
query latency and recall (unfiltered and at the §6.6 filter-selectivity
levels); snapshot bytes/day; portal CPU/network saturation; largest-partition
recovery and movement time; and full-cluster restore time. The portal applies
admission control before measured limits are exceeded.

Growth beyond the envelope may require multiple query coordinators, a direct
blob data path, a different metadata store, or hierarchical routing. Those are
implementation migrations behind stable APIs and placement epochs—not claims
that the v1 single-portal implementation itself scales to hundreds of millions.

---

## 9. Authentication and authorization

Unchanged three-layer design from v1, plus API keys:

1. **Identity providers** (pluggable): v1 ships Tailscale WhoIs; OIDC/
   passkeys are later additions behind the same interface.
2. **Users**: stable internal `user_id`; `identities (provider, external_id)
   → user_id`. Single-user v1, multi-user-ready schema.
3. **Sessions**: portal-issued cookies/tokens; downstream code sees only
   `user_id`.

- **API keys** for applications: bearer tokens scoped to collections with
  rights (`read` / `write` / `admin`), created in the dashboard, hashed at
  rest. Tailnet identity says which machine is calling; the key says which
  app and what it may touch. Both are checked.
- **Share links**: signed, expiring, revocable capability tokens for individual
  regular files. v1 recipients must already be able to reach the portal on the
  tailnet; public-internet sharing is not implied. The token carries file id,
  permission, expiry, and a revocation generation. Vault sharing is deferred
  because it requires a separate recipient-key and content-key rewrapping
  protocol.
- **Vault re-auth**: vault access requires the vault passphrase on top of an
  authenticated session.

Browser sessions use secure, HTTP-only, same-site cookies and CSRF protection
for state-changing requests. WhoIs authenticates the network peer; it does not
replace browser-session or application authorization.

---

## 10. Encryption

- **Versioned envelope encryption** (age-style): a per-file random content key
  encrypts chunks with XChaCha20-Poly1305 in v1. The manifest records protocol
  version, algorithm, KDF parameters, unique nonce per chunk, ordered chunk
  index, plaintext length, and authenticated file metadata. Chunk index, file
  id, and envelope version are authenticated as AAD to prevent chunk
  substitution, reordering, and cross-file replay.
- **Regular files**: wrapped by a server-held master key (portal-side; file
  with tight permissions in v1, keychain/passphrase-unlock later). The master
  key has a separately tested backup and rotation procedure; backing up chunks
  without it is not recovery. This protects data at rest on storage machines,
  not against portal compromise.
- **Vault**: content keys wrapped by an Argon2id passphrase-derived key the
  server never holds. Browser-side encryption uses an audited, pinned WASM
  implementation for Argon2id and XChaCha20-Poly1305; these algorithms are not
  assumed to be native WebCrypto primitives. The implementation streams chunks
  without buffering an entire file and uses bounded, benchmarked KDF parameters
  across supported clients. The portal relays ciphertext. Vault files are never
  embedded (§4). Passphrase loss = data loss, by design.
- **Vector plane at rest**: snapshots and WAL archives are blob-plane objects
  → encrypted like everything else. Live partition files and WALs on index
  nodes are encrypted with per-partition keys wrapped by the master key.
  Embeddings are treated as *content-equivalent* in sensitivity.
- **API provider keys, portal signing keys, and join credentials**: encrypted
  at rest under the master key; verification keys are non-secret and distributed
  to roles during join/rotation.

### 10.1 Threat-model boundary

Regular-file encryption protects against theft or compromise of storage disks
and agents. It does not protect plaintext from the portal, embedding provider,
or an authenticated user allowed to download the file.

The vault protects plaintext from storage agents and from passive portal-side
storage or database disclosure. The browser application is delivered by the
portal, so a malicious or actively compromised portal can serve JavaScript that
captures the vault passphrase or plaintext. v1 does not claim protection from
that attacker. Extending the boundary would require a separately distributed
and verifiable client (native app, signed extension, or equivalent).

---

## 11. Metadata, portal recovery

SQLite (WAL mode) + Litestream versioned offsite replication; sqlc-style typed
queries (Rust: `sqlx`/`rusqlite`), portable SQL as the Postgres escape hatch.
The dashboard reports observed replication lag. "Replicated" never means zero
RPO unless a synchronous mechanism establishes it.

Schema sketch:

```sql
users / identities / sessions / audit_log            -- as v1
nodes / volumes                                      -- as v1
classes (blob placement policies)                    -- as v1
files (id, owner_id, name, size, class_id, tags, state,
       upload_id, manifest_hash, manifest_version, manifest_generation,
       content_key_wrapped, wrap_scheme,
       pipeline_status, extractor_id, extractor_version, delete_generation,
       created_at, modified_at, deleted_at)
files_fts (FTS5: name, extracted_text; trigram on name) -- hybrid search (§4)
chunks / file_chunks (including ordinal) / placements
shares (capability tokens)                           -- as v1

collections (id, name, dim, metric, embed_config_json,
             durability, model_id, status)
partitions  (id, collection_id, key_range, placement_epoch,
             committed_high_water,
             size_bytes, ram_resident, tombstone_ratio, state)
partition_placements (partition_id, node_id, role primary|replica,
                      state, snapshot_ref, wal_high_water)
record_routes (collection_id, record_id, partition_id) -- only for future
                                                       -- non-hash assignment;
                                                       -- v1 hash routing is computed
api_keys    (key_hash, name, scopes_json, created_by, expires_at)
smart_collections (id, owner_id, name, query_json, pinned)
manual_collections / manual_collection_files
reconcile_jobs (id, kind, object_id, generation, state, lease_until)
```

The portal is a SPOF for availability. Metadata and master-key custody are also
critical data dependencies even though chunks and snapshots live on other
machines. The system may claim that a portal loss is recoverable only after the
following procedure is automated and rehearsed: restore SQLite to a declared
point, recover the master key through its separate protected procedure, start
the portal on a surviving machine, fence recovered partition epochs, inventory
roles plus replicated manifests/deletion markers, and reconcile metadata
against physical objects. Replicated manifests and markers can reconstruct core
file records that are newer than the restored SQLite point, though sessions,
shares, audit history, and other control metadata still obey the declared
metadata RPO. Target portal recovery is ~10 minutes after required recovery
material is available.

### 11.1 Disaster-recovery contract

Replication inside one tailnet protects against individual machine and disk
failure; it is not a backup. Before distr-hnsw is allowed to hold the only copy of
a file, an offsite job must continuously copy committed encrypted blob objects
(file manifests, deletion markers, and chunks; vector snapshots and archived
WALs) into versioned object storage. The corresponding SQLite history and
master-key recovery material must be recoverable independently of every cluster
machine.

The operator selects and can see explicit targets for:

- metadata and blob recovery-point objective (RPO);
- portal-loss and total-cluster-loss recovery-time objectives (RTO);
- live deletion grace period and offsite backup retention;
- last successful backup, integrity verification, and full restore drill.

A release gate restores a representative cluster into empty infrastructure,
downloads regular files byte-for-byte, rebuilds a file collection from source,
and restores a primary raw-vector collection from snapshot + WAL. The product
does not display "durable" or "recovery ready" unless this gate has passed for
the configured deployment. True portal HA remains deferred; future HA requires
an explicit consensus/lease design rather than assuming the current portal is
stateless.

---

## 12. External API (sketch)

```
# files (the product)
POST   /api/files                    upload (multipart, Idempotency-Key) → file_id
GET    /api/files/{id}               download (Range supported)
DELETE /api/files/{id}               tombstone + asynchronous collection
GET    /api/search?q=…&filters=…     hybrid (keyword + semantic) + faceted search
GET    /api/files/{id}/similar       neighbors
CRUD   /api/smart-collections
CRUD   /api/manual-collections
CRUD   /api/tags, /api/shares

# advanced collections surface (same storage service; API-key auth)
POST   /api/collections              create (dim, metric, embedding, durability)
PUT    /api/collections/{c}/records  idempotent upsert documents or vectors
POST   /api/collections/{c}/query    vector or text query, top-k, filters
DELETE /api/collections/{c}/records/{id}

# ops
GET    /api/cluster/health           machines, budgets, partition map, alerts
```

Internal (portal ↔ roles, tailnet-only, portal-signed grants):
blob plane as v1 (`PUT/GET/DELETE /chunks/{hash}`, scrub, health); index
plane adds `POST /partitions/{id}/wal` (append batch), `POST
/partitions/{id}/query`, snapshot fetch/install, `GET /health` (RAM/disk
usage per partition). Plain HTTP, streaming bodies, curl-able. No gRPC.

---

## 13. Tech stack

| layer | choice | rationale |
|---|---|---|
| language | **Rust** | ANN search is CPU-bound SIMD work — performance is a feature now, not a nicety; fearless concurrency for a long-lived daemon holding mutable indexes; the category's home language (Qdrant, LanceDB, Garage). tsnet was Go's trump card and we don't use it (LocalAPI is language-neutral). |
| runtime/web | tokio + axum + tower | standard, mature |
| HNSW | **in-repo implementation** | persistence, deletes/tombstones, crash recovery, mmap snapshots, and measured recall are core learning and product goals. Owning the engine means owning its format and failure behavior; it is an intentional schedule cost. |
| distance kernels | simsimd or stable architecture intrinsics; optional nightly `std::simd` experiments | the hot loop; stable builds do not depend on experimental portable SIMD |
| index persistence | custom binary snapshots + memmap2; CRC-checksummed WAL segments | recovery and compaction are first-class |
| metadata | SQLite (rusqlite/sqlx) + Litestream | single writer within the measured v1 envelope, ops simplicity, Postgres escape hatch |
| hashing / crypto | BLAKE3; XChaCha20-Poly1305, Argon2id (RustCrypto) | as v1 |
| tailscale | LocalAPI over Unix socket (WhoIs, certs) | §3 |
| embeddings | provider trait → TEI/Ollama (local) or OpenAI-compatible (api) | §7 |
| extraction | extractor trait → pdfium-render + format crates in an out-of-process capped worker; OCR provider later | §7.1 |
| keyword search | SQLite FTS5 (BM25, trigram on names) fused with ANN via RRF | §4, §8.3 |
| frontend | React + TS + Vite + Tailwind, embedded via rust-embed | interaction-heavy UI; single-binary deploy preserved |
| observability | tracing + Prometheus endpoints | dashboard health page reads the same metrics |
| deployment | single binary, systemd units, IOWeight/CPUWeight caps | polite tenant on multi-purpose machines |
| testing | model/property tests on placement and state machines; failure-injection harness spawning real roles on temp volumes; forced process/host loss, network partition, stale-node return, bit flips, ENOSPC, and rolling upgrades; recall benchmarks vs. brute force; empty-infrastructure restore drills | the storage layer earns trust through torture and recovery; the index earns it through measured recall |

---

## 14. Build order

Riskiest-first — and *product*-riskiest first: the semantic bet is validated
before any distributed machinery exists. Vector-plane correctness is measured
against brute force from day one. Estimates are rough solo-effort scale
markers, not commitments; they exist so that scope decisions are made against
a calendar rather than in the abstract.

0. **Validation prototype** (~days). A flat script: embed the operator's
   real corpus into SQLite, brute-force search, and compare semantic
   retrieval against filename/date retrieval on real queries. Output is a
   go/no-go on the core UX bet plus the chosen default embedding model and
   dimensionality (§15). No distributed anything; the code is disposable.
1. **Blob plane + recovery foundation** (~2–3 months) — agent + chunk store +
   volumes/quotas; then portal-core commit state machine, placement, write
   quorum, delete, scrub/reconcile/move. Add offsite encrypted-object backup,
   independent master-key recovery, Litestream restore, and an
   empty-infrastructure drill. Torture tests green on 3 local processes. This
   remains first because everything above it—snapshots, WALs, and source
   documents—stands on it. distr-hnsw may not hold the only copy of a file until
   this recovery gate passes.
2. **Tailscale auth + sessions + API keys** (~2–4 weeks) — WhoIs provider,
   certs, bind discipline, ACL docs.
3. **Single-partition vector engine** (~1–2 months) — HNSW
   build/query/persist, int8 scalar quantization with exact rescoring, WAL,
   snapshot/restore, tombstones + compaction, filtered-query execution
   (masked traversal + brute-force cutover, §6.6), recall benchmarks vs.
   brute force on standard datasets — unfiltered and filtered.
4. **Distributed vector plane** (~2–3 months) — partitions, routing table, WAL
   replication + quorum, epochs/fencing/promotion, scatter-gather query merge,
   partition movement protocol, the balancer (budgets, joins, splits). Failure
   injection: kill and partition a primary mid-write, return a stale primary,
   move and split a partition under load, fill a machine, and restore a raw
   collection from snapshot + WAL. No two-primary history may be accepted.
5. **Extraction + embedding pipeline + file semantics** (~1–2 months) —
   extractor trait and out-of-process worker (§7.1), FTS5 index, embedding
   providers (local + api), ingest flow, file collections, hybrid
   search/similar (RRF fusion), smart-collection and manual-collection APIs.
6. **Dashboard** (~1–2 months) — semantic file UI (search-first browser,
   neighborhoods, smart and manual collections, tags/facets), cluster ops UI
   (machines, budgets, partition map, extract/embed queue, backup/restore
   status, alerts).
7. **Vault + sharing + productization** (~1 month) — vault E2E flow, share links,
   `distr-hnsw init`/`join` bootstrap UX, deployment docs for third parties, and
   release qualification against the published v1 operating envelope.

Reserved seams (designed now, built later): centroid routing layer (§8.4),
direct-read data path, OIDC provider, mTLS non-Tailscale mode, DiskANN-style
beyond-RAM partitions, portal HA, multi-user, image/CLIP multimodal
collections (§4), OCR extraction provider (§7.1).

---

## 15. Open questions

Each question is tagged with the phase (§14) that must resolve it.

- **[phase 0] Default local embedding model and dimensionality** for the
  file collections (≤768 dims per §6.5) — chosen empirically by the
  validation prototype, not during phase 5.
- **[phase 1] Master key custody**: file vs. keychain vs. passphrase-unlock
  at portal start (unattended reboot tradeoff), plus the independent
  recovery ceremony.
- **[phase 1] Backup defaults**: which versioned object-store targets ship
  first, and what RPO/retention defaults are safe enough before distr-hnsw
  may hold unique data?
- **[phase 1] Admission-control UX** when the cluster is globally over
  budget. Working default: files-first — file collections win over app
  collections.
- **[phase 5] Chunk-to-file score aggregation**: max-sim first; revisit
  (mean-of-top-m, learned) once real usage exists.
- **[post-v1] Namespace compatibility**: do we ever need path/WebDAV *views*
  over the flat namespace for legacy tooling, or are API + web UI + manual
  collections enough?
- **[post-v1] Share audience**: remain tailnet-only, or later add a
  deliberately separate public ingress mode with its own abuse and
  authentication model?
- **[pre-publication] Public name**: working name is `distr-hnsw` (renamed
  from `filedrop`, 2026-07-17); test it and alternatives (**Scatter** remains
  a candidate) against the trust signal required of a durability product
  before the repo goes public.

Decided since v2: v1 file search is text-only — image/CLIP multimodal
collections move to a reserved post-v1 seam (§4, §14).

# distr-hnsw

Self-hosted, Tailscale-native distributed semantic storage service: replicated
encrypted blobs plus distributed HNSW vector search. Product design lives in
`DESIGN.md`. M0 is accepted; M1 blob-plane implementation is in progress under
`crates/distr-hnsw/`.


## Stack

- Rust service/binary; SQLite control-plane metadata.
- Tailscale networking and identity; browser dashboard/API.
- HNSW indexes, local or API embedding providers, encrypted blob storage.

## Working agreement

- Read the relevant section of `DESIGN.md` before changing architecture.
- Add or update focused tests with implementation changes; run the smallest
  relevant build, format, lint, and test commands before handoff.
- Preserve storage, durability, and epoch invariants; call out any intentional
  tradeoff or deviation.
- As you work, update the AGENTS.md and documentation.
- Two machines share this repository over Tailscale: the MacBook (`ssh
  macbook`, development and light testing) and the Fedora desktop `anthonypc`
  (RTX 3060 Ti, btrfs). Sessions may run on either; check `hostname` first.
  Large tests with large artifacts (storage flows, GPU embedding, corruption
  and movement matrices) belong on `anthonypc`. Both checkouts track
  `origin/main`; sync through git, not by copying trees.

## Documentation

- Documentation should go in /docs. If there isn't /docs already, create the directory.
- Update documentation as you go. Should be concise, accurate, with docs on the tech stack, architecture, etc.
- Documentation should be professional, as the end goal fo this project is not only a learning project, but also hopefully something we can open-source.

## Git

- Do not commit, push, create branches, or alter history unless the human
  explicitly asks.
- Keep changes and eventual commits small and scoped; do not bundle unrelated
  work or rewrite existing history.
- Do not discard or overwrite existing user changes.

## Structure

- `DESIGN.md` — product and architecture specification.
- `AGENTS.md` / `CLAUDE.md` — repository guidance.
- `docs/` — living documentation (phase status, next steps).
- `docs/roadmap.md` — gated milestones, acceptance criteria, and verification.
- `docs/m1-storage-contract.md` — persistent-format and durability contract for
  the implemented M1 blob-plane passes.
- `docs/m1-implementation-plan.md` — M1 work packages and dependency order.
- `docs/m1-lifecycle-contract.md` — safety gates for reconciliation, movement,
  retirement, and physical deletion.
- `docs/m1-filesystem-qualification.md` — durable-write review per filesystem
  and the power-loss drill still owed.
- `docs/m1-phase-1-decisions.md` — ratified phase-1 decisions (key custody,
  backup target, admission) and the tests each must pass.
- `docs/m1-restore-drill.md`, `docs/m1-lifecycle-matrix.md`,
  `docs/m1-capacity-drill.md` — M1 drill evidence.
- `scripts/` — operational drills and the MinIO test harness (see
  `scripts/README.md`).
- `crates/distr-hnsw/` — product service and CLI; keep this separate from the
  disposable phase-0 prototype.
- `crates/distr-hnsw-index/` — single-partition vector engine (M3): flat
  vectors, distance kernels, exact oracle, then HNSW. No tokio, no network.
- `crates/distr-hnsw-bench/` — benchmark harness against pinned datasets;
  manifests under `docs/bench/`, data under `~/distr-hnsw-bench/` on
  `anthonypc`. Plan: `docs/m3-implementation-plan.md`; formats and semantics:
  `docs/m3-engine-contract.md` (read before touching the engine).
- `prototype/` — **disposable** phase-0 validation CLI (`distr-hnsw-validate`).
  Not product code; do not grow it into the distributed service. See
  `docs/phase-0-validation.md`.

## Phase-0 compute

Heavy embed/eval runs belong on `ssh anthonylu@anthonypc` (Fedora, RTX 3060 Ti)
with Ollama. Laptop is for development and light fixture tests only.

Phase-0 bakeoffs use a fresh run-scoped work directory. Persist the Ollama
digest when embeddings are written and refuse evaluation if the query model no
longer matches. Reports must include query, source-tree, and executable hashes;
measure provider cold start separately from warmed p50/p95 query latency. Full
reports stay private/gitignored, while a sanitized aggregate summary lives
under `docs/` for reproducible public review.

Canonical bakeoffs must identify a clean source revision, retain the evaluated
binary in the private run directory, evaluate twice against unchanged inputs,
and pass `prototype/scripts/compare-eval-reports.py`. Retrieval evidence must
match exactly; latency variance is reported separately.

Only evaluate a model at its native dimension or at truncation dimensions the
model explicitly documents as supported (for example, Matryoshka embeddings).
Do not treat arbitrary vector slicing as a valid model configuration. Results
above the DESIGN product cap are diagnostic only: they cannot choose the
default model, affect dimension-lock confidence, or produce a go verdict.
Do not mask provider failures by silently padding or rewriting individual
inputs; any text normalization must be an explicit, corpus-wide experiment so
results remain comparable.

Assemble larger corpora with `prototype/scripts/assemble-mixed-corpus.sh`
(copy-only onto `anthonypc:~/distr-hnsw-proto/corpora/…`). Keep the query JSON
beside the stage tree, not inside it, so prepare does not index labels. Run
`prototype/scripts/run-bakeoff-anthonypc.sh` for the matrix.

**M0 status:** Accepted. The frozen holdout and exact repeat validate
`nomic-embed-text`; the documented non-inferiority tie-break locks 512d in
DESIGN §15 (`docs/phase-0-validation.md`). Do not retune the holdout or grow the
prototype into product code. M1 is unblocked and starts with the recovery-first
blob-plane slice in `docs/roadmap.md`.

**M1 status:** Accepted 2026-09-30 with recorded limitations (`docs/roadmap.md`).
The blob plane (`crates/distr-hnsw/`) provides the RF2 commit spine, logical
deletion, recovery, incarnations and observations, scrub/repair, drain,
retire, capacity admission, proof-based GC, master-key recovery bundles, and
backup set v1 with directory and S3 targets (`portal init|put|get|delete|
recover|scrub|health|key|backup|restore|drain|retire|gc`). Drills and their
evidence: `scripts/` and `docs/m1-*-drill.md`, `docs/m1-lifecycle-matrix.md`,
`docs/m1-filesystem-qualification.md`. Agents stay loopback-only until M2.
Tests use `TMPDIR=target/` via `.cargo/config.toml` so they run on a real
filesystem; `aws-lc-sys` needs a C compiler. Large matrices and drills run on
`anthonypc`.

**M3 status:** Exit review passed 2026-10-01; acceptance proposed with one
limitation (`project-nomic-512` measurements wait on the shared GPU). The
engine (`crates/distr-hnsw-index`) provides flat and int8 vectors, HNSW with
measured per-metric defaults, filtered search with a measured cutover, WAL and
snapshots with fail-closed recovery, two-phase compaction, and RAM budget
admission; `distr-hnsw index archive|restore` moves partition state through
the blob plane. Evidence: `docs/bench/README.md`, `docs/m3-engine-contract.md`.

**M3 status:** Exit review passed 2026-10-01; acceptance proposed with one
limitation (`project-nomic-512` measurements wait on the shared GPU). The
engine (`crates/distr-hnsw-index`) provides flat and int8 vectors, HNSW with
measured per-metric defaults, filtered search with a measured cutover, WAL and
snapshots with fail-closed recovery, two-phase compaction, and RAM budget
admission; `distr-hnsw index archive|restore` moves partition state through
the blob plane. Evidence: `docs/bench/README.md`, `docs/m3-engine-contract.md`.

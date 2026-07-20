# distr-hnsw

Pre-implementation design for a self-hosted, Tailscale-native distributed
semantic storage service: replicated encrypted blobs plus distributed HNSW
vector search. The full design is in `DESIGN.md`.


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
- I am currently developing on my personal laptop. This machine should only be used for developement and light testing. For larger tests where we might have large artifacts (like testing storage flow), use anthonylu@anthony pc on my tailscale network.

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

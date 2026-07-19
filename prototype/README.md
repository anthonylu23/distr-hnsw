# distr-hnsw-validate (phase-0 prototype)

Disposable Rust CLI that embeds a local corpus into SQLite, brute-force searches,
and compares semantic retrieval against name / recency / keyword baselines.

**Not product code.** See [DESIGN.md](../DESIGN.md) §14.0 and
[docs/phase-0-validation.md](../docs/phase-0-validation.md).

## Build

```bash
cargo build --release -p distr-hnsw-validate
```

Binary: `target/release/distr-hnsw-validate`

## Prerequisites

- [Ollama](https://ollama.com) listening on `http://127.0.0.1:11434`
- Embedding model(s), e.g. `ollama pull nomic-embed-text`

## Mixed corpus assembly (anthonypc)

Copy-only staging of a larger bake-off tree (never deletes/moves originals):

```bash
# From the laptop (Tailscale SSH + rsync to anthonypc):
./prototype/scripts/assemble-mixed-corpus.sh
# The script prints a unique stage such as:
# anthonypc:~/distr-hnsw-proto/corpora/mixed-20260719T050000Z/
```

Every invocation requires an empty, run-scoped destination, copies only
extractor-supported extensions, excludes worktree/build/fixture metadata,
deduplicates flattened PDFs by content, and writes `SOURCES`, `INVENTORY`,
`MANIFEST.sha256`, and `MANIFEST.digest` beside the stage. Set `CORPUS_ID` to a
stable label when a named rerun is useful; the script refuses to reuse a
non-empty stage.

Then run the bake-off script with that corpus and a ≥40-query set:

```bash
export CORPUS=$HOME/distr-hnsw-proto/corpora/mixed-20260719T050000Z
export QUERIES=${CORPUS}-queries.json
# Full matrix (default): nomic 768/512/384 + bge-m3 native 1024 diagnostic
./prototype/scripts/run-bakeoff-anthonypc.sh
# Nomic-only smoke: MODEL=nomic-embed-text ./prototype/scripts/run-bakeoff-anthonypc.sh
```

Pull models first if needed: `ollama pull nomic-embed-text` and `ollama pull bge-m3`.
The Ollama `bge-m3` artifact returns native 1024-dimensional vectors. BGE-M3 is
not documented as a Matryoshka model, so the prototype does not treat arbitrary
768/512 truncations as valid model configurations. Its 1024d result is reported
as a diagnostic but is ineligible for the product default because DESIGN caps
that default at 768 dimensions. `recommend()` chooses globally among eligible
configs; an oversized result cannot select a model, affect dim-lock confidence,
or produce a go verdict.

## Typical workflow (anthonypc)

```bash
export STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
export WORK_DIR="$HOME/distr-hnsw-proto/runs/$STAMP"
mkdir -p "$WORK_DIR"

# 1. Index corpus (extract + chunk)
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  prepare --fresh --corpus /path/to/your/files

# 2. Embed bake-off matrix (or use run-bakeoff-anthonypc.sh)
for dims in 768 512 384; do
  ./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
    embed --model nomic-embed-text --dims "$dims"
done
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  embed --model bge-m3 --dims 1024 --concurrency 1 --batch-size 1

# 3. Explore
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  query --text "spring tax documents" --model nomic-embed-text --dims 768

# 4. Evaluate labeled queries → reports/<stem>.{md,html,json}
#    (stop each model once before eval for a real cold start)
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  eval --queries queries.json \
  --model nomic-embed-text:768 \
  --model nomic-embed-text:512 \
  --model nomic-embed-text:384 \
  --model bge-m3:1024
```

Copy [`queries.example.json`](queries.example.json), assign each query a
`category`, and fill `relevant_path_globs` / `grades` for your corpus. Relative
grade selectors may name a relevant path component, filename, or filename stem
(case-insensitive), but can only grade files already declared relevant; they
never broaden the relevance set.

Embedding writes persist the Ollama model digest. If a tag changes, `embed` and
`eval` refuse to mix old document vectors with the current query model; rerun
the affected embedding configuration with `--force`. The digest is recorded
before the first vector so an interrupted run can resume safely. `--force`
clears that model/dimension configuration before rebuilding it, preventing a
failed rebuild from leaving mixed-provider vectors.

Evaluation warms each configuration before timing. Reports record one provider
cold start per model plus warmed p50/p95 latency, source-tree and executable
hashes, the query-set and indexed-corpus hashes, preparation fingerprint,
embedding-time model digests, category-stratified metrics, and an explicit
`dims_locked` decision.

## Fixture smoke test

```bash
cargo test -p distr-hnsw-validate
./target/release/distr-hnsw-validate --work-dir /tmp/dh-proto \
  prepare --corpus prototype/testdata/corpus
```

## Subcommands

| Command   | Purpose                                      |
|-----------|----------------------------------------------|
| `prepare` | Walk corpus, extract, chunk → SQLite         |
| `embed`   | Ollama embeddings for one `model` / `dims`   |
| `query`   | Side-by-side semantic + baselines            |
| `eval`    | Labeled eval + go/no-go report               |
| `status`  | Counts and embedding configs                 |

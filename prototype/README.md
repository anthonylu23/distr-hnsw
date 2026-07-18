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

## Typical workflow (anthonypc)

```bash
export STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
export WORK_DIR="$HOME/distr-hnsw-proto/runs/$STAMP"
mkdir -p "$WORK_DIR"

# 1. Index corpus (extract + chunk)
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  prepare --fresh --corpus /path/to/your/files

# 2. Embed bake-off matrix
for dims in 768 512 384; do
  ./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
    embed --model nomic-embed-text --dims "$dims"
done

# Optional second model (VRAM permitting):
# ollama pull bge-m3
# ./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" embed --model bge-m3 --dims 768

# 3. Explore
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  query --text "spring tax documents" --model nomic-embed-text --dims 768

# 4. Evaluate labeled queries → reports/<stem>.{md,html,json}
./target/release/distr-hnsw-validate --work-dir "$WORK_DIR" \
  eval --queries queries.json \
  --model nomic-embed-text:768 \
  --model nomic-embed-text:512 \
  --model nomic-embed-text:384
```

Copy [`queries.example.json`](queries.example.json) and fill `relevant_path_globs` /
`grades` for your corpus.

Embedding writes persist the Ollama model digest. If a tag changes, `embed` and
`eval` refuse to mix old document vectors with the current query model; rerun
the affected embedding configuration with `--force`.

Evaluation warms each configuration before timing. Reports record one provider
cold start per model plus warmed p50/p95 latency, source-tree and executable
hashes, the query-set hash, preparation fingerprint, and embedding-time model
digests.

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

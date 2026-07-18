#!/usr/bin/env bash
# Run phase-0 bake-off on anthonypc after repo is present and Ollama is up.
set -euo pipefail

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK_DIR="${WORK_DIR:-$HOME/distr-hnsw-proto/runs/${STAMP}}"
CORPUS="${CORPUS:?set CORPUS=/path/to/files}"
QUERIES="${QUERIES:?set QUERIES=/path/to/queries.json}"
BIN="${BIN:-./target/release/distr-hnsw-validate}"
MODEL="${MODEL:-nomic-embed-text}"

mkdir -p "$WORK_DIR"
cargo build --release -p distr-hnsw-validate

"$BIN" --work-dir "$WORK_DIR" prepare --fresh --corpus "$CORPUS"
for dims in 768 512 384; do
  "$BIN" --work-dir "$WORK_DIR" embed --model "$MODEL" --dims "$dims" --concurrency 2
done

# Make the evaluator's first per-model request a real provider cold start. Eval
# records it separately, completes an unreported full sweep, then scores a
# second warmed sweep so provider session initialization cannot bias rankings.
if command -v ollama >/dev/null 2>&1; then
  ollama stop "$MODEL" >/dev/null 2>&1 || true
fi

"$BIN" --work-dir "$WORK_DIR" eval --queries "$QUERIES" \
  --model "${MODEL}:768" \
  --model "${MODEL}:512" \
  --model "${MODEL}:384" \
  --out "bakeoff-${STAMP}"

"$BIN" --work-dir "$WORK_DIR" status
echo "reports in $WORK_DIR/reports"

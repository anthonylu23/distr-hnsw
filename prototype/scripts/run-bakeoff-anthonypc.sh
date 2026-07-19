#!/usr/bin/env bash
# Run phase-0 bake-off on anthonypc after repo is present and Ollama is up.
#
# Default matrix:
#   nomic-embed-text: 768, 512, 384
#   bge-m3:           1024 (native-dimension diagnostic; ineligible for product lock)
# Override MODEL=nomic-embed-text for a nomic-only smoke run.
set -euo pipefail

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK_DIR="${WORK_DIR:-$HOME/distr-hnsw-proto/runs/${STAMP}}"
CORPUS="${CORPUS:?set CORPUS=/path/to/files}"
QUERIES="${QUERIES:?set QUERIES=/path/to/queries.json}"
BIN="${BIN:-./target/release/distr-hnsw-validate}"
# Empty MODEL → full matrix. MODEL=nomic-embed-text → nomic dims only.
MODEL="${MODEL:-}"
NOMIC_EMBED_CONCURRENCY="${NOMIC_EMBED_CONCURRENCY:-2}"
NOMIC_EMBED_BATCH_SIZE="${NOMIC_EMBED_BATCH_SIZE:-8}"
BGE_EMBED_CONCURRENCY="${BGE_EMBED_CONCURRENCY:-1}"
BGE_EMBED_BATCH_SIZE="${BGE_EMBED_BATCH_SIZE:-1}"

mkdir -p "$WORK_DIR"
cargo build --release -p distr-hnsw-validate

configs=()
eval_models=()
if [[ -z "$MODEL" || "$MODEL" == "nomic-embed-text" ]]; then
  for dims in 768 512 384; do
    configs+=("nomic-embed-text:${dims}")
    eval_models+=(--model "nomic-embed-text:${dims}")
  done
fi
if [[ -z "$MODEL" || "$MODEL" == "bge-m3" ]]; then
  configs+=("bge-m3:1024")
  eval_models+=(--model "bge-m3:1024")
fi
if [[ ${#configs[@]} -eq 0 ]]; then
  echo "unknown MODEL=${MODEL@Q}; use empty (full matrix), nomic-embed-text, or bge-m3" >&2
  exit 1
fi

"$BIN" --work-dir "$WORK_DIR" prepare --fresh --corpus "$CORPUS"
for cfg in "${configs[@]}"; do
  model="${cfg%%:*}"
  dims="${cfg##*:}"
  case "$model" in
    nomic-embed-text)
      embed_concurrency="$NOMIC_EMBED_CONCURRENCY"
      embed_batch_size="$NOMIC_EMBED_BATCH_SIZE"
      ;;
    bge-m3)
      embed_concurrency="$BGE_EMBED_CONCURRENCY"
      embed_batch_size="$BGE_EMBED_BATCH_SIZE"
      ;;
  esac
  "$BIN" --work-dir "$WORK_DIR" embed --model "$model" --dims "$dims" \
    --concurrency "$embed_concurrency" --batch-size "$embed_batch_size"
done

# Make the evaluator's first per-model request a real provider cold start. Eval
# records it separately, completes an unreported full sweep, then scores a
# second warmed sweep so provider session initialization cannot bias rankings.
# Stop each distinct model once before eval.
if command -v ollama >/dev/null 2>&1; then
  declare -A stopped=()
  for cfg in "${configs[@]}"; do
    model="${cfg%%:*}"
    if [[ -z "${stopped[$model]+x}" ]]; then
      ollama stop "$model" >/dev/null 2>&1 || true
      stopped[$model]=1
    fi
  done
fi

"$BIN" --work-dir "$WORK_DIR" eval --queries "$QUERIES" \
  "${eval_models[@]}" \
  --out "bakeoff-${STAMP}"

"$BIN" --work-dir "$WORK_DIR" status
echo "reports in $WORK_DIR/reports"

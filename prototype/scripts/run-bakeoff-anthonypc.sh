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
SOURCE_REVISION="${SOURCE_REVISION:-}"
ALLOW_DIRTY_SOURCE="${ALLOW_DIRTY_SOURCE:-0}"
# Empty MODEL → full matrix. MODEL=nomic-embed-text → nomic dims only.
MODEL="${MODEL:-}"
NOMIC_EMBED_CONCURRENCY="${NOMIC_EMBED_CONCURRENCY:-2}"
NOMIC_EMBED_BATCH_SIZE="${NOMIC_EMBED_BATCH_SIZE:-8}"
BGE_EMBED_CONCURRENCY="${BGE_EMBED_CONCURRENCY:-1}"
BGE_EMBED_BATCH_SIZE="${BGE_EMBED_BATCH_SIZE:-1}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
COMPARE_SCRIPT="${SCRIPT_DIR}/compare-eval-reports.py"

mkdir -p "$WORK_DIR"

if [[ -z "$SOURCE_REVISION" ]]; then
  if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    SOURCE_REVISION="$(git rev-parse HEAD)"
    if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
      if [[ "$ALLOW_DIRTY_SOURCE" != "1" ]]; then
        echo "refusing canonical bake-off from a dirty source tree; commit the scoped changes or set ALLOW_DIRTY_SOURCE=1 for a non-canonical diagnostic" >&2
        exit 1
      fi
      SOURCE_REVISION="${SOURCE_REVISION}-dirty"
    fi
  else
    echo "SOURCE_REVISION is required when the remote source tree has no Git metadata" >&2
    exit 1
  fi
fi
if [[ "$SOURCE_REVISION" == "unknown" ]]; then
  echo "SOURCE_REVISION must identify an exact source revision" >&2
  exit 1
fi

DISTR_HNSW_SOURCE_REVISION="$SOURCE_REVISION" cargo build --release -p distr-hnsw-validate
test -x "$BIN"
test -f "$COMPARE_SCRIPT"

ARTIFACT_DIR="$WORK_DIR/artifacts"
mkdir -p "$ARTIFACT_DIR"
RUN_BIN="$ARTIFACT_DIR/distr-hnsw-validate"
install -m 0755 "$BIN" "$RUN_BIN"

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

"$RUN_BIN" --work-dir "$WORK_DIR" prepare --fresh --corpus "$CORPUS"
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
  "$RUN_BIN" --work-dir "$WORK_DIR" embed --model "$model" --dims "$dims" \
    --concurrency "$embed_concurrency" --batch-size "$embed_batch_size"
done

stop_models() {
  if ! command -v ollama >/dev/null 2>&1; then
    return
  fi
  declare -A stopped=()
  for cfg in "${configs[@]}"; do
    model="${cfg%%:*}"
    if [[ -z "${stopped[$model]+x}" ]]; then
      ollama stop "$model" >/dev/null 2>&1 || true
      stopped[$model]=1
    fi
  done
}

# Each evaluator invocation records a provider cold start, completes an
# unreported warm-up sweep, and then scores a warmed sweep. Stop each model
# before both invocations so latency variance is comparable while retrieval
# metrics are checked for exact equality.
stop_models
"$RUN_BIN" --work-dir "$WORK_DIR" eval --queries "$QUERIES" \
  "${eval_models[@]}" \
  --out "bakeoff-${STAMP}"

stop_models
"$RUN_BIN" --work-dir "$WORK_DIR" eval --queries "$QUERIES" \
  "${eval_models[@]}" \
  --out "bakeoff-${STAMP}-repeat"

python3 "$COMPARE_SCRIPT" \
  "$WORK_DIR/reports/bakeoff-${STAMP}.json" \
  "$WORK_DIR/reports/bakeoff-${STAMP}-repeat.json" \
  --out "$WORK_DIR/reports/repeat-comparison.json"

"$RUN_BIN" --work-dir "$WORK_DIR" status
echo "source revision: $SOURCE_REVISION"
echo "reports in $WORK_DIR/reports"

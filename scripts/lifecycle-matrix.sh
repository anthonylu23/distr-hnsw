#!/usr/bin/env bash
# Large storage, corruption, movement, retirement, and garbage-collection
# matrix for the M1 blob plane (roadmap M1 "Delete, repair, and capacity
# safety" evidence). Runs on anthonypc; needs no root.
#
# Usage: scripts/lifecycle-matrix.sh [--files N] [--max-mib M] [--flip K]
#        [--work DIR]
set -euo pipefail

FILES=160
MAX_MIB=24
FLIP=24
WORK_ROOT="$HOME/distr-hnsw-drill"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --files) FILES="$2"; shift 2 ;;
    --max-mib) MAX_MIB="$2"; shift 2 ;;
    --flip) FLIP="$2"; shift 2 ;;
    --work) WORK_ROOT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$WORK_ROOT/matrix-$STAMP"
REPORT="$WORK_ROOT/matrix-report-$STAMP.json"
mkdir -p "$WORK/originals"
log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
now() { date -u +%s.%N; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
PIDS=(); declare -A AGENT_PID
cleanup() { for pid in "${PIDS[@]:-}"; do [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true; done; }
trap cleanup EXIT

(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
SOURCE_REV="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
DB="$WORK/portal.sqlite"; KEY="$WORK/master.key"
"$BIN" portal init --no-recovery-bundle --database "$DB" --master-key "$KEY" >/dev/null

declare -a TARGETS=()
start_agent() {
  local id="$1" domain="$2" port volume
  port="$(free_port)"; volume="$WORK/$id"; mkdir -p "$volume"
  "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" --volume "$volume" >"$WORK/$id.log" 2>&1 &
  PIDS+=("$!"); AGENT_PID["$id"]="$!"
  for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$port/v1/health" >/dev/null && break; sleep 0.05; done
  TARGETS+=("$id,$domain,http://127.0.0.1:$port")
}
args() { local a=(); for t in "${TARGETS[@]}"; do a+=(--agent "$t"); done; printf '%s\n' "${a[@]}"; }
portal() { local cmd="$1"; shift; mapfile -t A < <(args); "$BIN" portal "$cmd" --database "$DB" "${A[@]}" "$@"; }
kportal() { local cmd="$1"; shift; mapfile -t A < <(args); "$BIN" portal "$cmd" --database "$DB" --master-key "$KEY" "${A[@]}" "$@"; }
timed() { local name="$1"; shift; local s; s="$(now)"; "$@"; local f; f="$(now)"; python3 -c "print(round($f-$s,3))" >"$WORK/t-$name"; }
took() { cat "$WORK/t-$1"; }

for i in 1 2 3; do start_agent "agent-$i" "host-$i"; done

# ---- 1. Corpus -----------------------------------------------------------
log "committing $FILES files up to $MAX_MIB MiB"
MANIFEST="$WORK/files.tsv"; : >"$MANIFEST"; TOTAL=0
commit_loop() {
  local i name bytes sha id
  for i in $(seq 1 "$FILES"); do
    name="f-$(printf '%04d' "$i").bin"
    bytes=$(( (RANDOM * 32768 + RANDOM) % (MAX_MIB * 1048576 - 4096) + 4096 ))
    head -c "$bytes" /dev/urandom >"$WORK/originals/$name"
    sha="$(sha256sum "$WORK/originals/$name" | cut -d' ' -f1)"
    id="$(kportal put --idempotency-key "m-$STAMP-$i" "$WORK/originals/$name")"
    printf '%s\t%s\t%s\t%s\n' "$name" "$bytes" "$sha" "$id" >>"$MANIFEST"
  done
}
timed commit commit_loop
TOTAL="$(awk -F'\t' '{s+=$2} END {print s}' "$MANIFEST")"
portal scrub >"$WORK/scrub-0.json"
REQUIRED="$(jq '.totals.required_objects' "$WORK/scrub-0.json")"
log "corpus committed: $TOTAL bytes, $REQUIRED required objects"

# ---- 2. Corruption injection -----------------------------------------------
# Flip one byte in FLIP random required objects spread across agents; also
# truncate a few and delete a few.
python3 - "$WORK" "$FLIP" <<'EOF'
import json, os, random, sys
work, flip = sys.argv[1], int(sys.argv[2])
scrub = json.load(open(f"{work}/scrub-0.json"))
random.seed(7)
objs = []
import sqlite3
db = sqlite3.connect(f"{work}/portal.sqlite")
for kind, h in db.execute("SELECT object_kind, object_hash FROM placements WHERE state='confirmed' GROUP BY 1,2"):
    objs.append((kind, h))
random.shuffle(objs)
chosen = objs[:flip]
log = []
for n, (kind, h) in enumerate(chosen):
    agent = f"agent-{n % 3 + 1}"
    path = f"{work}/{agent}/objects/{kind}/{h[:2]}/{h[2:4]}/{h}"
    mode = ["flip", "truncate", "remove"][n % 3]
    if mode == "flip":
        with open(path, "r+b") as f:
            f.seek(random.randrange(os.path.getsize(path))); b = f.read(1); f.seek(-1, 1); f.write(bytes([b[0] ^ 0x5A]))
    elif mode == "truncate":
        with open(path, "r+b") as f:
            f.truncate(max(0, os.path.getsize(path) // 2))
    else:
        os.remove(path)
    log.append({"agent": agent, "kind": kind, "hash": h, "mode": mode})
json.dump(log, open(f"{work}/injected.json", "w"), indent=1)
EOF
INJECTED="$(jq length "$WORK/injected.json")"
timed scrub-detect portal scrub >"$WORK/scrub-1.json" || true
DETECTED_CORRUPT="$(jq '.totals.corrupt_copies' "$WORK/scrub-1.json")"
DETECTED_MISSING="$(jq '.totals.missing_copies' "$WORK/scrub-1.json")"
timed scrub-repair portal scrub --repair >"$WORK/scrub-2.json" || true
REPAIRED="$(jq '.totals.repairs_applied' "$WORK/scrub-2.json")"
HEALTH_AFTER_REPAIR="$(jq -c '.health' "$WORK/scrub-2.json")"
log "injected $INJECTED, detected corrupt=$DETECTED_CORRUPT missing=$DETECTED_MISSING, repaired $REPAIRED, health $HEALTH_AFTER_REPAIR"

# Downloads never served corrupt bytes: verify a sample now.
verify_sample() {
  local n="$1" ok=0 bad=0
  while IFS=$'\t' read -r name bytes sha id; do
    out="$WORK/get-$name"
    if kportal get "$id" "$out" >/dev/null 2>&1 && [[ "$(sha256sum "$out" | cut -d' ' -f1)" == "$sha" ]]; then ok=$((ok+1)); else bad=$((bad+1)); fi
    rm -f "$out"
  done < <(shuf -n "$n" --random-source=<(yes) "$MANIFEST")
  echo "$ok $bad"
}
read -r S1_OK S1_BAD <<<"$(verify_sample 24)"

# ---- 3. Drain, retire, replace ------------------------------------------------
timed drain portal drain --agent-id agent-3 >"$WORK/drain.json" || true
DRAIN="$(jq -c '.totals' "$WORK/drain.json")"
kill "${AGENT_PID[agent-3]}" 2>/dev/null || true
timed retire portal retire --agent-id agent-3 >"$WORK/retire.json" || true
RETIRED="$(jq -r '.retired_incarnation // "none"' "$WORK/retire.json")"
# Replacement node in a new failure domain; scrub repair restores desired placement.
TARGETS=("${TARGETS[0]}" "${TARGETS[1]}")
start_agent "agent-4" "host-4"
timed rebalance portal scrub --repair >"$WORK/scrub-3.json" || true
REBALANCED="$(jq '.totals.repairs_applied' "$WORK/scrub-3.json")"
HEALTH_AFTER_REBALANCE="$(jq -c '.health' "$WORK/scrub-3.json")"
read -r S2_OK S2_BAD <<<"$(verify_sample 24)"
log "drain $DRAIN; retired $RETIRED; rebalance placed $REBALANCED; health $HEALTH_AFTER_REBALANCE"

# ---- 4. Delete and garbage-collect --------------------------------------------
DELETED=0
while IFS=$'\t' read -r name bytes sha id; do
  kportal delete --idempotency-key "m-$STAMP-del-$id" "$id" >/dev/null; DELETED=$((DELETED+1))
done < <(awk 'NR % 4 == 0' "$MANIFEST")
USED_BEFORE=$(du -sb "$WORK/agent-1/objects" "$WORK/agent-2/objects" "$WORK/agent-4/objects" | awk '{s+=$1} END {print s}')
timed gc-blocked portal gc --apply --retention-seconds 0 --staging-grace-seconds 0 >"$WORK/gc-0.json" || true
GC_BLOCKED_BEFORE_SCAN="$(jq '.totals.blocked' "$WORK/gc-0.json")"
portal scrub >"$WORK/scrub-4.json" || true
timed gc-plan portal gc --retention-seconds 0 --staging-grace-seconds 0 >"$WORK/gc-1.json" || true
timed gc-apply portal gc --apply --retention-seconds 0 --staging-grace-seconds 0 >"$WORK/gc-2.json" || true
GC="$(jq -c '.totals' "$WORK/gc-2.json")"
USED_AFTER=$(du -sb "$WORK/agent-1/objects" "$WORK/agent-2/objects" "$WORK/agent-4/objects" | awk '{s+=$1} END {print s}')
portal scrub >"$WORK/scrub-5.json" || true
FINAL_HEALTH="$(jq -c '.health' "$WORK/scrub-5.json")"
read -r S3_OK S3_BAD <<<"$(awk 'NR % 4 != 0' "$MANIFEST" > "$WORK/kept.tsv"; MANIFEST="$WORK/kept.tsv" verify_sample 24)"
DELETED_READABLE=0
while IFS=$'\t' read -r name bytes sha id; do
  if kportal get "$id" "$WORK/zombie" >/dev/null 2>&1; then DELETED_READABLE=$((DELETED_READABLE+1)); rm -f "$WORK/zombie"; fi
done < <(awk 'NR % 4 == 0' "$MANIFEST")
log "deleted $DELETED; gc before scan blocked=$GC_BLOCKED_BEFORE_SCAN; gc $GC; bytes $USED_BEFORE -> $USED_AFTER; health $FINAL_HEALTH"
cleanup; PIDS=()

VERDICT=pass
[[ "$DETECTED_CORRUPT" -ge $((INJECTED * 2 / 3)) ]] || VERDICT=fail          # flips + truncations
[[ "$DETECTED_MISSING" -ge $((INJECTED / 3)) ]] || VERDICT=fail
[[ "$(jq '.health.at_risk + .health.lost + .health.degraded' "$WORK/scrub-2.json")" == 0 ]] || VERDICT=fail
[[ "$RETIRED" != none ]] || VERDICT=fail
[[ "$(jq '.health.at_risk + .health.lost + .health.degraded' "$WORK/scrub-3.json")" == 0 ]] || VERDICT=fail
[[ "$GC_BLOCKED_BEFORE_SCAN" -gt 0 ]] || VERDICT=fail
[[ "$(jq '.totals.applied' "$WORK/gc-2.json")" -gt 0 ]] || VERDICT=fail
[[ "$USED_AFTER" -lt "$USED_BEFORE" ]] || VERDICT=fail
[[ "$(jq '.health.at_risk + .health.lost + .health.degraded' "$WORK/scrub-5.json")" == 0 ]] || VERDICT=fail
[[ "$S1_BAD$S2_BAD$S3_BAD" == 000 && "$DELETED_READABLE" == 0 ]] || VERDICT=fail

jq -n --arg stamp "$STAMP" --arg rev "$SOURCE_REV" --arg host "$(hostname)" --arg kernel "$(uname -r)" --arg verdict "$VERDICT" \
  --argjson files "$FILES" --argjson bytes "$TOTAL" --argjson required "$REQUIRED" --argjson injected "$INJECTED" \
  --argjson corrupt "$DETECTED_CORRUPT" --argjson missing "$DETECTED_MISSING" --argjson repaired "$REPAIRED" \
  --argjson health_repair "$HEALTH_AFTER_REPAIR" --argjson drain "$DRAIN" --arg retired "$RETIRED" \
  --argjson rebalanced "$REBALANCED" --argjson health_rebalance "$HEALTH_AFTER_REBALANCE" \
  --argjson deleted "$DELETED" --argjson gc_blocked_before_scan "$GC_BLOCKED_BEFORE_SCAN" --argjson gc "$GC" \
  --argjson used_before "$USED_BEFORE" --argjson used_after "$USED_AFTER" --argjson final_health "$FINAL_HEALTH" \
  --argjson s1 "[$S1_OK,$S1_BAD]" --argjson s2 "[$S2_OK,$S2_BAD]" --argjson s3 "[$S3_OK,$S3_BAD]" --argjson zombies "$DELETED_READABLE" \
  --argjson t_commit "$(took commit)" --argjson t_detect "$(took scrub-detect)" --argjson t_repair "$(took scrub-repair)" \
  --argjson t_drain "$(took drain)" --argjson t_retire "$(took retire)" --argjson t_rebalance "$(took rebalance)" \
  --argjson t_gc_plan "$(took gc-plan)" --argjson t_gc_apply "$(took gc-apply)" \
  --slurpfile injected_log "$WORK/injected.json" \
  '{report_type:"LifecycleMatrixReportV1", version:1, stamp:$stamp, host:$host, kernel:$kernel, source_revision:$rev, verdict:$verdict,
    corpus:{files:$files, bytes:$bytes, required_objects:$required, agents:3},
    corruption:{injected:$injected, by_mode:($injected_log[0] | group_by(.mode) | map({key:.[0].mode, value:length}) | from_entries),
                detected_corrupt:$corrupt, detected_missing:$missing, repairs_applied:$repaired, health_after_repair:$health_repair,
                sample_downloads_ok:$s1[0], sample_downloads_bad:$s1[1]},
    movement:{drain:$drain, retired_incarnation:$retired, replacement_repairs:$rebalanced, health_after_rebalance:$health_rebalance,
              sample_downloads_ok:$s2[0], sample_downloads_bad:$s2[1]},
    gc:{deleted_files:$deleted, blocked_before_observation:$gc_blocked_before_scan, totals:$gc,
        bytes_before:$used_before, bytes_after:$used_after, final_health:$final_health,
        sample_downloads_ok:$s3[0], sample_downloads_bad:$s3[1], deleted_files_readable:$zombies},
    seconds:{commit:$t_commit, scrub_detect:$t_detect, scrub_repair:$t_repair, drain:$t_drain, retire:$t_retire,
             rebalance:$t_rebalance, gc_plan:$t_gc_plan, gc_apply:$t_gc_apply}}' >"$REPORT"
log "verdict: $VERDICT  report: $REPORT"
jq '{verdict, corpus, corruption, movement, gc, seconds}' "$REPORT" >&2
[[ "$VERDICT" == pass ]]

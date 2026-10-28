#!/usr/bin/env bash
# Large storage, corruption, movement, retirement, and garbage-collection
# matrix for the M1 blob plane (roadmap M1 "Delete, repair, and capacity
# safety" evidence). Runs on the reference host; needs no root. Section 5 covers the
# orders the 2026-10-25 audit found broken: retirement after GC, and GC after
# the portal database is replaced by a snapshot taken mid-upload.
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
# Agent routes need a grant, so readiness is the listening port, not /v1/health.
wait_port() {
  for _ in $(seq 1 100); do (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && return 0; sleep 0.05; done
  return 1
}
PIDS=(); declare -A AGENT_PID
cleanup() { for pid in "${PIDS[@]:-}"; do [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true; done; }
trap cleanup EXIT

# The failpoints feature lets section 5 stop an upload mid-flight.
(cd "$REPO" && cargo build --release -p distr-hnsw --features failpoints >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
SOURCE_REV="$(git -C "$REPO" describe --always --dirty --abbrev=40 2>/dev/null || echo unknown)"
DB="$WORK/portal.sqlite"; KEY="$WORK/master.key"
"$BIN" portal init --no-recovery-bundle --database "$DB" --master-key "$KEY" >/dev/null

declare -a TARGETS=()
# Start an agent with a single-use join token and join it. The token stays in
# the environment, never in a file or log.
start_agent() {
  local id="$1" domain="$2" port volume token target
  port="$(free_port)"; volume="$WORK/$id"; mkdir -p "$volume"
  token="$("$BIN" portal agent token --database "$DB" --master-key "$KEY" --agent-id "$id")"
  DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" \
    --insecure-loopback --volume "$volume" >"$WORK/$id.log" 2>&1 &
  PIDS+=("$!"); AGENT_PID["$id"]="$!"
  wait_port "$port" || { log "agent $id did not start: $(tail -3 "$WORK/$id.log")"; exit 1; }
  target="$id,$domain,http://127.0.0.1:$port"
  DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" portal agent join --database "$DB" --master-key "$KEY" \
    --agent "$target" >"$WORK/$id.join.json"
  TARGETS+=("$target")
}
args() { local a=(); for t in "${TARGETS[@]}"; do a+=(--agent "$t"); done; printf '%s\n' "${a[@]}"; }
# Every command that contacts agents needs the master key: it unseals the
# key that signs agent grants.
portal() { local cmd="$1"; shift; mapfile -t A < <(args); "$BIN" portal "$cmd" --database "$DB" --master-key "$KEY" "${A[@]}" "$@"; }
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
    id="$(portal put --idempotency-key "m-$STAMP-$i" "$WORK/originals/$name")"
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
    if portal get "$id" "$out" >/dev/null 2>&1 && [[ "$(sha256sum "$out" | cut -d' ' -f1)" == "$sha" ]]; then ok=$((ok+1)); else bad=$((bad+1)); fi
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
  portal delete --idempotency-key "m-$STAMP-del-$id" "$id" >/dev/null; DELETED=$((DELETED+1))
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
  if portal get "$id" "$WORK/zombie" >/dev/null 2>&1; then DELETED_READABLE=$((DELETED_READABLE+1)); rm -f "$WORK/zombie"; fi
done < <(awk 'NR % 4 == 0' "$MANIFEST")
log "deleted $DELETED; gc before scan blocked=$GC_BLOCKED_BEFORE_SCAN; gc $GC; bytes $USED_BEFORE -> $USED_AFTER; health $FINAL_HEALTH"

# ---- 5. Orders after GC ---------------------------------------------------------
# 5a. Retirement after GC: collected objects must not count as at risk.
start_agent "agent-5" "host-5"
portal drain --agent-id agent-4 >"$WORK/drain-after-gc.json" || true
kill "${AGENT_PID[agent-4]}" 2>/dev/null || true
timed retire-after-gc portal retire --agent-id agent-4 >"$WORK/retire-after-gc.json" || true
RETIRED_AFTER_GC="$(jq -r '.retired_incarnation // "none"' "$WORK/retire-after-gc.json")"
TARGETS=("${TARGETS[0]}" "${TARGETS[1]}" "${TARGETS[3]}")
portal scrub --repair >"$WORK/scrub-6.json" || true

# 5b. GC after a stale metadata restore: snapshot while an upload is in flight,
# let the upload commit, then put the snapshot back. GC must collect nothing
# until recovery reconciles the metadata, and the acknowledged file survives.
head -c 3000000 /dev/urandom >"$WORK/originals/late.bin"
LATE_SHA="$(sha256sum "$WORK/originals/late.bin" | cut -d' ' -f1)"
DISTR_HNSW_FAILPOINT=after_chunks_durable portal put --idempotency-key "m-$STAMP-late" "$WORK/originals/late.bin" >/dev/null 2>&1 || true
python3 -c "import sqlite3,sys; sqlite3.connect(sys.argv[1]).execute('VACUUM INTO ?', (sys.argv[2],))" "$DB" "$WORK/stale.sqlite"
LATE_ID="$(portal put --idempotency-key "m-$STAMP-late" "$WORK/originals/late.bin")"
rm -f "$DB" "$DB-wal" "$DB-shm"; cp "$WORK/stale.sqlite" "$DB"
portal scrub >"$WORK/scrub-7.json" || true
portal gc --apply --retention-seconds 0 --staging-grace-seconds 0 >"$WORK/gc-stale.json" || true
GC_STALE_APPLIED="$(jq '.totals.applied' "$WORK/gc-stale.json")"
GC_STALE_UNKNOWN="$(jq '[.candidates[] | select(any(.blockers[]; test("unknown to the metadata")))] | length' "$WORK/gc-stale.json")"
portal recover --apply >"$WORK/recover-stale.json" || true
RECOVER_STALE_BLOCKED="$(jq '.totals.blocked' "$WORK/recover-stale.json")"
LATE_OK=0
if portal get "$LATE_ID" "$WORK/late-out.bin" >/dev/null 2>&1 \
   && [[ "$(sha256sum "$WORK/late-out.bin" | cut -d' ' -f1)" == "$LATE_SHA" ]]; then LATE_OK=1; fi
log "retired after gc: $RETIRED_AFTER_GC; stale-restore gc applied=$GC_STALE_APPLIED unknown-blocked=$GC_STALE_UNKNOWN; recover blocked=$RECOVER_STALE_BLOCKED; late file intact=$LATE_OK"
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
[[ "$RETIRED_AFTER_GC" != none ]] || VERDICT=fail
[[ "$GC_STALE_APPLIED" == 0 && "$GC_STALE_UNKNOWN" -gt 0 ]] || VERDICT=fail
[[ "$RECOVER_STALE_BLOCKED" == 0 && "$LATE_OK" == 1 ]] || VERDICT=fail

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
  --arg retired_after_gc "$RETIRED_AFTER_GC" --argjson gc_stale_applied "$GC_STALE_APPLIED" \
  --argjson gc_stale_unknown "$GC_STALE_UNKNOWN" --argjson recover_stale_blocked "$RECOVER_STALE_BLOCKED" \
  --argjson late_ok "$LATE_OK" --argjson t_retire_after_gc "$(took retire-after-gc)" \
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
    after_gc:{retired_incarnation:$retired_after_gc, stale_restore_gc_applied:$gc_stale_applied,
              stale_restore_candidates_blocked_by_unknown_objects:$gc_stale_unknown,
              stale_restore_recover_blocked:$recover_stale_blocked, acknowledged_file_intact:($late_ok == 1)},
    seconds:{commit:$t_commit, scrub_detect:$t_detect, scrub_repair:$t_repair, drain:$t_drain, retire:$t_retire,
             rebalance:$t_rebalance, gc_plan:$t_gc_plan, gc_apply:$t_gc_apply, retire_after_gc:$t_retire_after_gc}}' >"$REPORT"
log "verdict: $VERDICT  report: $REPORT"
jq '{verdict, corpus, corruption, movement, gc, after_gc, seconds}' "$REPORT" >&2
[[ "$VERDICT" == pass ]]

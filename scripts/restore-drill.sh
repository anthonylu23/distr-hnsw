#!/usr/bin/env bash
# Representative empty-infrastructure restore drill (DESIGN §11.1, roadmap M1
# exit gate). Builds a cluster, commits and deletes files, backs up, destroys
# every cluster artifact, and rebuilds from the backup set, the recovery
# bundle, and an off-cluster passphrase. Records declared vs. actual RPO/RTO,
# source and restored hashes, and every operator step in a JSON report.
#
# Usage: scripts/restore-drill.sh [--files N] [--max-mib M] [--target SPEC]
#        [--work DIR] [--agents 3]
#   --target defaults to dir:<work>/offsite; pass s3:<bucket>/<prefix> with
#   the usual DISTR_HNSW_S3_ENDPOINT / AWS_* environment for an offsite run.
# Needs no root. Everything lives under --work (default ~/distr-hnsw-drill).
set -euo pipefail

FILES=64
MAX_MIB=8
TARGET=""
WORK_ROOT="$HOME/distr-hnsw-drill"
AGENT_COUNT=3
while [[ $# -gt 0 ]]; do
  case "$1" in
    --files) FILES="$2"; shift 2 ;;
    --max-mib) MAX_MIB="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --work) WORK_ROOT="$2"; shift 2 ;;
    --agents) AGENT_COUNT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$WORK_ROOT/restore-$STAMP"
CLUSTER="$WORK/cluster"
ORIGINALS="$WORK/originals"
RESTORED="$WORK/restored"
REPORT="$WORK_ROOT/restore-report-$STAMP.json"
STEPS="$WORK/steps.jsonl"
mkdir -p "$CLUSTER" "$ORIGINALS" "$RESTORED"
[[ -n "$TARGET" ]] || TARGET="dir:$WORK/offsite"

# Declared targets (DESIGN §11.1 defaults).
DECLARED_METADATA_RPO=300
DECLARED_BLOB_RPO=900
DECLARED_PORTAL_RTO=600

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
now() { date -u +%s.%N; }
step() {
  local name="$1" started="$2" finished="$3" detail="${4:-}"
  jq -cn --arg n "$name" --arg s "$started" --arg f "$finished" --arg d "$detail" \
    '{step:$n, started:($s|tonumber), finished:($f|tonumber), seconds:(($f|tonumber)-($s|tonumber)), detail:$d}' >> "$STEPS"
}
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

t0="$(now)"
log "building release binary"
(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
SOURCE_REV="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
step build "$t0" "$(now)" "$SOURCE_REV"

start_agents() {
  # $1 = root dir for volumes, $2 = name of array variable to fill with targets
  local root="$1"; local -n out="$2"; out=()
  for i in $(seq 1 "$AGENT_COUNT"); do
    local id="agent-$i" domain="host-$i" port volume
    port="$(free_port)"; volume="$root/$id"; mkdir -p "$volume"
    "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" --volume "$volume" \
      >"$root/$id.log" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$port/v1/health" >/dev/null && break; sleep 0.05; done
    out+=("$id,$domain,http://127.0.0.1:$port")
  done
}
agent_args() { local a=(); for t in "$@"; do a+=(--agent "$t"); done; printf '%s\n' "${a[@]}"; }

# ---- 1. Cluster and key ceremony -------------------------------------------
t="$(now)"
declare -a AGENTS
start_agents "$CLUSTER" AGENTS
DB="$CLUSTER/portal.sqlite"; KEY="$CLUSTER/master.key"
PASSPHRASE="$(head -c 24 /dev/urandom | base32 | tr -d '=')"
INIT_OUT="$WORK/init.out"
printf '%s\n' "$PASSPHRASE" | "$BIN" portal init --passphrase-stdin --database "$DB" --master-key "$KEY" >"$INIT_OUT"
# The bundle and passphrase are the only recovery material kept off-cluster.
sed -n '/-----BEGIN DISTR-HNSW RECOVERY BUNDLE-----/,/-----END DISTR-HNSW RECOVERY BUNDLE-----/p' "$INIT_OUT" >"$WORK/bundle.txt"
printf '%s\n' "$PASSPHRASE" >"$WORK/passphrase.txt"; chmod 600 "$WORK/passphrase.txt"
KEY_ID="$("$BIN" portal key show-id --master-key "$KEY")"
step init "$t" "$(now)" "key id $KEY_ID"

# ---- 2. Commit and delete files --------------------------------------------
t="$(now)"
mapfile -t AGENT_ARGS < <(agent_args "${AGENTS[@]}")
MANIFEST="$WORK/files.tsv"   # name<TAB>bytes<TAB>sha256<TAB>file_id<TAB>kept|deleted
: >"$MANIFEST"
TOTAL_BYTES=0
for i in $(seq 1 "$FILES"); do
  name="file-$(printf '%04d' "$i").bin"
  bytes=$(( (RANDOM * 32768 + RANDOM) % (MAX_MIB * 1048576 - 1024) + 1024 ))
  head -c "$bytes" /dev/urandom >"$ORIGINALS/$name"
  sha="$(sha256sum "$ORIGINALS/$name" | cut -d' ' -f1)"
  file_id="$("$BIN" portal put --database "$DB" --master-key "$KEY" "${AGENT_ARGS[@]}" \
    --idempotency-key "drill-$STAMP-$i" "$ORIGINALS/$name")"
  TOTAL_BYTES=$((TOTAL_BYTES + bytes))
  fate=kept
  if (( i % 8 == 0 )); then
    "$BIN" portal delete --database "$DB" --master-key "$KEY" "${AGENT_ARGS[@]}" \
      --idempotency-key "drill-$STAMP-del-$i" "$file_id" >/dev/null
    fate=deleted
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$bytes" "$sha" "$file_id" "$fate" >>"$MANIFEST"
done
LAST_COMMIT_AT="$(now)"
step commit "$t" "$LAST_COMMIT_AT" "$FILES files, $TOTAL_BYTES bytes, every 8th deleted"

# ---- 3. Scrub and backup ---------------------------------------------------
t="$(now)"
"$BIN" portal scrub --database "$DB" "${AGENT_ARGS[@]}" >"$WORK/scrub-before.json"
BACKUP1="$WORK/backup-1.json"; BACKUP2="$WORK/backup-2.json"
"$BIN" portal backup --database "$DB" "${AGENT_ARGS[@]}" --target "$TARGET" >"$BACKUP1"
"$BIN" portal backup --database "$DB" "${AGENT_ARGS[@]}" --target "$TARGET" >"$BACKUP2"
BACKUP_DONE_AT="$(now)"
"$BIN" portal health --database "$DB" >"$WORK/health-before.json"
step backup "$t" "$BACKUP_DONE_AT" "$(jq -c '.totals' "$BACKUP1")"
HISTORY_OBJECTS="$(jq '.totals.history_objects' "$BACKUP1")"
SNAPSHOT_NAME="$(jq -r '.snapshot.name' "$BACKUP1")"
BACKUP_PENDING="$(jq '.backup.targets[0].objects_pending' "$WORK/health-before.json")"
# Actual recovery point: the last committed change is fully offsite when the
# backup completes; RPO actual = backup completion - last commit.
ACTUAL_RPO="$(python3 -c "print(round($BACKUP_DONE_AT - $LAST_COMMIT_AT, 3))")"

# ---- 4. Total loss --------------------------------------------------------
cleanup; PIDS=()
LOSS_AT="$(now)"
rm -rf "$CLUSTER"
step destroy "$LOSS_AT" "$(now)" "agents killed; database, key, and volumes removed"

# ---- 5. Rebuild from backup, bundle, and passphrase -----------------------
t="$(now)"
NEW_KEY="$RESTORED/master.key"; NEW_DB="$RESTORED/portal.sqlite"
"$BIN" portal key restore --bundle "$WORK/bundle.txt" --master-key "$NEW_KEY" <"$WORK/passphrase.txt" >"$WORK/key-restore.out"
step key-restore "$t" "$(now)" "$(tail -1 "$WORK/key-restore.out")"
t="$(now)"
"$BIN" portal restore metadata --target "$TARGET" --database "$NEW_DB" >"$WORK/restore-metadata.json"
step restore-metadata "$t" "$(now)" "$(jq -r '.snapshot.name' "$WORK/restore-metadata.json")"
t="$(now)"
declare -a NEW_AGENTS
start_agents "$RESTORED" NEW_AGENTS
mapfile -t NEW_AGENT_ARGS < <(agent_args "${NEW_AGENTS[@]}")
"$BIN" portal restore objects --target "$TARGET" "${NEW_AGENT_ARGS[@]}" >"$WORK/restore-objects.json"
step restore-objects "$t" "$(now)" "$(jq -c '{objects,placed,failed}' "$WORK/restore-objects.json")"
t="$(now)"
"$BIN" portal recover --database "$NEW_DB" --master-key "$NEW_KEY" "${NEW_AGENT_ARGS[@]}" --apply >"$WORK/recover.json"
RESTORED_AT="$(now)"
step recover "$t" "$RESTORED_AT" "$(jq -c '.totals' "$WORK/recover.json")"
# Files are readable from here: recovery time ends at converged recovery.
ACTUAL_RTO="$(python3 -c "print(round($RESTORED_AT - $LOSS_AT, 3))")"
# Restore placed the floor (two domains); desired placement across every
# configured agent is restored by scrub repair, copy-first.
t="$(now)"
"$BIN" portal scrub --repair --database "$NEW_DB" "${NEW_AGENT_ARGS[@]}" >"$WORK/scrub-repair.json" || true
step scrub-repair "$t" "$(now)" "$(jq -c '{repairs_applied: .totals.repairs_applied, health}' "$WORK/scrub-repair.json")"

# ---- 6. Verify ------------------------------------------------------------
t="$(now)"
VERIFIED=0; MISMATCH=0; DELETED_OK=0; DELETED_BAD=0
while IFS=$'\t' read -r name bytes sha file_id fate; do
  out="$RESTORED/out-$name"
  if [[ "$fate" == kept ]]; then
    if "$BIN" portal get --database "$NEW_DB" --master-key "$NEW_KEY" "${NEW_AGENT_ARGS[@]}" "$file_id" "$out" >/dev/null 2>&1 \
       && [[ "$(sha256sum "$out" | cut -d' ' -f1)" == "$sha" ]]; then
      VERIFIED=$((VERIFIED + 1))
    else
      MISMATCH=$((MISMATCH + 1)); log "MISMATCH $name ($file_id)"
    fi
    rm -f "$out"
  else
    if "$BIN" portal get --database "$NEW_DB" --master-key "$NEW_KEY" "${NEW_AGENT_ARGS[@]}" "$file_id" "$out" >/dev/null 2>&1; then
      DELETED_BAD=$((DELETED_BAD + 1)); log "RESURRECTED $name ($file_id)"
    else
      DELETED_OK=$((DELETED_OK + 1))
    fi
  fi
done <"$MANIFEST"
"$BIN" portal scrub --database "$NEW_DB" "${NEW_AGENT_ARGS[@]}" >"$WORK/scrub-after.json" || true
"$BIN" portal health --database "$NEW_DB" >"$WORK/health-after.json" || true
step verify "$t" "$(now)" "$VERIFIED verified, $MISMATCH mismatched, $DELETED_OK deleted stay unreadable, $DELETED_BAD resurrected"
cleanup; PIDS=()

KEPT=$(( FILES - FILES / 8 ))
VERDICT=fail
if (( MISMATCH == 0 && DELETED_BAD == 0 && VERIFIED == KEPT )) \
   && [[ "$(jq '.totals.blocked' "$WORK/recover.json")" == 0 ]] \
   && [[ "$(jq '.health.at_risk + .health.lost + .health.degraded' "$WORK/scrub-after.json")" == 0 ]]; then
  VERDICT=pass
fi

jq -n \
  --arg stamp "$STAMP" --arg rev "$SOURCE_REV" --arg kernel "$(uname -r)" --arg host "$(hostname)" \
  --arg target "$TARGET" --arg target_kind "${TARGET%%:*}" --arg verdict "$VERDICT" --arg key_id "$KEY_ID" \
  --argjson files "$FILES" --argjson bytes "$TOTAL_BYTES" --argjson kept "$KEPT" --argjson agents "$AGENT_COUNT" \
  --argjson history "$HISTORY_OBJECTS" --arg snapshot "$SNAPSHOT_NAME" --argjson pending "$BACKUP_PENDING" \
  --argjson declared_metadata_rpo "$DECLARED_METADATA_RPO" --argjson declared_blob_rpo "$DECLARED_BLOB_RPO" \
  --argjson declared_portal_rto "$DECLARED_PORTAL_RTO" --argjson actual_rpo "$ACTUAL_RPO" --argjson actual_rto "$ACTUAL_RTO" \
  --argjson verified "$VERIFIED" --argjson mismatched "$MISMATCH" --argjson deleted_ok "$DELETED_OK" --argjson resurrected "$DELETED_BAD" \
  --slurpfile steps "$STEPS" --slurpfile backup "$BACKUP1" --slurpfile backup2 "$BACKUP2" \
  --slurpfile restore_objects "$WORK/restore-objects.json" --slurpfile recover "$WORK/recover.json" \
  --slurpfile scrub_after "$WORK/scrub-after.json" --slurpfile health_after "$WORK/health-after.json" \
  '{
    report_type: "RestoreDrillReportV1", version: 1, stamp: $stamp, host: $host, kernel: $kernel,
    source_revision: $rev, master_key_id: $key_id, target: $target, target_kind: $target_kind, verdict: $verdict,
    corpus: {files: $files, bytes: $bytes, kept: $kept, deleted: ($files - $kept), agents: $agents,
             history_objects: $history},
    backup: {first: $backup[0].totals, second: $backup2[0].totals, snapshot: $snapshot,
             snapshot_skipped_on_second_run: $backup2[0].snapshot_skipped_identical, objects_pending_before_loss: $pending},
    objectives: {declared_metadata_rpo_seconds: $declared_metadata_rpo, declared_blob_rpo_seconds: $declared_blob_rpo,
                 declared_portal_loss_rto_seconds: $declared_portal_rto,
                 actual_recovery_point_lag_seconds: $actual_rpo, actual_recovery_time_seconds: $actual_rto,
                 within_targets: ($actual_rpo <= $declared_blob_rpo and $actual_rto <= $declared_portal_rto)},
    restore: {objects: $restore_objects[0], recover_totals: $recover[0].totals,
              scrub_health_after: $scrub_after[0].health, backup_status_after: $health_after[0].backup},
    verification: {kept_verified: $verified, mismatched: $mismatched, deleted_unreadable: $deleted_ok,
                   resurrected: $resurrected, missing_control_metadata: []},
    operator_steps: $steps
  }' >"$REPORT"

log "verdict: $VERDICT  report: $REPORT"
jq '{verdict, corpus, objectives, verification}' "$REPORT" >&2
[[ "$VERDICT" == pass ]]

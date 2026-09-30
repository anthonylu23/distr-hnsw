#!/usr/bin/env bash
# enospc-drill.sh -- capacity admission and true-ENOSPC failure injection for
# the distr-hnsw agent object store.
#
# Usage: scripts/enospc-drill.sh [--keep] [--max-puts N]
#
# What it does (see docs/m1-capacity-drill.md):
#   1. Creates a 96 MiB sparse image under $HOME/distr-hnsw-drill, attaches it
#      as a loop device, runs mkfs.ext4, mounts it, and chowns the mount so the
#      agent runs unprivileged. Agent A serves a volume on that filesystem
#      with --reserve-bytes 8 MiB --hard-floor-bytes 1 MiB and no quota;
#      agent B (the second failure domain RF2 needs) serves a plain directory.
#   2. Phase 1, admission: uploads 4 MiB files with distinct idempotency keys
#      until `portal put` exits 3. Records the capacity state trajectory, the
#      exit code, the agent's direct HTTP 507, that scrub sees exactly two
#      required objects per admitted file (one chunk, one manifest), that a
#      deletion marker still lands (control objects may use the reserve), and
#      that the refused key is refused identically on retry.
#   3. Phase 2, true ENOSPC: stops agent A, fallocates a filler so that exactly
#      ceil((4 MiB + 16 B) / block size) blocks stay free -- byte-exact
#      admission passes, block-granular allocation cannot -- restarts A with
#      reserve and hard floor 0 on the same volume (same incarnation), and
#      attempts a put. Expects exit 3, an `enospc` limiting factor,
#      capacity.state == enospc_observed, and no leftover .*.tmp file.
#   4. Phase 3, recovery: removes the filler, scrubs, runs
#      `portal gc --apply --retention-seconds 0 --staging-grace-seconds 0`,
#      checks that the agent's used_bytes drops, then retries both refused
#      keys and verifies the downloads byte-for-byte.
#   5. Writes $HOME/distr-hnsw-drill/report-enospc-<utc>.json.
#
# Safety: the only block device this script touches is the loop device
# attached to the image it created; `losetup -j` is printed and checked
# before every privileged operation. The mount point is
# $HOME/distr-hnsw-drill/mnt-enospc-<pid>. An EXIT trap kills the agents,
# unmounts, detaches the loop device, and prints leftover counts from
# `losetup -a` and `mount`.
#
# Requirements: passwordless sudo, mkfs.ext4, losetup, fallocate, jq, curl,
# sha256sum, and target/release/distr-hnsw.

set -euo pipefail

KEEP=0
MAX_PUTS=40
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        --max-puts) MAX_PUTS="$2"; shift 2 ;;
        *) echo "usage: $0 [--keep] [--max-puts N]" >&2; exit 2 ;;
    esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DISTR_HNSW_BIN:-$REPO/target/release/distr-hnsw}"
DRILL="${DRILL_DIR:-$HOME/distr-hnsw-drill}"
UTC="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$DRILL/run-enospc-$UTC"
IMG="$RUN/enospc.img"
IMG_SIZE="${IMG_SIZE:-96M}"
MNT="$DRILL/mnt-enospc-$$"
PORT_BASE=$((18000 + ($$ % 1000) * 4))
REPORT="$DRILL/report-enospc-$UTC.json"

RESERVE_BYTES=8388608      # 8 MiB: regular chunks may not consume this
HARD_FLOOR_BYTES=1048576   # 1 MiB: control objects may not consume this
FILE_SIZE=4194304          # one 4 MiB chunk per file
CHUNK_CIPHERTEXT=$((FILE_SIZE + 16))   # AEAD tag; what admission is charged

mkdir -p "$DRILL" "$RUN"
log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RUN/drill.log" >&2; }
die() { log "FATAL: $*"; exit 1; }

[ -x "$BIN" ] || die "binary not found: $BIN (run: cargo build --release -p distr-hnsw)"
for tool in mkfs.ext4 losetup fallocate jq curl sha256sum stat findmnt; do
    command -v "$tool" >/dev/null || die "missing tool: $tool"
done
sudo -n true 2>/dev/null || die "passwordless sudo required"

# --- state for cleanup -------------------------------------------------------
DEV=""
AGENT_PIDS=()

# Print and verify that $1 is a loop device attached to image $2 that we made.
check_loop() {
    local dev="$1" img="$2" attached
    attached="$(losetup -j "$img" | cut -d: -f1)"
    log "device check: $dev must be loop for $img (losetup -j: ${attached:-none})"
    case "$dev" in /dev/loop[0-9]*) ;; *) die "refusing: $dev is not a loop device" ;; esac
    [ "$attached" = "$dev" ] || die "refusing: $dev is not attached to $img"
}

cleanup() {
    local rc=$?
    set +e
    log "cleanup starting (rc=$rc)"
    for pid in "${AGENT_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null && wait "$pid" 2>/dev/null
    done
    if mountpoint -q "$MNT" 2>/dev/null; then sudo umount "$MNT" || sudo umount -l "$MNT"; fi
    if [ -n "$DEV" ]; then
        check_loop "$DEV" "$IMG" && sudo losetup -d "$DEV"
    fi
    rmdir "$MNT" 2>/dev/null
    if [ "$KEEP" = 0 ]; then rm -f "$IMG"; fi
    log "cleanup verification: losetup -a | grep $IMG -> $(losetup -a | grep -c "$IMG" || true) entries; mount | grep $MNT -> $(mount | grep -c "$MNT" || true) entries"
    log "cleanup verification: mount | grep drill -> $(mount | grep -c drill || true) entries (any drill mount, including other drills)"
    exit $rc
}
trap cleanup EXIT

# --- helpers ----------------------------------------------------------------
wait_health() {
    local port="$1" i
    for i in $(seq 1 100); do
        if curl -fsS "http://127.0.0.1:$port/v1/health" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    return 1
}

# Starts an agent in the background; the PID is left in $LAST_PID (not
# echoed, so AGENT_PIDS is updated in this shell, not a subshell).
LAST_PID=""
start_agent() {
    local id="$1" domain="$2" port="$3" volume="$4" logfile="$5"
    shift 5
    "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" --volume "$volume" "$@" >>"$logfile" 2>&1 &
    LAST_PID=$!
    AGENT_PIDS+=("$LAST_PID")
}

health_a() { curl -fsS "http://127.0.0.1:$PORT_A/v1/health"; }
cap_a() { health_a | jq -c .capacity; }
cap_field() { cap_a | jq -r ".$1"; }
fs_avail_bytes() { echo $(( $(stat -f -c %a "$MNT") * $(stat -f -c %S "$MNT") )); }
tmp_files() { find "$VOL_A" -name '.*.tmp' 2>/dev/null; }

# Number of objects an agent lists in one namespace (strict pagination).
inv_count() {
    local port="$1" kind="$2" after="" url page n total=0
    while :; do
        url="http://127.0.0.1:$port/v1/inventory/$kind"
        [ -n "$after" ] && url="$url?after=$after"
        page="$(curl -fsS "$url")" || return 1
        n="$(jq '.objects | length' <<<"$page")"
        total=$((total + n))
        after="$(jq -r '.next_after // empty' <<<"$page")"
        [ -z "$after" ] && break
    done
    echo "$total"
}

# Direct agent PUT with an all-zero hash. Admission runs before the hash is
# verified, so 507 means refused by capacity and 409 means admitted (then
# rejected on hash mismatch before any write). Nothing is stored either way.
probe_put() {
    local port="$1" kind="$2" size="$3"
    local zero="0000000000000000000000000000000000000000000000000000000000000000"
    head -c "$size" /dev/zero > "$RUN/probe.bin"
    curl -s -o "$RUN/probe-$kind-body.json" -w '%{http_code}' -X PUT \
        --data-binary @"$RUN/probe.bin" "http://127.0.0.1:$port/v1/objects/$kind/$zero"
}

PUT_RC=0; PUT_OUT=""; PUT_ERR=""
portal_put() {
    local key="$1" source="$2"
    set +e
    PUT_OUT="$("$BIN" portal put --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" \
        --agent "$AGENT_A" --agent "$AGENT_B" --idempotency-key "$key" "$source" 2>"$RUN/put-stderr.txt")"
    PUT_RC=$?
    set -e
    PUT_ERR="$(cat "$RUN/put-stderr.txt")"
    printf '%s\n' "--- put key=$key rc=$PUT_RC" "$PUT_ERR" >> "$RUN/portal-put.log"
}

CMD_RC=0; CMD_OUT=""
portal_run() {
    local name="$1"; shift
    set +e
    CMD_OUT="$("$BIN" portal "$@" 2>"$RUN/$name-stderr.txt")"
    CMD_RC=$?
    set -e
    printf '%s\n' "$CMD_OUT" > "$RUN/$name.json"
}

: > "$RUN/checks.jsonl"
declare -A PHASE_FAIL=([setup]=0 [phase1]=0 [phase2]=0 [phase3]=0)
check() {
    local phase="$1" name="$2" ok="$3" expected="$4" observed="$5"
    [ "$ok" = true ] || PHASE_FAIL[$phase]=1
    jq -nc --arg p "$phase" --arg n "$name" --argjson ok "$ok" --arg e "$expected" --arg o "$observed" \
        '{phase:$p, check:$n, pass:$ok, expected:$e, observed:$o}' >> "$RUN/checks.jsonl"
    log "check[$phase] $name: $([ "$ok" = true ] && echo PASS || echo FAIL) expected=[$expected] observed=[$observed]"
}
bool() { if "$@"; then echo true; else echo false; fi; }

T0=$(date +%s)
log "drill start run=$RUN mount=$MNT ports from $PORT_BASE"

# --- 0. loop device, mkfs.ext4, mount -----------------------------------------
truncate -s "$IMG_SIZE" "$IMG"
DEV="$(sudo losetup --find --show "$IMG")"
check_loop "$DEV" "$IMG"
MKFS_VERSION="$(mkfs.ext4 -V 2>&1 || true)"
MKFS_VERSION="${MKFS_VERSION%%$'\n'*}"
sudo mkfs.ext4 -q "$DEV"
mkdir -p "$MNT"
check_loop "$DEV" "$IMG"
sudo mount "$DEV" "$MNT"
sudo chown "$(id -u):$(id -g)" "$MNT"
MOUNT_OPTS="$(findmnt -no OPTIONS "$MNT")"
BLOCK="$(stat -f -c %S "$MNT")"
FS_TOTAL=$(( $(stat -f -c %b "$MNT") * BLOCK ))
FS_AVAIL_START="$(fs_avail_bytes)"
log "mounted $DEV at $MNT ($MOUNT_OPTS); block=$BLOCK total=$FS_TOTAL avail=$FS_AVAIL_START"

# --- agents and portal --------------------------------------------------------
VOL_A="$MNT/agent-a"
VOL_B="$RUN/agent-b"
PORTAL="$RUN/portal"
mkdir -p "$VOL_A" "$VOL_B" "$PORTAL"
PORT_A=$PORT_BASE
PORT_B=$((PORT_BASE + 1))
AGENT_A="a,host-a,http://127.0.0.1:$PORT_A"
AGENT_B="b,host-b,http://127.0.0.1:$PORT_B"
start_agent a host-a "$PORT_A" "$VOL_A" "$RUN/agent-a.log" \
    --reserve-bytes "$RESERVE_BYTES" --hard-floor-bytes "$HARD_FLOOR_BYTES"; PID_A=$LAST_PID
start_agent b host-b "$PORT_B" "$VOL_B" "$RUN/agent-b.log"; PID_B=$LAST_PID
wait_health "$PORT_A" || die "agent a did not become healthy: $(tail -3 "$RUN/agent-a.log")"
wait_health "$PORT_B" || die "agent b did not become healthy"
INCARNATION_ORIG="$(health_a | jq -r .incarnation_id)"
CAP_INITIAL="$(cap_a)"
log "agent a pid=$PID_A port=$PORT_A incarnation=$INCARNATION_ORIG capacity=$CAP_INITIAL"
"$BIN" portal init --no-recovery-bundle --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" >"$RUN/portal-init.log" 2>&1 \
    || die "portal init failed: $(cat "$RUN/portal-init.log")"
check setup initial_capacity_state "$(bool [ "$(jq -r .state <<<"$CAP_INITIAL")" = ok ])" "ok" "$(jq -r .state <<<"$CAP_INITIAL")"
check setup reserve_and_floor_applied \
    "$(bool [ "$(jq -r .reserve_bytes <<<"$CAP_INITIAL")" = "$RESERVE_BYTES" -a "$(jq -r .hard_floor_bytes <<<"$CAP_INITIAL")" = "$HARD_FLOOR_BYTES" ])" \
    "reserve=$RESERVE_BYTES floor=$HARD_FLOOR_BYTES" \
    "reserve=$(jq -r .reserve_bytes <<<"$CAP_INITIAL") floor=$(jq -r .hard_floor_bytes <<<"$CAP_INITIAL")"

# --- 1. admission ---------------------------------------------------------------
log "phase 1: uploading ${FILE_SIZE}-byte files until portal put exits 3"
: > "$RUN/admitted.jsonl"
ADMITTED=0; REFUSED_KEY=""; REFUSED_SRC=""; REFUSED_ERR=""; REFUSED_RC=""; FIRST_WARNING=0; FIRST_PAUSED=0
declare -a FILE_IDS=()
for i in $(seq 1 "$MAX_PUTS"); do
    src="$RUN/file-$i.bin"
    head -c "$FILE_SIZE" /dev/urandom > "$src"
    key="enospc-$UTC-admit-$i"
    portal_put "$key" "$src"
    cap="$(cap_a)"
    state="$(jq -r .state <<<"$cap")"
    if [ "$PUT_RC" = 0 ]; then
        ADMITTED=$((ADMITTED + 1))
        FILE_IDS+=("$PUT_OUT")
        [ "$FIRST_WARNING" = 0 ] && [ "$state" = warning ] && FIRST_WARNING=$i
        [ "$FIRST_PAUSED" = 0 ] && [ "$state" = admission_paused ] && FIRST_PAUSED=$i
        jq -nc --argjson i "$i" --arg fid "$PUT_OUT" --argjson cap "$cap" \
            '{put:$i, file_id:$fid, state:$cap.state, fs_free_bytes:$cap.fs_free_bytes, effective_free_bytes:$cap.effective_free_bytes, used_bytes:$cap.used_bytes}' \
            >> "$RUN/admitted.jsonl"
        log "put $i admitted file_id=$PUT_OUT state=$state fs_free=$(jq -r .fs_free_bytes <<<"$cap") effective=$(jq -r .effective_free_bytes <<<"$cap")"
    else
        REFUSED_KEY="$key"; REFUSED_SRC="$src"; REFUSED_ERR="$PUT_ERR"; REFUSED_RC="$PUT_RC"
        log "put $i refused rc=$PUT_RC state=$state stderr=$PUT_ERR"
        break
    fi
done
CAP_AFTER_REFUSAL="$(cap_a)"
STATE_AFTER="$(jq -r .state <<<"$CAP_AFTER_REFUSAL")"
[ -n "$REFUSED_KEY" ] || die "no refusal after $MAX_PUTS puts; raise --max-puts or shrink IMG_SIZE"

check phase1 admitted_at_least_one "$(bool [ "$ADMITTED" -gt 0 ])" ">0" "$ADMITTED"
check phase1 refusal_exit_code "$(bool [ "$REFUSED_RC" = 3 ])" "3" "$REFUSED_RC"
check phase1 refusal_message_names_capacity "$(bool grep -qi 'insufficient capacity' <<<"$REFUSED_ERR")" "stderr contains 'insufficient capacity'" "$REFUSED_ERR"
check phase1 state_before "$(bool [ "$(jq -r .state <<<"$CAP_INITIAL")" = ok ])" "ok" "$(jq -r .state <<<"$CAP_INITIAL")"
check phase1 state_after "$(bool [ "$STATE_AFTER" = warning -o "$STATE_AFTER" = admission_paused ])" "warning|admission_paused" "$STATE_AFTER"
check phase1 last_refusal_at_unset_by_portal_precheck true "informational" "last_refusal_at=$(jq -r .last_refusal_at <<<"$CAP_AFTER_REFUSAL") (portal refused before contacting the agent)"

# Direct agent probes: a chunk must be refused with 507; a manifest-sized
# control object must pass admission (409 = hash mismatch after admission).
PROBE_CHUNK="$(probe_put "$PORT_A" chunk "$CHUNK_CIPHERTEXT")"
PROBE_CHUNK_BODY="$(cat "$RUN/probe-chunk-body.json")"
PROBE_MANIFEST="$(probe_put "$PORT_A" manifest 1024)"
CAP_AFTER_PROBE="$(cap_a)"
check phase1 agent_direct_chunk_put_507 "$(bool [ "$PROBE_CHUNK" = 507 ])" "507" "$PROBE_CHUNK $PROBE_CHUNK_BODY"
check phase1 agent_507_body_code "$(bool [ "$(jq -r .code <<<"$PROBE_CHUNK_BODY" 2>/dev/null)" = insufficient_capacity ])" "insufficient_capacity" "$(jq -c '{code,limiting,volume_id}' <<<"$PROBE_CHUNK_BODY" 2>/dev/null || echo "$PROBE_CHUNK_BODY")"
check phase1 agent_direct_manifest_put_admitted "$(bool [ "$PROBE_MANIFEST" = 409 ])" "409 (admitted via control headroom, rejected on hash)" "$PROBE_MANIFEST"
check phase1 last_refusal_at_set_by_agent_507 "$(bool [ "$(jq -r .last_refusal_at <<<"$CAP_AFTER_PROBE")" != null ])" "non-null" "$(jq -r .last_refusal_at <<<"$CAP_AFTER_PROBE")"

INV_A_CHUNK="$(inv_count "$PORT_A" chunk)"; INV_A_MANIFEST="$(inv_count "$PORT_A" manifest)"; INV_A_MARKER="$(inv_count "$PORT_A" deletion_marker)"
TMP_P1="$(tmp_files | wc -l)"
check phase1 agent_a_inventory "$(bool [ "$INV_A_CHUNK" = "$ADMITTED" -a "$INV_A_MANIFEST" = "$ADMITTED" -a "$INV_A_MARKER" = 0 ])" \
    "chunks=$ADMITTED manifests=$ADMITTED markers=0" "chunks=$INV_A_CHUNK manifests=$INV_A_MANIFEST markers=$INV_A_MARKER"
check phase1 no_tmp_files_after_refusal "$(bool [ "$TMP_P1" = 0 ])" "0" "$TMP_P1"

portal_run scrub-p1 scrub --database "$PORTAL/portal.db" --agent "$AGENT_A" --agent "$AGENT_B"
SCRUB_P1_REQUIRED="$(jq -r .totals.required_objects <<<"$CMD_OUT")"
SCRUB_P1_HEALTH="$(jq -c .health <<<"$CMD_OUT")"
check phase1 scrub_required_objects "$(bool [ "$SCRUB_P1_REQUIRED" = "$((ADMITTED * 2))" -a "$CMD_RC" = 0 ])" \
    "required=$((ADMITTED * 2)) exit=0" "required=$SCRUB_P1_REQUIRED exit=$CMD_RC health=$SCRUB_P1_HEALTH"
portal_run health-p1 health --database "$PORTAL/portal.db"
HEALTH_P1="$(jq -c '{health, unhealthy:(.unhealthy_objects|length)}' <<<"$CMD_OUT")"
check phase1 portal_health_no_partial_file "$(bool [ "$(jq -r '.health.durable' <<<"$CMD_OUT")" = "$((ADMITTED * 2))" -a "$(jq -r '.unhealthy_objects|length' <<<"$CMD_OUT")" = 0 ])" \
    "durable=$((ADMITTED * 2)) unhealthy=0" "$HEALTH_P1"

# Deletion marker while admission is paused: control objects may use the reserve.
DELETED_FID="${FILE_IDS[0]}"
portal_run delete-p1 delete --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" \
    --agent "$AGENT_A" --agent "$AGENT_B" --idempotency-key "enospc-$UTC-delete-1" "$DELETED_FID"
DELETE_RC=$CMD_RC; MARKER_HASH="$CMD_OUT"
INV_A_MARKER="$(inv_count "$PORT_A" deletion_marker)"
CAP_AFTER_DELETE="$(cap_a)"
check phase1 delete_succeeds_under_paused_admission "$(bool [ "$DELETE_RC" = 0 -a "$INV_A_MARKER" = 1 ])" \
    "exit=0 markers_on_a=1" "exit=$DELETE_RC markers_on_a=$INV_A_MARKER marker=$MARKER_HASH state=$(jq -r .state <<<"$CAP_AFTER_DELETE") stderr=$(cat "$RUN/delete-p1-stderr.txt")"

# Retrying the refused key must be refused again, identically.
portal_put "$REFUSED_KEY" "$REFUSED_SRC"
check phase1 retry_refused_key_same_result "$(bool [ "$PUT_RC" = 3 -a "$PUT_ERR" = "$REFUSED_ERR" ])" \
    "exit=3 same stderr" "exit=$PUT_RC same_stderr=$([ "$PUT_ERR" = "$REFUSED_ERR" ] && echo yes || echo no)"
log "phase 1 done: admitted=$ADMITTED first_warning_at=$FIRST_WARNING first_paused_at=$FIRST_PAUSED"

# --- 2. true ENOSPC ---------------------------------------------------------------
log "phase 2: stopping agent a, filling the filesystem from outside"
kill "$PID_A"; wait "$PID_A" 2>/dev/null || true
# Leave exactly the number of blocks a chunk's ciphertext rounds up to:
# admission (byte-exact) passes, but the write also needs the new fanout
# directory blocks, so the kernel must return ENOSPC.
KEEP_BLOCKS=$(( (CHUNK_CIPHERTEXT + BLOCK - 1) / BLOCK ))
AVAIL_BEFORE_FILL="$(fs_avail_bytes)"
FILLER_BYTES=$(( AVAIL_BEFORE_FILL - KEEP_BLOCKS * BLOCK ))
fallocate -l "$FILLER_BYTES" "$MNT/filler.bin"
AVAIL_AFTER_FILL="$(fs_avail_bytes)"
while [ "$AVAIL_AFTER_FILL" -lt "$CHUNK_CIPHERTEXT" ]; do
    truncate -s "-$BLOCK" "$MNT/filler.bin"
    AVAIL_AFTER_FILL="$(fs_avail_bytes)"
done
log "filler=$FILLER_BYTES bytes; avail before=$AVAIL_BEFORE_FILL after=$AVAIL_AFTER_FILL (target $((KEEP_BLOCKS * BLOCK)); chunk ciphertext $CHUNK_CIPHERTEXT)"

PORT_A=$((PORT_BASE + 2))
AGENT_A="a,host-a,http://127.0.0.1:$PORT_A"
start_agent a host-a "$PORT_A" "$VOL_A" "$RUN/agent-a.log" --reserve-bytes 0 --hard-floor-bytes 0; PID_A=$LAST_PID
wait_health "$PORT_A" || die "agent a did not restart: $(tail -3 "$RUN/agent-a.log")"
INCARNATION_P2="$(health_a | jq -r .incarnation_id)"
CAP_P2_BEFORE="$(cap_a)"
USED_P2_BEFORE="$(jq -r .used_bytes <<<"$CAP_P2_BEFORE")"
check phase2 same_incarnation_after_restart "$(bool [ "$INCARNATION_P2" = "$INCARNATION_ORIG" ])" "$INCARNATION_ORIG" "$INCARNATION_P2"
check phase2 admission_passes_before_put "$(bool [ "$(jq -r .effective_free_bytes <<<"$CAP_P2_BEFORE")" -ge "$CHUNK_CIPHERTEXT" ])" \
    "effective_free_bytes >= $CHUNK_CIPHERTEXT" "$CAP_P2_BEFORE"

ENOSPC_KEY="enospc-$UTC-enospc"
ENOSPC_SRC="$RUN/file-enospc.bin"
head -c "$FILE_SIZE" /dev/urandom > "$ENOSPC_SRC"
portal_put "$ENOSPC_KEY" "$ENOSPC_SRC"
ENOSPC_RC=$PUT_RC; ENOSPC_ERR=$PUT_ERR
CAP_P2_AFTER="$(cap_a)"
TMP_P2_LIST="$(tmp_files || true)"
TMP_P2="$(printf '%s' "$TMP_P2_LIST" | grep -c . || true)"
INV_B_CHUNK_P2="$(inv_count "$PORT_B" chunk)"
INV_A_CHUNK_P2="$(inv_count "$PORT_A" chunk)"
LIMITING="$(grep -o 'limiting: [a-z_]*' <<<"$ENOSPC_ERR" | head -1 | cut -d' ' -f2 || true)"
HTTP_SEEN="$(grep -o 'HTTP [0-9]\{3\}' <<<"$ENOSPC_ERR" | head -1 | tr -dc 0-9 || true)"
if [ -z "$HTTP_SEEN" ] && [ "$ENOSPC_RC" = 3 ] && [ "$LIMITING" = enospc ]; then
    HTTP_SEEN="507 (portal maps only a 507 body with limiting=enospc to this error)"
elif [ -z "$HTTP_SEEN" ]; then
    HTTP_SEEN="unknown"
fi
check phase2 put_exit_code "$(bool [ "$ENOSPC_RC" = 3 ])" "3" "$ENOSPC_RC"
check phase2 stderr_mentions_capacity_and_enospc "$(bool grep -qi 'capacity' <<<"$ENOSPC_ERR" && grep -qi 'enospc' <<<"$ENOSPC_ERR")" \
    "insufficient capacity ... limiting: enospc" "$ENOSPC_ERR"
check phase2 agent_http_status "$(bool [ "${HTTP_SEEN%% *}" = 507 ])" "507" "$HTTP_SEEN"
check phase2 state_enospc_observed "$(bool [ "$(jq -r .state <<<"$CAP_P2_AFTER")" = enospc_observed ])" "enospc_observed" "$(jq -r .state <<<"$CAP_P2_AFTER")"
check phase2 last_refusal_at_set "$(bool [ "$(jq -r .last_refusal_at <<<"$CAP_P2_AFTER")" != null ])" "non-null" "$(jq -r .last_refusal_at <<<"$CAP_P2_AFTER")"
check phase2 no_tmp_file_leaked "$(bool [ "$TMP_P2" = 0 ])" "0" "$TMP_P2 ${TMP_P2_LIST:-}"
check phase2 used_bytes_unchanged_by_failed_put "$(bool [ "$(jq -r .used_bytes <<<"$CAP_P2_AFTER")" = "$USED_P2_BEFORE" ])" "$USED_P2_BEFORE" "$(jq -r .used_bytes <<<"$CAP_P2_AFTER")"
check phase2 chunk_not_on_agent_a "$(bool [ "$INV_A_CHUNK_P2" = "$ADMITTED" ])" "chunks_on_a=$ADMITTED" "chunks_on_a=$INV_A_CHUNK_P2 chunks_on_b=$INV_B_CHUNK_P2"
log "phase 2 done: rc=$ENOSPC_RC limiting=${LIMITING:-none} http=$HTTP_SEEN state=$(jq -r .state <<<"$CAP_P2_AFTER") tmp=$TMP_P2"

# --- 3. recovery of space -------------------------------------------------------------
log "phase 3: removing filler, scrub, gc --apply, retry refused keys"
rm -f "$MNT/filler.bin"
AVAIL_AFTER_UNFILL="$(fs_avail_bytes)"
portal_run scrub-p3 scrub --database "$PORTAL/portal.db" --agent "$AGENT_A" --agent "$AGENT_B"
SCRUB_P3_RC=$CMD_RC
USED_BEFORE_GC="$(cap_field used_bytes)"
portal_run gc-p3 gc --database "$PORTAL/portal.db" --agent "$AGENT_A" --agent "$AGENT_B" \
    --apply --retention-seconds 0 --staging-grace-seconds 0
GC_RC=$CMD_RC
GC_TOTALS="$(jq -c .totals <<<"$CMD_OUT")"
GC_CANDIDATES="$(jq -c '[.candidates[] | {kind, status, reason, blockers}]' <<<"$CMD_OUT")"
USED_AFTER_GC="$(cap_field used_bytes)"
check phase3 gc_applied_deleted_generation "$(bool [ "$(jq -r .applied <<<"$GC_TOTALS")" -ge 2 ])" "applied>=2 (chunk+manifest of the deleted file)" "exit=$GC_RC totals=$GC_TOTALS"
check phase3 used_bytes_dropped "$(bool [ "$USED_AFTER_GC" -le $((USED_BEFORE_GC - CHUNK_CIPHERTEXT)) ])" \
    "drop >= $CHUNK_CIPHERTEXT" "before=$USED_BEFORE_GC after=$USED_AFTER_GC drop=$((USED_BEFORE_GC - USED_AFTER_GC))"
CAP_P3="$(cap_a)"

portal_put "$REFUSED_KEY" "$REFUSED_SRC"
RETRY1_RC=$PUT_RC; RETRY1_FID="$PUT_OUT"; RETRY1_ERR="$PUT_ERR"
portal_put "$ENOSPC_KEY" "$ENOSPC_SRC"
RETRY2_RC=$PUT_RC; RETRY2_FID="$PUT_OUT"; RETRY2_ERR="$PUT_ERR"
check phase3 refused_key_converges "$(bool [ "$RETRY1_RC" = 0 ])" "exit=0" "exit=$RETRY1_RC file_id=$RETRY1_FID $RETRY1_ERR"
check phase3 enospc_key_converges "$(bool [ "$RETRY2_RC" = 0 ])" "exit=0" "exit=$RETRY2_RC file_id=$RETRY2_FID $RETRY2_ERR"
for pair in "1:$RETRY1_FID:$REFUSED_SRC" "2:$RETRY2_FID:$ENOSPC_SRC"; do
    n="${pair%%:*}"; rest="${pair#*:}"; fid="${rest%%:*}"; src="${rest#*:}"
    if [ -n "$fid" ]; then
        portal_run "get-p3-$n" get --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" \
            --agent "$AGENT_A" --agent "$AGENT_B" "$fid" "$RUN/download-$n.bin"
        want="$(sha256sum "$src" | cut -d' ' -f1)"; got="$(sha256sum "$RUN/download-$n.bin" 2>/dev/null | cut -d' ' -f1 || true)"
        check phase3 "download_${n}_hash_matches" "$(bool [ "$CMD_RC" = 0 -a "$want" = "$got" ])" "$want" "exit=$CMD_RC sha256=${got:-none}"
    else
        check phase3 "download_${n}_hash_matches" false "downloadable" "no file id"
    fi
done
portal_run scrub-final scrub --database "$PORTAL/portal.db" --agent "$AGENT_A" --agent "$AGENT_B"
EXPECTED_REQUIRED=$(( (ADMITTED - 1) * 2 + 1 + 4 ))
FINAL_REQUIRED="$(jq -r .totals.required_objects <<<"$CMD_OUT")"
check phase3 final_scrub_durable "$(bool [ "$CMD_RC" = 0 -a "$FINAL_REQUIRED" = "$EXPECTED_REQUIRED" ])" \
    "exit=0 required=$EXPECTED_REQUIRED" "exit=$CMD_RC required=$FINAL_REQUIRED health=$(jq -c .health <<<"$CMD_OUT")"
TMP_P3="$(tmp_files | wc -l)"
check phase3 no_tmp_files_final "$(bool [ "$TMP_P3" = 0 ])" "0" "$TMP_P3"
CAP_FINAL="$(cap_a)"
log "phase 3 done: final capacity=$CAP_FINAL"

# --- report ---------------------------------------------------------------------------
T1=$(date +%s)
verdict() { [ "${PHASE_FAIL[$1]}" = 0 ] && echo pass || echo FAIL; }
jq -n \
    --arg utc "$UTC" --arg host "$(hostname)" --arg kernel "$(uname -r)" --arg mkfs "$MKFS_VERSION" \
    --arg mount_opts "$MOUNT_OPTS" --arg dev "$DEV" --arg img_size "$IMG_SIZE" \
    --argjson block "$BLOCK" --argjson fs_total "$FS_TOTAL" --argjson fs_avail_start "$FS_AVAIL_START" \
    --argjson reserve "$RESERVE_BYTES" --argjson floor "$HARD_FLOOR_BYTES" --argjson file_size "$FILE_SIZE" \
    --arg incarnation "$INCARNATION_ORIG" --arg bin_sha "$(sha256sum "$BIN" | cut -d' ' -f1)" \
    --arg source_rev "$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
    --argjson admitted "$ADMITTED" --argjson first_warning "$FIRST_WARNING" --argjson first_paused "$FIRST_PAUSED" \
    --argjson cap_initial "$CAP_INITIAL" --argjson cap_after_refusal "$CAP_AFTER_REFUSAL" --argjson cap_after_delete "$CAP_AFTER_DELETE" \
    --arg refused_key "$REFUSED_KEY" --argjson refused_rc "$REFUSED_RC" --arg refused_err "$REFUSED_ERR" \
    --argjson probe_chunk "$PROBE_CHUNK" --arg probe_chunk_body "$PROBE_CHUNK_BODY" --argjson probe_manifest "$PROBE_MANIFEST" \
    --argjson scrub_p1_required "$SCRUB_P1_REQUIRED" --arg marker "$MARKER_HASH" \
    --argjson keep_blocks "$KEEP_BLOCKS" --argjson filler "$FILLER_BYTES" --argjson avail_before_fill "$AVAIL_BEFORE_FILL" --argjson avail_after_fill "$AVAIL_AFTER_FILL" \
    --argjson cap_p2_before "$CAP_P2_BEFORE" --argjson cap_p2_after "$CAP_P2_AFTER" \
    --argjson enospc_rc "$ENOSPC_RC" --arg enospc_err "$ENOSPC_ERR" --arg limiting "${LIMITING:-}" --arg http_seen "$HTTP_SEEN" \
    --argjson tmp_p2 "$TMP_P2" --arg tmp_p2_list "$TMP_P2_LIST" \
    --argjson avail_after_unfill "$AVAIL_AFTER_UNFILL" --argjson scrub_p3_rc "$SCRUB_P3_RC" --argjson gc_rc "$GC_RC" \
    --argjson gc_totals "$GC_TOTALS" --argjson gc_candidates "$GC_CANDIDATES" \
    --argjson used_before_gc "$USED_BEFORE_GC" --argjson used_after_gc "$USED_AFTER_GC" \
    --argjson retry1_rc "$RETRY1_RC" --arg retry1_fid "$RETRY1_FID" --argjson retry2_rc "$RETRY2_RC" --arg retry2_fid "$RETRY2_FID" \
    --argjson final_required "$FINAL_REQUIRED" --argjson cap_final "$CAP_FINAL" \
    --arg v_setup "$(verdict setup)" --arg v1 "$(verdict phase1)" --arg v2 "$(verdict phase2)" --arg v3 "$(verdict phase3)" \
    --argjson seconds "$((T1 - T0))" \
    --slurpfile admitted_puts "$RUN/admitted.jsonl" --slurpfile checks "$RUN/checks.jsonl" \
    '{drill:"enospc", started_utc:$utc, host:$host, kernel:$kernel, filesystem:"ext4", mkfs_version:$mkfs,
      mount_options:$mount_opts, device:$dev, image_size:$img_size, block_size:$block, fs_total_bytes:$fs_total,
      fs_avail_start_bytes:$fs_avail_start, binary_sha256:$bin_sha, source_rev:$source_rev,
      agent_a:{reserve_bytes:$reserve, hard_floor_bytes:$floor, quota_bytes:null, incarnation:$incarnation},
      file_size:$file_size,
      phase1:{verdict:$v1, admitted:$admitted, first_warning_at_put:$first_warning, first_admission_paused_at_put:$first_paused,
              capacity_initial:$cap_initial, capacity_after_refusal:$cap_after_refusal, capacity_after_delete:$cap_after_delete,
              refused_key:$refused_key, refused_exit:$refused_rc, refused_stderr:$refused_err,
              agent_probe:{chunk_http:$probe_chunk, chunk_body:$probe_chunk_body, manifest_http:$probe_manifest},
              scrub_required_objects:$scrub_p1_required, deletion_marker:$marker, puts:$admitted_puts},
      phase2:{verdict:$v2, keep_blocks:$keep_blocks, filler_bytes:$filler, avail_before_fill:$avail_before_fill, avail_after_fill:$avail_after_fill,
              capacity_before_put:$cap_p2_before, capacity_after_put:$cap_p2_after, put_exit:$enospc_rc, put_stderr:$enospc_err,
              limiting:$limiting, agent_http_status:$http_seen, leftover_tmp_files:$tmp_p2, leftover_tmp_list:$tmp_p2_list},
      phase3:{verdict:$v3, avail_after_unfill:$avail_after_unfill, scrub_exit:$scrub_p3_rc, gc_exit:$gc_rc, gc_totals:$gc_totals, gc_candidates:$gc_candidates,
              used_bytes_before_gc:$used_before_gc, used_bytes_after_gc:$used_after_gc,
              retry_refused_key:{exit:$retry1_rc, file_id:$retry1_fid}, retry_enospc_key:{exit:$retry2_rc, file_id:$retry2_fid},
              final_scrub_required_objects:$final_required, capacity_final:$cap_final},
      setup_verdict:$v_setup, checks:$checks,
      verdict:(if $v_setup == "pass" and $v1 == "pass" and $v2 == "pass" and $v3 == "pass" then "all-phases-pass" else "FAIL" end),
      wall_clock_seconds:$seconds,
      caveats:["ext4 on a loop device over a sparse image on btrfs /home",
               "phase 2 leaves exactly ceil(chunk_ciphertext / block_size) blocks free so byte-exact admission passes while block-granular allocation cannot; a coarser fill is refused by admission and never reaches the kernel",
               "capacity.state stays enospc_observed for 15 minutes after the last ENOSPC regardless of freed space; admission itself is arithmetic",
               "agent A keeps reserve 0 / hard floor 0 through phase 3"]}' > "$REPORT"
log "report: $REPORT"
log "RESULT setup=$(verdict setup) phase1=$(verdict phase1) (admitted=$ADMITTED) phase2=$(verdict phase2) (rc=$ENOSPC_RC http=$HTTP_SEEN state=$(jq -r .state <<<"$CAP_P2_AFTER") tmp=$TMP_P2) phase3=$(verdict phase3) (gc=$GC_TOTALS) in $((T1 - T0))s"
[ "$(jq -r .verdict "$REPORT")" = all-phases-pass ]

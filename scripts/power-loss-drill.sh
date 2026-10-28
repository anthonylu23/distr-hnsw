#!/usr/bin/env bash
# power-loss-drill.sh -- dm-log-writes power-loss qualification for the
# distr-hnsw agent object store.
#
# Usage: scripts/power-loss-drill.sh <btrfs|ext4|xfs> [--files N] [--keep]
#
# What it does (see docs/m1-filesystem-qualification.md):
#   1. Creates two sparse images under $HOME/distr-hnsw-drill, attaches them
#      as loop devices, stacks a dm-log-writes target on the data device,
#      runs mkfs.<fs> on it and mounts it with default options.
#   2. Inits a portal DB outside the mount with its master key on the mount
#      (mark `master-key`), starts one agent on the dm-backed volume (a
#      second agent on a plain directory supplies the second failure domain
#      RF2 requires), each with a single-use join token, joins both, uploads
#      N files, and after every acknowledged put inserts a log mark and
#      snapshots the agent inventory.
#   2b. Re-PUT: restarts agent a with DISTR_HNSW_AGENT_FAILPOINT=
#      put_after_rename, so the next PUT exits after its rename and before
#      its directory sync, unacknowledged. Restarts agent a normally,
#      re-PUTs the same object, and marks `reput-ack` once that PUT is
#      acknowledged by the already-present path.
#   3. kill -9 the agent, marks `end`, unmounts, removes the dm device.
#   4. For every put mark: discards the data device, replays the log up to
#      the mark, mounts the data device (journal recovery runs as it would
#      after a real restart), starts a fresh agent on the replayed volume
#      (it needs no token: its trust is on the volume) and
#      checks that every inventory entry recorded at that mark answers
#      GET /v1/objects/{kind}/{hash} with 200 (the agent re-hashes on GET).
#      Every agent request is granted: it goes through `portal agent request`
#      with the portal database (off the dm volume) and an off-volume copy of
#      the master key. The join completes before the first `put-<i>` mark, so
#      every replay that starts an agent includes its durable trust files.
#      Then unmounts and runs the filesystem's read-only checker. The same
#      check runs at `reput-ack`; at `master-key` the key file must match
#      the key written by `portal init` byte for byte.
#   5. Writes $HOME/distr-hnsw-drill/report-<fs>-<utc>.json.
#
# Safety: only loop devices attached to images this script created are ever
# passed to dmsetup/mkfs/mount/blkdiscard/replay-log; each is verified with
# `losetup -j <image>` immediately before use. Device-mapper names are
# dhq-<fs>-<pid>; mount points live under $HOME/distr-hnsw-drill/mnt-*.
# An EXIT trap tears everything down and the script verifies cleanup.
#
# Requirements: passwordless sudo, dm-log-writes module, mkfs.<fs>, jq,
# gcc + a shallow xfstests clone for replay-log (built automatically into
# $HOME/distr-hnsw-drill/replay-log), and target/release/distr-hnsw built
# with failpoints: cargo build --release -p distr-hnsw --features failpoints

set -euo pipefail

FS="${1:-}"
shift || true
FILES=8
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --files) FILES="$2"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
case "$FS" in
    btrfs|ext4|xfs) ;;
    *) echo "usage: $0 <btrfs|ext4|xfs> [--files N] [--keep]" >&2; exit 2 ;;
esac

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DISTR_HNSW_BIN:-$REPO/target/release/distr-hnsw}"
DRILL="${DRILL_DIR:-$HOME/distr-hnsw-drill}"
UTC="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$DRILL/run-$FS-$UTC"
IMG_SIZE="${IMG_SIZE:-4G}"
DM_NAME="dhq-$FS-$$"
DM_DEV="/dev/mapper/$DM_NAME"
MNT="$DRILL/mnt-$FS-$$"
RMNT="$DRILL/mnt-replay-$FS-$$"
PORT_BASE=$((17000 + ($$ % 1000) * 20))
REPORT="$DRILL/report-$FS-$UTC.json"
REPLAY="$DRILL/replay-log"
XFSTESTS="$DRILL/xfstests-dev"

mkdir -p "$DRILL" "$RUN"
log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RUN/drill.log" >&2; }
die() { log "FATAL: $*"; exit 1; }

[ -x "$BIN" ] || die "binary not found: $BIN (run: cargo build --release -p distr-hnsw)"
for tool in "mkfs.$FS" dmsetup losetup blockdev blkdiscard jq gcc; do
    command -v "$tool" >/dev/null || die "missing tool: $tool"
done
# A failpoints build rejects an unknown agent failpoint by name; a build
# without failpoints refuses any.
FP_PROBE="$(DISTR_HNSW_AGENT_FAILPOINT=probe "$BIN" agent --id probe --failure-domain probe \
    --bind 127.0.0.1:1 --insecure-loopback --volume "$DRILL/failpoint-probe" 2>&1 || true)"
rm -rf "$DRILL/failpoint-probe"
case "$FP_PROBE" in
    *"unknown agent failpoint"*) ;;
    *) die "binary lacks failpoints (run: cargo build --release -p distr-hnsw --features failpoints)" ;;
esac
sudo -n true 2>/dev/null || die "passwordless sudo required"
sudo modprobe dm-log-writes || die "cannot load dm-log-writes"

# --- replay-log from xfstests ------------------------------------------------
if [ ! -x "$REPLAY" ]; then
    if [ ! -d "$XFSTESTS" ]; then
        log "cloning xfstests (shallow) for replay-log"
        git clone --depth 1 https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git "$XFSTESTS" \
            || die "xfstests clone failed; see docs for the dm-flakey fallback"
    fi
    gcc -O2 -o "$REPLAY" "$XFSTESTS/src/log-writes/replay-log.c" "$XFSTESTS/src/log-writes/log-writes.c" \
        || die "replay-log build failed; see docs for the dm-flakey fallback"
fi
XFSTESTS_REV="$(git -C "$XFSTESTS" rev-parse --short HEAD 2>/dev/null || echo unknown)"

# --- state for cleanup -------------------------------------------------------
DATA_IMG="$RUN/data.img"
LOG_IMG="$RUN/log.img"
DATA=""
LOG=""
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
    for pid in "${AGENT_PIDS[@]:-}"; do [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null; done
    for m in "$RMNT" "$MNT"; do
        if mountpoint -q "$m" 2>/dev/null; then sudo umount "$m" || sudo umount -l "$m"; fi
    done
    if sudo dmsetup info "$DM_NAME" >/dev/null 2>&1; then sudo dmsetup remove "$DM_NAME" || sudo dmsetup remove --force "$DM_NAME"; fi
    for dev in "$DATA" "$LOG"; do
        [ -n "$dev" ] && sudo losetup -d "$dev" 2>/dev/null
    done
    rmdir "$RMNT" "$MNT" 2>/dev/null
    if [ "$KEEP" = 0 ]; then rm -f "$DATA_IMG" "$LOG_IMG"; fi
    log "cleanup verification: losetup -a | grep $RUN -> $(losetup -a | grep -c "$RUN" || true) entries; dmsetup ls | grep $DM_NAME -> $(sudo dmsetup ls | grep -c "$DM_NAME" || true) entries"
    exit $rc
}
trap cleanup EXIT

# --- helpers ----------------------------------------------------------------
# An unjoined agent answers only POST /v1/join, so its readiness is the
# listening port; a joined agent is polled with a granted /v1/health.
wait_port() {
    local port="$1" i
    for i in $(seq 1 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then return 0; fi
        sleep 0.1
    done
    return 1
}

# Starts an agent in the background; the PID is left in $LAST_PID (not echoed,
# so the EXIT trap's AGENT_PIDS list is updated in this shell, not a subshell).
# An unjoined agent needs DISTR_HNSW_JOIN_TOKEN in the caller's environment
# (see join_agent); a joined agent restarts without one.
LAST_PID=""
start_agent() {
    local id="$1" domain="$2" port="$3" volume="$4" logfile="$5"
    env ${AGENT_FAILPOINT:+DISTR_HNSW_AGENT_FAILPOINT=$AGENT_FAILPOINT} "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" --insecure-loopback --volume "$volume" >"$logfile" 2>&1 &
    LAST_PID=$!
    AGENT_PIDS+=("$LAST_PID")
}

# Starts a new agent with a single-use join token (same arguments as
# start_agent), waits for its port, and joins it. The token stays in the
# environment, never in a file or log.
join_agent() {
    local id="$1" domain="$2" port="$3" token
    token="$("$BIN" portal agent token --database "$PORTAL/portal.db" --master-key "$KEY" --agent-id "$id")" \
        || die "portal agent token $id failed"
    DISTR_HNSW_JOIN_TOKEN="$token" start_agent "$@"
    wait_port "$port" || die "agent $id did not start: $(tail -3 "$5")"
    DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" portal agent join --database "$PORTAL/portal.db" --master-key "$KEY" \
        --agent "$id,$domain,http://127.0.0.1:$port" >>"$RUN/portal-join.log" 2>&1 \
        || die "portal agent join $id failed: $(tail -3 "$RUN/portal-join.log")"
}

# One granted request to agent a (the agent under test) listening on $1,
# through `portal agent request`; prints only the HTTP status. The database
# lives off the dm volume and GRANT_KEY is an off-volume copy of the master
# key, so this also works while the drilled volume is unmounted for replays.
agent_request() {
    local port="$1"; shift
    "$BIN" portal agent request --database "$PORTAL/portal.db" --master-key "$GRANT_KEY" \
        --agent "a,host-a,http://127.0.0.1:$port" "$@" </dev/null
}

wait_health() {
    local port="$1" i
    for i in $(seq 1 100); do
        if [ "$(agent_request "$port" health 2>/dev/null)" = 200 ]; then return 0; fi
        sleep 0.1
    done
    return 1
}

# Print the dm table and verify it maps only onto our two loop devices.
check_dm() {
    local table data_mm log_mm
    table="$(sudo dmsetup table "$DM_NAME")"
    data_mm="$(stat -c '%Hr:%Lr' "$DATA")"
    log_mm="$(stat -c '%Hr:%Lr' "$LOG")"
    log "dm check: $DM_NAME table: $table (expect log-writes $data_mm $log_mm = $DATA $LOG)"
    case "$table" in
        *" log-writes $data_mm $log_mm") ;;
        *) die "refusing: $DM_NAME does not map onto $DATA $LOG" ;;
    esac
}

# Full inventory of one agent as JSON lines: {"kind":..,"hash":..,"size":..}
inventory() {
    local port="$1" kind after code page
    local -a cursor
    for kind in chunk manifest deletion_marker; do
        after=""
        while :; do
            cursor=()
            [ -n "$after" ] && cursor=(--after "$after")
            code="$(agent_request "$port" inventory --kind "$kind" "${cursor[@]}" --out "$RUN/inventory-page.json")" \
                || return 1
            [ "$code" = 200 ] || return 1
            page="$(cat "$RUN/inventory-page.json")"
            printf '%s' "$page" | jq -c --arg kind "$kind" '.objects[] | {kind:$kind, hash, size}'
            after="$(printf '%s' "$page" | jq -r '.next_after // empty')"
            [ -z "$after" ] && break
        done
    done
}

mark() { log "mark $1"; sudo dmsetup message "$DM_NAME" 0 mark "$1"; }

fsck_ro() {
    # Read-only checker on the unmounted data device. Prints status line.
    case "$FS" in
        btrfs) sudo btrfs check --readonly "$DATA" ;;
        ext4)  sudo e2fsck -fn "$DATA" ;;
        xfs)   sudo xfs_repair -n "$DATA" ;;
    esac
}

T0=$(date +%s)
log "drill start fs=$FS files=$FILES run=$RUN dm=$DM_NAME ports from $PORT_BASE"

# --- 1. loop devices, dm-log-writes, mkfs, mount -----------------------------
truncate -s "$IMG_SIZE" "$DATA_IMG"
truncate -s "$IMG_SIZE" "$LOG_IMG"
DATA="$(sudo losetup --find --show "$DATA_IMG")"
LOG="$(sudo losetup --find --show "$LOG_IMG")"
check_loop "$DATA" "$DATA_IMG"
check_loop "$LOG" "$LOG_IMG"
DATA_SECTORS="$(sudo blockdev --getsz "$DATA")"
DATA_WC="$(cat "/sys/block/$(basename "$DATA")/queue/write_cache")"
LOG_WC="$(cat "/sys/block/$(basename "$LOG")/queue/write_cache")"
log "data=$DATA ($DATA_SECTORS sectors, write_cache=$DATA_WC) log=$LOG (write_cache=$LOG_WC)"

log "dmsetup create $DM_NAME --table \"0 $DATA_SECTORS log-writes $DATA $LOG\""
sudo dmsetup create "$DM_NAME" --table "0 $DATA_SECTORS log-writes $DATA $LOG"
DM_WC="$(cat "/sys/block/$(basename "$(readlink -f "$DM_DEV")")/queue/write_cache")"
check_dm

# Version strings are captured whole and trimmed to the first line: piping
# `mke2fs -V` into `head -1` raises SIGPIPE under `pipefail`.
case "$FS" in
    btrfs) MKFS_VERSION="$(mkfs.btrfs --version 2>&1 || true)"; sudo mkfs.btrfs -q "$DM_DEV" ;;
    ext4)  MKFS_VERSION="$(mkfs.ext4 -V 2>&1 || true)";        sudo mkfs.ext4 -q "$DM_DEV" ;;
    xfs)   MKFS_VERSION="$(mkfs.xfs -V 2>&1 || true)";         sudo mkfs.xfs -q "$DM_DEV" ;;
esac
MKFS_VERSION="${MKFS_VERSION%%$'\n'*}"
mark mkfs
mkdir -p "$MNT" "$RMNT"
check_dm
sudo mount "$DM_DEV" "$MNT"
sudo chown "$(id -u):$(id -g)" "$MNT"
MOUNT_OPTS="$(findmnt -no OPTIONS "$MNT")"
log "mounted $DM_DEV at $MNT with options: $MOUNT_OPTS"

# --- 2. agents, portal, uploads ------------------------------------------------
VOL_A="$MNT/agent-a"
VOL_B="$RUN/agent-b"
PORTAL="$RUN/portal"
mkdir -p "$VOL_A" "$VOL_B" "$PORTAL"
PORT_A=$PORT_BASE
PORT_B=$((PORT_BASE + 1))

# The master key lives on the drilled volume so its durable write is tested;
# the portal database stays off it. Init runs before the agents start: it
# creates the grant-signing key that join tokens need.
KEY="$MNT/portal-key/master.key"
"$BIN" portal init --no-recovery-bundle --database "$PORTAL/portal.db" --master-key "$KEY" >"$RUN/portal-init.log" 2>&1 \
    || die "portal init failed: $(cat "$RUN/portal-init.log")"
KEY_SHA256="$(sha256sum "$KEY" | cut -d' ' -f1)"
KEY_SIZE="$(stat -c %s "$KEY")"
GRANT_KEY="$PORTAL/master.key"
install -m 600 "$KEY" "$GRANT_KEY"
mark master-key

join_agent a host-a "$PORT_A" "$VOL_A" "$RUN/agent-a.log"; PID_A=$LAST_PID
join_agent b host-b "$PORT_B" "$VOL_B" "$RUN/agent-b.log"; PID_B=$LAST_PID
[ "$(agent_request "$PORT_A" health --out "$RUN/health-a.json")" = 200 ] || die "agent a health failed"
INCARNATION_ORIG="$(jq -r .incarnation_id "$RUN/health-a.json")"
log "agent a pid=$PID_A port=$PORT_A incarnation=$INCARNATION_ORIG; agent b pid=$PID_B port=$PORT_B"

# File sizes: a few KiB up to ~6 MiB (CHUNK_SIZE is 4 MiB, so >4 MiB gives 2 chunks).
SIZES=(3072 65536 1048576 4194305 6291456 20480 5242880 6000000)
: > "$RUN/marks.jsonl"
for i in $(seq 1 "$FILES"); do
    size="${SIZES[$(( (i - 1) % ${#SIZES[@]} ))]}"
    src="$RUN/file-$i.bin"
    head -c "$size" /dev/urandom > "$src"
    fid="$("$BIN" portal put --database "$PORTAL/portal.db" --master-key "$KEY" \
        --agent "a,host-a,http://127.0.0.1:$PORT_A" --agent "b,host-b,http://127.0.0.1:$PORT_B" \
        --idempotency-key "drill-$FS-$UTC-$i" "$src" 2>>"$RUN/portal-put.log")" \
        || die "portal put $i failed: $(tail -5 "$RUN/portal-put.log")"
    mark "put-$i"
    inventory "$PORT_A" > "$RUN/inventory-put-$i.jsonl" || die "inventory after put $i failed"
    count="$(wc -l < "$RUN/inventory-put-$i.jsonl")"
    jq -nc --arg mark "put-$i" --arg fid "$fid" --argjson size "$size" --argjson objects "$count" \
        '{mark:$mark, file_id:$fid, file_size:$size, acknowledged_objects:$objects}' >> "$RUN/marks.jsonl"
    log "put $i: size=$size file_id=$fid acknowledged objects on agent a=$count"
done

# --- 2b. re-PUT after a crash between rename and directory sync --------------
# Ports above the replay range (PORT_BASE + 2 .. PORT_BASE + 2 + FILES).
PORT_CRASH=$((PORT_BASE + FILES + 3))
PORT_RETRY=$((PORT_BASE + FILES + 4))
kill -9 "$PID_A"; wait "$PID_A" 2>/dev/null || true
AGENT_FAILPOINT=put_after_rename start_agent a host-a "$PORT_CRASH" "$VOL_A" "$RUN/agent-a-crash.log"; PID_CRASH=$LAST_PID
wait_health "$PORT_CRASH" || die "failpoint agent did not become healthy"
head -c 3072 /dev/urandom > "$RUN/file-reput.bin"
"$BIN" portal put --database "$PORTAL/portal.db" --master-key "$KEY" \
    --agent "a,host-a,http://127.0.0.1:$PORT_CRASH" --agent "b,host-b,http://127.0.0.1:$PORT_B" \
    --idempotency-key "drill-$FS-$UTC-reput" "$RUN/file-reput.bin" >>"$RUN/portal-put.log" 2>&1 \
    && die "portal put succeeded although agent a should have exited after its rename"
CRASH_RC=0; wait "$PID_CRASH" 2>/dev/null || CRASH_RC=$?
CRASHED_AT_MS=$(date +%s%3N)
[ "$CRASH_RC" -eq 86 ] || die "failpoint agent exited with $CRASH_RC, expected 86"
# The object agent a renamed into place but never acknowledged.
REPUT_PATH="$(comm -13 <(jq -r .hash "$RUN/inventory-put-$FILES.jsonl" | sort) \
    <(find "$VOL_A/objects" -type f ! -name '.*' -printf '%f\n' | sort))"
[ "$(printf '%s\n' "$REPUT_PATH" | grep -c .)" -eq 1 ] || die "expected one unacknowledged object, found: $REPUT_PATH"
REPUT_HASH="$REPUT_PATH"
REPUT_FILE="$(find "$VOL_A/objects" -type f -name "$REPUT_HASH")"
REPUT_KIND="$(basename "$(dirname "$(dirname "$(dirname "$REPUT_FILE")")")")"
cp "$REPUT_FILE" "$RUN/reput-object.bin"
start_agent a host-a "$PORT_RETRY" "$VOL_A" "$RUN/agent-a-retry.log"; PID_A=$LAST_PID
wait_health "$PORT_RETRY" || die "agent a did not restart after the failpoint"
REPUT_CODE="$(agent_request "$PORT_RETRY" put --kind "$REPUT_KIND" --hash "$REPUT_HASH" \
    --body-file "$RUN/reput-object.bin")"
[ "$REPUT_CODE" = 204 ] || die "re-PUT answered $REPUT_CODE, expected 204"
REPUT_GAP_MS=$(( $(date +%s%3N) - CRASHED_AT_MS ))
mark reput-ack
inventory "$PORT_RETRY" > "$RUN/inventory-reput-ack.jsonl" || die "inventory after re-PUT failed"
log "re-PUT: $REPUT_KIND $REPUT_HASH acknowledged ${REPUT_GAP_MS} ms after the crash"

# Negative control: a 1 MiB file written with NO fsync, followed by a mark. If
# the replay to that mark shows the full file, the log is not discarding
# cached writes and every pass above would be meaningless.
CONTROL_SIZE=1048576
head -c "$CONTROL_SIZE" /dev/urandom > "$VOL_A/control-unsynced.bin"
mark unsynced-control

# --- 3. simulated power loss ----------------------------------------------------
log "kill -9 agent a ($PID_A)"
kill -9 "$PID_A"; wait "$PID_A" 2>/dev/null || true
kill -9 "$PID_B"; wait "$PID_B" 2>/dev/null || true
AGENT_PIDS=()
mark end
# Everything the unmount flushes lands after the `end` mark and is ignored by
# replays to put-<i> (this is the xfstests _log_writes_unmount/_remove order).
sudo umount "$MNT"
sudo dmsetup remove "$DM_NAME"
# (the usage text says --number-entries; the option table spells it --num-entries)
LOG_ENTRIES="$(sudo "$REPLAY" --log "$LOG" --num-entries | tr -dc '0-9')"
[ -n "$LOG_ENTRIES" ] || LOG_ENTRIES=0
log "dm device removed; log has $LOG_ENTRIES entries"

# --- 4. replay to each mark and verify --------------------------------------------
: > "$RUN/results.jsonl"
PASS=0
MARKS_TOTAL=$((FILES + 1))
for i in $(seq 1 "$MARKS_TOTAL"); do
    if [ "$i" -le "$FILES" ]; then m="put-$i"; else m="reput-ack"; fi
    inv="$RUN/inventory-$m.jsonl"
    port=$((PORT_BASE + 1 + i))
    check_loop "$DATA" "$DATA_IMG"
    check_loop "$LOG" "$LOG_IMG"
    # Discard the whole data device so nothing written after the mark can
    # survive from the pre-replay state (stricter than plain xfstests replay).
    sudo blkdiscard -f "$DATA" || sudo dd if=/dev/zero of="$DATA" bs=4M status=none || true
    sudo "$REPLAY" --log "$LOG" --replay "$DATA" --end-mark "$m" >"$RUN/replay-$m.log" 2>&1 \
        || die "replay to $m failed: $(tail -3 "$RUN/replay-$m.log")"
    [ "$FS" = btrfs ] && sudo btrfs device scan --forget >/dev/null 2>&1 || true

    mount_ok=true; mount_err=""
    if ! mount_err="$(sudo mount "$DATA" "$RMNT" 2>&1)"; then mount_ok=false; fi
    ropts=""; agent_ok=false; incarnation=""; present=0; missing=0; corrupt=0; other=0; tmp_files=0; later_visible=0
    missing_list="[]"
    if $mount_ok; then
        ropts="$(findmnt -no OPTIONS "$RMNT")"
        sudo chown "$(id -u):$(id -g)" "$RMNT" 2>/dev/null || true
        tmp_files="$(find "$RMNT/agent-a" -name '.*.tmp' 2>/dev/null | wc -l)"
        start_agent a host-a "$port" "$RMNT/agent-a" "$RUN/agent-replay-$m.log"; pid=$LAST_PID
        if wait_health "$port"; then
            agent_ok=true
            [ "$(agent_request "$port" health --out "$RUN/health-replay-$m.json")" = 200 ] \
                || die "replayed agent health failed at $m"
            incarnation="$(jq -r .incarnation_id "$RUN/health-replay-$m.json")"
            # Objects the replayed agent lists that were only acknowledged
            # after this mark (informational; they were never promised).
            inventory "$port" > "$RUN/inventory-replay-$m.jsonl" || true
            later_visible="$(comm -13 <(jq -r .hash "$inv" | sort) \
                                     <(jq -r .hash "$RUN/inventory-replay-$m.jsonl" | sort) | wc -l)"
            while IFS= read -r line; do
                kind="$(jq -r .kind <<<"$line")"; hash="$(jq -r .hash <<<"$line")"
                code="$(agent_request "$port" get --kind "$kind" --hash "$hash")"
                case "$code" in
                    200) present=$((present + 1)) ;;
                    404) missing=$((missing + 1)); missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                    409) corrupt=$((corrupt + 1)); missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                    *)   other=$((other + 1));   missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                esac
            done < "$inv"
        fi
        kill -9 "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
        AGENT_PIDS=()
        sudo umount "$RMNT"
    fi
    fsck_rc=0
    fsck_ro >"$RUN/fsck-$m.log" 2>&1 || fsck_rc=$?
    expected="$(wc -l < "$inv")"
    ok=false
    if $mount_ok && $agent_ok && [ "$present" -eq "$expected" ] && [ "$fsck_rc" -eq 0 ]; then ok=true; PASS=$((PASS + 1)); fi
    jq -nc --arg mark "$m" --argjson ok "$ok" --argjson mount_ok "$mount_ok" --arg mount_err "$mount_err" \
        --arg ropts "$ropts" --argjson agent_ok "$agent_ok" --arg incarnation "$incarnation" \
        --argjson expected "$expected" --argjson present "$present" --argjson missing "$missing" \
        --argjson corrupt "$corrupt" --argjson other "$other" --argjson tmp "$tmp_files" \
        --argjson fsck_rc "$fsck_rc" --arg fsck_tail "$(tail -3 "$RUN/fsck-$m.log" | tr '\n' ' ')" \
        --argjson failures "$missing_list" \
        --argjson incarnation_same "$([ "$incarnation" = "$INCARNATION_ORIG" ] && echo true || echo false)" \
        --argjson later_visible "$later_visible" \
        '{mark:$mark, pass:$ok, mount_ok:$mount_ok, mount_error:$mount_err, mount_options:$ropts,
          agent_started:$agent_ok, incarnation_id:$incarnation, incarnation_same:$incarnation_same, expected_objects:$expected,
          present:$present, missing:$missing, corrupt:$corrupt, other_status:$other,
          later_acknowledged_objects_visible:$later_visible,
          surviving_tmp_files:$tmp, fsck_exit:$fsck_rc, fsck_tail:$fsck_tail, failures:$failures}' \
        >> "$RUN/results.jsonl"
    log "$m: pass=$ok present=$present/$expected missing=$missing corrupt=$corrupt later_visible=$later_visible tmp=$tmp_files fsck_rc=$fsck_rc incarnation_same=$([ "$incarnation" = "$INCARNATION_ORIG" ] && echo yes || echo no)"
done

# --- 4a. master key at its mark ----------------------------------------------------
check_loop "$DATA" "$DATA_IMG"
check_loop "$LOG" "$LOG_IMG"
sudo blkdiscard -f "$DATA" || sudo dd if=/dev/zero of="$DATA" bs=4M status=none || true
sudo "$REPLAY" --log "$LOG" --replay "$DATA" --end-mark master-key >"$RUN/replay-master-key.log" 2>&1 \
    || die "replay to master-key failed"
[ "$FS" = btrfs ] && sudo btrfs device scan --forget >/dev/null 2>&1 || true
KEY_OK=false; KEY_SHA_AFTER=""; KEY_SIZE_AFTER=-1
if sudo mount "$DATA" "$RMNT" 2>>"$RUN/drill.log"; then
    rkey="$RMNT/portal-key/master.key"
    if sudo test -f "$rkey"; then
        KEY_SHA_AFTER="$(sudo sha256sum "$rkey" | cut -d' ' -f1)"
        KEY_SIZE_AFTER="$(sudo stat -c %s "$rkey")"
    fi
    sudo umount "$RMNT"
fi
KEY_FSCK_RC=0
fsck_ro >"$RUN/fsck-master-key.log" 2>&1 || KEY_FSCK_RC=$?
KEY_SHA_MATCH=false; [ "$KEY_SHA_AFTER" = "$KEY_SHA256" ] && KEY_SHA_MATCH=true
if $KEY_SHA_MATCH && [ "$KEY_FSCK_RC" -eq 0 ]; then KEY_OK=true; fi
log "master-key: pass=$KEY_OK size=$KEY_SIZE_AFTER/$KEY_SIZE sha256_match=$KEY_SHA_MATCH fsck_rc=$KEY_FSCK_RC"

# --- 4b. negative control: the un-fsynced file must not have survived --------------
check_loop "$DATA" "$DATA_IMG"
check_loop "$LOG" "$LOG_IMG"
sudo blkdiscard -f "$DATA" || sudo dd if=/dev/zero of="$DATA" bs=4M status=none || true
sudo "$REPLAY" --log "$LOG" --replay "$DATA" --end-mark unsynced-control >"$RUN/replay-unsynced-control.log" 2>&1 \
    || die "replay to unsynced-control failed"
[ "$FS" = btrfs ] && sudo btrfs device scan --forget >/dev/null 2>&1 || true
CONTROL_VISIBLE=-1
if sudo mount "$DATA" "$RMNT" 2>>"$RUN/drill.log"; then
    CONTROL_VISIBLE="$(stat -c %s "$RMNT/agent-a/control-unsynced.bin" 2>/dev/null || echo 0)"
    sudo umount "$RMNT"
fi
if [ "$CONTROL_VISIBLE" -lt "$CONTROL_SIZE" ]; then CONTROL_OK=true; else CONTROL_OK=false; fi
log "negative control: un-fsynced ${CONTROL_SIZE}-byte file shows $CONTROL_VISIBLE bytes after replay (loss observed: $CONTROL_OK)"

# --- 5. report ----------------------------------------------------------------
T1=$(date +%s)
jq -n \
    --arg fs "$FS" --arg utc "$UTC" --arg host "$(hostname)" --arg kernel "$(uname -r)" \
    --arg mkfs "$MKFS_VERSION" --arg dm "$(sudo dmsetup targets | grep -E '^log-writes' | tr -s ' ')" \
    --arg xfstests "$XFSTESTS_REV" --arg mount_opts "$MOUNT_OPTS" \
    --arg source_rev "$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." describe --always --dirty 2>/dev/null || echo unknown)" \
    --arg data_wc "$DATA_WC" --arg log_wc "$LOG_WC" --arg dm_wc "$DM_WC" \
    --arg data "$DATA" --arg logdev "$LOG" --argjson entries "${LOG_ENTRIES:-0}" \
    --arg incarnation "$INCARNATION_ORIG" \
    --argjson files "$MARKS_TOTAL" --argjson pass "$PASS" --argjson seconds "$((T1 - T0))" \
    --argjson key_ok "$KEY_OK" --argjson key_size "$KEY_SIZE" --argjson key_size_after "$KEY_SIZE_AFTER" \
    --argjson key_fsck "$KEY_FSCK_RC" --argjson key_sha_match "$KEY_SHA_MATCH" --arg reput_kind "$REPUT_KIND" --arg reput_hash "$REPUT_HASH" \
    --argjson reput_gap "$REPUT_GAP_MS" \
    --argjson control_size "$CONTROL_SIZE" --argjson control_visible "$CONTROL_VISIBLE" --argjson control_ok "$CONTROL_OK" \
    --slurpfile marks "$RUN/marks.jsonl" --slurpfile results "$RUN/results.jsonl" \
    '{filesystem:$fs, started_utc:$utc, host:$host, kernel:$kernel, source_revision:$source_rev, mkfs_version:$mkfs,
      dm_log_writes_target:$dm, xfstests_rev:$xfstests, replay_tool:"xfstests replay-log",
      mount_options:$mount_opts, loop_write_cache:{data:$data_wc, log:$log_wc, dm:$dm_wc},
      devices:{data:$data, log:$logdev}, log_entries:$entries, original_incarnation:$incarnation,
      marks_total:$files, marks_passed:$pass, binary_failpoints:true,
      master_key:{mark:"master-key", pass:$key_ok, bytes_written:$key_size, bytes_after_replay:$key_size_after,
                  sha256_match:$key_sha_match, fsck_exit:$key_fsck},
      reput:{mark:"reput-ack", kind:$reput_kind, hash:$reput_hash, ms_from_crash_to_ack:$reput_gap},
      negative_control:{unsynced_file_bytes_written:$control_size, bytes_visible_after_replay:$control_visible, loss_observed:$control_ok},
      verdict:(if $pass == $files and $key_ok and $control_ok then "all-marks-pass"
               elif $pass == $files and $key_ok then "INCONCLUSIVE: negative control did not show loss" else "FAIL" end),
      wall_clock_seconds:$seconds, caveats:["loop device over a sparse image on btrfs /home; the physical drive cache was not exercised",
      "data device discarded before each replay (stricter than plain xfstests replay)",
      "replayed volume mounted read-write with default options so journal recovery runs as on a real restart"],
      uploads:$marks, results:$results}' > "$REPORT"
log "report: $REPORT"
log "RESULT fs=$FS kernel=$(uname -r) marks passed $PASS/$MARKS_TOTAL, master key intact=$KEY_OK, negative control loss observed=$CONTROL_OK, in $((T1 - T0))s"
[ "$PASS" -eq "$MARKS_TOTAL" ] && $KEY_OK && $CONTROL_OK

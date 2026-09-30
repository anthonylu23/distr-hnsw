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
#   2. Starts one agent on the dm-backed volume (a second agent on a plain
#      directory supplies the second failure domain RF2 requires), inits a
#      portal DB outside the mount, uploads N files, and after every
#      acknowledged put inserts a log mark and snapshots the agent inventory.
#   3. kill -9 the agent, marks `end`, unmounts, removes the dm device.
#   4. For every put mark: discards the data device, replays the log up to
#      the mark, mounts the data device (journal recovery runs as it would
#      after a real restart), starts a fresh agent on the replayed volume and
#      checks that every inventory entry recorded at that mark answers
#      GET /v1/objects/{kind}/{hash} with 200 (the agent re-hashes on GET).
#      Then unmounts and runs the filesystem's read-only checker.
#   5. Writes $HOME/distr-hnsw-drill/report-<fs>-<utc>.json.
#
# Safety: only loop devices attached to images this script created are ever
# passed to dmsetup/mkfs/mount/blkdiscard/replay-log; each is verified with
# `losetup -j <image>` immediately before use. Device-mapper names are
# dhq-<fs>-<pid>; mount points live under $HOME/distr-hnsw-drill/mnt-*.
# An EXIT trap tears everything down and the script verifies cleanup.
#
# Requirements: passwordless sudo, dm-log-writes module, mkfs.<fs>, jq, curl,
# gcc + a shallow xfstests clone for replay-log (built automatically into
# $HOME/distr-hnsw-drill/replay-log), and target/release/distr-hnsw.

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
for tool in "mkfs.$FS" dmsetup losetup blockdev blkdiscard jq curl gcc; do
    command -v "$tool" >/dev/null || die "missing tool: $tool"
done
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
wait_health() {
    local port="$1" i
    for i in $(seq 1 100); do
        if curl -fsS "http://127.0.0.1:$port/v1/health" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    return 1
}

# Starts an agent in the background; the PID is left in $LAST_PID (not echoed,
# so the EXIT trap's AGENT_PIDS list is updated in this shell, not a subshell).
LAST_PID=""
start_agent() {
    local id="$1" domain="$2" port="$3" volume="$4" logfile="$5"
    "$BIN" agent --id "$id" --failure-domain "$domain" --bind "127.0.0.1:$port" --volume "$volume" >"$logfile" 2>&1 &
    LAST_PID=$!
    AGENT_PIDS+=("$LAST_PID")
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
    local port="$1" kind after url page
    for kind in chunk manifest deletion_marker; do
        after=""
        while :; do
            url="http://127.0.0.1:$port/v1/inventory/$kind"
            [ -n "$after" ] && url="$url?after=$after"
            page="$(curl -fsS "$url")" || return 1
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
start_agent a host-a "$PORT_A" "$VOL_A" "$RUN/agent-a.log"; PID_A=$LAST_PID
start_agent b host-b "$PORT_B" "$VOL_B" "$RUN/agent-b.log"; PID_B=$LAST_PID
wait_health "$PORT_A" || die "agent a did not become healthy"
wait_health "$PORT_B" || die "agent b did not become healthy"
INCARNATION_ORIG="$(curl -fsS "http://127.0.0.1:$PORT_A/v1/health" | jq -r .incarnation_id)"
log "agent a pid=$PID_A port=$PORT_A incarnation=$INCARNATION_ORIG; agent b pid=$PID_B port=$PORT_B"

"$BIN" portal init --no-recovery-bundle --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" >"$RUN/portal-init.log" 2>&1 \
    || die "portal init failed: $(cat "$RUN/portal-init.log")"

# File sizes: a few KiB up to ~6 MiB (CHUNK_SIZE is 4 MiB, so >4 MiB gives 2 chunks).
SIZES=(3072 65536 1048576 4194305 6291456 20480 5242880 6000000)
: > "$RUN/marks.jsonl"
for i in $(seq 1 "$FILES"); do
    size="${SIZES[$(( (i - 1) % ${#SIZES[@]} ))]}"
    src="$RUN/file-$i.bin"
    head -c "$size" /dev/urandom > "$src"
    fid="$("$BIN" portal put --database "$PORTAL/portal.db" --master-key "$PORTAL/master.key" \
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
for i in $(seq 1 "$FILES"); do
    m="put-$i"
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
            incarnation="$(curl -fsS "http://127.0.0.1:$port/v1/health" | jq -r .incarnation_id)"
            # Objects the replayed agent lists that were only acknowledged
            # after this mark (informational; they were never promised).
            inventory "$port" > "$RUN/inventory-replay-$m.jsonl" || true
            later_visible="$(comm -13 <(jq -r .hash "$RUN/inventory-put-$i.jsonl" | sort) \
                                     <(jq -r .hash "$RUN/inventory-replay-$m.jsonl" | sort) | wc -l)"
            while IFS= read -r line; do
                kind="$(jq -r .kind <<<"$line")"; hash="$(jq -r .hash <<<"$line")"
                code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/v1/objects/$kind/$hash")"
                case "$code" in
                    200) present=$((present + 1)) ;;
                    404) missing=$((missing + 1)); missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                    409) corrupt=$((corrupt + 1)); missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                    *)   other=$((other + 1));   missing_list="$(jq -c --arg k "$kind" --arg h "$hash" --arg c "$code" '. + [{kind:$k,hash:$h,status:$c}]' <<<"$missing_list")" ;;
                esac
            done < "$RUN/inventory-put-$i.jsonl"
        fi
        kill -9 "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
        AGENT_PIDS=()
        sudo umount "$RMNT"
    fi
    fsck_rc=0
    fsck_ro >"$RUN/fsck-$m.log" 2>&1 || fsck_rc=$?
    expected="$(wc -l < "$RUN/inventory-put-$i.jsonl")"
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
    --arg data_wc "$DATA_WC" --arg log_wc "$LOG_WC" --arg dm_wc "$DM_WC" \
    --arg data "$DATA" --arg logdev "$LOG" --argjson entries "${LOG_ENTRIES:-0}" \
    --arg incarnation "$INCARNATION_ORIG" \
    --argjson files "$FILES" --argjson pass "$PASS" --argjson seconds "$((T1 - T0))" \
    --argjson control_size "$CONTROL_SIZE" --argjson control_visible "$CONTROL_VISIBLE" --argjson control_ok "$CONTROL_OK" \
    --slurpfile marks "$RUN/marks.jsonl" --slurpfile results "$RUN/results.jsonl" \
    '{filesystem:$fs, started_utc:$utc, host:$host, kernel:$kernel, mkfs_version:$mkfs,
      dm_log_writes_target:$dm, xfstests_rev:$xfstests, replay_tool:"xfstests replay-log",
      mount_options:$mount_opts, loop_write_cache:{data:$data_wc, log:$log_wc, dm:$dm_wc},
      devices:{data:$data, log:$logdev}, log_entries:$entries, original_incarnation:$incarnation,
      marks_total:$files, marks_passed:$pass,
      negative_control:{unsynced_file_bytes_written:$control_size, bytes_visible_after_replay:$control_visible, loss_observed:$control_ok},
      verdict:(if $pass == $files and $control_ok then "all-marks-pass"
               elif $pass == $files then "INCONCLUSIVE: negative control did not show loss" else "FAIL" end),
      wall_clock_seconds:$seconds, caveats:["loop device over a sparse image on btrfs /home; the physical drive cache was not exercised",
      "data device discarded before each replay (stricter than plain xfstests replay)",
      "replayed volume mounted read-write with default options so journal recovery runs as on a real restart"],
      uploads:$marks, results:$results}' > "$REPORT"
log "report: $REPORT"
log "RESULT fs=$FS kernel=$(uname -r) marks passed $PASS/$FILES, negative control loss observed=$CONTROL_OK, in $((T1 - T0))s"
[ "$PASS" -eq "$FILES" ] && $CONTROL_OK

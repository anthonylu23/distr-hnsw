#!/usr/bin/env bash
# Tailnet authorization drill (M2; docs/m2-implementation-plan.md).
#
# Runs two agents on this node's tailnet address, joins them, and checks:
#   1. the portal round-trips a file over the tailnet with grants;
#   2. a request from another tailnet node ($REMOTE, reached over SSH) is
#      refused on every agent route, with and without forged identity
#      headers, because WhoIs names a node other than the pinned portal;
#   3. a local request without a grant is refused.
# It changes no tailnet setting (no serve, Funnel, certificates, or ACLs)
# and uses high ports that it releases on exit.
#
# A host firewall that admits only some ports on tailscale0 stops the other
# node's requests before they reach the agent. OPEN_FIREWALL=1 (needs sudo
# and firewalld) adds a runtime-only rule for the two drill ports on the
# tailscale0 zone with a 300 s timeout, removes it on exit, and records the
# zone's ports before and after; nothing permanent changes.
#
# Usage: REMOTE=user@other-node [OPEN_FIREWALL=1] scripts/tailnet-authorization-drill.sh
set -euo pipefail

REMOTE="${REMOTE:?set REMOTE to the SSH destination of another tailnet node}"
PORT_A="${PORT_A:-47311}"
PORT_B="${PORT_B:-47312}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$HOME/distr-hnsw-drill/tailnet-authz-$STAMP"
REPORT="$HOME/distr-hnsw-drill/tailnet-authz-report-$STAMP.json"
mkdir -p "$WORK"

(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
REVISION="$(cd "$REPO" && git describe --always --dirty)"
TAILNET_IP="$(tailscale ip -4 | head -1)"
DB="$WORK/portal.sqlite"
KEY="$WORK/master.key"

PIDS=()
ZONE=""
FIREWALL_BEFORE=""
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [[ -n "$ZONE" ]]; then
        sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT_A-$PORT_B/tcp" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ "${OPEN_FIREWALL:-0}" == 1 ]]; then
    ZONE="$(firewall-cmd --get-zone-of-interface=tailscale0)"
    FIREWALL_BEFORE="$(sudo firewall-cmd --zone="$ZONE" --list-ports)"
    sudo firewall-cmd --zone="$ZONE" --add-port="$PORT_A-$PORT_B/tcp" --timeout=300 >/dev/null
fi

wait_port() {
    local port="$1"
    for _ in $(seq 1 200); do
        (exec 3<>"/dev/tcp/$TAILNET_IP/$port") 2>/dev/null && return 0
        sleep 0.05
    done
    echo "agent on port $port did not start" >&2
    return 1
}

"$BIN" portal init --database "$DB" --master-key "$KEY" --no-recovery-bundle \
    >"$WORK/init.log" 2>&1
AGENTS=()
for spec in "a1 d1 $PORT_A" "a2 d2 $PORT_B"; do
    read -r id domain port <<<"$spec"
    token="$("$BIN" portal agent token --database "$DB" --master-key "$KEY" --agent-id "$id")"
    DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" agent --id "$id" --failure-domain "$domain" \
        --bind "tailnet:$port" --volume "$WORK/$id" >"$WORK/$id.log" 2>&1 &
    PIDS+=("$!")
    wait_port "$port"
    DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" portal agent join --database "$DB" \
        --master-key "$KEY" --agent "$id,$domain,http://$TAILNET_IP:$port" \
        >"$WORK/join-$id.json"
    AGENTS+=(--agent "$id,$domain,http://$TAILNET_IP:$port")
done

# 1. A granted round trip over the tailnet.
head -c 300000 /dev/urandom >"$WORK/source.bin"
FILE_ID="$("$BIN" portal put --database "$DB" --master-key "$KEY" "${AGENTS[@]}" \
    --idempotency-key drill "$WORK/source.bin" | tail -1)"
"$BIN" portal get --database "$DB" --master-key "$KEY" "${AGENTS[@]}" \
    "$FILE_ID" "$WORK/restored.bin" >/dev/null
ROUND_TRIP=false
cmp -s "$WORK/source.bin" "$WORK/restored.bin" && ROUND_TRIP=true
# A chunk the drill just stored, so a refused DELETE is a real target.
HASH="$(find "$WORK/a1/objects/chunk" -type f ! -name '.*' -printf '%f\n' | head -1)"

# 2. Requests from the other node: every route, then forged headers.
remote_status() {
    local method="$1" path="$2"; shift 2
    # shellcheck disable=SC2029
    ssh -o BatchMode=yes "$REMOTE" \
        "curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X $method $* http://$TAILNET_IP:$PORT_A$path" \
        || echo "ssh-failed"
}
declare -A REMOTE_RESULTS
REMOTE_RESULTS[health]="$(remote_status GET /v1/health)"
REMOTE_RESULTS[inventory]="$(remote_status GET /v1/inventory/chunk)"
REMOTE_RESULTS[get]="$(remote_status GET "/v1/objects/chunk/$HASH")"
REMOTE_RESULTS[delete]="$(remote_status DELETE "/v1/objects/chunk/$HASH")"
REMOTE_RESULTS[put]="$(remote_status PUT "/v1/objects/chunk/$HASH" --data-binary x)"
REMOTE_RESULTS[forged_headers]="$(remote_status DELETE "/v1/objects/chunk/$HASH" \
    -H "'Tailscale-User-Login: owner@example.com'" -H "'Tailscale-User-Name: Owner'" \
    -H "'X-Forwarded-For: $TAILNET_IP'")"
REMOTE_RESULTS[join]="$(remote_status POST /v1/join -H "'Content-Type: application/json'" \
    --data "'{\"token\":\"guess\",\"key_set\":\"AA\"}'")"

# 3. A local request without a grant.
LOCAL_NO_GRANT="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    -X DELETE "http://$TAILNET_IP:$PORT_A/v1/objects/chunk/$HASH")"

# The object must still be there after every refused DELETE.
STILL_PRESENT=false
"$BIN" portal get --database "$DB" --master-key "$KEY" "${AGENTS[@]}" \
    "$FILE_ID" "$WORK/after.bin" >/dev/null 2>&1 \
  && cmp -s "$WORK/source.bin" "$WORK/after.bin" && STILL_PRESENT=true

REFUSED=true
# Every route refuses the other node (403, peer rejected); join on a joined
# agent refuses before reading the request (409, already joined).
for route in "${!REMOTE_RESULTS[@]}"; do
    expected=403
    [[ "$route" == join ]] && expected=409
    [[ "${REMOTE_RESULTS[$route]}" == "$expected" ]] || REFUSED=false
done
[[ "$LOCAL_NO_GRANT" == 401 ]] || REFUSED=false
VERDICT=fail
[[ "$ROUND_TRIP" == true && "$STILL_PRESENT" == true && "$REFUSED" == true ]] && VERDICT=pass

FIREWALL_AFTER=""
if [[ -n "$ZONE" ]]; then
    sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT_A-$PORT_B/tcp" >/dev/null 2>&1 || true
    FIREWALL_AFTER="$(sudo firewall-cmd --zone="$ZONE" --list-ports)"
    ZONE_RESTORED=false
    [[ "$FIREWALL_AFTER" == "$FIREWALL_BEFORE" ]] && ZONE_RESTORED=true
    [[ "$ZONE_RESTORED" == true ]] || VERDICT=fail
fi

{
    printf '{\n  "drill": "tailnet-authorization",\n  "utc": "%s",\n' "$STAMP"
    printf '  "source_revision": "%s",\n  "verdict": "%s",\n' "$REVISION" "$VERDICT"
    printf '  "granted_round_trip": %s,\n  "object_intact_after_refused_deletes": %s,\n' \
        "$ROUND_TRIP" "$STILL_PRESENT"
    if [[ -n "$ZONE" ]]; then
        printf '  "firewall": {"zone": "%s", "temporary_ports": "%s-%s/tcp", "restored": %s},\n' \
            "$ZONE" "$PORT_A" "$PORT_B" "$ZONE_RESTORED"
    fi
    printf '  "local_request_without_grant": %s,\n  "remote_node_requests": {' "$LOCAL_NO_GRANT"
    sep=""
    for route in health inventory get put delete forged_headers join; do
        printf '%s\n    "%s": "%s"' "$sep" "$route" "${REMOTE_RESULTS[$route]}"
        sep=","
    done
    printf '\n  }\n}\n'
} >"$REPORT"
cat "$REPORT"
[[ "$VERDICT" == pass ]]

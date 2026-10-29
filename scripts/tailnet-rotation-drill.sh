#!/usr/bin/env bash
# Signing-key rotation and node-join drill (M2 work package 7;
# docs/m2-implementation-plan.md).
#
# Runs three agents on this node's tailnet address and captures the
# portal's requests off the wire (tcpdump on loopback, which carries traffic
# to the node's own tailnet address), then checks:
#   join        a token names one agent and is single use on both sides; a
#               sniffed or guessed token is refused from another node; the
#               portal records the agent's tailnet node;
#   replay      a captured grant is refused from another node, on another
#               agent, and for another operation; from the portal's node it
#               repeats only the operation it names, until it expires;
#   rotation    the old key keeps verifying during the overlap; an agent
#               offline across a rotation catches up on its next request;
#   revocation  after `retire-signing --revoke` and one granted request per
#               agent, grants under the revoked key or the old epoch are
#               refused; an agent offline across the revocation refuses the
#               new key set (the revoked key cannot vouch for it) and rejoins
#               with --reset-trust and a new token.
# The remote checks finish within the firewall rule's 300 s; the expiry
# check then waits out the grant lifetime plus the clock-skew allowance.
#
# Needs sudo for tcpdump (and OPEN_FIREWALL=1, as in
# tailnet-authorization-drill.sh). Changes no tailnet setting.
#
# Usage: REMOTE=user@other-node [OPEN_FIREWALL=1] scripts/tailnet-rotation-drill.sh
set -euo pipefail

REMOTE="${REMOTE:?set REMOTE to the SSH destination of another tailnet node}"
PORT_A="${PORT_A:-47311}"
PORT_B=$((PORT_A + 1))
PORT_C=$((PORT_A + 2))
REPO="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$HOME/distr-hnsw-drill/tailnet-rotation-$STAMP"
REPORT="$HOME/distr-hnsw-drill/tailnet-rotation-report-$STAMP.json"
mkdir -p "$WORK"

(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
REVISION="$(cd "$REPO" && git describe --always --dirty)"
TAILNET_IP="$(tailscale ip -4 | head -1)"
SELF_NODE="$(tailscale status --json | jq -r '.Self.ID')"
P=(--database "$WORK/portal.sqlite" --master-key "$WORK/master.key")

PIDS=()
declare -A AGENT_PID
ZONE=""
FIREWALL_BEFORE=""
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    sudo pkill -f "tcpdump -i lo .*portrange $PORT_A-$PORT_C" 2>/dev/null || true
    wait 2>/dev/null || true
    if [[ -n "$ZONE" ]]; then
        sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT_A-$PORT_C/tcp" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ "${OPEN_FIREWALL:-0}" == 1 ]]; then
    ZONE="$(firewall-cmd --get-zone-of-interface=tailscale0)"
    FIREWALL_BEFORE="$(sudo firewall-cmd --zone="$ZONE" --list-ports)"
    sudo firewall-cmd --zone="$ZONE" --add-port="$PORT_A-$PORT_C/tcp" --timeout=300 >/dev/null
fi

sudo tcpdump -i lo -l -A -s 0 -nn "tcp dst portrange $PORT_A-$PORT_C" \
    >"$WORK/capture.txt" 2>"$WORK/tcpdump.log" &
PIDS+=("$!")
until grep -q "listening on" "$WORK/tcpdump.log" 2>/dev/null; do sleep 0.1; done

remote() {
    # shellcheck disable=SC2029
    ssh -o BatchMode=yes "$REMOTE" "$@"
}
port_of() { case "$1" in a1) echo "$PORT_A" ;; a2) echo "$PORT_B" ;; a3) echo "$PORT_C" ;; esac; }
target() { echo "$1,d-$1,http://$TAILNET_IP:$(port_of "$1")"; }
start_agent() {
    local id="$1"; shift
    "$BIN" agent --id "$id" --failure-domain "d-$id" --bind "tailnet:$(port_of "$id")" \
        --volume "$WORK/$id" "$@" >>"$WORK/$id.log" 2>&1 &
    AGENT_PID[$id]=$!
    PIDS+=("$!")
    for _ in $(seq 1 200); do
        (exec 3<>"/dev/tcp/$TAILNET_IP/$(port_of "$id")") 2>/dev/null && return 0
        sleep 0.05
    done
    echo "agent $id did not start" >&2
    return 1
}
stop_agent() { kill "${AGENT_PID[$1]}"; wait "${AGENT_PID[$1]}" 2>/dev/null || true; }
token() { "$BIN" portal agent token "${P[@]}" --agent-id "$1"; }
join() { DISTR_HNSW_JOIN_TOKEN="$2" "$BIN" portal agent join "${P[@]}" --agent "$(target "$1")"; }
outcome() { if "$@" >>"$WORK/commands.log" 2>&1; then echo accepted; else echo refused; fi; }
# "<status> <refusal code>|<agent's message>" from a body followed by a
# status line; just the status when the body has no refusal code. Checks
# compare the part before "|"; the report keeps the message.
coded() {
    local status="${1##*$'\n'}" body="${1%$'\n'*}" code message
    code="$(jq -r '.code // empty' <<<"$body" 2>/dev/null || true)"
    message="$(jq -r '.error // empty' <<<"$body" 2>/dev/null || true)"
    echo "$status${code:+ $code}${message:+|$message}"
}
# One granted health request from the portal.
request() {
    local status
    status="$("$BIN" portal agent request "${P[@]}" --agent "$(target "$1")" health \
        --out "$WORK/response.json" 2>>"$WORK/commands.log")" || { echo error; return; }
    coded "$(cat "$WORK/response.json")"$'\n'"$status"
}
# The grant on the most recent captured request.
last_grant() { sleep 0.3; grep -ao 'DistrGrant [A-Za-z0-9_-]*' "$WORK/capture.txt" | tail -1 | cut -d' ' -f2; }
replay_local() {
    coded "$(curl -s -w '\n%{http_code}' --max-time 5 -X "$2" \
        -H "Authorization: DistrGrant $1" "http://$TAILNET_IP:$(port_of "$3")$4")"
}
replay_remote() {
    coded "$(remote "curl -s -w '\n%{http_code}' --max-time 10 -X $2 \
        -H 'Authorization: DistrGrant $1' http://$TAILNET_IP:$(port_of "$3")$4" || echo ssh-failed)"
}
join_body() { printf '{"token":"%s","key_set":"%s"}' "$1" "$2"; }
declare -A R EXPECT
check() { R[$1]="$2"; EXPECT[$1]="$3"; }

"$BIN" portal init "${P[@]}" --no-recovery-bundle >"$WORK/init.log" 2>&1
signing() { "$BIN" portal key show-signing "${P[@]}"; }
K1="$(signing | jq -r '.keys[] | select(.status == "active") | .key_id')"

# Join.
T1="$(token a1)"; T2="$(token a2)"; T3="$(token a3)"
DISTR_HNSW_JOIN_TOKEN="$T1" start_agent a1
DISTR_HNSW_JOIN_TOKEN="$T2" start_agent a2
DISTR_HNSW_JOIN_TOKEN="$T3" start_agent a3
check join_token_for_other_agent "$(outcome join a1 "$T2")" refused
join a1 "$T1" >"$WORK/join-a1.json"
check join_records_agent_node "$(jq -r '.agent_node' "$WORK/join-a1.json")" "$SELF_NODE"
check join_token_reuse "$(outcome join a1 "$T1")" refused
sleep 0.3
SNIFFED_TOKEN="$(grep -ao '"token":"[^"]*"' "$WORK/capture.txt" | head -1 | cut -d'"' -f4)"
SNIFFED_SET="$(grep -ao '"key_set":"[^"]*"' "$WORK/capture.txt" | head -1 | cut -d'"' -f4)"
check capture_saw_join_token "$([[ "$SNIFFED_TOKEN" == "$T1" ]] && echo yes || echo no)" yes
check remote_join_sniffed_token "$(coded "$(remote "curl -s -w '\n%{http_code}' --max-time 10 \
    -X POST -H 'Content-Type: application/json' --data '$(join_body "$SNIFFED_TOKEN" "$SNIFFED_SET")' \
    http://$TAILNET_IP:$PORT_A/v1/join")")" "409 already_joined"
check remote_join_guessed_token "$(coded "$(remote "curl -s -w '\n%{http_code}' --max-time 10 \
    -X POST -H 'Content-Type: application/json' --data '$(join_body guess "$SNIFFED_SET")' \
    http://$TAILNET_IP:$PORT_B/v1/join")")" "403 join_refused"
check join_after_guess "$(outcome join a2 "$T2")" accepted
join a3 "$T3" >"$WORK/join-a3.json"
check agent_side_token_reuse "$(coded "$(curl -s -w '\n%{http_code}' --max-time 5 -X POST \
    -H 'Content-Type: application/json' --data "$(join_body "$T3" "$SNIFFED_SET")" \
    "http://$TAILNET_IP:$PORT_C/v1/join")")" "409 already_joined"

# Replay of a captured grant.
AGENTS=(--agent "$(target a1)" --agent "$(target a2)" --agent "$(target a3)")
head -c 300000 /dev/urandom >"$WORK/source.bin"
FILE_ID="$("$BIN" portal put "${P[@]}" "${AGENTS[@]}" --idempotency-key drill "$WORK/source.bin" | tail -1)"
HASH="$(find "$WORK"/a?/objects/chunk -type f ! -name '.*' -printf '%f\n' | head -1)"
check request_k1 "$(request a1)" 200
G1="$(last_grant)"
check replay_same_node "$(replay_local "$G1" GET a1 /v1/health)" 200
check replay_remote "$(replay_remote "$G1" GET a1 /v1/health)" "403 peer_rejected"
check replay_other_agent "$(replay_local "$G1" GET a2 /v1/health)" "403 grant_rejected"
check replay_other_operation "$(replay_local "$G1" GET a1 /v1/inventory/chunk)" "403 grant_rejected"
check replay_as_delete "$(replay_local "$G1" DELETE a1 "/v1/objects/chunk/$HASH")" "403 grant_rejected"

# Rotation: a2 and a3 are offline; a1 takes the new key set.
stop_agent a2
stop_agent a3
K2="$("$BIN" portal key rotate-signing "${P[@]}")"
check request_after_rotation "$(request a1)" 200
G2="$(last_grant)"
check overlap_old_key "$(replay_local "$G1" GET a1 /v1/health)" 200
start_agent a2
check missed_rotation_catch_up "$(request a2)" 200

# Revocation: K1 leaves the key set and the epoch rises.
"$BIN" portal key retire-signing "${P[@]}" --key-id "$K1" --revoke
SIGNING="$(signing)"
check key_set_after_revoke "$(jq -c '{epoch, keys: [.keys[].key_id]}' <<<"$SIGNING")" \
    "$(jq -nc --arg k "$K2" '{epoch: 2, keys: [$k]}')"
# Observation: an agent learns of a revocation on its next portal request.
check revoked_before_contact "$(replay_local "$G1" GET a1 /v1/health)" 200
check revocation_push_a1 "$(request a1)" 200
check revocation_push_a2 "$(request a2)" 200
G3="$(last_grant)"
check revoked_key "$(replay_local "$G1" GET a1 /v1/health)" "409 key_set_outdated"
check stale_epoch "$(replay_local "$G2" GET a1 /v1/health)" "403 grant_rejected"
check replay_remote_current "$(replay_remote "$G3" GET a2 /v1/health)" "403 peer_rejected"
G3_AT="$(date +%s)"

# An agent offline across the revocation cannot be vouched for by the
# revoked key; it rejoins.
start_agent a3
check missed_revocation "$(request a3)" "403 key_set_untrusted"
stop_agent a3
T3B="$(token a3)"
DISTR_HNSW_JOIN_TOKEN="$T3B" start_agent a3 --reset-trust
check rejoin_after_reset "$(join a3 "$T3B" | jq -r '.rejoined')" true
check request_after_rejoin "$(request a3)" 200
"$BIN" portal get "${P[@]}" "${AGENTS[@]}" "$FILE_ID" "$WORK/restored.bin" >>"$WORK/commands.log" 2>&1 || true
check file_intact "$(cmp -s "$WORK/source.bin" "$WORK/restored.bin" && echo yes || echo no)" yes

ZONE_RESTORED=null
if [[ -n "$ZONE" ]]; then
    sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT_A-$PORT_C/tcp" >/dev/null 2>&1 || true
    [[ "$(sudo firewall-cmd --zone="$ZONE" --list-ports)" == "$FIREWALL_BEFORE" ]] \
        && ZONE_RESTORED=true || ZONE_RESTORED=false
fi

# Expiry: the captured grant dies after its lifetime plus the skew allowance
# (grant.rs: 120 s + 60 s).
WAIT=$((G3_AT + 185 - $(date +%s)))
[[ "$WAIT" -le 0 ]] || sleep "$WAIT"
check expired_grant "$(replay_local "$G3" GET a2 /v1/health)" "403 grant_rejected"

VERDICT=pass
[[ "$ZONE_RESTORED" != false ]] || VERDICT=fail
ORDER=(join_token_for_other_agent join_records_agent_node join_token_reuse capture_saw_join_token
    remote_join_sniffed_token remote_join_guessed_token join_after_guess agent_side_token_reuse
    request_k1 replay_same_node replay_remote replay_other_agent replay_other_operation
    replay_as_delete request_after_rotation overlap_old_key missed_rotation_catch_up
    key_set_after_revoke revoked_before_contact revocation_push_a1 revocation_push_a2 revoked_key
    stale_epoch replay_remote_current missed_revocation rejoin_after_reset request_after_rejoin
    file_intact expired_grant)
for check in "${ORDER[@]}"; do
    [[ "${R[$check]%%|*}" == "${EXPECT[$check]}" ]] || VERDICT=fail
done
{
    printf '{\n  "drill": "tailnet-rotation",\n  "utc": "%s",\n  "source_revision": "%s",\n' \
        "$STAMP" "$REVISION"
    printf '  "verdict": "%s",\n  "firewall_restored": %s,\n  "checks": {' "$VERDICT" "$ZONE_RESTORED"
    sep=""
    for check in "${ORDER[@]}"; do
        detail="null"
        [[ "${R[$check]}" != *"|"* ]] || detail="$(jq -Rc . <<<"${R[$check]#*|}")"
        printf '%s\n    "%s": {"expected": %s, "observed": %s, "detail": %s}' "$sep" "$check" \
            "$(jq -Rc . <<<"${EXPECT[$check]}")" "$(jq -Rc . <<<"${R[$check]%%|*}")" "$detail"
        sep=","
    done
    printf '\n  }\n}\n'
} >"$REPORT"
cat "$REPORT"
[[ "$VERDICT" == pass ]]

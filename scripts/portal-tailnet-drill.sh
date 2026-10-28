#!/usr/bin/env bash
# Portal tailnet drill (M2 work packages 4 to 7; docs/m2-implementation-plan.md).
#
# Runs `portal serve` on this node's tailnet address with the node's real
# certificate from tailscaled, and checks from another tailnet node
# ($REMOTE, reached over SSH) that:
#   1. the certificate validates against the remote's system trust store;
#   2. a plaintext request to the portal port gets no HTTP answer;
#   3. WhoIs names the remote node, whatever identity headers it forges;
#   4. its user cannot sign in until added, then can, and the session cookie
#      carries every attribute; CSRF-less and cross-origin writes fail;
#   5. an API key works within its scope and nowhere else.
#
# Certificates need root or the tailnet operator, so `portal serve` runs
# under sudo. If this node has no cached certificate, tailscaled issues one:
# a public Let's Encrypt certificate whose name is recorded in Certificate
# Transparency logs (docs/deployment.md). The drill changes no tailnet
# setting (no serve, Funnel, ACL, or HTTPS toggle). OPEN_FIREWALL=1 adds a
# runtime-only firewalld rule for the drill port on tailscale0 with a 300 s
# timeout and removes it on exit.
#
# Usage: REMOTE=user@other-node [OPEN_FIREWALL=1] scripts/portal-tailnet-drill.sh
set -euo pipefail

REMOTE="${REMOTE:?set REMOTE to the SSH destination of another tailnet node}"
PORT="${PORT:-47320}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$HOME/distr-hnsw-drill/portal-tailnet-$STAMP"
REPORT="$HOME/distr-hnsw-drill/portal-tailnet-report-$STAMP.json"
mkdir -p "$WORK"

(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
REVISION="$(cd "$REPO" && git describe --always --dirty)"
DOMAIN="$(tailscale status --json | jq -r '.CertDomains[0] // empty')"
[[ -n "$DOMAIN" ]] || { echo "tailnet HTTPS certificates are off; nothing to drill" >&2; exit 1; }
# The remote's tailnet address as its SSH session sees it, and the login
# WhoIs reports for it (computed here, independently of the portal).
REMOTE_IP="$(ssh -o BatchMode=yes "$REMOTE" 'echo $SSH_CONNECTION' | awk '{print $3}')"
REMOTE_LOGIN="$(tailscale whois --json "$REMOTE_IP" | jq -r '.UserProfile.LoginName // empty')"
[[ -n "$REMOTE_LOGIN" ]] || {
    echo "SSH to $REMOTE does not run over the tailnet ($REMOTE_IP); cannot name its user" >&2
    exit 1
}
DB="$WORK/portal.sqlite"
KEY="$WORK/master.key"
URL="https://$DOMAIN:$PORT"

PIDS=()
ZONE=""
FIREWALL_BEFORE=""
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    sudo pkill -f "portal serve --database $DB" 2>/dev/null || true
    wait 2>/dev/null || true
    if [[ -n "$ZONE" ]]; then
        sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT/tcp" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ "${OPEN_FIREWALL:-0}" == 1 ]]; then
    ZONE="$(firewall-cmd --get-zone-of-interface=tailscale0)"
    FIREWALL_BEFORE="$(sudo firewall-cmd --zone="$ZONE" --list-ports)"
    sudo firewall-cmd --zone="$ZONE" --add-port="$PORT/tcp" --timeout=300 >/dev/null
fi

# Portal and two loopback agents (the agent tailnet path has its own drill).
"$BIN" portal init --database "$DB" --master-key "$KEY" --no-recovery-bundle >"$WORK/init.log" 2>&1
AGENTS=()
for spec in "a1 d1 47321" "a2 d2 47322"; do
    read -r id domain port <<<"$spec"
    token="$("$BIN" portal agent token --database "$DB" --master-key "$KEY" --agent-id "$id")"
    DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" agent --id "$id" --failure-domain "$domain" \
        --bind "127.0.0.1:$port" --insecure-loopback --volume "$WORK/$id" >"$WORK/$id.log" 2>&1 &
    PIDS+=("$!")
    until (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; do sleep 0.05; done
    DISTR_HNSW_JOIN_TOKEN="$token" "$BIN" portal agent join --database "$DB" \
        --master-key "$KEY" --agent "$id,$domain,http://127.0.0.1:$port" >"$WORK/join-$id.json"
    AGENTS+=(--agent "$id,$domain,http://127.0.0.1:$port")
done
# A user who is not the remote's, so the remote starts unauthorized.
"$BIN" portal user add --database "$DB" --tailscale-login nobody@example.invalid >/dev/null

sudo "$BIN" portal serve --database "$DB" --master-key "$KEY" "${AGENTS[@]}" \
    --bind "tailnet:$PORT" >"$WORK/portal.log" 2>&1 &
PIDS+=("$!")
TAILNET_IP="$(tailscale ip -4 | head -1)"
for _ in $(seq 1 600); do
    (exec 3<>"/dev/tcp/$TAILNET_IP/$PORT") 2>/dev/null && break
    sleep 0.1
done

remote() {
    # shellcheck disable=SC2029
    ssh -o BatchMode=yes "$REMOTE" "$@"
}
status() { remote "curl -s -o /dev/null -w '%{http_code}' --max-time 15 $*" || echo "ssh-failed"; }
declare -A R

# 1. Certificate validation with the remote's own trust store.
R[tls_verified]="$(status "$URL/api/whoami")"
CERT_INFO="$(remote "echo | openssl s_client -connect $DOMAIN:$PORT -servername $DOMAIN 2>/dev/null \
    | openssl x509 -noout -issuer -enddate" 2>/dev/null | tr '\n' ';' || true)"
# 2. Plaintext gets no HTTP answer.
PLAIN="$(remote "curl -s --max-time 10 -o /dev/null -w '%{http_code}' http://$DOMAIN:$PORT/api/whoami" || true)"
R[plaintext]="${PLAIN:-000}"
# 3. WhoIs identity, with forged headers.
WHOAMI="$(remote "curl -s --max-time 15 -H 'Tailscale-User-Login: nobody@example.invalid' \
    -H 'X-Forwarded-User: nobody@example.invalid' $URL/api/whoami")"
R[whoami_login]="$(jq -r '.peer.user_login' <<<"$WHOAMI")"
# 4. Sign-in: refused before the user exists, allowed after.
R[login_unknown]="$(status "-X POST -H 'Tailscale-User-Login: nobody@example.invalid' $URL/api/session")"
sudo "$BIN" portal user add --database "$DB" --tailscale-login "$REMOTE_LOGIN" >/dev/null
LOGIN="$(remote "curl -s --max-time 15 -D - -X POST $URL/api/session")"
COOKIE="$(grep -i '^set-cookie:' <<<"$LOGIN" | head -1 | sed 's/^[Ss]et-[Cc]ookie: //' | tr -d '\r')"
CSRF="$(sed -n '/^\r\?$/,$p' <<<"$LOGIN" | jq -r '.csrf_token' 2>/dev/null || true)"
SESSION="${COOKIE%%;*}"
R[cookie_attributes]="$([[ "$COOKIE" == *"Path=/; Secure; HttpOnly; SameSite=Strict"* ]] && echo ok || echo missing)"
R[write_without_csrf]="$(status "-X POST -H 'Cookie: $SESSION' -H 'Content-Type: application/json' \
    --data '{\"name\":\"x\",\"scopes\":[\"files:read\"]}' $URL/api/keys")"
R[write_cross_origin]="$(status "-X POST -H 'Cookie: $SESSION' -H 'X-CSRF-Token: $CSRF' \
    -H 'Origin: https://evil.example' -H 'Content-Type: application/json' \
    --data '{\"name\":\"x\",\"scopes\":[\"files:read\"]}' $URL/api/keys")"
KEY_JSON="$(remote "curl -s --max-time 15 -X POST -H 'Cookie: $SESSION' -H 'X-CSRF-Token: $CSRF' \
    -H 'Origin: $URL' -H 'Content-Type: application/json' \
    --data '{\"name\":\"drill\",\"scopes\":[\"files:read\"],\"node\":\"self\"}' $URL/api/keys")"
APIKEY="$(jq -r '.key' <<<"$KEY_JSON")"
# 5. The key's scope: read allowed (404 for an unknown file), write refused.
R[key_read_scope]="$(status "-H 'Authorization: Bearer $APIKEY' $URL/api/files/00000000-0000-4000-8000-000000000000")"
R[key_write_scope]="$(status "-X POST -H 'Authorization: Bearer $APIKEY' -H 'Idempotency-Key: k' \
    --data x '$URL/api/files?name=x'")"
R[key_manage_keys]="$(status "-H 'Authorization: Bearer $APIKEY' $URL/api/keys")"

declare -A EXPECT=(
    [tls_verified]=200 [plaintext]=000 [whoami_login]="$REMOTE_LOGIN" [login_unknown]=403
    [cookie_attributes]=ok [write_without_csrf]=403 [write_cross_origin]=403
    [key_read_scope]=404 [key_write_scope]=403 [key_manage_keys]=403
)
VERDICT=pass
for check in "${!EXPECT[@]}"; do
    [[ "${R[$check]}" == "${EXPECT[$check]}" ]] || VERDICT=fail
done
AUDIT_EVENTS="$(sudo "$BIN" portal audit --database "$DB" | jq -sc 'group_by(.event + ":" + .outcome) | map({(.[0].event + ":" + .[0].outcome): length}) | add')"

ZONE_RESTORED=null
if [[ -n "$ZONE" ]]; then
    sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT/tcp" >/dev/null 2>&1 || true
    [[ "$(sudo firewall-cmd --zone="$ZONE" --list-ports)" == "$FIREWALL_BEFORE" ]] \
        && ZONE_RESTORED=true || { ZONE_RESTORED=false; VERDICT=fail; }
fi

{
    printf '{\n  "drill": "portal-tailnet",\n  "utc": "%s",\n  "source_revision": "%s",\n' "$STAMP" "$REVISION"
    printf '  "verdict": "%s",\n  "certificate": "%s",\n  "firewall_restored": %s,\n' \
        "$VERDICT" "${CERT_INFO//\"/}" "$ZONE_RESTORED"
    printf '  "audit_events": %s,\n  "checks": {' "${AUDIT_EVENTS:-null}"
    sep=""
    for check in tls_verified plaintext whoami_login login_unknown cookie_attributes \
        write_without_csrf write_cross_origin key_read_scope key_write_scope key_manage_keys; do
        printf '%s\n    "%s": {"expected": "%s", "observed": "%s"}' "$sep" "$check" \
            "${EXPECT[$check]}" "${R[$check]}"
        sep=","
    done
    printf '\n  }\n}\n'
} >"$REPORT"
cat "$REPORT"
[[ "$VERDICT" == pass ]]

#!/usr/bin/env bash
# Portal tailnet drill (M2 work packages 4 to 7; docs/m2-implementation-plan.md).
#
# Runs `portal serve` on this node's tailnet address and checks, from another
# tailnet node ($REMOTE, reached over SSH) and from a local process on the
# portal's own node, that:
#   1. TLS validates (and, with a private CA, fails without that CA), and a
#      plaintext request to the portal port gets no HTTP answer;
#   2. WhoIs names each caller's node, whatever identity headers it forges;
#      the portal's own node cannot sign in;
#   3. the remote's user cannot sign in until added, then can; the session
#      cookie carries every attribute; CSRF-less and cross-origin writes
#      fail; the cookie replayed from the portal node is refused and revoked;
#   4. an API key works within its scope and only from the node it names.
#
# CERT_SOURCE selects the certificate:
#   private-ca (default)  a drill CA issues the leaf for this node's tailnet
#       name; scripts/localapi-cert-shim.py answers the LocalAPI certificate
#       call and forwards everything else (status, WhoIs) to the real
#       tailscaled. No public issuance, no root for the portal; the remote
#       trusts the drill CA through a temporary file.
#   tailscale  the node's real certificate from tailscaled, under sudo. If
#       the node has no cached certificate, tailscaled issues a public Let's
#       Encrypt certificate whose name is recorded in Certificate Transparency
#       logs (docs/deployment.md); get the tailnet owner's agreement first.
#
# The drill changes no tailnet setting (no serve, Funnel, ACL, or HTTPS
# toggle). OPEN_FIREWALL=1 adds a runtime-only firewalld rule for the drill
# port on tailscale0 with a 300 s timeout and removes it on exit.
#
# Usage: REMOTE=user@other-node [CERT_SOURCE=private-ca|tailscale] [OPEN_FIREWALL=1] \
#            scripts/portal-tailnet-drill.sh
set -euo pipefail

REMOTE="${REMOTE:?set REMOTE to the SSH destination of another tailnet node}"
CERT_SOURCE="${CERT_SOURCE:-private-ca}"
[[ "$CERT_SOURCE" == private-ca || "$CERT_SOURCE" == tailscale ]] \
    || { echo "CERT_SOURCE must be private-ca or tailscale" >&2; exit 2; }
PORT="${PORT:-47320}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$HOME/distr-hnsw-drill/portal-tailnet-$STAMP"
REPORT="$HOME/distr-hnsw-drill/portal-tailnet-report-$STAMP.json"
mkdir -p "$WORK"

(cd "$REPO" && cargo build --release -p distr-hnsw >/dev/null 2>&1)
BIN="$REPO/target/release/distr-hnsw"
REVISION="$(cd "$REPO" && git describe --always --dirty)"
STATUS_JSON="$(tailscale status --json)"
DOMAIN="$(jq -r '.CertDomains[0] // empty' <<<"$STATUS_JSON")"
SELF_NODE="$(jq -r '.Self.ID' <<<"$STATUS_JSON")"
TAILNET_IP="$(tailscale ip -4 | head -1)"
[[ -n "$DOMAIN" ]] || { echo "tailnet HTTPS certificates are off; nothing to drill" >&2; exit 1; }

remote() {
    # shellcheck disable=SC2029
    ssh -o BatchMode=yes "$REMOTE" "$@"
}
# The remote's tailnet address as its SSH session sees it, and the login
# WhoIs reports for it (computed here, independently of the portal).
REMOTE_IP="$(remote 'echo $SSH_CONNECTION' | awk '{print $3}')"
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
REMOTE_CA=""
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    if [[ "$CERT_SOURCE" == tailscale ]]; then
        sudo pkill -f "portal serve --database $DB" 2>/dev/null || true
    fi
    wait 2>/dev/null || true
    if [[ -n "$ZONE" ]]; then
        sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT/tcp" >/dev/null 2>&1 || true
    fi
    [[ -z "$REMOTE_CA" ]] || remote "rm -f '$REMOTE_CA'" 2>/dev/null || true
}
trap cleanup EXIT

if [[ "${OPEN_FIREWALL:-0}" == 1 ]]; then
    ZONE="$(firewall-cmd --get-zone-of-interface=tailscale0)"
    FIREWALL_BEFORE="$(sudo firewall-cmd --zone="$ZONE" --list-ports)"
    sudo firewall-cmd --zone="$ZONE" --add-port="$PORT/tcp" --timeout=300 >/dev/null
fi

# Certificate source. AS_PORTAL runs portal commands as the portal's user.
if [[ "$CERT_SOURCE" == private-ca ]]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \
        -keyout "$WORK/ca.key" -out "$WORK/ca.pem" -subj "/CN=distr-hnsw drill CA $STAMP" \
        -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" \
        2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout "$WORK/leaf.key" -out "$WORK/leaf.csr" -subj "/CN=$DOMAIN" 2>/dev/null
    printf 'subjectAltName=DNS:%s\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n' \
        "$DOMAIN" >"$WORK/leaf.ext"
    openssl x509 -req -in "$WORK/leaf.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
        -CAcreateserial -days 1 -extfile "$WORK/leaf.ext" -out "$WORK/leaf.pem" 2>/dev/null
    cat "$WORK/leaf.key" "$WORK/leaf.pem" >"$WORK/pair.pem"
    python3 -I "$REPO/scripts/localapi-cert-shim.py" "$WORK/localapi.sock" \
        /var/run/tailscale/tailscaled.sock "$WORK/pair.pem" 2>"$WORK/shim.log" &
    PIDS+=("$!")
    until [[ -S "$WORK/localapi.sock" ]]; do sleep 0.05; done
    export DISTR_HNSW_TAILSCALE_SOCKET="$WORK/localapi.sock"
    AS_PORTAL=()
    REMOTE_CA="$(remote 'mktemp -t distr-hnsw-drill-ca.XXXXXX')"
    remote "cat > '$REMOTE_CA'" <"$WORK/ca.pem"
    REMOTE_TLS="--cacert '$REMOTE_CA'"
    LOCAL_TLS=(--cacert "$WORK/ca.pem")
else
    AS_PORTAL=(sudo)
    REMOTE_TLS=""
    LOCAL_TLS=()
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

"${AS_PORTAL[@]}" "$BIN" portal serve --database "$DB" --master-key "$KEY" "${AGENTS[@]}" \
    --bind "tailnet:$PORT" >"$WORK/portal.log" 2>&1 &
PIDS+=("$!")
for _ in $(seq 1 600); do
    (exec 3<>"/dev/tcp/$TAILNET_IP/$PORT") 2>/dev/null && break
    sleep 0.1
done

# Remote requests resolve the portal name to its tailnet address, so the
# check does not depend on the remote's DNS settings.
RCURL="curl -s --max-time 15 --resolve $DOMAIN:$PORT:$TAILNET_IP"
# "<status> <refusal code>", or just the status when the body has no code.
coded() {
    local status="${1##*$'\n'}" body="${1%$'\n'*}" code
    code="$(jq -r '.code // empty' <<<"$body" 2>/dev/null || true)"
    echo "$status${code:+ $code}"
}
status() { coded "$(remote "$RCURL $REMOTE_TLS -w '\n%{http_code}' $*" || echo "ssh-failed")"; }
# A local process on the portal's node (its source address is the node's
# own tailnet address, so WhoIs names the portal node).
lcurl() { curl -s --max-time 15 --resolve "$DOMAIN:$PORT:$TAILNET_IP" "${LOCAL_TLS[@]}" "$@"; }
lstatus() { coded "$(lcurl -w '\n%{http_code}' "$@" || true)"; }
declare -A R EXPECT

# 1. TLS: validates with the remote's trust (the drill CA, or its system
#    store); refused without the drill CA; plaintext gets no HTTP answer.
R[tls_verified]="$(status "$URL/api/whoami")"
EXPECT[tls_verified]=200
if [[ "$CERT_SOURCE" == private-ca ]]; then
    R[tls_untrusted]="$(remote "$RCURL -o /dev/null -w '%{http_code}' $URL/api/whoami" || true)"
    EXPECT[tls_untrusted]=000
    CA_OPT="-CAfile '$REMOTE_CA'"
else
    CA_OPT=""
fi
CERT_INFO="$(remote "echo | openssl s_client -connect $TAILNET_IP:$PORT -servername $DOMAIN $CA_OPT 2>/dev/null \
    | openssl x509 -noout -issuer -enddate" 2>/dev/null | tr '\n' ';' || true)"
PLAIN="$(remote "curl -s --max-time 10 -o /dev/null -w '%{http_code}' http://$TAILNET_IP:$PORT/api/whoami" || true)"
R[plaintext]="${PLAIN:-000}"
EXPECT[plaintext]=000

# 2. WhoIs identity, with forged headers, from the remote and from the
#    portal's own node; the portal node cannot sign in.
WHOAMI="$(remote "$RCURL $REMOTE_TLS -H 'Tailscale-User-Login: nobody@example.invalid' \
    -H 'X-Forwarded-User: nobody@example.invalid' $URL/api/whoami")"
R[whoami_login]="$(jq -r '.peer.user_login' <<<"$WHOAMI")"
EXPECT[whoami_login]="$REMOTE_LOGIN"
LOCAL_WHOAMI="$(lcurl -H "Tailscale-User-Login: $REMOTE_LOGIN" -H "Forwarded: for=$REMOTE_IP" \
    "$URL/api/whoami" || true)"
R[portal_node_whoami]="$(jq -r '.peer.node' <<<"$LOCAL_WHOAMI" 2>/dev/null || echo error)"
EXPECT[portal_node_whoami]="$SELF_NODE"

# 3. Sign-in: refused before the user exists, allowed after; never from the
#    portal node.
R[login_unknown]="$(status "-X POST -H 'Tailscale-User-Login: nobody@example.invalid' $URL/api/session")"
EXPECT[login_unknown]="403 unknown_user"
"${AS_PORTAL[@]}" "$BIN" portal user add --database "$DB" --tailscale-login "$REMOTE_LOGIN" >/dev/null
R[portal_node_sign_in]="$(lstatus -X POST -H "Tailscale-User-Login: $REMOTE_LOGIN" "$URL/api/session")"
EXPECT[portal_node_sign_in]="403 portal_node"
LOGIN="$(remote "$RCURL $REMOTE_TLS -D - -X POST $URL/api/session")"
COOKIE="$(grep -i '^set-cookie:' <<<"$LOGIN" | head -1 | sed 's/^[Ss]et-[Cc]ookie: //' | tr -d '\r')"
CSRF="$(sed -n '/^\r\?$/,$p' <<<"$LOGIN" | jq -r '.csrf_token' 2>/dev/null || true)"
SESSION="${COOKIE%%;*}"
R[cookie_attributes]="$([[ "$COOKIE" == *"Path=/; Secure; HttpOnly; SameSite=Strict"* ]] && echo ok || echo missing)"
EXPECT[cookie_attributes]=ok
R[write_without_csrf]="$(status "-X POST -H 'Cookie: $SESSION' -H 'Content-Type: application/json' \
    --data '{\"name\":\"x\",\"scopes\":[\"files:read\"],\"node\":\"self\"}' $URL/api/keys")"
EXPECT[write_without_csrf]="403 csrf_rejected"
R[write_cross_origin]="$(status "-X POST -H 'Cookie: $SESSION' -H 'X-CSRF-Token: $CSRF' \
    -H 'Origin: https://evil.example' -H 'Content-Type: application/json' \
    --data '{\"name\":\"x\",\"scopes\":[\"files:read\"],\"node\":\"self\"}' $URL/api/keys")"
EXPECT[write_cross_origin]="403 cross_origin"

# 4. An API key bound to the remote: read allowed (404 for an unknown file),
#    write and key management refused, and refused from the portal node.
KEY_JSON="$(remote "$RCURL $REMOTE_TLS -X POST -H 'Cookie: $SESSION' -H 'X-CSRF-Token: $CSRF' \
    -H 'Origin: $URL' -H 'Content-Type: application/json' \
    --data '{\"name\":\"drill\",\"scopes\":[\"files:read\"],\"node\":\"self\"}' $URL/api/keys")"
APIKEY="$(jq -r '.key' <<<"$KEY_JSON")"
R[key_read_scope]="$(status "-H 'Authorization: Bearer $APIKEY' $URL/api/files/00000000-0000-4000-8000-000000000000")"
EXPECT[key_read_scope]="404 file_not_found"
R[key_write_scope]="$(status "-X POST -H 'Authorization: Bearer $APIKEY' -H 'Idempotency-Key: k' \
    --data x '$URL/api/files?name=x'")"
EXPECT[key_write_scope]="403 scope_denied"
R[key_manage_keys]="$(status "-H 'Authorization: Bearer $APIKEY' $URL/api/keys")"
EXPECT[key_manage_keys]="403 session_required"
R[key_other_node]="$(lstatus -H "Authorization: Bearer $APIKEY" \
    "$URL/api/files/00000000-0000-4000-8000-000000000000")"
EXPECT[key_other_node]="403 api_key_wrong_node"

# 5. The remote's session cookie replayed from the portal node is refused
#    and revoked, so it no longer works from the remote either.
R[cookie_replayed_other_node]="$(lstatus -H "Cookie: $SESSION" "$URL/api/keys")"
EXPECT[cookie_replayed_other_node]="401 session_wrong_node"
R[cookie_after_replay]="$(status "-H 'Cookie: $SESSION' $URL/api/keys")"
EXPECT[cookie_after_replay]="401 session_revoked"

AUDIT="$("${AS_PORTAL[@]}" "$BIN" portal audit --database "$DB")"
AUDIT_EVENTS="$(jq -sc 'group_by(.event + ":" + .outcome) | map({(.[0].event + ":" + .[0].outcome): length}) | add' <<<"$AUDIT")"
# Every forged identity header was seen and recorded.
R[forged_headers_audited]="$(jq -s '[.[] | select(.event == "identity_headers")] | length > 0' <<<"$AUDIT")"
EXPECT[forged_headers_audited]=true

VERDICT=pass
for check in "${!EXPECT[@]}"; do
    [[ "${R[$check]}" == "${EXPECT[$check]}" ]] || VERDICT=fail
done
SHIM_CERT_CALLS=null
SHIM_FORWARDED=null
if [[ "$CERT_SOURCE" == private-ca ]]; then
    SHIM_CERT_CALLS="$(grep -c '^cert ' "$WORK/shim.log" || true)"
    SHIM_FORWARDED="$(grep -c '^forward ' "$WORK/shim.log" || true)"
fi

ZONE_RESTORED=null
if [[ -n "$ZONE" ]]; then
    sudo firewall-cmd --zone="$ZONE" --remove-port="$PORT/tcp" >/dev/null 2>&1 || true
    [[ "$(sudo firewall-cmd --zone="$ZONE" --list-ports)" == "$FIREWALL_BEFORE" ]] \
        && ZONE_RESTORED=true || { ZONE_RESTORED=false; VERDICT=fail; }
fi

{
    printf '{\n  "drill": "portal-tailnet",\n  "utc": "%s",\n  "source_revision": "%s",\n' "$STAMP" "$REVISION"
    printf '  "verdict": "%s",\n  "certificate_source": "%s",\n  "certificate": "%s",\n' \
        "$VERDICT" "$CERT_SOURCE" "${CERT_INFO//\"/}"
    printf '  "shim_requests": {"cert": %s, "forwarded": %s},\n' "$SHIM_CERT_CALLS" "$SHIM_FORWARDED"
    printf '  "firewall_restored": %s,\n  "audit_events": %s,\n  "checks": {' \
        "$ZONE_RESTORED" "${AUDIT_EVENTS:-null}"
    sep=""
    for check in tls_verified tls_untrusted plaintext whoami_login portal_node_whoami \
        login_unknown portal_node_sign_in cookie_attributes write_without_csrf \
        write_cross_origin key_read_scope key_write_scope key_manage_keys key_other_node \
        cookie_replayed_other_node cookie_after_replay forged_headers_audited; do
        [[ -n "${EXPECT[$check]+set}" ]] || continue
        printf '%s\n    "%s": {"expected": "%s", "observed": "%s"}' "$sep" "$check" \
            "${EXPECT[$check]}" "${R[$check]}"
        sep=","
    done
    printf '\n  }\n}\n'
} >"$REPORT"
cat "$REPORT"
[[ "$VERDICT" == pass ]]

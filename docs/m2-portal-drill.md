# M2 portal drill

Status: **passed on the reference host, 2026-10-29, from `130df90`**
(`scripts/portal-tailnet-drill.sh`, `CERT_SOURCE=private-ca`). This is the
real-tailnet evidence for M2 work packages 4 to 6: `portal serve` on the
reference host's tailnet address, called from the development laptop and
from a process on the portal's own node. The design is in
[m2-implementation-plan.md](m2-implementation-plan.md).

## Setup

| Item | Value |
|---|---|
| Portal | `portal serve --bind tailnet:47320` as the drill user (no root), with two loopback agents |
| Identity | real: `status` and WhoIs from the reference host's `tailscaled`, through `scripts/localapi-cert-shim.py` (23 requests forwarded) |
| Certificate | a private drill CA's ECDSA P-256 leaf for the host's tailnet name, valid for one day; the shim answered the one certificate call |
| Other node | the development laptop, trusting the drill CA through a temporary file |
| Tailnet changes | none (no serve, Funnel, certificate, HTTPS, or ACL change) |
| Host firewall | a runtime-only rule for 47320/tcp on the `tailscale` zone with a 300 s timeout, removed on exit; the zone was verified restored |

The certificate is private because the owner deferred the public run: the
reference host has no cached certificate, so a fetch through LocalAPI is a
new public issuance, and it would be the first to publish the tailnet's
name in Certificate Transparency logs
([deployment.md](deployment.md#certificates-and-certificate-transparency)).

## Results

Each refusal is checked by status and the portal's refusal code.

| Check | Caller | Expected | Observed |
|---|---|---|---|
| TLS with the drill CA | laptop | 200 | 200 |
| TLS without the drill CA | laptop | no answer (verification fails) | no answer |
| Plaintext HTTP to the portal port | laptop | no HTTP answer | none |
| WhoIs login, with forged `Tailscale-User-Login` and `X-Forwarded-User` | laptop | the laptop's login | the laptop's login |
| WhoIs node, with forged `Tailscale-User-Login` and `Forwarded` | portal node | the portal's node | the portal's node |
| Sign-in before the user is added | laptop | 403 `unknown_user` | 403 `unknown_user` |
| Sign-in from the portal's node | portal node | 403 `portal_node` | 403 `portal_node` |
| Session cookie attributes | laptop | `__Host-`, `Path=/`, `Secure`, `HttpOnly`, `SameSite=Strict` | all present |
| Key creation without the CSRF token | laptop | 403 `csrf_rejected` | 403 `csrf_rejected` |
| Key creation with a foreign `Origin` | laptop | 403 `cross_origin` | 403 `cross_origin` |
| `files:read` key: read an unknown file | laptop | 404 `file_not_found` | 404 `file_not_found` |
| `files:read` key: upload | laptop | 403 `scope_denied` | 403 `scope_denied` |
| `files:read` key: list keys | laptop | 403 `session_required` | 403 `session_required` |
| The laptop's key, from the portal's node | portal node | 403 `api_key_wrong_node` | 403 `api_key_wrong_node` |
| The laptop's session cookie, from the portal's node | portal node | 401 `session_wrong_node` | 401 `session_wrong_node` |
| The same cookie, back on the laptop | laptop | 401 `session_revoked` | 401 `session_revoked` |
| Forged identity headers in the audit log | — | recorded | recorded |

The audit log held one sign-in, one key creation, two refused sign-ins,
seven other refusals (five authentication, two authorization), four
identity-header records, and the two user additions from the CLI.

## Not covered here

- **A publicly trusted certificate from `tailscaled`:** issuance through
  LocalAPI and validation against the laptop's system trust store.
  `CERT_SOURCE=tailscale` runs it once the owner agrees. Renewal, hot
  reload, and expiry without a plaintext fallback are covered by
  `tests/portal_server.rs` against the fake LocalAPI.
- **Tagged nodes and a second user's devices:** the tailnet has one user and
  no tagged nodes. Both are in the authorization matrix
  (`every_route_authorizes_every_kind_of_caller`).
- **ACL enforcement,** which the owner applies to their own tailnet.

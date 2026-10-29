# M2 authorization drill

Status: **passed on the reference host, 2026-10-28, from `25e31be`**
(`scripts/tailnet-authorization-drill.sh`); the rotation and join drill
below passed on 2026-10-29. This is the real-tailnet evidence for M2 work
packages 1 to 3 and the key-rotation and node-join drills of work
package 7: agents bound to a tailnet address,
joined with single-use tokens, accepting only grants signed by the portal
and only from the node they pinned at join. The design is in
[m2-implementation-plan.md](m2-implementation-plan.md).

## Setup

| Item | Value |
|---|---|
| Agents | two, on the reference host's tailnet IPv4 address, ports 47311 and 47312, joined by a portal on the same node |
| Other node | the development laptop, on the same tailnet, reached over SSH |
| Tailnet changes | none (no serve, Funnel, certificate, or ACL change) |
| Host firewall | the reference host's `tailscale` zone admits only SSH, RDP, and 3390/tcp; the drill added a runtime-only rule for 47311-47312/tcp with a 300 s timeout and removed it on exit (`OPEN_FIREWALL=1`); the zone was verified restored |

## Results

| Check | Expected | Observed |
|---|---|---|
| Portal put and get over the tailnet (grants, WhoIs pin) | round trip intact | intact |
| Other node: `GET /v1/health` | 403 | 403 |
| Other node: `GET /v1/inventory/chunk` | 403 | 403 |
| Other node: `GET`, `PUT`, `DELETE /v1/objects/chunk/<stored chunk>` | 403 each | 403 each |
| Other node: `DELETE` with forged `Tailscale-User-Login`, `Tailscale-User-Name`, and `X-Forwarded-For` headers | 403 | 403 |
| Other node: `POST /v1/join` with a guessed token | refused (agent already joined) | 409 |
| Same node, no grant: `DELETE` | 401 | 401 |
| Stored object after the refused deletes | readable through the portal | readable, byte-identical |

The other node's requests are refused by the WhoIs check before the grant
is read: Tailscale names the laptop's node, which is not the portal node the
agent pinned at join. A process on the portal's own node passes that check
and is then refused for lacking a grant.

## Rotation and join drill

Status: **passed on the reference host, 2026-10-29, from `130df90`**
(`scripts/tailnet-rotation-drill.sh`). Three agents ran on the reference
host's tailnet address (ports 47311 to 47313). `tcpdump` on loopback, which
carries traffic to the node's own tailnet address, captured the portal's
requests, so every replay below uses a real grant or a real join token. The
same runtime-only firewall rule as above admitted the laptop for at most
300 s and was verified removed.

Each refusal is checked by status and the agent's refusal code; the agent's
message is quoted where it names the check that failed.

| Check | Expected | Observed |
|---|---|---|
| Join with a token issued for another agent | refused by the portal | "join token was issued for agent a2" |
| Join records the agent's tailnet node | the reference host's node | as expected |
| Reuse of a burned token, by the portal | refused | "join token was already used" |
| Sniffed join token and key set, from the laptop, to the joined agent | 409 `already_joined` | 409 `already_joined` |
| Guessed token with a real key set, from the laptop, to an unjoined agent | 403 `join_refused` | 403 `join_refused` ("join token does not match") |
| The real token after that guess | join succeeds | succeeds |
| The burned token sent straight to its agent | 409 `already_joined` | 409 `already_joined` |
| Captured health grant, replayed from the portal's node | 200 (repeats the operation it names) | 200 |
| The same grant from the laptop | 403 `peer_rejected` | 403 `peer_rejected` ("… is not the portal node") |
| The same grant at another agent | 403 `grant_rejected` | 403 (grant is for agent "a1") |
| The same grant for inventory, or for `DELETE` of a stored chunk | 403 `grant_rejected` | 403 ("grant is for another operation"), each |
| Rotation: the old key's grant during the overlap | 200 | 200 |
| An agent offline across the rotation, on its next request | 200 (the old key vouches for the new set) | 200 |
| Key set after `retire-signing --revoke` | epoch 2, only the new key | as expected |
| The revoked key's grant, before the agent hears from the portal | 200 (observation) | 200 |
| The revoked key's grant, after one portal request to the agent | 409 `key_set_outdated` | 409 ("grant signed by unknown key …") |
| A new-key grant from epoch 1, after the revocation | 403 `grant_rejected` | 403 ("grant epoch 1 is older than the current epoch 2") |
| A current grant from the laptop | 403 `peer_rejected` | 403 `peer_rejected` |
| An agent offline across rotation and revocation | 403 `key_set_untrusted` | 403 ("key set is not signed by a trusted key") |
| That agent, restarted with `--reset-trust` and a new token | rejoins, then 200 | rejoined, 200 |
| The stored file after all of the above | intact | byte-identical |
| A captured grant after its lifetime plus the skew allowance (185 s) | 403 `grant_rejected` | 403 ("grant expired") |

What the observations mean for operators
([deployment.md](deployment.md#routine-operation)):

- **Revocation is lazy.** An agent learns of a revocation on its next
  request from the portal. Until then, a grant captured under the revoked
  key keeps working there for the rest of its lifetime (at most 2 minutes,
  plus 60 seconds of skew). After `--revoke`, send every agent one granted
  request.
- **A missed revocation needs a rejoin.** The new key set is signed only by
  keys still trusted, so an agent that never saw the intermediate set
  refuses it. Restarting it with `--reset-trust` and a new token takes a
  local operator on the agent's host.
- **A revoked-key grant gets 409.** An agent answers a grant under a key it
  does not know with 409 `key_set_outdated`, which tells the real portal to
  push its key set. A replayer gains nothing from that answer: a key-set
  push is accepted only from the pinned portal node, and only if a trusted
  key signed it.

## Other drills with grants enabled

The M1 drills were migrated to joined agents and re-run from clean commits:

| Drill | Revision | Result |
|---|---|---|
| Power loss, btrfs / ext4 / XFS | `25e31be` | 9/9 marks each, master key intact, negative control lost; re-PUT acknowledged 31 / 28 / 27 ms after the crash |
| Capacity (ENOSPC) | `25e31be` | pass: reserve refusal, kernel `ENOSPC` as HTTP 507, recovery after GC |
| Restore (directory target) | `5a8656a` | pass: 56 of 56 kept files verified, 8 deleted unreadable, none resurrected, recovery 3.8 s |
| Lifecycle matrix | `5a8656a` | pass: 160 files (1.87 GB), 24 flips, 179 GC candidates applied after the post-deletion scan, no deleted file readable |

## Limitations

- Both agents ran on one node; the laptop was the only other node, so the
  drill exercises "another tailnet node" but not "the portal on another
  node". The rotation drill has the same shape. Cross-host agents wait for a
  second Linux node; M2 is blocked on them.
- The firewall rule was needed only because the reference host's own
  firewall already blocks the drill ports from the tailnet; ACLs remain the
  operator's outer wall (DESIGN §3).

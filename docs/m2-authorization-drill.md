# M2 authorization drill

Status: **passed on the reference host, 2026-10-28, from `25e31be`**
(`scripts/tailnet-authorization-drill.sh`). This is the real-tailnet
evidence for M2 work packages 1 to 3: agents bound to a tailnet address,
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
  node". Cross-host agents are part of work package 7.
- The firewall rule was needed only because the reference host's own
  firewall already blocks the drill ports from the tailnet; ACLs remain the
  operator's outer wall (DESIGN §3).

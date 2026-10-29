# Deployment guide

**Status: pre-release.** distr-hnsw has no supported production
deployment yet. This guide describes how to run the current blob plane for
evaluation, and the limitations any deployment must carry. Do not let it
hold the only copy of data you cannot lose until your deployment is
recovery ready (below). M1 is accepted, with the limitations below
([roadmap.md](roadmap.md)).

## Limitations that apply to every deployment

- **M2 is in progress** ([m2-implementation-plan.md](m2-implementation-plan.md)).
  Every agent request must carry a capability grant signed by the portal,
  and an agent serves nothing but its join endpoint until it is joined. The
  portal service (`portal serve`) identifies callers with Tailscale WhoIs
  and admits only sessions and API keys, but it has not yet passed its
  live-tailnet certificate drill. The local commands remain: anyone who can
  read the master key and the portal database can operate the cluster.
- **Single host for agents.** Until M2 is accepted, run agents on loopback
  with `--insecure-loopback`. Grants are still checked there, but the
  caller's tailnet node is not. Failure domains are logical (separate disks
  or volumes on one machine), so RF2 protects against a disk failure, not
  against losing the host.
- **Clocks.** Grants expire two minutes after issue, and agents tolerate 60
  seconds of clock difference. Keep portal and agent hosts on NTP.
- **Filesystems.** btrfs, ext4, and XFS passed the power-loss drill on loop
  devices, not on a physical drive's write cache. APFS is unqualified. See
  [m1-filesystem-qualification.md](m1-filesystem-qualification.md).
- **Offsite retention** of deleted generations is enforced by the backup
  target's lifecycle rules and Object Lock retention, not by the portal.
- **Serialized file operations.** `portal serve` runs uploads,
  downloads, and deletes one after another. Periodic jobs
  (scrub, backup) still run as separate commands with a scheduler or the
  built-in `--interval` loops. `portal serve` picks up a signing-key
  rotation on its next file operation, without a restart.

## Components

| Piece | What to provision |
|---|---|
| Agents | One `distr-hnsw agent` per volume, each with a unique `--id`, its own `--failure-domain`, a loopback `--bind` with `--insecure-loopback`, and a `--volume` directory on a qualified filesystem. Optional `--quota-bytes`, `--reserve-bytes`, `--hard-floor-bytes`. A new agent needs a single-use join token (`DISTR_HNSW_JOIN_TOKEN` or `--join-token-file`) and refuses to start without one; once joined it keeps its trust on the volume and restarts without a token. Agents refuse wildcard and non-tailnet addresses. |
| Portal database | One SQLite file (`--database`) on a qualified filesystem. It holds the grant-signing keys (sealed under the master key) and the joined agents. Back it up through `portal backup`, not by copying the file. |
| Master key | A file with mode `0600` (`--master-key`). It encrypts file contents and unseals the grant-signing key, so every command that contacts agents needs it. Its recovery bundle and passphrase must be stored apart from the cluster and from each other. |
| Portal service | `portal serve` on the portal's node: it binds that node's tailnet address (default port 8443) and serves HTTPS with the node's `ts.net` certificate from `tailscaled`, so the tailnet must have HTTPS certificates enabled and the service needs root or the tailnet operator user to fetch them. It refuses loopback and wildcard addresses and never serves plaintext. |
| Backup target | `dir:<path>` on a rotated or remote disk, or `s3:<bucket>[/<prefix>]` with bucket versioning and governance-mode Object Lock. S3 endpoint and credentials come from `DISTR_HNSW_S3_ENDPOINT` and the standard `AWS_*` variables. |

## First run

```sh
cargo build --release -p distr-hnsw
BIN=target/release/distr-hnsw
DB=/var/lib/distr-hnsw/portal.sqlite
KEY=/var/lib/distr-hnsw/master.key

# Portal: creates the master key, the database, and the grant-signing key,
# and prints the recovery bundle once.
$BIN portal init --database $DB --master-key $KEY

# Agents (one per volume / failure domain): issue a single-use token, start
# the agent with it, then join it. Until M2 is accepted, agents run on
# loopback, which must be chosen explicitly.
for n in 1 2; do
  TOKEN=$($BIN portal agent token --database $DB --master-key $KEY --agent-id a$n)
  DISTR_HNSW_JOIN_TOKEN=$TOKEN $BIN agent --id a$n --failure-domain disk-$n \
    --bind 127.0.0.1:710$n --insecure-loopback --volume /srv/dh/a$n &
  until (exec 3<>/dev/tcp/127.0.0.1/710$n) 2>/dev/null; do sleep 0.1; done
  DISTR_HNSW_JOIN_TOKEN=$TOKEN $BIN portal agent join --database $DB --master-key $KEY \
    --agent a$n,disk-$n,http://127.0.0.1:710$n
done
```

Store the printed bundle and passphrase separately and offline. If `init`
fails after creating the key, rerun it: the bundle stays owed until it has
been emitted.

A token is valid for 24 hours and works once. Keep it out of shell history,
logs, and files that outlive the join (`--join-token-file` reads it from a
file). An unjoined agent answers only `POST /v1/join`, and every other
agent route, `/v1/health` included, needs a grant, so supervise agents by
their port or through `portal scrub`, not with an unauthenticated health
probe. An agent whose volume is wiped or replaced needs a new token and a
new join; joining an agent id that was joined before raises the grant
epoch, which invalidates outstanding grants.

Agents are named on every portal command as
`--agent <id>,<failure-domain>,<base-url>`, and every command that contacts
agents takes `--database` and `--master-key`.

## Portal service

```sh
# Who may sign in: a Tailscale login, as tailnet ACLs name it.
$BIN portal user add --database $DB --tailscale-login alice@github

# Serve the API on this node's tailnet address, as a user that may fetch
# the node's certificate (see "Service user" below).
$BIN portal serve --database $DB --master-key $KEY \
  --agent a1,disk-1,http://<agent-1> --agent a2,disk-2,http://<agent-2>
```

- **Identity.** Each connection is resolved with LocalAPI WhoIs on its
  socket address. A peer WhoIs cannot name is dropped. `Tailscale-User-*`,
  `X-Forwarded-*`, `Forwarded`, and similar identity headers are removed,
  logged, and recorded in the audit log; they never grant anything, so do
  not put the portal behind `tailscale serve` or another proxy.
- **Browser sessions.** `POST /api/session` signs in as the user whose
  Tailscale login WhoIs reports. A tagged node cannot sign in, and neither
  can the portal's own node unless `--allow-portal-node-sign-in` is given:
  any process there would pass WhoIs as the node's owner. WhoIs-based
  sign-in means any process on the user's other devices can sign in too;
  ACLs decide which devices reach the portal at all. Sign-ins are limited
  to a burst of 10, then one per 30 seconds, per node. The cookie is
  `__Host-distr-session` (`Secure`, `HttpOnly`, `SameSite=Strict`), lasts 12
  hours, ends after 1 hour unused, and works only from the node that
  created it: presented from another node or as another user it is revoked.
  Signing in again with a live cookie replaces it; `DELETE /api/session`
  signs out; `portal user disable` revokes every session and API key of
  that user. State-changing requests need the session's CSRF token in
  `X-CSRF-Token` (from the sign-in response or `GET /api/whoami`). Sign-in
  itself needs none: it can only create a session for the calling device's
  own identity, and a sign-in with a foreign `Origin` is refused.
- **API keys** are shown once and stored only as a BLAKE3 verifier. Each
  has collection scopes (`files:read`, `files:write`, `<collection>:admin`;
  `files` covers `/api/files`) and names the one tailnet node it may be
  used from (a stable node id from `tailscale status --json`, or `"self"`
  for the node creating it), unless it is explicitly created for any node
  (`any_node`, `--any-node`). Create them from a session (`POST /api/keys`)
  or with `portal api-key create`; send them as
  `Authorization: Bearer dhk_…`. Keys cannot manage keys, read the audit
  log, or open sessions.
- **Audit log.** Sign-ins, refusals, identity headers, key changes, uploads,
  and deletes are appended to the database's `audit_log`, which refuses
  updates and deletes. A state change is recorded before it runs (if that
  record cannot be written, the change is refused) and again when it ends,
  even if the client has disconnected. Refusals and ignored headers are
  recorded per peer at a burst of 30, then one per six seconds; the next
  record notes how many were skipped. Read the log with `portal audit` or
  `GET /api/audit`. It is never trimmed by the portal.
- **Limits.** Request headers must arrive within 10 seconds. A connection
  closes after 10 minutes (after its current request), so a client
  reconnects and WhoIs resolves it again. One address may hold 32
  connections and the portal 512. Four uploads and four downloads run at
  once; an upload is refused when the spool's filesystem would keep less
  than 1 GiB free. Uploads and downloads pass through `<database>.spool`
  (mode `0700`): downloads sit there decrypted while they are sent, and
  leftovers are removed when the portal starts.
- **Certificates.** The portal fetches `<node>.<tailnet>.ts.net` from
  `tailscaled` at start, renews when a third of its lifetime remains, and
  swaps the new certificate in without a restart. If renewal keeps failing
  and the certificate expires, every TLS handshake fails until a fetch
  succeeds; the portal does not fall back to plaintext.
- **Supervision.** Like agents, the portal exits with status 75 when its
  tailnet address changes; restart it (`Restart=on-failure`, `After=
  tailscaled.service`).
- **Service user.** Fetching the node's certificate needs root or the
  user `tailscaled` names as its operator. Prefer a dedicated account set
  with `tailscale set --operator=<account>`, knowing the operator can also
  reconfigure `tailscaled`; otherwise run as root under systemd with
  filesystem protections. The reference drills run it with `sudo` only to
  leave the reference host's operator setting unchanged.
- **Restores.** Users, sessions, API keys, and the audit log live in the
  portal database. A database restored from a snapshot brings back the
  keys and sessions of that moment: revoke again any key revoked since,
  and sign in afresh. A database rebuilt by `portal recover` alone has no
  users or keys.

## Tailnet ACLs

ACLs are the outer wall (DESIGN §3); the portal and agents still check
WhoIs and grants behind them. distr-hnsw does not edit your tailnet policy.
A policy that matches the design lets only your own devices reach the
portal port and only the portal node reach agent ports, for example with
the portal and agents on tagged nodes:

```json
{
  "tagOwners": {
    "tag:distr-portal": ["autogroup:admin"],
    "tag:distr-agent": ["autogroup:admin"]
  },
  "acls": [
    { "action": "accept", "src": ["alice@github"], "dst": ["tag:distr-portal:8443"] },
    { "action": "accept", "src": ["tag:distr-portal"], "dst": ["tag:distr-agent:7101-7199"] }
  ]
}
```

Tagging a node changes its identity: WhoIs then reports the tag, not a
user, so nobody can open a browser session from a tagged node. A host
firewall on `tailscale0` must also admit the portal and agent ports.

## Certificates and Certificate Transparency

Every certificate `tailscaled` issues is a public Let's Encrypt certificate
for `<node>.<tailnet>.ts.net`, and every public certificate is recorded in
Certificate Transparency logs. Issuing one therefore publishes the node's
name and your tailnet's `ts.net` name permanently, though not its addresses
or any route to it. Name nodes accordingly before the first `portal serve`,
and expect the name to stay public after the node is gone.

## Routine operation

| Task | Command | Cadence |
|---|---|---|
| Store / fetch / delete | `portal put`, `portal get`, `portal delete` (each mutation takes `--idempotency-key`) | as needed |
| Verify and repair | `portal scrub --repair` | continuously (`--interval`) |
| Backup | `portal backup --target … --verify-sample N` | every few minutes (`--interval`) |
| Health | `portal health` (no agent contact, no master key) | monitoring |
| Garbage collection | `portal gc` (plan), then `portal gc --apply` | periodically, after a fresh scrub |
| Add an agent | `portal agent token`, start the agent with the token, `portal agent join` | as needed |
| Replace a disk | `portal drain --agent-id X`, then `portal retire --agent-id X`; join the replacement as a new agent | as needed |
| Rotate the grant-signing key | `portal key rotate-signing`, then `portal key retire-signing --key-id <old>` once agents use the new key (`--revoke` if the old key may be compromised) | periodically, or on suspected compromise |

Every command in this table except `portal health` takes `--master-key`.

An agent learns of a rotation or revocation on its next request from the
portal. An agent offline across a rotation catches up then, because the old
key vouches for the new key set. After `retire-signing --revoke`, send every
agent one granted request (`portal agent request --agent … health`, or a
`portal scrub`) so each refuses outstanding grants at once; until then a
captured grant still works there for the rest of its lifetime (2 minutes,
plus 60 seconds of clock skew). An agent offline across both a rotation and
the revocation trusts no key in the new key set: restart it once with
`--reset-trust` and a new token, and join it again
([m2-authorization-drill.md](m2-authorization-drill.md)).

GC refuses to collect while any agent holds a manifest or deletion marker
the database does not know. It also waits for backup coverage of deleted
generations, and refuses to run on a restored database that has not been
recovered.

## Recovery ready

A deployment may be called recovery ready only after an
empty-infrastructure restore drill has passed against **its own** backup
target:

```sh
scripts/restore-drill.sh --target s3:<your-bucket>/<drill-prefix>
distr-hnsw portal drill record --database /var/lib/distr-hnsw/portal.sqlite \
  --report ~/distr-hnsw-drill/restore-report-<utc>.json
distr-hnsw portal health --database /var/lib/distr-hnsw/portal.sqlite
```

`portal health` then reports `recovery_ready: true` only when the drill
passed within its declared RPO/RTO, completed within the last 92 days, ran
against the same bucket (or directory) as a configured target, and that
target's backup lag is within the declared blob RPO. Repeat the drill before
every release and at least quarterly.

## Restoring after loss

1. Restore the master key from the recovery bundle (`portal key restore`).
2. `portal restore metadata --target … --database <new>` (marks the
   database recovery pending).
3. If the agents' volumes were lost too, start replacement agents on empty
   volumes with tokens from the restored database (`portal agent token
   --database <new>`), join them, then run `portal restore objects --target …
   --database <new> --master-key … --agent …`.
4. `portal recover --apply` with the restored key and every agent. This
   rebuilds file records newer than the snapshot and clears the
   recovery-pending flag.
5. `portal scrub --repair`, then verify downloads.

GC, drain, and retire refuse to run between steps 2 and 4.

If the portal database is lost and no metadata snapshot exists, a new
database has a new grant-signing key that the surviving agents do not
trust. On each agent's host, restart the agent once with a new token and
`--reset-trust`, which forgets the old portal; join it, then run
`portal recover --apply`. Only someone who can start the agent on its
volume can do this, and that person already controls the volume's data.

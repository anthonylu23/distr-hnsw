# Deployment guide

**Status: pre-release.** distr-hnsw has no supported production
deployment yet. This guide describes how to run the current blob plane for
evaluation, and the limitations any deployment must carry. Do not let it
hold the only copy of data you cannot lose until your deployment is
recovery ready (below). M1 is accepted, with the limitations below
([roadmap.md](roadmap.md)).

## Limitations that apply to every deployment

- **Agent authorization only.** Every agent request must carry a capability
  grant signed by the portal, and an agent serves nothing but its join
  endpoint until it is joined (M2, [m2-implementation-plan.md](m2-implementation-plan.md)).
  There is no portal daemon, user login, or API key yet: anyone who can read
  the master key and the portal database can operate the cluster.
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
- **No portal daemon.** The portal is a set of one-shot commands sharing one
  SQLite database. Run periodic jobs (scrub, backup) with a scheduler or the
  built-in `--interval` loops.

## Components

| Piece | What to provision |
|---|---|
| Agents | One `distr-hnsw agent` per volume, each with a unique `--id`, its own `--failure-domain`, a loopback `--bind` with `--insecure-loopback`, and a `--volume` directory on a qualified filesystem. Optional `--quota-bytes`, `--reserve-bytes`, `--hard-floor-bytes`. A new agent needs a single-use join token (`DISTR_HNSW_JOIN_TOKEN` or `--join-token-file`) and refuses to start without one; once joined it keeps its trust on the volume and restarts without a token. Agents refuse wildcard and non-tailnet addresses. |
| Portal database | One SQLite file (`--database`) on a qualified filesystem. It holds the grant-signing keys (sealed under the master key) and the joined agents. Back it up through `portal backup`, not by copying the file. |
| Master key | A file with mode `0600` (`--master-key`). It encrypts file contents and unseals the grant-signing key, so every command that contacts agents needs it. Its recovery bundle and passphrase must be stored apart from the cluster and from each other. |
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

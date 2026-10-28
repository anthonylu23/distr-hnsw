# Deployment guide

**Status: pre-release.** distr-hnsw has no supported production
deployment yet. This guide describes how to run the current blob plane for
evaluation, and the limitations any deployment must carry. Do not let it
hold the only copy of data you cannot lose until your deployment is
recovery ready (below). M1 is accepted, with the limitations below
([roadmap.md](roadmap.md)).

## Limitations that apply to every deployment

- **No authentication.** Agents bind to loopback only and accept any request
  from the local host, including `DELETE`. On a shared machine, any local
  user can delete stored objects. Tailscale identity and authorization
  arrive in M2.
- **Single host for agents.** Because agents are loopback-only, failure
  domains are logical (separate disks or volumes on one machine) until M2.
  RF2 across volumes on one host protects against a disk failure, not
  against losing the host.
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
| Agents | One `distr-hnsw agent` per volume, each with a unique `--id`, its own `--failure-domain`, a loopback `--bind` with `--insecure-loopback`, and a `--volume` directory on a qualified filesystem. Optional `--quota-bytes`, `--reserve-bytes`, `--hard-floor-bytes`. Agents refuse wildcard and non-tailnet addresses, and refuse to serve on a tailnet address until capability grants land (M2). |
| Portal database | One SQLite file (`--database`) on a qualified filesystem. Back it up through `portal backup`, not by copying the file. |
| Master key | A file with mode `0600` (`--master-key`). Its recovery bundle and passphrase must be stored apart from the cluster and from each other. |
| Backup target | `dir:<path>` on a rotated or remote disk, or `s3:<bucket>[/<prefix>]` with bucket versioning and governance-mode Object Lock. S3 endpoint and credentials come from `DISTR_HNSW_S3_ENDPOINT` and the standard `AWS_*` variables. |

## First run

```sh
cargo build --release -p distr-hnsw
BIN=target/release/distr-hnsw

# Agents (one per volume / failure domain). Until M2's capability grants
# land, agents serve only on loopback, which must be chosen explicitly.
$BIN agent --id a1 --failure-domain disk-1 --bind 127.0.0.1:7101 --insecure-loopback --volume /srv/dh/a1 &
$BIN agent --id a2 --failure-domain disk-2 --bind 127.0.0.1:7102 --insecure-loopback --volume /srv/dh/a2 &

# Portal: creates the key and database and prints the recovery bundle once.
$BIN portal init --database /var/lib/distr-hnsw/portal.sqlite \
  --master-key /var/lib/distr-hnsw/master.key
```

Store the printed bundle and passphrase separately and offline. If `init`
fails after creating the key, rerun it: the bundle stays owed until it has
been emitted.

Agents are named on every portal command as
`--agent <id>,<failure-domain>,<base-url>`.

## Routine operation

| Task | Command | Cadence |
|---|---|---|
| Store / fetch / delete | `portal put`, `portal get`, `portal delete` (each mutation takes `--idempotency-key`) | as needed |
| Verify and repair | `portal scrub --repair` | continuously (`--interval`) |
| Backup | `portal backup --target … --verify-sample N` | every few minutes (`--interval`) |
| Health | `portal health` (no agent contact) | monitoring |
| Garbage collection | `portal gc` (plan), then `portal gc --apply` | periodically, after a fresh scrub |
| Replace a disk | `portal drain --agent-id X`, then `portal retire --agent-id X` | as needed |

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
3. If the agents' volumes were lost too, run
   `portal restore objects --target … --agent …`.
4. `portal recover --apply` with the restored key and every agent. This
   rebuilds file records newer than the snapshot and clears the
   recovery-pending flag.
5. `portal scrub --repair`, then verify downloads.

GC, drain, and retire refuse to run between steps 2 and 4.

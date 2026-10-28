# M1 empty-infrastructure restore drill

Status: **passed on the reference host with both the versioned-directory target and
the S3-compatible target (MinIO with Object Lock), re-run from the clean
hardening revision `dc276f3` on 2026-10-27.** Both reports were then
recorded with `portal drill record`, which derived `recovery_ready` from
them.

This is the evidence package the roadmap requires for the M1 exit gate: a
representative cluster is destroyed and rebuilt into empty infrastructure
from the backup set, the recovery bundle, and an off-cluster passphrase, and
every committed regular file is downloaded byte for byte. The drill is
automated by [`scripts/restore-drill.sh`](../scripts/restore-drill.sh);
full reports stay under `~/distr-hnsw-drill/` on the reference host and are not
committed because they contain file identifiers and local paths.

## Procedure

1. Build the release binary; start three loopback agents in three failure
   domains; `portal init` with an operator-supplied passphrase and keep only
   the printed recovery bundle and the passphrase outside the cluster.
2. Commit random files of 1 KiB to 8 MiB, deleting every eighth one after
   commit, so the backup set carries both live and superseded generations.
3. `portal scrub`, then `portal backup` twice (the second run must copy
   nothing and skip the snapshot).
4. Kill every agent and delete the database, the master key, and all
   volumes. The loss timestamp starts the recovery clock.
5. `portal key restore` from the bundle and passphrase; `portal restore
   metadata` from the newest snapshot; start three fresh agents on empty
   volumes; `portal restore objects`; `portal recover --apply`. The clock
   stops when recovery converges, because files are downloadable from then.
6. `portal scrub --repair` to restore desired placement on every configured
   agent (restore places the two-domain floor only).
7. Download every kept file and compare SHA-256 to the original; confirm
   every deleted file is still unreadable; final scrub must report every
   required object durable.

## Result (run `20261027T014106Z`, directory target)

| Field | Value |
|---|---|
| Host, kernel | reference host, `7.1.9-200.fc44.x86_64` |
| Source revision | `dc276f3` (clean) |
| Backup target | versioned directory (`dir:`), local disk |
| Corpus | 64 files, 293,134,603 bytes; 56 kept, 8 deleted; 3 agents |
| Backup set | 175 objects copied and read-back verified; second run copied 0, snapshot skipped; 0 pending before loss |
| Restore | 175 objects placed to 2 domains each (350 placements, 0 failed); recovery converged 64/64 files, 0 blocked |
| Desired placement | final health 155 required objects durable, 0 degraded, 0 at risk, 0 lost |
| Verification | 56/56 kept files match SHA-256; 8/8 deleted files unreadable; 0 resurrected; no missing control metadata |
| Report digest (SHA-256, first 16) | `8aae9284fd88556b` |

| Objective | Declared | Actual |
|---|---|---|
| Metadata RPO | 300 s (snapshot interval) | 2.5 s from last commit to verified backup |
| Blob RPO | 900 s | 2.5 s |
| Portal-loss RTO | 600 s once recovery material is at hand | 3.9 s from loss to converged recovery |

Step timings (seconds): commit 7.3, backup 2.5, destroy 0.2, key restore
0.1, restore metadata 0.0, restore objects 2.6, recover 1.0, scrub repair
2.3, verify 3.3.

## Result with the S3-compatible target (run `20261027T014126Z`)

Same procedure against MinIO `RELEASE.2025-09-07T16-13-09Z` on this machine,
bucket created with Object Lock enabled (versioning on, no default retention,
which the adapter warns about), prefix `site-a`, source revision `dc276f3`.

| Field | Value |
|---|---|
| Corpus | 64 files, 243,636,328 bytes; 56 kept, 8 deleted; 3 agents |
| Backup set | 165 objects copied and read-back verified through S3; second run copied 0 and skipped the snapshot |
| Restore | 165 objects placed to 2 domains each (330 placements, 0 failed); recovery converged 64/64, 0 blocked |
| Verification | 56/56 kept files match SHA-256; 8/8 deleted files unreadable; 0 resurrected; final health 145 durable |
| Actual recovery point lag | 3.3 s |
| Actual recovery time | 3.75 s |
| Report digest (SHA-256, first 16) | `6f38c3a39b4712a4` |

Step timings (seconds): commit 6.8, backup 3.3, key restore 0.1, restore
metadata 0.0, restore objects 2.6, recover 0.8, scrub repair 2.0, verify
2.9. `scripts/minio-test.sh test` passed 3/3 against the same MinIO.

## Recording the drill

Each report was recorded into a copy of its own restored database with
`portal drill record`:

- directory report into the directory deployment: `recovery_ready: true`
  ("passed against dir:…/offsite; backup lag 47s within RPO 900s");
- S3 report into the S3 deployment: `recovery_ready: true` (lag 27 s);
- S3 report into the directory deployment: `recovery_ready: false`
  ("restore drill target s3:… is not a configured backup target").

A freshly restored database lists its backup target with every object
pending and the snapshotting job `interrupted`. The snapshot is taken
before objects are copied, so it cannot record their verification. The
next `portal backup` re-verifies the objects as identical on the target
(none are copied again). Until then, lag grows from the restore time. GC
also refuses to collect deleted generations until the target holds
verified copies again.

## Earlier runs

The 2026-09-30 runs (`20260930T000508Z` directory, revision `9862462`;
`20260930T001413Z` S3, revision `c6228e7`) passed with 7.7 s and 4.2 s
recovery time. They predate the hardening pass and are superseded.

## What this proves and what it does not

- The backup set, the bundle, and the passphrase are sufficient to rebuild
  a portal and its files with no surviving cluster machine, key file, or
  database, and deleted files stay deleted through the rebuild.
- Recovery rebinds placements to the new agent incarnations; the old
  incarnations are superseded, not trusted.
- Both targets ran on the same machine. The directory run demonstrates the
  layout and restore path; the S3 run demonstrates the offsite adapter
  end to end against an Object-Lock-enabled bucket, but a MinIO on the same
  host is not offsite. A deployment is recovery ready only after this drill
  passes against its real offsite bucket.
- The corpus is hundreds of megabytes, not the multi-gigabyte scale of the
  v1 operating envelope. Timings will scale with object count and size.

## Rerunning

```sh
scripts/restore-drill.sh --files 64 --max-mib 8
scripts/restore-drill.sh --target s3:bucket/prefix   # with DISTR_HNSW_S3_ENDPOINT and AWS_* set
```

The script exits 0 only on a `pass` verdict and prints the report path.

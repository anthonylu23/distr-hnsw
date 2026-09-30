# M1 empty-infrastructure restore drill

Status: **passed on `anthonypc` with both the versioned-directory target and
the S3-compatible target (MinIO with Object Lock), 2026-09-30.**

This is the evidence package the roadmap requires for the M1 exit gate: a
representative cluster is destroyed and rebuilt into empty infrastructure
from the backup set, the recovery bundle, and an off-cluster passphrase, and
every committed regular file is downloaded byte for byte. The drill is
automated by [`scripts/restore-drill.sh`](../scripts/restore-drill.sh);
full reports stay under `~/distr-hnsw-drill/` on `anthonypc` and are not
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

## Result (run `20260930T000508Z`)

| Field | Value |
|---|---|
| Host, kernel | `anthonypc`, `7.1.9-200.fc44.x86_64` |
| Source revision | `9862462` |
| Backup target | versioned directory (`dir:`), local disk |
| Corpus | 64 files, 247,273,429 bytes; 56 kept, 8 deleted; 3 agents |
| Backup set | 164 objects copied and read-back verified; second run copied 0, snapshot skipped; 0 pending before loss |
| Restore | 164 objects placed to 2 domains each (328 placements, 0 failed); recovery converged 64/64 files, 0 blocked |
| Desired placement | scrub repair applied 147 copies; final health 147 durable, 0 degraded, 0 at risk, 0 lost |
| Verification | 56/56 kept files match SHA-256; 8/8 deleted files unreadable; 0 resurrected; no missing control metadata |
| Report digest (SHA-256, first 16) | `149ea90df3768139` |

| Objective | Declared | Actual |
|---|---|---|
| Metadata RPO | 300 s (snapshot interval) | 3.7 s from last commit to verified backup |
| Blob RPO | 900 s | 3.7 s |
| Portal-loss RTO | 600 s once recovery material is at hand | 7.7 s from loss to converged recovery |

Step timings (seconds): commit 7.3, backup 3.7, destroy 0.2, key restore
0.1, restore metadata 0.02, restore objects 4.3, recover 3.1, scrub repair
2.8, verify 3.8.

## Result with the S3-compatible target (run `20260930T001413Z`)

Same procedure against MinIO `RELEASE.2025-09-07T16-13-09Z` on this machine,
bucket created with Object Lock enabled (versioning on, no default retention,
which the adapter warns about), prefix `site-a`, source revision `c6228e7`.

| Field | Value |
|---|---|
| Corpus | 64 files, 253,674,286 bytes; 56 kept, 8 deleted; 3 agents |
| Backup set | 164 objects copied and read-back verified through S3; second run copied 0 and skipped the snapshot |
| Restore | 164 objects placed to 2 domains each (328 placements, 0 failed); recovery converged 64/64, 0 blocked |
| Verification | 56/56 kept files match SHA-256; 8/8 deleted files unreadable; final health all durable |
| Actual recovery point lag | 3.9 s |
| Actual recovery time | 4.2 s |
| Report digest (SHA-256, first 16) | `bb955c0b3a8bc149` |

Step timings (seconds): commit 6.8, backup 3.9, key restore 0.1, restore
metadata 0.02, restore objects 2.6, recover 1.3, scrub repair 2.4, verify
3.0.

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

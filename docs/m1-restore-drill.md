# M1 empty-infrastructure restore drill

Status: **passed on `anthonypc` with the versioned-directory target
(2026-09-30)**. The S3-compatible offsite rerun is pending that adapter.

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

## What this proves and what it does not

- The backup set, the bundle, and the passphrase are sufficient to rebuild
  a portal and its files with no surviving cluster machine, key file, or
  database, and deleted files stay deleted through the rebuild.
- Recovery rebinds placements to the new agent incarnations; the old
  incarnations are superseded, not trusted.
- The target was a local directory on the same machine. It demonstrates the
  layout and the restore path, not offsite protection; the drill must be
  repeated with the S3-compatible target before the deployment may be
  called recovery ready.
- The corpus is hundreds of megabytes, not the multi-gigabyte scale of the
  v1 operating envelope. Timings will scale with object count and size.

## Rerunning

```sh
scripts/restore-drill.sh --files 64 --max-mib 8
scripts/restore-drill.sh --target s3:bucket/prefix   # with DISTR_HNSW_S3_ENDPOINT and AWS_* set
```

The script exits 0 only on a `pass` verdict and prints the report path.

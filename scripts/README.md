# scripts/

Operational scripts that are not part of the product binary.

## power-loss-drill.sh

Filesystem power-loss qualification for the agent object store, using
`dm-log-writes` on loop devices. Background, results, and the reasoning behind
the procedure are in
[docs/m1-filesystem-qualification.md](../docs/m1-filesystem-qualification.md).

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p distr-hnsw
scripts/power-loss-drill.sh btrfs     # also: ext4, xfs
scripts/power-loss-drill.sh ext4 --files 16 --keep   # more marks; keep images
```

Requirements (all present on `anthonypc`): passwordless `sudo`, the
`dm-log-writes` module, `mkfs.<fs>`, `dmsetup`, `losetup`, `blkdiscard`,
`jq`, `curl`, `gcc`, and network access on first run to shallow-clone
xfstests for `replay-log` (compiled into `$HOME/distr-hnsw-drill/replay-log`).

What it does:

1. Creates two 4 GiB sparse images under `$HOME/distr-hnsw-drill/run-<fs>-<utc>/`,
   attaches them as loop devices, stacks `dm-log-writes` (`dhq-<fs>-<pid>`) on
   the data device, runs `mkfs.<fs>`, marks `mkfs`, mounts with default options.
2. Starts one agent on the dm-backed volume and a second on a plain directory
   (RF2 needs two failure domains; only the first is under test), inits a
   portal database outside the mount, and uploads 8 files (3 KiB to 6 MiB).
   After each acknowledged `portal put` it inserts mark `put-<i>` and
   snapshots the agent's `/v1/inventory/*`.
3. Writes a 1 MiB file with no `fsync` and marks `unsynced-control`, then
   `kill -9`s the agent, marks `end`, unmounts, and removes the dm device.
4. For each `put-<i>`: `blkdiscard`s the data device, replays the log to the
   mark, mounts the device, starts a fresh agent on it, requires
   `GET /v1/objects/{kind}/{hash}` to return 200 for every snapshotted object
   (the agent re-hashes on GET), counts surviving `.*.tmp` files, unmounts,
   and runs the read-only checker (`btrfs check --readonly`, `e2fsck -fn`,
   `xfs_repair -n`). Finally replays to `unsynced-control` and requires the
   control file to be missing or short.
5. Writes `$HOME/distr-hnsw-drill/report-<fs>-<utc>.json` (kernel, mkfs and
   dm versions, mount options, `queue/write_cache`, per-mark results, wall
   clock). Exit status is 0 only if every mark passed and the control showed
   loss.

Safety: every `dmsetup`/`mkfs`/`mount`/`blkdiscard`/`replay-log` call is
preceded by a printed check that the target is a loop device attached to an
image the script created (`losetup -j`, and the dm table's major:minor
numbers). An `EXIT` trap kills agents, unmounts, removes the dm device,
detaches the loops, deletes the images (unless `--keep`), and prints the
leftover count from `losetup -a` and `dmsetup ls`. Nothing outside
`$HOME/distr-hnsw-drill/` is touched.

Do not run this on the laptop; it needs root and a Linux block layer. It
takes about 10 seconds per filesystem on `anthonypc`.

## restore-drill.sh

Empty-infrastructure restore drill (roadmap M1 exit gate). Builds a
three-agent cluster, commits and deletes files, backs up, destroys the
database, key, and volumes, and rebuilds from the backup set, the recovery
bundle, and the passphrase; verifies every kept file by SHA-256 and every
deleted file stays unreadable; records declared vs. actual RPO/RTO. Results
and interpretation: [docs/m1-restore-drill.md](../docs/m1-restore-drill.md).

```sh
scripts/restore-drill.sh --files 64 --max-mib 8            # local directory target
scripts/restore-drill.sh --target s3:bucket/prefix          # offsite target
```

Needs no root. Writes `$HOME/distr-hnsw-drill/restore-report-<utc>.json`
and exits 0 only on a `pass` verdict.

# scripts/

Operational scripts that are not part of the product binary.

## power-loss-drill.sh

Filesystem power-loss qualification for the agent object store, using
`dm-log-writes` on loop devices. Background, results, and the reasoning behind
the procedure are in
[docs/m1-filesystem-qualification.md](../docs/m1-filesystem-qualification.md).

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p distr-hnsw --features failpoints
scripts/power-loss-drill.sh btrfs     # also: ext4, xfs
scripts/power-loss-drill.sh ext4 --files 16 --keep   # more marks; keep images
```

Requirements (all present on `anthonypc`): passwordless `sudo`, the
`dm-log-writes` module, `mkfs.<fs>`, `dmsetup`, `losetup`, `blkdiscard`,
`jq`, `curl`, `gcc`, and network access on first run to shallow-clone
xfstests for `replay-log` (compiled into `$HOME/distr-hnsw-drill/replay-log`).
The binary must be built with `--features failpoints`: the re-PUT phase
stops an agent between a PUT's rename and its directory sync through
`DISTR_HNSW_AGENT_FAILPOINT=put_after_rename`, and the script refuses a
build without failpoints.

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
and exits 0 only on a `pass` verdict. A deployment becomes recovery ready
only after a passing drill against its own backup target (same bucket, or
same directory) is recorded into its database:

```sh
distr-hnsw portal drill record --database /var/lib/distr-hnsw/portal.sqlite \
  --report ~/distr-hnsw-drill/restore-report-<utc>.json
```

## lifecycle-matrix.sh

Large corruption, movement, retirement, and GC matrix: commits ~2 GB across
three agents, injects byte flips, truncations, and removals, repairs, drains
and retires an agent, joins a replacement, deletes files, and garbage-collects
with proofs. It then retires an agent after GC, and replaces the database
with a snapshot taken mid-upload to check that GC collects nothing until
`recover --apply` has reconciled it and that the acknowledged file survives.
It builds the release binary with `--features failpoints` to stop that
upload. Results: [docs/m1-lifecycle-matrix.md](../docs/m1-lifecycle-matrix.md).

```sh
scripts/lifecycle-matrix.sh --files 160 --max-mib 24 --flip 24
```

## enospc-drill.sh

Capacity and real-`ENOSPC` failure injection on a small ext4 loop device
(needs sudo). Results: [docs/m1-capacity-drill.md](../docs/m1-capacity-drill.md).

## minio-test.sh

Runs the S3 backup target tests against a throwaway MinIO
(`start|test|stop`, or no argument for all three). The tests are
`#[ignore]`d so a plain `cargo test` never counts them as passing; this
script runs them with `--ignored`. Used by the S3 leg of the
restore drill: start MinIO, create an Object-Lock bucket, run
`restore-drill.sh --target s3:<bucket>/<prefix>` with the MinIO credentials in
the environment, then stop.

## bench/convert-annb.py

Converts an ann-benchmarks HDF5 dataset into the flat pinned layout the bench
harness reads (`base.f32le`, `queries.f32le`, `groundtruth.i32le`,
`manifest.json` with BLAKE3 digests). Run once per dataset inside the venv at
`~/distr-hnsw-bench/.venv` (`h5py`, `numpy`, `blake3`); commit only the
manifest under `docs/bench/`.

```sh
~/distr-hnsw-bench/.venv/bin/python scripts/bench/convert-annb.py \
  ~/distr-hnsw-bench/downloads/sift-128-euclidean.hdf5 \
  ~/distr-hnsw-bench/datasets/sift-128-euclidean \
  --name sift-128-euclidean --source https://ann-benchmarks.com/sift-128-euclidean.hdf5
cargo run --release -p distr-hnsw-bench -- verify ~/distr-hnsw-bench/datasets/sift-128-euclidean
cargo run --release -p distr-hnsw-bench -- oracle ~/distr-hnsw-bench/datasets/sift-128-euclidean --k 10
```

## bench/build-project-set.py

Builds `project-nomic-512`, the only bench set at the product's dimension:
text chunks embedded with the locked phase-0 model and recipe
(`nomic-embed-text` through Ollama, `search_document: ` / `search_query: `
prefixes, truncated from 768 to 512 dimensions and L2-normalized). Sources,
in stream order: the phase-0 mixed corpus (text files; PDFs skipped), then
Wikipedia `20231101.en` parquet shards pinned to a Hugging Face revision.
Chunking is an exact port of the phase-0 chunker (2,000 characters, 200
overlap). Every 100th chunk becomes a query (its first 256 characters);
the rest fill the base. The script declares its dependencies for `uv`.
Embedding runs on the GPU (88 to 125 chunks/s on the RTX 3060 Ti, about
2.3 hours for the 1M set) and resumes where it stopped; it refuses to
continue if the Ollama model digest changes. Finalizing computes exact
ground truth on the CPU (154 s at 1M, 4.2 GiB peak).

```sh
REV=b04c8d1ceb2f5cd4588862100d08de323dccfbaa   # wikimedia/wikipedia revision
WORK=~/distr-hnsw-bench/work/project-nomic-512
scripts/bench/build-project-set.py prepare --corpus ~/distr-hnsw-proto/corpora/mixed-v4-20260719 \
  --wikipedia ~/distr-hnsw-bench/downloads/wikipedia-20231101.en --wikipedia-revision $REV --work $WORK
scripts/bench/build-project-set.py embed --work $WORK --limit 200000   # 200k prefix
scripts/bench/build-project-set.py finalize --work $WORK --base-count 200000 \
  --name project-nomic-512-200k --out ~/distr-hnsw-bench/datasets/project-nomic-512-200k
scripts/bench/build-project-set.py embed --work $WORK                  # the rest of the 1M
scripts/bench/build-project-set.py finalize --work $WORK --base-count 1000000 \
  --name project-nomic-512 --out ~/distr-hnsw-bench/datasets/project-nomic-512
```

The 1M persistence and compaction benches peak at 7.7 and 9.8 GiB RSS on
`anthonypc`. Run them in a memory-capped scope so an overrun kills only the
bench, not the session that started it:

```sh
systemd-run --user --scope -q -p MemoryMax=10G -p MemorySwapMax=0 -- \
  target/release/distr-hnsw-bench --canonical compact ~/distr-hnsw-bench/datasets/project-nomic-512 \
  --partition-dir <dir> --m 32 --ef 500 --out <report.json>
```

The work directory holds private corpus text and stays on the bench host;
commit only the manifest under `docs/bench/`.

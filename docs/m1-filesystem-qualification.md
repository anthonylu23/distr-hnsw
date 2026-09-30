# M1 filesystem qualification for durable writes

Status: **dm-log-writes drill passed on btrfs, ext4, and XFS (kernel
7.1.9-200.fc44, loop devices); qualified-with-caveats. APFS not qualified.**
(2026-09-29)

This document reviews the durable-write path in
`crates/distr-hnsw/src/durability.rs` against the filesystems distr-hnsw is
expected to run on, and records what the current test suite does and does not
prove. It exists because the M1 acceptance criteria in
[roadmap.md](roadmap.md) require reviewing the implementation on every
supported filesystem rather than inferring parent-directory durability from a
passing process-restart test. The claim under review is the one made in
[m1-storage-contract.md](m1-storage-contract.md): an object acknowledged by an
agent PUT survives immediate power loss on that agent.

## Durable-write sequence

`DurableStore::put` performs the following steps after verifying the supplied
BLAKE3 digest and creating the two-level fanout directories (each new directory
is followed by an `fsync` of its parent via `ensure_child_directory`).

| Step | Call | What it guarantees |
| --- | --- | --- |
| 1 | `OpenOptions::create_new` on `<parent>/.<uuid>.tmp` | Temp file lives in the final directory, so the later rename is a same-filesystem metadata operation. `O_EXCL` prevents clobbering a concurrent writer. Inventory skips `.*.tmp` names. |
| 2 | `write_all(bytes)` | Full payload is in the page cache; nothing is durable yet. |
| 3 | `sync_regular_file` | Linux: `fsync(2)`, which writes data and metadata and flushes the device write cache ("This includes writing through or flushing a disk cache if present" [1]). macOS: see [APFS](#apfs-macos). |
| 4 | `drop(file)` | Closes the descriptor; no durability effect. |
| 5 | `fs::rename(temp, final)` | Atomic name switch. Readers see either no object or a complete, already-synced object; a partially written temp file is never visible under the final name. |
| 6 | `sync_directory(parent)` | `fsync` on the directory descriptor. Required because file `fsync` "does not necessarily ensure that the entry in the directory containing the file has also reached disk" [1]. This is what makes the rename itself durable. |

On any error the temp file is removed. If the final path already exists it is
read back and hash-verified; a corrupt existing object is overwritten by the
same sequence. Objects are immutable, so there is no in-place update and no
torn-overwrite case. This is the sequence recommended by Moyer's "Ensuring data
reaches disk" [2] and by the storage contract.

Two implementation details matter for the analysis below:

- `File::sync_all` in the Rust standard library issues `fcntl(F_FULLFSYNC)`
  on all Apple targets, not `fsync` (verified against
  `library/std/src/sys/fs/unix.rs` [3]). Consequently `sync_directory` also
  requests a full flush on macOS, and the explicit `F_FULLFSYNC` in
  `sync_regular_file` is redundant. The "fallback" to `sync_all` does not
  degrade to plain `fsync`: if `F_FULLFSYNC` is unsupported the PUT fails.
  That is the correct fail-closed behaviour, but the contract wording
  ("macOS requests `F_FULLFSYNC` for regular files") understates what the
  directory sync does.
- `O_DIRECT` is not used and is irrelevant to this claim: it bypasses the page
  cache but does not flush metadata or the device cache, so `fsync` would
  still be required.

The same helpers persist the master key (`crypto.rs`), the SQLite parent
directory (`metadata.rs`), and the portal staging directory (`portal.rs`), so
the conclusions apply to those paths too.

## Per-filesystem analysis

### btrfs (development and test desktop)

`anthonypc` runs Fedora with `/home` on btrfs, the Fedora desktop default since
Fedora 33 (root and home subvolumes) [4]. Recorded on 2026-09-29: kernel
`7.1.9-200.fc44.x86_64`; `/home` is `/dev/nvme0n1p3` (subvolume `/home`)
mounted `rw,relatime,seclabel,compress=zstd:1,ssd,discard=async,space_cache=v2`
(so `barrier` and `treelog` are at their defaults, on); the NVMe drive reports
`/sys/block/nvme0n1/queue/write_cache = write back`, meaning the kernel must
issue cache flushes and the `fsync` path is load-bearing. The MacBook runs
macOS 27.0 (Darwin 27.0.0) on APFS.

- **Sufficiency.** btrfs implements `fsync` through the tree log
  (`treelog`, default on), which "stores changes without the need of a full
  filesystem sync" [5]. `barrier` is on by default and ensures writes "make it
  through the device cache and are stored permanently" at each consistency
  checkpoint [5]. `fsync(file)` followed by `rename` followed by
  `fsync(dir)` is therefore expected to persist both the bytes and the name.
  The explicit directory `fsync` is the conservative path: after a rename
  that touches inodes outside the log, btrfs forces a full transaction commit
  rather than a partial log write (tree-logging unlink/rename fixes [6]).
- **Caveats.** The tree-log code has a long history of crash-consistency
  bugs in exactly the `write, rename, fsync` family: the 2009 unlink/rename
  fixes [6]; "fix data loss after inode eviction, renaming it, and fsync it"
  (upstream `d1d832a0`, stable 4.4+, 2019) [7]; CrashMonkey's 2018 findings,
  several of which btrfs developers initially disputed [8][9]; and a
  log-replay bug that became likely after a stable backport into 6.15.3 and
  was fixed in 6.16-rc5 ("fix iteration of extrefs during log replay" and
  related commits) [10]. None of these are known to affect kernel 7.1.x,
  but they show that tree-log correctness is kernel-version dependent, so the
  qualification must name the kernel it was performed on. Do not mount with
  `nobarrier` or `notreelog`; `flushoncommit` is not required.
- **Data checksums.** btrfs checksums data by default, so a torn write is
  reported as an I/O error rather than returned silently. distr-hnsw does not
  rely on this (GET re-hashes every object), but it is a useful second line.

### ext4 (typical Linux servers)

- **Sufficiency.** The default `data=ordered` mode forces data "out to the
  main file system prior to its metadata being committed to the journal"
  [11], `barrier` is on by default, and `fsync` flushes the device cache
  [1]. `fsync(file)` persists the bytes; `fsync(dir)` commits the journal
  transaction containing the rename. The sequence is sufficient.
- **Why we still fsync explicitly.** ext4 uses delayed allocation (`delalloc`,
  default) [11]. Without a file `fsync`, a crash after `rename` can leave a
  zero-length file whose name is durable but whose blocks were never
  allocated. The `auto_da_alloc` heuristic detects the replace-via-rename
  pattern and forces allocation "at the next journal commit" [11], but that
  is a mitigation for "broken applications" bounded by the commit interval,
  not a durability guarantee, and it does not flush the device cache. Pillai
  et al. showed that ordering properties differ even between ext3 and ext4 in
  the same journaling mode [12]. The explicit `fsync` calls remove the
  dependence on these heuristics.
- **Caveats.** `data=writeback` or `nobarrier` would invalidate the claim;
  deployments must not use them. No physical ext4 volume exists in the
  current test fleet; the drill below used a loop-backed ext4 image.

### XFS

XFS journals metadata only and does not write data until asked "with fsync, an
O_SYNC or O_DIRECT open" [13]; the historical "NULL bytes after power loss"
reports are the unsynced-data case the file `fsync` in step 3 avoids. The
directory `fsync` requirement is the same as on ext4. XFS issues cache flushes
unless the device reports a persistent cache, and the XFS FAQ is explicit that
"if nobarrier makes a difference skipping it is not safe" [13]. The sequence is
sufficient with the same caveats as ext4. No physical XFS volume is present in
the test fleet; the drill below used a loop-backed XFS image.

### APFS (macOS)

- **Why `fsync` alone is not enough.** Apple's `fsync(2)` states that it
  "will flush all data from the host to the drive" but "the drive itself may
  not physically write the data to the platters for quite some time and it
  may be written in an out-of-order sequence" [14]. `F_FULLFSYNC` "asks the
  drive to flush all buffered data to the permanent storage device" and "acts
  as a barrier"; it is implemented on HFS, FAT, UDF, and APFS [15].
- **Directory sync.** The code opens the parent directory and calls
  `sync_all`, which on Apple targets is `F_FULLFSYNC` [3]. Apple's man pages
  do not state whether `F_FULLFSYNC` on a directory descriptor persists the
  rename on APFS; this review could not find an authoritative statement and
  no macOS machine was available to test. The storage contract's description
  ("uses directory `fsync` for rename persistence") should be read as
  "requests a full flush on the directory descriptor; rename persistence on
  APFS is unverified."
- **Caveats.** `F_FULLFSYNC` is expensive and "may take quite a while to
  complete" [15]; external drives may ignore it. macOS is not a production
  target for M1 and has no test coverage.

## What the tests verify today

| Test | Mechanism | Proves | Does not prove |
| --- | --- | --- | --- |
| `durability.rs` unit tests | In-process, `tempfile::tempdir()` | Idempotent PUT, hash verification on GET, inventory paging and malformed-entry rejection | Any durability property |
| `commit_spine_process.rs`, `delete_recovery.rs` | Child portal calls `std::process::exit(86)` at each `Failpoint`; agents keep running | Recovery from a lost process at every durable boundary; SQLite and object-store state after the OS has accepted the writes | Power loss. The kernel page cache and device cache survive a process exit, so a missing `fsync` would pass these tests. |
| `scripts/power-loss-drill.sh` (manual, `anthonypc`) | `dm-log-writes` on loop devices; log replayed to the mark taken after each acknowledged put; fresh agent on the replayed volume re-hashes every object via `GET /v1/objects` | Every object the agent had acknowledged before the mark is present and hash-valid after discarding everything that was not flushed; the filesystem passes its read-only checker after journal recovery; the agent `incarnation` survives; a deliberately un-fsynced 1 MiB control file does not survive | Behaviour of a physical drive's own write cache (loop devices honour flushes by `fsync` on the backing file); concurrency (one sequential writer); kernels other than the one recorded |

Two further limitations apply:

- `tempfile::tempdir()` honours `TMPDIR`. On `anthonypc`, `/tmp` is tmpfs
  (`findmnt /tmp` reports `tmpfs`), where `fsync` is a no-op, so until
  2026-09-29 the suite had exercised no real Linux filesystem. The workspace
  now sets `TMPDIR` to `target/` in `.cargo/config.toml`, so tests run on
  btrfs on `anthonypc` and APFS on the MacBook. This still proves only
  process-loss safety, not power-loss safety.
- The drill observes the flush stream at the device-mapper layer: `dm-log-writes`
  only commits queued writes to its log when a `REQ_PREFLUSH` arrives, so a
  missing or ineffective `fsync` in the agent would have surfaced as a missing
  object (the negative control shows that un-flushed data is indeed dropped).
  It does not observe the NVMe/SATA drive in `anthonypc`; whether that drive
  honours flushes still rests on kernel documentation [1][16].

## Qualification status

A filesystem is qualified only by a drill that discards un-flushed writes
(real power cut, `dm-log-writes` replay, or a CrashMonkey-style harness).
Ratings below combine the documentation review with the `dm-log-writes` drill
run on 2026-09-29 (details in the next section).

| Filesystem | Status | Evidence | Caveats |
| --- | --- | --- | --- |
| btrfs | qualified-with-caveats | `dm-log-writes` drill 8/8 marks, 20/20 objects present and hash-valid at the last mark, `btrfs check --readonly` clean at every mark, negative control lost; code review against btrfs docs [5]; process-exit and scrub tests pass | Kernel `7.1.9-200.fc44.x86_64` only (tree-log history [6][7][10] makes this kernel-specific); loop device, so the physical drive's write cache was not tested; default mount options (`ssd,discard=async,space_cache=v2`, `barrier`/`treelog` default on); single sequential writer |
| ext4 | qualified-with-caveats | `dm-log-writes` drill 8/8 marks, 20/20 objects, `e2fsck -fn` clean at every mark, negative control lost; code review against kernel ext4 docs [11] and `fsync(2)` [1] | Same kernel and loop-device caveats; default `data=ordered`, `barrier` on; `data=writeback`/`nobarrier` remain excluded; not yet run on a physical ext4 volume |
| XFS | qualified-with-caveats | `dm-log-writes` drill 8/8 marks, 20/20 objects, `xfs_repair -n` clean at every mark, negative control lost; code review against XFS FAQ [13] | Same kernel and loop-device caveats; default options (`inode64,logbufs=8,logbsize=32k`); not yet run on a physical XFS volume |
| APFS (macOS) | not qualified | Code review; `F_FULLFSYNC` verified in std [3] and Apple docs [15] | Directory `F_FULLFSYNC` semantics for rename persistence unverified; no macOS test host; no drill mechanism equivalent to `dm-log-writes` identified |

"Qualified-with-caveats" means: on the named kernel, with default mount
options, every object acknowledged by the agent before a simulated power cut
was present and hash-valid after replay, at every one of the tested cut
points. It does not cover a drive that lies about flushes, other kernels, or
concurrent writers. A real power cut on the physical drive remains the only
test of the drive's own cache behaviour.

### Drill results, 2026-09-29

Host `anthonypc`, kernel `7.1.9-200.fc44.x86_64`, `dm-log-writes` target
v1.1.0, `replay-log` built from xfstests `3e1ee80`, loop devices over 4 GiB
sparse images on the btrfs `/home` volume (`/sys/block/loopN/queue/write_cache
= write back` for data, log, and the dm device). Eight files of 3 KiB to
6 MiB (`CHUNK_SIZE` is 4 MiB, so four of them produce two chunks) were
uploaded through the portal at RF2 with the dm-backed agent as one failure
domain; a mark was inserted after each acknowledged put and the agent's
inventory snapshotted (2, 4, 6, 9, 12, 14, 17, 20 objects). The agent was
then `kill -9`ed and the device torn down. Full reports:
`~/distr-hnsw-drill/report-<fs>-<utc>.json` on `anthonypc` (not committed).

| Filesystem | mkfs | Mount options observed | Log entries | Marks passed | Objects at last mark | `.*.tmp` survivors | Read-only check | Negative control (1 MiB un-fsynced file) | Wall clock |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| btrfs | btrfs-progs v7.1 | `rw,relatime,seclabel,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/` | 1092 | 8/8 | 20/20 present, 0 missing, 0 corrupt | 0 | `btrfs check --readonly` exit 0 at all 8 marks | 0 bytes visible | 7 s |
| ext4 | mke2fs 1.47.3 | `rw,relatime,seclabel` | 1697 | 8/8 | 20/20 present, 0 missing, 0 corrupt | 0 | `e2fsck -fn` exit 0 at all 8 marks | 0 bytes visible | 7 s |
| XFS | xfsprogs 7.1.1 | `rw,relatime,seclabel,inode64,logbufs=8,logbsize=32k,noquota` | 394 | 8/8 | 20/20 present, 0 missing, 0 corrupt | 0 | `xfs_repair -n` exit 0 at all 8 marks | 0 bytes visible | 7 s |

At every mark on every filesystem: the replayed volume mounted with default
options (journal/log recovery ran as it would on a real restart), a fresh
agent started, its `incarnation` matched the original, every object in the
inventory snapshot for that mark answered `GET /v1/objects/{kind}/{hash}`
with 200 (the agent re-hashes on GET, so this is a content check), and no
object acknowledged only after that mark was visible. The negative control
(a file written with no `fsync` immediately before its own mark) replayed as
absent on all three, which confirms the log was dropping cached writes rather
than passing everything through.

## Drill: dm-log-writes on `anthonypc`

Implemented as `scripts/power-loss-drill.sh <btrfs|ext4|xfs>` (see
`scripts/README.md`); the outline below is what it does.

`dm-log-writes` records every write and only commits queued writes to the log
when a `REQ_PREFLUSH` arrives, so replaying the log to a given mark reproduces
"what is on disk and not what is in cache" [17]. Run it on a loop device so
no real volume is at risk. Outline (as root, `sudo`):

```sh
# 1. Two loop-backed devices: data target and log.
truncate -s 8G /var/tmp/dhq-data.img && truncate -s 8G /var/tmp/dhq-log.img
DATA=$(losetup --find --show /var/tmp/dhq-data.img)
LOG=$(losetup --find --show /var/tmp/dhq-log.img)
dmsetup create dhq --table "0 $(blockdev --getsz $DATA) log-writes $DATA $LOG"
mkfs.btrfs /dev/mapper/dhq            # repeat with mkfs.ext4 / mkfs.xfs
dmsetup message dhq 0 mark mkfs
mount /dev/mapper/dhq /mnt/dhq

# 2. Run an agent on the volume; after each acknowledged portal put, record
#    the manifest/chunk hashes and insert a mark named after the upload.
distr-hnsw agent --id a --failure-domain host-a --bind 127.0.0.1:7101 \
  --volume /mnt/dhq/agent-a &
#    (distr-hnsw portal put ...; dmsetup message dhq 0 mark <upload-id>)

# 3. Tear down without unmounting cleanly, replay to each mark, restart the
#    agent on the replayed volume, and compare GET /v1/inventory/{kind}
#    plus b3sum of each object against the recorded acknowledged hashes.
dmsetup remove dhq
replay-log --log $LOG --replay $DATA --end-mark <upload-id>
mount $DATA /mnt/replay
```

`replay-log` ships with xfstests; the script builds only
`src/log-writes/replay-log.c` + `log-writes.c` from a shallow clone (its usage
text advertises `--number-entries`, but the option table spells it
`--num-entries`). `dm-flakey` with `drop_writes` [18] is a cheaper
alternative for a single crash point but cannot replay to arbitrary flush
boundaries; it was not needed. The script records kernel version, mount
options, and `/sys/block/<disk>/queue/write_cache` [19] in the report and
runs the same procedure for ext4 and XFS. A real power cut on the physical
drive remains the only test of the drive's own cache behaviour.

Where the implementation departs from the outline, deliberately:

- The teardown follows xfstests `_log_writes_unmount` / `_log_writes_remove`:
  after `kill -9` of the agent an `end` mark is inserted, then the volume is
  unmounted normally and the dm device removed. Everything the unmount
  flushes lands after `end` and is ignored by replays to `put-<i>`.
- The data device is `blkdiscard`ed before every replay, so no sector written
  after the mark can survive from the previous replay state. Plain xfstests
  replay writes onto whatever the device already holds.
- The replayed device is mounted read-write with default options rather than
  read-only, so journal (ext4), log (XFS), or tree-log (btrfs) recovery runs
  exactly as on a real restart; the agent only reads. The read-only checker
  (`btrfs check --readonly`, `e2fsck -fn`, `xfs_repair -n`) runs after that
  mount is released, because both `e2fsck -n` and `xfs_repair -n` refuse or
  misreport a dirty journal.
- A negative control (1 MiB written with no `fsync`, then a mark) is replayed
  last; the run is reported `INCONCLUSIVE` if that file survives intact.

## Gaps and next actions

- Done 2026-09-29: `TMPDIR` points at `target/` so the suite exercises a real
  filesystem; `anthonypc` kernel, mount options, and write-cache mode are
  recorded above. Document the filesystem each CI run used once CI exists.
- Done 2026-09-29: `dm-log-writes` drill on `anthonypc` for btrfs, ext4, and
  XFS via `scripts/power-loss-drill.sh`; 8/8 marks on each, table re-rated.
- Re-run the drill after every kernel update on `anthonypc` and record the
  kernel in this document; the btrfs rating in particular is kernel-specific.
- Extend the drill beyond a single sequential writer: several concurrent
  portal puts between marks, a delete (deletion-marker objects were not
  exercised, the inventory was empty for that kind), and larger N. Consider
  `replay-log --fsck ... --check flush` for a checker at every flush boundary.
- Run the drill on a physical ext4 and XFS partition (or an external drive)
  rather than a loop device, and, when possible, a real power cut on the
  physical btrfs volume: only that exercises the drive's own write cache.
- Add a startup health check that logs the volume filesystem and warns on
  `nobarrier`, `notreelog`, or `data=writeback`.
- Amend the macOS wording in [m1-storage-contract.md](m1-storage-contract.md)
  to say the directory sync is `F_FULLFSYNC` via `sync_all` and that
  rename persistence on APFS is unverified; the redundant `fcntl` in
  `sync_regular_file` can stay as documentation of intent.
- Add a fault-injection test that intercepts `sync_regular_file` /
  `sync_directory` failures and asserts the PUT is not acknowledged and the
  temp file is removed.
- The M1 exit-gate statement ("distr-hnsw must not hold the only copy of any
  file") is unchanged by this document. The drill now passes on loop devices
  for three filesystems; whether that, plus the physical-drive caveat above,
  satisfies the gate is a roadmap decision, not a conclusion of this review.

## References

1. `fsync(2)`, Linux man-pages: https://man7.org/linux/man-pages/man2/fsync.2.html
2. J. Moyer, "Ensuring data reaches disk", LWN, 2011-09-07: https://lwn.net/Articles/457667/
3. Rust `library/std/src/sys/fs/unix.rs` (`os_fsync` uses `F_FULLFSYNC` when `target_vendor = "apple"`): https://github.com/rust-lang/rust/blob/master/library/std/src/sys/fs/unix.rs
4. Fedora Change "BtrfsByDefault": https://fedoraproject.org/wiki/Changes/BtrfsByDefault
5. `btrfs(5)` mount options (`treelog`, `barrier`, `flushoncommit`): https://btrfs.readthedocs.io/en/latest/btrfs-man5.html
6. "Btrfs: tree logging unlink/rename fixes", commit `12fcfd22`: https://nv-tegra.nvidia.com/r/plugins/gitiles/linux-4.4/+/12fcfd22fe5bf4fe74710232098bc101af497995%5E%21/fs/btrfs
7. "Btrfs: fix data loss after inode eviction, renaming it, and fsync it", upstream `d1d832a0`, stable 4.4+: https://lkml.indiana.edu/hypermail/linux/kernel/1907.3/01469.html
8. J. Mohan et al., "Finding Crash-Consistency Bugs with Bounded Black-Box Crash Testing", OSDI 2018: https://www.usenix.org/conference/osdi18/presentation/mohan
9. J. Edge, "Filesystems and crash resistance", LWN, 2019-05-21: https://lwn.net/Articles/788938/
10. "[GIT PULL] Btrfs fixes for 6.16-rc5", 2025-07-03 (log replay fixes): https://lkml.rescloud.iu.edu/2507.0/05562.html. The link to the 6.15.3 stable backport is reported by third-party coverage and was not independently verified.
11. ext4 kernel documentation (`data=ordered`, `delalloc`, `auto_da_alloc`, `barrier`): https://docs.kernel.org/admin-guide/ext4.html
12. T. S. Pillai et al., "All File Systems Are Not Created Equal", OSDI 2014: https://research.cs.wisc.edu/adsl/Publications/alice-osdi14.pdf
13. XFS FAQ (NULL bytes after power loss; write cache and barriers): https://xfs.org/index.php/XFS_FAQ
14. Apple `fsync(2)`: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html
15. Apple `fcntl(2)`, `F_FULLFSYNC` and `F_BARRIERFSYNC` (current Xcode man page): https://keith.github.io/xcode-man-pages/fcntl.2.html
16. Linux block layer, "Explicit volatile write back cache control": https://docs.kernel.org/block/writeback_cache_control.html
17. `dm-log-writes`: https://docs.kernel.org/admin-guide/device-mapper/log-writes.html
18. `dm-flakey`: https://docs.kernel.org/admin-guide/device-mapper/dm-flakey.html
19. sysfs `/sys/block/<disk>/queue/write_cache`: https://www.kernel.org/doc/Documentation/ABI/stable/sysfs-block

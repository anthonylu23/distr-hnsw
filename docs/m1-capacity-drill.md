# M1 capacity and ENOSPC drill

Status: **passed on ext4 (loop device, kernel 7.1.9-200.fc44, `anthonypc`),
all three phases, 36/36 checks.** (2026-09-29)

This document records the storage-pressure drill that
[m1-lifecycle-contract.md](m1-lifecycle-contract.md) ("Capacity and failure
behavior", implementation step 4) and
[m1-phase-1-decisions.md](m1-phase-1-decisions.md) (decision 3) left open:
the unit tests exercise quota exhaustion, not a full filesystem. The drill
verifies, against a real kernel `ENOSPC`, the claims in
[m1-storage-contract.md](m1-storage-contract.md) ("Capacity and admission",
"Garbage collection"): admission refuses ahead of the reserve with CLI exit 3
and HTTP 507, control objects may still use the reserve, a mid-write `ENOSPC`
maps to the same refusal without leaving a partially visible object, and a
refused idempotency key converges once space exists. The script is
`scripts/enospc-drill.sh`; the full machine-readable report stays off-repo
under `$HOME/distr-hnsw-drill/report-enospc-<utc>.json`.

## Setup

| Item | Value |
| --- | --- |
| Host, kernel | `anthonypc`, Linux 7.1.9-200.fc44.x86_64 |
| Filesystem | ext4, `mke2fs 1.47.3`, 96 MiB sparse image on a loop device, mounted `rw,relatime,seclabel`; 1 KiB blocks, 88,566,784 bytes total, 81,473,536 available to the user at start |
| Agent A (under test) | volume on the loop filesystem, no `--quota-bytes`, `--reserve-bytes 8388608`, `--hard-floor-bytes 1048576`; restarted for phase 2 with reserve and hard floor 0 |
| Agent B | plain directory on `/home` (btrfs), default policy; supplies the second failure domain RF2 needs |
| Portal | `portal init --no-recovery-bundle`, database and key outside the mount |
| Uploads | 4 MiB random files, one chunk (4,194,320 ciphertext bytes) plus one manifest each |
| Binary | `target/release/distr-hnsw`, source rev `9862462`, sha256 `aa53d6e9...2baedf` |
| Wall clock | 4 seconds |

## Results

### Phase 1: admission ahead of the reserve

| Check | Expected | Observed |
| --- | --- | --- |
| Files admitted before refusal | > 0 | 17 (17 chunks + 17 manifests on A) |
| Capacity state trajectory | `ok` then `warning`/`admission_paused` | `ok` through put 15, `warning` after put 16 (fs_free 14.3 MB < 20% of 88.6 MB), `admission_paused` after put 17 (effective 1,668,096 < 4 MiB) |
| `portal put` exit on refusal | 3 | 3; stderr `insufficient capacity: 4194320 bytes need 2 failure domains but 1 can admit them (limiting: quota); the durability floor is unchanged and nothing was deleted` |
| Direct agent PUT of a chunk | HTTP 507 | 507, body `{"code":"insufficient_capacity","limiting":"reserve","volume_id":"a","required_bytes":4194320,"available_bytes":1668096}`; `last_refusal_at` set |
| Direct agent PUT of a 1 KiB manifest | admitted (409 hash mismatch after admission) | 409 |
| No partially visible file | scrub `required_objects` = 34, `portal health` durable = 34, 0 unhealthy, no `.*.tmp` on A | 34 / 34 / 0 / 0 |
| `portal delete` of an admitted file while paused | exit 0, marker on A | exit 0, 1 deletion marker on A (marker used the reserve) |
| Retry of the refused key | exit 3, identical stderr | exit 3, identical |

### Phase 2: true ENOSPC

The filler is sized so that exactly `ceil(4,194,320 / 1024)` = 4097 blocks
(4,195,328 bytes) stay free. Admission is byte-exact and passes
(`effective_free_bytes` 4,195,328 >= 4,194,320); the write also needs new
fanout-directory blocks, so the kernel refuses it. Agent A was restarted on
the same volume with reserve 0 and hard floor 0 (incarnation unchanged).

| Check | Expected | Observed |
| --- | --- | --- |
| `portal put` exit | 3 | 3 |
| stderr | mentions capacity and enospc | `insufficient capacity: 4194320 bytes need 2 failure domains but 0 can admit them (limiting: enospc); ...` |
| HTTP status from agent A | 507 | 507 (the portal produces `limiting: enospc` only from a 507 body; no `HTTP 500` text appeared) |
| `capacity.state` | `enospc_observed` | `enospc_observed`, `last_refusal_at` = 1790727123 |
| Leftover `.*.tmp` on the volume | 0 | 0 |
| `used_bytes` after the failed put | unchanged (71,309,590) | 71,309,590 |
| Chunk on agent A | absent | absent (17 chunks on A; the copy on B was accepted and stays as staged garbage) |

### Phase 3: recovery of space

| Check | Expected | Observed |
| --- | --- | --- |
| `portal scrub` after removing the filler | exit 0, complete observation | exit 0, 33 required objects durable |
| `portal gc --apply --retention-seconds 0 --staging-grace-seconds 0` | deleted file's chunk and manifest collected | candidates 3, proven 2, applied 2, blocked 1 (the ENOSPC upload's chunk: `placement on a is pending (movement in flight)`), exit 0 |
| Agent A `used_bytes` | drops by >= 4,194,320 | 71,309,590 to 67,114,918 (drop 4,194,672) |
| Retry of the phase-1 refused key | exit 0, download hash matches | exit 0, sha256 matches |
| Retry of the phase-2 ENOSPC key | exit 0, download hash matches | exit 0, sha256 matches (the pending placement on A was completed, not re-planned) |
| Final scrub | exit 0, 37 required objects (16 x 2 + 1 marker + 2 x 2) | exit 0, 37 durable |

## Product findings

No durability or admission bug was found. Two labelling observations:

- The portal's pre-check refusal reports `limiting: quota` regardless of the
  actual limiting factor (`require_admissible_capacity` in `portal.rs` hard
  codes the string); agent A had no quota and was limited by its reserve,
  as the agent's own 507 body correctly said. Cosmetic, but it misdirects an
  operator reading `portal put` output.
- A portal-side refusal never contacts the agent, so the agent's
  `last_refusal_at` stays null after a `portal put` exit 3 unless a client
  reached the agent directly. Agent health alone therefore under-reports
  refusals; cluster-level reporting belongs to the future `portal status`.

## Caveats

- ext4 on a loop device over a sparse image on btrfs `/home`; 1 KiB blocks
  because the filesystem is under 512 MiB. Other filesystems were not run.
- The "leave ~2 MiB free" recipe cannot produce a kernel `ENOSPC` with this
  code: admission would refuse the chunk first. Only the block-exact fill
  reaches the kernel, and it relies on the object's fanout directories being
  new (true whenever the chunk's 4-hex prefix is unseen, as here).
- `capacity.state` remains `enospc_observed` for 15 minutes after the last
  `ENOSPC` regardless of freed space, by decision 3; admission is arithmetic
  and did converge within that window.
- Agent A ran with reserve 0 / hard floor 0 through phase 3, so the
  convergence is not evidence about the reserve policy after recovery.

## Rerun

On `anthonypc` (passwordless sudo, `mkfs.ext4`, `losetup`, `fallocate`,
`jq`, `curl`):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p distr-hnsw
scripts/enospc-drill.sh            # add --keep to retain the image
```

The script only touches the loop device attached to the image it created
(checked with `losetup -j` before every privileged call), mounts at
`$HOME/distr-hnsw-drill/mnt-enospc-<pid>`, and an `EXIT` trap kills the
agents, unmounts, detaches, and prints `losetup -a` and `mount` leftovers.
Exit status is 0 only when every check in every phase passed.

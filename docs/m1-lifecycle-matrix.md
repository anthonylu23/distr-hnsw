# M1 lifecycle matrix

Status: **passed on the reference host, re-run from the clean hardening revision
`dc276f3` on 2026-10-27** (run `20261027T014148Z`), and again with M2 grants
from `5a8656a` on 2026-10-28 (run `20261028T211631Z`: 160 files, 1.87 GB,
24 flips, 40 deletions, 179 GC candidates blocked before the post-deletion
scan and applied after it, 548 objects durable, no deleted file readable,
stale-restore GC applied nothing). Automated by
[`scripts/lifecycle-matrix.sh`](../scripts/lifecycle-matrix.sh); the full
report stays under `~/distr-hnsw-drill/` (SHA-256 prefix `7e32162d036d8730`).
The first run (`20260930T001138Z`, revision `861baee`) predates the
hardening pass and did not cover the orders in step 5.

This is the large storage, corruption, movement, retirement, and
garbage-collection evidence the roadmap's M1 "delete, repair, and capacity
safety" criteria ask for. It complements the fixture-scale tests in
`crates/distr-hnsw/tests/` with a corpus of hundreds of objects and
gigabytes of ciphertext.

## Procedure

1. Three loopback agents in three failure domains; commit 160 random files
   of 4 KiB to 24 MiB with the release binary.
2. Inject 24 faults spread across agents and modes: flip one byte, truncate
   to half, or remove the object file. Run `portal scrub` (verify only), then
   `portal scrub --repair`. Download a 24-file sample and compare SHA-256.
3. `portal drain --agent-id agent-3`, stop that agent, `portal retire`, join
   `agent-4` in a new failure domain, `portal scrub --repair`. Sample again.
4. Delete every fourth file. Run `portal gc --apply` before any new
   observation, then `portal scrub`, `portal gc` (plan), `portal gc --apply`.
   Measure bytes held by the live agents before and after. Sample kept files
   again and confirm every deleted file is unreadable.
5. Orders the 2026-10-25 audit found broken. (a) Join `agent-5`, drain and
   retire `agent-4` after GC has applied proofs. (b) Stop an upload after
   its chunks are durable (`after_chunks_durable` failpoint, which is why the
   script builds with `--features failpoints`), snapshot the database with
   `VACUUM INTO`, let the retry commit the file, then replace the database
   with the snapshot. Run `portal scrub` and `portal gc --apply` with zero
   retention and grace, then `portal recover --apply`, and download the
   acknowledged file.

## Results

| Field | Value |
|---|---|
| Corpus | 160 files, 2,073,181,804 bytes, 734 required objects, 3 agents |
| Faults injected | 24 (8 byte flips, 8 truncations, 8 removals) |
| Detected by verify scrub | 16 corrupt copies, 8 missing copies (all 24) |
| Repaired | 24 copies; health after repair 734 durable, 0 degraded / at risk / lost |
| Downloads during and after faults | 24/24 samples match SHA-256 at each of three checkpoints; corrupt bytes were never served |
| Drain of agent-3 | 734 objects considered, all already redundant on two other domains, 734 placements orphaned, 0 blocked, 0 moves needed |
| Retirement | agent-3 retired while unreachable; health 734 durable on the remaining agents |
| Replacement | agent-4 joined; no repairs needed because desired placement (two copies) already held |
| Garbage collection | 40 files deleted; before a new observation every candidate was blocked (170 blocked); after scrub 170 candidates proven and applied, 0 stale, 0 deferred |
| Space | 4,146,569,880 bytes held before GC, 3,231,936,242 after |
| Final health | 604 required objects durable; 0 deleted files readable |
| Retire after GC | agent-4 drained and retired after proofs were applied (this was refused forever before the fix, H2) |
| GC after a stale restore | 0 objects collected; the late file's candidate was blocked as an object unknown to the metadata (H1); `recover --apply` blocked 0 files; the acknowledged file downloads intact |

Timings (seconds): commit 40.3, verify scrub 12.6, repair scrub 12.9,
drain 11.5, retire 8.2, replacement scrub 9.2, GC plan 0.16, GC apply 0.76,
retire after GC 6.4.

## Interpretation

- Bit flips, truncations, and removals are all detected by read-back
  hashing and repaired copy-first without serving corrupt plaintext.
- Draining and retiring a node never dropped an object below RF2, and the
  retired incarnation is locked out.
- Deletion is refused until every active incarnation has observed the
  namespace after the deletion, and then reclaims space through proofs.
- Collected objects no longer count toward retirement, and a portal running
  on stale metadata collects nothing while agents hold objects it does not
  know.
- The matrix ran on one machine with loopback agents; failure domains are
  logical, not physical, and network partitions are not exercised. That is
  the M1 scope; multi-node fault injection belongs to M4.

## Rerunning

```sh
scripts/lifecycle-matrix.sh --files 160 --max-mib 24 --flip 24
```

Exit status is 0 only on a `pass` verdict; the report path is printed.

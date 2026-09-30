# M1 lifecycle matrix

Status: **passed on `anthonypc` (2026-09-30)**, run `20260930T001138Z`,
source revision `861baee`. Automated by
[`scripts/lifecycle-matrix.sh`](../scripts/lifecycle-matrix.sh); the full
report stays under `~/distr-hnsw-drill/` (SHA-256 prefix `585b5a344316ed43`).

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

## Results

| Field | Value |
|---|---|
| Corpus | 160 files, 1,863,883,678 bytes, 677 required objects, 3 agents |
| Faults injected | 24 (8 byte flips, 8 truncations, 8 removals) |
| Detected by verify scrub | 16 corrupt copies, 8 missing copies (all 24) |
| Repaired | 24 copies; health after repair 677 durable, 0 degraded / at risk / lost |
| Downloads during and after faults | 24/24 samples match SHA-256 at each of three checkpoints; corrupt bytes were never served |
| Drain of agent-3 | 677 objects considered, all already redundant on two other domains, 677 placements orphaned, 0 blocked, 0 moves needed |
| Retirement | agent-3 retired while unreachable; health 677 durable on the remaining agents |
| Replacement | agent-4 joined; no repairs needed because desired placement (two copies) already held |
| Garbage collection | 40 files deleted; before a new observation every candidate was blocked (177 blocked); after scrub 177 candidates proven and applied, 0 stale, 0 deferred |
| Space | 3,727,964,052 bytes held before GC, 2,729,931,746 after |
| Final health | 540 required objects durable; 0 deleted files readable |

Timings (seconds): commit 37.7, verify scrub 11.7, repair scrub 11.8,
drain 10.3, retire 5.9, replacement scrub 8.2, GC plan 0.16, GC apply 0.84.

## Interpretation

- Bit flips, truncations, and removals are all detected by read-back
  hashing and repaired copy-first without serving corrupt plaintext.
- Draining and retiring a node never dropped an object below RF2, and the
  retired incarnation is locked out.
- Deletion is refused until every active incarnation has observed the
  namespace after the deletion, and then reclaims space through proofs.
- The matrix ran on one machine with loopback agents; failure domains are
  logical, not physical, and network partitions are not exercised. That is
  the M1 scope; multi-node fault injection belongs to M4.

## Rerunning

```sh
scripts/lifecycle-matrix.sh --files 160 --max-mib 24 --flip 24
```

Exit status is 0 only on a `pass` verdict; the report path is printed.

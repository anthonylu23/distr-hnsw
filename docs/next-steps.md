# Next steps

## Phase 0 follow-through

1. Re-run bake-off on a **larger mixed corpus** (≥ hundreds of files; PDFs +
   notes + code) with ≥40 meaning-oriented labeled queries (fewer filename echoes).
   Use `prototype/scripts/run-bakeoff-anthonypc.sh` (run-scoped `WORK_DIR` +
   `prepare --fresh`). Confirm stored/current Ollama digests match and retain the
   sanitized provenance summary plus cold and warmed latency metrics.
2. Optionally add `bge-m3` to the matrix if VRAM allows on anthonypc.
3. If the larger bake-off clears the ≥60% win-rate vs **name** (or clear
   qualitative go) **and** judged ≥ 40: lock model/dims into DESIGN §15 and
   proceed to phase 1.
4. If still no-go on meaning queries: revisit extraction / chunking / model
   before distributed work.

## Phase 1 (after go)

Blob plane + recovery foundation per DESIGN.md §14. Keep `prototype/` disposable.

## Ops notes

- `ssh anthonylu@anthonypc` may require a one-time Tailscale SSH browser check.
- Remote `OLLAMA_HOST` is `127.0.0.1:11434` (no scheme); the CLI normalizes this.
- Corrected vault bake-off: `~/distr-hnsw-proto/runs/20260718T215901Z` (vs name
  16.7%; dims not locked).

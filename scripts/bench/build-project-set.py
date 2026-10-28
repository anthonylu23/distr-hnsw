#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy>=2", "pyarrow>=17", "blake3>=1"]
# ///
"""Build the project-nomic-512 benchmark set (docs/m3-implementation-plan.md,
"Datasets"): text chunks embedded with the locked phase-0 model and recipe,
written in the flat BenchDatasetV1 layout the bench harness reads.

    build-project-set.py prepare  --corpus DIR --wikipedia DIR --work DIR
    build-project-set.py embed    --work DIR [--limit N]
    build-project-set.py finalize --work DIR --base-count N --name NAME --out DIR

prepare  Chunks the phase-0 mixed corpus (text files only; PDFs are skipped)
         and then Wikipedia articles from pinned parquet shards, in a fixed
         order, with the phase-0 chunker (2,000 characters, 200 overlap).
         Every 100th chunk is held out as a query source until the query
         set is full; the rest fill the base. A query is the first 256
         characters of its held-out chunk, cut at whitespace.
embed    Embeds queries, then base chunks in order, through Ollama's
         OpenAI-compatible endpoint with nomic's task prefixes
         ("search_query: ", "search_document: "), truncates 768 -> 512 and
         L2-normalizes (the phase-0 recipe, DESIGN §15). Resumable; refuses
         to continue if the Ollama model digest changed. A failed or
         malformed embedding stops the run; nothing is padded or rewritten.
finalize Writes base.f32le (the first N embedded base vectors),
         queries.f32le, groundtruth.i32le (exact top-100 by cosine, ties by
         id), and manifest.json with sources, recipe, and BLAKE3 digests.

The work directory holds chunk texts and embeddings, including private
corpus text; it stays on the bench host and is never committed.
"""
import argparse
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

import blake3
import numpy as np
import pyarrow.parquet as pq

MODEL = "nomic-embed-text"
NATIVE_DIMS = 768
DIMS = 512
DOC_PREFIX = "search_document: "
QUERY_PREFIX = "search_query: "
CHUNK_CHARS = 2000
CHUNK_OVERLAP = 200
MAX_FILE_BYTES = 8 * 1024 * 1024
QUERY_EVERY = 100
QUERY_CHARS = 256
GROUNDTRUTH_K = 100
BATCH = 128


def last_whitespace(window: str) -> int:
    """Index of the last whitespace character, or -1 (Rust `rposition`)."""
    for index in range(len(window) - 1, -1, -1):
        if window[index].isspace():
            return index
    return -1


def chunk_text(text: str) -> list[str]:
    """Port of prototype/src/chunk.rs::chunk_text: windows of CHUNK_CHARS,
    breaking at the last whitespace when that keeps at least half a window,
    overlapping by CHUNK_OVERLAP, trimmed, empty windows dropped."""
    out = []
    start, n = 0, len(text)
    overlap = min(CHUNK_OVERLAP, CHUNK_CHARS - 1)
    while start < n:
        end = min(start + CHUNK_CHARS, n)
        if end < n:
            rel = last_whitespace(text[start:end])
            if rel >= 0 and rel + 1 >= CHUNK_CHARS // 2:
                end = start + rel + 1
        if end <= start:
            end = min(start + CHUNK_CHARS, n)
        trimmed = text[start:end].strip()
        if trimmed:
            out.append(trimmed)
        if end >= n:
            break
        nxt = max(end - overlap, 0)
        start = end if nxt <= start else nxt
    return out


def query_text(chunk: str) -> str:
    if len(chunk) <= QUERY_CHARS:
        return chunk
    head = chunk[:QUERY_CHARS]
    cut = last_whitespace(head)
    return (head[:cut] if cut >= QUERY_CHARS // 2 else head).strip()


def file_digest(path: Path) -> str:
    hasher = blake3.blake3()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(block)
    return hasher.hexdigest()


def corpus_documents(root: Path, stats: dict):
    """Text files under the corpus in sorted path order. The tree digest
    covers relative paths and file digests of the included files only."""
    tree = blake3.blake3()
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        rel = path.relative_to(root).as_posix()
        if path.suffix.lower() == ".pdf":
            stats["skipped_pdf"] += 1
            continue
        if path.stat().st_size > MAX_FILE_BYTES:
            stats["skipped_large"] += 1
            continue
        raw = path.read_bytes()
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            stats["skipped_binary"] += 1
            continue
        if "\x00" in text:
            stats["skipped_binary"] += 1
            continue
        stats["files"] += 1
        tree.update(f"{rel}\0{blake3.blake3(raw).hexdigest()}\n".encode())
        yield text
    stats["tree_blake3"] = tree.hexdigest()


def wikipedia_documents(shards: list[Path], stats: dict):
    for shard in shards:
        for batch in pq.ParquetFile(shard).iter_batches(columns=["title", "text"], batch_size=1024):
            for title, text in zip(batch.column("title").to_pylist(), batch.column("text").to_pylist()):
                stats["articles"] += 1
                yield f"{title}\n\n{text}"
        stats["shards_read"] += 1


def prepare(args) -> int:
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    if (work / "plan.json").exists():
        print("plan.json exists; refusing to re-prepare", file=sys.stderr)
        return 2
    shards = sorted(args.wikipedia.glob("*.parquet"))
    sums = {}
    sums_file = args.wikipedia / "SHA256SUMS"
    if sums_file.exists():
        for line in sums_file.read_text().splitlines():
            digest, name = line.split()
            sums[name] = digest
    corpus_stats = {"files": 0, "skipped_pdf": 0, "skipped_large": 0, "skipped_binary": 0, "chunks_base": 0, "chunks_query": 0}
    wiki_stats = {"articles": 0, "shards_read": 0, "chunks_base": 0, "chunks_query": 0}
    base_out = (work / "base.jsonl").open("w")
    query_out = (work / "queries.jsonl").open("w")
    base = queries = index = 0

    def take(chunks, stats):
        nonlocal base, queries, index
        for chunk in chunks:
            if base >= args.base_count and queries >= args.query_count:
                return True
            if index % QUERY_EVERY == QUERY_EVERY - 1 and queries < args.query_count:
                query_out.write(json.dumps({"i": index, "t": query_text(chunk)}) + "\n")
                queries += 1
                stats["chunks_query"] += 1
            elif base < args.base_count:
                base_out.write(json.dumps({"i": index, "t": chunk}) + "\n")
                base += 1
                stats["chunks_base"] += 1
            index += 1
        return False

    done = False
    for text in corpus_documents(args.corpus, corpus_stats):
        if take(chunk_text(text), corpus_stats):
            done = True
            break
    if not done:
        for text in wikipedia_documents(shards, wiki_stats):
            if take(chunk_text(text), wiki_stats):
                done = True
                break
    base_out.close()
    query_out.close()
    if not done:
        print(f"sources exhausted at {base} base and {queries} queries", file=sys.stderr)
        return 1
    plan = {
        "base_count": base,
        "query_count": queries,
        "chunking": {"chunk_chars": CHUNK_CHARS, "chunk_overlap": CHUNK_OVERLAP,
                     "port_of": "prototype/src/chunk.rs::chunk_text",
                     "max_file_bytes": MAX_FILE_BYTES},
        "split": f"every {QUERY_EVERY}th chunk in stream order is a query source until "
                 f"{queries} queries; a query is the first {QUERY_CHARS} characters of "
                 "its chunk, cut at whitespace; all other chunks are base, in order",
        "sources": [
            {"name": "phase-0 mixed corpus (private; text files only, PDFs skipped)",
             "path_hint": "mixed-v4-20260719", **corpus_stats},
            {"name": "Wikipedia 20231101.en (wikimedia/wikipedia on Hugging Face)",
             "revision": args.wikipedia_revision,
             "files": [{"name": s.name, "sha256": sums.get(s.name), "blake3": file_digest(s)}
                       for s in shards[: max(wiki_stats["shards_read"] + 1, 1)]],
             **wiki_stats},
        ],
    }
    (work / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    print(json.dumps(plan, indent=2))
    return 0


def ollama_digest(base_url: str) -> str:
    with urllib.request.urlopen(f"{base_url}/api/tags", timeout=30) as response:
        tags = json.load(response)
    for model in tags["models"]:
        if model["name"] in (MODEL, f"{MODEL}:latest"):
            return model["digest"]
    raise SystemExit(f"Ollama has no {MODEL}")


def embed_batch(base_url: str, texts: list[str], prefix: str) -> np.ndarray:
    body = json.dumps({"model": MODEL, "input": [prefix + t for t in texts]}).encode()
    request = urllib.request.Request(f"{base_url}/v1/embeddings", data=body,
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=600) as response:
        data = json.load(response)["data"]
    if len(data) != len(texts):
        raise SystemExit(f"Ollama returned {len(data)} embeddings for {len(texts)} inputs")
    out = np.empty((len(texts), DIMS), dtype=np.float32)
    seen = set()
    for item in data:
        vector = np.asarray(item["embedding"], dtype=np.float32)
        if vector.shape != (NATIVE_DIMS,) or not np.isfinite(vector).all():
            raise SystemExit(f"malformed embedding at batch index {item['index']}")
        vector = vector[:DIMS]
        norm = float(np.linalg.norm(vector))
        if norm == 0.0:
            raise SystemExit(f"zero embedding at batch index {item['index']}")
        out[item["index"]] = vector / norm
        seen.add(item["index"])
    if seen != set(range(len(texts))):
        raise SystemExit("Ollama response indices do not cover the batch")
    return out


def write_progress(work: Path, progress: dict) -> None:
    temporary = work / "progress.json.tmp"
    temporary.write_text(json.dumps(progress, indent=2) + "\n")
    os.replace(temporary, work / "progress.json")


def embed_file(work, name, count, prefix, progress, key, limit, base_url):
    matrix = np.lib.format.open_memmap(work / f"{name}.npy", mode="r+" if (work / f"{name}.npy").exists() else "w+",
                                       dtype=np.float32, shape=(count, DIMS))
    done = progress[key]
    target = min(count, limit)
    started, first = time.time(), done
    with (work / f"{name}.jsonl").open() as handle:
        for _ in range(done):
            handle.readline()
        while done < target:
            texts = []
            while len(texts) < BATCH and done + len(texts) < target:
                texts.append(json.loads(handle.readline())["t"])
            matrix[done:done + len(texts)] = embed_batch(base_url, texts, prefix)
            done += len(texts)
            if done % (BATCH * 40) < BATCH or done == target:
                matrix.flush()
                progress[key] = done
                write_progress(work, progress)
                rate = (done - first) / max(time.time() - started, 1e-9)
                print(f"{name}: {done}/{target} ({rate:.0f}/s)", flush=True)
    matrix.flush()
    progress[key] = done
    write_progress(work, progress)


def embed(args) -> int:
    work = args.work
    plan = json.loads((work / "plan.json").read_text())
    digest = ollama_digest(args.ollama)
    progress_path = work / "progress.json"
    if progress_path.exists():
        progress = json.loads(progress_path.read_text())
        if progress["ollama_digest"] != digest:
            print(f"Ollama {MODEL} digest changed: {progress['ollama_digest']} -> {digest}", file=sys.stderr)
            return 2
    else:
        progress = {"ollama_digest": digest, "queries_done": 0, "base_done": 0}
        write_progress(work, progress)
    embed_file(work, "queries", plan["query_count"], QUERY_PREFIX, progress, "queries_done",
               plan["query_count"], args.ollama)
    embed_file(work, "base", plan["base_count"], DOC_PREFIX, progress, "base_done",
               args.limit or plan["base_count"], args.ollama)
    if ollama_digest(args.ollama) != digest:
        print("Ollama model digest changed during the run", file=sys.stderr)
        return 2
    return 0


def groundtruth(base: np.ndarray, queries: np.ndarray, k: int) -> np.ndarray:
    """Exact top-k by dot product on unit vectors (cosine), ties by id."""
    out = np.empty((len(queries), k), dtype=np.int32)
    block = 100_000
    for q0 in range(0, len(queries), 1000):
        q = queries[q0:q0 + 1000]
        best_scores = np.full((len(q), 0), -np.inf, dtype=np.float32)
        best_ids = np.zeros((len(q), 0), dtype=np.int64)
        for b0 in range(0, len(base), block):
            scores = q @ base[b0:b0 + block].T
            take = min(k, scores.shape[1])
            part = np.argpartition(-scores, take - 1, axis=1)[:, :take]
            best_scores = np.concatenate([best_scores, np.take_along_axis(scores, part, axis=1)], axis=1)
            best_ids = np.concatenate([best_ids, part + b0], axis=1)
            keep = min(k, best_scores.shape[1])
            order = np.lexsort((best_ids, -best_scores), axis=1)[:, :keep]
            best_scores = np.take_along_axis(best_scores, order, axis=1)
            best_ids = np.take_along_axis(best_ids, order, axis=1)
        out[q0:q0 + len(q)] = best_ids
    return out


def finalize(args) -> int:
    work = args.work
    plan = json.loads((work / "plan.json").read_text())
    progress = json.loads((work / "progress.json").read_text())
    if progress["queries_done"] != plan["query_count"] or progress["base_done"] < args.base_count:
        print(f"only {progress['base_done']} base and {progress['queries_done']} queries embedded", file=sys.stderr)
        return 2
    base = np.load(work / "base.npy", mmap_mode="r")[: args.base_count]
    queries = np.load(work / "queries.npy")
    started = time.time()
    truth = groundtruth(np.ascontiguousarray(base), queries, GROUNDTRUTH_K)
    args.out.mkdir(parents=True, exist_ok=True)
    np.ascontiguousarray(base, dtype="<f4").tofile(args.out / "base.f32le")
    np.ascontiguousarray(queries, dtype="<f4").tofile(args.out / "queries.f32le")
    np.ascontiguousarray(truth, dtype="<i4").tofile(args.out / "groundtruth.i32le")
    sources = plan["sources"]
    manifest = {
        "manifest_type": "BenchDatasetV1",
        "version": 1,
        "name": args.name,
        "source": "built from text with scripts/bench/build-project-set.py",
        "converted_with": "scripts/bench/build-project-set.py",
        "metric": "cosine",
        "dims": DIMS,
        "base_count": int(args.base_count),
        "query_count": int(len(queries)),
        "groundtruth_k": GROUNDTRUTH_K,
        "embedding": {
            "model": MODEL,
            "ollama_digest": progress["ollama_digest"],
            "native_dims": NATIVE_DIMS,
            "recipe": f'prefix "{DOC_PREFIX}" (base) or "{QUERY_PREFIX}" (queries); '
                      f"truncate {NATIVE_DIMS} -> {DIMS}; L2-normalize (DESIGN §15)",
        },
        "chunking": plan["chunking"],
        "split": plan["split"],
        "base_prefix_of": f"{plan['base_count']} prepared base chunks; this set uses the first {args.base_count}",
        "sources": sources,
        "groundtruth": "exact top-100 by cosine over this base (numpy float32), ties by id",
        "files": {
            name: {"bytes": (args.out / name).stat().st_size, "blake3": file_digest(args.out / name)}
            for name in ("base.f32le", "queries.f32le", "groundtruth.i32le")
        },
    }
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"{args.name}: {args.base_count} x {DIMS}, {len(queries)} queries, "
          f"ground truth in {time.time() - started:.0f}s")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--wikipedia", type=Path, required=True)
    p.add_argument("--wikipedia-revision", required=True)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--base-count", type=int, default=1_000_000)
    p.add_argument("--query-count", type=int, default=10_000)
    e = sub.add_parser("embed")
    e.add_argument("--work", type=Path, required=True)
    e.add_argument("--limit", type=int, help="embed only the first N base chunks for now")
    e.add_argument("--ollama", default="http://127.0.0.1:11434")
    f = sub.add_parser("finalize")
    f.add_argument("--work", type=Path, required=True)
    f.add_argument("--base-count", type=int, required=True)
    f.add_argument("--name", required=True)
    f.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    return {"prepare": prepare, "embed": embed, "finalize": finalize}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())

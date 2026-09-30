#!/usr/bin/env python3
"""Convert an ann-benchmarks HDF5 dataset into the flat pinned layout the
distr-hnsw bench harness reads (docs/m3-implementation-plan.md, "Datasets").

    convert-annb.py <input.hdf5> <output-dir> --name <name> --source <url>

Writes base.f32le, queries.f32le, groundtruth.i32le (row-major, little
endian) and manifest.json with counts, dimensions, metric, and the BLAKE3 of
every file. Vectors are written exactly as distributed; normalization for
angular datasets is the engine's job, so the oracle sees the same bytes.
"""
import argparse
import json
import sys
import time
from pathlib import Path

import blake3
import h5py
import numpy as np

METRICS = {"angular": "cosine", "euclidean": "l2", "dot": "dot"}


def digest(path: Path) -> str:
    hasher = blake3.blake3()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(block)
    return hasher.hexdigest()


def write(array: np.ndarray, path: Path, dtype) -> None:
    np.ascontiguousarray(array, dtype=dtype).tofile(path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--name", required=True)
    parser.add_argument("--source", required=True)
    args = parser.parse_args()

    started = time.time()
    with h5py.File(args.input, "r") as f:
        distance = f.attrs.get("distance")
        if isinstance(distance, bytes):
            distance = distance.decode()
        metric = METRICS.get(str(distance))
        if metric is None:
            print(f"unsupported distance attribute {distance!r}", file=sys.stderr)
            return 2
        train = f["train"]
        test = f["test"]
        neighbors = f["neighbors"]
        if train.shape[1] != test.shape[1]:
            print("train/test dimension mismatch", file=sys.stderr)
            return 2
        args.output.mkdir(parents=True, exist_ok=True)
        write(train[...], args.output / "base.f32le", "<f4")
        write(test[...], args.output / "queries.f32le", "<f4")
        write(neighbors[...], args.output / "groundtruth.i32le", "<i4")
        manifest = {
            "manifest_type": "BenchDatasetV1",
            "version": 1,
            "name": args.name,
            "source": args.source,
            "source_file_blake3": digest(args.input),
            "converted_with": "scripts/bench/convert-annb.py",
            "metric": metric,
            "dims": int(train.shape[1]),
            "base_count": int(train.shape[0]),
            "query_count": int(test.shape[0]),
            "groundtruth_k": int(neighbors.shape[1]),
            "files": {
                name: {
                    "bytes": (args.output / name).stat().st_size,
                    "blake3": digest(args.output / name),
                }
                for name in ("base.f32le", "queries.f32le", "groundtruth.i32le")
            },
        }
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(
        f"{args.name}: {manifest['base_count']} x {manifest['dims']} ({metric}), "
        f"{manifest['query_count']} queries, k={manifest['groundtruth_k']}, "
        f"{time.time() - started:.1f}s"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Compare two phase-0 reports while treating latency as nondeterministic."""

from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
from typing import Any


LATENCY_FIELDS = {
    "cold_latency_ms",
    "warm_mean_latency_ms",
    "warm_p50_latency_ms",
    "warm_p95_latency_ms",
}


def load_report(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        report = json.load(handle)
    if not isinstance(report, dict):
        raise ValueError(f"report must be a JSON object: {path}")
    return report


def retrieval_view(report: dict[str, Any]) -> dict[str, Any]:
    """Return all deterministic evidence, excluding timestamps and latency."""
    view = copy.deepcopy(report)
    view.pop("generated_at", None)
    for config in view.get("configs", []):
        for field in LATENCY_FIELDS:
            config.pop(field, None)
    return view


def latency_view(report: dict[str, Any]) -> dict[str, dict[str, float | None]]:
    values: dict[str, dict[str, float | None]] = {}
    for config in report.get("configs", []):
        key = f"{config.get('model')}:{config.get('dims')}"
        values[key] = {field: config.get(field) for field in sorted(LATENCY_FIELDS)}
    return values


def first_difference(left: Any, right: Any, path: str = "$") -> str | None:
    if type(left) is not type(right):
        return f"{path}: type {type(left).__name__} != {type(right).__name__}"
    if isinstance(left, dict):
        if left.keys() != right.keys():
            return f"{path}: keys differ"
        for key in left:
            difference = first_difference(left[key], right[key], f"{path}.{key}")
            if difference:
                return difference
        return None
    if isinstance(left, list):
        if len(left) != len(right):
            return f"{path}: length {len(left)} != {len(right)}"
        for index, (left_item, right_item) in enumerate(zip(left, right, strict=True)):
            difference = first_difference(left_item, right_item, f"{path}[{index}]")
            if difference:
                return difference
        return None
    if left != right:
        return f"{path}: {left!r} != {right!r}"
    return None


def compare_reports(first: dict[str, Any], repeat: dict[str, Any]) -> dict[str, Any]:
    first_provenance = first.get("provenance", {})
    repeat_provenance = repeat.get("provenance", {})
    required_provenance = (
        "source_revision",
        "source_tree_blake3",
        "executable_blake3",
        "corpus_index_blake3",
        "query_set_blake3",
        "models",
    )
    provenance_issues = []
    for field in required_provenance:
        first_value = first_provenance.get(field)
        repeat_value = repeat_provenance.get(field)
        if first_value in (None, "", "unknown"):
            provenance_issues.append(f"first report has invalid {field}")
        if repeat_value in (None, "", "unknown"):
            provenance_issues.append(f"repeat report has invalid {field}")
        if first_value != repeat_value:
            provenance_issues.append(f"{field} differs between reports")

    first_retrieval = retrieval_view(first)
    repeat_retrieval = retrieval_view(repeat)
    difference = first_difference(first_retrieval, repeat_retrieval)
    retrieval_equal = difference is None
    provenance_equal = not provenance_issues

    return {
        "retrieval_metrics_equal_on_repeat": retrieval_equal,
        "provenance_equal_and_complete": provenance_equal,
        "first_difference": difference,
        "provenance_issues": provenance_issues,
        "latency": {
            "first": latency_view(first),
            "repeat": latency_view(repeat),
        },
        "passed": retrieval_equal and provenance_equal,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("first", type=Path)
    parser.add_argument("repeat", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    result = compare_reports(load_report(args.first), load_report(args.repeat))
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

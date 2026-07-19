import copy
import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("compare-eval-reports.py")
SPEC = importlib.util.spec_from_file_location("compare_eval_reports", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def report():
    return {
        "generated_at": "2026-07-19T00:00:00Z",
        "k": 10,
        "recommendation": "Suggested model=`nomic-embed-text` dims=512, warm latency p50/p95=19/25ms). dims locked",
        "provenance": {
            "source_revision": "abc123",
            "source_tree_blake3": "source-hash",
            "executable_blake3": "exe-hash",
            "corpus_index_blake3": "corpus-hash",
            "query_set_blake3": "query-hash",
            "models": [{"name": "nomic-embed-text", "dims": 512}],
        },
        "configs": [
            {
                "model": "nomic-embed-text",
                "dims": 512,
                "wins_vs_name": 3,
                "cold_latency_ms": 100.0,
                "mean_latency_ms": 21.0,
                "warm_mean_latency_ms": 20.0,
                "warm_p50_latency_ms": 19.0,
                "warm_p95_latency_ms": 25.0,
            }
        ],
        "queries": [
            {
                "query_id": "q1",
                "semantic": [{"rank": 1, "file_id": 7, "score": 0.8}],
            }
        ],
    }


class CompareEvalReportsTests(unittest.TestCase):
    def test_latency_variance_is_allowed(self):
        first = report()
        repeat = copy.deepcopy(first)
        repeat["generated_at"] = "2026-07-19T00:01:00Z"
        repeat["configs"][0]["cold_latency_ms"] = 120.0
        repeat["configs"][0]["mean_latency_ms"] = 22.0
        repeat["configs"][0]["warm_p95_latency_ms"] = 29.0
        repeat["recommendation"] = "Suggested model=`nomic-embed-text` dims=512, warm latency p50/p95=20/29ms). dims locked"

        result = MODULE.compare_reports(first, repeat)

        self.assertTrue(result["passed"])
        self.assertTrue(result["retrieval_metrics_equal_on_repeat"])

    def test_recommended_model_difference_fails(self):
        first = report()
        repeat = copy.deepcopy(first)
        repeat["recommendation"] = first["recommendation"].replace("dims=512", "dims=384")

        result = MODULE.compare_reports(first, repeat)

        self.assertFalse(result["passed"])
        self.assertIn("recommendation", result["first_difference"])

    def test_ranking_difference_fails(self):
        first = report()
        repeat = copy.deepcopy(first)
        repeat["queries"][0]["semantic"][0]["file_id"] = 8

        result = MODULE.compare_reports(first, repeat)

        self.assertFalse(result["passed"])
        self.assertIn("file_id", result["first_difference"])

    def test_unknown_revision_fails(self):
        first = report()
        repeat = copy.deepcopy(first)
        first["provenance"]["source_revision"] = "unknown"
        repeat["provenance"]["source_revision"] = "unknown"

        result = MODULE.compare_reports(first, repeat)

        self.assertFalse(result["passed"])
        self.assertFalse(result["provenance_equal_and_complete"])


if __name__ == "__main__":
    unittest.main()

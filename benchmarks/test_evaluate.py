"""Deterministic checks of the paired release gate; no timing assertions."""

import copy
import itertools
import math
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from benchmarks import evaluate
from benchmarks.run import SURFACES


def report(commit, cost=1.0):
    rows = []
    for surface, mode, body, read, concurrency in itertools.product(
        SURFACES,
        ("one-shot", "pooled"),
        ("small", "large"),
        ("buffered", "streaming"),
        (1, 4),
    ):
        factor = cost if surface != "python-oracle" else 1.0
        rows.append(
            {
                "surface": surface,
                "mode": mode,
                "body": body,
                "read": read,
                "concurrency": concurrency,
                "requests": 100,
                "allocation_replay_concurrency": 1,
                "latency_ns_samples": [int(1_000_000 * factor)] * 100,
                "latency_ms": {"p95": factor},
                "throughput_requests_per_second": 100 / factor,
                "cpu_seconds": factor,
                "rss_scope": "measured-phase",
                "rss_peak_bytes": int(1000 * factor),
                "python_peak_alloc_bytes": int(500 * factor)
                if surface.startswith("python")
                else None,
                "native_total_allocated_bytes": int(500 * factor)
                if surface.startswith("rust")
                else None,
                "body_bytes_total": 100,
                "checksum_total": 100,
                "application_chunks": 0,
                "connections_opened": 1,
                "requests_reusing_connections": 99,
            }
        )
    return {
        "schema_version": 3,
        "evaluator_sha256": "a" * 64,
        "machine": {"platform": "linux"},
        "toolchains": {"rustc": "same"},
        "git": {
            "rewrite_commit": commit,
            "rewrite_dirty": False,
            "oracle_commit": "oracle",
        },
        "python_artifact": {"versions": {}, "python_soabi": "same"},
        "config": {
            "surfaces": list(SURFACES),
            "concurrency_levels": [1, 4],
            "requests_per_case": 100,
            "warmup_per_worker": 4,
            "small_bytes": 1,
            "large_bytes": 1,
        },
        "results": rows,
    }


class EvaluationTests(unittest.TestCase):
    def test_preflight_rejects_missing_or_incompatible_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "pyproject.toml"
            for requirement in (
                "pip>99999",
                "requests-native-missing-evaluation-dependency>=1",
            ):
                project.write_text(
                    f'[project]\nrequires-python = ">=3.10"\ndependencies = ["{requirement}"]\n'
                )
                with (
                    self.subTest(requirement=requirement),
                    self.assertRaises(ValueError),
                ):
                    evaluate.check_runtime_dependencies(root)
            project.write_text(
                '[project]\nrequires-python = ">=3.10"\ndependencies = ["pip>=1; python_version >= \'3.10\'"]\n'
            )
            evaluate.check_runtime_dependencies(root)

    def test_worktree_snapshot_preserves_build_permissions_and_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source"
            files = ["Cargo.lock", "src/build-helper.sh", *evaluate.EVALUATOR_FILES[2:]]
            for name in files:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fixture")
            executable = root / "src/build-helper.sh"
            executable.chmod(0o755)
            listing = b"\0".join(os.fsencode(name) for name in files)
            with (
                mock.patch.object(evaluate, "ROOT", root),
                mock.patch.object(
                    evaluate.subprocess,
                    "check_output",
                    side_effect=[listing, b"", listing, b""],
                ),
            ):
                first = evaluate.snapshot("WORKTREE", Path(directory) / "first")
                self.assertEqual(
                    (Path(directory) / "first/src/build-helper.sh").stat().st_mode
                    & 0o777,
                    0o755,
                )
                executable.chmod(0o644)
                second = evaluate.snapshot("WORKTREE", Path(directory) / "second")
                self.assertNotEqual(first, second)

    def pairs(self, cost=1.0):
        return [(report("base"), report("candidate", cost)) for _ in range(5)]

    def test_stable_pairs_pass_and_sustained_regression_fails(self):
        self.assertEqual(
            evaluate.compare_pairs(self.pairs(), gate=True)["status"], "passed"
        )
        self.assertEqual(
            evaluate.compare_pairs(self.pairs(1.4), gate=True)["status"], "regression"
        )

    def test_small_or_unstable_evidence_cannot_pass_release(self):
        self.assertEqual(
            evaluate.compare_pairs(self.pairs()[:1], gate=True)["status"],
            "inconclusive",
        )
        pairs = self.pairs()
        pairs[-1] = (report("base"), report("candidate", 2.0))
        self.assertEqual(
            evaluate.compare_pairs(pairs, gate=True)["status"], "inconclusive"
        )

        pairs = self.pairs()
        for pair in pairs:
            for document in pair:
                document["config"]["concurrency_levels"] = [1]
                document["results"] = [
                    row for row in document["results"] if row["concurrency"] == 1
                ]
        self.assertEqual(
            evaluate.compare_pairs(pairs, gate=True)["status"], "inconclusive"
        )

    def test_oracle_drift_is_noise_not_a_candidate_regression(self):
        pairs = self.pairs()
        for _, candidate in pairs:
            for row in candidate["results"]:
                if row["surface"] == "python-oracle":
                    row["latency_ms"]["p95"] *= 2
        self.assertEqual(
            evaluate.compare_pairs(pairs, gate=True)["status"], "inconclusive"
        )

    def test_malformed_or_mismatched_reports_fail_closed(self):
        for mutate in (
            lambda r: r["results"].pop(),
            lambda r: r["results"].append(copy.deepcopy(r["results"][0])),
            lambda r: r["config"].update(requests_per_case=101),
            lambda r: r["toolchains"].update(rustc="different"),
            lambda r: r.update(evaluator_sha256="different"),
            lambda r: r["results"][0].update(throughput_requests_per_second=math.nan),
            lambda r: r["results"][0].update(rss_peak_bytes=-1),
            lambda r: r["results"][0].update(cpu_seconds=None),
            lambda r: r["results"][0]["latency_ns_samples"].__setitem__(0, math.nan),
            lambda r: r["results"][0]["latency_ns_samples"].__setitem__(0, math.inf),
            lambda r: r["git"].update(rewrite_dirty=True),
        ):
            pairs = self.pairs()
            mutate(pairs[-1][1])
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                evaluate.compare_pairs(pairs, gate=True)

    def test_tick_granular_zero_cpu_is_inconclusive(self):
        pairs = self.pairs()
        for pair in pairs:
            for document in pair:
                for row in document["results"]:
                    row["cpu_seconds"] = 0.0
        self.assertEqual(
            evaluate.compare_pairs(pairs, gate=True)["status"], "inconclusive"
        )


if __name__ == "__main__":
    unittest.main()

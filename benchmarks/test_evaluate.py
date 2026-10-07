"""Deterministic checks of the paired release gate; no timing assertions."""

import copy
import itertools
import json
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
        "machine": {"platform": "linux", "allowed_cpus": [0, 1]},
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
    def core_pairs(self):
        pairs = self.pairs()
        for pair in pairs:
            for document in pair:
                document["evaluator_commit"] = "d" * 40
                document["config"]["qualification_scope"] = "core"
                document["config"]["surfaces"] = [
                    "python-oracle",
                    "rust-async",
                    "rust-blocking",
                ]
                document["results"] = [
                    row
                    for row in document["results"]
                    if row["surface"] != "python-rust"
                ]
        return pairs

    def test_core_scope_requires_explicit_scope_and_preserves_full_default(self):
        result = evaluate.compare_pairs(self.core_pairs(), gate=True, scope="core")
        self.assertEqual(result["status"], "passed")
        self.assertFalse(result["insufficient_release_evidence"])
        self.assertEqual(result["qualification_scope"], "core")
        self.assertEqual(result["evaluator_commit"], "d" * 40)
        self.assertEqual(len(result["metrics"]), 195)
        with self.assertRaises(ValueError):
            evaluate.compare_pairs(self.core_pairs(), gate=True)
        self.assertEqual(
            evaluate.compare_pairs(self.pairs(), gate=True)["status"], "passed"
        )

    def test_core_scope_rejects_malformed_scope_source_and_driver_provenance(self):
        for mutate in (
            lambda r: r["config"].update(qualification_scope="full"),
            lambda r: r["config"].pop("qualification_scope"),
            lambda r: r["config"]["surfaces"].append("python-rust"),
            lambda r: r["config"]["surfaces"].remove("rust-blocking"),
            lambda r: r.pop("evaluator_commit"),
            lambda r: r.update(evaluator_commit="not-a-commit"),
            lambda r: r.update(evaluator_commit="e" * 40),
            lambda r: r.update(evaluator_sha256="different"),
            lambda r: r["git"].update(rewrite_commit="other-source"),
            lambda r: r["git"].update(rewrite_dirty=True),
            lambda r: r["results"].pop(),
        ):
            pairs = self.core_pairs()
            mutate(pairs[-1][1])
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                evaluate.compare_pairs(pairs, gate=True, scope="core")
        with self.assertRaises(ValueError):
            evaluate.compare_pairs(self.pairs(), gate=True, scope="partial")

    def test_core_scope_preserves_insufficient_cpu_rss_and_regression_policy(self):
        for change in ("pairs", "count", "warmup", "concurrency", "cpu", "rss"):
            pairs = self.core_pairs()
            if change == "pairs":
                pairs.pop()
            for pair in pairs:
                for document in pair:
                    if change == "count":
                        document["config"]["requests_per_case"] = 99
                        for row in document["results"]:
                            row["requests"] = 99
                            row["latency_ns_samples"].pop()
                    elif change == "warmup":
                        document["config"]["warmup_per_worker"] = 3
                    elif change == "concurrency":
                        document["config"]["concurrency_levels"] = [1]
                        document["results"] = [
                            r for r in document["results"] if r["concurrency"] == 1
                        ]
                    elif change in ("cpu", "rss"):
                        for row in document["results"]:
                            row.update(
                                {"cpu_seconds": 0}
                                if change == "cpu"
                                else {"rss_scope": "process"}
                            )
            with self.subTest(change=change):
                self.assertTrue(
                    evaluate.compare_pairs(pairs, gate=True, scope="core")[
                        "insufficient_release_evidence"
                    ]
                )
        pairs = self.core_pairs()
        for _, candidate in pairs:
            for row in candidate["results"]:
                if row["surface"] != "python-oracle":
                    row["latency_ms"]["p95"] *= 1.4
        self.assertEqual(
            evaluate.compare_pairs(pairs, gate=True, scope="core")["status"],
            "regression",
        )

    def test_driver_identity_rejects_uncommitted_evaluator_for_release(self):
        with (
            mock.patch.object(
                evaluate.subprocess,
                "check_output",
                side_effect=[b"d" * 40 + b"\n", b"benchmarks/evaluate.py\n"],
            ),
            self.assertRaisesRegex(ValueError, "evaluator.*committed"),
        ):
            evaluate.evaluator_commit(gate=True)

    def test_cli_keeps_candidate_separate_from_driver_and_only_restores_full_scope(
        self,
    ):
        for scope in ("full", "core"):
            with self.subTest(scope=scope), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                oracle = root / "oracle/src/requests"
                oracle.mkdir(parents=True)
                (oracle / "__init__.py").touch()
                output = root / "evidence"
                commands = []
                source_sha, driver_sha = "c" * 40, "d" * 40

                def snapshot(revision, destination):
                    destination.mkdir()
                    (destination / "rust-toolchain.toml").write_text(
                        '[toolchain]\nchannel = "1.98.1"\n'
                    )
                    (destination / "Cargo.lock").write_text("transport-lock")
                    driver = destination / "benchmarks/rust-native"
                    driver.mkdir(parents=True)
                    (driver / "Cargo.lock").write_text("driver-lock")
                    return revision

                def invoke(command, **kwargs):
                    commands.append(command)
                    if "--output" in command:
                        document = (
                            self.core_pairs()[0][0]
                            if scope == "core"
                            else report("base")
                        )
                        document["git"]["rewrite_commit"] = kwargs["env"][
                            "REQUESTS_BENCHMARK_COMMIT"
                        ]
                        destination = Path(command[command.index("--output") + 1])
                        destination.write_text(json.dumps(document))

                options = ["--scope", "core"] if scope == "core" else []
                with (
                    mock.patch.object(evaluate, "ROOT", root),
                    mock.patch.object(evaluate.run, "ORACLE_ROOT", root / "oracle"),
                    mock.patch.object(
                        evaluate, "evaluator_commit", return_value=driver_sha
                    ),
                    mock.patch.object(
                        evaluate, "evaluator_digest", return_value="a" * 64
                    ),
                    mock.patch.object(
                        evaluate, "snapshot", side_effect=snapshot
                    ) as snapshots,
                    mock.patch.object(evaluate, "check_runtime_dependencies"),
                    mock.patch.object(evaluate.subprocess, "run", side_effect=invoke),
                    mock.patch.object(
                        evaluate.run,
                        "resolve_maturin",
                        return_value=Path("/fake/maturin"),
                    ),
                    mock.patch(
                        "sys.argv",
                        [
                            "evaluate",
                            "--base",
                            "b" * 40,
                            "--candidate",
                            source_sha,
                            "--gate",
                            "--output",
                            str(output),
                            *options,
                        ],
                    ),
                ):
                    self.assertEqual(evaluate.main(), 0)
                self.assertEqual(snapshots.call_args_list[1].args[0], source_sha)
                document = json.loads((output / "pair-01-candidate.json").read_text())
                self.assertEqual(document["git"]["rewrite_commit"], source_sha)
                self.assertEqual(document["evaluator_commit"], driver_sha)
                self.assertEqual(document["evaluator_sha256"], "a" * 64)
                self.assertEqual(
                    sum("develop" in command for command in commands),
                    1 if scope == "full" else 0,
                )
                collectors = [command for command in commands if "--output" in command]
                self.assertEqual(len(collectors), 10)
                self.assertTrue(
                    all(
                        command[command.index("--scope") + 1] == scope
                        for command in collectors
                    )
                )

    def surface_pairs(self, count=200):
        pairs = self.pairs()
        for pair in pairs:
            for document in pair:
                counts = dict.fromkeys(SURFACES, 100)
                counts["rust-async"] = count
                document["config"]["requests_per_surface"] = counts
                for row in document["results"]:
                    if row["surface"] == "rust-async":
                        row["requests"] = count
                        row["latency_ns_samples"] = [1_000_000] * count
        return pairs

    def test_fixed_surface_counts_pass_and_each_gate_surface_requires_100(self):
        self.assertEqual(
            evaluate.compare_pairs(self.surface_pairs(), gate=True)["status"],
            "passed",
        )
        result = evaluate.compare_pairs(self.surface_pairs(99), gate=True)
        self.assertEqual(result["status"], "inconclusive")
        self.assertTrue(result["insufficient_release_evidence"])

    def test_surface_maps_and_rows_must_match_exactly(self):
        for change in (
            "missing",
            "unknown",
            "negative",
            "boolean",
            "fraction",
            "null",
            "row",
            "pair",
        ):
            pairs = self.surface_pairs(200 if change in ("row", "pair") else 100)
            if change in ("row", "pair"):
                target = pairs[-1][1]
                if change == "row":
                    row = next(
                        r for r in target["results"] if r["surface"] == "rust-async"
                    )
                    row["requests"] = 100
                    row["latency_ns_samples"] = [1_000_000] * 100
                else:
                    target["config"]["requests_per_surface"]["rust-async"] = 201
            else:
                for pair in pairs:
                    for document in pair:
                        counts = document["config"]["requests_per_surface"]
                        if change == "missing":
                            counts.pop("python-rust")
                        elif change == "unknown":
                            counts["unknown"] = 100
                        elif change == "null":
                            document["config"]["requests_per_surface"] = None
                        else:
                            counts["rust-async"] = {
                                "negative": -1,
                                "boolean": True,
                                "fraction": 100.5,
                            }[change]
            with self.subTest(change=change), self.assertRaises(ValueError):
                evaluate.compare_pairs(pairs, gate=True)

    def test_equal_numeric_float_in_later_count_map_is_rejected(self):
        pairs = self.surface_pairs()
        pairs[-1][1]["config"]["requests_per_surface"]["rust-async"] = 200.0
        with self.assertRaises(ValueError):
            evaluate.compare_pairs(pairs, gate=True)

    def test_equal_numeric_float_row_count_is_rejected(self):
        pairs = self.pairs()
        pairs[-1][1]["results"][0]["requests"] = 100.0
        with self.assertRaises(ValueError):
            evaluate.compare_pairs(pairs, gate=True)

    def test_invalid_release_samples_and_deadlines_fail_before_builds(self):
        for options, message in (
            (["--pairs", "4"], "at least 5 pairs"),
            (["--requests", "99"], "at least 5 pairs"),
            (["--warmup", "3"], "at least 5 pairs"),
            (["--surface-requests", "rust-async=99"], "at least 5 pairs"),
            (["--surface-requests", "unknown=100"], "unknown surface"),
            (
                [
                    "--surface-requests",
                    "rust-async=100",
                    "--surface-requests",
                    "rust-async=200",
                ],
                "duplicate surface",
            ),
            (["--case-timeout-seconds", "0"], "invalid evaluation"),
            (["--case-timeout-seconds", "nan"], "invalid evaluation"),
            (["--case-timeout-seconds", "inf"], "invalid evaluation"),
        ):
            with (
                self.subTest(options=options),
                mock.patch(
                    "sys.argv", ["evaluate", "--base", "HEAD", "--gate", *options]
                ),
                mock.patch("sys.stderr") as stderr,
                mock.patch.object(evaluate, "snapshot") as snapshot,
                self.assertRaises(SystemExit) as error,
            ):
                evaluate.main()
            self.assertEqual(error.exception.code, 2)
            snapshot.assert_not_called()
            self.assertIn(
                message, "".join(call.args[0] for call in stderr.write.call_args_list)
            )

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
            lambda r: r["machine"].update(allowed_cpus=[0]),
            lambda r: r["machine"].update(allowed_cpus=None),
            lambda r: r["machine"].pop("allowed_cpus"),
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

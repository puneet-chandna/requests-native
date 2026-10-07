from __future__ import annotations

import contextlib
import inspect
import json
import os
import runpy
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import benchmarks.run as benchmark
from benchmarks.run import (
    enable_tcp_nodelay,
    normalize_peak_rss,
    percentile,
    run_json_command,
    split_work,
    validate_allocations,
    validate_result_row,
    validate_worker_result,
)


class BenchmarkHarnessTests(unittest.TestCase):
    def test_failed_warmup_retains_case_worker_and_expected_contract(self):
        body = bytes(index % 251 for index in range(128))
        worker = {
            "warmup_latencies_ns": [1],
            "warmup_body_bytes": len(body),
            "warmup_checksum": benchmark.body_checksum(body),
            "warmup_application_chunks": 0,
            "warmup_native_responses": 1,
            "warmup_connection_ids": [1],
            "connection_ids": [1],
        }
        server = mock.Mock(server_address=("127.0.0.1", 1))
        server.snapshot.side_effect = [(0, 0), (3, 1)]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "pair-03-base.json"
            arguments = benchmark.parser().parse_args(
                [
                    "--surfaces",
                    "python-oracle",
                    "--requests",
                    "2",
                    "--warmup",
                    "1",
                    "--output",
                    str(output),
                ]
            )
            with (
                mock.patch.object(benchmark, "git_metadata", return_value={}),
                mock.patch.object(benchmark, "tool_version", return_value=None),
                mock.patch.object(
                    benchmark,
                    "fixture_server",
                    return_value=contextlib.nullcontext(server),
                ),
                mock.patch.object(benchmark, "fixture_self_check"),
                mock.patch.object(benchmark, "run_json_command", return_value=worker),
            ):
                with self.assertRaisesRegex(ValueError, "warm-up violates"):
                    benchmark.orchestrate(arguments)
            self.assertFalse(output.exists())
            failure = json.loads(output.with_suffix(".failure.json").read_text())
            self.assertEqual(
                failure["case"], "python-oracle/one-shot/small/buffered/concurrency-1"
            )
            self.assertEqual(failure["worker"], worker)
            self.assertEqual(failure["expected"]["warmup_native_responses"], 0)
            self.assertEqual(failure["expected"]["warmup_requests"], 1)
            self.assertEqual(failure["expected"]["fixture_requests"], 3)
            self.assertEqual(failure["actual"]["fixture_requests"], 3)
            self.assertEqual(failure["actual"]["fixture_connections"], 1)
            self.assertIn("ValueError: warm-up violates", failure["error"])

    def test_core_scope_builds_only_rust_driver_without_python_install(self):
        arguments = benchmark.parser().parse_args(["--scope", "core"])
        with (
            mock.patch.object(
                benchmark,
                "build_python_extension",
                side_effect=AssertionError("Python build"),
            ),
            mock.patch.object(
                benchmark,
                "python_artifact_provenance",
                side_effect=AssertionError("Python probe"),
            ),
            mock.patch.object(benchmark, "build_native") as native,
            mock.patch.object(benchmark, "git_metadata", return_value={}),
            mock.patch.object(benchmark, "tool_version", return_value=None),
            mock.patch.object(
                benchmark, "fixture_server", side_effect=RuntimeError("prepared core")
            ),
        ):
            with self.assertRaisesRegex(RuntimeError, "prepared core"):
                benchmark.orchestrate(arguments)
            native.assert_called_once()

    def test_core_scope_rejects_missing_or_extra_surface_before_build(self):
        for surfaces in (("rust-async",), benchmark.SURFACES):
            arguments = benchmark.parser().parse_args(
                ["--scope", "core", "--surfaces", *surfaces]
            )
            with (
                self.subTest(surfaces=surfaces),
                mock.patch.object(benchmark, "build_native") as native,
            ):
                with self.assertRaisesRegex(ValueError, "core.*surfaces"):
                    benchmark.orchestrate(arguments)
                native.assert_not_called()

    def test_oracle_only_preparation_does_not_build_or_install_native_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "oracle.json"
            arguments = benchmark.parser().parse_args(
                [
                    "--surfaces",
                    "python-oracle",
                    "--requests",
                    "2",
                    "--warmup",
                    "1",
                    "--output",
                    str(output),
                ]
            )
            with (
                mock.patch.object(
                    benchmark,
                    "build_python_extension",
                    side_effect=AssertionError("native build"),
                ),
                mock.patch.object(
                    benchmark,
                    "build_native",
                    side_effect=AssertionError("native build"),
                ),
            ):
                self.assertEqual(benchmark.orchestrate(arguments), 0)
            report = json.loads(output.read_text())
            self.assertEqual(report["python_artifact"]["scope"], "oracle-only")
            self.assertTrue(report["python_artifact"]["python_soabi"])
            self.assertEqual(len(report["results"]), 16)

    def test_checksum_detects_byte_order_and_matches_crc32(self):
        self.assertEqual(benchmark.body_checksum(b"123456789"), 0xCBF43926)
        self.assertNotEqual(
            benchmark.body_checksum(b"abc"), benchmark.body_checksum(b"cba")
        )

    @unittest.skipUnless(benchmark.NATIVE_BINARY.is_file(), "native benchmark build")
    def test_all_surfaces_preserve_crc32_across_stream_chunks(self):
        body = b"123456789"
        with benchmark.fixture_server(body, body) as server:
            host, port = server.server_address
            for surface in benchmark.SURFACES:
                for read in ("buffered", "streaming"):
                    with self.subTest(surface=surface, read=read):
                        result = benchmark.run_json_command(
                            benchmark.worker_command(
                                surface,
                                url=f"http://{host}:{port}/small",
                                requests=5,
                                concurrency=2,
                                mode="pooled",
                                read=read,
                                chunk_size=2,
                                warmup=2,
                            ),
                            cwd=benchmark.ROOT,
                            timeout_seconds=15,
                        )
                        self.assertEqual(result["checksum"], 5 * 0xCBF43926)
                        self.assertEqual(result["warmup_checksum"], 4 * 0xCBF43926)
                        self.assertEqual(
                            result["application_chunks"],
                            25 if read == "streaming" else 0,
                        )

    def test_surface_request_overrides_resolve_defaults_and_reject_invalid_values(self):
        resolve = benchmark.resolve_surface_requests
        expected = dict.fromkeys(benchmark.SURFACES, 100)
        self.assertEqual(resolve(100, []), expected)
        expected["rust-async"] = 3000
        arguments = benchmark.parser().parse_args(
            ["--requests", "100", "--surface-requests", "rust-async=3000"]
        )
        self.assertEqual(
            resolve(arguments.requests, arguments.surface_requests), expected
        )
        for overrides in (
            ["unknown=100"],
            ["rust-async=0"],
            ["rust-async=-1"],
            ["rust-async=1.5"],
            ["rust-async=nan"],
            ["rust-async"],
            ["rust-async=100", "rust-async=200"],
        ):
            with self.subTest(overrides=overrides), self.assertRaises(ValueError):
                resolve(100, overrides)

    @unittest.skipUnless(sys.platform == "linux", "Linux RSS measurement")
    def test_measured_rss_discards_released_warmup_peak(self):
        result = benchmark.run_json_command(
            [
                sys.executable,
                "-c",
                "import mmap, json; from benchmarks import run; "
                "m = mmap.mmap(-1, 64 * 1024 * 1024); "
                "[m.write(b'x' * 65536) for _ in range(1024)]; "
                "before = run.peak_rss_bytes(); m.close(); "
                "reset = run.reset_peak_rss(); after = run.peak_rss_bytes(); "
                "print(json.dumps({'before': before, 'after': after, 'reset': reset}))",
            ],
            cwd=benchmark.ROOT,
            timeout_seconds=10,
        )
        self.assertTrue(result["reset"])
        self.assertGreater(result["before"] - result["after"], 32 * 1024 * 1024)

    def test_warmup_preserves_pooled_clients_and_excludes_measured_samples(self):
        with benchmark.fixture_server(b"abc", b"abcdef") as server:
            host, port = server.server_address
            command = benchmark.worker_command(
                "python-oracle",
                url=f"http://{host}:{port}/small",
                requests=4,
                concurrency=2,
                mode="pooled",
                read="streaming",
                chunk_size=2,
                warmup=2,
            )
            result = benchmark.run_json_command(
                command, cwd=benchmark.ROOT, timeout_seconds=10
            )
            self.assertEqual(server.snapshot(), (8, 2))
            self.assertEqual(len(result["latencies_ns"]), 4)
            self.assertEqual(len(result["warmup_latencies_ns"]), 4)
            self.assertEqual(result["body_bytes"], 12)
            self.assertEqual(result["warmup_body_bytes"], 12)
            self.assertEqual(
                result["warmup_checksum"], 4 * benchmark.body_checksum(b"abc")
            )
            self.assertEqual(result["warmup_application_chunks"], 8)
            self.assertEqual(
                set(result["connection_ids"]), set(result["warmup_connection_ids"])
            )

    def test_oracle_location_accepts_an_override_and_preserves_sibling_default(self):
        with tempfile.TemporaryDirectory() as directory:
            override = Path(directory) / "frozen-oracle"
            with mock.patch.dict(os.environ, {"REQUESTS_ORACLE_ROOT": str(override)}):
                configured = runpy.run_path(benchmark.__file__)
            self.assertEqual(configured["ORACLE_ROOT"], override)
        with mock.patch.dict(os.environ):
            os.environ.pop("REQUESTS_ORACLE_ROOT", None)
            default = runpy.run_path(benchmark.__file__)
        self.assertEqual(default["ORACLE_ROOT"], benchmark.ROOT.parent / "requests")

    def test_python_artifact_provenance_separates_release_and_compat_versions(
        self,
    ) -> None:
        source = inspect.getsource(benchmark.python_artifact_provenance)

        self.assertIn('metadata.version("requests-native")', source)
        self.assertIn('"distribution_name": "requests-native"', source)
        self.assertIn('"compatibility_import": "requests"', source)
        self.assertIn('"compatibility_version": requests.__version__', source)
        self.assertIn('provenance["distribution_version"] != expected_version', source)
        self.assertIn('provenance["compatibility_version"] != "2.34.2"', source)

    def test_public_report_replaces_checkout_paths_and_rejects_home_paths(self) -> None:
        sanitizer = getattr(benchmark, "sanitize_public_report", None)
        self.assertIsNotNone(sanitizer)
        repository = Path("/home/example/projects/requests-rewrite")
        oracle = Path("/home/example/projects/requests")
        report = {
            "command": f"{repository}/.venv/bin/python {repository}/benchmarks/run.py",
            "implementation": f"{oracle}/src/requests/__init__.py",
        }
        self.assertEqual(
            sanitizer(report, repository=repository, oracle=oracle),
            {
                "command": "{repository}/.venv/bin/python {repository}/benchmarks/run.py",
                "implementation": "{oracle}/src/requests/__init__.py",
            },
        )
        with self.assertRaisesRegex(ValueError, "personal home path"):
            sanitizer(
                {"unrelated": "/home/another-user/private/file"},
                repository=repository,
                oracle=oracle,
            )

    def test_release_library_selection_is_platform_specific(self) -> None:
        resolver = getattr(benchmark, "find_release_library", lambda *_: None)
        for system, filename in (
            ("linux", "lib_requests_rust.so"),
            ("darwin", "lib_requests_rust.dylib"),
            ("win32", "_requests_rust.dll"),
        ):
            with (
                self.subTest(system=system),
                tempfile.TemporaryDirectory() as directory,
            ):
                release = Path(directory)
                expected = release / filename
                expected.touch()
                self.assertEqual(resolver(release, system), expected.resolve())

    def test_release_library_selection_rejects_ambiguous_matches(self) -> None:
        resolver = getattr(benchmark, "find_release_library", lambda *_: None)
        with tempfile.TemporaryDirectory() as directory:
            release = Path(directory)
            (release / "lib_requests_rust.so").touch()
            (release / "copy_requests_rust.so").touch()
            with self.assertRaisesRegex(RuntimeError, "exactly one"):
                resolver(release, "linux")

    def test_maturin_is_resolved_from_the_active_environment_layout(self) -> None:
        resolver = getattr(benchmark, "resolve_maturin", lambda *_args, **_kwargs: None)
        for system, scripts, executable in (
            ("linux", "bin", "maturin"),
            ("darwin", "bin", "maturin"),
            ("win32", "Scripts", "maturin.exe"),
        ):
            with self.subTest(system=system):
                environment = Path("/active-python")
                expected_directory = str(environment / scripts)
                expected = str(environment / scripts / executable)

                def fake_which(name: str, path: str | None = None) -> str | None:
                    if name == executable and path == expected_directory:
                        return expected
                    return None

                self.assertEqual(
                    resolver(environment, system, which=fake_which), Path(expected)
                )

    def test_explicit_zero_workload_values_are_not_replaced_by_defaults(self) -> None:
        selector = getattr(
            benchmark, "value_or_default", lambda value, default: value or default
        )
        self.assertEqual(selector(0, 12), 0)

    def test_each_explicit_zero_workload_value_fails_validation(self) -> None:
        resolver = getattr(benchmark, "resolve_workload", lambda _arguments: None)
        for option in (
            "--requests",
            "--concurrency",
            "--small-bytes",
            "--large-bytes",
            "--chunk-size",
            "--case-timeout-seconds",
        ):
            with self.subTest(option=option):
                arguments = benchmark.parser().parse_args([option, "0"])
                with self.assertRaisesRegex(ValueError, "positive"):
                    resolver(arguments)

    def test_percentile_uses_nearest_rank(self) -> None:
        values = [1.0, 2.0, 3.0, 4.0]
        self.assertEqual(percentile(values, 0.50), 2.0)
        self.assertEqual(percentile(values, 0.95), 4.0)

    def test_split_work_preserves_request_count(self) -> None:
        split = split_work(11, 4)
        self.assertEqual(split, [3, 3, 3, 2])
        self.assertEqual(sum(split), 11)

    def test_worker_validation_rejects_unequal_body_bytes(self) -> None:
        worker = {
            "latencies_ns": [1, 2],
            "body_bytes": 7,
            "checksum": 9,
            "connection_ids": [1],
            "elapsed_ns": 3,
            "cpu_seconds": 0.0,
            "rss_peak_bytes": 1,
            "python_peak_alloc_bytes": None,
            "native_responses": 2,
            "application_chunks": 0,
            "native_total_allocated_bytes": None,
            "implementation": "/tmp/requests/__init__.py",
        }
        with self.assertRaisesRegex(ValueError, "body bytes"):
            validate_worker_result(
                worker,
                requests=2,
                expected_bytes=4,
                expected_checksum=9,
                expected_application_chunks=0,
                require_native=True,
            )

    def test_tcp_nodelay_is_enabled_on_accepted_fixture_socket(self) -> None:
        class FakeSocket:
            value = 0

            def setsockopt(self, level, option, value):
                self.call = (level, option, value)
                self.value = value

            def getsockopt(self, level, option):
                return self.value

        connection = FakeSocket()
        enable_tcp_nodelay(connection)
        self.assertEqual(connection.value, 1)

    def test_peak_rss_units_are_platform_specific(self) -> None:
        self.assertEqual(normalize_peak_rss(7, "linux"), 7 * 1024)
        self.assertEqual(normalize_peak_rss(7, "darwin"), 7)

    def test_subprocess_deadline_reports_the_command(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "timed out.*sleep"):
            run_json_command(
                [sys.executable, "-c", "import time; time.sleep(1)"],
                cwd=None,
                timeout_seconds=0.02,
            )

    def test_native_allocation_is_required_for_native_surfaces(self) -> None:
        with self.assertRaisesRegex(ValueError, "native allocation"):
            validate_allocations(
                {"python_peak_alloc_bytes": None, "native_total_allocated_bytes": None},
                "rust-async",
            )

    def test_result_schema_requires_application_chunks(self) -> None:
        with self.assertRaisesRegex(ValueError, "application_chunks"):
            validate_result_row(
                {
                    "surface": "python-oracle",
                    "mode": "pooled",
                    "body": "small",
                    "read": "streaming",
                    "concurrency": 1,
                    "requests": 1,
                    "allocation_replay_concurrency": 1,
                    "latency_ns_samples": [1],
                    "latency_ms": {},
                    "throughput_requests_per_second": 1.0,
                    "python_peak_alloc_bytes": 1,
                    "native_total_allocated_bytes": None,
                    "body_bytes_total": 1,
                    "checksum_total": 1,
                    "connections_opened": 1,
                    "requests_reusing_connections": 0,
                }
            )


if __name__ == "__main__":
    unittest.main()

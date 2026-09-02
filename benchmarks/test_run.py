from __future__ import annotations

import sys
import unittest

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

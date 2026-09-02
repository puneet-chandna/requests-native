from __future__ import annotations

import unittest

from benchmarks.run import percentile, split_work, validate_worker_result


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
            "implementation": "/tmp/requests/__init__.py",
        }
        with self.assertRaisesRegex(ValueError, "body bytes"):
            validate_worker_result(
                worker,
                requests=2,
                expected_bytes=4,
                expected_checksum=9,
                require_native=True,
            )


if __name__ == "__main__":
    unittest.main()

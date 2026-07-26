#!/usr/bin/env python3
"""Run the adapter/retry matrix with only pip's vendored urllib3 substituted."""

from __future__ import annotations

import os
import pathlib
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
VENDORED = pathlib.Path("/usr/lib/python3/dist-packages/pip/_vendor/urllib3")


def main() -> int:
    if not VENDORED.is_dir():
        print(f"missing vendored urllib3: {VENDORED}", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory(prefix="requests-urllib3-126-") as directory:
        isolated = pathlib.Path(directory)
        (isolated / "urllib3").symlink_to(VENDORED, target_is_directory=True)
        sys.path.insert(0, str(isolated))
        sys.path.insert(1, str(ROOT))

        import pytest
        import urllib3
        from urllib3.util.retry import RequestHistory, Retry

        import requests

        print(f"python={sys.executable}")
        print(f"isolation={isolated}")
        print(f"urllib3_version={urllib3.__version__}")
        print(f"urllib3_path={urllib3.__file__}")
        print(f"requests_path={requests.__file__}")
        print(f"extension_path={requests._requests_rust.__file__}")
        print(f"retry_module={Retry.__module__}")
        print(f"history_module={RequestHistory.__module__}")
        print(f"pytest_path={pytest.__file__}")
        print(f"pythonpath={os.environ.get('PYTHONPATH', '')}")
        result = pytest.main(
            [
                "-q",
                str(ROOT / "tests_differential/test_retries.py"),
                str(ROOT / "tests_differential/test_adapters.py"),
            ]
        )
        print(f"exit={result}")
        return int(result)


if __name__ == "__main__":
    raise SystemExit(main())

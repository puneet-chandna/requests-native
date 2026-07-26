#!/usr/bin/env python3
"""Run adapter/retry evidence with the official urllib3 1.26.20 wheel."""

from __future__ import annotations

import argparse
import hashlib
import os
import pathlib
import sys
import tempfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION = "1.26.20"
FILENAME = "urllib3-1.26.20-py2.py3-none-any.whl"
URL = (
    "https://files.pythonhosted.org/packages/33/cf/"
    "8435d5a7159e2a9c83a95896ed596f68cf798005fe107cc655b5c5c14704/" + FILENAME
)
SHA256 = "0ed14ccfbf1c30a9072c7ca157e4319b70d65f623e91e7b32fadb2853431016e"


def wheel_path(directory: pathlib.Path, supplied: pathlib.Path | None) -> pathlib.Path:
    if supplied is not None:
        return supplied.resolve()
    destination = directory / FILENAME
    with urllib.request.urlopen(URL) as response:
        destination.write_bytes(response.read())
    return destination


def verify_and_extract(wheel: pathlib.Path, isolated: pathlib.Path) -> str:
    digest = hashlib.sha256(wheel.read_bytes()).hexdigest()
    if digest != SHA256:
        raise ValueError(f"sha256 mismatch: expected {SHA256}, got {digest}")
    with zipfile.ZipFile(wheel) as archive:
        members = [
            member
            for member in archive.infolist()
            if member.filename.startswith("urllib3/") and not member.is_dir()
        ]
        if not members:
            raise ValueError("official wheel has no top-level urllib3 package")
        for member in members:
            archive.extract(member, isolated)
    return digest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wheel", type=pathlib.Path)
    arguments = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="requests-urllib3-126-") as directory:
        isolated = pathlib.Path(directory)
        try:
            wheel = wheel_path(isolated, arguments.wheel)
            digest = verify_and_extract(wheel, isolated)
        except (OSError, ValueError, zipfile.BadZipFile) as error:
            print(error, file=sys.stderr)
            return 2

        sys.path.insert(0, str(isolated))
        sys.path.insert(1, str(ROOT))

        import pytest
        import urllib3
        from urllib3.util.retry import RequestHistory, Retry

        import requests

        print(f"python={sys.executable}")
        print(f"isolation={isolated}")
        print(f"official_url={URL}")
        print(f"wheel_path={wheel}")
        print(f"wheel_sha256={digest}")
        print(f"urllib3_version={urllib3.__version__}")
        print(f"urllib3_path={urllib3.__file__}")
        print(f"requests_path={requests.__file__}")
        print(f"extension_path={requests._requests_rust.__file__}")
        print(f"retry_module={Retry.__module__}")
        print(f"history_module={RequestHistory.__module__}")
        print(f"pytest_path={pytest.__file__}")
        print(f"pythonpath={os.environ.get('PYTHONPATH', '')}")
        if urllib3.__version__ != VERSION:
            print(f"unexpected urllib3 version: {urllib3.__version__}", file=sys.stderr)
            return 2
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

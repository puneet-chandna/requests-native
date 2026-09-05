from __future__ import annotations

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def _assert_named_rust_contract(package: str, name: str) -> None:
    completed = subprocess.run(
        [
            "cargo",
            "test",
            "--offline",
            "--locked",
            "-p",
            package,
            name,
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    output = completed.stdout + completed.stderr
    assert completed.returncode == 0, output
    assert f"{name} ... ok" in output, output


def test_core_response_disposition_contract_is_compiled() -> None:
    _assert_named_rust_contract(
        "requests-native",
        "response_disposition_is_monotonic_and_exactly_once",
    )


def test_binding_response_payload_and_owner_contract_is_compiled() -> None:
    _assert_named_rust_contract(
        "requests-native-python",
        "response_payloads_and_origin_owner_are_explicit",
    )

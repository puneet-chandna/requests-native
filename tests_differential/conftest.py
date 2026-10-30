from __future__ import annotations

import os
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
from tests_differential.runner import (
    DEFAULT_ORACLE_ROOT,
    CaseRun,
    run_oracle_case,
    run_rewrite_case,
)


@pytest.fixture
def oracle_root() -> Path:
    root = Path(os.environ.get("REQUESTS_ORACLE_ROOT", DEFAULT_ORACLE_ROOT)).resolve()
    package = root / "src" / "requests" / "__init__.py"
    if not package.is_file():
        pytest.fail(f"configured oracle package does not exist: {package}")
    return root


@pytest.fixture
def run_differential_case() -> Callable[[dict[str, Any]], tuple[CaseRun, CaseRun]]:
    def run(case: dict[str, Any]) -> tuple[CaseRun, CaseRun]:
        return run_oracle_case(case), run_rewrite_case(case)

    return run


@pytest.fixture
def strict_admission(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Re-enable the exhaustive per-request native admission walk for one test.

    The default admission path verifies the exact built-in facade when it is
    registered and then re-checks per request only the cheap facts that decide
    native versus Python dispatch. Tests that pin the exhaustive contract ask
    for it explicitly instead of depending on it being the default; child
    processes inherit the same setting through the environment.
    """

    monkeypatch.setenv("REQUESTS_NATIVE_STRICT_ADMISSION", "1")
    try:
        from requests import _requests_rust
    except ImportError:
        yield
        return
    setter = getattr(_requests_rust, "_set_strict_admission_trial", None)
    reader = getattr(_requests_rust, "_strict_admission_trial", None)
    if setter is None:
        yield
        return
    previous = bool(reader()) if reader is not None else False
    setter(True)
    try:
        yield
    finally:
        setter(previous)

from __future__ import annotations

import os
from collections.abc import Callable
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

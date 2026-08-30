from __future__ import annotations

import argparse
import ast
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

BACKEND_NODE_IDS = [
    "tests_rust/test_backend_boundary.py",
    "tests_differential/test_public_types.py::test_task17_default_backend_remains_python_outside_explicit_trial",
    "tests_differential/test_import_api.py::test_task17_red_root_one_shot_uses_explicit_semantic_session_trial",
    "tests_differential/test_adapters.py::test_exact_admission_falls_back_before_native_pool_creation",
    "tests_differential/test_adapters.py::test_restored_instance_class_and_module_state_readmits_native_trial",
    "tests_differential/test_task17_supplemental.py::test_task17_adapter_registration_is_stable_until_close_and_readmission",
]


def static_check() -> None:
    facade = ROOT / "src" / "requests" / "_rust_public.py"
    source = facade.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(facade))
    trial = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_trial_enabled"
    )
    names = {node.id for node in ast.walk(trial) if isinstance(node, ast.Name)}
    if names - {"bool", "getattr", "_TRIAL_STATE"}:
        raise ValueError("trial activation must remain thread-local and explicit")
    if "os.environ" in source or "REQUESTS_RUST_BACKEND" in source:
        raise ValueError(
            "environment/global backend selector is forbidden before Task 20"
        )
    sessions = (ROOT / "src" / "requests" / "sessions.py").read_text(encoding="utf-8")
    if "def _rust_public_trial" not in sessions:
        raise ValueError("private explicit trial context is missing")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Prove the explicit Requests backend boundary"
    )
    parser.add_argument("--static-only", action="store_true")
    arguments = parser.parse_args()
    try:
        static_check()
    except (OSError, SyntaxError, StopIteration, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    if arguments.static_only:
        print("backend boundary static check passed")
        return 0
    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            *BACKEND_NODE_IDS,
        ],
        cwd=ROOT,
        check=False,
    )
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())

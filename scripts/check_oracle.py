from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10
    import tomli as tomllib

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tests_differential.runner import run_oracle_case  # noqa: E402


def resolve_oracle_root(
    lock: dict,
    environment: dict[str, str] = os.environ,
    *,
    root: Path = ROOT,
) -> Path:
    configured = Path(environment.get("REQUESTS_ORACLE_ROOT", lock["oracle_path"]))
    if not configured.is_absolute():
        configured = root / configured
    return configured.resolve()


def main() -> int:
    with (ROOT / "ORACLE.lock").open("rb") as stream:
        lock = tomllib.load(stream)
    oracle_root = resolve_oracle_root(lock)
    try:
        head = _git(oracle_root, "rev-parse", "HEAD")
        source_tree = _git(
            oracle_root,
            "rev-parse",
            f"{lock['frozen_source_commit']}^{{tree}}",
        )
        subprocess.run(
            [
                "git",
                "-C",
                str(oracle_root),
                "diff",
                "--exit-code",
                lock["frozen_source_commit"],
                "--",
                "src/requests",
                "tests",
                "pyproject.toml",
                "setup.py",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        dirty = _git(
            oracle_root,
            "status",
            "--porcelain",
            "--",
            "src/requests",
            "tests",
            "pyproject.toml",
            "setup.py",
        )
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"oracle git verification failed: {error}", file=sys.stderr)
        return 1
    if head != lock["documentation_commit"]:
        print(
            f"oracle HEAD differs from ORACLE.lock: {head}",
            file=sys.stderr,
        )
        return 1
    if source_tree != lock["frozen_source_tree"]:
        print(
            f"frozen source tree differs from ORACLE.lock: {source_tree}",
            file=sys.stderr,
        )
        return 1
    if dirty:
        print(f"oracle has uncommitted source changes:\n{dirty}", file=sys.stderr)
        return 1
    run = run_oracle_case(
        {
            "source": (
                "import requests\n"
                "side_effects.append({"
                "'file': requests.__file__, "
                "'version': requests.__version__"
                "})\n"
            )
        }
    )
    imported = run.observations["side_effects"][0]
    requests_file = Path(imported["file"]).resolve()
    if not requests_file.is_relative_to(oracle_root):
        print(
            f"oracle contamination: imported {requests_file} outside {oracle_root}",
            file=sys.stderr,
        )
        return 1
    print(
        json.dumps(
            {
                "oracle_root": str(oracle_root),
                "requests_file": str(requests_file),
                "version": imported["version"],
                "head": head,
                "frozen_source_commit": lock["frozen_source_commit"],
                "frozen_source_tree": source_tree,
            },
            sort_keys=True,
        )
    )
    return 0


def _git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


if __name__ == "__main__":
    raise SystemExit(main())

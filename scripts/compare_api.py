from __future__ import annotations

import csv
import importlib
import inspect
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tests_differential.runner import _child_environment  # noqa: E402

ORACLE_ROOT = ROOT.parent / "requests"
CHILD = "--child"
RESOLVABLE = re.compile(r"^[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*$")
PROBE_TIMEOUT_SECONDS = 30


def ledger_rows() -> list[dict[str, str]]:
    with (ROOT / "API_COMPATIBILITY.tsv").open(newline="", encoding="utf-8") as stream:
        return list(csv.DictReader(stream, delimiter="\t"))


def selected_rows() -> tuple[list[dict[str, str]], list[str]]:
    rows = ledger_rows()
    selected = [
        row
        for row in rows
        if row["module"] != "cross-cutting" and RESOLVABLE.fullmatch(row["symbol"])
    ]
    selected_keys = {(row["module"], row["symbol"]) for row in selected}
    skipped = [
        f"{row['module']}:{row['symbol']}"
        for row in rows
        if (row["module"], row["symbol"]) not in selected_keys
    ]
    return selected, skipped


def safe_value(value: Any) -> Any:
    if value is None or type(value) in {bool, int, float, str}:
        return value
    if type(value) in {list, tuple} and len(value) <= 100:
        return [safe_value(item) for item in value]
    if type(value) is dict and len(value) <= 100:
        return [[safe_value(key), safe_value(item)] for key, item in value.items()]
    value_type = type(value)
    return {
        "unsupported": [value_type.__module__, value_type.__qualname__],
    }


def record(row: dict[str, str]) -> dict[str, Any]:
    try:
        value: Any = importlib.import_module(row["module"])
        for part in row["symbol"].split("."):
            value = inspect.getattr_static(value, part)
    except BaseException as error:
        return {"error": [type(error).__module__, type(error).__qualname__, error.args]}
    try:
        signature = str(inspect.signature(value)) if callable(value) else None
    except (TypeError, ValueError):
        signature = None
    value_type = type(value)
    return {
        "type": [value_type.__module__, value_type.__qualname__],
        "defined": [
            getattr(value, "__module__", None),
            getattr(value, "__qualname__", None),
        ],
        "signature": signature,
        "value": safe_value(value),
    }


def child() -> int:
    rows = json.load(sys.stdin)
    json.dump([record(row) for row in rows], sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    return 0


def probe(package_root: Path, rows: list[dict[str, str]]) -> list[dict[str, Any]]:
    try:
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), CHILD],
            input=json.dumps(rows),
            text=True,
            capture_output=True,
            cwd=ROOT,
            env=_child_environment(package_root, "api-parity"),
            timeout=PROBE_TIMEOUT_SECONDS,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"API probe for {package_root} timed out after {PROBE_TIMEOUT_SECONDS}s"
        ) from error
    if completed.returncode:
        raise RuntimeError(completed.stderr.strip() or "API probe failed")
    return json.loads(completed.stdout)


def main() -> int:
    if CHILD in sys.argv:
        return child()
    rows, skipped_keys = selected_rows()
    try:
        oracle = probe(ORACLE_ROOT / "src", rows)
        rewrite = probe(ROOT / "src", rows)
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        print(error, file=sys.stderr)
        return 1
    mismatches = [
        (row, left, right)
        for row, left, right in zip(rows, oracle, rewrite, strict=True)
        if left != right
    ]
    for row, left, right in mismatches:
        print(
            f"{row['module']}:{row['symbol']}: oracle={left!r} rewrite={right!r}",
            file=sys.stderr,
        )
    print(
        f"compared={len(rows)} skipped_non_object_rows={len(skipped_keys)} "
        f"mismatches={len(mismatches)}"
    )
    print("skipped_non_object_keys=" + json.dumps(skipped_keys, separators=(",", ":")))
    return int(bool(mismatches))


if __name__ == "__main__":
    raise SystemExit(main())

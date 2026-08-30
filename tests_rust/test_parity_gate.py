from __future__ import annotations

import csv
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def run_script(name: str, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(ROOT / "scripts" / name), *arguments],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


def test_current_oracle_ledgers_api_and_boundary_are_valid() -> None:
    for name, arguments in [
        ("check_oracle.py", ()),
        ("check_ledgers.py", ()),
        ("compare_api.py", ()),
        ("verify_backend_boundary.py", ("--static-only",)),
    ]:
        completed = run_script(name, *arguments)
        assert completed.returncode == 0, (name, completed.stdout, completed.stderr)


def test_ledger_checker_rejects_duplicate_keys_and_unreviewed_closure(
    tmp_path: Path,
) -> None:
    api = tmp_path / "api.tsv"
    lifetime = tmp_path / "lifetimes.tsv"
    with (ROOT / "API_COMPATIBILITY.tsv").open(newline="", encoding="utf-8") as stream:
        api_rows = list(csv.reader(stream, delimiter="\t"))
    with (ROOT / "LIFETIMES.tsv").open(newline="", encoding="utf-8") as stream:
        lifetime_rows = list(csv.reader(stream, delimiter="\t"))
    api.write_text(
        "\n".join("\t".join(row) for row in [api_rows[0], api_rows[1], api_rows[1]])
        + "\n",
        encoding="utf-8",
    )
    lifetime.write_text(
        "\n".join("\t".join(row) for row in lifetime_rows[:2]) + "\n",
        encoding="utf-8",
    )
    duplicate = run_script(
        "check_ledgers.py", "--api", str(api), "--lifetimes", str(lifetime)
    )
    assert duplicate.returncode == 1
    assert "duplicate key" in duplicate.stderr

    api_rows[1][6] = "VERIFIED"
    api_rows[1][7] = "promise only"
    api.write_text(
        "\t".join(api_rows[0]) + "\n" + "\t".join(api_rows[1]) + "\n",
        encoding="utf-8",
    )
    unreviewed = run_script(
        "check_ledgers.py", "--api", str(api), "--lifetimes", str(lifetime)
    )
    assert unreviewed.returncode == 1
    assert "evidence missing" in unreviewed.stderr

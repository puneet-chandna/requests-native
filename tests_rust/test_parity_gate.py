from __future__ import annotations

import csv
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
EXPECTED_API_ROWS = 366
EXPECTED_LIFETIME_ROWS = 105


def run_script(name: str, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(ROOT / "scripts" / name), *arguments],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


def write_minimal_ledgers(
    tmp_path: Path, *, api_status: str, lifetime_status: str, future: bool = False
) -> tuple[Path, Path]:
    api = tmp_path / "api.tsv"
    lifetime = tmp_path / "lifetimes.tsv"
    with (ROOT / "API_COMPATIBILITY.tsv").open(newline="", encoding="utf-8") as stream:
        api_source = list(csv.reader(stream, delimiter="\t"))[:2]
    with (ROOT / "LIFETIMES.tsv").open(newline="", encoding="utf-8") as stream:
        lifetime_source = list(csv.reader(stream, delimiter="\t"))[:2]
    api_rows = [api_source[0]]
    for index in range(EXPECTED_API_ROWS):
        row = api_source[1].copy()
        row[0:3] = [f"test.module.{index}", f"symbol-{index}", "function"]
        row[6] = "VERIFIED"
        row[7] = (
            "oracle: source; test: tests/test_requests.py; review: accepted 4d01c2c"
        )
        api_rows.append(row)
    lifetime_rows = [lifetime_source[0]]
    for index in range(EXPECTED_LIFETIME_ROWS):
        row = lifetime_source[1].copy()
        row[0:3] = ["test.py", f"Owner{index}", f"field-{index}"]
        row[9] = "VERIFIED"
        row[8] = (
            "oracle: source; rust: binding; test: tests/test_requests.py; "
            "review: accepted 4d01c2c"
        )
        lifetime_rows.append(row)
    if future:
        api_rows[1][0:7] = [
            "cross-cutting",
            "default backend",
            "architecture",
            "built-in HTTPAdapter sends through Rust",
            "Do not route default network I/O through urllib3",
            "approved design",
            api_status,
        ]
    api_rows[1][6] = api_status
    api_rows[1][7] = (
        "future: Task 20 default switch"
        if future
        else ("oracle: source; test: tests/test_requests.py; review: accepted 4d01c2c")
    )
    lifetime_rows[1][9] = lifetime_status
    api.write_text(
        "\n".join("\t".join(row) for row in api_rows) + "\n", encoding="utf-8"
    )
    lifetime.write_text(
        "\n".join("\t".join(row) for row in lifetime_rows) + "\n",
        encoding="utf-8",
    )
    return api, lifetime


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


@pytest.mark.parametrize("status", ["NOT_PORTED", "IN_PROGRESS"])
def test_ledger_completion_rejects_unfinished_api_rows(
    tmp_path: Path, status: str
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status=status, lifetime_status="VERIFIED"
    )

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert f"unfinished API status {status!r}" in completed.stderr


@pytest.mark.parametrize(
    "status", ["PROPOSED", "IN_PROGRESS", "REVIEW_REQUIRED", "UNKNOWN"]
)
def test_ledger_completion_rejects_unfinished_lifetime_rows(
    tmp_path: Path, status: str
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status=status
    )

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert f"unfinished lifetime status {status!r}" in completed.stderr


@pytest.mark.parametrize("status", ["NOT_PORTED", "IN_PROGRESS"])
def test_task_20_candidate_default_backend_is_not_a_completion_exception(
    tmp_path: Path,
    status: str,
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path,
        api_status=status,
        lifetime_status="VERIFIED",
        future=True,
    )

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert f"unfinished API status {status!r}" in completed.stderr


def test_current_default_backend_row_records_local_candidate_and_remote_deferral() -> (
    None
):
    with (ROOT / "API_COMPATIBILITY.tsv").open(newline="", encoding="utf-8") as stream:
        rows = list(csv.DictReader(stream, delimiter="\t"))

    row = next(
        row
        for row in rows
        if (row["module"], row["symbol"], row["kind"])
        == ("cross-cutting", "default backend", "architecture")
    )
    assert row["port_status"] == "IN_PROGRESS"
    assert "tests_rust/test_default_backend.py" in row["evidence"]
    assert "Task 21 v1.0.0-beta gate" in row["evidence"]


def test_ledger_completion_rejects_verified_api_with_unfinished_evidence(
    tmp_path: Path,
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = "oracle: source; test: case; review: ready; public wiring pending"
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "contradictory completion evidence" in completed.stderr


def test_ledger_completion_rejects_verified_lifetime_with_unfinished_notes(
    tmp_path: Path,
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(lifetime.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][10] = "Public integration remains Task 17."
    lifetime.write_text(
        "\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8"
    )

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "contradictory completion evidence" in completed.stderr


def test_ledger_completion_rejects_verified_api_with_open_marker(
    tmp_path: Path,
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = "oracle: source; test: case; review: ready; open: manifest missing"
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "contradictory completion evidence" in completed.stderr


def test_ledger_completion_rejects_verified_numbered_future_task(
    tmp_path: Path,
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path,
        api_status="VERIFIED",
        lifetime_status="VERIFIED",
        future=True,
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = (
        "oracle: source; test: case; review: ready; future: Task 20 default switch"
    )
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "contradictory completion evidence" in completed.stderr


def test_task_20_exception_rejects_task_200_and_open_work(tmp_path: Path) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path,
        api_status="NOT_PORTED",
        lifetime_status="VERIFIED",
        future=True,
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = "future: Task 200; open: publish manifest missing"
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "unfinished API status 'NOT_PORTED'" in completed.stderr


def test_ledger_completion_rejects_verified_deferred_task(tmp_path: Path) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = (
        "oracle: source; test: tests/test_requests.py; review: ready; "
        "deferred to Task 17"
    )
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "contradictory completion evidence" in completed.stderr


@pytest.mark.parametrize("ledger", ["api", "lifetime"])
def test_ledger_completion_rejects_header_only_inventory(
    tmp_path: Path, ledger: str
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    path = api if ledger == "api" else lifetime
    path.write_text(path.read_text(encoding="utf-8").splitlines()[0] + "\n")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert f"empty {ledger} inventory" in completed.stderr


@pytest.mark.parametrize(
    "claim", ["approved architecture", "generated signature/call-error snapshot"]
)
def test_ledger_completion_rejects_placeholder_test_evidence(
    tmp_path: Path, claim: str
) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = (
        f"oracle: source; test: {claim}; closure-test: tests/test_requests.py; "
        "review: accepted 4d01c2c"
    )
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py",
        "--completion",
        "--api",
        str(api),
        "--lifetimes",
        str(lifetime),
    )

    assert completed.returncode == 1
    assert "placeholder test evidence" in completed.stderr


def test_ledger_checker_requires_primary_test_evidence_token(tmp_path: Path) -> None:
    api, lifetime = write_minimal_ledgers(
        tmp_path, api_status="VERIFIED", lifetime_status="VERIFIED"
    )
    rows = list(
        csv.reader(api.read_text(encoding="utf-8").splitlines(), delimiter="\t")
    )
    rows[1][7] = (
        "oracle: source; closure-test: tests/test_requests.py; review: accepted 4d01c2c"
    )
    api.write_text("\n".join("\t".join(row) for row in rows) + "\n", encoding="utf-8")

    completed = run_script(
        "check_ledgers.py", "--api", str(api), "--lifetimes", str(lifetime)
    )

    assert completed.returncode == 1
    assert "evidence missing test:" in completed.stderr

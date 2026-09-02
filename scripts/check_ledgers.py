from __future__ import annotations

import argparse
import csv
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXPECTED_API_ROWS = 366
EXPECTED_LIFETIME_ROWS = 105

API_HEADERS = (
    "module",
    "symbol",
    "kind",
    "signature_or_shape",
    "compatibility_requirement",
    "oracle_source",
    "port_status",
    "evidence",
)
LIFETIME_HEADERS = (
    "python_file",
    "owner",
    "field",
    "python_shape",
    "ownership",
    "proposed_rust",
    "threading",
    "drop_or_close",
    "evidence",
    "status",
    "reviewer_notes",
)
UNFINISHED_COMPLETION = re.compile(
    r"\b(?:pending|incomplete|not[_ -]?ported|future public|future work)\b"
    r"|\bremains?\s+(?:later|tasks?\s*\d+(?:/\d+)?)\b"
    r"|\bopen\s*:"
    r"|\b(?:future\s*:|deferred\s+to)\s*tasks?\s*\d+(?:/\d+)?\b",
    re.IGNORECASE,
)
EXECUTABLE_TEST_PATH = re.compile(
    r"(?:tests|tests_differential|tests_rust)/[A-Za-z0-9_./-]+\.py\b"
    r"|crates/[A-Za-z0-9_-]+/tests/[A-Za-z0-9_./-]+\.rs\b"
    r"|scripts/[A-Za-z0-9_./-]+\.py\b"
)
CARGO_TEST_COMMAND = re.compile(r"\bcargo(?:\s+\+\S+)?(?:\s+miri)?\s+test\b")
PLACEHOLDER_TEST_EVIDENCE = re.compile(
    r"(?:^|;\s*)test:\s*(?:approved architecture|generated\b|instrumented\b"
    r"|API snapshot\b|CLI snapshot\b|identity \+ signature snapshot\b"
    r"|unchanged\b)",
    re.IGNORECASE,
)
REVIEW_COMMIT = re.compile(r"(?:^|;\s*)review:[^;]*\b[0-9a-f]{7,40}\b")


def read_rows(path: Path, headers: tuple[str, ...]) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        if tuple(reader.fieldnames or ()) != headers:
            raise ValueError(f"{path}: expected headers {headers!r}")
        rows = list(reader)
    for number, row in enumerate(rows, 2):
        if None in row or any(not value.strip() for value in row.values()):
            raise ValueError(f"{path}:{number}: empty or extra field")
    return rows


def require_unique(
    path: Path, rows: list[dict[str, str]], keys: tuple[str, ...]
) -> None:
    seen: dict[tuple[str, ...], int] = {}
    for number, row in enumerate(rows, 2):
        key = tuple(row[name] for name in keys)
        if previous := seen.get(key):
            raise ValueError(
                f"{path}:{number}: duplicate key {key!r}; first seen on line {previous}"
            )
        seen[key] = number


def require_tokens(
    path: Path, number: int, evidence: str, tokens: tuple[str, ...]
) -> None:
    missing = [
        token
        for token in tokens
        if not re.search(rf"(?:^|;\s*){re.escape(token)}", evidence)
    ]
    if missing:
        raise ValueError(f"{path}:{number}: evidence missing {', '.join(missing)}")


def reject_unfinished_claims(
    path: Path, number: int, fields: tuple[tuple[str, str], ...]
) -> None:
    for name, value in fields:
        if match := UNFINISHED_COMPLETION.search(value):
            raise ValueError(
                f"{path}:{number}: contradictory completion evidence in {name}: "
                f"{match.group(0)!r}"
            )


def require_executable_evidence(path: Path, number: int, evidence: str) -> None:
    match = re.search(r"(?:^|;\s*)test:\s*([^;]*)", evidence)
    test_claim = match.group(1) if match else ""
    test_paths = EXECUTABLE_TEST_PATH.findall(test_claim)
    if any((ROOT / test_path).is_file() for test_path in test_paths):
        return
    if CARGO_TEST_COMMAND.search(test_claim):
        return
    raise ValueError(f"{path}:{number}: no executable test evidence")


def reject_placeholder_test_evidence(path: Path, number: int, evidence: str) -> None:
    if match := PLACEHOLDER_TEST_EVIDENCE.search(evidence):
        raise ValueError(
            f"{path}:{number}: placeholder test evidence: {match.group(0)!r}"
        )


def require_review_commit(path: Path, number: int, evidence: str) -> None:
    if not REVIEW_COMMIT.search(evidence):
        raise ValueError(f"{path}:{number}: review evidence missing exact commit")


def check_api(path: Path, *, completion: bool = False) -> Counter[str]:
    rows = read_rows(path, API_HEADERS)
    if completion and not rows:
        raise ValueError(f"{path}: empty api inventory")
    if completion and len(rows) != EXPECTED_API_ROWS:
        raise ValueError(
            f"{path}: expected {EXPECTED_API_ROWS} API rows, found {len(rows)}"
        )
    require_unique(path, rows, ("module", "symbol", "kind"))
    allowed = {"NOT_PORTED", "IN_PROGRESS", "VERIFIED", "INSPECTION_ONLY"}
    for number, row in enumerate(rows, 2):
        status = row["port_status"]
        if status not in allowed:
            raise ValueError(f"{path}:{number}: unknown port status {status!r}")
        if status == "VERIFIED":
            if completion:
                reject_unfinished_claims(path, number, (("evidence", row["evidence"]),))
            require_tokens(
                path, number, row["evidence"], ("oracle:", "test:", "review:")
            )
            reject_placeholder_test_evidence(path, number, row["evidence"])
            require_executable_evidence(path, number, row["evidence"])
            require_review_commit(path, number, row["evidence"])
        elif status == "INSPECTION_ONLY":
            require_tokens(path, number, row["evidence"], ("reason:", "review:"))
        if completion and status not in {"VERIFIED", "INSPECTION_ONLY"}:
            raise ValueError(f"{path}:{number}: unfinished API status {status!r}")
    return Counter(row["port_status"] for row in rows)


def check_lifetimes(path: Path, *, completion: bool = False) -> Counter[str]:
    rows = read_rows(path, LIFETIME_HEADERS)
    if completion and not rows:
        raise ValueError(f"{path}: empty lifetime inventory")
    if completion and len(rows) != EXPECTED_LIFETIME_ROWS:
        raise ValueError(
            f"{path}: expected {EXPECTED_LIFETIME_ROWS} lifetime rows, found {len(rows)}"
        )
    require_unique(path, rows, ("python_file", "owner", "field"))
    allowed = {"PROPOSED", "IN_PROGRESS", "VERIFIED", "REVIEW_REQUIRED", "UNKNOWN"}
    for number, row in enumerate(rows, 2):
        status = row["status"]
        if status not in allowed:
            raise ValueError(f"{path}:{number}: unknown lifetime status {status!r}")
        if status == "VERIFIED":
            if completion:
                reject_unfinished_claims(
                    path,
                    number,
                    tuple(
                        (name, row[name])
                        for name in (
                            "proposed_rust",
                            "threading",
                            "drop_or_close",
                            "evidence",
                            "reviewer_notes",
                        )
                    ),
                )
            require_tokens(
                path,
                number,
                row["evidence"],
                ("oracle:", "rust:", "test:", "review:"),
            )
            reject_placeholder_test_evidence(path, number, row["evidence"])
            require_executable_evidence(path, number, row["evidence"])
            require_review_commit(path, number, row["evidence"])
        elif completion:
            raise ValueError(f"{path}:{number}: unfinished lifetime status {status!r}")
    return Counter(row["status"] for row in rows)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate compatibility ledger structure"
    )
    parser.add_argument("--api", type=Path, default=ROOT / "API_COMPATIBILITY.tsv")
    parser.add_argument("--lifetimes", type=Path, default=ROOT / "LIFETIMES.tsv")
    parser.add_argument(
        "--completion",
        action="store_true",
        help="require every compatibility and lifetime row to be closed",
    )
    arguments = parser.parse_args()
    try:
        api = check_api(arguments.api, completion=arguments.completion)
        lifetimes = check_lifetimes(
            arguments.lifetimes, completion=arguments.completion
        )
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    print("api " + " ".join(f"{key}={api[key]}" for key in sorted(api)))
    print(
        "lifetimes " + " ".join(f"{key}={lifetimes[key]}" for key in sorted(lifetimes))
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

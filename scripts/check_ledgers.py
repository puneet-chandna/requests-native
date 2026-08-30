from __future__ import annotations

import argparse
import csv
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

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
    missing = [token for token in tokens if token not in evidence]
    if missing:
        raise ValueError(f"{path}:{number}: evidence missing {', '.join(missing)}")


def check_api(path: Path) -> Counter[str]:
    rows = read_rows(path, API_HEADERS)
    require_unique(path, rows, ("module", "symbol", "kind"))
    allowed = {"NOT_PORTED", "IN_PROGRESS", "VERIFIED", "INSPECTION_ONLY"}
    for number, row in enumerate(rows, 2):
        status = row["port_status"]
        if status not in allowed:
            raise ValueError(f"{path}:{number}: unknown port status {status!r}")
        if status == "VERIFIED":
            require_tokens(
                path, number, row["evidence"], ("oracle:", "test:", "review:")
            )
        elif status == "INSPECTION_ONLY":
            require_tokens(path, number, row["evidence"], ("reason:", "review:"))
    return Counter(row["port_status"] for row in rows)


def check_lifetimes(path: Path) -> Counter[str]:
    rows = read_rows(path, LIFETIME_HEADERS)
    require_unique(path, rows, ("python_file", "owner", "field"))
    allowed = {"PROPOSED", "IN_PROGRESS", "VERIFIED", "REVIEW_REQUIRED", "UNKNOWN"}
    for number, row in enumerate(rows, 2):
        status = row["status"]
        if status not in allowed:
            raise ValueError(f"{path}:{number}: unknown lifetime status {status!r}")
        if status == "VERIFIED":
            require_tokens(
                path,
                number,
                row["evidence"],
                ("oracle:", "rust:", "test:", "review:"),
            )
    return Counter(row["status"] for row in rows)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate compatibility ledger structure"
    )
    parser.add_argument("--api", type=Path, default=ROOT / "API_COMPATIBILITY.tsv")
    parser.add_argument("--lifetimes", type=Path, default=ROOT / "LIFETIMES.tsv")
    arguments = parser.parse_args()
    try:
        api = check_api(arguments.api)
        lifetimes = check_lifetimes(arguments.lifetimes)
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

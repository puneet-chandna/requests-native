#!/usr/bin/env python3
"""Generate the auditable third-party license corpus embedded in NOTICE."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import deque
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NOTICE = ROOT / "NOTICE"
WORKSPACE_PACKAGE = "requests-native-python"
LICENSE_PREFIXES = ("COPYING", "COPYRIGHT", "LICENCE", "LICENSE", "NOTICE", "UNLICENSE")
SHARED_LICENSES = {
    # These two crates are released from the same repository under BSD-3-Clause,
    # but alloc-stdlib's published crate omits the repository's LICENSE file.
    ("alloc-stdlib", "0.2.4"): ("alloc-no-stdlib", "2.0.4", "LICENSE"),
}
HEADER = """Requests
Copyright 2019 Kenneth Reitz

Requests Rust is an unofficial derivative that adds a native Rust transport while
preserving the Requests Python API. The following upstream-derived Python files
carry prominent per-file modification notices:

- src/requests/__init__.py
- src/requests/adapters.py
- src/requests/models.py
- src/requests/sessions.py

THIRD-PARTY DEPENDENCY LICENSE CORPUS

This deterministic inventory covers the transitive normal-dependency graph of the
native extension across supported Cargo target conditions. Build-only and
development-only dependencies are excluded. It records each dependency's declared
license expression and reproduces its packaged top-level license, notice,
copyright, or copying files. This is compliance evidence, not legal advice;
owner/legal approval is still required before redistribution.
"""


def cargo_metadata() -> dict:
    completed = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline", "--format-version=1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


def runtime_packages(metadata: dict) -> list[dict]:
    packages = {package["id"]: package for package in metadata["packages"]}
    roots = [
        package["id"]
        for package in metadata["packages"]
        if package["name"] == WORKSPACE_PACKAGE and package["source"] is None
    ]
    if len(roots) != 1:
        raise ValueError(f"expected one {WORKSPACE_PACKAGE!r} workspace package")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    queue = deque(roots)
    visited: set[str] = set()
    while queue:
        package_id = queue.popleft()
        if package_id in visited:
            continue
        visited.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                queue.append(dependency["pkg"])
    result = [
        packages[package_id] for package_id in visited if packages[package_id]["source"]
    ]
    return sorted(
        result,
        key=lambda package: (
            package["name"].casefold(),
            package["version"],
            package["source"],
        ),
    )


def license_files(
    package: dict, packages_by_key: dict[tuple[str, str], dict]
) -> list[tuple[str, Path]]:
    package_root = Path(package["manifest_path"]).parent
    paths = [
        path
        for path in package_root.iterdir()
        if path.is_file() and path.name.upper().startswith(LICENSE_PREFIXES)
    ]
    declared = package.get("license_file")
    if declared:
        paths.append(package_root / declared)
    unique = sorted(
        {path.resolve() for path in paths}, key=lambda path: path.name.casefold()
    )
    if not unique and (package["name"], package["version"]) in SHARED_LICENSES:
        sibling_name, sibling_version, filename = SHARED_LICENSES[
            package["name"], package["version"]
        ]
        sibling = packages_by_key[sibling_name, sibling_version]
        path = Path(sibling["manifest_path"]).parent / filename
        return [
            (
                f"{filename} (from {sibling_name} {sibling_version}; same repository and license)",
                path.resolve(),
            )
        ]
    if not unique:
        raise ValueError(
            f"{package['name']} {package['version']} has no packaged license/notice file"
        )
    return [(path.name, path) for path in unique]


def render_notice() -> bytes:
    sections = [HEADER.rstrip()]
    packages = runtime_packages(cargo_metadata())
    packages_by_key = {
        (package["name"], package["version"]): package for package in packages
    }
    for package in packages:
        expression = package.get("license") or "NOASSERTION"
        source = package["source"]
        sections.append(
            f"----- BEGIN {package['name']} {package['version']} -----\n"
            f"Declared license: {expression}\n"
            f"Cargo source: {source}"
        )
        for label, path in license_files(package, packages_by_key):
            content = "\n".join(
                line.rstrip()
                for line in path.read_text(encoding="utf-8")
                .replace("\r\n", "\n")
                .splitlines()
            ).rstrip()
            sections.append(f"--- {label} ---\n{content}")
        sections.append(f"----- END {package['name']} {package['version']} -----")
    return ("\n\n".join(sections) + "\n").encode()


def main() -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    arguments = parser.parse_args()
    expected = render_notice()
    if arguments.write:
        NOTICE.write_bytes(expected)
        return 0
    actual = NOTICE.read_bytes()
    if actual != expected:
        print(
            f"NOTICE is stale: expected {len(expected)} bytes, found {len(actual)} bytes",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

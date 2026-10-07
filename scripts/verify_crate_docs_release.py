"""Verify the 1.0.1 documentation package against published 1.0.0 bytes."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import tarfile
from pathlib import Path

import tomllib

BASELINE_SOURCE = "c1087413e54b7817a05d4080c3aaeca8e5c27db0"
BASELINE_SHA256 = "43170f424e6ec6c63939367d7680dde58a28a2fbf81f434daecf825bc78b1cbf"
LEGAL = ("LICENSE", "NOTICE", "AUTHORS.rst", "RUST_RUNTIME_NOTICES.html")


def archive_contents(path: Path, version: str) -> dict[str, bytes]:
    assert path.stat().st_size <= 10_000_000, "Archive exceeds the registry size limit"
    prefix = f"requests-native-{version}/"
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        assert 0 < len(members) <= 200, "Unexpected archive inventory size"
        assert len({member.name for member in members}) == len(members), (
            "Duplicate member"
        )
        assert all(
            member.isfile()
            and member.name.startswith(prefix)
            and ".." not in Path(member.name).parts
            and "\\" not in member.name
            and 0 <= member.size <= 3_000_000
            for member in members
        ), "Archive requires bounded regular files under its exact package root"
        assert sum(member.size for member in members) <= 20_000_000
        return {
            member.name.removeprefix(prefix): archive.extractfile(member).read()
            for member in members
        }


def normalize_lock(data: bytes, version: str, names: tuple[str, ...]) -> dict:
    locked = tomllib.loads(data.decode())
    for name in names:
        packages = [package for package in locked["package"] if package["name"] == name]
        assert len(packages) == 1 and packages[0]["version"] == version, name
        packages[0]["version"] = "version-only-equivalent"
    return locked


def compare_contents(
    before: dict[str, bytes], after: dict[str, bytes], source_sha: str
) -> None:
    assert set(before) == set(after), "Package inventory changed"
    allowed = {"README.md", "Cargo.toml", "Cargo.lock", ".cargo_vcs_info.json"}
    assert all(before[name] == after[name] for name in before if name not in allowed), (
        "Rust, fixtures, notices or original manifest changed"
    )
    assert all(name in after for name in LEGAL)
    old = tomllib.loads(before["Cargo.toml"].decode())
    new = tomllib.loads(after["Cargo.toml"].decode())
    assert old["package"].pop("version") == "1.0.0"
    assert new["package"].pop("version") == "1.0.1"
    assert old == new, "Effective package configuration changed beyond version"
    assert normalize_lock(
        before["Cargo.lock"], "1.0.0", ("requests-native",)
    ) == normalize_lock(after["Cargo.lock"], "1.0.1", ("requests-native",)), (
        "Locked dependencies changed"
    )
    for contents, sha in ((before, BASELINE_SOURCE), (after, source_sha)):
        vcs = json.loads(contents[".cargo_vcs_info.json"])
        assert vcs["git"]["sha1"] == sha and not vcs["git"].get("dirty", False)
        assert vcs["path_in_vcs"] == "crates/requests"


def verify_source(repo: Path, contents: dict[str, bytes], source_sha: str) -> None:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", *args], cwd=repo)

    def source(sha: str, name: str) -> bytes:
        return git("show", f"{sha}:{name}")

    assert git("rev-parse", "HEAD").decode().strip() == source_sha, "Wrong checkout"
    assert not git("status", "--porcelain", "--untracked-files=no"), "Dirty checkout"
    git("merge-base", "--is-ancestor", BASELINE_SOURCE, source_sha)
    old_workspace = tomllib.loads(source(BASELINE_SOURCE, "Cargo.toml").decode())
    new_workspace = tomllib.loads(source(source_sha, "Cargo.toml").decode())
    assert old_workspace["workspace"]["package"].pop("version") == "1.0.0"
    assert new_workspace["workspace"]["package"].pop("version") == "1.0.1"
    assert old_workspace == new_workspace, "Workspace changed beyond version"
    assert normalize_lock(
        source(BASELINE_SOURCE, "Cargo.lock"),
        "1.0.0",
        ("requests-native", "requests-native-python"),
    ) == normalize_lock(
        source(source_sha, "Cargo.lock"),
        "1.0.1",
        ("requests-native", "requests-native-python"),
    ), "Workspace dependencies changed"

    # The Python sources and binding remain unchanged; this does not qualify Python artifacts.
    paths = ("src/requests", "crates/requests-python", "pyproject.toml")
    old_paths = git("ls-tree", "-r", "--name-only", BASELINE_SOURCE, "--", *paths)
    new_paths = git("ls-tree", "-r", "--name-only", source_sha, "--", *paths)
    assert old_paths == new_paths, "Python or binding inventory changed"
    assert all(
        source(BASELINE_SOURCE, name) == source(source_sha, name)
        for name in new_paths.decode().splitlines()
    ), "Python or binding source changed"
    for name in ("crates/requests/Cargo.toml", "crates/requests-python/Cargo.toml"):
        package = tomllib.loads(source(source_sha, name).decode())["package"]
        assert package["version"] == {"workspace": True}
        assert package["publish"] is (name == "crates/requests/Cargo.toml")
    project = tomllib.loads(source(source_sha, "pyproject.toml").decode())
    assert "version" in project["project"]["dynamic"]
    assert (
        project["tool"]["maturin"]["manifest-path"]
        == "crates/requests-python/Cargo.toml"
    )
    assert b'__version__ = "2.34.2"' in source(
        source_sha, "src/requests/__version__.py"
    )

    tracked = (
        git("ls-tree", "-r", "--name-only", source_sha, "--", "crates/requests")
        .decode()
        .splitlines()
    )
    original = {
        name.removeprefix("crates/requests/"): source(source_sha, name)
        for name in tracked
        if name.startswith(("crates/requests/src/", "crates/requests/tests/fixtures/"))
        or Path(name).name in (*LEGAL, "README.md")
    }
    original["Cargo.toml.orig"] = source(source_sha, "crates/requests/Cargo.toml")
    assert set(contents) == set(original) | {
        "Cargo.toml",
        "Cargo.lock",
        ".cargo_vcs_info.json",
    }
    assert all(contents[name] == value for name, value in original.items()), (
        "Package differs from source"
    )
    assert all(
        contents[name] == source(source_sha, name) for name in (*LEGAL, "README.md")
    ), "Canonical README or notices differ"
    assert not any(name == "crates/requests/build.rs" for name in tracked)
    assert not any(
        b"CARGO_PKG_VERSION" in value
        for name, value in original.items()
        if name.endswith(".rs")
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--archive-sha256", required=True)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    args = parser.parse_args()
    assert re.fullmatch(r"[0-9a-f]{40}", args.source_sha)
    assert re.fullmatch(r"[0-9a-f]{64}", args.archive_sha256)
    assert hashlib.sha256(args.baseline.read_bytes()).hexdigest() == BASELINE_SHA256, (
        "Unapproved baseline"
    )
    assert (
        hashlib.sha256(args.candidate.read_bytes()).hexdigest() == args.archive_sha256
    ), "Unapproved archive"
    before = archive_contents(args.baseline, "1.0.0")
    after = archive_contents(args.candidate, "1.0.1")
    compare_contents(before, after, args.source_sha)
    verify_source(Path.cwd(), after, args.source_sha)
    manifest = {
        "source_sha": args.source_sha,
        "version": "1.0.1",
        "sha256": args.archive_sha256,
        "filename": "requests-native-1.0.1.crate",
        "rust_toolchain": "1.98.1",
        "runtime_equivalent_to": {
            "source_sha": BASELINE_SOURCE,
            "version": "1.0.0",
            "sha256": BASELINE_SHA256,
        },
        "qualification": "documentation/version-only; retained benchmark evidence measures c108 1.0.0",
        "python_publication": "deferred; no stable Python artifact qualification claimed",
    }
    args.manifest.write_text(json.dumps(manifest, sort_keys=True) + "\n")
    print("Verified exact 1.0.1 package and runtime equivalence to published 1.0.0.")


if __name__ == "__main__":
    main()

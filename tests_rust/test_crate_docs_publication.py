from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
import subprocess
import sys
import tarfile
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SHA = "a" * 40


def validator():
    path = ROOT / "scripts/verify_crate_docs_release.py"
    assert path.is_file(), "The shared local/CI archive validator is required"
    spec = importlib.util.spec_from_file_location("crate_docs_release", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def package(version: str) -> dict[str, bytes]:
    manifest = (
        '[package]\nname = "requests-native"\n'
        f'version = "{version}"\nrust-version = "1.98.1"\n'
        '[features]\ndefault = ["blocking"]\nblocking = []\n'
        '[dependencies]\nbytes = "1"\n'
    )
    lock = (
        'version = 4\n[[package]]\nname = "requests-native"\n'
        f'version = "{version}"\ndependencies = ["bytes"]\n'
        '[[package]]\nname = "bytes"\nversion = "1.12.0"\nchecksum = "abc"\n'
    )
    return {
        "Cargo.toml": manifest.encode(),
        "Cargo.toml.orig": b'[package]\nname = "requests-native"\nversion.workspace = true\n',
        "Cargo.lock": lock.encode(),
        ".cargo_vcs_info.json": json.dumps(
            {
                "git": {
                    "sha1": SHA if version == "1.0.1" else validator().BASELINE_SOURCE
                },
                "path_in_vcs": "crates/requests",
            }
        ).encode(),
        "README.md": f"Documentation for {version}\n".encode(),
        "src/lib.rs": b"pub fn unchanged() {}\n",
        "tests/fixtures/certificate.pem": b"public fixture\n",
        **{name: b"retained legal notice\n" for name in validator().LEGAL},
    }


@pytest.mark.parametrize(
    "change",
    [
        "valid",
        "rust",
        "fixture",
        "legal",
        "added",
        "missing",
        "dependency",
        "feature",
        "msrv",
        "orig",
        "lock-dependency",
        "lock-root",
        "version",
        "dirty-vcs",
        "wrong-vcs",
        "wrong-path",
        "baseline-vcs",
    ],
)
def test_docs_archive_rejects_changes_beyond_readme_version_and_clean_vcs(
    change: str,
) -> None:
    check = validator()
    before, after = package("1.0.0"), package("1.0.1")
    if change in {"rust", "fixture", "legal"}:
        name = {
            "rust": "src/lib.rs",
            "fixture": "tests/fixtures/certificate.pem",
            "legal": "NOTICE",
        }[change]
        after[name] += b"changed"
    elif change == "added":
        after["src/new.rs"] = b"new code"
    elif change == "missing":
        del after["src/lib.rs"]
    elif change in {"dependency", "feature", "msrv", "version"}:
        old, new = {
            "dependency": (b'bytes = "1"', b'bytes = "2"'),
            "feature": (b"blocking = []", b'blocking = ["new"]'),
            "msrv": (b"1.98.1", b"1.98.0"),
            "version": (b"1.0.1", b"1.0.2"),
        }[change]
        after["Cargo.toml"] = after["Cargo.toml"].replace(old, new)
    elif change == "orig":
        after["Cargo.toml.orig"] += b'build = "build.rs"\n'
    elif change == "lock-dependency":
        after["Cargo.lock"] = after["Cargo.lock"].replace(
            b'checksum = "abc"', b'checksum = "xyz"'
        )
    elif change == "lock-root":
        after["Cargo.lock"] = after["Cargo.lock"].replace(b"1.0.1", b"1.0.2")
    elif change in {"dirty-vcs", "wrong-vcs", "wrong-path", "baseline-vcs"}:
        target = before if change == "baseline-vcs" else after
        vcs = json.loads(target[".cargo_vcs_info.json"])
        if change == "dirty-vcs":
            vcs["git"]["dirty"] = True
        elif change == "wrong-path":
            vcs["path_in_vcs"] = "other"
        else:
            vcs["git"]["sha1"] = "b" * 40
        target[".cargo_vcs_info.json"] = json.dumps(vcs).encode()
    if change == "valid":
        check.compare_contents(before, after, SHA)
    else:
        with pytest.raises((AssertionError, ValueError)):
            check.compare_contents(before, after, SHA)


@pytest.mark.parametrize(
    "change", ["valid", "duplicate", "traversal", "symlink", "root", "oversized"]
)
def test_docs_archive_inventory_is_bounded_and_regular(
    tmp_path: Path, change: str
) -> None:
    check = validator()
    archive = tmp_path / "candidate.crate"
    with tarfile.open(archive, "w:gz") as data:
        name = "requests-native-1.0.1/README.md"
        if change == "traversal":
            name = "requests-native-1.0.1/../README.md"
        elif change == "root":
            name = "other/README.md"
        member = tarfile.TarInfo(name)
        body = b"readme\n"
        member.size = 3_000_001 if change == "oversized" else len(body)
        if change == "symlink":
            member.type = tarfile.SYMTYPE
            member.linkname = "/tmp/outside"
            member.size = 0
        data.addfile(
            member, io.BytesIO(body if change != "oversized" else b"x" * member.size)
        )
        if change == "duplicate":
            data.addfile(member, io.BytesIO(body))
    if change == "valid":
        assert check.archive_contents(archive, "1.0.1") == {"README.md": b"readme\n"}
    else:
        with pytest.raises((AssertionError, ValueError)):
            check.archive_contents(archive, "1.0.1")


def test_docs_upload_protects_exact_source_and_exposes_token_only_after_validation(
    tmp_path: Path,
) -> None:
    path = ROOT / ".github/workflows/publish-crate-docs.yml"
    assert path.is_file(), "The dedicated protected documentation publisher is required"
    workflow = yaml.safe_load(path.read_text())
    assert set(workflow[True]) == {"workflow_dispatch"}
    assert workflow["permissions"] == {"contents": "read"}
    assert workflow["concurrency"]["cancel-in-progress"] is False
    assert set(workflow["jobs"]) == {"publish"}
    job = workflow["jobs"]["publish"]
    assert job["runs-on"] == "namespace-profile-puneet-chandna"
    assert job["environment"]["name"] == "crates-io"
    steps = job["steps"]
    token = [i for i, step in enumerate(steps) if "CRATES_IO_API_TOKEN" in str(step)]
    assert len(token) == 1
    upload = steps[token[0]]
    assert upload["env"] == {
        "CARGO_REGISTRY_TOKEN": "${{ secrets.CRATES_IO_API_TOKEN }}"
    }
    assert upload["if"] == "${{ inputs.publish }}"
    assert "--locked --no-verify" in upload["run"] and "cargo test" not in upload["run"]
    assert any(
        "verify_crate_docs_release.py" in step.get("run", "")
        for step in steps[: token[0]]
    )
    assert all(
        step["with"]["persist-credentials"] is False
        for step in steps
        if "checkout@" in step.get("uses", "")
    )
    assert next(
        i for i, s in enumerate(steps) if "rustup toolchain install" in s.get("run", "")
    ) < next(i for i, s in enumerate(steps) if "cargo package" in s.get("run", ""))
    script = steps[0]["run"]
    tool = tmp_path / "gh"
    tool.write_text(
        "#!" + sys.executable + "\nimport json,os,sys\n"
        "print(json.dumps(json.loads(os.environ['FAKE_GITHUB'])[sys.argv[2]]))\n"
    )
    tool.chmod(0o755)
    repo = "/repos/puneet-chandna/requests-native"
    records = {
        repo + "/commits/main": {"sha": SHA},
        repo + "/environments/crates-io": {
            "can_admins_bypass": True,
            "protection_rules": [
                {
                    "type": "required_reviewers",
                    "reviewers": [{"type": "User", "reviewer": {"id": 121252460}}],
                }
            ],
            "deployment_branch_policy": {
                "protected_branches": False,
                "custom_branch_policies": True,
            },
        },
        repo + "/environments/crates-io/deployment-branch-policies": {
            "branch_policies": [{"name": "main", "type": "branch"}]
        },
    }
    original = {
        **os.environ,
        "PATH": str(tmp_path) + os.pathsep + os.environ["PATH"],
        "GITHUB_REPOSITORY": "puneet-chandna/requests-native",
        "GITHUB_REF": "refs/heads/main",
        "GITHUB_SHA": SHA,
        "SOURCE_SHA": SHA,
        "ARCHIVE_SHA256": "f" * 64,
    }
    for change in (
        "valid",
        "repo",
        "tag",
        "source",
        "sha-input",
        "hash-input",
        "stale-main",
        "reviewer",
        "branches",
    ):
        trial, data = dict(original), copy.deepcopy(records)
        if change == "repo":
            trial["GITHUB_REPOSITORY"] = "other/fork"
        elif change == "tag":
            trial["GITHUB_REF"] = "refs/tags/v1.0.1"
        elif change == "source":
            trial["SOURCE_SHA"] = "b" * 40
        elif change == "sha-input":
            trial["SOURCE_SHA"] = SHA + "; true"
        elif change == "hash-input":
            trial["ARCHIVE_SHA256"] = "f" * 64 + "; true"
        elif change == "stale-main":
            data[repo + "/commits/main"]["sha"] = "b" * 40
        elif change == "reviewer":
            data[repo + "/environments/crates-io"]["protection_rules"] = []
        elif change == "branches":
            data[repo + "/environments/crates-io/deployment-branch-policies"][
                "branch_policies"
            ][0]["name"] = "*"
        trial["FAKE_GITHUB"] = json.dumps(data)
        result = subprocess.run(
            ["bash", "-eo", "pipefail", "-c", script],
            env=trial,
            capture_output=True,
            text=True,
        )
        assert (result.returncode == 0) is (change == "valid"), (change, result.stderr)

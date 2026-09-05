#!/usr/bin/env python3
"""Build one path-sanitized release wheel with a deterministic Rust SBOM."""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

try:
    from . import generate_release_sbom
except ImportError:  # Executed directly from the scripts directory.
    import generate_release_sbom

ROOT = Path(__file__).resolve().parents[1]
SBOM_NAME = "requests-native-python.cyclonedx.json"
ENCODED_FLAG_SEPARATOR = "\x1f"
RUSTFLAG_NAMES = {
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
}
TARGET_RUSTFLAGS = re.compile(r"^CARGO_TARGET_.+_RUSTFLAGS$")
CARGO_CONFIG_RUSTFLAGS = re.compile(
    r"(?im)^\s*(?:(?:[A-Za-z0-9_'\".-]+)\.)*"
    r"(?:rustflags|RUSTFLAGS|CARGO_ENCODED_RUSTFLAGS|CARGO_BUILD_RUSTFLAGS|"
    r"CARGO_TARGET_[A-Z0-9_]+_RUSTFLAGS)\s*="
)


def reject_preexisting_rustflags(environment: dict[str, str]) -> None:
    forbidden = sorted(
        name
        for name in environment
        if name in RUSTFLAG_NAMES or TARGET_RUSTFLAGS.fullmatch(name)
    )
    if forbidden:
        raise ValueError(
            f"preexisting Rust flag environment is forbidden: {forbidden[0]}"
        )


def reject_repository_rustflags(repository: Path) -> None:
    cargo = repository / ".cargo"
    for name in ("config", "config.toml"):
        path = cargo / name
        if path.is_file() and CARGO_CONFIG_RUSTFLAGS.search(
            path.read_text(encoding="utf-8")
        ):
            raise ValueError("repository Cargo rustflags configuration is forbidden")


def path_spellings(value: str | os.PathLike[str]) -> tuple[str, ...]:
    raw = os.fspath(value)
    spellings = {raw, raw.replace("\\", "/")}
    drive = re.match(r"^([A-Za-z]):[\\/](.*)$", raw)
    if drive:
        tail = drive.group(2).replace("\\", "/")
        native_tail = tail.replace("/", "\\")
        for letter in {drive.group(1).lower(), drive.group(1).upper()}:
            native = f"{letter}:\\{native_tail}"
            slash = f"{letter}:/{tail}"
            spellings.update(
                {
                    native,
                    slash,
                    "\\\\?\\" + native,
                    "//?/" + slash,
                }
            )
    if raw.startswith("\\\\"):
        tail = raw.lstrip("\\").replace("\\", "/")
        spellings.update({"//" + tail, "\\\\?\\UNC\\" + tail.replace("/", "\\")})
    return tuple(
        sorted((item for item in spellings if item), key=lambda item: (len(item), item))
    )


def encoded_path_needles(spelling: str) -> tuple[bytes, ...]:
    return tuple(dict.fromkeys((spelling.encode("utf-8"), spelling.encode("utf-16le"))))


def _absolute_variants(path: Path, repository: Path) -> set[Path]:
    expanded = path.expanduser()
    if not expanded.is_absolute():
        expanded = repository / expanded
    return {expanded.absolute(), expanded.resolve(strict=False)}


def is_generic_temp_namespace(value: str | os.PathLike[str]) -> bool:
    if os.name == "nt":
        return False
    return os.path.realpath(os.fspath(value)) in {"/tmp", "/var/tmp", "/usr/tmp"}


def computed_remap_rules(
    *,
    repository: Path,
    home: Path,
    sysroot: Path,
    staging: Path,
    environment: dict[str, str],
) -> list[tuple[str, str]]:
    repository = repository.resolve(strict=False)
    cargo_home = Path(environment.get("CARGO_HOME", os.fspath(home / ".cargo")))
    # The staging directory is the exact temporary build root.  The platform's
    # generic temp root is intentionally not a path identity: compiled runtime
    # behavior can legitimately contain strings such as ``file:///tmp/...``.
    roots: list[tuple[Path, str]] = [
        (home, "home"),
        (cargo_home, "cargo-home"),
        (sysroot, "rust-sysroot"),
        (repository / "target", "target"),
        (staging, "staging"),
        (staging / "target", "target"),
        (repository, "source"),
    ]
    for name, label in (
        ("TMPDIR", "temp"),
        ("TEMP", "temp"),
        ("TMP", "temp"),
        ("RUNNER_TEMP", "runner-temp"),
        ("RUNNER_TOOL_CACHE", "runner-tools"),
        ("GITHUB_WORKSPACE", "source"),
        ("CARGO_TARGET_DIR", "target"),
    ):
        if environment.get(name) and not (
            name in {"TMPDIR", "TEMP", "TMP"}
            and is_generic_temp_namespace(environment[name])
        ):
            roots.append((Path(environment[name]), label))

    by_source: dict[str, str] = {}
    for path, label in roots:
        for variant in _absolute_variants(path, repository):
            for spelling in path_spellings(variant):
                by_source[spelling] = f"requests-native-build/{label}"
    return sorted(by_source.items(), key=lambda item: (len(item[0]), item[0]))


def _record_digest(content: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(content).digest())
    return "sha256=" + digest.rstrip(b"=").decode("ascii")


def validate_record(archive: zipfile.ZipFile, record_name: str) -> None:
    files = {
        member.filename: archive.read(member)
        for member in archive.infolist()
        if not member.is_dir()
    }
    try:
        rows = list(csv.reader(io.StringIO(files[record_name].decode("utf-8"))))
    except (KeyError, UnicodeDecodeError, csv.Error) as error:
        raise ValueError("wheel RECORD is missing or invalid") from error
    if any(len(row) != 3 for row in rows):
        raise ValueError("wheel RECORD row has the wrong shape")
    records = {row[0]: (row[1], row[2]) for row in rows}
    if len(records) != len(rows) or set(records) != set(files):
        raise ValueError("wheel RECORD inventory mismatch")
    for name, content in files.items():
        digest, size = records[name]
        if name == record_name:
            if digest or size:
                raise ValueError("wheel RECORD must not hash itself")
        elif digest != _record_digest(content) or size != str(len(content)):
            raise ValueError(f"wheel RECORD hash or size mismatch: {name}")


def validate_release_wheel(
    wheel: Path,
    *,
    expected_sbom: bytes,
    forbidden_paths: tuple[tuple[str, str], ...],
) -> None:
    forbidden = tuple(
        (needle, label)
        for spelling, label in forbidden_paths
        if spelling
        for needle in encoded_path_needles(spelling)
    )
    with zipfile.ZipFile(wheel) as archive:
        file_names = [
            member.filename for member in archive.infolist() if not member.is_dir()
        ]
        for name in file_names:
            content = archive.read(name)
            match = next(
                (label for value, label in forbidden if value in content), None
            )
            if match is not None:
                raise ValueError(
                    f"wheel member contains a private build path ({match}): {name}"
                )
        sboms = [name for name in file_names if name.endswith(f"/sboms/{SBOM_NAME}")]
        if len(sboms) != 1 or archive.read(sboms[0]) != expected_sbom:
            raise ValueError("wheel does not contain the deterministic release SBOM")
        generate_release_sbom.validate_sbom(json.loads(archive.read(sboms[0])))
        records = [name for name in file_names if name.endswith(".dist-info/RECORD")]
        if len(records) != 1:
            raise ValueError("wheel must contain exactly one RECORD")
        validate_record(archive, records[0])


def _rustc_sysroot(environment: dict[str, str]) -> Path:
    completed = subprocess.run(
        ["rustc", "--print", "sysroot"],
        cwd=ROOT,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    )
    return Path(completed.stdout.strip())


def _validate_epoch(environment: dict[str, str]) -> None:
    value = environment.get("SOURCE_DATE_EPOCH")
    if value is None or not value.isascii() or not value.isdigit():
        raise ValueError("SOURCE_DATE_EPOCH must be set to a non-negative integer")


def build_release_wheel(
    *,
    interpreter: str,
    output: Path,
    manylinux: str | None,
    offline: bool,
) -> Path:
    original_environment = dict(os.environ)
    reject_preexisting_rustflags(original_environment)
    reject_repository_rustflags(ROOT)
    _validate_epoch(original_environment)
    sysroot = _rustc_sysroot(original_environment)

    with tempfile.TemporaryDirectory(prefix="requests-native-release-") as temporary:
        staging = Path(temporary)
        sbom_path = staging / SBOM_NAME
        metadata = generate_release_sbom.cargo_metadata(
            root=ROOT, environment=original_environment, offline=offline
        )
        expected_sbom = generate_release_sbom.render_sbom(metadata)
        sbom_path.write_bytes(expected_sbom)

        rules = computed_remap_rules(
            repository=ROOT,
            home=Path.home(),
            sysroot=sysroot,
            staging=staging,
            environment=original_environment,
        )
        flags = [f"--remap-path-prefix={source}={target}" for source, target in rules]
        environment = dict(original_environment)
        environment["CARGO_ENCODED_RUSTFLAGS"] = ENCODED_FLAG_SEPARATOR.join(flags)
        environment["CARGO_TARGET_DIR"] = os.fspath(staging / "target")
        subprocess_temp = staging / "temp"
        subprocess_temp.mkdir()
        for name in ("TMPDIR", "TEMP", "TMP"):
            environment[name] = os.fspath(subprocess_temp)

        wheelhouse = staging / "wheelhouse"
        wheelhouse.mkdir()
        command = [
            sys.executable,
            "-m",
            "maturin",
            "build",
            "--release",
            "--strip",
            "--locked",
            "--interpreter",
            interpreter,
            "--out",
            os.fspath(wheelhouse),
            "--sbom-include",
            os.fspath(sbom_path),
        ]
        if manylinux is not None:
            command.extend(["--manylinux", manylinux])
        if offline:
            command.append("--offline")
        subprocess.run(command, cwd=ROOT, env=environment, check=True)
        wheels = sorted(wheelhouse.glob("*.whl"))
        if len(wheels) != 1:
            raise ValueError(
                f"release build produced {len(wheels)} wheels instead of one"
            )
        validate_release_wheel(
            wheels[0], expected_sbom=expected_sbom, forbidden_paths=tuple(rules)
        )

        output = output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        destination = output / wheels[0].name
        if destination.exists():
            raise FileExistsError(
                f"refusing to overwrite release wheel: {destination.name}"
            )
        shutil.copy2(wheels[0], destination)
        return destination


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--interpreter", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--manylinux")
    parser.add_argument("--offline", action="store_true")
    arguments = parser.parse_args()
    wheel = build_release_wheel(
        interpreter=arguments.interpreter,
        output=arguments.out,
        manylinux=arguments.manylinux,
        offline=arguments.offline,
    )
    print(wheel)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

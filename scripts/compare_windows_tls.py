"""One uninstrumented, bounded Windows source/profile comparison; no publication."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

REVISIONS = {
    "old": "275ad59820691ada9824472f5ebe75bfeb90ad41",
    "current": "437e59a054399192c345d3dd082572e92f598b9b",
}
CELLS = [(source, profile) for source in REVISIONS for profile in ("debug", "release")]
NODES = (
    "tests/test_requests.py::TestRequests::test_pyopenssl_redirect",
    "tests/test_requests.py::TestRequests::test_auth_is_stripped_on_http_downgrade",
)
KEEP = (
    "COMSPEC",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "WINDIR",
)
PROVENANCE = r"""
import hashlib, importlib.metadata as m, json, sys, sysconfig
from pathlib import Path
import requests
import requests._requests_rust as native
name, backend, version, proof_path, expected_hash = sys.argv[1:]
platlib = Path(sysconfig.get_path('platlib')).resolve()
assert Path(requests.__file__).resolve().is_relative_to(platlib)
assert Path(native.__file__).resolve().is_relative_to(platlib)
assert native.backend_name() == backend
assert requests.__version__ == '2.34.2'
assert m.version(name) == version
assert (m.distribution(name).locate_file('requests') / '__init__.py').resolve() == Path(requests.__file__).resolve()
with Path(native.__file__).open('rb') as stream:
    extension_hash = hashlib.file_digest(stream, 'sha256').hexdigest()
assert extension_hash == expected_hash, (extension_hash, expected_hash)
proof = dict(python=sys.version, package=requests.__file__, extension=native.__file__, extension_sha256=extension_hash, backend=backend, distribution=name, version=version, dependencies=sorted((d.metadata['Name'], d.version) for d in m.distributions()))
Path(proof_path).write_text(json.dumps(proof, indent=2) + '\n', encoding='utf-8')
print(json.dumps(proof, indent=2))
"""


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def wheel_extension_hash(wheel):
    with zipfile.ZipFile(wheel) as archive:
        extensions = [
            name
            for name in archive.namelist()
            if name.startswith("requests/_requests_rust") and name.endswith(".pyd")
        ]
        assert len(extensions) == 1, extensions
        with archive.open(extensions[0]) as stream:
            return hashlib.file_digest(stream, "sha256").hexdigest()


def external_dependencies(proof):
    return tuple(
        sorted(
            (name.lower().replace("_", "-"), version)
            for name, version in proof["dependencies"]
            if name.lower().replace("_", "-") not in {"requests", "requests-native"}
        )
    )


def command_once(command, cwd, env, log_path, timeout):
    """Record failure without retrying; terminate timed-out Windows descendants."""
    started = time.monotonic()
    record = {"command": command, "cwd": str(cwd), "timeout_seconds": timeout}
    with log_path.open("w", encoding="utf-8") as log:
        log.write(json.dumps(record) + "\n")
        log.flush()
        process = subprocess.Popen(
            command, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT
        )
        try:
            record["exit_code"] = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill", "/F", "/T", "/PID", str(process.pid)],
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=15,
                    check=False,
                )
            process.kill()
            process.wait(timeout=15)
            record.update(exit_code=124, timed_out=True)
    record["seconds"] = time.monotonic() - started
    write_json(log_path.with_suffix(".json"), record)
    return record


def build_command(python, output, profile):
    command = [
        python,
        "-m",
        "maturin",
        "build",
        "--locked",
        "-vv",
        "--interpreter",
        python,
        "--out",
        str(output),
    ]
    if profile == "release":
        command.append("--release")
    return command


def test_commands(python, source, proof_path, expected_hash):
    identity = (
        ("requests", "requests-rust", "2.34.2")
        if source == "old"
        else ("requests-native", "requests-native", "1.0.0b1")
    )
    return [
        (
            "provenance",
            [python, "-I", "-c", PROVENANCE, *identity, str(proof_path), expected_hash],
            30,
        ),
        (
            "admission",
            [
                python,
                "-m",
                "pytest",
                "-q",
                "tests_rust/test_backend_boundary.py::test_urllib3_2_https_remains_native_eligible",
            ],
            60,
        ),
        ("redirect", [python, "-m", "pytest", "-q", NODES[0]], 60),
        ("downgrade", [python, "-m", "pytest", "-q", NODES[1]], 60),
        ("full-order", [python, "-m", "pytest", "-q", "tests"], 300),
    ]


def stage_suite(source, suite):
    suite.mkdir(parents=True)
    for part in ("tests", "tests_differential"):
        shutil.copytree(
            source / part,
            suite / part,
            ignore=shutil.ignore_patterns("__pycache__"),
        )
    (suite / "tests_rust").mkdir()
    shutil.copyfile(
        source / "tests_rust/test_backend_boundary.py",
        suite / "tests_rust/test_backend_boundary.py",
    )
    shutil.copyfile(source / "requirements-dev.txt", suite / "requirements-dev.txt")
    for relative in (
        "tests/certs/valid/ca/ca.crt",
        "tests/certs/mtls/client/ca/ca.crt",
    ):
        assert (suite / relative).read_bytes() == (source / relative).read_bytes()


def compare(output):
    assert sys.platform == "win32" and sys.version_info[:3] == (3, 12, 10)
    assert output.resolve().is_relative_to(Path(os.environ["RUNNER_TEMP"]).resolve())
    output.mkdir(parents=True, exist_ok=True)
    evidence = output / "evidence"
    evidence.mkdir(exist_ok=True)
    checkout = Path(__file__).resolve().parents[1]
    pins = Path(__file__).with_name("windows-tls-dependencies.txt")
    build_env = dict(os.environ)
    for key in (
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "PYTHONPATH",
        "PYO3_USE_ABI3_FORWARD_COMPATIBILITY",
    ):
        build_env.pop(key, None)
    build_env.update(
        CARGO_TARGET_DIR=str(output / "target"),
        RUSTUP_TOOLCHAIN="1.98.0",
        PYTHONDONTWRITEBYTECODE="1",
        CARGO_TERM_COLOR="never",
    )
    test_env = {key: os.environ[key] for key in KEEP if key in os.environ}
    test_env.update(PYTHONDONTWRITEBYTECODE="1", PYTHONNOUSERSITE="1")
    write_json(
        evidence / "environment.json",
        dict(
            python=sys.version,
            machine=os.environ.get("PROCESSOR_IDENTIFIER"),
            image_os=os.environ.get("ImageOS"),
            image_version=os.environ.get("ImageVersion"),
            runner_arch=os.environ.get("RUNNER_ARCH"),
            build={
                key: build_env.get(key)
                for key in ("CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN", "CARGO_TERM_COLOR")
            },
            test=test_env,
            dependencies=pins.read_text().splitlines(),
            dependency_pins_sha256=sha256(pins),
            order=CELLS,
        ),
    )
    results = {}
    # Leave several minutes of the 40-minute job for setup and artifact upload.
    deadline = time.monotonic() + 34 * 60

    def run(label, command, cwd=checkout, env=build_env, timeout=900, required=True):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(
                "comparison time budget exhausted; results are incomplete"
            )
        record = command_once(
            command, cwd, env, evidence / f"{label}.log", min(timeout, remaining)
        )
        results[label] = record
        write_json(evidence / "results.json", results)
        print(label, record["exit_code"], round(record["seconds"], 2), flush=True)
        if record.get("timed_out") and remaining < timeout:
            raise RuntimeError(
                "comparison time budget exhausted; results are incomplete"
            )
        if required and record["exit_code"]:
            raise RuntimeError(f"{label} failed; comparison is incomplete")
        return record

    try:
        for tool in ("rustc", "cargo"):
            run(tool, [tool, "--version", "--verbose"], timeout=30)
        run("maturin", [sys.executable, "-m", "maturin", "--version"], timeout=30)
        manifests = {}
        for source, revision in REVISIONS.items():
            source_dir = output / source
            source_dir.mkdir()
            archive = output / f"{source}.tar"
            run(
                f"{source}-archive",
                ["git", "archive", "--format=tar", f"--output={archive}", revision],
                timeout=60,
            )
            run(
                f"{source}-extract",
                ["tar", "-xf", str(archive), "-C", str(source_dir)],
                timeout=60,
            )
            manifests[source] = {
                "revision": revision,
                "archive_sha256": sha256(archive),
            }
            for part in (
                "crates/requests/src",
                "crates/requests-python/src",
                "src/requests",
                "tests",
                "Cargo.lock",
            ):
                manifests[source][part] = subprocess.check_output(
                    ["git", "rev-parse", f"{revision}:{part}"], cwd=checkout, text=True
                ).strip()
        assert (
            manifests["old"]["crates/requests/src"]
            == "6348fb64dfb329f7c591b3cff0e9302a737b2718"
        )
        assert (
            manifests["old"]["crates/requests-python/src"]
            == "264118b5c306bb059810866f87cd358e418be050"
        )
        assert (
            manifests["old"]["src/requests"]
            == "be22c0760ceffdae3f4f620336e20e30245da9f0"
        )
        write_json(evidence / "sources.json", manifests)
        # Use the ordinary checkout's Windows symlinks, as the installed suite does.
        # The archive-derived certificate directory link was unreadable on Windows.
        suite_inputs = [
            "tests",
            "tests_differential",
            "tests_rust/test_backend_boundary.py",
            "requirements-dev.txt",
        ]
        untracked = subprocess.check_output(
            ["git", "ls-files", "--others", "--", *suite_inputs],
            cwd=checkout,
            timeout=30,
            text=True,
        )
        assert not untracked.strip(), f"untracked suite inputs: {untracked}"
        run(
            "suite-source-check",
            [
                "git",
                "diff",
                "--exit-code",
                REVISIONS["current"],
                "--",
                *suite_inputs,
            ],
            timeout=30,
        )
        for source, profile in CELLS:
            stage_suite(checkout, output / f"{source}-{profile}" / "outside")
        write_json(
            evidence / "suite-staging.json",
            {
                "source_revision": REVISIONS["current"],
                "source": str(checkout),
                "cells": [f"{source}-{profile}" for source, profile in CELLS],
                "completed_before_builds": True,
            },
        )

        wheelhouse = output / "dependencies"
        run(
            "download-dependencies",
            [
                sys.executable,
                "-m",
                "pip",
                "download",
                "--disable-pip-version-check",
                "--retries",
                "0",
                "--timeout",
                "30",
                "--only-binary=:all:",
                "--no-deps",
                "-r",
                str(pins),
                "--dest",
                str(wheelhouse),
            ],
            timeout=180,
        )
        write_json(
            evidence / "dependency-wheels.json",
            {wheel.name: sha256(wheel) for wheel in wheelhouse.glob("*.whl")},
        )
        wheels = {}
        extension_hashes = {}
        for source, profile in CELLS:
            name = f"{source}-{profile}"
            cell = output / name
            # The suite preflight already created this cell.
            run(
                f"{name}-build",
                build_command(sys.executable, cell, profile),
                cwd=output / source,
            )
            built = list(cell.glob("*.whl"))
            assert len(built) == 1, (name, built)
            wheels[name] = built[0]
            extension_hashes[name] = wheel_extension_hash(built[0])
            write_json(
                evidence / "built-wheels.json",
                {
                    key: {
                        "file": wheel.name,
                        "sha256": sha256(wheel),
                        "extension_sha256": extension_hashes[key],
                    }
                    for key, wheel in wheels.items()
                },
            )

        # Finish every build and environment install before any test child starts.
        for source, profile in CELLS:
            name = f"{source}-{profile}"
            cell = output / name
            environment = cell / "venv"
            run(
                f"{name}-venv",
                [sys.executable, "-m", "venv", str(environment)],
                timeout=60,
            )
            python = str(environment / "Scripts" / "python.exe")
            run(
                f"{name}-install",
                [
                    python,
                    "-m",
                    "pip",
                    "install",
                    "--disable-pip-version-check",
                    "--no-index",
                    "--no-deps",
                    "--find-links",
                    str(wheelhouse),
                    "-r",
                    str(pins),
                    str(wheels[name]),
                ],
                timeout=120,
            )
            run(f"{name}-pip-check", [python, "-m", "pip", "check"], timeout=30)

        valid_cells = []
        dependency_inventories = {}
        for source, profile in CELLS:
            name = f"{source}-{profile}"
            cell = output / name
            python = str(cell / "venv/Scripts/python.exe")
            proof_path = evidence / f"{name}-installed.json"
            valid = True
            for label, command, timeout in test_commands(
                python, source, proof_path, extension_hashes[name]
            ):
                record = run(
                    f"{name}-{label}",
                    command,
                    cwd=cell / "outside",
                    env=test_env,
                    timeout=timeout,
                    required=False,
                )
                if label in ("provenance", "admission") and record["exit_code"]:
                    valid = False
                if label == "provenance" and record["exit_code"] == 0:
                    dependency_inventories[name] = external_dependencies(
                        json.loads(proof_path.read_text(encoding="utf-8"))
                    )
            if valid:
                valid_cells.append(name)
        write_json(evidence / "external-dependencies.json", dependency_inventories)
        assert len(set(dependency_inventories.values())) <= 1, (
            "external dependencies differ across installed cells"
        )
        failed = [label for label, record in results.items() if record["exit_code"]]
        write_json(
            evidence / "summary.json",
            dict(
                complete=True,
                native_valid_cells=valid_cells,
                failures=failed,
                passed=not failed,
                instrumentation=False,
                explicit_maturin_strip=False,
                limitation="Cargo release defaults include debuginfo stripping. This compares source/build profiles; it does not prove a low-level cause or a repair.",
            ),
        )
        return int(bool(failed))
    except Exception as error:
        write_json(
            evidence / "summary.json",
            dict(
                complete=False,
                error=f"{type(error).__name__}: {error}",
                completed_commands=list(results),
            ),
        )
        raise


def self_check():
    assert build_command("python", Path("wheelhouse"), "release") == build_command(
        "python", Path("wheelhouse"), "debug"
    ) + ["--release"]
    assert "--strip" not in build_command("python", Path("wheelhouse"), "release")
    assert len(CELLS) == 4 and len(set(CELLS)) == 4
    commands = test_commands("python", "old", Path("proof.json"), "a" * 64)
    assert commands[-1][1][-1] == "tests" and commands[-1][2] == 300
    assert [item[2] for item in commands[2:4]] == [60, 60]
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        suite = root / "outside"
        checkout = Path(__file__).resolve().parents[1]
        stage_suite(checkout, suite)
        for relative in (
            "tests/certs/valid/ca/ca.crt",
            "tests/certs/mtls/client/ca/ca.crt",
        ):
            assert (suite / relative).read_bytes() == (checkout / relative).read_bytes()
            assert not (suite / relative).parent.is_symlink()
        wheel = root / "test.whl"
        with zipfile.ZipFile(wheel, "w") as archive:
            archive.writestr(
                "requests/_requests_rust.cp312-win_amd64.pyd", b"extension"
            )
        assert wheel_extension_hash(wheel) == hashlib.sha256(b"extension").hexdigest()
        assert external_dependencies(
            {"dependencies": [("requests", "2.34.2"), ("urllib3", "2.7.0")]}
        ) == external_dependencies(
            {"dependencies": [("requests_native", "1.0.0b1"), ("urllib3", "2.7.0")]}
        )
        failure = command_once(
            [sys.executable, "-c", "raise SystemExit(7)"],
            root,
            dict(os.environ),
            root / "failure.log",
            5,
        )
        success = command_once(
            [sys.executable, "-c", "print('next cell survives')"],
            root,
            dict(os.environ),
            root / "success.log",
            5,
        )
        timeout = command_once(
            [sys.executable, "-c", "import time; time.sleep(10)"],
            root,
            dict(os.environ),
            root / "timeout.log",
            0.1,
        )
        assert failure["exit_code"] == 7 and success["exit_code"] == 0
        assert timeout["exit_code"] == 124 and timeout["timed_out"]
        assert json.loads((root / "failure.json").read_text())["exit_code"] == 7
    print("self-check passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-check", action="store_true")
    arguments = parser.parse_args()
    if arguments.self_check:
        self_check()
    elif arguments.output:
        sys.exit(compare(arguments.output.resolve()))
    else:
        parser.error("--output or --self-check is required")

from __future__ import annotations

from pathlib import Path

import yaml

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[1]


def test_platform_smoke_dependencies_are_optional_and_explicit() -> None:
    with (ROOT / "crates/requests/Cargo.toml").open("rb") as manifest:
        core = tomllib.load(manifest)

    expected = {
        "hyper-rustls": "0.27",
        "hyper-util": "0.1",
        "rustls": "0.23",
        "tokio": "1",
    }
    assert core["features"]["platform-smoke"] == [
        f"dep:{dependency}" for dependency in expected
    ]
    for dependency, version in expected.items():
        declaration = core["dependencies"][dependency]
        assert declaration["version"] == version
        assert declaration["optional"] is True


def test_platform_probe_covers_plain_tls_connect_and_socks_shapes() -> None:
    source = (ROOT / "crates/requests/src/lib.rs").read_text()

    assert "HttpsConnectorBuilder" in source
    assert ".https_or_http()" in source
    assert "http_connect_signature" in source
    assert "socks4_signature" in source
    assert "socks5_signature" in source


def test_bootstrap_workflow_has_the_complete_supported_matrix() -> None:
    workflow = ROOT / ".github/workflows/bootstrap-matrix.yml"
    assert workflow.is_file(), "missing bootstrap wheel matrix"
    document = yaml.safe_load(workflow.read_text())

    triggers = document.get("on", document.get(True))
    assert set(triggers) == {"workflow_dispatch", "push", "pull_request"}

    job = document["jobs"]["wheel-smoke"]
    matrix = job["strategy"]["matrix"]
    assert matrix["python"] == [
        "3.10",
        "3.11",
        "3.12",
        "3.13",
        "3.14",
        "3.14t",
        "3.15-dev",
        "pypy-3.11",
    ]
    assert matrix["os"] == ["ubuntu-22.04", "macos-latest", "windows-latest"]
    assert matrix["exclude"] == [
        {"python": "pypy-3.11", "os": "windows-latest"}
    ]

    steps = {step.get("name"): step for step in job["steps"]}
    assert steps["Compile every target and feature"]["run"] == (
        "cargo check --workspace --all-targets --all-features"
    )
    assert steps["Build version-specific wheel"]["run"] == (
        "python -m maturin build --interpreter python --out dist"
    )
    install_script = steps[
        "Install wheel into a fresh environment and import it"
    ]["run"]
    assert "python -m venv wheel-smoke" in install_script
    assert "from requests import _requests_rust" in install_script

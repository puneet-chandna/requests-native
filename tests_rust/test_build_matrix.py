from __future__ import annotations

from pathlib import Path

import yaml

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[1]


def _core_manifest() -> dict:
    with (ROOT / "crates/requests/Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)


def test_direct_http_dependencies_are_required_and_explicit() -> None:
    core = _core_manifest()
    dependencies = core["dependencies"]

    assert dependencies["http-body-util"] == "0.1"
    assert dependencies["hyper"] == {
        "version": "1",
        "features": ["client", "http1"],
    }
    assert dependencies["hyper-util"] == {
        "version": "0.1",
        "default-features": False,
        "features": ["tokio"],
    }
    assert dependencies["tokio"] == {
        "version": "1",
        "features": ["io-util", "macros", "net", "rt", "time"],
    }
    assert core["features"]["blocking"] == ["tokio/rt-multi-thread"]


def test_platform_smoke_keeps_tls_optional_and_legacy_client_gated() -> None:
    core = _core_manifest()
    optional_tls = {
        "hyper-rustls": "0.27",
        "rustls": "0.23",
    }
    assert core["features"]["platform-smoke"] == [
        "dep:hyper-rustls",
        "hyper-util/client-legacy",
        "hyper-util/http1",
        "dep:rustls",
    ]
    for dependency, version in optional_tls.items():
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
    assert matrix["exclude"] == [{"python": "pypy-3.11", "os": "windows-latest"}]

    steps = {step.get("name"): step for step in job["steps"]}
    assert steps["Compile every target and feature"]["run"] == (
        "cargo check --workspace --all-targets --all-features"
    )
    assert steps["Build version-specific wheel"]["run"] == (
        "python -m maturin build --interpreter python --out dist"
    )
    install_script = steps["Install wheel into a fresh environment and import it"][
        "run"
    ]
    assert "python -m venv wheel-smoke" in install_script
    assert "from requests import _requests_rust" in install_script

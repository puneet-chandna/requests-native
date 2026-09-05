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


def test_direct_tls_is_required_and_legacy_client_stays_platform_gated() -> None:
    core = _core_manifest()
    assert core["features"]["platform-smoke"] == [
        "dep:hyper-rustls",
        "hyper-util/client-legacy",
        "hyper-util/http1",
    ]
    assert core["dependencies"]["hyper-rustls"] == {
        "version": "0.27",
        "default-features": False,
        "features": ["http1", "native-tokio", "ring", "tls12"],
        "optional": True,
    }
    assert core["dependencies"]["rustls"] == {
        "version": "0.23",
        "default-features": False,
        "features": ["ring", "std", "tls12"],
    }
    assert core["dependencies"]["tokio-rustls"] == {
        "version": "0.26",
        "default-features": False,
        "features": ["ring", "tls12"],
    }
    assert core["dependencies"]["rustls-pemfile"] == "2"
    assert core["dependencies"]["rustls-native-certs"] == "0.8"


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
    assert set(triggers) == {"workflow_dispatch"}

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
    build_script = steps["Build version-specific wheel"]["run"]
    assert "scripts/build_release_wheel.py" in build_script
    assert "--manylinux 2_34" in build_script
    verification = steps["Verify exact wheel contents"]["run"]
    assert "--verify-wheel" in verification
    assert '--source-commit "$GITHUB_SHA"' in verification
    assert steps["Install maturin"]["run"] == (
        'python -m pip install --upgrade "maturin==1.15.0"'
    )
    assert "SOURCE_DATE_EPOCH" in steps["Set reproducible build epoch"]["run"]
    install_script = steps["Install wheel into a fresh environment and import it"][
        "run"
    ]
    assert "python -m venv wheel-smoke" in install_script
    assert "from requests import _requests_rust" in install_script

from __future__ import annotations

from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[1]


def load_toml(relative_path: str) -> dict:
    path = ROOT / relative_path
    assert path.is_file(), f"missing required manifest: {relative_path}"
    with path.open("rb") as manifest:
        return tomllib.load(manifest)


def test_workspace_has_exactly_two_rust_crates() -> None:
    workspace = load_toml("Cargo.toml")

    assert workspace["workspace"]["members"] == [
        "crates/requests",
        "crates/requests-python",
    ]
    assert workspace["workspace"]["resolver"] == "3"
    assert workspace["workspace"]["package"]["edition"] == "2024"
    assert workspace["workspace"]["lints"]["rust"]["unsafe_code"] == "forbid"


def test_core_is_python_independent_and_binding_depends_on_core() -> None:
    core = load_toml("crates/requests/Cargo.toml")
    binding = load_toml("crates/requests-python/Cargo.toml")

    assert "pyo3" not in core.get("dependencies", {})
    assert core["package"]["name"] == "requests"
    assert "requests" in binding["dependencies"]
    assert binding["dependencies"]["requests"]["path"] == "../requests"
    assert "pyo3" in binding["dependencies"]
    assert binding["lib"]["name"] == "_requests_rust"
    assert binding["lib"]["crate-type"] == ["cdylib"]


def test_maturin_mixed_project_preserves_requests_metadata() -> None:
    project = load_toml("pyproject.toml")

    assert project["build-system"] == {
        "requires": ["maturin>=1.13,<2"],
        "build-backend": "maturin",
    }
    assert project["tool"]["maturin"] == {
        "manifest-path": "crates/requests-python/Cargo.toml",
        "python-source": "src",
        "module-name": "requests._requests_rust",
        "bindings": "pyo3",
        "include": [
            {"path": "HISTORY.md", "format": "sdist"},
        ],
    }

    metadata = project["project"]
    assert metadata["name"] == "requests"
    assert metadata["requires-python"] == ">=3.10"
    assert metadata["dynamic"] == ["version"]
    assert metadata["dependencies"] == [
        "charset_normalizer>=2,<4",
        "idna>=2.5,<4",
        "urllib3>=1.26,<3",
        "certifi>=2023.5.7",
    ]
    assert metadata["optional-dependencies"] == {
        "security": [],
        "socks": ["PySocks>=1.5.6, !=1.5.7"],
        "use_chardet_on_py3": ["chardet>=3.0.2,<8"],
    }

    test_dependencies = project["dependency-groups"]["test"]
    assert "PyYAML>=6" in test_dependencies
    assert "tomli>=2; python_version < '3.11'" in test_dependencies

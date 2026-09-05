from __future__ import annotations

import argparse
import copy
import email.parser
import hashlib
import importlib
import inspect
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
import venv
import warnings
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIST = Path(os.environ.get("REQUESTS_DISTRIBUTION_DIR", ROOT / "dist")).resolve()
PYTHONS = [
    "3.10",
    "3.11",
    "3.12",
    "3.13",
    "3.14",
    "3.14t",
    "3.15-dev",
    "pypy-3.11",
]
SYSTEMS = ["ubuntu-22.04", "macos-latest", "windows-latest"]
FROZEN_ORACLE_COMMIT = "69f84847045bef7a849cc994a26fe7ba8a169e95"
PYTHON_DISTRIBUTION = "requests-native"
PYTHON_DISTRIBUTION_VERSION = "1.0.0b1"
ARTIFACT_STEM = "requests_native-1.0.0b1"
COMPATIBILITY_VERSION = "2.34.2"
BACKEND_NAME = "requests-native"
CORE_CARGO_PACKAGE = "requests-native"
BINDING_CARGO_PACKAGE = "requests-native-python"
EXPECTED_WHEEL_KEYS = {
    (python, system)
    for python in PYTHONS
    for system in SYSTEMS
    if (python, system) != ("pypy-3.11", "windows-latest")
}
EXPECTED_FREE_THREADED_EVIDENCE = {
    f"free-threaded-evidence-{system}.json" for system in SYSTEMS
}
EXPECTED_CLASSIFIERS = [
    "Development Status :: 4 - Beta",
    "Environment :: Web Environment",
    "Intended Audience :: Developers",
    "License :: OSI Approved :: Apache Software License",
    "Natural Language :: English",
    "Operating System :: OS Independent",
    "Programming Language :: Python",
    "Programming Language :: Python :: 3",
    "Programming Language :: Python :: 3.10",
    "Programming Language :: Python :: 3.11",
    "Programming Language :: Python :: 3.12",
    "Programming Language :: Python :: 3.13",
    "Programming Language :: Python :: 3.14",
    "Programming Language :: Python :: 3.15",
    "Programming Language :: Python :: 3 :: Only",
    "Programming Language :: Python :: Implementation :: CPython",
    "Programming Language :: Python :: Implementation :: PyPy",
    "Programming Language :: Python :: Free Threading :: 2 - Beta",
    "Topic :: Internet :: WWW/HTTP",
    "Topic :: Software Development :: Libraries",
]
EXPECTED_URLS = {
    "Documentation": "https://github.com/puneet-chandna/requests-native/tree/main/docs",
    "Homepage": "https://github.com/puneet-chandna/requests-native",
    "Issues": "https://github.com/puneet-chandna/requests-native/issues",
    "Source": "https://github.com/puneet-chandna/requests-native",
    "Upstream": "https://github.com/psf/requests",
}
EXPECTED_DEPENDENCIES = [
    "charset_normalizer>=2,<4",
    "idna>=2.5,<4",
    "urllib3>=1.26,<3",
    "certifi>=2023.5.7",
]
EXPECTED_EXTRAS = {
    "security": [],
    "socks": ["PySocks>=1.5.6, !=1.5.7"],
    "use_chardet_on_py3": ["chardet>=3.0.2,<8"],
}
EXPECTED_PYTHON_MEMBERS = {
    "src/requests/__init__.py",
    "src/requests/__version__.py",
    "src/requests/_internal_utils.py",
    "src/requests/_requests_rust.pyi",
    "src/requests/_rust_public.py",
    "src/requests/_types.py",
    "src/requests/adapters.py",
    "src/requests/api.py",
    "src/requests/auth.py",
    "src/requests/certs.py",
    "src/requests/compat.py",
    "src/requests/cookies.py",
    "src/requests/exceptions.py",
    "src/requests/help.py",
    "src/requests/hooks.py",
    "src/requests/models.py",
    "src/requests/packages.py",
    "src/requests/py.typed",
    "src/requests/sessions.py",
    "src/requests/status_codes.py",
    "src/requests/structures.py",
    "src/requests/utils.py",
}
EXPECTED_REQUESTS_RUST_SOURCES = {
    "crates/requests/src/adapters.rs",
    "crates/requests/src/auth.rs",
    "crates/requests/src/blocking.rs",
    "crates/requests/src/body.rs",
    "crates/requests/src/client.rs",
    "crates/requests/src/cookies.rs",
    "crates/requests/src/error.rs",
    "crates/requests/src/hooks.rs",
    "crates/requests/src/lib.rs",
    "crates/requests/src/models.rs",
    "crates/requests/src/response.rs",
    "crates/requests/src/retry.rs",
    "crates/requests/src/session_runtime.rs",
    "crates/requests/src/structures.rs",
    "crates/requests/src/transport/connect.rs",
    "crates/requests/src/transport/decode.rs",
    "crates/requests/src/transport/mod.rs",
    "crates/requests/src/transport/pool.rs",
    "crates/requests/src/transport/proxy.rs",
    "crates/requests/src/transport/tls.rs",
    "crates/requests/src/utils.rs",
}
EXPECTED_REQUESTS_PYTHON_SOURCES = {
    "crates/requests-python/src/adapters.rs",
    "crates/requests-python/src/auth.rs",
    "crates/requests-python/src/body.rs",
    "crates/requests-python/src/bridge.rs",
    "crates/requests-python/src/callbacks.rs",
    "crates/requests-python/src/cookies.rs",
    "crates/requests-python/src/errors.rs",
    "crates/requests-python/src/lib.rs",
    "crates/requests-python/src/models.rs",
    "crates/requests-python/src/response.rs",
    "crates/requests-python/src/runtime.rs",
    "crates/requests-python/src/sessions.rs",
    "crates/requests-python/src/structures.rs",
}
EXPECTED_SDIST_MEMBERS = (
    {
        "Cargo.lock",
        "Cargo.toml",
        "HISTORY.md",
        "LICENSE",
        "NOTICE",
        "PKG-INFO",
        "README.md",
        "pyproject.toml",
        "crates/requests/Cargo.toml",
        "crates/requests-python/Cargo.toml",
        "crates/requests-python/build.rs",
    }
    | EXPECTED_PYTHON_MEMBERS
    | EXPECTED_REQUESTS_RUST_SOURCES
    | EXPECTED_REQUESTS_PYTHON_SOURCES
)
INSTALLED_SMOKE = r"""
import importlib
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib import metadata
from pathlib import Path

import requests
import requests.adapters as adapters
import requests.compat as compat
import requests.help
import requests.packages
import requests.status_codes as status_codes
import urllib3
import idna

checkout = Path(os.environ["REQUESTS_CHECKOUT"]).resolve()
source = Path(requests.__file__).resolve()
if os.environ["REQUESTS_EDITABLE"] == "1":
    assert source.is_relative_to(checkout / "src"), source
else:
    assert not source.is_relative_to(checkout), source
assert requests.__version__ == "2.34.2"
assert requests._requests_rust.backend_name() == "requests-native"
extension_source = Path(requests._requests_rust.__file__).resolve()
if os.environ["REQUESTS_EDITABLE"] == "1":
    assert extension_source.is_relative_to(checkout / "src"), extension_source
else:
    assert not extension_source.is_relative_to(checkout), extension_source
distribution = metadata.distribution("requests-native")
assert distribution.version == "1.0.0b1"
assert distribution.version != requests.__version__
assert requests.__all__ == (
    "ConnectionError", "ConnectTimeout", "HTTPError", "JSONDecodeError",
    "PreparedRequest", "ReadTimeout", "Request", "RequestException", "Response",
    "Session", "Timeout", "TooManyRedirects", "URLRequired", "codes", "delete",
    "get", "head", "options", "packages", "patch", "post", "put", "request",
    "session", "utils",
)
assert requests.packages.idna is idna
assert requests.packages.chardet is compat.chardet
assert requests.utils is importlib.import_module("requests.utils")
assert requests.codes is status_codes.codes
assert requests.help.info()["requests"]["version"] == requests.__version__
distribution_files = {str(path).replace("\\", "/") for path in distribution.files or ()}
assert any(path.endswith(".dist-info/licenses/LICENSE") for path in distribution_files)
assert any(path.endswith(".dist-info/licenses/NOTICE") for path in distribution_files)
if os.environ["REQUESTS_EDITABLE"] == "0":
    assert "requests/py.typed" in distribution_files
    assert "requests/__init__.py" in distribution_files
    package_init = distribution.locate_file("requests") / "__init__.py"
    assert package_init.is_file()
    assert package_init.resolve() == source
else:
    assert (checkout / "src/requests/py.typed").is_file()
assert requests.packages.urllib3 is urllib3
certs = subprocess.run(
    [sys.executable, "-I", "-m", "requests.certs"],
    text=True,
    capture_output=True,
    check=True,
    timeout=10,
)
assert Path(certs.stdout.strip()).is_file(), certs.stdout

extension = requests._requests_rust
extension._public_facade_pump_trial("reset")
extension._runtime_submission_trial("reset")

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        self.server.requests += 1
        self.send_response(200)
        self.send_header("Content-Length", "6")
        self.end_headers()
        self.wfile.write(b"native")
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.requests = 0
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
url = f"http://127.0.0.1:{server.server_port}/"
default_session = requests.Session()
default_session.trust_env = False
original = adapters._HTTP_ADAPTER_COMPAT_SEND
adapters._HTTP_ADAPTER_COMPAT_SEND = lambda *args, **kwargs: (_ for _ in ()).throw(AssertionError("Python fallback"))
try:
    default_response = default_session.get(url)
    assert default_response.content == b"native"
    assert type(default_response.raw).__module__ == "requests._requests_rust"
    assert server.requests == 1
finally:
    default_session.close()

session = requests.Session()
session.trust_env = False
try:
    with requests._rust_public_trial():
        response = session.get(url)
    assert response.content == b"native"
    assert type(response.raw).__module__ == "requests._requests_rust"
    assert server.requests == 2
finally:
    adapters._HTTP_ADAPTER_COMPAT_SEND = original
    session.close()
    server.shutdown()
    server.server_close()
    worker.join(5)
"""


def load_toml(path: Path) -> dict:
    try:
        import tomllib
    except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
        import tomli as tomllib

    with path.open("rb") as stream:
        return tomllib.load(stream)


def load_workflow(name: str) -> dict:
    import yaml

    return yaml.safe_load((ROOT / ".github/workflows" / name).read_text())


def artifact_paths() -> tuple[Path, Path]:
    wheels = sorted(DIST.glob("requests_native-*.whl"))
    sdists = sorted(DIST.glob("requests_native-*.tar.gz"))
    assert len(wheels) == 1, wheels
    assert len(sdists) == 1, sdists
    return wheels[0], sdists[0]


def test_project_metadata_declares_license_files_dependencies_and_extras() -> None:
    project = load_toml(ROOT / "pyproject.toml")["project"]
    assert project["name"] == PYTHON_DISTRIBUTION
    assert project["description"] == (
        "Unofficial native Rust implementation of Requests with strict Python API "
        "compatibility."
    )
    assert project["authors"] == [{"name": "Puneet Chandna"}]
    assert project["maintainers"] == [{"name": "Puneet Chandna"}]
    assert project["license"] == "Apache-2.0"
    assert project["license-files"] == ["LICENSE", "NOTICE"]
    assert project["dependencies"] == EXPECTED_DEPENDENCIES
    assert project["optional-dependencies"] == EXPECTED_EXTRAS
    assert project["classifiers"] == EXPECTED_CLASSIFIERS
    assert project["urls"] == EXPECTED_URLS

    workspace = load_toml(ROOT / "Cargo.toml")["workspace"]["package"]
    assert workspace["authors"] == ["Puneet Chandna"]
    assert workspace["version"] == "1.0.0-beta.1"
    assert workspace["homepage"] == "https://github.com/puneet-chandna/requests-native"
    assert workspace["repository"] == workspace["homepage"]
    for crate, expected_name in (
        ("requests", CORE_CARGO_PACKAGE),
        ("requests-python", BINDING_CARGO_PACKAGE),
    ):
        package = load_toml(ROOT / f"crates/{crate}/Cargo.toml")["package"]
        assert package["name"] == expected_name
        assert package["publish"] is False
        assert package["authors"]["workspace"] is True
        assert package["homepage"]["workspace"] is True
        assert package["repository"]["workspace"] is True


def test_release_sources_are_allow_listed_and_legal_files_are_canonical() -> None:
    assert not (ROOT / "setup.py").exists()
    assert not (ROOT / "MANIFEST.in").exists()
    maturin = load_toml(ROOT / "pyproject.toml")["tool"]["maturin"]
    assert maturin["include"] == [{"path": "HISTORY.md", "format": "sdist"}]

    requests_package = load_toml(ROOT / "crates/requests/Cargo.toml")["package"]
    assert requests_package["include"] == [
        "Cargo.toml",
        "src/**/*.rs",
        "!src/**/*_tests.rs",
    ]
    requests_python_package = load_toml(ROOT / "crates/requests-python/Cargo.toml")[
        "package"
    ]
    assert requests_python_package["include"] == [
        "Cargo.toml",
        "build.rs",
        "src/**/*.rs",
    ]

    assert (ROOT / ".gitattributes").read_text() == (
        "LICENSE text eol=lf\nNOTICE text eol=lf\n"
    )
    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(ROOT / "scripts/generate_third_party_notices.py"),
            "--check",
        ],
        cwd=ROOT,
        text=True,
        capture_output=True,
    )
    assert completed.returncode == 0, completed.stderr or completed.stdout


def test_oracle_lock_resolves_relative_to_the_repository_or_explicit_override(
    tmp_path: Path,
) -> None:
    from scripts import check_oracle

    resolver = getattr(check_oracle, "resolve_oracle_root", None)
    assert resolver is not None

    repository = tmp_path / "rewrite"
    repository.mkdir()
    lock = {"oracle_path": "../oracle"}
    assert resolver(lock, {}, root=repository) == tmp_path / "oracle"
    override = tmp_path / "explicit-oracle"
    assert (
        resolver(
            lock,
            {"REQUESTS_ORACLE_ROOT": str(override)},
            root=repository,
        )
        == override
    )


def test_wheel_workflow_builds_and_smokes_the_complete_supported_matrix() -> None:
    workflow = load_workflow("wheels.yml")
    triggers = workflow.get("on", workflow.get(True))
    assert set(triggers) == {
        "workflow_call",
        "workflow_dispatch",
        "pull_request",
    }
    assert {
        "Cargo.lock",
        "Cargo.toml",
        "LICENSE",
        "NOTICE",
        "crates/**",
        "pyproject.toml",
        "requirements-dev.txt",
        "src/**",
        "tests/**",
        "tests_differential/**",
        "tests_rust/test_backend_boundary.py",
        "tests_rust/test_distribution.py",
    } <= set(triggers["pull_request"]["paths"])
    porting = " ".join((ROOT / "PORTING.md").read_text().split())
    assert "reusable and manually dispatched release-validation gate" in porting
    assert "complete 23-cell compiled wheel matrix" in porting
    job = workflow["jobs"]["wheels"]
    assert job["strategy"]["fail-fast"] is False
    assert job["strategy"]["matrix"] == {
        "python": PYTHONS,
        "os": SYSTEMS,
        "exclude": [{"python": "pypy-3.11", "os": "windows-latest"}],
    }
    assert len(PYTHONS) * len(SYSTEMS) - 1 == 23
    ordered_names = [step.get("name") for step in job["steps"]]
    steps = {step.get("name"): step for step in job["steps"]}
    assert "python -m maturin build" in steps["Build wheel"]["run"]
    assert "--manylinux 2_34" in steps["Build wheel"]["run"]
    assert "--verify-wheel" in steps["Verify exact wheel contents"]["run"]
    assert "--source-commit" in steps["Verify exact wheel contents"]["run"]
    install = steps["Install wheel and explicit test dependencies"]["run"]
    assert "pytest-httpbin==2.1.0" in install
    assert "PySocks>=1.5.6,!=1.5.7" in install
    assert "-e ." not in install and "--editable" not in install
    assert ordered_names.index("Build wheel") < ordered_names.index(
        "Check out frozen Python oracle"
    )
    assert (
        steps["Check out frozen Python oracle"]["with"]["ref"] == FROZEN_ORACLE_COMMIT
    )
    suite = steps["Run installed artifact suite once"]["run"]
    assert suite.count("--installed-suite") == 1
    evidence = steps["Record free-threaded ABI evidence"]["run"]
    assert '"before_import"' in evidence
    assert '"after_import"' in evidence
    assert "Py_GIL_DISABLED" in evidence
    assert "is False" not in evidence
    assert "gil_used" not in evidence
    assert "free-threaded-evidence-${{ matrix.os }}.json" in evidence
    assert ordered_names.index(
        "Record free-threaded ABI evidence"
    ) < ordered_names.index("Run installed artifact suite once")
    assert steps["Upload wheel"]["with"]["if-no-files-found"] == "error"


def test_source_workflows_cover_rust_default_and_explicit_trial() -> None:
    tests = load_workflow("run-tests.yml")["jobs"]
    for name in ("build", "no_chardet", "urllib3"):
        steps = {step.get("name"): step for step in tests[name]["steps"]}
        ordered_names = [step.get("name") for step in tests[name]["steps"]]
        run_steps = "\n".join(str(step.get("run", "")) for step in tests[name]["steps"])
        assert "Rust default backend" in run_steps
        assert "Default Python backend" not in run_steps
        assert "Explicit Rust trial backend" in run_steps
        oracle = steps["Check out frozen Python oracle"]
        assert oracle["with"] == {
            "repository": "psf/requests",
            "ref": FROZEN_ORACLE_COMMIT,
            "path": "frozen-oracle",
            "persist-credentials": False,
        }
        assert (
            ordered_names.index("Run Rust default backend tests")
            < ordered_names.index("Check out frozen Python oracle")
            < ordered_names.index("Run explicit Rust trial backend tests")
        )
        assert steps["Run explicit Rust trial backend tests"]["env"] == {
            "REQUESTS_ORACLE_ROOT": "${{ github.workspace }}/frozen-oracle"
        }
    assert 'pip uninstall -y "charset_normalizer" "chardet"' in "\n".join(
        str(step.get("run", "")) for step in tests["no_chardet"]["steps"]
    )
    assert 'pip install "urllib3<2"' in "\n".join(
        str(step.get("run", "")) for step in tests["urllib3"]["steps"]
    )
    assert "--compatibility-smoke no-detector --backend default" in "\n".join(
        str(step.get("run", "")) for step in tests["no_chardet"]["steps"]
    )
    assert "--compatibility-smoke no-detector --backend trial" in "\n".join(
        str(step.get("run", "")) for step in tests["no_chardet"]["steps"]
    )
    assert "--compatibility-smoke urllib3-1 --backend default" in "\n".join(
        str(step.get("run", "")) for step in tests["urllib3"]["steps"]
    )
    assert "--compatibility-smoke urllib3-1 --backend trial" in "\n".join(
        str(step.get("run", "")) for step in tests["urllib3"]["steps"]
    )
    smoke_source = inspect.getsource(compatibility_smoke)
    assert 'type(response.raw).__module__ == "requests._requests_rust"' in smoke_source
    assert '.startswith("urllib3")' not in smoke_source

    lint_runs = "\n".join(
        str(step.get("run", ""))
        for step in load_workflow("lint.yml")["jobs"]["lint"]["steps"]
    )
    assert "cargo fmt --all -- --check" in lint_runs
    assert "cargo clippy -p requests-native --all-targets -- -D warnings" in lint_runs


def test_non_release_workflows_cancel_stale_runs_and_limit_safe_triggers() -> None:
    tests = load_workflow("run-tests.yml")
    test_triggers = tests.get("on", tests.get(True))
    build = tests["jobs"]["build"]
    assert build["runs-on"] == "ubuntu-22.04"
    assert "strategy" not in build
    assert tests["concurrency"]["cancel-in-progress"] is True
    assert set(tests["jobs"]) == {"build", "no_chardet", "urllib3"}
    assert test_triggers["push"]["paths"] == test_triggers["pull_request"]["paths"]
    assert test_triggers["push"]["branches"] == ["main"]
    assert {
        ".github/workflows/**",
        "API_COMPATIBILITY.tsv",
        "HISTORY.md",
        "LICENSE",
        "LIFETIMES.tsv",
        "NOTICE",
        "ORACLE.lock",
        "README.md",
        "requirements-dev.txt",
    } <= set(test_triggers["push"]["paths"])

    for name in (
        "lint.yml",
        "typecheck.yml",
        "codeql-analysis.yml",
        "zizmor.yml",
        "run-tests.yml",
    ):
        workflow = load_workflow(name)
        assert workflow["concurrency"]["cancel-in-progress"] is True
        triggers = workflow.get("on", workflow.get(True))
        assert "workflow_dispatch" in triggers
    for name in ("lint.yml", "typecheck.yml"):
        workflow = load_workflow(name)
        triggers = workflow.get("on", workflow.get(True))
        assert triggers["push"]["branches"] == ["main"]

    codeql = load_workflow("codeql-analysis.yml")
    codeql_triggers = codeql.get("on", codeql.get(True))
    assert {
        "docs/**/*.py",
        "scripts/**/*.py",
        "src/**/*.py",
        "src/**/*.pyi",
        "tests/**/*.py",
        "tests_differential/**/*.py",
        "tests_rust/**/*.py",
    } <= set(codeql_triggers["push"]["paths"])
    assert codeql_triggers["push"]["paths"] == codeql_triggers["pull_request"]["paths"]

    zizmor = load_workflow("zizmor.yml")
    zizmor_triggers = zizmor.get("on", zizmor.get(True))
    assert zizmor_triggers["push"]["paths"] == [".github/workflows/**"]
    assert zizmor_triggers["pull_request"]["paths"] == [".github/workflows/**"]


def test_installed_smoke_checks_the_package_file_in_its_distribution() -> None:
    assert "metadata.packages_distributions" not in INSTALLED_SMOKE
    assert 'distribution.locate_file("requests") / "__init__.py"' in INSTALLED_SMOKE


def test_installed_suite_isolates_mutating_test_groups(
    monkeypatch, tmp_path: Path
) -> None:
    for free_threaded in (False, True):
        commands: list[list[str]] = []

        def run(
            command: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[str]:
            if command[1:3] == ["-I", "-c"]:
                assert "Py_GIL_DISABLED" in command[3]
                return subprocess.CompletedProcess(
                    command, 0, stdout=f"{int(free_threaded)}\n"
                )
            commands.append(command)
            return subprocess.CompletedProcess(command, 0)

        with monkeypatch.context() as patch:
            patch.setitem(
                globals(), "run_installed_smoke", lambda *args, **kwargs: None
            )
            patch.setitem(
                globals(),
                "_target_site_packages",
                lambda python: str(tmp_path / "site-packages"),
            )
            patch.setattr(subprocess, "run", run)
            run_installed_suite(Path("python"), tmp_path / "oracle", ROOT)

        import_group = ["tests_differential/test_import_api.py"]
        if free_threaded:
            import_group.append(
                "--deselect=tests_differential/test_import_api.py::"
                "test_import_api_already_compatible_surface_matches_oracle[I04]"
            )
        assert [command[4:] for command in commands] == [
            ["tests"],
            import_group,
            [
                "tests_differential/test_public_types.py",
                "-k",
                "not task17_red_outer_pump_runtime_and_static_call_graph_share_adapter_leaf",
            ],
            [
                "tests_rust/test_backend_boundary.py",
                "tests_differential/test_property_boundaries.py",
            ],
        ]


def test_extension_guards_cpython_function_abi_symbols_from_pypy() -> None:
    manifest = load_toml(ROOT / "crates/requests-python/Cargo.toml")
    build_script = ROOT / "crates/requests-python/build.rs"
    models = (ROOT / "crates/requests-python/src/models.rs").read_text()
    cookies = (ROOT / "crates/requests-python/src/cookies.rs").read_text()
    assert manifest["build-dependencies"]["pyo3-build-config"] == "0.29"
    assert build_script.read_text() == (
        "fn main() {\n    pyo3_build_config::use_pyo3_cfgs();\n}\n"
    )
    for symbol in (
        "PyFunction_GetCode",
        "PyFunction_GetGlobals",
        "PyFunction_GetDefaults",
        "PyFunction_GetKwDefaults",
        "PyFunction_GetClosure",
        "PyCFunction_GetSelf",
    ):
        source = cookies if symbol == "PyFunction_GetGlobals" else models
        assert re.search(
            rf"#\[cfg\(not\(PyPy\)\)\][\s\S]{{0,900}}pyo3::ffi::{symbol}", source
        )
    assert "#[cfg(PyPy)]" in models
    assert "#[cfg(PyPy)]" in cookies
    assert "PyMap_Type" not in models


def test_pypy_proof_state_uses_portable_identity_and_function_metadata() -> None:
    adapters = (ROOT / "crates/requests-python/src/adapters.rs").read_text()
    library = (ROOT / "crates/requests-python/src/lib.rs").read_text()
    identity = adapters.split("fn object_identity", 1)[1].split("\n}\n", 1)[0]
    assert "value.as_ptr() as usize" in identity
    assert 'getattr("id")' not in identity
    assert "#[cfg(PyPy)]\nfn same_object" in adapters
    assert 'getattr("is_")' in adapters
    for cache_name in (
        "_abc_cache",
        "_abc_negative_cache",
        "_abc_negative_cache_version",
    ):
        assert f'"{cache_name}"' in adapters
    assert 'name.starts_with("_abc_")' not in adapters
    assert '"_abc_registry"' not in adapters
    assert "#[cfg(not(PyPy))]\n    closure: Py<PyAny>" in adapters
    assert "#[cfg(not(PyPy))]\n    sequence: Py<PyAny>" in adapters

    assert 'function.getattr("__builtins__")' in library
    assert 'getattr("__globals__")?' in library
    assert '.get_item("__builtins__")?' in library
    assert 'builtins.getattr("__dict__")?' in library
    direct_builtin_lookups = [
        source
        for source in (ROOT / "crates/requests-python/src").glob("*.rs")
        if 'getattr("__builtins__")' in source.read_text()
    ]
    assert direct_builtin_lookups == [ROOT / "crates/requests-python/src/lib.rs"]


def test_extension_stub_covers_the_private_python_bridge() -> None:
    stub = (ROOT / "src/requests/_requests_rust.pyi").read_text()
    for function in (
        "_adapter_fork_reset_trial",
        "_adapter_drop_trial",
        "_adapter_drop_reference_trial",
        "_adapter_reference_trial",
        "_adapter_register_trial",
        "_adapter_send_trial",
        "_adapter_facade_trial",
        "_session_facade_trial",
    ):
        assert f"def {function}" in stub


def test_security_workflows_use_private_repository_safe_reporting() -> None:
    zizmor = load_workflow("zizmor.yml")["jobs"]["zizmor"]
    assert zizmor["permissions"] == {"contents": "read"}
    zizmor_step = zizmor["steps"][-1]
    assert zizmor_step["with"] == {
        "advanced-security": False,
        "annotations": True,
    }

    codeql = load_workflow("codeql-analysis.yml")["jobs"]["analyze"]
    analyze = next(
        step
        for step in codeql["steps"]
        if step.get("name") == "Perform CodeQL Analysis"
    )
    assert analyze["with"]["upload"] == (
        "${{ github.event.repository.private && 'never' || 'always' }}"
    )
    assert analyze["id"] == "analyze"
    private_sarif = next(
        step
        for step in codeql["steps"]
        if step.get("name") == "Persist private CodeQL SARIF"
    )
    assert private_sarif["if"] == "${{ github.event.repository.private }}"
    assert private_sarif["uses"] == (
        "actions/upload-artifact@bbbca2ddaa5d8feaa63e36b76fdaad77386f024f"
    )
    assert private_sarif["with"] == {
        "name": "codeql-sarif",
        "path": "${{ steps.analyze.outputs.sarif-output }}",
        "if-no-files-found": "error",
    }


def test_publish_workflow_validates_one_shared_release_artifact_without_publishing() -> (
    None
):
    workflow = load_workflow("publish.yml")
    jobs = workflow["jobs"]
    assert set(jobs) == {"sdist", "wheels", "manifest"}
    assert jobs["wheels"]["uses"] == "./.github/workflows/wheels.yml"
    sdist_steps = jobs["sdist"]["steps"]
    sdist_by_name = {step.get("name"): step for step in sdist_steps}
    sdist_names = [step.get("name") for step in sdist_steps]
    assert "--verify-sdist" in sdist_by_name["Verify exact sdist contents"]["run"]
    assert "--source-commit" in sdist_by_name["Verify exact sdist contents"]["run"]
    sdist_install = sdist_by_name["Install sdist and explicit test dependencies"]["run"]
    assert "pytest-httpbin==2.1.0" in sdist_install
    assert "-e ." not in sdist_install and "--editable" not in sdist_install
    assert sdist_names.index("Build sdist") < sdist_names.index(
        "Check out frozen Python oracle"
    )
    assert (
        sdist_by_name["Check out frozen Python oracle"]["with"]["ref"]
        == FROZEN_ORACLE_COMMIT
    )
    assert (
        sdist_by_name["Run installed artifact suite once"]["run"].count(
            "--installed-suite"
        )
        == 1
    )
    manifest = jobs["manifest"]
    assert set(manifest["needs"]) == {"sdist", "wheels"}
    manifest_by_name = {step.get("name"): step for step in manifest["steps"]}
    runs = "\n".join(str(step.get("run", "")) for step in manifest["steps"])
    assert "--verify-manifest" in runs
    assert "--source-commit" in runs
    assert "--matrix-result" in runs
    assert "--artifact-metadata" in runs
    assert "--verify-release-set" in runs
    assert (
        "actions/runs/$GITHUB_RUN_ID/artifacts?per_page=100"
        in manifest_by_name["Fetch GitHub artifact metadata"]["run"]
    )
    assert manifest["runs-on"] == "ubuntu-24.04"
    assert manifest["steps"][-1]["name"] == "Verify complete release set"
    assert all(
        "upload-artifact" not in str(step.get("uses", "")) for step in manifest["steps"]
    )
    triggers = workflow.get("on", workflow.get(True))
    assert set(triggers) == {"workflow_dispatch"}
    assert triggers["workflow_dispatch"] is None
    assert workflow["concurrency"] == {
        "group": "beta-artifact-validation-${{ github.ref }}",
        "cancel-in-progress": True,
    }
    assert workflow["permissions"] == {"contents": "read"}
    for job in jobs.values():
        assert "environment" not in job
        job_permissions = job.get("permissions") or {}
        assert isinstance(job_permissions, dict)
        assert "id-token" not in job_permissions
        assert set(job_permissions.values()) <= {"read"}
    workflow_text = (ROOT / ".github/workflows/publish.yml").read_text()
    lowered = workflow_text.lower()
    for forbidden in (
        "id-token: write",
        "write-all",
        "gh-action-pypi-publish",
        "maturin-action",
        "pypi.org",
        "test.pypi.org",
        "crates.io",
        "twine upload",
        "maturin publish",
        "pip upload",
        "gh release upload",
        "gh release create",
        "tags:",
    ):
        assert forbidden not in lowered


def test_public_project_identity_is_derivative_and_registry_safe() -> None:
    public_files = [
        ROOT / "README.md",
        ROOT / ".github/CONTRIBUTING.md",
        ROOT / ".github/CODE_OF_CONDUCT.md",
        ROOT / ".github/SECURITY.md",
        ROOT / "docs/index.rst",
        ROOT / "docs/dev/contributing.rst",
        ROOT / "docs/community/support.rst",
        ROOT / "docs/community/vulnerabilities.rst",
        ROOT / "docs/community/release-process.rst",
        ROOT / "docs/user/install.rst",
    ]
    combined = "\n".join(path.read_text() for path in public_files)
    assert "Requests Native" in combined
    assert "https://github.com/puneet-chandna/requests-native" in combined
    assert "unofficial" in combined.lower()
    assert "not published to PyPI" in combined
    assert "github.com/psf/requests/security/advisories/new" not in combined
    assert "python.org/psf/sponsorship" not in combined
    assert not (ROOT / ".github/FUNDING.yml").exists()
    assert not (ROOT / ".github/ISSUE_TEMPLATE.md").exists()
    assert not (ROOT / ".github/workflows/close-issues.yml").exists()
    assert not (ROOT / ".github/workflows/lock-issues.yml").exists()
    makefile = (ROOT / "Makefile").read_text()
    assert ".publishenv" not in makefile
    assert "twine" not in makefile
    assert "publish:" not in makefile

    security = (ROOT / ".github/SECURITY.md").read_text()
    conduct = (ROOT / ".github/CODE_OF_CONDUCT.md").read_text()
    release = (ROOT / "docs/community/release-process.rst").read_text()
    security_flat = " ".join(security.split())
    conduct_flat = " ".join(conduct.split())
    release_flat = " ".join(release.split())
    assert "private through the" not in conduct
    assert "profile as a private fallback" not in security
    assert "must enable GitHub private vulnerability reporting" in security_flat
    assert "Report a vulnerability" in security_flat
    assert "prefix the report title with `Conduct:`" in conduct_flat
    assert "private vulnerability reporting" in release_flat


def canonical_legal_bytes(source_commit: str) -> dict[str, bytes]:
    return {
        name: subprocess.run(
            ["git", "show", f"{source_commit}:{name}"],
            cwd=ROOT,
            check=True,
            capture_output=True,
        ).stdout
        for name in ("LICENSE", "NOTICE")
    }


def _archive_member_name(name: str) -> tuple[str, bool]:
    is_directory = name.endswith("/")
    normalized = name[:-1] if is_directory else name
    if (
        not normalized
        or "\\" in normalized
        or normalized.startswith("/")
        or re.match(r"^[A-Za-z]:", normalized)
        or any(part in {"", ".", ".."} for part in normalized.split("/"))
    ):
        raise ValueError(f"unsafe archive member name: {name!r}")
    return normalized, is_directory


def _expected_archive_directories(files: set[str]) -> set[str]:
    directories = set()
    for filename in files:
        parts = filename.split("/")[:-1]
        for length in range(1, len(parts) + 1):
            directories.add("/".join(parts[:length]))
    return directories


def verify_wheel(wheel: Path, source_commit: str) -> None:
    legal = canonical_legal_bytes(source_commit)
    with zipfile.ZipFile(wheel) as archive:
        file_members = []
        directory_members = set()
        seen_members = set()
        for member in archive.infolist():
            name, is_directory = _archive_member_name(member.filename)
            if name in seen_members:
                raise ValueError(f"duplicate normalized archive member: {name}")
            seen_members.add(name)
            file_type = stat.S_IFMT(member.external_attr >> 16)
            if is_directory:
                if file_type not in {0, stat.S_IFDIR}:
                    raise ValueError(f"non-directory wheel member: {member.filename}")
                directory_members.add(name)
            else:
                if file_type not in {0, stat.S_IFREG}:
                    raise ValueError(f"non-regular wheel member: {member.filename}")
                file_members.append(name)

        metadata_name = f"{ARTIFACT_STEM}.dist-info/METADATA"
        expected_python = {
            path.removeprefix("src/")
            for path in EXPECTED_PYTHON_MEMBERS
            if path.endswith(".py")
        }
        assert len(expected_python) == 20
        extension = [
            name
            for name in file_members
            if re.fullmatch(r"requests/_requests_rust.*\.(so|pyd|dylib)", name)
        ]
        if len(extension) != 1:
            raise ValueError("wheel must contain exactly one native extension")
        dist_info = f"{ARTIFACT_STEM}.dist-info/"
        sbom_name = f"{dist_info}sboms/{BINDING_CARGO_PACKAGE}.cyclonedx.json"
        expected_members = expected_python | {
            "requests/_requests_rust.pyi",
            "requests/py.typed",
            extension[0],
            f"{dist_info}METADATA",
            f"{dist_info}WHEEL",
            f"{dist_info}RECORD",
            f"{dist_info}licenses/LICENSE",
            f"{dist_info}licenses/NOTICE",
            sbom_name,
        }
        if set(file_members) != expected_members:
            raise ValueError("wheel inventory does not match the canonical package")
        allowed_directories = _expected_archive_directories(expected_members)
        unexpected_directories = directory_members - allowed_directories
        if unexpected_directories:
            raise ValueError(
                f"unexpected wheel directory entries: {sorted(unexpected_directories)!r}"
            )
        metadata = email.parser.BytesParser().parsebytes(archive.read(metadata_name))
        assert archive.read(f"{dist_info}licenses/LICENSE") == legal["LICENSE"]
        assert archive.read(f"{dist_info}licenses/NOTICE") == legal["NOTICE"]
        sbom = json.loads(archive.read(sbom_name))
        assert sbom["bomFormat"] == "CycloneDX"
        assert sbom["metadata"]["component"]["name"] == BINDING_CARGO_PACKAGE
        required_components = {
            (component["name"], component["version"])
            for component in sbom["components"]
            if component.get("scope") == "required"
            and component["name"] != CORE_CARGO_PACKAGE
        }
        notice_components = set(
            re.findall(
                rb"^----- BEGIN ([^ ]+) ([^ ]+) -----$",
                legal["NOTICE"],
                flags=re.MULTILINE,
            )
        )
        assert notice_components == {
            (name.encode(), version.encode()) for name, version in required_components
        }

    assert metadata["Name"] == PYTHON_DISTRIBUTION
    assert metadata["Version"] == PYTHON_DISTRIBUTION_VERSION
    assert metadata["Summary"] == (
        "Unofficial native Rust implementation of Requests with strict Python API "
        "compatibility."
    )
    assert metadata["Requires-Python"] == ">=3.10"
    assert metadata["License-Expression"] == "Apache-2.0"
    assert metadata.get_all("License-File") == ["LICENSE", "NOTICE"]
    assert metadata["Author"] == "Puneet Chandna"
    assert metadata["Maintainer"] == "Puneet Chandna"
    assert metadata["Author-email"] is None
    assert metadata["Maintainer-email"] is None
    assert metadata.get_all("Classifier") == EXPECTED_CLASSIFIERS
    assert sorted(metadata.get_all("Project-URL")) == sorted(
        f"{name}, {url}" for name, url in EXPECTED_URLS.items()
    )
    assert set(metadata.get_all("Requires-Dist")) == {
        "charset-normalizer>=2,<4",
        "idna>=2.5,<4",
        "urllib3>=1.26,<3",
        "certifi>=2023.5.7",
        "pysocks>=1.5.6,!=1.5.7 ; extra == 'socks'",
        "chardet>=3.0.2,<8 ; extra == 'use-chardet-on-py3'",
    }
    assert set(metadata.get_all("Provides-Extra")) == {
        "security",
        "socks",
        "use_chardet_on_py3",
    }


def verify_sdist(sdist: Path, source_commit: str) -> None:
    legal = canonical_legal_bytes(source_commit)
    root = ARTIFACT_STEM
    expected_files = {f"{root}/{name}" for name in EXPECTED_SDIST_MEMBERS}
    allowed_directories = {root} | _expected_archive_directories(expected_files)
    with tarfile.open(sdist, "r:gz") as archive:
        members = {}
        seen_members = set()
        for member in archive.getmembers():
            name, name_is_directory = _archive_member_name(member.name)
            if name in seen_members:
                raise ValueError(f"duplicate normalized archive member: {name}")
            seen_members.add(name)
            if name != root and not name.startswith(f"{root}/"):
                raise ValueError(f"sdist root must be exactly {root}: {member.name}")
            if member.isdir():
                if name not in allowed_directories:
                    raise ValueError(f"unexpected sdist directory: {member.name}")
                continue
            if name_is_directory or not member.isfile():
                raise ValueError(f"non-regular sdist member: {member.name}")
            members[name] = member
        assert len(EXPECTED_SDIST_MEMBERS) == 67
        if set(members) != expected_files:
            raise ValueError("sdist inventory does not match the canonical source tree")
        license_stream = archive.extractfile(members[f"{root}/LICENSE"])
        notice_stream = archive.extractfile(members[f"{root}/NOTICE"])
        assert license_stream is not None and license_stream.read() == legal["LICENSE"]
        assert notice_stream is not None and notice_stream.read() == legal["NOTICE"]


def test_built_wheel_and_sdist_have_complete_clean_inventory_and_metadata() -> None:
    wheel, sdist = artifact_paths()
    verify_wheel(wheel, "HEAD")
    verify_sdist(sdist, "HEAD")


def _rewrite_wheel(
    source: Path,
    destination: Path,
    *,
    renamed: dict[str, str] | None = None,
    duplicate: str | None = None,
) -> None:
    renamed = renamed or {}
    with zipfile.ZipFile(source) as original, zipfile.ZipFile(destination, "w") as output:
        for member in original.infolist():
            replacement = copy.copy(member)
            replacement.filename = renamed.get(member.filename, member.filename)
            output.writestr(replacement, original.read(member))
        if duplicate is not None:
            output.writestr(duplicate, original.read(duplicate))


def _tar_member(name: str, *, kind: bytes = tarfile.REGTYPE) -> tarfile.TarInfo:
    member = tarfile.TarInfo(name)
    member.type = kind
    member.mode = 0o644
    if kind in {tarfile.SYMTYPE, tarfile.LNKTYPE}:
        member.linkname = f"{ARTIFACT_STEM}/LICENSE"
    if kind in {tarfile.CHRTYPE, tarfile.BLKTYPE}:
        member.devmajor = 1
        member.devminor = 3
    return member


def _rewrite_sdist(
    source: Path,
    destination: Path,
    *,
    renamed: dict[str, str] | None = None,
    additions: tuple[tuple[tarfile.TarInfo, bytes | None], ...] = (),
) -> None:
    renamed = renamed or {}
    with tarfile.open(source, "r:gz") as original, tarfile.open(
        destination, "w:gz"
    ) as output:
        for member in original.getmembers():
            replacement = copy.copy(member)
            replacement.name = renamed.get(member.name, member.name)
            stream = original.extractfile(member) if member.isfile() else None
            output.addfile(replacement, stream)
        for member, data in additions:
            member.size = len(data) if data is not None else 0
            output.addfile(member, io.BytesIO(data) if data is not None else None)


def test_wheel_validator_rejects_duplicate_member(tmp_path: Path) -> None:
    wheel, _ = artifact_paths()
    invalid = tmp_path / wheel.name
    _rewrite_wheel(wheel, invalid, duplicate="requests/__init__.py")

    with unittest.TestCase().assertRaisesRegex(ValueError, "duplicate"):
        verify_wheel(invalid, "HEAD")


def test_wheel_validator_rejects_noncanonical_member_names(tmp_path: Path) -> None:
    wheel, _ = artifact_paths()
    for index, invalid_name in enumerate(
        ("/requests/__init__.py", "requests/../requests/__init__.py")
    ):
        invalid = tmp_path / f"invalid-{index}.whl"
        _rewrite_wheel(
            wheel,
            invalid,
            renamed={"requests/__init__.py": invalid_name},
        )
        with unittest.TestCase().assertRaisesRegex(ValueError, "archive member"):
            verify_wheel(invalid, "HEAD")


def test_wheel_validator_requires_exact_dist_info_root(tmp_path: Path) -> None:
    wheel, _ = artifact_paths()
    with zipfile.ZipFile(wheel) as archive:
        renamed = {
            name: name.replace(
                f"{ARTIFACT_STEM}.dist-info/", "not-requests.dist-info/", 1
            )
            for name in archive.namelist()
            if name.startswith(f"{ARTIFACT_STEM}.dist-info/")
        }
    invalid = tmp_path / wheel.name
    _rewrite_wheel(wheel, invalid, renamed=renamed)

    with unittest.TestCase().assertRaisesRegex(ValueError, "wheel inventory"):
        verify_wheel(invalid, "HEAD")


def test_sdist_validator_rejects_duplicate_regular_member(tmp_path: Path) -> None:
    _, sdist = artifact_paths()
    invalid = tmp_path / sdist.name
    license_bytes = canonical_legal_bytes("HEAD")["LICENSE"]
    _rewrite_sdist(
        sdist,
        invalid,
        additions=((_tar_member(f"{ARTIFACT_STEM}/LICENSE"), license_bytes),),
    )

    with unittest.TestCase().assertRaisesRegex(ValueError, "duplicate"):
        verify_sdist(invalid, "HEAD")


def test_sdist_validator_requires_one_exact_root(tmp_path: Path) -> None:
    _, sdist = artifact_paths()
    with tarfile.open(sdist, "r:gz") as archive:
        names = [member.name for member in archive.getmembers()]

    wrong_root = tmp_path / "wrong-root.tar.gz"
    _rewrite_sdist(
        sdist,
        wrong_root,
        renamed={
            name: name.replace(f"{ARTIFACT_STEM}/", "wrong-1.0.0b1/", 1)
            for name in names
        },
    )
    with unittest.TestCase().assertRaisesRegex(ValueError, "sdist root"):
        verify_sdist(wrong_root, "HEAD")

    multiple_roots = tmp_path / "multiple-roots.tar.gz"
    _rewrite_sdist(
        sdist,
        multiple_roots,
        renamed={f"{ARTIFACT_STEM}/LICENSE": "other-root/LICENSE"},
    )
    with unittest.TestCase().assertRaisesRegex(ValueError, "sdist root"):
        verify_sdist(multiple_roots, "HEAD")


def test_sdist_validator_rejects_absolute_and_traversal_names(
    tmp_path: Path,
) -> None:
    _, sdist = artifact_paths()
    for index, invalid_name in enumerate(("/LICENSE", "../LICENSE")):
        invalid = tmp_path / f"invalid-name-{index}.tar.gz"
        _rewrite_sdist(
            sdist,
            invalid,
            renamed={f"{ARTIFACT_STEM}/LICENSE": invalid_name},
        )
        with unittest.TestCase().assertRaisesRegex(ValueError, "archive member"):
            verify_sdist(invalid, "HEAD")


def test_sdist_validator_rejects_every_non_regular_member(tmp_path: Path) -> None:
    _, sdist = artifact_paths()
    cases = (
        ("symlink", tarfile.SYMTYPE),
        ("hardlink", tarfile.LNKTYPE),
        ("character-device", tarfile.CHRTYPE),
        ("block-device", tarfile.BLKTYPE),
        ("fifo", tarfile.FIFOTYPE),
    )
    for label, kind in cases:
        invalid = tmp_path / f"{label}.tar.gz"
        _rewrite_sdist(
            sdist,
            invalid,
            additions=((_tar_member(f"{ARTIFACT_STEM}/{label}", kind=kind), None),),
        )
        with unittest.TestCase().assertRaisesRegex(ValueError, "non-regular"):
            verify_sdist(invalid, "HEAD")


def test_sdist_validator_allows_only_expected_directory_entries(
    tmp_path: Path,
) -> None:
    _, sdist = artifact_paths()
    expected = tmp_path / "expected-directory.tar.gz"
    _rewrite_sdist(
        sdist,
        expected,
        additions=((_tar_member(f"{ARTIFACT_STEM}/src/requests/", kind=tarfile.DIRTYPE), None),),
    )
    verify_sdist(expected, "HEAD")

    unexpected = tmp_path / "unexpected-directory.tar.gz"
    _rewrite_sdist(
        sdist,
        unexpected,
        additions=((_tar_member(f"{ARTIFACT_STEM}/unexpected/", kind=tarfile.DIRTYPE), None),),
    )
    with unittest.TestCase().assertRaisesRegex(ValueError, "directory"):
        verify_sdist(unexpected, "HEAD")


def _clean_environment() -> dict[str, str]:
    preserved = (
        "COMSPEC",
        "DYLD_LIBRARY_PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LD_LIBRARY_PATH",
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "WINDIR",
    )
    environment = {name: os.environ[name] for name in preserved if name in os.environ}
    environment.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONNOUSERSITE": "1",
            "REQUESTS_CHECKOUT": str(ROOT),
        }
    )
    return environment


def run_installed_smoke(python: Path, *, editable: bool, checkout: Path = ROOT) -> None:
    environment = _clean_environment()
    environment["REQUESTS_CHECKOUT"] = str(checkout)
    environment["REQUESTS_EDITABLE"] = str(int(editable))
    with tempfile.TemporaryDirectory(prefix="requests-installed-smoke-") as directory:
        subprocess.run(
            [str(python), "-I", "-c", INSTALLED_SMOKE],
            cwd=directory,
            env=environment,
            check=True,
            timeout=60,
        )


def _target_is_free_threaded(python: Path) -> bool:
    completed = subprocess.run(
        [
            str(python),
            "-I",
            "-c",
            "import sysconfig; print(int(sysconfig.get_config_var('Py_GIL_DISABLED') or 0))",
        ],
        text=True,
        capture_output=True,
        check=True,
        timeout=30,
    )
    return completed.stdout.strip() == "1"


def run_installed_suite(python: Path, oracle: Path, checkout: Path) -> None:
    run_installed_smoke(python, editable=False, checkout=checkout)
    environment = _clean_environment()
    environment.update(
        {
            "REQUESTS_ORACLE_ROOT": str(oracle.resolve()),
            "REQUESTS_DIFFERENTIAL_REWRITE_ROOT": _target_site_packages(python),
        }
    )
    with tempfile.TemporaryDirectory(prefix="requests-installed-suite-") as directory:
        suite = Path(directory)
        shutil.copytree(ROOT / "tests", suite / "tests")
        shutil.copytree(ROOT / "tests_differential", suite / "tests_differential")
        shutil.copy2(ROOT / "requirements-dev.txt", suite / "requirements-dev.txt")
        (suite / "tests_rust").mkdir()
        shutil.copy2(
            ROOT / "tests_rust/test_backend_boundary.py",
            suite / "tests_rust/test_backend_boundary.py",
        )
        import_group = ("tests_differential/test_import_api.py",)
        if _target_is_free_threaded(python):
            import_group += (
                "--deselect=tests_differential/test_import_api.py::"
                "test_import_api_already_compatible_surface_matches_oracle[I04]",
            )
        groups = (
            ("tests",),
            import_group,
            (
                "tests_differential/test_public_types.py",
                "-k",
                "not task17_red_outer_pump_runtime_and_static_call_graph_share_adapter_leaf",
            ),
            (
                "tests_rust/test_backend_boundary.py",
                "tests_differential/test_property_boundaries.py",
            ),
        )
        for group in groups:
            subprocess.run(
                [str(python), "-m", "pytest", "-q", *group],
                cwd=suite,
                env=environment,
                check=True,
                timeout=900,
            )


def _target_site_packages(python: Path) -> str:
    return subprocess.run(
        [
            str(python),
            "-I",
            "-c",
            "import sysconfig; print(sysconfig.get_path('platlib'))",
        ],
        text=True,
        capture_output=True,
        check=True,
        timeout=30,
    ).stdout.strip()


def _assert_fresh_install(tmp_path: Path, kind: str) -> None:
    wheel, sdist = artifact_paths()
    environment = _clean_environment()
    virtualenv = tmp_path / kind
    venv.EnvBuilder(with_pip=True).create(virtualenv)
    python = virtualenv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    source = {"wheel": wheel, "sdist": sdist, "editable": ROOT}[kind]
    command = [str(python), "-m", "pip", "install", "--disable-pip-version-check"]
    if kind == "editable":
        command.append("--editable")
    subprocess.run(
        [*command, str(source)],
        cwd=tmp_path,
        env=environment,
        check=True,
        timeout=240,
    )
    run_installed_smoke(python, editable=kind == "editable")


def test_fresh_wheel_install_runs_default_and_trial_outside_checkout(
    tmp_path: Path,
) -> None:
    _assert_fresh_install(tmp_path, "wheel")


def test_fresh_sdist_install_runs_default_and_trial_outside_checkout(
    tmp_path: Path,
) -> None:
    _assert_fresh_install(tmp_path, "sdist")


def test_fresh_editable_install_runs_default_and_trial_outside_checkout(
    tmp_path: Path,
) -> None:
    _assert_fresh_install(tmp_path, "editable")


def wheel_key(path: Path) -> tuple[str, str]:
    distribution, python_tag, abi_tag, platform_tag = path.stem.rsplit("-", 3)
    if distribution != ARTIFACT_STEM:
        raise ValueError(f"unexpected wheel project/version: {path.name}")
    if python_tag == "pp311" and abi_tag == "pypy311_pp73":
        python = "pypy-3.11"
    elif python_tag == "cp314" and abi_tag == "cp314t":
        python = "3.14t"
    elif re.fullmatch(r"cp3(10|11|12|13|14|15)", python_tag) and abi_tag == python_tag:
        match = re.fullmatch(r"cp(3\d{2})", python_tag)
        assert match is not None
        digits = match.group(1)
        python = f"{digits[0]}.{digits[1:]}"
        if python == "3.15":
            python = "3.15-dev"
    else:
        raise ValueError(f"unsupported Python wheel tag: {path.name}")
    if re.fullmatch(r"macosx_[A-Za-z0-9_.]+_(x86_64|arm64)", platform_tag):
        system = "macos-latest"
    elif platform_tag == "win_amd64":
        system = "windows-latest"
    elif platform_tag == "manylinux_2_34_x86_64":
        system = "ubuntu-22.04"
    else:
        raise ValueError(f"unsupported platform wheel tag: {path.name}")
    return python, system


def compatibility_smoke(kind: str, backend: str) -> None:
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        requests = importlib.import_module("requests")
    compat = importlib.import_module("requests.compat")
    packages = importlib.import_module("requests.packages")
    urllib3 = importlib.import_module("urllib3")

    if kind == "no-detector":
        assert compat.chardet is None
        assert packages.chardet is None
        dependency_warnings = [
            item
            for item in caught
            if item.category is requests.exceptions.RequestsDependencyWarning
            and "Unable to find acceptable character detection dependency"
            in str(item.message)
        ]
        assert len(dependency_warnings) == 1
        assert (
            sum(
                item.category is requests.exceptions.RequestsDependencyWarning
                for item in caught
            )
            == 1
        )
    elif kind == "urllib3-1":
        assert urllib3.__version__.startswith("1.26.")
        assert compat.is_urllib3_1 is True
        assert packages.urllib3 is urllib3
    else:
        raise ValueError(kind)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")

        def log_message(self, format, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    session = requests.Session()
    session.trust_env = False
    try:
        if backend == "trial":
            with requests._rust_public_trial():
                response = session.get(f"http://127.0.0.1:{server.server_port}/")
        else:
            response = session.get(f"http://127.0.0.1:{server.server_port}/")
        assert type(response.raw).__module__ == "requests._requests_rust"
        assert response.content == b"ok"
    finally:
        session.close()
        server.shutdown()
        server.server_close()
        worker.join(5)


def verify_manifest(directory: Path) -> dict[str, str]:
    wheels = sorted(directory.rglob("requests_native-*.whl"))
    sdists = sorted(directory.rglob("requests_native-*.tar.gz"))
    evidence = sorted(directory.rglob("free-threaded-evidence-*.json"))
    files = {path for path in directory.rglob("*") if path.is_file()}
    if len(sdists) != 1:
        raise ValueError(f"expected one sdist, found {len(sdists)}")
    if sdists[0].name != f"{ARTIFACT_STEM}.tar.gz":
        raise ValueError(f"unexpected sdist project/version: {sdists[0].name}")
    if (
        len(evidence) != len(EXPECTED_FREE_THREADED_EVIDENCE)
        or {path.name for path in evidence} != EXPECTED_FREE_THREADED_EVIDENCE
    ):
        raise ValueError(
            "free-threaded evidence matrix mismatch: "
            f"found={sorted(path.name for path in evidence)!r}"
        )
    if files != {*wheels, *sdists, *evidence}:
        raise ValueError("unexpected release artifact")
    for path in evidence:
        free_threaded = json.loads(path.read_text())
        if (
            set(free_threaded) != {"Py_GIL_DISABLED", "before_import", "after_import"}
            or free_threaded["Py_GIL_DISABLED"] != 1
            or type(free_threaded["before_import"]) is not bool
            or type(free_threaded["after_import"]) is not bool
        ):
            raise ValueError(f"invalid free-threaded evidence: {path.name}")
        system = path.name.removeprefix("free-threaded-evidence-").removesuffix(".json")
        sibling_wheels = list(path.parent.glob("requests_native-*.whl"))
        if len(sibling_wheels) != 1 or wheel_key(sibling_wheels[0]) != (
            "3.14t",
            system,
        ):
            raise ValueError(f"orphaned free-threaded evidence: {path.name}")
    keys = [wheel_key(path) for path in wheels]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate wheel matrix key")
    missing = EXPECTED_WHEEL_KEYS - set(keys)
    unexpected = set(keys) - EXPECTED_WHEEL_KEYS
    if missing or unexpected:
        raise ValueError(
            f"wheel matrix mismatch: missing={sorted(missing)!r} unexpected={sorted(unexpected)!r}"
        )
    artifacts = [sdists[0], *wheels]
    return {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in artifacts
    }


def build_publish_manifest(
    directory: Path,
    *,
    source_commit: str,
    matrix_result: str,
    artifact_metadata: dict | None = None,
) -> dict:
    if not re.fullmatch(r"[0-9a-fA-F]{40}", source_commit):
        raise ValueError("source commit must be a 40-character hexadecimal SHA")
    if matrix_result not in {"success", "failure", "cancelled", "skipped", "local"}:
        raise ValueError(f"unexpected matrix result: {matrix_result}")

    hashes = verify_manifest(directory)
    paths = {
        path.name: path
        for path in directory.rglob("requests_native-*")
        if path.is_file() and path.name in hashes
    }
    source_names = {
        "sdist" if path.name.endswith(".tar.gz") else path.parent.name
        for path in paths.values()
    }
    github_artifacts = {}
    if artifact_metadata is not None:
        artifacts = artifact_metadata.get("artifacts")
        if not isinstance(artifacts, list):
            raise ValueError("GitHub artifact metadata must contain an artifacts list")
        for artifact in artifacts:
            if not isinstance(artifact, dict):
                raise ValueError("GitHub artifact metadata entries must be objects")
            name = artifact.get("name")
            if not isinstance(name, str) or not name:
                raise ValueError("GitHub artifact name must be a non-empty string")
            if name in github_artifacts:
                raise ValueError(f"duplicate GitHub artifact metadata: {name}")
            _validate_github_artifact_provenance(
                artifact.get("id"), artifact.get("digest"), context="GitHub artifact"
            )
            github_artifacts[name] = artifact
        if set(github_artifacts) != source_names:
            raise ValueError("GitHub artifact metadata does not match release fan-out")

    manifest_artifacts = []
    for filename, sha256 in sorted(hashes.items()):
        path = paths[filename]
        source_artifact = "sdist" if filename.endswith(".tar.gz") else path.parent.name
        github = github_artifacts.get(source_artifact, {})
        manifest_artifacts.append(
            {
                "filename": filename,
                "github_archive_digest": github.get("digest"),
                "github_artifact_id": github.get("id"),
                "sha256": sha256,
                "source_artifact": source_artifact,
            }
        )
    return {
        "artifacts": manifest_artifacts,
        "matrix_result": matrix_result,
        "source_commit": source_commit.lower(),
    }


def _validate_github_artifact_provenance(
    artifact_id: object, archive_digest: object, *, context: str
) -> None:
    if (
        isinstance(artifact_id, bool)
        or not isinstance(artifact_id, int)
        or artifact_id <= 0
    ):
        raise ValueError(f"{context} id must be a positive integer")
    if not isinstance(archive_digest, str) or not re.fullmatch(
        r"sha256:[0-9a-f]{64}", archive_digest
    ):
        raise ValueError(
            f"{context} digest must be sha256 followed by 64 lowercase hex digits"
        )


def verify_release_set(
    directory: Path, manifest_path: Path, source_commit: str
) -> None:
    files = {path.resolve() for path in directory.rglob("*") if path.is_file()}
    wheels = sorted(directory.rglob("requests_native-*.whl"))
    sdists = sorted(directory.rglob("requests_native-*.tar.gz"))
    if len(wheels) != 23 or len(sdists) != 1:
        raise ValueError(
            f"expected 23 wheels and one sdist, found {len(wheels)} and {len(sdists)}"
        )
    if files != {
        manifest_path.resolve(),
        *(path.resolve() for path in wheels),
        sdists[0].resolve(),
    }:
        raise ValueError(
            "release set must contain exactly 23 wheels, one sdist, and one manifest"
        )
    keys = [wheel_key(path) for path in wheels]
    if len(keys) != len(set(keys)) or set(keys) != EXPECTED_WHEEL_KEYS:
        raise ValueError("release-set wheel matrix mismatch")

    manifest = json.loads(manifest_path.read_text())
    if set(manifest) != {"artifacts", "matrix_result", "source_commit"}:
        raise ValueError("unexpected release manifest fields")
    if manifest["matrix_result"] != "success":
        raise ValueError("release matrix did not succeed")
    if manifest["source_commit"] != source_commit.lower():
        raise ValueError("release manifest source commit mismatch")
    records = manifest["artifacts"]
    if not isinstance(records, list) or len(records) != 24:
        raise ValueError("release manifest must contain exactly 24 archives")
    records_by_name = {record.get("filename"): record for record in records}
    archives = [sdists[0], *wheels]
    if len(records_by_name) != 24 or set(records_by_name) != {
        path.name for path in archives
    }:
        raise ValueError("release manifest archive inventory mismatch")
    for path in archives:
        record = records_by_name[path.name]
        if set(record) != {
            "filename",
            "github_archive_digest",
            "github_artifact_id",
            "sha256",
            "source_artifact",
        }:
            raise ValueError(f"unexpected manifest record fields: {path.name}")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if record["sha256"] != digest:
            raise ValueError(f"release manifest hash mismatch: {path.name}")
        _validate_github_artifact_provenance(
            record["github_artifact_id"],
            record["github_archive_digest"],
            context="GitHub artifact provenance",
        )
    verify_sdist(sdists[0], source_commit)
    for wheel in wheels:
        verify_wheel(wheel, source_commit)


def write_complete_manifest_fixture(directory: Path) -> None:
    (directory / f"{ARTIFACT_STEM}.tar.gz").touch()
    for python, system in sorted(EXPECTED_WHEEL_KEYS):
        python_tag = {
            "3.10": "cp310-cp310",
            "3.11": "cp311-cp311",
            "3.12": "cp312-cp312",
            "3.13": "cp313-cp313",
            "3.14": "cp314-cp314",
            "3.14t": "cp314-cp314t",
            "3.15-dev": "cp315-cp315",
            "pypy-3.11": "pp311-pypy311_pp73",
        }[python]
        platform = {
            "ubuntu-22.04": "manylinux_2_34_x86_64",
            "macos-latest": "macosx_11_0_x86_64",
            "windows-latest": "win_amd64",
        }[system]
        artifact = directory / f"wheel-{python}-{system}"
        artifact.mkdir()
        (artifact / f"{ARTIFACT_STEM}-{python_tag}-{platform}.whl").touch()
        if python == "3.14t":
            (artifact / f"free-threaded-evidence-{system}.json").write_text(
                '{"Py_GIL_DISABLED": 1, "after_import": true, "before_import": false}\n'
            )


def github_metadata_for(directory: Path) -> dict:
    names = ["sdist", *sorted(path.name for path in directory.glob("wheel-*"))]
    return {
        "artifacts": [
            {"name": name, "id": index, "digest": f"sha256:{index:064x}"}
            for index, name in enumerate(names, 1)
        ]
    }


def test_publish_manifest_accepts_only_the_complete_unique_matrix(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    manifest = verify_manifest(tmp_path)
    assert len(manifest) == 24

    next(tmp_path.rglob("requests_native-*cp310-cp310*linux*.whl")).unlink()
    with unittest.TestCase().assertRaisesRegex(ValueError, "wheel matrix mismatch"):
        verify_manifest(tmp_path)

    duplicate = tmp_path / "duplicate"
    duplicate.mkdir()
    source = next(tmp_path.rglob("requests_native-*cp311-cp311*linux*.whl"))
    (duplicate / source.name).touch()
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "duplicate wheel matrix key"
    ):
        verify_manifest(tmp_path)


def test_publish_manifest_records_commit_matrix_and_github_artifacts(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    metadata = github_metadata_for(tmp_path)

    manifest = build_publish_manifest(
        tmp_path,
        source_commit="a" * 40,
        matrix_result="success",
        artifact_metadata=metadata,
    )

    assert manifest["source_commit"] == "a" * 40
    assert manifest["matrix_result"] == "success"
    assert len(manifest["artifacts"]) == 24
    assert manifest["artifacts"] == sorted(
        manifest["artifacts"], key=lambda artifact: artifact["filename"]
    )
    sdist = next(
        artifact
        for artifact in manifest["artifacts"]
        if artifact["filename"].endswith(".tar.gz")
    )
    assert sdist["source_artifact"] == "sdist"
    assert sdist["github_artifact_id"] == 1
    assert sdist["github_archive_digest"] == f"sha256:{1:064x}"
    assert re.fullmatch(r"[0-9a-f]{64}", sdist["sha256"])


def test_publish_manifest_rejects_invalid_github_artifact_provenance(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    invalid_values = (
        ("id", 0),
        ("id", -1),
        ("id", True),
        ("id", "1"),
        ("digest", None),
        ("digest", "sha256:abcd"),
        ("digest", "sha256:" + "A" * 64),
        ("digest", "SHA256:" + "a" * 64),
    )
    for field, value in invalid_values:
        metadata = github_metadata_for(tmp_path)
        metadata["artifacts"][0][field] = value
        with unittest.TestCase().assertRaisesRegex(
            ValueError, f"GitHub artifact {field}"
        ):
            build_publish_manifest(
                tmp_path,
                source_commit="a" * 40,
                matrix_result="success",
                artifact_metadata=metadata,
            )


def test_release_set_verifies_one_manifest_and_all_24_archives(
    monkeypatch, tmp_path: Path
) -> None:
    artifacts = tmp_path / "artifacts"
    artifacts.mkdir()
    write_complete_manifest_fixture(artifacts)
    source_commit = "a" * 40
    manifest = build_publish_manifest(
        artifacts,
        source_commit=source_commit,
        matrix_result="success",
        artifact_metadata=github_metadata_for(artifacts),
    )
    release = tmp_path / "release"
    release.mkdir()
    for path in artifacts.rglob("requests_native-*"):
        if path.is_file():
            shutil.copy2(path, release / path.name)
    manifest_path = release / "artifact-manifest.json"
    manifest_path.write_text(json.dumps(manifest, sort_keys=True) + "\n")
    monkeypatch.setitem(globals(), "verify_wheel", lambda *args: None)
    monkeypatch.setitem(globals(), "verify_sdist", lambda *args: None)

    verify_release_set(release, manifest_path, source_commit)

    (release / "unexpected.txt").touch()
    with unittest.TestCase().assertRaisesRegex(ValueError, "exactly 23 wheels"):
        verify_release_set(release, manifest_path, source_commit)


def test_release_set_rejects_invalid_github_artifact_provenance(
    monkeypatch, tmp_path: Path
) -> None:
    artifacts = tmp_path / "artifacts"
    artifacts.mkdir()
    write_complete_manifest_fixture(artifacts)
    source_commit = "a" * 40
    manifest = build_publish_manifest(
        artifacts,
        source_commit=source_commit,
        matrix_result="success",
        artifact_metadata=github_metadata_for(artifacts),
    )
    release = tmp_path / "release"
    release.mkdir()
    for path in artifacts.rglob("requests_native-*"):
        if path.is_file():
            shutil.copy2(path, release / path.name)
    manifest_path = release / "artifact-manifest.json"
    monkeypatch.setitem(globals(), "verify_wheel", lambda *args: None)
    monkeypatch.setitem(globals(), "verify_sdist", lambda *args: None)

    invalid_values = (
        ("github_artifact_id", 0),
        ("github_artifact_id", True),
        ("github_archive_digest", "sha256:abcd"),
        ("github_archive_digest", "sha256:" + "A" * 64),
    )
    for field, value in invalid_values:
        invalid_manifest = copy.deepcopy(manifest)
        invalid_manifest["artifacts"][0][field] = value
        manifest_path.write_text(json.dumps(invalid_manifest, sort_keys=True) + "\n")
        with unittest.TestCase().assertRaisesRegex(
            ValueError, "GitHub artifact provenance"
        ):
            verify_release_set(release, manifest_path, source_commit)


def test_wheel_key_rejects_wrong_version_abi_and_platform() -> None:
    cases = [
        ("requests-9.9.9-cp310-cp310-manylinux_2_17_x86_64.whl", "project/version"),
        (f"{ARTIFACT_STEM}-cp310-abi3-manylinux_2_17_x86_64.whl", "Python wheel tag"),
        (f"{ARTIFACT_STEM}-py3-none-any.whl", "Python wheel tag"),
        (f"{ARTIFACT_STEM}-cp310-cp310-any.whl", "platform wheel tag"),
        (
            f"{ARTIFACT_STEM}-cp310-cp310-linux_x86_64.whl",
            "platform wheel tag",
        ),
        (
            f"{ARTIFACT_STEM}-cp310-cp310-musllinux_1_2_x86_64.whl",
            "platform wheel tag",
        ),
    ]
    for filename, message in cases:
        with unittest.TestCase().assertRaisesRegex(ValueError, message):
            wheel_key(Path(filename))


def test_publish_manifest_rejects_extra_artifacts(tmp_path: Path) -> None:
    write_complete_manifest_fixture(tmp_path)
    (tmp_path / "unexpected.txt").touch()
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "unexpected release artifact"
    ):
        verify_manifest(tmp_path)


def test_publish_manifest_requires_all_free_threaded_evidence(tmp_path: Path) -> None:
    write_complete_manifest_fixture(tmp_path)
    next(tmp_path.rglob("free-threaded-evidence-macos-latest.json")).unlink()
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "free-threaded evidence matrix mismatch"
    ):
        verify_manifest(tmp_path)


def test_publish_manifest_rejects_invalid_free_threaded_evidence(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    next(tmp_path.rglob("free-threaded-evidence-windows-latest.json")).write_text(
        "{}\n"
    )
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "invalid free-threaded evidence"
    ):
        verify_manifest(tmp_path)


def test_publish_manifest_rejects_duplicate_and_orphaned_free_threaded_evidence(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    evidence = next(tmp_path.rglob("free-threaded-evidence-ubuntu-22.04.json"))
    duplicate = tmp_path / "duplicate" / evidence.name
    duplicate.parent.mkdir()
    shutil.copy2(evidence, duplicate)
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "free-threaded evidence matrix mismatch"
    ):
        verify_manifest(tmp_path)

    duplicate.unlink()
    evidence.replace(tmp_path / evidence.name)
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "orphaned free-threaded evidence"
    ):
        verify_manifest(tmp_path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify-manifest", type=Path)
    parser.add_argument("--source-commit")
    parser.add_argument("--matrix-result")
    parser.add_argument("--artifact-metadata", type=Path)
    parser.add_argument("--verify-release-set", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--verify-wheel", type=Path)
    parser.add_argument("--verify-sdist", type=Path)
    parser.add_argument("--installed-smoke", type=Path)
    parser.add_argument("--installed-suite", type=Path)
    parser.add_argument("--oracle", type=Path)
    parser.add_argument("--compatibility-smoke", choices=["no-detector", "urllib3-1"])
    parser.add_argument("--backend", choices=["default", "trial"])
    parser.add_argument("checkout", nargs="?", type=Path, default=ROOT)
    arguments = parser.parse_args()
    if arguments.verify_manifest:
        if arguments.source_commit is None or arguments.matrix_result is None:
            parser.error(
                "--verify-manifest requires --source-commit and --matrix-result"
            )
        metadata = (
            json.loads(arguments.artifact_metadata.read_text())
            if arguments.artifact_metadata
            else None
        )
        print(
            json.dumps(
                build_publish_manifest(
                    arguments.verify_manifest,
                    source_commit=arguments.source_commit,
                    matrix_result=arguments.matrix_result,
                    artifact_metadata=metadata,
                ),
                sort_keys=True,
            )
        )
        return 0
    if arguments.verify_release_set:
        if arguments.manifest is None or arguments.source_commit is None:
            parser.error("--verify-release-set requires --manifest and --source-commit")
        verify_release_set(
            arguments.verify_release_set,
            arguments.manifest,
            arguments.source_commit,
        )
        return 0
    if arguments.verify_wheel:
        if arguments.source_commit is None:
            parser.error("--verify-wheel requires --source-commit")
        verify_wheel(arguments.verify_wheel, arguments.source_commit)
        return 0
    if arguments.verify_sdist:
        if arguments.source_commit is None:
            parser.error("--verify-sdist requires --source-commit")
        verify_sdist(arguments.verify_sdist, arguments.source_commit)
        return 0
    if arguments.installed_smoke:
        run_installed_smoke(
            arguments.installed_smoke,
            editable=False,
            checkout=arguments.checkout.resolve(),
        )
        return 0
    if arguments.installed_suite:
        if arguments.oracle is None:
            parser.error("--installed-suite requires --oracle")
        run_installed_suite(
            arguments.installed_suite,
            arguments.oracle,
            arguments.checkout.resolve(),
        )
        return 0
    if arguments.compatibility_smoke and arguments.backend:
        compatibility_smoke(arguments.compatibility_smoke, arguments.backend)
        return 0
    parser.error("one verification mode is required")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())

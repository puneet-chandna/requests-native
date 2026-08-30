from __future__ import annotations

import argparse
import email.parser
import hashlib
import importlib
import json
import os
import re
import shutil
import subprocess
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
    "Development Status :: 5 - Production/Stable",
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
    "Documentation": "https://requests.readthedocs.io",
    "Source": "https://github.com/psf/requests",
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
assert requests._requests_rust.backend_name() == "requests-rust"
extension_source = Path(requests._requests_rust.__file__).resolve()
if os.environ["REQUESTS_EDITABLE"] == "1":
    assert extension_source.is_relative_to(checkout / "src"), extension_source
else:
    assert not extension_source.is_relative_to(checkout), extension_source
distribution = metadata.distribution("requests")
assert distribution.version == requests.__version__
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
try:
    default_response = default_session.get(url)
    assert default_response.content == b"native"
    assert type(default_response.raw).__module__.startswith("urllib3")
    assert server.requests == 1
finally:
    default_session.close()
assert extension._public_facade_pump_trial("snapshot")["submission_ids"] == []
assert extension._runtime_submission_trial("snapshot")["events"] == []

session = requests.Session()
session.trust_env = False
original = adapters._HTTP_ADAPTER_COMPAT_SEND
adapters._HTTP_ADAPTER_COMPAT_SEND = lambda *args, **kwargs: (_ for _ in ()).throw(AssertionError("Python fallback"))
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
    wheels = sorted(DIST.glob("requests-*.whl"))
    sdists = sorted(DIST.glob("requests-*.tar.gz"))
    assert len(wheels) == 1, wheels
    assert len(sdists) == 1, sdists
    return wheels[0], sdists[0]


def test_project_metadata_declares_license_files_dependencies_and_extras() -> None:
    project = load_toml(ROOT / "pyproject.toml")["project"]
    assert project["license"] == "Apache-2.0"
    assert project["license-files"] == ["LICENSE", "NOTICE"]
    assert project["dependencies"] == EXPECTED_DEPENDENCIES
    assert project["optional-dependencies"] == EXPECTED_EXTRAS
    assert project["classifiers"] == EXPECTED_CLASSIFIERS
    assert project["urls"] == EXPECTED_URLS


def test_wheel_workflow_builds_and_smokes_the_complete_supported_matrix() -> None:
    workflow = load_workflow("wheels.yml")
    triggers = workflow.get("on", workflow.get(True))
    assert set(triggers) == {
        "workflow_call",
        "workflow_dispatch",
        "push",
        "pull_request",
    }
    job = workflow["jobs"]["wheels"]
    assert job["strategy"]["matrix"] == {
        "python": PYTHONS,
        "os": SYSTEMS,
        "exclude": [{"python": "pypy-3.11", "os": "windows-latest"}],
    }
    assert triggers["push"]["branches"] == ["main"]
    ordered_names = [step.get("name") for step in job["steps"]]
    steps = {step.get("name"): step for step in job["steps"]}
    assert "python -m maturin build" in steps["Build wheel"]["run"]
    assert "--manylinux 2_34" in steps["Build wheel"]["run"]
    assert "--verify-wheel" in steps["Verify exact wheel contents"]["run"]
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
    assert steps["Upload wheel"]["with"]["if-no-files-found"] == "error"


def test_test_and_lint_workflows_cover_python_default_and_explicit_rust_trial() -> None:
    tests = load_workflow("run-tests.yml")["jobs"]
    for name in ("build", "no_chardet", "urllib3"):
        steps = {step.get("name"): step for step in tests[name]["steps"]}
        ordered_names = [step.get("name") for step in tests[name]["steps"]]
        run_steps = "\n".join(str(step.get("run", "")) for step in tests[name]["steps"])
        assert "Default Python backend" in run_steps
        assert "Explicit Rust trial backend" in run_steps
        oracle = steps["Check out frozen Python oracle"]
        assert oracle["with"] == {
            "repository": "psf/requests",
            "ref": FROZEN_ORACLE_COMMIT,
            "path": "frozen-oracle",
            "persist-credentials": False,
        }
        assert (
            ordered_names.index("Run default Python backend tests")
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

    lint_runs = "\n".join(
        str(step.get("run", ""))
        for step in load_workflow("lint.yml")["jobs"]["lint"]["steps"]
    )
    assert "cargo fmt --all -- --check" in lint_runs
    assert "cargo clippy -p requests --all-targets -- -D warnings" in lint_runs


def test_installed_smoke_checks_the_package_file_in_its_distribution() -> None:
    assert "metadata.packages_distributions" not in INSTALLED_SMOKE
    assert 'distribution.locate_file("requests") / "__init__.py"' in INSTALLED_SMOKE


def test_extension_guards_cpython_function_abi_symbols_from_pypy() -> None:
    models = (ROOT / "crates/requests-python/src/models.rs").read_text()
    cookies = (ROOT / "crates/requests-python/src/cookies.rs").read_text()
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
    jobs = load_workflow("publish.yml")["jobs"]
    assert jobs["wheels"]["uses"] == "./.github/workflows/wheels.yml"
    sdist_steps = jobs["sdist"]["steps"]
    sdist_by_name = {step.get("name"): step for step in sdist_steps}
    sdist_names = [step.get("name") for step in sdist_steps]
    assert "--verify-sdist" in sdist_by_name["Verify exact sdist contents"]["run"]
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
    runs = "\n".join(str(step.get("run", "")) for step in manifest["steps"])
    assert "--verify-manifest" in runs
    assert manifest["steps"][-1]["with"]["name"] == "release-dist"
    for name in ("publish", "publish-test-pypi"):
        download = jobs[name]["steps"][0]
        assert download["with"]["name"] == "release-dist"
        assert jobs[name]["permissions"] == {"id-token": "write"}
        assert all(
            "checkout" not in step.get("uses", "") for step in jobs[name]["steps"]
        )
    assert "github.ref == 'refs/heads/main'" in jobs["publish-test-pypi"]["if"]


def verify_wheel(wheel: Path) -> None:
    with zipfile.ZipFile(wheel) as archive:
        members = archive.namelist()
        metadata_name = next(
            name for name in members if name.endswith(".dist-info/METADATA")
        )
        metadata = email.parser.BytesParser().parsebytes(archive.read(metadata_name))
        expected_python = {
            f"requests/{path.name}" for path in (ROOT / "src/requests").glob("*.py")
        }
        assert len(expected_python) == 20
        extension = [
            name
            for name in members
            if re.fullmatch(r"requests/_requests_rust.*\.(so|pyd|dylib)", name)
        ]
        assert len(extension) == 1
        dist_info = metadata_name.removesuffix("METADATA")
        sbom_name = f"{dist_info}sboms/requests-python.cyclonedx.json"
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
        assert set(members) == expected_members
        assert (
            archive.read(f"{dist_info}licenses/LICENSE")
            == (ROOT / "LICENSE").read_bytes()
        )
        assert (
            archive.read(f"{dist_info}licenses/NOTICE")
            == (ROOT / "NOTICE").read_bytes()
        )
        sbom = json.loads(archive.read(sbom_name))
        assert sbom["bomFormat"] == "CycloneDX"
        assert sbom["metadata"]["component"]["name"] == "requests-python"

    assert metadata["Name"] == "requests"
    assert metadata["Version"] == "2.34.2"
    assert metadata["Summary"] == "Python HTTP for Humans."
    assert metadata["Requires-Python"] == ">=3.10"
    assert metadata["License-Expression"] == "Apache-2.0"
    assert metadata.get_all("License-File") == ["LICENSE", "NOTICE"]
    assert metadata["Author-email"] == "Kenneth Reitz <me@kennethreitz.org>"
    assert metadata["Maintainer-email"] == (
        "Ian Stapleton Cordasco <graffatcolmingov@gmail.com>, "
        "Nate Prewitt <nate.prewitt@gmail.com>"
    )
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


def verify_sdist(sdist: Path) -> None:
    with tarfile.open(sdist, "r:gz") as archive:
        names = {
            member.name.split("/", 1)[-1]
            for member in archive.getmembers()
            if member.isfile()
        }
    semantic = {
        "HISTORY.md",
        "LICENSE",
        "MANIFEST.in",
        "NOTICE",
        "README.md",
        "pyproject.toml",
        "requirements-dev.txt",
        "setup.py",
    }
    semantic |= {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "src/requests").iterdir()
        if path.is_file()
        and (path.suffix in {".py", ".pyi"} or path.name == "py.typed")
    }
    semantic |= {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "tests").rglob("*.py")
        if "__pycache__" not in path.parts
    }
    semantic |= {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "tests/certs").rglob("*")
        if path.is_file()
    }
    ca_files = [path.name for path in (ROOT / "tests/certs/expired/ca").iterdir()]
    semantic |= {
        f"tests/certs/{alias}/{name}"
        for alias in ("mtls/client/ca", "valid/ca")
        for name in ca_files
    }
    assert len(semantic) == 80
    rust_inputs = {"Cargo.toml", "Cargo.lock"} | {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "crates").rglob("*")
        if path.is_file()
        and "__fuzz__" not in path.parts
        and (path.name == "Cargo.toml" or path.suffix == ".rs")
    }
    assert names == semantic | rust_inputs | {"PKG-INFO"}


def test_built_wheel_and_sdist_have_complete_clean_inventory_and_metadata() -> None:
    wheel, sdist = artifact_paths()
    verify_wheel(wheel)
    verify_sdist(sdist)


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
        # This is the only selected test that reads rewrite source; artifacts do not
        # contain source or Rust crates, so the installed suite excludes it by name.
        subprocess.run(
            [
                str(python),
                "-m",
                "pytest",
                "-q",
                "tests",
                "tests_differential/test_import_api.py",
                "tests_differential/test_public_types.py",
                "tests_differential/test_property_boundaries.py",
                "tests_rust/test_backend_boundary.py",
                "-k",
                "not task17_red_outer_pump_runtime_and_static_call_graph_share_adapter_leaf",
            ],
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
    if distribution != "requests-2.34.2":
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
            assert type(response.raw).__module__ == "requests._requests_rust"
        else:
            response = session.get(f"http://127.0.0.1:{server.server_port}/")
            assert type(response.raw).__module__.startswith("urllib3")
        assert response.content == b"ok"
    finally:
        session.close()
        server.shutdown()
        server.server_close()
        worker.join(5)


def verify_manifest(directory: Path) -> dict[str, str]:
    wheels = sorted(directory.rglob("requests-*.whl"))
    sdists = sorted(directory.rglob("requests-*.tar.gz"))
    evidence = sorted(directory.rglob("free-threaded-evidence-*.json"))
    files = {path for path in directory.rglob("*") if path.is_file()}
    if len(sdists) != 1:
        raise ValueError(f"expected one sdist, found {len(sdists)}")
    if sdists[0].name != "requests-2.34.2.tar.gz":
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
        sibling_wheels = list(path.parent.glob("requests-*.whl"))
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


def write_complete_manifest_fixture(directory: Path) -> None:
    (directory / "requests-2.34.2.tar.gz").touch()
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
        (artifact / f"requests-2.34.2-{python_tag}-{platform}.whl").touch()
        if python == "3.14t":
            (artifact / f"free-threaded-evidence-{system}.json").write_text(
                '{"Py_GIL_DISABLED": 1, "after_import": true, "before_import": false}\n'
            )


def test_publish_manifest_accepts_only_the_complete_unique_matrix(
    tmp_path: Path,
) -> None:
    write_complete_manifest_fixture(tmp_path)
    manifest = verify_manifest(tmp_path)
    assert len(manifest) == 24

    next(tmp_path.rglob("requests-*cp310-cp310*linux*.whl")).unlink()
    with unittest.TestCase().assertRaisesRegex(ValueError, "wheel matrix mismatch"):
        verify_manifest(tmp_path)

    duplicate = tmp_path / "duplicate"
    duplicate.mkdir()
    source = next(tmp_path.rglob("requests-*cp311-cp311*linux*.whl"))
    (duplicate / source.name).touch()
    with unittest.TestCase().assertRaisesRegex(
        ValueError, "duplicate wheel matrix key"
    ):
        verify_manifest(tmp_path)


def test_wheel_key_rejects_wrong_version_abi_and_platform() -> None:
    cases = [
        ("requests-9.9.9-cp310-cp310-manylinux_2_17_x86_64.whl", "project/version"),
        ("requests-2.34.2-cp310-abi3-manylinux_2_17_x86_64.whl", "Python wheel tag"),
        ("requests-2.34.2-py3-none-any.whl", "Python wheel tag"),
        ("requests-2.34.2-cp310-cp310-any.whl", "platform wheel tag"),
        (
            "requests-2.34.2-cp310-cp310-linux_x86_64.whl",
            "platform wheel tag",
        ),
        (
            "requests-2.34.2-cp310-cp310-musllinux_1_2_x86_64.whl",
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
        print(json.dumps(verify_manifest(arguments.verify_manifest), sort_keys=True))
        return 0
    if arguments.verify_wheel:
        verify_wheel(arguments.verify_wheel)
        return 0
    if arguments.verify_sdist:
        verify_sdist(arguments.verify_sdist)
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

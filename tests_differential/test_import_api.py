from __future__ import annotations

import ast
from dataclasses import dataclass
from pathlib import Path
from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case


@dataclass(frozen=True)
class ImportApiCase:
    case_id: str
    obligation: str
    source: str


_HELPERS = r"""
import contextlib
import importlib
import inspect
import io
import json
import logging
import os
import subprocess
import sys
import tempfile
import warnings
from pathlib import Path

import requests
import requests.api as api_module
import requests.help as help_module
import requests.models as models_module
import requests.sessions as sessions_module

def type_name(value):
    cls = value if isinstance(value, type) else type(value)
    return [cls.__module__, cls.__qualname__]

def error_record(callback, marker=None):
    try:
        returned = callback()
    except BaseException as error:
        return {
            "type": type_name(error), "arg_types": [type_name(arg) for arg in error.args],
            "identity": error is marker, "traceback": error.__traceback__ is not None,
            "cause": None if error.__cause__ is None else type_name(error.__cause__),
            "context": None if error.__context__ is None else type_name(error.__context__),
        }
    return {"returned": returned}

def supervised_run(label, command, environment):
    try:
        return subprocess.run(
            command, text=True, capture_output=True, timeout=8, check=False,
            env=environment,
        )
    except subprocess.TimeoutExpired as error:
        stdout = error.stdout.decode(errors="replace") if isinstance(error.stdout, bytes) else (error.stdout or "")
        stderr = error.stderr.decode(errors="replace") if isinstance(error.stderr, bytes) else (error.stderr or "")
        raise AssertionError(
            f"{label} timed out after 8 seconds; "
            f"stdout_tail={stdout[-512:]!r}; stderr_tail={stderr[-512:]!r}"
        ) from error
"""


_NAMESPACE = r"""
functions = ("request", "get", "options", "head", "post", "put", "patch", "delete")
metadata = ("__version__", "__build__", "__title__", "__description__", "__url__", "__author__", "__author_email__", "__license__", "__copyright__", "__cake__")
visible = functions + ("session", "Session", "Request", "PreparedRequest", "Response", "codes", "packages", "utils")
result = {
    "all": list(requests.__all__), "all_type": type_name(requests.__all__),
    "visible_names": [name for name in visible if name in requests.__dict__],
    "root_api_identity": [[name, getattr(requests, name) is getattr(api_module, name)] for name in functions],
    "root_type_identity": [requests.Request is models_module.Request, requests.PreparedRequest is models_module.PreparedRequest, requests.Response is models_module.Response, requests.Session is sessions_module.Session, requests.session is sessions_module.session],
    "signatures": [[name, str(inspect.signature(getattr(requests, name)))] for name in functions + ("session",)],
    "api_signatures": [[name, str(inspect.signature(getattr(api_module, name)))] for name in functions],
    "metadata": [[name, getattr(requests, name)] for name in metadata],
    "aliases": [requests.packages is importlib.import_module("requests.packages"), requests.utils is importlib.import_module("requests.utils"), requests.codes is importlib.import_module("requests.status_codes").codes],
    "invalid": [[name, error_record(call)] for name, call in (
        ("request", lambda: requests.request()), ("get", lambda: requests.get()),
        ("post", lambda: requests.post()), ("session", lambda: requests.session(1)),
    )],
}
"""


_FORWARDING = r"""
events = []; sentinel = object(); original = api_module.request
def recorder(*args, **kwargs):
    events.append([list(args), [[key, value] for key, value in kwargs.items()]])
    return sentinel
api_module.request = recorder
try:
    returned = [
        requests.get("u-get", params=[("p", "1")], marker="get"),
        requests.options("u-options", marker="options"),
        requests.head("u-head", marker="head"), requests.head("u-head-explicit", allow_redirects=True),
        requests.post("u-post", data="data", json="json", marker="post"),
        requests.put("u-put", data="data", marker="put"), requests.patch("u-patch", data="data", marker="patch"),
        requests.delete("u-delete", marker="delete"),
    ]
finally: api_module.request = original
result = {"all_identity": all(item is sentinel for item in returned), "events": events}
"""


_ONE_SHOT = r"""
def run_scenario(stage):
    events = []; marker = KeyboardInterrupt("stop-" + stage); response = object()
    class DynamicSession:
        def __init__(self):
            events.append("construct")
            if stage == "construct": raise marker
        def __enter__(self):
            events.append("enter")
            if stage == "enter": raise marker
            return self
        def request(self, *args, **kwargs):
            events.append(["request", list(args), [[key, value] for key, value in kwargs.items()]])
            if stage == "request": raise marker
            return response
        def close(self):
            events.append("close")
            if stage == "close": raise marker
        def __exit__(self, *args):
            events.append(["exit", args[0] is None, args[1] is marker]); self.close()
    original = api_module.sessions.Session; api_module.sessions.Session = DynamicSession
    try: observed = error_record(lambda: api_module.request("GET", "mock://one-shot", token=stage), marker)
    finally: api_module.sessions.Session = original
    return [stage, events, observed, observed.get("returned") is response]
scenarios = [run_scenario(stage) for stage in ("success", "construct", "enter", "request", "close")]
events = []; request_marker = KeyboardInterrupt("request-error"); exit_marker = SystemExit("exit-error")
class PrecedenceSession:
    def __enter__(self): events.append("enter"); return self
    def request(self, **kwargs): events.append("request"); raise request_marker
    def __exit__(self, *args): events.append(["exit", args[1] is request_marker]); raise exit_marker
original = api_module.sessions.Session; api_module.sessions.Session = PrecedenceSession
try: precedence = error_record(lambda: api_module.request("GET", "mock://precedence"), exit_marker)
finally: api_module.sessions.Session = original
result = {"scenarios": scenarios, "precedence": [events, precedence]}
"""


_RELOAD_HELP = r"""
logger = logging.getLogger("requests"); before_handlers = [type_name(item) for item in logger.handlers]
before_filters = [[item[0], None if item[2] is None else type_name(item[2]), item[3], item[4]] for item in warnings.filters]
before_all = requests.__all__; reloaded = importlib.reload(requests)
after_handlers = [type_name(item) for item in logger.handlers]
after_filters = [[item[0], None if item[2] is None else type_name(item[2]), item[3], item[4]] for item in warnings.filters]
buffer = io.StringIO()
with contextlib.redirect_stdout(buffer): returned = help_module.main()
main_text = buffer.getvalue(); main_document = json.loads(main_text)
completed = supervised_run("requests.help CLI", [sys.executable, "-m", "requests.help"], dict(os.environ))
cli_document = json.loads(completed.stdout) if completed.returncode == 0 else None
result = {
    "reload_identity": reloaded is requests, "all_equal": requests.__all__ == before_all, "all_replaced": requests.__all__ is not before_all,
    "handlers": [before_handlers, after_handlers], "filters": [before_filters, after_filters],
    "main": [returned, main_text.endswith("\n"), main_document],
    "cli": [completed.returncode, completed.stderr, completed.stdout.endswith("\n"), cli_document],
}
"""


_OPTIONAL_HELP = r"""
sitecustomize = '''
import importlib.abc, json, os, sys, types
rules = json.loads(os.environ.get("TASK17_OPTIONAL_RULES", "{}"))
class Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname in rules.get("broken", []): raise RuntimeError("task17-broken:" + fullname)
        if fullname in rules.get("blocked", []): raise ImportError("task17-blocked:" + fullname)
        return None
sys.meta_path.insert(0, Finder())
for name in rules.get("stubs", []):
    module = types.ModuleType(name)
    if name == "chardet": module.__version__ = "5.2.0"
    elif name == "charset_normalizer": module.__version__ = "3.3.2"
    elif name == "cryptography": module.__version__ = "42.0.0"
    elif name == "OpenSSL":
        module.__version__ = "24.0.0"; module.SSL = types.SimpleNamespace(OPENSSL_VERSION_NUMBER=0x30200000)
    sys.modules[name] = module
'''
scenarios = (
    ("missing-chardet", {"blocked": ["chardet"], "stubs": ["charset_normalizer"]}),
    ("missing-charset", {"blocked": ["charset_normalizer"], "stubs": ["chardet"]}),
    ("missing-pyopenssl", {"blocked": ["urllib3.contrib.pyopenssl"], "stubs": ["chardet", "charset_normalizer"]}),
    ("stub-openssl-cryptography", {"stubs": ["chardet", "charset_normalizer", "urllib3.contrib.pyopenssl", "OpenSSL", "cryptography"]}),
    ("broken-chardet", {"broken": ["chardet"]}),
    ("broken-charset", {"broken": ["charset_normalizer"]}),
    ("broken-pyopenssl", {"broken": ["urllib3.contrib.pyopenssl"], "stubs": ["chardet", "charset_normalizer"]}),
    ("broken-cryptography", {"broken": ["cryptography"], "stubs": ["chardet", "charset_normalizer", "urllib3.contrib.pyopenssl"]}),
)
records = []
with tempfile.TemporaryDirectory() as directory:
    Path(directory, "sitecustomize.py").write_text(sitecustomize)
    for name, rules in scenarios:
        environment = dict(os.environ); environment["PYTHONPATH"] = directory + os.pathsep + environment.get("PYTHONPATH", "")
        environment["TASK17_OPTIONAL_RULES"] = json.dumps(rules, sort_keys=True)
        mode_rows = []
        for mode, command in (
            ("help.main", [sys.executable, "-c", "import requests.help as module; module.main()"]),
            ("python-m", [sys.executable, "-m", "requests.help"]),
        ):
            completed = supervised_run(name + ":" + mode, command, environment)
            if completed.returncode == 0:
                document = json.loads(completed.stdout)
                selected = {key: document[key] for key in ("using_pyopenssl", "using_charset_normalizer", "pyOpenSSL", "chardet", "charset_normalizer", "cryptography")}
                mode_rows.append([mode, 0, completed.stdout.endswith("\n"), selected, ""])
            else:
                last = completed.stderr.rstrip().splitlines()[-1]
                mode_rows.append([mode, completed.returncode != 0, False, None, last])
        records.append([name, mode_rows])
result = records
"""


IMPORT_API_CASES = (
    ImportApiCase("I01", "explicit-namespace-metadata-alias-signature", _NAMESPACE),
    ImportApiCase("I02", "root-defaults-forwarding", _FORWARDING),
    ImportApiCase("I03", "python-one-shot-lifecycle-precedence", _ONE_SHOT),
    ImportApiCase("I04", "reload-logging-warnings-help-main-cli", _RELOAD_HELP),
    ImportApiCase("I05", "optional-dependency-help-main-cli", _OPTIONAL_HELP),
)


def case_source(case: ImportApiCase) -> str:
    return dedent(_HELPERS + case.source)


@pytest.mark.parametrize("case", IMPORT_API_CASES, ids=lambda case: case.case_id)
def test_import_api_frozen_oracle_contract(case: ImportApiCase) -> None:
    oracle = run_oracle_case({"source": case_source(case)})
    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""


def test_import_api_oracle_observations_are_repeatable() -> None:
    for case in IMPORT_API_CASES:
        payload = {"source": case_source(case)}
        assert run_oracle_case(payload) == run_oracle_case(payload), case.case_id


@pytest.mark.parametrize("case", IMPORT_API_CASES, ids=lambda case: case.case_id)
def test_import_api_already_compatible_surface_matches_oracle(
    case: ImportApiCase,
) -> None:
    payload = {"source": case_source(case)}
    assert run_rewrite_case(payload) == run_oracle_case(payload)


_ROOT_TRIAL = r"""
from types import SimpleNamespace
import requests
import requests.api as api_module
from requests.models import Response
events = []; extension = requests._requests_rust; original = extension._session_facade_trial
response = Response(); response.status_code = 200; response.url = "mock://root"; response._content = b"root-native"; response._content_consumed = True; response.raw = SimpleNamespace(_original_response=None); response.history = []
def semantic(session, operation, args, kwargs):
    events.append(["session", operation, list(args), list(kwargs)])
    if operation == "request": return response
    return NotImplemented
extension._session_facade_trial = semantic
old_factory = api_module.sessions.Session
def dynamic_factory(): events.append(["factory"]); return old_factory()
api_module.sessions.Session = dynamic_factory
old_trials = {name: getattr(extension, name) for name in ("_adapter_send_trial", "_session_pipeline_trial", "_session_runtime_trial") if hasattr(extension, name)}
for name in old_trials: setattr(extension, name, lambda *args, _name=name, **kwargs: (_ for _ in ()).throw(AssertionError("forbidden:" + _name)))
try:
    with requests._rust_public_trial(): returned = requests.get("mock://root", token="value")
finally:
    extension._session_facade_trial = original; api_module.sessions.Session = old_factory
    for name, value in old_trials.items(): setattr(extension, name, value)
result = SimpleNamespace(identity=returned is response, content=returned.content.decode(), events=events)
"""


def test_task17_red_root_one_shot_uses_explicit_semantic_session_trial() -> None:
    run = run_rewrite_case({"source": dedent(_ROOT_TRIAL)})
    assert run.observations["exception"] is None
    result = run.observations["result"]["public_state"]
    assert result["identity"] is True and result["content"] == "root-native"
    assert result["events"][0] == ["factory"]
    assert [row[1] for row in result["events"] if row[0] == "session"] == [
        "construct",
        "enter",
        "request",
        "exit",
        "close",
    ]


def test_task17_import_inventory_reuses_existing_coverage() -> None:
    root = Path(__file__).resolve().parents[1]
    expected = {
        root / "tests_differential/test_compatibility_values.py": {
            "test_certs_version_root_exports_filters_logger_and_compatibility_checks",
            "test_root_import_dependency_warning_has_exact_category_message_and_filtering",
            "test_packages_preserve_loaded_alias_identity_and_import_timing",
            "test_broken_optional_imports_propagate_non_import_errors",
        },
        root / "tests/test_help.py": {
            "test_system_ssl",
            "test_idna_without_version_attribute",
            "test_idna_with_version_attribute",
        },
    }
    for path, required in expected.items():
        tree = ast.parse(path.read_text())
        functions = {
            node.name for node in tree.body if isinstance(node, ast.FunctionDef)
        }
        assert required.issubset(functions)


def test_task17_import_api_inventory_and_subprocess_guards_are_consolidated() -> None:
    assert tuple(case.case_id for case in IMPORT_API_CASES) == (
        "I01",
        "I02",
        "I03",
        "I04",
        "I05",
    )
    for case in IMPORT_API_CASES:
        tree = ast.parse(case_source(case))
        assignments = [
            node
            for node in tree.body
            if isinstance(node, ast.Assign)
            and any(
                isinstance(target, ast.Name) and target.id == "result"
                for target in node.targets
            )
        ]
        assert len(assignments) == 1
        assert not any(
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "sleep"
            for node in ast.walk(tree)
        )
    optional = ast.parse(case_source(IMPORT_API_CASES[-1]))
    calls = [
        node
        for node in ast.walk(optional)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr == "run"
    ]
    assert len(calls) == 1
    keywords = {keyword.arg for keyword in calls[0].keywords}
    assert {"timeout", "check", "capture_output"}.issubset(keywords)
    source = case_source(IMPORT_API_CASES[-1])
    assert all(
        marker in source for marker in ("timed out after", "stdout_tail", "stderr_tail")
    )
    assert all(
        name in source
        for name in (
            "chardet",
            "charset_normalizer",
            "urllib3.contrib.pyopenssl",
            "OpenSSL",
            "cryptography",
            '"-m", "requests.help"',
        )
    )
    assert all(
        name in _ROOT_TRIAL
        for name in (
            "_rust_public_trial",
            "_session_facade_trial",
            "_adapter_send_trial",
            "_session_pipeline_trial",
            "_session_runtime_trial",
        )
    )

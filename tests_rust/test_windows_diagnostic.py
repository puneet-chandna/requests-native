import importlib.util
import json
import sys
from pathlib import Path

import requests

ROOT = Path(__file__).resolve().parents[1]


def diagnostic():
    path = ROOT / "scripts/diagnose_windows_testserver.py"
    assert path.is_file(), "the bounded Windows diagnostic helper is missing"
    spec = importlib.util.spec_from_file_location("windows_diagnostic", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_diagnostic_observes_server_without_replacing_client(tmp_path):
    from tests.testserver.server import Server

    helper = diagnostic()
    output = tmp_path / "trace.jsonl"
    client_methods = (
        requests.get,
        requests.Session.send,
        requests.adapters.HTTPAdapter.send,
    )
    trace = helper.Trace(output)
    trace.node = "tests/test_testserver.py::TestTestServer::test_text_response"
    trace.start()
    try:
        with Server.basic_response_server() as (host, port):
            response = requests.get(f"http://{host}:{port}")
            assert response.status_code == 200
            assert type(response.raw).__name__ == "NativeAdapterRaw"
    finally:
        trace.stop()
    assert client_methods == (
        requests.get,
        requests.Session.send,
        requests.adapters.HTTPAdapter.send,
    )
    assert Server.WAIT_EVENT_TIMEOUT == 5
    records = [json.loads(line) for line in output.read_text().splitlines()]
    assert any(row["boundary"] == "listener_ready" for row in records)
    assert any(
        row["boundary"] == "select" and row["event"] == "c_return" for row in records
    )
    assert any(row["boundary"] == "_close_server_sock_ignore_errors" for row in records)
    assert any(row["boundary"] in {"request", "send", "get"} for row in records)
    assert any(row["boundary"] == "_trial_http_adapter_send" for row in records)
    assert all(
        "node" in row and "thread" in row and "time_ns" in row for row in records
    )
    assert str(ROOT) not in output.read_text()


def test_diagnostic_runs_both_implementations_after_first_failure(
    tmp_path, monkeypatch
):
    helper = diagnostic()
    calls = []

    def run(command, **kwargs):
        calls.append((command, kwargs))
        return __import__("subprocess").CompletedProcess(command, len(calls) == 1)

    monkeypatch.setattr(helper.subprocess, "run", run)
    targets = [
        ("candidate", Path(sys.executable), ROOT),
        ("oracle", Path(sys.executable), ROOT),
    ]
    assert helper.run_suites(targets, tmp_path) == 1
    assert len(calls) == 2
    for (command, options), label in zip(calls, ("candidate", "oracle")):
        assert command[-1] == label
        assert options["cwd"] == tmp_path / label
        assert options["timeout"] == 300
        assert options["env"]["PYTHONDONTWRITEBYTECODE"] == "1"
        assert "PYTHONPATH" not in options["env"]
        assert (options["cwd"] / "tests/test_testserver.py").is_file()
    assert json.loads((tmp_path / "results.json").read_text()) == {
        "candidate": 1,
        "oracle": 0,
    }


def test_windows_diagnostic_workflow_cannot_start_a_matrix_or_publish():
    import re

    import yaml

    path = ROOT / ".github/workflows/diagnose-windows-testserver.yml"
    assert path.is_file(), "the isolated manual diagnostic workflow is missing"
    workflow = yaml.load(path.read_text(), Loader=yaml.BaseLoader)
    assert workflow["on"] == {"workflow_dispatch": ""}
    assert workflow["permissions"] == {"contents": "read"}
    assert list(workflow["jobs"]) == ["diagnose"]
    job = workflow["jobs"]["diagnose"]
    assert job["runs-on"] == "windows-latest"
    assert job["timeout-minutes"] == "20"
    assert "strategy" not in job
    actions = [step for step in job["steps"] if "uses" in step]
    assert all(
        re.fullmatch(
            r"actions/(checkout|setup-python|upload-artifact)@[0-9a-f]{40}",
            step["uses"],
        )
        for step in actions
    )
    upload = [step for step in actions if "upload-artifact@" in step["uses"]]
    assert len(upload) == 1
    assert upload[0]["if"] == "always()"
    assert "wheelhouse" not in upload[0]["with"]["path"]

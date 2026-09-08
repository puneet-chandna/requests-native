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
        for event, result in (
            ("client_tcp_connected", "complete"),
            ("client_tls_end", "complete"),
            ("client_plaintext_write_result", "accepted"),
            ("client_plaintext_flush_result", "complete"),
        ):
            kwargs["stdout"].write(
                "NATIVE_TLS_TRACE "
                + json.dumps({"event": event, "result": result, "bytes": 1})
                + "\n"
            )
        return __import__("subprocess").CompletedProcess(command, len(calls) == 1)

    monkeypatch.setattr(helper.subprocess, "run", run)
    oracle_source = tmp_path / "oracle-source"
    (oracle_source / "tests").mkdir(parents=True)
    (oracle_source / "tests/test_testserver.py").touch()
    (oracle_source / "requirements-dev.txt").touch()
    (oracle_source / "src/requests").mkdir(parents=True)
    (oracle_source / "src/requests/__init__.py").touch()
    targets = [
        ("candidate", Path(sys.executable), ROOT),
        ("oracle", Path(sys.executable), oracle_source),
    ]
    assert helper.run_suites(targets, tmp_path) == 1
    assert len(calls) == 6
    for index, (command, options) in enumerate(calls):
        label = "candidate" if index < 3 else "oracle"
        assert command[4] == label
        assert options["cwd"].parent == tmp_path / label
        assert options["timeout"] == 60
        assert options["env"]["PYTHONDONTWRITEBYTECODE"] == "1"
        assert "PYTHONPATH" not in options["env"]
        assert (options["cwd"] / "tests/test_testserver.py").is_file()
    groups = [command[5:] for command, _ in calls[:3]]
    assert groups[0] == [
        "tests/test_requests.py::TestRequests::test_pyopenssl_redirect"
    ]
    assert groups[1] == [
        "tests/test_requests.py::TestRequests::test_auth_is_stripped_on_http_downgrade"
    ]
    assert groups[2] == groups[0] + groups[1]
    assert [command[5:] for command, _ in calls[3:]] == groups
    assert list(json.loads((tmp_path / "results.json").read_text()).values()) == [
        1,
        0,
        0,
        0,
        0,
        0,
    ]


def test_diagnostic_requires_actual_native_io_for_passing_candidate():
    helper = diagnostic()
    missing = helper.native_evidence("2 passed", 0)
    assert missing["sufficient"] is False
    assert missing["state"] == "no-native-connect-observed"
    failed = helper.native_evidence("failed before connect", 1)
    assert failed["state"] == "no-native-connect-observed"
    complete = "\n".join(
        "NATIVE_TLS_TRACE " + json.dumps({"event": event, "result": result, "bytes": 1})
        for event, result in (
            ("client_tcp_connected", "complete"),
            ("client_tls_end", "complete"),
            ("client_plaintext_write_result", "accepted"),
            ("client_plaintext_flush_result", "complete"),
        )
    )
    assert helper.native_evidence(complete, 0)["sufficient"] is True
    truncated = complete + '\nNATIVE_TLS_TRACE {"event":"trace_truncated"}'
    assert helper.native_evidence(truncated, 0)["sufficient"] is False
    malformed = helper.native_evidence(complete + '\nNATIVE_TLS_TRACE {"event":', 0)
    assert malformed["sufficient"] is False
    assert malformed["parse_errors"] == 1


def test_diagnostic_build_uses_canonical_copy_and_cleans_it(tmp_path, monkeypatch):
    helper = diagnostic()
    original = ROOT / "crates/requests/src/transport/mod.rs"
    original_bytes = original.read_bytes()
    patch = ROOT / "scripts/diagnose_windows_tls.patch"
    patch_bytes = patch.read_bytes()
    read_bytes = Path.read_bytes

    def read_crlf(path):
        return (
            patch_bytes.replace(b"\n", b"\r\n") if path == patch else read_bytes(path)
        )

    monkeypatch.setattr(Path, "read_bytes", read_crlf)
    run = helper.subprocess.run
    builds = []

    def capture_build(command, **kwargs):
        if len(command) > 1 and str(command[1]).endswith("build_release_wheel.py"):
            source = kwargs["cwd"]
            assert (
                "NATIVE_TLS_TRACE"
                in (source / "crates/requests/src/transport/mod.rs").read_text()
            )
            assert original.read_bytes() == original_bytes
            assert kwargs["env"]["SOURCE_DATE_EPOCH"].isdigit()
            builds.append(source)
            return __import__("subprocess").CompletedProcess(command, 0)
        return run(command, **kwargs)

    monkeypatch.setattr(helper.subprocess, "run", capture_build)
    helper.build_diagnostic(Path(sys.executable), tmp_path)
    assert len(builds) == 1
    assert not builds[0].exists()
    assert original.read_bytes() == original_bytes
    metadata = json.loads((tmp_path / "diagnostic-only.json").read_text())
    assert metadata["instrumented"] is True
    assert metadata["release_qualified"] is False
    assert (
        metadata["patch_sha256"]
        == __import__("hashlib").sha256(patch_bytes).hexdigest()
    )


def test_diagnostic_correlates_tls_plaintext_without_changing_deadline(tmp_path):
    import socket
    import ssl

    from httpbin import app
    from pytest_httpbin import certs, serve

    helper = diagnostic()
    output = tmp_path / "tls.jsonl"
    methods = (requests.get, requests.Session.send, requests.adapters.HTTPAdapter.send)
    trace = helper.Trace(output)
    trace.node = "tests/test_requests.py::TestRequests::test_pyopenssl_redirect"
    trace.start()
    try:
        with serve.SecureServer(application=app) as server:
            response = requests.get(server.url + "/get", verify=certs.where())
            assert response.status_code == 200
            assert type(response.raw).__name__ == "NativeAdapterRaw"
            context = ssl.create_default_context(cafile=certs.where())
            with socket.create_connection((server.host, server.port)) as raw:
                with context.wrap_socket(raw, server_hostname="localhost") as client:
                    client.sendall(b"GET /get HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    assert client.recv(4096)
    finally:
        trace.stop()
    assert methods == (
        requests.get,
        requests.Session.send,
        requests.adapters.HTTPAdapter.send,
    )
    rows = [json.loads(line) for line in output.read_text().splitlines()]
    complete = next(row for row in rows if row["boundary"] == "server_tls_complete")
    assert complete["timeout"] == 1.0
    assert complete["time_ns"] > 1_700_000_000_000_000_000
    reads = [
        row
        for row in rows
        if row["boundary"] == "server_plaintext_read" and row.get("bytes", 0) > 0
    ]
    assert reads
    assert reads[0]["local"] == complete["local"]
    assert reads[0]["peer"] == complete["peer"]
    assert any(row["boundary"] == "server_requestline_complete" for row in rows)
    client_write = next(
        row
        for row in rows
        if row["boundary"] == "oracle_ssl_sendall" and row["event"] == "call"
    )
    assert client_write["bytes"] > 0
    assert client_write["local"] and client_write["peer"]
    assert any(
        row["boundary"] == "oracle_tls_handshake" and row["event"] == "return"
        for row in rows
    )


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
    assert job["timeout-minutes"] == "12"
    assert "strategy" not in job
    assert all("timeout-minutes" not in step for step in job["steps"])
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

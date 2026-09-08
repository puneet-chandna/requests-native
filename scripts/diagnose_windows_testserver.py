"""Private, bounded test-server diagnostics; never modifies the Requests client."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import threading
import time
from pathlib import Path

TLS_NODES = (
    "tests/test_requests.py::TestRequests::test_pyopenssl_redirect",
    "tests/test_requests.py::TestRequests::test_auth_is_stripped_on_http_downgrade",
)


def socket_fields(sock):
    try:
        return {
            "local": list(sock.getsockname()),
            "peer": list(sock.getpeername()),
            "timeout": sock.gettimeout(),
        }
    except OSError:
        return {"socket_closed": True}


class Trace:
    def __init__(self, output):
        self.output = output.open("w", encoding="utf-8", buffering=1)
        self.lock = threading.Lock()
        self.node = ""

    def emit(self, boundary, event, **fields):
        record = dict(
            time_ns=time.time_ns(),
            monotonic_ns=time.monotonic_ns(),
            thread=threading.get_ident(),
            node=self.node,
            boundary=boundary,
            event=event,
            **fields,
        )
        with self.lock:
            if not self.output.closed:
                self.output.write(json.dumps(record, sort_keys=True) + "\n")

    def profile(self, frame, event, arg):
        if not self.node:
            return
        module = frame.f_globals.get("__name__", "")
        name = frame.f_code.co_name
        owner = frame.f_locals.get("self")
        if (
            module == "pytest_httpbin.serve"
            and name == "get_request"
            and event == "return"
        ):
            if isinstance(arg, tuple):
                self.emit("server_tls_complete", event, **socket_fields(arg[0]))
        elif (
            module == "ssl"
            and name in {"do_handshake", "send", "sendall"}
            and not getattr(owner, "server_side", True)
            and event in {"call", "return"}
        ):
            fields = socket_fields(owner)
            if name in {"send", "sendall"}:
                fields["bytes"] = (
                    len(frame.f_locals.get("data", b""))
                    if event == "call"
                    else arg
                    if isinstance(arg, int)
                    else 0
                )
            self.emit(
                "oracle_tls_handshake"
                if name == "do_handshake"
                else "oracle_ssl_" + name,
                event,
                **fields,
            )
        elif (
            module == "ssl"
            and name in {"do_handshake", "read"}
            and getattr(owner, "server_side", False)
        ):
            fields = socket_fields(owner)
            if event == "return":
                fields["bytes"] = (
                    arg
                    if isinstance(arg, int)
                    else len(arg)
                    if isinstance(arg, bytes)
                    else 0
                )
            boundary = (
                "server_tls_handshake"
                if name == "do_handshake"
                else "server_plaintext_read"
            )
            self.emit(boundary, event, **fields)
        elif (
            module == "pytest_httpbin.serve"
            and name == "handle"
            and event.startswith("c_")
            and getattr(arg, "__name__", "") == "readline"
        ):
            self.emit(
                "server_requestline_read", event, **socket_fields(owner.connection)
            )
        elif module == "http.server" and name == "parse_request" and event == "call":
            self.emit(
                "server_requestline_complete",
                event,
                bytes=len(owner.raw_requestline),
                **socket_fields(owner.connection),
            )
        elif module == "tests.testserver.server":
            owner = frame.f_locals.get("self")
            fields = {}
            if owner is not None:
                fields["server"] = id(owner)
                sock = getattr(owner, "server_sock", None)
                if sock is not None:
                    fields["fd"] = sock.fileno()
                    if name == "_handle_requests" and event == "call":
                        try:
                            fields["address"] = sock.getsockname()
                        except OSError:
                            fields["address"] = "closed"
                        fields["deadline_seconds"] = owner.WAIT_EVENT_TIMEOUT
            if event.startswith("c_"):
                if getattr(arg, "__name__", "") != "select":
                    return
                name = "select"
            elif name == "_handle_requests" and event == "call":
                name = "listener_ready"
            elif name == "_accept_connection" and event == "return":
                fields["accepted"] = arg is not None
            self.emit(name, event, **fields)
        elif module == "threading" and name == "invoke_excepthook" and event == "call":
            exception = sys.exc_info()[0]
            self.emit(
                "thread_exception",
                event,
                exception=exception.__name__ if exception else None,
            )
        elif event.startswith("c_"):
            owner_module = type(getattr(arg, "__self__", None)).__module__
            function_module = getattr(arg, "__module__", "") or ""
            if owner_module.startswith("requests") or function_module.startswith(
                "requests"
            ):
                self.emit(
                    getattr(arg, "__name__", "native"),
                    event,
                    module=function_module or owner_module,
                )
        elif (
            module in {"requests.api", "requests.sessions", "requests.adapters"}
            and name
            in {
                "get",
                "request",
                "prepare_request",
                "send",
                "resolve_redirects",
                "_session_facade_request",
                "_trial_http_adapter_send",
            }
        ) or (
            module in {"ssl", "http.server", "socketserver"}
            and name
            in {
                "read",
                "do_handshake",
                "handle_one_request",
                "get_request",
                "handle_error",
            }
        ):
            self.emit(name, event, module=module)

    def start(self):
        self.old_profile = sys.getprofile()
        self.old_thread_profile = threading.getprofile()
        sys.setprofile(self.profile)
        threading.setprofile(self.profile)

    def stop(self):
        self.node = ""
        sys.setprofile(self.old_profile)
        threading.setprofile(self.old_thread_profile)
        with self.lock:
            self.output.close()

    def pytest_runtest_logstart(self, nodeid, location):
        if "test_testserver.py" in nodeid or any(
            part in nodeid
            for part in ("pyopenssl_redirect", "http_downgrade", "https", "tls")
        ):
            self.node = nodeid
            self.emit(
                "test",
                "start",
                gil=sys._is_gil_enabled() if hasattr(sys, "_is_gil_enabled") else True,
            )

    def pytest_runtest_logreport(self, report):
        if self.node:
            self.emit(
                "test", report.when, outcome=report.outcome, duration=report.duration
            )

    def pytest_runtest_logfinish(self, nodeid, location):
        if self.node:
            self.emit("test", "finish")
        self.node = ""


def run_child(import_root, implementation, nodes=TLS_NODES):
    # The script and tests are copied outside both checkouts. Only the oracle
    # explicitly adds a source import root; candidate must use its installed wheel.
    gil_before = sys._is_gil_enabled() if hasattr(sys, "_is_gil_enabled") else True
    if implementation == "oracle":
        sys.path.insert(0, str(import_root))
    import requests

    expected = (
        import_root
        if implementation == "oracle"
        else Path(sysconfig.get_path("platlib"))
    )
    assert Path(requests.__file__).resolve().is_relative_to(expected.resolve())
    if implementation == "candidate":
        from requests import _requests_rust

        assert _requests_rust.backend_name() == "requests-native"
        assert (
            Path(_requests_rust.__file__).resolve().is_relative_to(expected.resolve())
        )
    else:
        assert not hasattr(requests, "_requests_rust")
    import pytest

    trace = Trace(Path("trace.jsonl"))
    trace.emit(
        "implementation",
        "verified",
        implementation=implementation,
        gil_before_import=gil_before,
        coverage="Python and visible C outer boundaries; not Rust internals",
        free_threaded=sysconfig.get_config_var("Py_GIL_DISABLED"),
        gil=sys._is_gil_enabled() if hasattr(sys, "_is_gil_enabled") else True,
    )
    trace.start()
    try:
        return int(
            pytest.main(
                ["-q", "-s", "-p", "no:cacheprovider", "--tb=short", *nodes],
                plugins=[trace],
            )
        )
    finally:
        trace.stop()


def native_evidence(log, returncode):
    rows = []
    parse_errors = 0
    for line in log.splitlines():
        if "NATIVE_TLS_TRACE " in line:
            try:
                rows.append(json.loads(line.split("NATIVE_TLS_TRACE ", 1)[1]))
            except json.JSONDecodeError:
                parse_errors += 1
    connected = any(row["event"] == "client_tcp_connected" for row in rows)
    tls = any(
        row["event"] == "client_tls_end" and row["result"] == "complete" for row in rows
    )
    written = any(
        row["event"]
        in {"client_plaintext_write_result", "client_plaintext_write_vectored_result"}
        and row["result"] == "accepted"
        and row["bytes"] > 0
        for row in rows
    )
    flushed = any(
        row["event"] == "client_plaintext_flush_result" and row["result"] == "complete"
        for row in rows
    )
    truncated = any(row["event"] == "trace_truncated" for row in rows)
    return {
        "sufficient": connected
        and tls
        and written
        and flushed
        and not truncated
        and not parse_errors,
        "parse_errors": parse_errors,
        "truncated": truncated,
        "state": "native-connect-observed"
        if connected
        else "no-native-connect-observed",
        "child_result": returncode,
        "events": rows,
    }


def run_suites(targets, output):
    # Reuse the release harness's allowlisted environment, including no bytecode.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from tests_rust.test_distribution import _clean_environment

    results = {}
    for implementation, python, source in targets:
        for index, nodes in enumerate(((TLS_NODES[0],), (TLS_NODES[1],), TLS_NODES)):
            suite = output / implementation / str(index)
            suite.mkdir(parents=True)
            shutil.copytree(source / "tests", suite / "tests")
            shutil.copy2(
                source / "requirements-dev.txt", suite / "requirements-dev.txt"
            )
            shutil.copy2(__file__, suite / "diagnostic.py")
            import_root = source
            if implementation == "oracle":
                import_root = suite / "oracle-src"
                shutil.copytree(source / "src/requests", import_root / "requests")
            environment = _clean_environment()
            environment.pop("REQUESTS_CHECKOUT", None)
            key = f"{implementation}-{index}"
            with (suite / "pytest.log").open("w", encoding="utf-8") as log:
                try:
                    completed = subprocess.run(
                        [
                            str(python),
                            "diagnostic.py",
                            "--child",
                            str(import_root),
                            implementation,
                            *nodes,
                        ],
                        cwd=suite,
                        env=environment,
                        stdout=log,
                        stderr=subprocess.STDOUT,
                        timeout=60,
                        check=False,
                    )
                    results[key] = int(completed.returncode)
                except subprocess.TimeoutExpired:
                    results[key] = "timeout-60s"
            if implementation == "candidate":
                evidence = native_evidence(
                    (suite / "pytest.log").read_text(encoding="utf-8"), results[key]
                )
                (suite / "native.json").write_text(
                    json.dumps(evidence) + "\n", encoding="utf-8"
                )
                if results[key] == 0 and not evidence["sufficient"]:
                    results[key] = "missing-native-io-evidence"
            (output / "results.json").write_text(
                json.dumps(results, sort_keys=True) + "\n", encoding="utf-8"
            )
    return int(any(result != 0 for result in results.values()))


def build_diagnostic(interpreter, output):
    """Patch only a disposable Git archive; never change the normal checkout."""
    import os

    root = Path(__file__).resolve().parents[1]
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="diagnostic-source-", dir=output
    ) as directory:
        staging = Path(directory)
        archive = staging / "source.tar"
        subprocess.run(
            ["git", "archive", "--format=tar", "--output", str(archive), "HEAD"],
            cwd=root,
            check=True,
        )
        source = staging / "source"
        source.mkdir()
        subprocess.run(["tar", "-xf", str(archive), "-C", str(source)], check=True)
        # Git archive supplies canonical source bytes even on CRLF checkouts;
        # normalize the checked-out patch too, without changing either original.
        patch_bytes = (
            (root / "scripts/diagnose_windows_tls.patch")
            .read_bytes()
            .replace(b"\r\n", b"\n")
        )
        patch = staging / "diagnostic.patch"
        patch.write_bytes(patch_bytes)
        patch_environment = dict(os.environ, GIT_CEILING_DIRECTORIES=str(staging))
        subprocess.run(
            ["git", "apply", "--check", str(patch)],
            cwd=source,
            env=patch_environment,
            check=True,
        )
        subprocess.run(
            ["git", "apply", str(patch)], cwd=source, env=patch_environment, check=True
        )
        if "NATIVE_TLS_TRACE" not in (
            source / "crates/requests/src/transport/mod.rs"
        ).read_text(encoding="utf-8"):
            raise RuntimeError("diagnostic patch did not instrument the copied source")
        epoch = subprocess.check_output(
            ["git", "show", "-s", "--format=%ct", "HEAD"], cwd=root, text=True
        ).strip()
        environment = dict(
            os.environ, SOURCE_DATE_EPOCH=epoch, PYTHONDONTWRITEBYTECODE="1"
        )
        subprocess.run(
            [
                sys.executable,
                str(source / "scripts/build_release_wheel.py"),
                "--interpreter",
                str(interpreter),
                "--out",
                str(output / "wheelhouse"),
            ],
            cwd=source,
            env=environment,
            check=True,
        )
    (output / "diagnostic-only.json").write_text(
        json.dumps(
            {
                "source_commit": subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=root, text=True
                ).strip(),
                "patch_sha256": hashlib.sha256(patch_bytes).hexdigest(),
                "instrumented": True,
                "release_qualified": False,
                "coverage": "TLS/plaintext poll_write acceptance and flush completion, not wire timestamps",
            }
        )
        + "\n",
        encoding="utf-8",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--child", nargs=2, metavar=("SOURCE", "IMPLEMENTATION"))
    parser.add_argument("--candidate-python", type=Path)
    parser.add_argument("--oracle-python", type=Path)
    parser.add_argument("--oracle-root", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--build-diagnostic", type=Path, metavar="INTERPRETER")
    parser.add_argument("nodes", nargs="*")
    args = parser.parse_args()
    if args.child:
        return run_child(
            Path(args.child[0]), args.child[1], tuple(args.nodes) or TLS_NODES
        )
    if args.build_diagnostic:
        if args.output is None:
            parser.error("--build-diagnostic requires --output")
        build_diagnostic(args.build_diagnostic, args.output.resolve())
        return 0
    if not all(
        (args.candidate_python, args.oracle_python, args.oracle_root, args.output)
    ):
        parser.error("both interpreters, oracle root and output are required")
    args.output.mkdir(parents=True, exist_ok=True)
    return run_suites(
        [
            ("candidate", args.candidate_python, Path(__file__).resolve().parents[1]),
            ("oracle", args.oracle_python, args.oracle_root.resolve()),
        ],
        args.output.resolve(),
    )


if __name__ == "__main__":
    raise SystemExit(main())

"""Private, bounded test-server diagnostics; never modifies the Requests client."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import sysconfig
import threading
import time
from pathlib import Path


class Trace:
    def __init__(self, output):
        self.output = output.open("w", encoding="utf-8", buffering=1)
        self.lock = threading.Lock()
        self.node = ""

    def emit(self, boundary, event, **fields):
        record = dict(
            time_ns=time.monotonic_ns(),
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
        if module == "tests.testserver.server":
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
            part in nodeid for part in ("pyopenssl_redirect", "https", "tls")
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


def run_child(import_root, implementation, nodes=("tests",)):
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
                ["-q", "-p", "no:cacheprovider", "--tb=short", *nodes], plugins=[trace]
            )
        )
    finally:
        trace.stop()


def run_suites(targets, output):
    # Reuse the release harness's allowlisted environment, including no bytecode.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from tests_rust.test_distribution import _clean_environment

    results = {}
    for implementation, python, source in targets:
        suite = output / implementation
        suite.mkdir()
        shutil.copytree(source / "tests", suite / "tests")
        shutil.copy2(source / "requirements-dev.txt", suite / "requirements-dev.txt")
        shutil.copy2(__file__, suite / "diagnostic.py")
        import_root = source
        if implementation == "oracle":
            import_root = suite / "oracle-src"
            shutil.copytree(source / "src/requests", import_root / "requests")
        environment = _clean_environment()
        environment.pop("REQUESTS_CHECKOUT", None)
        with (suite / "pytest.log").open("w", encoding="utf-8") as log:
            try:
                completed = subprocess.run(
                    [
                        str(python),
                        "diagnostic.py",
                        "--child",
                        str(import_root),
                        implementation,
                    ],
                    cwd=suite,
                    env=environment,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=300,
                    check=False,
                )
                results[implementation] = int(completed.returncode)
            except subprocess.TimeoutExpired:
                results[implementation] = "timeout-300s"
        (output / "results.json").write_text(
            json.dumps(results, sort_keys=True) + "\n", encoding="utf-8"
        )
    return int(any(result != 0 for result in results.values()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--child", nargs=2, metavar=("SOURCE", "IMPLEMENTATION"))
    parser.add_argument("--candidate-python", type=Path)
    parser.add_argument("--oracle-python", type=Path)
    parser.add_argument("--oracle-root", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.child:
        return run_child(Path(args.child[0]), args.child[1])
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

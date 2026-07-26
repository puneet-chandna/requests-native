from __future__ import annotations

import ast
import shutil
import tempfile
from pathlib import Path
from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case

ROOT = Path(__file__).resolve().parents[1]
ORACLE_ROOT = ROOT.parent / "requests"
ORACLE_REQUESTS_FILE = ORACLE_ROOT / "src" / "requests" / "__init__.py"
WARNING_CLASS = "urllib3.exceptions.InsecureRequestWarning"

FIXTURE_MANIFEST = {
    "oracle-ca": (
        "oracle",
        "tests/certs/expired/ca/ca.crt",
        "1407cb9c2502bf453ffa3f54cb846022f161a46b73fe4caf436157d95ad93367",
    ),
    "oracle-valid-server-cert": (
        "oracle",
        "tests/certs/valid/server/server.pem",
        "d0b55dbe152874f2d7bce3513d52a8e5f587566186dfd7a88d14ad41367133d1",
    ),
    "oracle-valid-server-key": (
        "oracle",
        "tests/certs/valid/server/server.key",
        "5457b89bc1af7de6829d78c9baa639ddea79b4e9dffe7933166c12836f9345c2",
    ),
    "oracle-expired-server-cert": (
        "oracle",
        "tests/certs/expired/server/server.pem",
        "232cc4e13f97688281a03e6a555e93c3a7bd6cfda4604fdee085f4ac2556f2c2",
    ),
    "oracle-expired-server-key": (
        "oracle",
        "tests/certs/expired/server/server.key",
        "acce64eae44a4073909c97e5929a1a319c947f4f37e4b933c1dcd805e59c8516",
    ),
    "rewrite-wrong-host-cert": (
        "rewrite",
        "tests/fixtures/tls/wrong-host/wrong-host.pem",
        "4683302b234393ea541fa73597507ecdf18e259d52a91feffa0a337d189eee4c",
    ),
    "rewrite-wrong-host-key": (
        "rewrite",
        "tests/fixtures/tls/wrong-host/wrong-host.key",
        "de1b85229c0f8380329b7e697a72ef6a0995f9de659febd48ab5a2dd5dd552f6",
    ),
    "rewrite-mtls-client-chain": (
        "rewrite",
        "tests/fixtures/tls/mtls-client/client-chain.pem",
        "22cecdcfa7590772fe66f78aaea6908d3a68311a7cd15755e30eb0529b58b001",
    ),
    "rewrite-mtls-client-combined": (
        "rewrite",
        "tests/fixtures/tls/mtls-client/client-combined.pem",
        "24f2e77b259f7e2493f7de7aabf10a2b822f3e3cc52de477651e2ee3abe07c24",
    ),
    "rewrite-mtls-client-key": (
        "rewrite",
        "tests/fixtures/tls/mtls-client/client.key",
        "d78880595600e9e6d44285c39716aaad1fe569f329fd7c2151f847f1c6cfdb14",
    ),
}

_TLS_SOURCE = dedent(
    r"""
    import hashlib
    import os
    import platform
    import socket
    import ssl
    import sys
    import threading
    import time
    import warnings
    from pathlib import Path

    import requests
    import urllib3
    from urllib3.connection import HTTPSConnection

    ROOT = Path.cwd().resolve()
    ORACLE_ROOT = ROOT.parent / "requests"
    ORACLE_REQUESTS_FILE = ORACLE_ROOT / "src" / "requests" / "__init__.py"
    FIXTURE_MANIFEST = __FIXTURE_MANIFEST__
    IO_TIMEOUT = 2.0
    JOIN_TIMEOUT = 4.0
    RESPONSE = (
        b"HTTP/1.1 200 OK\r\n"
        b"Content-Length: 2\r\n"
        b"Connection: close\r\n\r\n"
        b"ok"
    )

    def qualified_name(value):
        value_type = value if isinstance(value, type) else type(value)
        return f"{value_type.__module__}.{value_type.__qualname__}"

    def fixture_path(label):
        scope, relative, _ = FIXTURE_MANIFEST[label]
        base = ORACLE_ROOT if scope == "oracle" else ROOT
        return (base / relative).resolve()

    def pre_network_prelude():
        requests_file = Path(requests.__file__).resolve()
        assert requests_file == ORACLE_REQUESTS_FILE.resolve()
        assert requests.__version__ == "2.34.2"
        assert os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "oracle"

        fixtures = {}
        for label, (scope, relative, expected_sha256) in FIXTURE_MANIFEST.items():
            path = fixture_path(label)
            assert path.is_file(), f"missing frozen TLS input: {label}"
            actual_sha256 = hashlib.sha256(path.read_bytes()).hexdigest()
            assert actual_sha256 == expected_sha256, f"changed frozen TLS input: {label}"
            fixtures[label] = {
                "scope": scope,
                "relative_path": relative,
                "sha256": actual_sha256,
            }

        return {
            "requests": {
                "module_path": str(requests_file),
                "version": requests.__version__,
            },
            "urllib3": {
                "module_path": str(Path(urllib3.__file__).resolve()),
                "version": urllib3.__version__,
            },
            "python": {
                "implementation": platform.python_implementation(),
                "version": platform.python_version(),
            },
            "openssl": ssl.OPENSSL_VERSION,
            "fixtures": fixtures,
        }

    PRELUDE = pre_network_prelude()

    class LoopbackServer:
        def __init__(
            self,
            *,
            certificate=None,
            key=None,
            client_ca=None,
            allow_handshake_failure=False,
            trace=None,
        ):
            self.certificate = certificate
            self.key = key
            self.client_ca = client_ca
            self.allow_handshake_failure = allow_handshake_failure
            self.trace = trace
            self.accepts = 0
            self.handshakes = 0
            self.decrypted_reads = 0
            self.decrypted_bytes = 0
            self.client_certificate = False
            self.events = []
            self.handshake_failure = None
            self._port = None
            self._listener = None
            self._worker_error = None
            self._ready = threading.Event()
            self._stop = threading.Event()
            self._worker = threading.Thread(
                target=self._run,
                name="tls-oracle-loopback",
                daemon=True,
            )

        @property
        def url(self):
            assert self._port is not None
            return f"https://localhost:{self._port}/oracle"

        def __enter__(self):
            self._worker.start()
            if not self._ready.wait(IO_TIMEOUT):
                self.close()
                raise TimeoutError("loopback server did not become ready")
            if self._worker_error is not None:
                self.close()
            return self

        def __exit__(self, _error_type, _error, _traceback):
            self.close()

        def close(self):
            self._stop.set()
            listener = self._listener
            if listener is not None:
                listener.close()
            self._worker.join(JOIN_TIMEOUT)
            if self._worker.is_alive():
                raise TimeoutError("loopback server worker did not stop")
            if self._worker_error is not None:
                raise self._worker_error

        def observation(self):
            return {
                "accepts": self.accepts,
                "handshakes": self.handshakes,
                "decrypted_reads": self.decrypted_reads,
                "decrypted_bytes_positive": self.decrypted_bytes > 0,
                "client_certificate": self.client_certificate,
                "events": self.events,
                "handshake_failure": self.handshake_failure,
                "worker_alive": self._worker.is_alive(),
            }

        def _server_context(self):
            if self.certificate is None:
                return None
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(self.certificate, self.key)
            if self.client_ca is not None:
                context.load_verify_locations(cafile=self.client_ca)
                context.verify_mode = ssl.CERT_REQUIRED
            return context

        def _run(self):
            listener = None
            try:
                context = self._server_context()
                listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(("127.0.0.1", 0))
                listener.listen(1)
                listener.settimeout(0.05)
                self._listener = listener
                self._port = listener.getsockname()[1]
                self._ready.set()

                deadline = time.monotonic() + IO_TIMEOUT
                accepted = None
                while accepted is None and not self._stop.is_set():
                    if time.monotonic() >= deadline:
                        break
                    try:
                        accepted, _ = listener.accept()
                    except socket.timeout:
                        continue
                    except OSError:
                        if self._stop.is_set():
                            break
                        raise

                if accepted is None:
                    return
                self.accepts += 1
                self.events.append("accept")
                accepted.settimeout(IO_TIMEOUT)
                with accepted:
                    if context is None:
                        return
                    try:
                        with context.wrap_socket(accepted, server_side=True) as stream:
                            stream.settimeout(IO_TIMEOUT)
                            self.handshakes += 1
                            self.events.append("handshake")
                            self.client_certificate = bool(stream.getpeercert())
                            request = stream.recv(65536)
                            if request:
                                self.decrypted_reads += 1
                                self.decrypted_bytes += len(request)
                                self.events.append("decrypted-read")
                                if self.trace is not None:
                                    self.trace.append("server-decrypted-read")
                                stream.sendall(RESPONSE)
                    except ssl.SSLError as error:
                        if not self.allow_handshake_failure:
                            raise
                        self.handshake_failure = qualified_name(error)
            except BaseException as error:
                self._worker_error = error
            finally:
                self._ready.set()
                if listener is not None:
                    listener.close()

    def exception_graph(error, limit=12):
        objects = [error]
        indices = {id(error): 0}
        records = []
        truncated = False
        cursor = 0
        while cursor < len(objects):
            current = objects[cursor]
            links = []
            children = [
                ("cause", current.__cause__),
                ("context", current.__context__),
            ]
            children.extend(
                (f"arg:{index}", argument)
                for index, argument in enumerate(current.args)
                if isinstance(argument, BaseException)
            )
            for label, child in children:
                if not isinstance(child, BaseException):
                    continue
                identity = id(child)
                if identity not in indices:
                    if len(objects) == limit:
                        truncated = True
                        continue
                    indices[identity] = len(objects)
                    objects.append(child)
                links.append([label, indices[identity]])
            records.append({"type": qualified_name(current), "links": links})
            cursor += 1
        return {"nodes": records, "truncated": truncated}, objects

    def normalized_verification(error):
        graph, objects = exception_graph(error)
        verification_errors = [
            item for item in objects if isinstance(item, ssl.SSLCertVerificationError)
        ]
        assert len(verification_errors) == 1
        code = verification_errors[0].verify_code
        assert isinstance(code, int)
        categories = {
            "expired": {10},
            "untrusted": {18, 19, 20, 21},
            "hostname-mismatch": {62},
        }
        category = next(
            name for name, verification_codes in categories.items()
            if code in verification_codes
        )
        return {
            "graph": graph,
            "verification": {"category": category, "code": code},
        }

    def direct_error(error):
        return {
            "type": qualified_name(error),
            "message": str(error),
        }

    def perform_request(url, *, trace=None, check_hostname=True, **kwargs):
        trace = [] if trace is None else trace
        connect_calls = 0
        connect_returns = 0
        connect_raises = 0
        warning_classes = []
        original_connect = HTTPSConnection.connect

        def observed_connect(connection):
            nonlocal connect_calls, connect_returns, connect_raises
            connect_calls += 1
            try:
                connected = original_connect(connection)
            except BaseException:
                connect_raises += 1
                raise
            connect_returns += 1
            trace.append("connect-returned")
            return connected

        error = None
        status = None
        HTTPSConnection.connect = observed_connect
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("always")
                original_showwarning = warnings.showwarning

                def observed_warning(
                    _message,
                    category,
                    _filename,
                    _lineno,
                    file=None,
                    line=None,
                ):
                    warning_class = qualified_name(category)
                    warning_classes.append(warning_class)
                    trace.append(f"warning:{warning_class}")

                warnings.showwarning = observed_warning
                try:
                    with requests.Session() as session:
                        session.trust_env = False
                        if not check_hostname:
                            adapter = requests.adapters.HTTPAdapter()
                            adapter.poolmanager.connection_pool_kw[
                                "assert_hostname"
                            ] = False
                            session.mount("https://", adapter)
                        try:
                            response = session.get(
                                url,
                                timeout=(IO_TIMEOUT, IO_TIMEOUT),
                                **kwargs,
                            )
                            status = response.status_code
                            response.close()
                        except BaseException as caught:
                            error = caught
                finally:
                    warnings.showwarning = original_showwarning
        finally:
            HTTPSConnection.connect = original_connect
        return {
            "status": status,
            "error": error,
            "connect_calls": connect_calls,
            "connect_returns": connect_returns,
            "connect_raises": connect_raises,
            "warning_classes": warning_classes,
            "trace": trace,
        }

    def wrap(value):
        return {"environment": PRELUDE, "value": value}

    def environment_probe():
        return wrap({"pre_network_checks": True})

    def missing_path_probe(missing_ca, missing_cert, missing_key):
        cases = [
            ("ca", {"verify": missing_ca}),
            ("certificate", {"verify": False, "cert": missing_cert}),
            (
                "key",
                {
                    "verify": False,
                    "cert": (
                        str(fixture_path("rewrite-mtls-client-chain")),
                        missing_key,
                    ),
                },
            ),
        ]
        observations = {}
        with LoopbackServer() as server:
            for name, kwargs in cases:
                request = perform_request(server.url, **kwargs)
                observations[name] = {
                    "error": direct_error(request["error"]),
                    "connect_calls": request["connect_calls"],
                    "connect_returns": request["connect_returns"],
                    "connect_raises": request["connect_raises"],
                    "warning_classes": request["warning_classes"],
                }
        return wrap({"cases": observations, "server": server.observation()})

    def capath_probe(capath):
        trace = []
        with LoopbackServer(
            certificate=str(fixture_path("oracle-valid-server-cert")),
            key=str(fixture_path("oracle-valid-server-key")),
            allow_handshake_failure=True,
            trace=trace,
        ) as server:
            request = perform_request(server.url, verify=capath, trace=trace)
        error = request.pop("error")
        request["error"] = None if error is None else normalized_verification(error)
        return wrap({"request": request, "server": server.observation()})

    def verification_probe(scenario):
        if scenario == "expired":
            certificate = fixture_path("oracle-expired-server-cert")
            key = fixture_path("oracle-expired-server-key")
            verify = fixture_path("oracle-ca")
        elif scenario == "untrusted":
            certificate = fixture_path("oracle-valid-server-cert")
            key = fixture_path("oracle-valid-server-key")
            verify = fixture_path("rewrite-wrong-host-cert")
        elif scenario == "hostname-mismatch":
            certificate = fixture_path("rewrite-wrong-host-cert")
            key = fixture_path("rewrite-wrong-host-key")
            verify = fixture_path("oracle-ca")
        else:
            raise AssertionError(f"unknown verification scenario: {scenario}")

        with LoopbackServer(
            certificate=str(certificate),
            key=str(key),
            allow_handshake_failure=True,
        ) as server:
            request = perform_request(
                server.url,
                verify=str(verify),
                check_hostname=scenario != "expired",
            )
        error = request.pop("error")
        assert error is not None
        request["error"] = normalized_verification(error)
        return wrap({"request": request, "server": server.observation()})

    def mtls_probe(combined):
        trace = []
        certificate = fixture_path(
            "rewrite-mtls-client-combined"
            if combined
            else "rewrite-mtls-client-chain"
        )
        cert = (
            str(certificate)
            if combined
            else (str(certificate), str(fixture_path("rewrite-mtls-client-key")))
        )
        with LoopbackServer(
            certificate=str(fixture_path("oracle-valid-server-cert")),
            key=str(fixture_path("oracle-valid-server-key")),
            client_ca=str(fixture_path("oracle-ca")),
            trace=trace,
        ) as server:
            request = perform_request(
                server.url,
                verify=False,
                cert=cert,
                trace=trace,
            )
        error = request.pop("error")
        request["error"] = None if error is None else exception_graph(error)[0]
        return wrap({"request": request, "server": server.observation()})

    def warning_probe():
        trace = []
        with LoopbackServer(
            certificate=str(fixture_path("oracle-valid-server-cert")),
            key=str(fixture_path("oracle-valid-server-key")),
            trace=trace,
        ) as server:
            request = perform_request(server.url, verify=False, trace=trace)
        error = request.pop("error")
        request["error"] = None if error is None else exception_graph(error)[0]
        return wrap({"request": request, "server": server.observation()})
    """
).replace("__FIXTURE_MANIFEST__", repr(FIXTURE_MANIFEST))


def _snapshot(expression: str) -> dict[str, object]:
    run = run_oracle_case({"source": f"{_TLS_SOURCE}\nresult = {expression}\n"})
    assert run.observations["exception"] is None
    assert run.observations["warnings"] == []
    assert run.stderr == ""
    result = ast.literal_eval(run.observations["result"]["repr"])
    environment = result["environment"]
    assert environment["requests"] == {
        "module_path": str(ORACLE_REQUESTS_FILE.resolve()),
        "version": "2.34.2",
    }
    return result


def _successful_server() -> dict[str, object]:
    return {
        "accepts": 1,
        "handshakes": 1,
        "decrypted_reads": 1,
        "decrypted_bytes_positive": True,
        "client_certificate": False,
        "events": ["accept", "handshake", "decrypted-read"],
        "handshake_failure": None,
        "worker_alive": False,
    }


def _failed_verification_server() -> dict[str, object]:
    return {
        "accepts": 1,
        "handshakes": 0,
        "decrypted_reads": 0,
        "decrypted_bytes_positive": False,
        "client_certificate": False,
        "events": ["accept"],
        "handshake_failure": "ssl.SSLError",
        "worker_alive": False,
    }


EXPECTED_EXCEPTION_GRAPH = {
    "nodes": [
        {
            "type": "requests.exceptions.SSLError",
            "links": [["context", 1], ["arg:0", 1]],
        },
        {
            "type": "urllib3.exceptions.MaxRetryError",
            "links": [["cause", 2], ["context", 2]],
        },
        {
            "type": "urllib3.exceptions.SSLError",
            "links": [["context", 3], ["arg:0", 3]],
        },
        {"type": "ssl.SSLCertVerificationError", "links": []},
    ],
    "truncated": False,
}


def test_controlled_oracle_and_frozen_tls_inputs_are_verified() -> None:
    observation = _snapshot("environment_probe()")
    assert observation["value"] == {"pre_network_checks": True}
    assert observation["environment"]["fixtures"] == {
        label: {
            "scope": scope,
            "relative_path": relative,
            "sha256": sha256,
        }
        for label, (scope, relative, sha256) in FIXTURE_MANIFEST.items()
    }


def test_missing_tls_paths_fail_before_tcp_accept(tmp_path: Path) -> None:
    missing_ca = tmp_path / "missing-ca.pem"
    missing_cert = tmp_path / "missing-client.pem"
    missing_key = tmp_path / "missing-client.key"
    assert not missing_ca.exists()
    assert not missing_cert.exists()
    assert not missing_key.exists()

    value = _snapshot(
        f"missing_path_probe({str(missing_ca)!r}, {str(missing_cert)!r},"
        f" {str(missing_key)!r})"
    )["value"]
    assert value["cases"] == {
        "ca": {
            "error": {
                "type": "builtins.OSError",
                "message": (
                    "Could not find a suitable TLS CA certificate bundle, "
                    f"invalid path: {missing_ca}"
                ),
            },
            "connect_calls": 0,
            "connect_returns": 0,
            "connect_raises": 0,
            "warning_classes": [],
        },
        "certificate": {
            "error": {
                "type": "builtins.OSError",
                "message": (
                    "Could not find the TLS certificate file, "
                    f"invalid path: {missing_cert}"
                ),
            },
            "connect_calls": 0,
            "connect_returns": 0,
            "connect_raises": 0,
            "warning_classes": [],
        },
        "key": {
            "error": {
                "type": "builtins.OSError",
                "message": (
                    f"Could not find the TLS key file, invalid path: {missing_key}"
                ),
            },
            "connect_calls": 0,
            "connect_returns": 0,
            "connect_raises": 0,
            "warning_classes": [],
        },
    }
    assert value["server"] == {
        "accepts": 0,
        "handshakes": 0,
        "decrypted_reads": 0,
        "decrypted_bytes_positive": False,
        "client_certificate": False,
        "events": [],
        "handshake_failure": None,
        "worker_alive": False,
    }


def test_openssl_capath_requires_the_correct_hash_name() -> None:
    temporary = tempfile.TemporaryDirectory(prefix="requests-red-g-capath-")
    capath = Path(temporary.name)
    try:
        ca_certificate = ORACLE_ROOT / FIXTURE_MANIFEST["oracle-ca"][1]
        shutil.copyfile(ca_certificate, capath / "ca.crt")
        unhashed = _snapshot(f"capath_probe({str(capath)!r})")["value"]
        shutil.copyfile(ca_certificate, capath / "117adfc4.0")
        hashed = _snapshot(f"capath_probe({str(capath)!r})")["value"]
    finally:
        temporary.cleanup()
    assert not capath.exists()

    assert unhashed["request"]["status"] is None
    assert unhashed["request"]["connect_calls"] == 1
    assert unhashed["request"]["connect_returns"] == 0
    assert unhashed["request"]["connect_raises"] == 1
    assert unhashed["request"]["warning_classes"] == []
    assert unhashed["request"]["trace"] == []
    assert unhashed["request"]["error"]["verification"] == {
        "category": "untrusted",
        "code": 19,
    }
    assert unhashed["request"]["error"]["graph"] == EXPECTED_EXCEPTION_GRAPH
    assert unhashed["server"] == _failed_verification_server()

    assert {
        name: value
        for name, value in hashed["request"].items()
        if name != "warning_classes"
    } == {
        "status": 200,
        "connect_calls": 1,
        "connect_returns": 1,
        "connect_raises": 0,
        "trace": ["connect-returned", "server-decrypted-read"],
        "error": None,
    }
    assert hashed["server"] == _successful_server()


@pytest.mark.parametrize(
    ("scenario", "verification"),
    [
        ("expired", {"category": "expired", "code": 10}),
        ("untrusted", {"category": "untrusted", "code": 19}),
        (
            "hostname-mismatch",
            {"category": "hostname-mismatch", "code": 62},
        ),
    ],
)
def test_server_verification_failures_are_normalized(
    scenario: str,
    verification: dict[str, object],
) -> None:
    value = _snapshot(f"verification_probe({scenario!r})")["value"]
    assert value["request"]["status"] is None
    assert value["request"]["connect_calls"] == 1
    assert value["request"]["connect_returns"] == 0
    assert value["request"]["connect_raises"] == 1
    assert value["request"]["warning_classes"] == []
    assert value["request"]["trace"] == []
    assert value["request"]["error"]["verification"] == verification
    assert value["request"]["error"]["graph"] == EXPECTED_EXCEPTION_GRAPH
    assert value["server"] == _failed_verification_server()


@pytest.mark.parametrize("combined", [True, False], ids=["combined", "separate"])
def test_long_lived_mtls_identity_is_presented(combined: bool) -> None:
    value = _snapshot(f"mtls_probe({combined!r})")["value"]
    assert value["request"] == {
        "status": 200,
        "connect_calls": 1,
        "connect_returns": 1,
        "connect_raises": 0,
        "warning_classes": [WARNING_CLASS],
        "trace": [
            "connect-returned",
            f"warning:{WARNING_CLASS}",
            "server-decrypted-read",
        ],
        "error": None,
    }
    expected_server = _successful_server()
    expected_server["client_certificate"] = True
    assert value["server"] == expected_server


def test_insecure_warning_follows_connect_and_precedes_http_bytes() -> None:
    value = _snapshot("warning_probe()")["value"]
    assert value["request"] == {
        "status": 200,
        "connect_calls": 1,
        "connect_returns": 1,
        "connect_raises": 0,
        "warning_classes": [WARNING_CLASS],
        "trace": [
            "connect-returned",
            f"warning:{WARNING_CLASS}",
            "server-decrypted-read",
        ],
        "error": None,
    }
    assert value["server"] == _successful_server()

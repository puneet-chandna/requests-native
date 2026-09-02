from __future__ import annotations

import socket
import threading

import pytest
import urllib3
from tests.testserver.server import Server, consume_socket_content
from tests_differential.test_adapters import loopback
from urllib3.exceptions import ProtocolError

import requests
import requests._requests_rust as native_module
import requests.adapters as adapters_module
from requests.sessions import Session


def test_pristine_default_uses_rust_without_calling_python_send(monkeypatch) -> None:
    def forbidden(*args, **kwargs):
        raise AssertionError("pristine default reached the Python adapter send")

    monkeypatch.setattr(adapters_module, "_HTTP_ADAPTER_COMPAT_SEND", forbidden)

    with loopback((200, {}, b"native")) as (server, url):
        session = Session()
        session.trust_env = False
        try:
            response = session.get(url)
        finally:
            session.close()

    assert response.content == b"native"
    assert type(response.raw).__module__ == "requests._requests_rust"
    assert server.requests == 1


def test_pristine_default_matches_python_http1_header_spelling() -> None:
    requests_seen: list[bytes] = []

    def handler(sock):
        for value in (b"first", b"third"):
            requests_seen.append(consume_socket_content(sock, timeout=0.5))
            sock.sendall(
                b"HTTP/1.1 200 OK\r\n"
                b"x-CuStOm: "
                + value
                + b"\r\nZ-Last: second\r\nContent-Length: 0\r\n\r\n"
            )

    close_server = threading.Event()
    server = Server(handler, wait_to_close_event=close_server)
    with server as (host, port):
        with requests.Session() as session:
            response = session.get(
                f"http://{host}:{port}/",
                headers={"x-CuStOm-HeAdEr": "yes"},
            )
            second = session.get(
                f"http://{host}:{port}/",
                headers={"aNoThEr-CaSe": "again"},
            )
        close_server.set()

    assert len(requests_seen) == 2
    assert b"x-CuStOm-HeAdEr: yes\r\n" in requests_seen[0]
    assert b"aNoThEr-CaSe: again\r\n" in requests_seen[1]
    assert list(response.headers.items()) == [
        ("x-CuStOm", "first"),
        ("Z-Last", "second"),
        ("Content-Length", "0"),
    ]
    assert list(second.headers.items()) == [
        ("x-CuStOm", "third"),
        ("Z-Last", "second"),
        ("Content-Length", "0"),
    ]


def test_exact_response_with_custom_raw_stays_on_python_without_trial(
    monkeypatch,
) -> None:
    def forbidden(*args, **kwargs):
        raise AssertionError("custom Response.raw reached the native response facade")

    class CustomRaw:
        def stream(self, chunk_size, decode_content):
            assert (chunk_size, decode_content) == (3, True)
            raise ProtocolError("custom raw")

    monkeypatch.setattr(native_module, "_response_facade_trial", forbidden)
    response = requests.Response()
    response.raw = CustomRaw()

    with pytest.raises(requests.exceptions.ChunkedEncodingError, match="custom raw"):
        list(response.iter_content(3))


def test_default_proxy_refusal_retains_nested_new_connection_error() -> None:
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
    listener.close()

    with pytest.raises(requests.exceptions.ProxyError) as caught:
        requests.get(
            "http://example.invalid/resource",
            proxies={"http": f"http://127.0.0.1:{port}"},
        )

    exhausted = caught.value.args[0]
    assert type(exhausted) is urllib3.exceptions.MaxRetryError
    reason = exhausted.reason
    assert type(reason) is urllib3.exceptions.ProxyError
    assert reason.args[0] == "Unable to connect to proxy"
    nested = reason.args[1]
    assert type(nested) is urllib3.exceptions.NewConnectionError
    assert isinstance(nested.__context__, ConnectionRefusedError)
    assert nested.args[0].endswith(
        f"Failed to establish a new connection: {nested.__context__}"
    )


def test_default_accepts_valid_eof_terminated_response_headers() -> None:
    def handler(sock):
        consume_socket_content(sock, timeout=0.5)
        sock.sendall(b"HTTP/1.1 204 No Content\r\nX-Eof: accepted\r\n")

    server = Server(handler)
    with server as (host, port):
        response = requests.get(f"http://{host}:{port}/")

    assert response.status_code == 204
    assert response.headers["x-eof"] == "accepted"
    assert len(server.handler_results) == 1


def test_default_does_not_turn_an_empty_response_into_success() -> None:
    def handler(sock):
        return consume_socket_content(sock, timeout=0.5)

    server = Server(handler)
    with server as (host, port):
        with pytest.raises(requests.exceptions.ConnectionError):
            requests.get(f"http://{host}:{port}/")

    assert len(server.handler_results) == 1

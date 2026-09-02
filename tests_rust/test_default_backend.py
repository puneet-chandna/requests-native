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

    def parsed_headers(request: bytes) -> list[tuple[str, str]]:
        head = request.split(b"\r\n\r\n", 1)[0]
        return [
            tuple(line.decode("latin-1").split(": ", 1))
            for line in head.split(b"\r\n")[1:]
        ]

    assert len(requests_seen) == 2
    authority = f"{host}:{port}"
    assert parsed_headers(requests_seen[0]) == [
        ("Host", authority),
        *list(response.request.headers.items()),
    ]
    assert parsed_headers(requests_seen[1]) == [
        ("Host", authority),
        *list(second.request.headers.items()),
    ]
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


@pytest.mark.parametrize(
    ("headers", "visible_value"),
    [
        (b"Content-Length: nope\r\n", "nope"),
        (b"Content-Length: 1\r\nContent-Length: nope\r\n", "1, nope"),
        (b"Content-Length: 1, nope\r\n", "1, nope"),
    ],
)
def test_default_ignores_noncanonical_content_length_like_urllib3(
    headers: bytes,
    visible_value: str,
) -> None:
    def handler(sock):
        request = consume_socket_content(sock, timeout=0.5)
        sock.sendall(b"HTTP/1.1 200 OK\r\n" + headers + b"\r\nx")
        return request

    server = Server(handler)
    with server as (host, port):
        response = requests.get(f"http://{host}:{port}/", stream=True)
        assert response.status_code == 200
        assert response.headers["Content-Length"] == visible_value
        assert response.content == b"x"

    assert len(server.handler_results) == 1


def test_default_overflowing_content_length_returns_head_then_body_error() -> None:
    def handler(sock):
        request = consume_socket_content(sock, timeout=0.5)
        sock.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\nx"
        )
        return request

    server = Server(handler)
    with server as (host, port):
        response = requests.get(f"http://{host}:{port}/", stream=True)
        assert response.status_code == 200
        with pytest.raises(requests.exceptions.ChunkedEncodingError) as caught:
            response.content

    protocol = caught.value.args[0]
    assert type(protocol) is ProtocolError
    assert protocol.args[0] == (
        "Connection broken: IncompleteRead(1 bytes read, "
        "18446744073709551615 more expected)"
    )
    incomplete = protocol.args[1]
    assert type(incomplete) is urllib3.exceptions.IncompleteRead
    assert incomplete.partial == 1
    assert incomplete.expected == 18446744073709551615
    assert len(server.handler_results) == 1


def test_default_conflicting_numeric_content_length_is_exact_invalid_header() -> None:
    def handler(sock):
        request = consume_socket_content(sock, timeout=0.5)
        sock.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 1, 2\r\n\r\nx")
        return request

    server = Server(handler)
    with server as (host, port):
        with pytest.raises(requests.exceptions.InvalidHeader) as caught:
            requests.get(f"http://{host}:{port}/", stream=True)

    original = caught.value.args[0]
    assert type(original) is urllib3.exceptions.InvalidHeader
    assert original.args == (
        "Content-Length contained multiple unmatching values (1, 2)",
    )
    assert len(server.handler_results) == 1

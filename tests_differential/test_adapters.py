from __future__ import annotations

import builtins
import functools
import gc
import gzip
import pickle
import random
import socket
import threading
import weakref
import zlib
from collections import OrderedDict
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import urllib3
from urllib3.util.retry import Retry

import requests
from requests import adapters
from requests.adapters import HTTPAdapter, _rust_adapter_trial
from requests.exceptions import RetryError
from requests.models import PreparedRequest

_PROVENANCE_PROBE = None


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        server = self.server
        with server.lock:
            server.requests += 1
            server.clients.add(self.client_address)
            response = server.responses.pop(0)
        status, headers, body = response[:3]
        reason = response[3] if len(response) == 4 else None
        close_after = len(response) >= 5 and response[4]
        if status is None:
            self.close_connection = True
            self.connection.close()
            return
        self.send_response(status, message=reason)
        for name, value in headers.items():
            self.send_header(name, value)
        wire_length = (
            len(body[0]) + len(body[2])
            if isinstance(body, tuple) and len(body) == 3
            else len(body)
        )
        if not any(name.lower() == "content-length" for name in headers):
            self.send_header("Content-Length", str(wire_length))
        self.end_headers()
        if isinstance(body, tuple) and len(body) == 3:
            first, release, rest = body
            self.wfile.write(first)
            self.wfile.flush()
            server.first_chunk_sent.set()
            release.wait(2)
            self.wfile.write(rest)
        else:
            self.wfile.write(body)
        if close_after:
            self.wfile.flush()
            self.close_connection = True

    do_POST = do_GET

    def log_message(self, format, *args):
        pass


@contextmanager
def loopback(*responses):
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.responses = list(responses)
    server.requests = 0
    server.clients = set()
    server.first_chunk_sent = threading.Event()
    server.lock = threading.Lock()
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield server, f"http://127.0.0.1:{server.server_port}/resource"
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)


@contextmanager
def socks5_loopback(*bodies):
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    observed = {"connections": 0, "requests": 0}

    def recv_exact(connection, amount):
        data = b""
        while len(data) < amount:
            data += connection.recv(amount - len(data))
        return data

    def serve():
        connection, _ = listener.accept()
        with connection:
            observed["connections"] += 1
            version, count = recv_exact(connection, 2)
            assert version == 5
            recv_exact(connection, count)
            connection.sendall(b"\x05\x00")
            version, command, _, address_type = recv_exact(connection, 4)
            assert (version, command) == (5, 1)
            if address_type == 1:
                recv_exact(connection, 4)
            elif address_type == 3:
                recv_exact(connection, recv_exact(connection, 1)[0])
            else:
                raise AssertionError(f"unexpected SOCKS address type {address_type}")
            recv_exact(connection, 2)
            connection.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x50")
            for body in bodies:
                request = b""
                while b"\r\n\r\n" not in request:
                    request += connection.recv(4096)
                observed["requests"] += 1
                connection.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Length: "
                    + str(len(body)).encode()
                    + b"\r\n\r\n"
                    + body
                )

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    try:
        yield observed, f"socks5h://127.0.0.1:{listener.getsockname()[1]}"
    finally:
        listener.close()
        worker.join(timeout=5)


@contextmanager
def closing_loopback_barrier():
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    accepted = threading.Event()
    release = threading.Event()

    def serve():
        connection, _ = listener.accept()
        with connection:
            accepted.set()
            release.wait(2)

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    try:
        yield (
            f"http://127.0.0.1:{listener.getsockname()[1]}/resource",
            accepted,
            release,
        )
    finally:
        release.set()
        listener.close()
        worker.join(timeout=5)


def prepared(url, body=None, method="GET"):
    request = PreparedRequest()
    request.prepare(method=method, url=url, headers={"X-Test": "adapter"}, data=body)
    return request


@contextmanager
def mutated_manager_behavior(manager, mutation):
    if mutation == "pools_getitem":
        function = type(manager.pools).__getitem__
        original = function.__code__
        function.__code__ = (lambda self, key: None).__code__
        try:
            yield
        finally:
            function.__code__ = original
        return
    if mutation == "key_partial_keywords":
        partial = manager.key_fn_by_scheme["http"]
        assert isinstance(partial, functools.partial)
        partial.keywords["_mutated_behavior"] = True
        try:
            yield
        finally:
            del partial.keywords["_mutated_behavior"]
        return
    if mutation == "pool_init":
        function = manager.pool_classes_by_scheme["http"].__init__
        original = function.__code__
        function.__code__ = (lambda self, *args, **kwargs: None).__code__
        try:
            yield
        finally:
            function.__code__ = original
        return
    raise AssertionError(f"unknown mutation {mutation}")


def test_default_path_and_visible_urllib3_state_remain_python_compatible(monkeypatch):
    adapter = HTTPAdapter(pool_connections=3, pool_maxsize=4, pool_block=True)
    original_retry = adapter.max_retries
    original_manager = adapter.poolmanager
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)

    assert adapter.send(prepared("http://example.test/")) is marker
    assert adapter.max_retries is original_retry
    assert adapter.poolmanager is original_manager
    assert pickle.loads(pickle.dumps(adapter)).proxy_manager == {}
    assert (
        adapter.proxy_manager_for("http://proxy.test")
        is adapter.proxy_manager["http://proxy.test"]
    )


def test_pristine_trial_uses_native_response_and_preserves_stream_across_clear():
    with loopback(
        (200, {"X-Reply": "first"}, b"first"),
        (200, {"X-Reply": "second"}, b"second"),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            response = adapter.send(prepared(url), stream=True)
            assert type(response.raw).__module__ == "requests._requests_rust"
            adapter.close()
            assert response.content == b"first"
            reused = adapter.send(prepared(url))
            assert reused.content == b"second"
            adapter.close()
        assert server.requests == 2


def test_exact_admission_falls_back_before_native_pool_creation(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    original_count = requests._requests_rust._adapter_pool_side_table_trial()

    adapter = HTTPAdapter()
    adapter.max_retries = object()
    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/")) is marker

    subclass = type("Subclass", (HTTPAdapter,), {})
    with _rust_adapter_trial():
        assert subclass().send(prepared("http://example.test/")) is marker

    monkeypatch.setattr(adapters, "select_proxy", lambda *a, **k: None)
    with _rust_adapter_trial():
        assert HTTPAdapter().send(prepared("http://example.test/")) is marker

    assert requests._requests_rust._adapter_pool_side_table_trial() == original_count


def test_nonreplayable_body_and_class_patch_fall_back(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with _rust_adapter_trial():
        assert (
            HTTPAdapter().send(
                prepared("http://example.test/", body=iter([b"payload"]))
            )
            is marker
        )

    monkeypatch.setattr(HTTPAdapter, "add_headers", lambda *a, **k: None)
    with _rust_adapter_trial():
        assert HTTPAdapter().send(prepared("http://example.test/")) is marker


def test_restored_instance_class_and_module_state_readmits_native_trial(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    request = prepared("http://example.test/")

    adapter.send = lambda *args, **kwargs: marker
    with _rust_adapter_trial():
        assert adapter.send(request) is marker
    del adapter.send

    with monkeypatch.context() as patched:
        patched.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
        patched.setattr(HTTPAdapter, "add_headers", lambda *a, **k: None)
        with _rust_adapter_trial():
            assert adapter.send(request) is marker

    with monkeypatch.context() as patched:
        patched.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
        patched.setattr(adapters, "select_proxy", lambda *a, **k: None)
        with _rust_adapter_trial():
            assert adapter.send(request) is marker

    with loopback((200, {}, b"restored")) as (server, url):
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert type(response.raw).__module__ == "requests._requests_rust"
            assert response.content == b"restored"
            adapter.close()
        assert server.requests == 1


def test_retry_loop_closes_prior_response_and_uses_origin_thread_sleep(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(
        total=3,
        status=3,
        status_forcelist={503},
        backoff_factor=0.1,
        raise_on_status=True,
    )
    with loopback(
        (503, {}, b"discard-one"),
        (503, {}, b"discard-two"),
        (200, {}, b"complete"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert response.content == b"complete"
            adapter.close()

    assert server.requests == 3
    assert len(server.clients) == 1
    assert sleeps == [0.2]
    assert adapter.max_retries is retry
    assert retry.history == ()


def test_retry_after_uses_python_clock_path_and_pool_side_table_is_weak(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(total=2, status=2, status_forcelist={503})
    with loopback(
        (503, {"Retry-After": "1"}, b"discard"),
        (200, {}, b"done"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert response.content == b"done"
        reference = weakref.ref(adapter)
        del response
        del adapter
        gc.collect()
        assert reference() is None
        assert requests._requests_rust._adapter_pool_side_table_trial() == 0
        assert server.requests == 2

    assert sleeps == [1]


def test_retry_after_date_uses_origin_thread_python_clock(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.time", lambda: 7)
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(total=1, status=1, status_forcelist={503})
    with loopback(
        (503, {"Retry-After": "Thu, 01 Jan 1970 00:00:10 GMT"}, b"discard"),
        (200, {}, b"done"),
    ) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter(max_retries=retry).send(prepared(url))
            assert response.content == b"done"
        assert server.requests == 2
    assert sleeps == [3]


def test_read_failure_before_response_head_uses_read_budget():
    retry = Retry(total=2, connect=0, read=2, other=0)
    with loopback(
        (None, {}, b""),
        (None, {}, b""),
        (200, {}, b"recovered"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert response.content == b"recovered"
            adapter.close()
        assert server.requests == 3


def test_method_filter_and_status_counter_exhaustion_are_adapter_owned():
    retry = Retry(
        total=2,
        status=2,
        allowed_methods={"GET"},
        status_forcelist={503},
    )
    with loopback((503, {}, b"not-retried")) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            response = adapter.send(prepared(url, method="POST"))
            assert response.status_code == 503
            assert response.content == b"not-retried"
            adapter.close()
        assert server.requests == 1

    retry = Retry(total=2, status=0, status_forcelist={503})
    with loopback((503, {}, b"exhausted")) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        try:
            with _rust_adapter_trial():
                adapter.send(prepared(url))
        except RetryError as error:
            assert error.request.url == url
        else:
            raise AssertionError("exhausted status retries must raise RetryError")
        assert server.requests == 1


def test_build_response_keeps_custom_raw_identity():
    adapter = HTTPAdapter()
    request = prepared("http://example.test/")

    class Raw:
        status = 204
        headers = {}
        reason = "No Content"

    raw = Raw()
    assert adapter.build_response(request, raw).raw is raw


def test_native_proxy_send_keeps_visible_proxy_manager_cache():
    with loopback((200, {}, b"proxied")) as (server, proxy_url):
        proxy_url = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            response = adapter.send(
                prepared("http://origin.example/resource"),
                proxies={"http": proxy_url},
            )
            assert response.content == b"proxied"
            assert list(adapter.proxy_manager) == [proxy_url]
            adapter.close()
        assert server.requests == 1


def test_close_order_duplicates_repetition_and_first_error_match_oracle():
    events = []

    class Manager:
        def __init__(self, name, error=None):
            self.name = name
            self.error = error

        def clear(self):
            events.append(self.name)
            if self.error is not None:
                raise self.error

    adapter = HTTPAdapter()
    shared = Manager("proxy")
    adapter.poolmanager = Manager("main")
    adapter.proxy_manager = {"first": shared, "second": shared}
    with _rust_adapter_trial():
        adapter.close()
        adapter.close()
    assert events == ["main", "proxy", "proxy", "main", "proxy", "proxy"]
    assert list(adapter.proxy_manager) == ["first", "second"]

    failure = RuntimeError("stop")
    adapter.poolmanager = Manager("main")
    adapter.proxy_manager = {
        "first": Manager("first", failure),
        "second": Manager("never"),
    }
    try:
        with _rust_adapter_trial():
            adapter.close()
    except RuntimeError as error:
        assert error is failure
    else:
        raise AssertionError("close must preserve the first visible manager failure")
    assert events[-2:] == ["main", "first"]


def test_manager_replacement_before_first_trial_falls_back_without_native_effects(
    monkeypatch,
):
    marker = object()
    adapter = HTTPAdapter()
    adapter.poolmanager = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    before = requests._requests_rust._adapter_pool_side_table_trial()
    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/")) is marker
    assert requests._requests_rust._adapter_pool_side_table_trial() == before


def test_zero_timeout_and_timeout_total_fall_back_before_native_effects(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    adapter = HTTPAdapter()
    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/"), timeout=0) is marker

    from urllib3.util import Timeout

    with _rust_adapter_trial():
        assert (
            adapter.send(
                prepared("http://example.test/"),
                timeout=Timeout(total=1),
            )
            is marker
        )


def test_verify_true_default_bundle_mutation_falls_back(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    monkeypatch.setattr(adapters, "DEFAULT_CA_BUNDLE_PATH", object())
    with _rust_adapter_trial():
        assert adapter.send(prepared("https://example.test/"), verify=True) is marker


def test_raw_read_zero_is_inert_and_custom_reason_is_preserved():
    compressed = gzip.compress(b"payload")
    with loopback((200, {"Content-Encoding": "gzip"}, compressed, "Very Fine")) as (
        server,
        url,
    ):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            response = adapter.send(prepared(url), stream=True)
            assert response.reason == "Very Fine"
            assert response.raw.read(0, decode_content=False) == b""
            assert response.raw.read(decode_content=False) == compressed
            adapter.close()
        assert server.requests == 1


def test_raw_stream_decode_content_true_decodes_but_false_preserves_wire():
    compressed = gzip.compress(b"payload")
    with loopback(
        (200, {"Content-Encoding": "gzip"}, compressed),
        (200, {"Content-Encoding": "gzip"}, compressed),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            raw = adapter.send(prepared(url), stream=True).raw
            assert b"".join(raw.stream(3, decode_content=False)) == compressed
            decoded = adapter.send(prepared(url), stream=True).raw
            assert b"".join(decoded.stream(3, decode_content=True)) == b"payload"
            adapter.close()
        assert server.requests == 2


def test_retry_drain_ignores_invalid_content_encoding():
    retry = Retry(total=1, status=1, status_forcelist={503})
    with loopback(
        (503, {"Content-Encoding": "gzip"}, b"invalid-gzip"),
        (200, {}, b"done"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"done"
            adapter.close()
        assert server.requests == 2


def test_close_outside_trial_clears_native_generation_and_reentry_is_fresh():
    with loopback(
        (200, {}, b"first"),
        (200, {}, b"second"),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            outstanding = adapter.send(prepared(url), stream=True)
        adapter.close()
        assert outstanding.content == b"first"
        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"second"
        assert server.requests == 2
        assert len(server.clients) == 2


def test_pool_block_holds_capacity_until_raw_release_but_overflow_does_not():
    with loopback(
        (200, {}, b"held"),
        (200, {}, b"blocked"),
        (200, {}, b"overflow-one"),
        (200, {}, b"overflow-two"),
    ) as (server, url):
        adapter = HTTPAdapter(pool_maxsize=1, pool_block=True)
        completed = threading.Event()
        result = []
        with _rust_adapter_trial():
            held = adapter.send(prepared(url), stream=True)

            def blocked_send():
                with _rust_adapter_trial():
                    result.append(adapter.send(prepared(url)).content)
                completed.set()

            worker = threading.Thread(target=blocked_send)
            worker.start()
            assert not completed.wait(0.1)
            assert server.requests == 1
            held.close()
            assert completed.wait(2)
            worker.join(timeout=2)
            assert result == [b"blocked"]
            adapter.close()

        adapter = HTTPAdapter(pool_maxsize=1, pool_block=False)
        completed.clear()
        result.clear()
        with _rust_adapter_trial():
            held = adapter.send(prepared(url), stream=True)
            worker = threading.Thread(target=blocked_send)
            worker.start()
            assert completed.wait(2)
            worker.join(timeout=2)
            assert result == [b"overflow-two"]
            held.close()
            adapter.close()


def test_pool_connections_lru_evicts_native_origin_with_visible_manager():
    with (
        loopback((200, {}, b"a1"), (200, {}, b"a2")) as (server_a, url_a),
        loopback((200, {}, b"b")) as (server_b, url_b),
    ):
        adapter = HTTPAdapter(pool_connections=1)
        with _rust_adapter_trial():
            assert adapter.send(prepared(url_a)).content == b"a1"
            assert adapter.send(prepared(url_b)).content == b"b"
            assert adapter.send(prepared(url_a)).content == b"a2"
            assert len(adapter.poolmanager.pools) == 1
            adapter.close()
        assert server_a.requests == 2
        assert len(server_a.clients) == 2
        assert server_b.requests == 1


def test_direct_and_proxy_managers_have_independent_native_lru_realms():
    with (
        loopback((200, {}, b"a1"), (200, {}, b"a2")) as (origin, origin_url),
        loopback((200, {}, b"proxy")) as (proxy, proxy_url),
    ):
        adapter = HTTPAdapter(pool_connections=1)
        proxy_root = proxy_url.rsplit("/", 1)[0]
        with _rust_adapter_trial():
            assert adapter.send(prepared(origin_url)).content == b"a1"
            assert (
                adapter.send(
                    prepared("http://origin.example/resource"),
                    proxies={"http": proxy_root},
                ).content
                == b"proxy"
            )
            assert adapter.send(prepared(origin_url)).content == b"a2"
            adapter.close()
        assert len(origin.clients) == 1
        assert len(proxy.clients) == 1


def test_each_proxy_manager_has_an_independent_native_lru_realm():
    with (
        loopback((200, {}, b"p1a"), (200, {}, b"p1b")) as (proxy_one, url_one),
        loopback((200, {}, b"p2")) as (proxy_two, url_two),
    ):
        adapter = HTTPAdapter(pool_connections=1)
        proxies = [url_one.rsplit("/", 1)[0], url_two.rsplit("/", 1)[0]]
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://one.example/"), proxies={"http": proxies[0]}
                ).content
                == b"p1a"
            )
            assert (
                adapter.send(
                    prepared("http://two.example/"), proxies={"http": proxies[1]}
                ).content
                == b"p2"
            )
            assert (
                adapter.send(
                    prepared("http://one.example/"), proxies={"http": proxies[0]}
                ).content
                == b"p1b"
            )
            adapter.close()
        assert len(proxy_one.clients) == 1
        assert len(proxy_two.clients) == 1


def test_manager_and_request_shape_mutations_fall_back_then_restore(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    request = prepared("http://example.test/")
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)

    adapter.poolmanager.connection_pool_kw["maxsize"] += 1
    with _rust_adapter_trial():
        assert adapter.send(request) is marker
    adapter.poolmanager.connection_pool_kw["maxsize"] -= 1

    adapter.poolmanager.connection_from_url = lambda *a, **k: None
    with _rust_adapter_trial():
        assert adapter.send(request) is marker
    del adapter.poolmanager.connection_from_url

    request.headers = {}
    with _rust_adapter_trial():
        assert adapter.send(request) is marker

    with loopback((200, {}, b"restored-manager")) as (server, url):
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert type(response.raw).__module__ == "requests._requests_rust"
            assert response.content == b"restored-manager"
            adapter.close()
        assert server.requests == 1


def test_preused_visible_main_or_proxy_manager_falls_back_then_restores(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    adapter.poolmanager.connection_from_url("http://preused.example/")
    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/")) is marker
    adapter.poolmanager.clear()

    adapter.proxy_manager_for("http://proxy.example/")
    with _rust_adapter_trial():
        assert (
            adapter.send(
                prepared("http://example.test/"),
                proxies={"http": "http://proxy.example/"},
            )
            is marker
        )
    adapter.proxy_manager.clear()

    with loopback((200, {}, b"restored-preuse")) as (server, url):
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert type(response.raw).__module__ == "requests._requests_rust"
            assert response.content == b"restored-preuse"
            adapter.close()
        assert server.requests == 1


def test_terminal_status_exhaustion_precedes_retry_after_parsing_and_sleep(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(
        total=0,
        status=0,
        status_forcelist={503},
        raise_on_status=False,
    )
    with loopback((503, {"Retry-After": "malformed"}, b"terminal")) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter(max_retries=retry).send(prepared(url))
            assert response.status_code == 503
            assert response.content == b"terminal"
        assert server.requests == 1
    assert sleeps == []


def test_respect_retry_after_false_never_parses_malformed_header(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(
        total=1,
        status=1,
        status_forcelist={503},
        respect_retry_after_header=False,
    )
    with loopback(
        (503, {"Retry-After": "malformed"}, b"discard"),
        (200, {}, b"done"),
    ) as (server, url):
        with _rust_adapter_trial():
            assert HTTPAdapter(max_retries=retry).send(prepared(url)).content == b"done"
        assert server.requests == 2
    assert sleeps == []


def test_invalid_retry_after_is_parsed_only_after_drain_and_connection_is_reusable():
    from urllib3.exceptions import InvalidHeader

    retry = Retry(total=1, status=1, status_forcelist={503})
    with loopback(
        (503, {"Retry-After": "malformed"}, b"discard"),
        (200, {}, b"reused"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            with pytest.raises(InvalidHeader):
                adapter.send(prepared(url))
            adapter.max_retries = Retry(total=0)
            assert adapter.send(prepared(url)).content == b"reused"
        assert server.requests == 2
        assert len(server.clients) == 1


def test_retry_drain_error_is_swallowed_and_retry_continues():
    retry = Retry(total=1, status=1, status_forcelist={503})
    with loopback(
        (503, {"Content-Length": "100"}, b"x", None, True),
        (200, {}, b"continued"),
    ) as (server, url):
        adapter = HTTPAdapter(max_retries=retry)
        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"continued"
        assert server.requests == 2


def test_force_listed_location_exhaustion_uses_raise_on_status():
    retry = Retry(
        total=1,
        redirect=0,
        status=0,
        status_forcelist={503},
        raise_on_redirect=False,
        raise_on_status=True,
    )
    with loopback((503, {"Location": "/elsewhere"}, b"redirect-terminal")) as (
        server,
        url,
    ):
        with pytest.raises(RetryError, match="too many 503 responses"):
            with _rust_adapter_trial():
                HTTPAdapter(max_retries=retry).send(prepared(url))
        assert server.requests == 1

    retry = Retry(
        total=1,
        redirect=0,
        status=0,
        status_forcelist={503},
        raise_on_redirect=True,
        raise_on_status=False,
    )
    with loopback((503, {"Location": "/elsewhere"}, b"redirect-terminal")) as (
        server,
        url,
    ):
        with _rust_adapter_trial():
            response = HTTPAdapter(max_retries=retry).send(prepared(url))
            assert response.status_code == 503
            assert response.content == b"redirect-terminal"
        assert server.requests == 1


def test_empty_retry_after_and_zero_total_do_not_enable_automatic_retry(monkeypatch):
    sleeps = []
    monkeypatch.setattr("time.sleep", sleeps.append)
    for retry, header in (
        (Retry(total=1), ""),
        (Retry(total=0), "1"),
    ):
        with loopback((503, {"Retry-After": header}, b"terminal")) as (server, url):
            with _rust_adapter_trial():
                response = HTTPAdapter(max_retries=retry).send(prepared(url))
                assert response.status_code == 503
            assert server.requests == 1
    assert sleeps == []


def test_huge_finite_timeout_falls_back_without_panicking(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with _rust_adapter_trial():
        assert (
            HTTPAdapter().send(prepared("http://example.test/"), timeout=1e308)
            is marker
        )


def test_zero_sized_pool_falls_back_before_native_effects(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    before = requests._requests_rust._adapter_pool_side_table_trial()
    for block in (False, True):
        with _rust_adapter_trial():
            assert (
                HTTPAdapter(pool_maxsize=0, pool_block=block).send(
                    prepared("http://example.test/")
                )
                is marker
            )
    assert requests._requests_rust._adapter_pool_side_table_trial() == before


def test_pickled_adapter_is_readmitted_to_native_trial():
    adapter = pickle.loads(pickle.dumps(HTTPAdapter()))
    with loopback((200, {}, b"pickled")) as (server, url):
        with _rust_adapter_trial():
            response = adapter.send(prepared(url))
            assert type(response.raw).__module__ == "requests._requests_rust"
            assert response.content == b"pickled"
        assert server.requests == 1


def test_raw_negative_read_and_zero_stream_match_urllib3_semantics():
    compressed = gzip.compress(b"staged payload")
    with loopback(
        (200, {"Content-Encoding": "gzip"}, compressed),
        (200, {}, b"wire"),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            decoded = adapter.send(prepared(url), stream=True).raw
            assert decoded.read(-1, decode_content=True) == b"staged payload"
            wire = adapter.send(prepared(url), stream=True).raw
            assert list(wire.stream(0, decode_content=False)) == []
            assert wire.read(-1, decode_content=False) == b"wire"
        assert server.requests == 2


def test_decoded_read_yields_before_wire_eof_and_close_releases_owner():
    payload = bytes(range(256)) * 200
    compressed = gzip.compress(payload)
    release = threading.Event()
    with loopback(
        (200, {"Content-Encoding": "gzip"}, (compressed[:64], release, compressed[64:]))
    ) as (server, url):
        adapter = HTTPAdapter(pool_maxsize=1, pool_block=True)
        with _rust_adapter_trial():
            raw = adapter.send(prepared(url), stream=True).raw
            assert server.first_chunk_sent.wait(1)
            chunk = raw.read(16, decode_content=True)
            if urllib3.__version__.startswith("1.26."):
                assert chunk == b""
            else:
                assert chunk == payload[:16]
            assert not release.is_set()
            raw.close()
            release.set()


def test_empty_redirect_location_consumes_status_not_redirect_budget():
    retry = Retry(
        total=1,
        status=0,
        redirect=1,
        status_forcelist={302},
        raise_on_status=True,
        raise_on_redirect=False,
    )
    with loopback((302, {"Location": ""}, b"terminal")) as (server, url):
        with pytest.raises(RetryError, match="too many 302 responses"):
            with _rust_adapter_trial():
                HTTPAdapter(max_retries=retry).send(prepared(url))
        assert server.requests == 1


@pytest.mark.parametrize(
    ("total", "expected_requests"),
    [(None, 1), (False, 1), (0, 1), (True, 2), (1, 2)],
)
def test_automatic_retry_after_uses_python_total_truthiness(total, expected_requests):
    responses = [(503, {"Retry-After": "0"}, b"first")]
    if expected_requests == 2:
        responses.append((200, {}, b"second"))
    retry = Retry(total=total)
    with loopback(*responses) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter(max_retries=retry).send(prepared(url))
            assert response.status_code == (200 if expected_requests == 2 else 503)
        assert server.requests == expected_requests


@pytest.mark.skipif(
    urllib3.__version__.startswith("1.26."), reason="jitter is urllib3 2.x only"
)
@pytest.mark.parametrize(
    ("random_value", "backoff_max", "expected_sleep"),
    [(0.5, 120, [0.5]), (2.0, 120, [2.0]), (2.0, 0, [])],
)
def test_jitter_observation_is_unclamped_and_precedes_cap(
    monkeypatch, random_value, backoff_max, expected_sleep
):
    random_calls = []
    sleeps = []
    monkeypatch.setattr(
        "random.random", lambda: random_calls.append(random_value) or random_value
    )
    monkeypatch.setattr("time.sleep", sleeps.append)
    retry = Retry(
        total=2,
        status=2,
        status_forcelist={503},
        backoff_factor=0,
        backoff_jitter=1,
        backoff_max=backoff_max,
    )
    with loopback(
        (503, {}, b"one"),
        (503, {}, b"two"),
        (200, {}, b"done"),
    ) as (server, url):
        with _rust_adapter_trial():
            assert HTTPAdapter(max_retries=retry).send(prepared(url)).content == b"done"
        assert server.requests == 3
    assert random_calls == [random_value]
    assert sleeps == expected_sleep


def test_stream_none_and_decode_mode_switch_match_urllib3():
    compressed = gzip.compress(bytes(range(256)) * 20)
    with loopback(
        (200, {"Content-Encoding": "gzip"}, compressed),
        (200, {"Content-Encoding": "gzip"}, compressed),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            raw = adapter.send(prepared(url), stream=True).raw
            assert (
                b"".join(raw.stream(None, decode_content=True))
                == bytes(range(256)) * 20
            )
            switched = adapter.send(prepared(url), stream=True).raw
            if urllib3.__version__.startswith("1.26."):
                assert switched.read(1, decode_content=True) == b""
                assert switched.read(1, decode_content=False) == compressed[1:2]
            else:
                assert switched.read(1, decode_content=True) == b"\x00"
                with pytest.raises(RuntimeError, match="decode_content=False"):
                    switched.read(1, decode_content=False)
        assert server.requests == 2


def test_decoded_retention_is_bounded_to_pending_output():
    payload = random.Random(0).randbytes(100_000)
    compressed = gzip.compress(payload)
    with loopback((200, {"Content-Encoding": "gzip"}, compressed)) as (server, url):
        with _rust_adapter_trial():
            raw = HTTPAdapter().send(prepared(url), stream=True).raw
            retained = []
            while raw.read(1024, decode_content=True):
                retained.append(raw._retained_decoded_bytes_trial())
            assert max(retained) < 16_384
        assert server.requests == 1


def test_decoder_selection_uses_live_version_capabilities_and_normalizes_case():
    import urllib3.response

    payload = b"decoder-capability-payload"
    compressed = gzip.compress(payload)
    decoders = urllib3.response.HTTPResponse.CONTENT_DECODERS
    cases = [("GZIP", compressed, payload), (None, b"plain-wire", b"plain-wire")]
    optional_wires = {
        "br": (
            urllib3.response.brotli.compress(payload)
            if "br" in decoders
            else b"unsupported-br-wire"
        ),
        "zstd": (
            urllib3.response.zstd.compress(payload)
            if "zstd" in decoders
            else b"unsupported-zstd-wire"
        ),
        "x-gzip": compressed,
    }
    for encoding, wire in optional_wires.items():
        expected = payload if encoding in decoders else wire
        cases.append((encoding, wire, expected))

    responses = [
        (200, {} if encoding is None else {"Content-Encoding": encoding}, wire)
        for encoding, wire, _ in cases
    ]
    with loopback(*responses) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            for _, _, expected in cases:
                raw = adapter.send(prepared(url), stream=True).raw
                assert raw.read(decode_content=True) == expected
        assert server.requests == len(cases)


@pytest.mark.parametrize("encoding", [None, "identity"])
def test_decode_mode_switch_without_supported_decoder_remains_wire_readable(encoding):
    headers = {} if encoding is None else {"Content-Encoding": encoding}
    with loopback((200, headers, b"abcdef")) as (server, url):
        with _rust_adapter_trial():
            raw = HTTPAdapter().send(prepared(url), stream=True).raw
            assert raw.read(2, decode_content=True) == b"ab"
            assert raw.read(2, decode_content=False) == b"cd"
        assert server.requests == 1


@pytest.mark.skipif(
    not urllib3.__version__.startswith("1.26."),
    reason="urllib3 1.26 uses its unbounded one-argument decoder ABI",
)
@pytest.mark.parametrize(
    ("encoding", "compress"),
    [
        ("GZIP", lambda payload: gzip.compress(payload, mtime=0)),
        ("deflate", zlib.compress),
    ],
)
def test_urllib3_126_finite_decoded_read_uses_exact_wire_amount_without_retention(
    encoding, compress
):
    payload = b"a" * 2_000_000
    wire = compress(payload)
    with loopback(
        (200, {"Content-Encoding": encoding}, wire),
        (200, {"Content-Encoding": encoding}, wire),
    ) as (server, url):
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            first = adapter.send(prepared(url), stream=True).raw
            assert first.read(1, decode_content=True) == b""
            assert first._retained_decoded_bytes_trial() == 0
            assert first.read(1, decode_content=False) == wire[1:2]

            amplified = adapter.send(prepared(url), stream=True).raw
            produced = amplified.read(32, decode_content=True)
            assert len(produced) > 32
            assert produced == b"a" * len(produced)
            assert amplified._retained_decoded_bytes_trial() == 0
            assert amplified.read(1, decode_content=False) == wire[32:33]
        assert server.requests == 2


@pytest.mark.skipif(
    not urllib3.__version__.startswith("1.26."),
    reason="urllib3 1.26 uses whole-output stream chunks",
)
def test_urllib3_126_stream_returns_whole_decoder_chunks_and_flushes_at_eof():
    payload = b"a" * 2_000_000
    wire = gzip.compress(payload, mtime=0)
    with loopback((200, {"Content-Encoding": "GZIP"}, wire)) as (server, url):
        with _rust_adapter_trial():
            raw = HTTPAdapter().send(prepared(url), stream=True).raw
            chunks = []
            for chunk in raw.stream(32, decode_content=True):
                chunks.append(chunk)
                assert raw._retained_decoded_bytes_trial() == 0
            assert b"".join(chunks) == payload
            assert any(len(chunk) > 32 for chunk in chunks)
        assert server.requests == 1


@pytest.mark.parametrize("target", ["proxy_manager_for", "proxy_from_url"])
def test_adapter_callable_code_mutation_falls_back_before_proxy_cache_or_socket(
    monkeypatch, target
):
    marker = object()
    adapter = HTTPAdapter()
    function = (
        HTTPAdapter.proxy_manager_for
        if target == "proxy_manager_for"
        else adapters.proxy_from_url
    )
    original = function.__code__
    calls = []
    adapters._PROVENANCE_PROBE = calls

    def replacement(*args, **kwargs):
        _PROVENANCE_PROBE.append((args, kwargs))
        return None

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with loopback((200, {}, b"restored-adapter-callable")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        try:
            function.__code__ = replacement.__code__
            with _rust_adapter_trial():
                assert (
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
                    is marker
                )
            assert calls == []
            assert adapter.proxy_manager == {}
            assert server.requests == 0
        finally:
            function.__code__ = original
            del adapters._PROVENANCE_PROBE

        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"),
                    proxies={"http": proxy_root},
                ).content
                == b"restored-adapter-callable"
            )
        assert server.requests == 1


def test_in_place_main_manager_mapping_mutations_fall_back_then_restore(monkeypatch):
    marker = object()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    adapter = HTTPAdapter()
    request = prepared("http://example.test/")
    for mapping_name, key in (
        ("key_fn_by_scheme", "http"),
        ("pool_classes_by_scheme", "http"),
    ):
        mapping = getattr(adapter.poolmanager, mapping_name)
        original = mapping[key]
        mapping[key] = object()
        with _rust_adapter_trial():
            assert adapter.send(request) is marker
        mapping[key] = original

    with loopback((200, {}, b"restored")) as (server, url):
        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"restored"
        assert server.requests == 1


@pytest.mark.parametrize(
    "mutation",
    ["pools_getitem", "key_partial_keywords", "pool_init"],
)
def test_main_manager_behavior_mutations_fall_back_then_restore(monkeypatch, mutation):
    marker = object()
    adapter = HTTPAdapter()
    manager = adapter.poolmanager
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with loopback((200, {}, b"restored-main-behavior")) as (server, url):
        with mutated_manager_behavior(manager, mutation):
            with _rust_adapter_trial():
                assert adapter.send(prepared(url)) is marker
            assert list(manager.pools._container.items()) == []
            assert server.requests == 0

        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"restored-main-behavior"
        assert server.requests == 1


def test_inherited_pool_base_behavior_mutation_falls_back_then_restores(monkeypatch):
    import urllib3.connectionpool

    marker = object()
    adapter = HTTPAdapter()
    manager = adapter.poolmanager
    function = urllib3.connectionpool.ConnectionPool.__init__
    original = function.__code__
    calls = []
    urllib3.connectionpool._PROVENANCE_PROBE = calls

    def replacement(*args, **kwargs):
        _PROVENANCE_PROBE.append((args, kwargs))

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with loopback((200, {}, b"restored-inherited-base")) as (server, url):
        try:
            function.__code__ = replacement.__code__
            with _rust_adapter_trial():
                assert adapter.send(prepared(url)) is marker
            assert calls == []
            assert list(manager.pools._container.items()) == []
            assert server.requests == 0
        finally:
            function.__code__ = original
            del urllib3.connectionpool._PROVENANCE_PROBE

        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"restored-inherited-base"
        assert server.requests == 1


def test_proxy_manager_partial_behavior_mutation_falls_back_then_restores(monkeypatch):
    marker = object()
    with loopback((200, {}, b"first"), (200, {}, b"restored-proxy-behavior")) as (
        server,
        proxy_url,
    ):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        request = prepared("http://origin.example/")
        proxies = {"http": proxy_root}
        with _rust_adapter_trial():
            assert adapter.send(request, proxies=proxies).content == b"first"
        manager = adapter.proxy_manager[proxy_root]
        monkeypatch.setattr(
            adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker
        )

        with mutated_manager_behavior(manager, "key_partial_keywords"):
            with _rust_adapter_trial():
                assert adapter.send(request, proxies=proxies) is marker
            assert server.requests == 1

        with _rust_adapter_trial():
            assert (
                adapter.send(request, proxies=proxies).content
                == b"restored-proxy-behavior"
            )
        assert server.requests == 2


def test_socks_manager_partial_behavior_mutation_falls_back_then_restores(monkeypatch):
    if not isinstance(adapters.SOCKSProxyManager, type):
        pytest.skip("PySocks is unavailable")
    marker = object()
    with socks5_loopback(b"first", b"restored-socks-behavior") as (
        observed,
        proxy_url,
    ):
        adapter = HTTPAdapter()
        request = prepared("http://origin.example/")
        proxies = {"http": proxy_url}
        with _rust_adapter_trial():
            assert adapter.send(request, proxies=proxies).content == b"first"
        manager = adapter.proxy_manager[proxy_url]
        monkeypatch.setattr(
            adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker
        )

        with mutated_manager_behavior(manager, "key_partial_keywords"):
            with _rust_adapter_trial():
                assert adapter.send(request, proxies=proxies) is marker
            assert observed == {"connections": 1, "requests": 1}

        with _rust_adapter_trial():
            assert (
                adapter.send(request, proxies=proxies).content
                == b"restored-socks-behavior"
            )
        assert observed == {"connections": 1, "requests": 2}


def test_proxy_constructor_mutation_falls_back_before_cache_or_socket(monkeypatch):
    import urllib3.poolmanager

    marker = object()
    adapter = HTTPAdapter()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    monkeypatch.setattr(urllib3.poolmanager, "ProxyManager", object())
    with _rust_adapter_trial():
        assert (
            adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": "http://proxy.example/"},
            )
            is marker
        )
    assert adapter.proxy_manager == {}


def test_proxy_manager_mutation_after_creation_falls_back_then_restores(monkeypatch):
    marker = object()
    with loopback((200, {}, b"first"), (200, {}, b"restored")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                ).content
                == b"first"
            )
        manager = adapter.proxy_manager[proxy_root]
        original = manager.connection_pool_kw["maxsize"]
        manager.connection_pool_kw["maxsize"] = original + 1
        monkeypatch.setattr(
            adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker
        )
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"),
                    proxies={"http": proxy_root},
                )
                is marker
            )
        assert server.requests == 1
        manager.connection_pool_kw["maxsize"] = original
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                ).content
                == b"restored"
            )
        assert server.requests == 2


def test_nested_main_pool_container_mutations_fall_back_then_restore(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    pools = adapter.poolmanager.pools
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    mutations = [
        ("_container", OrderedDict()),
        ("_maxsize", pools._maxsize + 1),
        ("dispose_func", object()),
        ("lock", threading.RLock()),
    ]
    with loopback((200, {}, b"restored-main")) as (server, url):
        for name, replacement in mutations:
            original = getattr(pools, name)
            setattr(pools, name, replacement)
            with _rust_adapter_trial():
                assert adapter.send(prepared(url)) is marker
            assert server.requests == 0
            setattr(pools, name, original)

        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"restored-main"
        assert server.requests == 1


def test_nested_proxy_manager_mutations_fall_back_then_restore(monkeypatch):
    marker = object()
    with loopback((200, {}, b"first"), (200, {}, b"restored-proxy")) as (
        server,
        proxy_url,
    ):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                ).content
                == b"first"
            )
        manager = adapter.proxy_manager[proxy_root]
        monkeypatch.setattr(
            adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker
        )
        mutations = [
            (manager.pools, "_container", OrderedDict()),
            (manager.pools, "_maxsize", manager.pools._maxsize + 1),
            (manager.pools, "dispose_func", object()),
            (manager, "proxy_headers", {"X-Mutated": "yes"}),
        ]
        for owner, name, replacement in mutations:
            original = getattr(owner, name)
            setattr(owner, name, replacement)
            with _rust_adapter_trial():
                assert (
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
                    is marker
                )
            assert server.requests == 1
            setattr(owner, name, original)

        manager.proxy_headers["X-Mutated"] = "yes"
        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                )
                is marker
            )
        assert server.requests == 1
        del manager.proxy_headers["X-Mutated"]

        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                ).content
                == b"restored-proxy"
            )
        assert server.requests == 2


def test_proxy_manager_class_behavior_is_frozen_before_visible_creation(monkeypatch):
    import urllib3.poolmanager

    marker = object()
    function = urllib3.poolmanager.ProxyManager.connection_from_url
    original_code = function.__code__
    adapter = HTTPAdapter()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with loopback((200, {}, b"restored-class")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        try:
            function.__code__ = (lambda self, *args, **kwargs: None).__code__
            with _rust_adapter_trial():
                assert (
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
                    is marker
                )
            assert adapter.proxy_manager == {}
            assert server.requests == 0
        finally:
            function.__code__ = original_code

        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"), proxies={"http": proxy_root}
                ).content
                == b"restored-class"
            )
        assert server.requests == 1


def test_socks_manager_class_behavior_is_frozen_before_visible_creation(monkeypatch):
    socks_manager = adapters.SOCKSProxyManager
    if not isinstance(socks_manager, type):
        pytest.skip("PySocks is unavailable")
    marker = object()
    function = socks_manager.__init__
    original_code = function.__code__
    adapter = HTTPAdapter()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    with socks5_loopback(b"restored-socks") as (observed, proxy_url):
        try:
            captured = None

            def replacement(self, *args, **kwargs):
                if captured:
                    raise AssertionError("unreachable")
                return None

            function.__code__ = replacement.__code__
            with _rust_adapter_trial():
                assert (
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_url},
                    )
                    is marker
                )
            assert adapter.proxy_manager == {}
            assert observed == {"connections": 0, "requests": 0}
        finally:
            function.__code__ = original_code

        with _rust_adapter_trial():
            assert (
                adapter.send(
                    prepared("http://origin.example/"),
                    proxies={"http": proxy_url},
                ).content
                == b"restored-socks"
            )
        assert observed == {"connections": 1, "requests": 1}


_ADAPTER_EXCEPTION_MODULE_GLOBALS = [
    "LocationValueError",
    "ProtocolError",
    "MaxRetryError",
    "ConnectTimeoutError",
    "NewConnectionError",
    "ResponseError",
    "_ProxyError",
    "_SSLError",
    "ClosedPoolError",
    "_HTTPError",
    "ReadTimeoutError",
    "_InvalidHeader",
    "InvalidURL",
    "ConnectionError",
    "ConnectTimeout",
    "RetryError",
    "ProxyError",
    "SSLError",
    "ReadTimeout",
    "InvalidHeader",
]


@pytest.mark.parametrize("name", _ADAPTER_EXCEPTION_MODULE_GLOBALS + ["OSError"])
def test_adapter_exception_globals_are_frozen_before_native_effects(monkeypatch, name):
    marker = object()
    adapter = HTTPAdapter()
    before = requests._requests_rust._adapter_pool_side_table_trial()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    owner = builtins if name == "OSError" else adapters
    monkeypatch.setattr(owner, name, object(), raising=False)

    with _rust_adapter_trial():
        assert adapter.send(prepared("http://127.0.0.1:1/resource")) is marker

    assert requests._requests_rust._adapter_pool_side_table_trial() == before
    assert adapter.proxy_manager == {}


@pytest.mark.parametrize("name", ["MaxRetryError", "ConnectionError"])
def test_missing_adapter_exception_globals_fall_back_before_native_effects(
    monkeypatch, name
):
    marker = object()
    adapter = HTTPAdapter()
    before = requests._requests_rust._adapter_pool_side_table_trial()
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)
    monkeypatch.delattr(adapters, name)

    with _rust_adapter_trial():
        assert adapter.send(prepared("http://127.0.0.1:1/resource")) is marker

    assert requests._requests_rust._adapter_pool_side_table_trial() == before


def test_transport_mapping_uses_live_adapter_target_after_the_socket_effect(
    monkeypatch,
):
    class LiveConnectionError(Exception):
        pass

    observed = []

    class LiveTarget:
        def __new__(cls, original, *, request):
            observed.append((original, request))
            return LiveConnectionError("live target")

    result = {}
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        monkeypatch.setattr(adapters, "ConnectionError", LiveTarget)
        release.set()
        worker.join(timeout=5)

    error = result["error"]
    assert isinstance(error, LiveConnectionError)
    assert len(observed) == 1
    original, mapped_request = observed[0]
    assert isinstance(original, urllib3.exceptions.ProtocolError)
    assert mapped_request is request
    assert error.__context__ is original


def test_transport_mapping_uses_live_adapter_sources_after_the_socket_effect(
    monkeypatch,
):
    class NonmatchingProtocolError(Exception):
        pass

    class ForbiddenTarget:
        def __new__(cls, *args, **kwargs):
            raise AssertionError("a nonmatching source must escape")

    result = {}
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        monkeypatch.setattr(adapters, "ProtocolError", NonmatchingProtocolError)
        monkeypatch.setattr(adapters, "ConnectionError", ForbiddenTarget)
        release.set()
        worker.join(timeout=5)

    assert type(result["error"]) is urllib3.exceptions.ProtocolError


def test_transport_live_target_failure_keeps_surrogate_context(monkeypatch):
    class ConstructorFailure(BaseException):
        pass

    failure = ConstructorFailure("target failed")

    class ExplodingTarget:
        def __new__(cls, *args, **kwargs):
            raise failure

    result = {}
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        monkeypatch.setattr(adapters, "ConnectionError", ExplodingTarget)
        release.set()
        worker.join(timeout=5)

    assert result["error"] is failure
    assert isinstance(failure.__context__, urllib3.exceptions.ProtocolError)


def test_transport_live_missing_source_keeps_surrogate_context(monkeypatch):
    result = {}
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        monkeypatch.delattr(adapters, "ProtocolError")
        release.set()
        worker.join(timeout=5)

    error = result["error"]
    assert isinstance(error, NameError)
    assert str(error) == "name 'ProtocolError' is not defined"
    assert isinstance(error.__context__, urllib3.exceptions.ProtocolError)


def test_transport_except_matching_ignores_source_instancecheck(monkeypatch):
    class MatchFailure(BaseException):
        pass

    failure = MatchFailure("match failed")

    class ExplodingMeta(type):
        def __instancecheck__(cls, instance):
            raise failure

    class ExplodingSource(Exception, metaclass=ExplodingMeta):
        pass

    result = {}
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        monkeypatch.setattr(adapters, "ProtocolError", ExplodingSource)
        release.set()
        worker.join(timeout=5)

    assert type(result["error"]) is urllib3.exceptions.ProtocolError
    assert failure.__context__ is None


def test_retry_exhaustion_uses_live_target_after_response_drain(monkeypatch):
    class LiveRetryError(Exception):
        pass

    observed = []

    class LiveTarget:
        def __new__(cls, original, *, request):
            observed.append((original, request))
            return LiveRetryError("live retry target")

    release = threading.Event()
    retry = Retry(total=0, status=0, status_forcelist={503})
    result = {}
    with loopback((503, {}, (b"x", release, b"y"))) as (server, url):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=retry).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert server.first_chunk_sent.wait(2)
        monkeypatch.setattr(adapters, "RetryError", LiveTarget)
        release.set()
        worker.join(timeout=5)

    error = result["error"]
    assert isinstance(error, LiveRetryError)
    assert len(observed) == 1
    original, mapped_request = observed[0]
    assert isinstance(original, urllib3.exceptions.MaxRetryError)
    assert isinstance(original.reason, urllib3.exceptions.ResponseError)
    assert mapped_request is request
    assert error.__context__ is original


def test_retry_exhaustion_live_nonclass_source_raises_handler_type_error(
    monkeypatch,
):
    release = threading.Event()
    retry = Retry(total=0, status=0, status_forcelist={503})
    result = {}
    with loopback((503, {}, (b"x", release, b"y"))) as (server, url):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=retry).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert server.first_chunk_sent.wait(2)
        monkeypatch.setattr(adapters, "ResponseError", object())
        release.set()
        worker.join(timeout=5)

    error = result["error"]
    assert isinstance(error, TypeError)
    assert str(error) == (
        "isinstance() arg 2 must be a type, a tuple of types, or a union"
    )
    assert isinstance(error.__context__, urllib3.exceptions.MaxRetryError)


def test_retry_exhaustion_live_source_metaclass_failure_keeps_context(monkeypatch):
    class MatchFailure(BaseException):
        pass

    failure = MatchFailure("match failed")

    class ExplodingMeta(type):
        def __instancecheck__(cls, instance):
            raise failure

    class ExplodingSource(Exception, metaclass=ExplodingMeta):
        pass

    release = threading.Event()
    retry = Retry(total=0, status=0, status_forcelist={503})
    result = {}
    with loopback((503, {}, (b"x", release, b"y"))) as (server, url):
        request = prepared(url)

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=retry).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert server.first_chunk_sent.wait(2)
        monkeypatch.setattr(adapters, "ResponseError", ExplodingSource)
        release.set()
        worker.join(timeout=5)

    assert result["error"] is failure
    assert isinstance(failure.__context__, urllib3.exceptions.MaxRetryError)


def test_truncated_native_body_maps_to_chunked_error_and_releases_raw():
    with loopback(
        (200, {"Content-Length": "20"}, b"x", None, True),
    ) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter().send(prepared(url), stream=True)
            raw = response.raw
            with pytest.raises(requests.exceptions.ChunkedEncodingError) as caught:
                list(response.iter_content(2))

        original = caught.value.args[0]
        assert isinstance(original, urllib3.exceptions.ProtocolError)
        assert caught.value.__context__ is original
        assert raw.closed is True
        assert server.requests == 1


def test_truncated_native_body_uses_live_models_target_after_send(monkeypatch):
    class LiveChunkedError(Exception):
        pass

    observed = []

    class LiveTarget:
        def __new__(cls, original):
            observed.append(original)
            return LiveChunkedError("live chunked target")

    with loopback(
        (200, {"Content-Length": "20"}, b"x", None, True),
    ) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter().send(prepared(url), stream=True)
            raw = response.raw
            monkeypatch.setattr(requests.models, "ChunkedEncodingError", LiveTarget)
            with pytest.raises(LiveChunkedError) as caught:
                list(response.iter_content(2))

        assert len(observed) == 1
        assert isinstance(observed[0], urllib3.exceptions.ProtocolError)
        assert caught.value.__context__ is observed[0]
        assert raw.closed is True
        assert server.requests == 1


def test_native_stream_read_timeout_maps_and_releases_pool_capacity():
    release = threading.Event()
    with loopback(
        (200, {}, (b"x", release, b"y")),
        (200, {}, b"reused"),
    ) as (server, url):
        adapter = HTTPAdapter(pool_maxsize=1, pool_block=True)
        try:
            with _rust_adapter_trial():
                response = adapter.send(prepared(url), stream=True, timeout=(1, 0.05))
                raw = response.raw
                iterator = response.iter_content(1)
                assert next(iterator) == b"x"
                with pytest.raises(requests.exceptions.ConnectionError) as caught:
                    next(iterator)
                original = caught.value.args[0]
                assert isinstance(original, urllib3.exceptions.ReadTimeoutError)
                assert caught.value.__context__ is original
                assert raw.closed is True
                assert adapter.send(prepared(url)).content == b"reused"
        finally:
            release.set()

        assert server.requests == 2


def test_native_decoder_python_error_is_passed_through_unchanged():
    with loopback(
        (200, {"Content-Encoding": "gzip"}, b"not-a-gzip-stream"),
    ) as (server, url):
        with _rust_adapter_trial():
            raw = HTTPAdapter().send(prepared(url), stream=True).raw
            with pytest.raises(zlib.error):
                raw.read(decode_content=True)
        assert server.requests == 1


def test_real_tls_handshake_failure_uses_adapter_ssl_handler():
    with loopback((200, {}, b"plaintext")) as (server, url):
        secure_url = url.replace("http://", "https://", 1)
        with _rust_adapter_trial():
            with pytest.raises(requests.exceptions.SSLError) as caught:
                HTTPAdapter(max_retries=Retry(total=0)).send(prepared(secure_url))

        original = caught.value.args[0]
        assert isinstance(original, urllib3.exceptions.MaxRetryError)
        assert isinstance(original.reason, urllib3.exceptions.SSLError)
        assert caught.value.__context__ is original
        assert server.requests == 0


def test_worker_panic_keeps_runtime_generation_and_native_pool_reusable():
    with loopback(
        (200, {}, b"before"),
        (200, {}, b"after"),
    ) as (server, url):
        adapter = HTTPAdapter()
        generation = requests._requests_rust._runtime_generation_trial()
        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"before"
            with pytest.raises(
                RuntimeError, match="native requests worker stopped unexpectedly"
            ) as caught:
                requests._requests_rust._panic_boundary_trial(True)
            assert caught.value.driver_generation == generation
            assert caught.value.recovery_generation == generation
            assert caught.value.recovery_result == "ok"
            assert requests._requests_rust._runtime_generation_trial() == generation
            assert adapter.send(prepared(url)).content == b"after"

        assert server.requests == 2
        assert len(server.clients) == 1

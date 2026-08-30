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
from tests_differential.runner import run_oracle_case, run_rewrite_case
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


def test_equal_but_distinct_manager_mapping_key_falls_back(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    mapping = adapter.poolmanager.connection_pool_kw
    original_items = list(mapping.items())
    original_key = next(key for key in mapping if key == "maxsize")
    replacement_key = "".join(("max", "size"))
    assert replacement_key == original_key and replacement_key is not original_key
    mapping.clear()
    mapping.update(
        (replacement_key if key is original_key else key, value)
        for key, value in original_items
    )
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)

    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/")) is marker


@pytest.mark.skipif(
    not hasattr(urllib3._collections.RecentlyUsedContainer, "_abc_registry"),
    reason="runtime exposes no mutable _abc_registry class cache",
)
def test_pypy_abc_registry_replacement_falls_back(monkeypatch):
    marker = object()
    adapter = HTTPAdapter()
    monkeypatch.setattr(
        urllib3._collections.RecentlyUsedContainer, "_abc_registry", object()
    )
    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", lambda *a, **k: marker)

    with _rust_adapter_trial():
        assert adapter.send(prepared("http://example.test/")) is marker


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
    assert isinstance(original, urllib3.exceptions.MaxRetryError)
    assert isinstance(original.reason, urllib3.exceptions.ProtocolError)
    assert mapped_request is request
    assert error.__context__ is original


def test_transport_mapping_uses_live_adapter_sources_after_the_socket_effect(
    monkeypatch,
):
    class NonmatchingMaxRetryError(Exception):
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
        monkeypatch.setattr(adapters, "MaxRetryError", NonmatchingMaxRetryError)
        monkeypatch.setattr(adapters, "ConnectionError", ForbiddenTarget)
        release.set()
        worker.join(timeout=5)

    assert type(result["error"]) is urllib3.exceptions.MaxRetryError
    assert isinstance(result["error"].reason, urllib3.exceptions.ProtocolError)


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
    assert isinstance(failure.__context__, urllib3.exceptions.MaxRetryError)
    assert isinstance(failure.__context__.reason, urllib3.exceptions.ProtocolError)


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
        monkeypatch.delattr(adapters, "MaxRetryError")
        release.set()
        worker.join(timeout=5)

    error = result["error"]
    assert isinstance(error, NameError)
    assert str(error) == "name 'MaxRetryError' is not defined"
    assert isinstance(error.__context__, urllib3.exceptions.MaxRetryError)
    assert isinstance(error.__context__.reason, urllib3.exceptions.ProtocolError)


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
        monkeypatch.setattr(adapters, "MaxRetryError", ExplodingSource)
        release.set()
        worker.join(timeout=5)

    assert type(result["error"]) is urllib3.exceptions.MaxRetryError
    assert isinstance(result["error"].reason, urllib3.exceptions.ProtocolError)
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
            if urllib3.__version__.startswith("1.26."):
                assert list(response.iter_content(2)) == [b"x"]
                assert raw.closed is True
                assert server.requests == 1
                return
            with pytest.raises(requests.exceptions.ChunkedEncodingError) as caught:
                list(response.iter_content(2))

        original = caught.value.args[0]
        assert isinstance(original, urllib3.exceptions.ProtocolError)
        assert len(original.args) == 2
        assert original.args[1].__class__.__name__ == "IncompleteRead"
        assert original.args[1].partial == 1
        assert original.args[1].expected == 19
        assert original.__context__ is original.args[1]
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
            if urllib3.__version__.startswith("1.26."):
                assert list(response.iter_content(2)) == [b"x"]
                assert observed == []
                assert raw.closed is True
                assert server.requests == 1
                return
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
                assert original.pool.host == "127.0.0.1"
                assert original.pool.port == server.server_port
                assert original.url is None
                assert original.args == (f"{original.pool}: Read timed out.",)
                assert isinstance(original.__context__, TimeoutError)
                if urllib3.__version__.startswith("1.26."):
                    assert original.__cause__ is None
                else:
                    assert original.__cause__ is original.__context__
                assert caught.value.__context__ is original
                assert raw.closed is True
                assert adapter.send(prepared(url)).content == b"reused"
        finally:
            release.set()

        assert server.requests == 2


def test_malformed_native_gzip_uses_public_requests_decode_graph_until_close():
    with loopback(
        (200, {"Content-Encoding": "gzip"}, b"not-a-gzip-stream"),
    ) as (server, url):
        with _rust_adapter_trial():
            response = HTTPAdapter().send(prepared(url), stream=True)
            raw = response.raw
            with pytest.raises(requests.exceptions.ContentDecodingError) as caught:
                list(response.iter_content(3))

            inner = caught.value.args[0]
            assert type(inner) is urllib3.exceptions.DecodeError
            assert len(inner.args) == 2
            assert inner.args[0] == (
                "Received response with content-encoding: gzip, but failed to decode it."
            )
            assert type(inner.args[1]) is zlib.error
            assert caught.value.__context__ is inner
            assert inner.__context__ is inner.args[1]
            if urllib3.__version__.startswith("1.26."):
                assert inner.__cause__ is None
                assert inner.__suppress_context__ is False
            else:
                assert inner.__cause__ is inner.args[1]
                assert inner.__suppress_context__ is True
            assert raw.closed is False
            response.close()
            assert raw.closed is True
        assert server.requests == 1


_MALFORMED_GZIP_LIFECYCLE_CASE = r"""
import os
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import requests
import urllib3
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        with self.server.lock:
            self.server.requests += 1
            encoding, body = self.server.responses.pop(0)
        self.send_response(200)
        self.send_header("Content-Encoding", encoding)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, format, *args):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.requests = 0
server.responses = [
    ("gzip", b"not-a-gzip-stream"),
    ("identity", b"second"),
    ("gzip", b"not-a-gzip-stream"),
]
server.lock = threading.Lock()
server_thread = threading.Thread(target=server.serve_forever, daemon=True)
server_thread.start()
url = f"http://127.0.0.1:{server.server_port}/resource"


def prepared():
    request = PreparedRequest()
    request.prepare(method="GET", url=url)
    return request


if os.environ.get("REQUESTS_DIFFERENTIAL_TARGET") == "rewrite":
    from requests.adapters import _rust_adapter_trial

    def trial():
        return _rust_adapter_trial()
else:

    def trial():
        return nullcontext()


adapter = HTTPAdapter(pool_maxsize=1, pool_block=True)
with trial():
    response = adapter.send(prepared(), stream=True)
    raw = response.raw
    try:
        list(response.iter_content(3))
    except requests.exceptions.ContentDecodingError as error:
        inner = error.args[0]
        graph_matches = (
            type(inner) is urllib3.exceptions.DecodeError
            and len(inner.args) == 2
            and inner.args[0]
            == "Received response with content-encoding: gzip, but failed to decode it."
            and type(inner.args[1]).__module__ == "zlib"
            and type(inner.args[1]).__name__ == "error"
            and error.__context__ is inner
            and inner.__context__ is inner.args[1]
        )
        first_error = type(error).__name__
    else:
        graph_matches = False
        first_error = None

    closed_after_error = raw.closed
    second = {}
    second_started = threading.Event()
    second_finished = threading.Event()

    def send_second():
        second_started.set()
        try:
            with trial():
                other = adapter.send(prepared(), stream=True)
                second["content"] = other.content.decode("ascii")
                other.close()
        except BaseException as error:
            second["error"] = type(error).__name__
        finally:
            second_finished.set()

    second_thread = threading.Thread(target=send_second)
    second_thread.start()
    second_started.wait(1)
    completed_before_close = second_finished.wait(0.2)
    requests_before_close = server.requests
    response.close()
    closed_after_close = raw.closed
    second_finished_after_close = second_finished.wait(3)
    second_thread.join(3)

repeat_adapter = HTTPAdapter(pool_maxsize=1, pool_block=True)
with trial():
    repeated = repeat_adapter.send(prepared(), stream=True)
    repeated_raw = repeated.raw
    try:
        next(repeated.iter_content(3))
    except requests.exceptions.ContentDecodingError:
        repeat_first_error = "ContentDecodingError"
    else:
        repeat_first_error = None
    repeat_closed_after_error = repeated_raw.closed
    try:
        next(repeated.iter_content(3))
    except StopIteration:
        repeat_second_read = "StopIteration"
    except BaseException as error:
        repeat_second_read = type(error).__name__
    else:
        repeat_second_read = "value"
    repeat_closed_after_read = repeated_raw.closed
    repeated.close()
    repeated.close()
    repeat_closed_after_close = repeated_raw.closed

side_effects.append(
    {
        "first_error": first_error,
        "graph_matches": graph_matches,
        "closed_after_error": closed_after_error,
        "completed_before_close": completed_before_close,
        "requests_before_close": requests_before_close,
        "closed_after_close": closed_after_close,
        "second_finished_after_close": second_finished_after_close,
        "second_thread_alive": second_thread.is_alive(),
        "second": second,
        "requests_after_close": server.requests,
        "repeat_first_error": repeat_first_error,
        "repeat_closed_after_error": repeat_closed_after_error,
        "repeat_second_read": repeat_second_read,
        "repeat_closed_after_read": repeat_closed_after_read,
        "repeat_closed_after_close": repeat_closed_after_close,
    }
)
server.shutdown()
server.server_close()
server_thread.join(3)
result = None
"""


def test_malformed_gzip_lifecycle_and_blocking_pool_match_frozen_oracle():
    case = {"source": _MALFORMED_GZIP_LIFECYCLE_CASE}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    expected = {
        "first_error": "ContentDecodingError",
        "graph_matches": True,
        "closed_after_error": False,
        "completed_before_close": False,
        "requests_before_close": 1,
        "closed_after_close": True,
        "second_finished_after_close": True,
        "second_thread_alive": False,
        "second": {"content": "second"},
        "requests_after_close": 3,
        "repeat_first_error": "ContentDecodingError",
        "repeat_closed_after_error": False,
        "repeat_second_read": "StopIteration",
        "repeat_closed_after_read": True,
        "repeat_closed_after_close": True,
    }
    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [expected]
    assert rewrite.observations == oracle.observations


def test_connection_refused_retains_max_retry_new_connection_graph():
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
    listener.close()
    url = f"http://127.0.0.1:{port}/resource"
    adapter = HTTPAdapter(max_retries=Retry(total=0))
    request = prepared(url)

    with _rust_adapter_trial():
        with pytest.raises(requests.exceptions.ConnectionError) as caught:
            adapter.send(request)

    outer = caught.value
    exhausted = outer.args[0]
    assert type(exhausted) is urllib3.exceptions.MaxRetryError
    assert exhausted.pool.host == "127.0.0.1"
    assert exhausted.pool.port == port
    assert exhausted.url == "/resource"
    assert type(exhausted.reason) is urllib3.exceptions.NewConnectionError
    source = exhausted.reason.__context__
    assert isinstance(source, ConnectionRefusedError)
    assert exhausted.reason.args[0].endswith(
        f"Failed to establish a new connection: {source}"
    )
    assert exhausted.reason.args[0].count(str(source)) == 1
    assert exhausted.__context__ is exhausted.reason
    if urllib3.__version__.startswith("1.26."):
        assert exhausted.reason.__cause__ is None
        assert exhausted.__cause__ is None
    else:
        assert exhausted.reason.__cause__ is source
        assert exhausted.__cause__ is exhausted.reason
    assert outer.__context__ is exhausted


def test_dns_failure_retains_versioned_max_retry_reason_graph():
    url = "http://task14-does-not-exist.invalid/resource"
    request = prepared(url)

    with _rust_adapter_trial():
        with pytest.raises(requests.exceptions.ConnectionError) as caught:
            HTTPAdapter(max_retries=Retry(total=0)).send(request)

    outer = caught.value
    exhausted = outer.args[0]
    assert type(exhausted) is urllib3.exceptions.MaxRetryError
    assert exhausted.pool.host == "task14-does-not-exist.invalid"
    assert exhausted.url == "/resource"
    source = exhausted.reason.__context__
    assert isinstance(source, socket.gaierror)
    assert source.errno == socket.EAI_NONAME
    if urllib3.__version__.startswith("1.26."):
        assert type(exhausted.reason) is urllib3.exceptions.NewConnectionError
        assert exhausted.reason.args[0].endswith(
            f"Failed to establish a new connection: {source}"
        )
        assert exhausted.reason.__cause__ is None
        assert exhausted.__cause__ is None
    else:
        assert type(exhausted.reason) is urllib3.exceptions.NameResolutionError
        assert (
            "Failed to resolve 'task14-does-not-exist.invalid'"
            in exhausted.reason.args[0]
        )
        assert exhausted.reason.__cause__ is source
        assert exhausted.__cause__ is exhausted.reason
    assert exhausted.__context__ is exhausted.reason
    assert outer.__context__ is exhausted


def test_connection_closed_during_send_retains_retry_protocol_os_graph():
    with closing_loopback_barrier() as (url, accepted, release):
        request = prepared(url)
        result = {}

        def send():
            try:
                with _rust_adapter_trial():
                    HTTPAdapter(max_retries=Retry(total=0)).send(request)
            except BaseException as error:
                result["error"] = error

        worker = threading.Thread(target=send)
        worker.start()
        assert accepted.wait(2)
        release.set()
        worker.join(timeout=5)

    outer = result["error"]
    assert type(outer) is requests.exceptions.ConnectionError
    exhausted = outer.args[0]
    assert type(exhausted) is urllib3.exceptions.MaxRetryError
    assert exhausted.pool.host == "127.0.0.1"
    assert exhausted.url == "/resource"
    protocol = exhausted.reason
    assert type(protocol) is urllib3.exceptions.ProtocolError
    assert protocol.args[0] == "Connection aborted."
    assert isinstance(protocol.args[1], OSError)
    assert protocol.__context__ is protocol.args[1]
    assert exhausted.__context__ is protocol.args[1]
    if urllib3.__version__.startswith("1.26."):
        assert protocol.__cause__ is None
        assert exhausted.__cause__ is None
    else:
        assert protocol.__cause__ is protocol.args[1]
        assert exhausted.__cause__ is protocol
    assert outer.__context__ is exhausted


def test_retry_reason_descriptor_is_reloaded_in_python_bytecode_order(monkeypatch):
    events = []

    class ReasonDescriptor:
        def __get__(self, instance, owner):
            if instance is None:
                return self
            events.append("reason")
            return instance.__dict__["reason"]

        def __set__(self, instance, value):
            instance.__dict__["reason"] = value

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
        monkeypatch.setattr(
            urllib3.exceptions.MaxRetryError,
            "reason",
            ReasonDescriptor(),
            raising=False,
        )
        release.set()
        worker.join(timeout=5)

    assert isinstance(result["error"], requests.exceptions.RetryError)
    assert events == ["reason", "reason"]


@pytest.mark.parametrize("route", ["direct", "proxy"])
def test_manager_entry_is_hard_native_commit_point(monkeypatch, route):
    calls = 0
    original_target = adapters.ConnectionError

    if route == "direct":
        from urllib3 import PoolManager

        target_code = PoolManager.connection_from_pool_key.__code__
    else:
        target_code = HTTPAdapter.proxy_manager_for.__code__

    class LiveConnectionError(requests.exceptions.ConnectionError):
        pass

    def trace(frame, event, arg):
        nonlocal calls
        if event == "call" and frame.f_code is target_code:
            calls += 1
            if calls == 1:
                adapters.ConnectionError = LiveConnectionError
        return trace

    try:
        with loopback((200, {}, b"committed")) as (server, url):
            adapter = HTTPAdapter()
            kwargs = {} if route == "direct" else {"proxies": {"http": url}}
            import sys

            sys.settrace(trace)
            try:
                with _rust_adapter_trial():
                    response = adapter.send(prepared(url), stream=True, **kwargs)
            finally:
                sys.settrace(None)

            assert response.content == b"committed"
            assert type(response.raw).__module__ == "requests._requests_rust"
            assert calls == 1
            assert server.requests == 1
            if route == "direct":
                assert len(adapter.poolmanager.pools) == 1
            else:
                assert len(adapter.proxy_manager) == 1
    finally:
        adapters.ConnectionError = original_target


_MANAGER_REFRESH_REENTRANCY_CASE = r"""
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import requests
from requests.adapters import HTTPAdapter, _rust_adapter_trial
from requests.models import PreparedRequest
from urllib3 import PoolManager


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        with self.server.lock:
            self.server.requests += 1
            body = self.server.responses.pop(0)
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, format, *args):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.requests = 0
server.responses = [b"first", b"restored"]
server.lock = threading.Lock()
server_thread = threading.Thread(target=server.serve_forever, daemon=True)
server_thread.start()
url = f"http://127.0.0.1:{server.server_port}/resource"
proxy_root = url.rsplit("/", 1)[0]
route = ROUTE


def prepared():
    request = PreparedRequest()
    request.prepare(
        method="GET",
        url=url if route == "direct" else "http://origin.example/resource",
    )
    return request


adapter = HTTPAdapter()
target_code = (
    PoolManager.connection_from_pool_key.__code__
    if route == "direct"
    else HTTPAdapter.proxy_manager_for.__code__
)
original_getattribute = PoolManager.__dict__.get("__getattribute__")
manager_entries = 0
reentries = 0
armed = False


def reentrant_getattribute(self, name):
    global reentries
    if name == "pools":
        reentries += 1
        print("REENTERED_WHILE_REFRESHING", flush=True)
        requests._requests_rust._adapter_pool_side_table_trial()
    if original_getattribute is None:
        return object.__getattribute__(self, name)
    return original_getattribute(self, name)


def trace(frame, event, arg):
    global armed, manager_entries
    if event == "call" and frame.f_code is target_code:
        manager_entries += 1
        if not armed:
            armed = True
            PoolManager.__getattribute__ = reentrant_getattribute
    return trace


kwargs = {} if route == "direct" else {"proxies": {"http": proxy_root}}
sys.settrace(trace)
try:
    with _rust_adapter_trial():
        response = adapter.send(prepared(), stream=True, **kwargs)
finally:
    sys.settrace(None)

first_native = type(response.raw).__module__ == "requests._requests_rust"
requests_after_first = server.requests
visible_effects = (
    len(adapter.poolmanager.pools)
    if route == "direct"
    else len(adapter.proxy_manager)
)
response.close()
adapter.close()

if original_getattribute is None:
    del PoolManager.__getattribute__
else:
    PoolManager.__getattribute__ = original_getattribute
restored = PoolManager.__dict__.get("__getattribute__") is original_getattribute

restored_adapter = HTTPAdapter()
with _rust_adapter_trial():
    restored_response = restored_adapter.send(prepared(), stream=True, **kwargs)
restored_native = (
    type(restored_response.raw).__module__ == "requests._requests_rust"
)
restored_content = restored_response.content.decode("ascii")
restored_response.close()
restored_adapter.close()

side_effects.append(
    {
        "route": route,
        "manager_entries": manager_entries,
        "reentries": reentries,
        "first_native": first_native,
        "requests_after_first": requests_after_first,
        "visible_effects": visible_effects,
        "restored": restored,
        "restored_native": restored_native,
        "restored_content": restored_content,
        "requests_after_restored": server.requests,
    }
)
server.shutdown()
server.server_close()
server_thread.join(3)
result = None
"""


@pytest.mark.parametrize("route", ["direct", "proxy"])
def test_manager_refresh_reentrancy_never_holds_adapter_registry_lock(
    monkeypatch, route
):
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_TIMEOUT", "2")
    source = _MANAGER_REFRESH_REENTRANCY_CASE.replace("ROUTE", repr(route), 1)
    rewrite = run_rewrite_case({"source": source})
    assert rewrite.observations["exception"] is None
    [observed] = rewrite.observations["side_effects"]
    assert observed["route"] == route
    assert observed["manager_entries"] == 1
    assert observed["reentries"] >= 1
    assert observed["first_native"] is True
    assert observed["requests_after_first"] == 1
    assert observed["visible_effects"] == 1
    assert observed["restored"] is True
    assert observed["restored_native"] is True
    assert observed["restored_content"] == "restored"
    assert observed["requests_after_restored"] == 2


_CONCURRENT_ADAPTER_ADMISSION_CASE = r"""
import os
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import requests
from requests import adapters
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest
from urllib3 import PoolManager


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        with self.server.lock:
            self.server.requests += 1
            body = self.server.responses.pop(0)
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, format, *args):
        pass


def start_server(*responses):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.requests = 0
    server.responses = list(responses)
    server.lock = threading.Lock()
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    return server, worker, f"http://127.0.0.1:{server.server_port}/resource"


servers = [
    start_server(b"first-0", b"restored"),
    start_server(b"first-1"),
]
route = ROUTE
mixed = MIXED
adapter = HTTPAdapter()
fallback_calls = 0

if os.environ.get("REQUESTS_DIFFERENTIAL_TARGET") == "rewrite":
    from requests.adapters import _rust_adapter_trial

    def trial(index):
        return nullcontext() if mixed and index == 1 else _rust_adapter_trial()

    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        global fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    adapters._HTTP_ADAPTER_COMPAT_SEND = counted_compat_send
else:

    def trial(index):
        return nullcontext()


def prepared(index):
    request = PreparedRequest()
    request.prepare(
        method="GET",
        url=(
            servers[index][2]
            if route == "direct"
            else f"http://origin-{index}.example/resource"
        ),
    )
    return request


def send_kwargs(index):
    if route == "direct":
        return {}
    return {"proxies": {"http": servers[index][2].rsplit("/", 1)[0]}}


target_code = (
    PoolManager.connection_from_pool_key.__code__
    if route == "direct" or mixed
    else HTTPAdapter.proxy_manager_for.__code__
)
admission_barrier = threading.Barrier(2)
first_manager_entry = threading.Event()
manager_entries = 0
requests_before_release = None
trace_lock = threading.Lock()


def trace(frame, event, arg):
    global manager_entries, requests_before_release
    barrier_event = "return" if mixed else "call"
    if event == barrier_event and frame.f_code is target_code:
        with trace_lock:
            manager_entries += 1
            entry = manager_entries
            if entry == 1:
                first_manager_entry.set()
            if entry == 2:
                requests_before_release = sum(server.requests for server, _, _ in servers)
        if entry <= 2:
            admission_barrier.wait(3)
    return trace


successes = []
errors = []
result_lock = threading.Lock()


def send(index):
    try:
        with trial(index):
            response = adapter.send(
                prepared(index),
                stream=True,
                **send_kwargs(index),
            )
        observed = (
            index,
            response.content.decode("ascii"),
            type(response.raw).__module__ == "requests._requests_rust",
        )
        response.close()
        with result_lock:
            successes.append(observed)
    except BaseException as error:
        with result_lock:
            errors.append((index, type(error).__name__, str(error)))


threading.settrace(trace)
workers = [threading.Thread(target=send, args=(index,)) for index in range(2)]
workers[0].start()
first_manager_entry.wait(3)
workers[1].start()
for worker in workers:
    worker.join(5)
threading.settrace(None)

requests_after_concurrent = [server.requests for server, _, _ in servers]
manager_effects = (
    len(adapter.poolmanager.pools)
    if route == "direct"
    else len(adapter.proxy_manager)
)
with trial(0):
    restored = adapter.send(
        prepared(0),
        stream=True,
        **send_kwargs(0),
    )
restored_native = type(restored.raw).__module__ == "requests._requests_rust"
restored_content = restored.content.decode("ascii")
restored.close()
adapter.close()

side_effects.append(
    {
        "route": route,
        "manager_entries": manager_entries,
        "requests_before_release": requests_before_release,
        "successes": sorted(successes),
        "errors": sorted(errors),
        "workers_alive": [worker.is_alive() for worker in workers],
        "requests_after_concurrent": requests_after_concurrent,
        "manager_effects": manager_effects,
        "fallback_calls": fallback_calls,
        "restored_native": restored_native,
        "restored_content": restored_content,
        "requests_after_restored": [
            server.requests for server, _, _ in servers
        ],
    }
)
for server, worker, _ in servers:
    server.shutdown()
    server.server_close()
    worker.join(3)
result = None
"""


@pytest.mark.parametrize("route", ["direct", "proxy"])
def test_concurrent_same_adapter_admissions_match_frozen_oracle(route):
    source = _CONCURRENT_ADAPTER_ADMISSION_CASE.replace(
        "ROUTE", repr(route), 1
    ).replace("MIXED", "False", 1)
    case = {"source": source}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    common = {
        "route": route,
        "manager_entries": 2,
        "requests_before_release": 0,
        "errors": [],
        "workers_alive": [False, False],
        "requests_after_concurrent": [1, 1],
        "manager_effects": 2,
        "fallback_calls": 0,
        "restored_content": "restored",
        "requests_after_restored": [2, 1],
    }
    oracle_expected = {
        **common,
        "successes": [
            [0, "first-0", False],
            [1, "first-1", False],
        ],
        "restored_native": False,
    }
    rewrite_expected = {
        **common,
        "successes": [
            [0, "first-0", True],
            [1, "first-1", True],
        ],
        "restored_native": True,
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [oracle_expected]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [rewrite_expected]


def test_concurrent_native_and_python_proxy_admissions_restore_native_proof():
    source = _CONCURRENT_ADAPTER_ADMISSION_CASE.replace(
        "ROUTE", repr("proxy"), 1
    ).replace("MIXED", "True", 1)
    case = {"source": source}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    common = {
        "route": "proxy",
        "manager_entries": 2,
        "requests_before_release": 0,
        "errors": [],
        "workers_alive": [False, False],
        "requests_after_concurrent": [1, 1],
        "manager_effects": 2,
        "restored_content": "restored",
        "requests_after_restored": [2, 1],
    }
    oracle_expected = {
        **common,
        "successes": [
            [0, "first-0", False],
            [1, "first-1", False],
        ],
        "fallback_calls": 0,
        "restored_native": False,
    }
    rewrite_expected = {
        **common,
        "successes": [
            [0, "first-0", True],
            [1, "first-1", False],
        ],
        "fallback_calls": 1,
        "restored_native": True,
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [oracle_expected]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [rewrite_expected]


def test_concurrent_native_and_python_direct_admissions_restore_native_proof():
    source = _CONCURRENT_ADAPTER_ADMISSION_CASE.replace(
        "ROUTE", repr("direct"), 1
    ).replace("MIXED", "True", 1)
    case = {"source": source}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    common = {
        "route": "direct",
        "manager_entries": 2,
        "requests_before_release": 0,
        "errors": [],
        "workers_alive": [False, False],
        "requests_after_concurrent": [1, 1],
        "manager_effects": 2,
        "restored_content": "restored",
        "requests_after_restored": [2, 1],
    }
    oracle_expected = {
        **common,
        "successes": [
            [0, "first-0", False],
            [1, "first-1", False],
        ],
        "fallback_calls": 0,
        "restored_native": False,
    }
    rewrite_expected = {
        **common,
        "successes": [
            [0, "first-0", True],
            [1, "first-1", False],
        ],
        "fallback_calls": 1,
        "restored_native": True,
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [oracle_expected]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [rewrite_expected]


def test_mixed_direct_manager_pool_refresh_retries_torn_observation_and_restores(
    monkeypatch,
):
    import sys

    from urllib3 import PoolManager

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with loopback(
        (200, {}, b"first"),
        (200, {}, b"restored"),
    ) as (server, url):
        adapter = HTTPAdapter()
        original_getattribute = PoolManager.__dict__.get("__getattribute__")
        pool_reads = 0
        injected = 0
        armed = False

        def restore_getattribute():
            if original_getattribute is None:
                del PoolManager.__getattribute__
            else:
                PoolManager.__getattribute__ = original_getattribute

        def getattribute(manager, name):
            nonlocal injected, pool_reads
            value = (
                object.__getattribute__(manager, name)
                if original_getattribute is None
                else original_getattribute(manager, name)
            )
            if manager is adapter.poolmanager and name == "pools":
                pool_reads += 1
                if pool_reads == 2:
                    restore_getattribute()
                    injected += 1
                    manager.connection_from_url("http://mixed-direct.example/resource")
            return value

        target_code = PoolManager.connection_from_pool_key.__code__

        def trace(frame, event, arg):
            nonlocal armed
            if not armed and event == "return" and frame.f_code is target_code:
                armed = True
                PoolManager.__getattribute__ = getattribute
            return trace

        sys.settrace(trace)
        try:
            with _rust_adapter_trial():
                first = adapter.send(prepared(url), stream=True)
        finally:
            sys.settrace(None)
            if PoolManager.__dict__.get("__getattribute__") is getattribute:
                restore_getattribute()

        assert first.content == b"first"
        assert type(first.raw).__module__ == "requests._requests_rust"
        first.close()
        assert injected == 1
        assert pool_reads == 2
        assert len(adapter.poolmanager.pools) == 2

        with _rust_adapter_trial():
            restored = adapter.send(prepared(url), stream=True)

        assert restored.content == b"restored"
        assert type(restored.raw).__module__ == "requests._requests_rust"
        restored.close()
        assert fallback_calls == 0
        assert server.requests == 2


def test_new_proxy_manager_wrong_destination_is_not_recorded_after_commit(
    monkeypatch,
):
    import sys

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with (
        loopback((200, {}, b"restored")) as (selected_server, selected_url),
        loopback((200, {}, b"wrong")) as (wrong_server, wrong_url),
    ):
        selected_proxy = selected_url.rsplit("/", 1)[0]
        wrong_proxy = wrong_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        wrong_manager = urllib3.ProxyManager(
            wrong_proxy,
            num_pools=adapter._pool_connections,
            maxsize=adapter._pool_maxsize,
            block=adapter._pool_block,
        )
        observed = {}

        def trace(frame, event, arg):
            if (
                "canonical" not in observed
                and event == "return"
                and frame.f_code is HTTPAdapter.proxy_manager_for.__code__
            ):
                observed["canonical"] = arg
                adapter.proxy_manager[selected_proxy] = wrong_manager
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": selected_proxy},
                    )
        finally:
            sys.settrace(None)

        assert selected_server.requests == 0
        assert wrong_server.requests == 0
        assert fallback_calls == 0

        adapter.proxy_manager[selected_proxy] = observed["canonical"]
        with _rust_adapter_trial():
            restored = adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": selected_proxy},
            )
        assert restored.content == b"restored"
        assert type(restored.raw).__module__ == "urllib3.response"
        assert selected_server.requests == 1
        assert wrong_server.requests == 0
        assert fallback_calls == 1
        restored.close()
        adapter.close()


@pytest.mark.parametrize("mutation", ["connection_pool_kw", "proxy_headers"])
def test_new_proxy_manager_immutable_mutation_is_not_recorded_after_commit(
    monkeypatch, mutation
):
    import sys

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with loopback((200, {}, b"restored")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        observed = {}

        def trace(frame, event, arg):
            if (
                "manager" not in observed
                and event == "return"
                and frame.f_code is HTTPAdapter.proxy_manager_for.__code__
            ):
                observed["manager"] = arg
                if mutation == "connection_pool_kw":
                    observed["original"] = arg.connection_pool_kw["maxsize"]
                    arg.connection_pool_kw["maxsize"] += 1
                else:
                    arg.proxy_headers["X-Mutated"] = "yes"
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
        finally:
            sys.settrace(None)

        assert server.requests == 0
        assert fallback_calls == 0
        manager = observed["manager"]
        if mutation == "connection_pool_kw":
            manager.connection_pool_kw["maxsize"] = observed["original"]
        else:
            del manager.proxy_headers["X-Mutated"]

        with _rust_adapter_trial():
            restored = adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": proxy_root},
            )
        assert restored.content == b"restored"
        assert type(restored.raw).__module__ == "urllib3.response"
        assert server.requests == 1
        assert fallback_calls == 1
        restored.close()
        adapter.close()


@pytest.mark.parametrize(
    ("mapping_name", "replacement_key"),
    [
        ("pool_classes_by_scheme", "https"),
        ("key_fn_by_scheme", "https"),
    ],
)
def test_new_http_proxy_manager_routing_mutation_is_not_recorded_after_commit(
    monkeypatch, mapping_name, replacement_key
):
    import sys

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with loopback((200, {}, b"restored-routing")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        observed = {}

        def trace(frame, event, arg):
            if (
                "manager" not in observed
                and event == "return"
                and frame.f_code is HTTPAdapter.proxy_headers.__code__
                and proxy_root in adapter.proxy_manager
            ):
                manager = adapter.proxy_manager[proxy_root]
                observed["manager"] = manager
                mapping = getattr(manager, mapping_name)
                observed["original"] = mapping["http"]
                mapping["http"] = mapping[replacement_key]
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
        finally:
            sys.settrace(None)
            if "manager" in observed:
                getattr(observed["manager"], mapping_name)["http"] = observed[
                    "original"
                ]

        assert server.requests == 0
        assert fallback_calls == 0

        del adapter.proxy_manager[proxy_root]
        with _rust_adapter_trial():
            restored = adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": proxy_root},
            )
        assert restored.content == b"restored-routing"
        assert type(restored.raw).__module__ == "requests._requests_rust"
        assert server.requests == 1
        assert fallback_calls == 0
        restored.close()
        adapter.close()


@pytest.mark.parametrize("mutation", ["proxy_url", "_socks_options"])
def test_new_socks_manager_provenance_mutation_is_not_recorded_after_commit(
    monkeypatch, mutation
):
    import sys

    if not isinstance(adapters.SOCKSProxyManager, type):
        pytest.skip("PySocks is unavailable")

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with socks5_loopback(b"restored-socks") as (observed, proxy_url):
        adapter = HTTPAdapter()
        request = prepared("http://origin.example/")
        observed_manager = {}

        def trace(frame, event, arg):
            if (
                "manager" not in observed_manager
                and event == "return"
                and frame.f_code is HTTPAdapter.proxy_manager_for.__code__
            ):
                observed_manager["manager"] = arg
                if mutation == "proxy_url":
                    observed_manager["original"] = arg.proxy_url
                    arg.proxy_url = "socks5h://wrong.example:1"
                else:
                    options = arg.connection_pool_kw["_socks_options"]
                    observed_manager["original"] = options["proxy_host"]
                    options["proxy_host"] = "wrong.example"
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        request,
                        stream=True,
                        proxies={"http": proxy_url},
                    )
        finally:
            sys.settrace(None)

        assert observed == {"connections": 0, "requests": 0}
        assert fallback_calls == 0
        manager = observed_manager["manager"]
        if mutation == "proxy_url":
            manager.proxy_url = observed_manager["original"]
        else:
            manager.connection_pool_kw["_socks_options"]["proxy_host"] = (
                observed_manager["original"]
            )

        with _rust_adapter_trial():
            restored = adapter.send(
                request,
                stream=True,
                proxies={"http": proxy_url},
            )

        assert restored.content == b"restored-socks"
        assert type(restored.raw).__module__ == "urllib3.response"
        restored.close()
        assert observed == {"connections": 1, "requests": 1}
        assert fallback_calls == 1


@pytest.mark.parametrize(
    ("mapping_name", "replacement_key"),
    [
        ("pool_classes_by_scheme", "https"),
        ("key_fn_by_scheme", "https"),
    ],
)
def test_new_socks_proxy_manager_routing_mutation_is_not_recorded_after_commit(
    monkeypatch, mapping_name, replacement_key
):
    import sys

    if not isinstance(adapters.SOCKSProxyManager, type):
        pytest.skip("PySocks is unavailable")

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with socks5_loopback(b"restored-socks-routing") as (observed, proxy_url):
        adapter = HTTPAdapter()
        request = prepared("http://origin.example/")
        observed_manager = {}

        def trace(frame, event, arg):
            if (
                "manager" not in observed_manager
                and event == "return"
                and frame.f_code is adapters.get_auth_from_url.__code__
                and proxy_url in adapter.proxy_manager
            ):
                manager = adapter.proxy_manager[proxy_url]
                observed_manager["manager"] = manager
                mapping = getattr(manager, mapping_name)
                observed_manager["original"] = mapping["http"]
                mapping["http"] = mapping[replacement_key]
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        request,
                        stream=True,
                        proxies={"http": proxy_url},
                    )
        finally:
            sys.settrace(None)
            if "manager" in observed_manager:
                getattr(observed_manager["manager"], mapping_name)["http"] = (
                    observed_manager["original"]
                )

        assert observed == {"connections": 0, "requests": 0}
        assert fallback_calls == 0

        del adapter.proxy_manager[proxy_url]
        with _rust_adapter_trial():
            restored = adapter.send(
                request,
                stream=True,
                proxies={"http": proxy_url},
            )
        assert restored.content == b"restored-socks-routing"
        assert type(restored.raw).__module__ == "requests._requests_rust"
        restored.close()
        assert observed == {"connections": 1, "requests": 1}
        assert fallback_calls == 0


@pytest.mark.parametrize(
    "source",
    [
        "http_pool_classes_by_scheme",
        "key_fn_by_scheme",
        "socks_pool_classes_by_scheme",
    ],
)
def test_replaced_live_routing_source_falls_back_before_native_effects(
    monkeypatch, source
):
    import urllib3.poolmanager as poolmanager

    if source == "socks_pool_classes_by_scheme" and not isinstance(
        adapters.SOCKSProxyManager, type
    ):
        pytest.skip("PySocks is unavailable")

    if source == "socks_pool_classes_by_scheme":
        owner = adapters.SOCKSProxyManager
        name = "pool_classes_by_scheme"
        proxy = "socks5://127.0.0.1:1"
    else:
        owner = poolmanager
        name = source.removeprefix("http_")
        proxy = "http://127.0.0.1:1"
    replacement = dict(getattr(owner, name))
    replacement["http"] = replacement["https"]
    monkeypatch.setattr(owner, name, replacement)

    marker = object()
    fallback_calls = 0

    def compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return marker

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", compat_send)
    adapter = HTTPAdapter()
    before = requests._requests_rust._adapter_pool_side_table_trial()

    with _rust_adapter_trial():
        response = adapter.send(
            prepared("http://origin.example/"),
            proxies={"http": proxy},
        )

    assert response is marker
    assert fallback_calls == 1
    assert adapter.proxy_manager == {}
    assert len(adapter.poolmanager.pools) == 0
    assert requests._requests_rust._adapter_pool_side_table_trial() == before
    adapter.close()


def test_replaced_live_routing_source_does_not_repeat_adversarial_key_equality(
    monkeypatch,
):
    import urllib3.poolmanager as poolmanager

    class ProbeError(Exception):
        pass

    comparisons = []

    class HttpKey:
        def __hash__(self):
            return hash("http")

        def __eq__(self, other):
            comparisons.append(other)
            if len(comparisons) > 1:
                raise ProbeError("routing key compared more than once")
            return other == "http"

    canonical = poolmanager.pool_classes_by_scheme
    replacement = {
        HttpKey(): canonical["http"],
        "https": canonical["https"],
    }
    monkeypatch.setattr(poolmanager, "pool_classes_by_scheme", replacement)

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with loopback((200, {}, b"compat-routing")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        before = requests._requests_rust._adapter_pool_side_table_trial()

        with _rust_adapter_trial():
            response = adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": proxy_root},
            )

        assert response.content == b"compat-routing"
        assert type(response.raw).__module__ == "urllib3.response"
        assert comparisons == ["http"]
        assert fallback_calls == 1
        assert server.requests == 1
        assert list(adapter.proxy_manager) == [proxy_root]
        assert len(adapter.proxy_manager[proxy_root].pools) == 1
        assert requests._requests_rust._adapter_pool_side_table_trial() == before
        response.close()
        adapter.close()


_MISSING_LIVE_ROUTING_SOURCE_CASE = r"""
import os
from contextlib import nullcontext

import urllib3.poolmanager as poolmanager
from requests import adapters
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest


adapter = HTTPAdapter()
fallback_calls = 0
events = []
if os.environ.get("REQUESTS_DIFFERENTIAL_TARGET") == "rewrite":
    from requests.adapters import _rust_adapter_trial

    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        global fallback_calls
        fallback_calls += 1
        events.append("compat")
        return original_compat_send(*args, **kwargs)

    adapters._HTTP_ADAPTER_COMPAT_SEND = counted_compat_send

    def trial():
        return _rust_adapter_trial()
else:

    def trial():
        return nullcontext()


delattr(poolmanager, SOURCE_NAME)
request = PreparedRequest()
request.prepare(method="GET", url="http://origin.example/resource")
try:
    with trial():
        adapter.send(
            request,
            proxies={"http": "http://127.0.0.1:1"},
        )
except BaseException as error:
    events.append(type(error).__name__)
    observed_error = {
        "module": type(error).__module__,
        "name": type(error).__name__,
        "args": list(error.args),
    }
else:
    observed_error = None

side_effects.append(
    {
        "error": observed_error,
        "events": events,
        "fallback_calls": fallback_calls,
        "proxy_managers": len(adapter.proxy_manager),
        "main_pools": len(adapter.poolmanager.pools),
    }
)
adapter.close()
result = None
"""


@pytest.mark.parametrize(
    "source_name",
    ["pool_classes_by_scheme", "key_fn_by_scheme"],
)
def test_missing_live_routing_source_preserves_authoritative_error_order(
    source_name,
):
    source = _MISSING_LIVE_ROUTING_SOURCE_CASE.replace(
        "SOURCE_NAME", repr(source_name), 1
    )
    case = {"source": source}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    error = {
        "module": "builtins",
        "name": "NameError",
        "args": [f"name '{source_name}' is not defined"],
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [
        {
            "error": error,
            "events": ["NameError"],
            "fallback_calls": 0,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [
        {
            "error": error,
            "events": ["compat", "NameError"],
            "fallback_calls": 1,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]


_MISSING_LIVE_SOCKS_MANAGER_CASE = r"""
import os
from contextlib import nullcontext

from requests import adapters
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest


adapter = HTTPAdapter()
fallback_calls = 0
events = []
if os.environ.get("REQUESTS_DIFFERENTIAL_TARGET") == "rewrite":
    from requests.adapters import _rust_adapter_trial

    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        global fallback_calls
        fallback_calls += 1
        events.append("compat")
        return original_compat_send(*args, **kwargs)

    adapters._HTTP_ADAPTER_COMPAT_SEND = counted_compat_send

    def trial():
        return _rust_adapter_trial()
else:

    def trial():
        return nullcontext()


delattr(adapters, "SOCKSProxyManager")
request = PreparedRequest()
request.prepare(method="GET", url="http://origin.example/resource")
try:
    with trial():
        adapter.send(
            request,
            proxies={"http": "socks5://127.0.0.1:1"},
        )
except BaseException as error:
    events.append(type(error).__name__)
    observed_error = {
        "module": type(error).__module__,
        "name": type(error).__name__,
        "args": list(error.args),
    }
else:
    observed_error = None

side_effects.append(
    {
        "error": observed_error,
        "events": events,
        "fallback_calls": fallback_calls,
        "proxy_managers": len(adapter.proxy_manager),
        "main_pools": len(adapter.poolmanager.pools),
    }
)
adapter.close()
result = None
"""

_RAISING_LIVE_SOCKS_MANAGER_OBSERVATION_CASE = _MISSING_LIVE_SOCKS_MANAGER_CASE.replace(
    'delattr(adapters, "SOCKSProxyManager")',
    r"""
class ObservationError(Exception):
    pass


def observe_missing_attribute(name):
    if name == "SOCKSProxyManager":
        events.append("observe")
        raise ObservationError("SOCKS manager observation failed")
    raise AttributeError(name)


adapters.__getattr__ = observe_missing_attribute
delattr(adapters, "SOCKSProxyManager")
""",
    1,
)


def test_missing_live_socks_manager_preserves_authoritative_error_order():
    case = {"source": _MISSING_LIVE_SOCKS_MANAGER_CASE}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    error = {
        "module": "builtins",
        "name": "NameError",
        "args": ["name 'SOCKSProxyManager' is not defined"],
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [
        {
            "error": error,
            "events": ["NameError"],
            "fallback_calls": 0,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [
        {
            "error": error,
            "events": ["compat", "NameError"],
            "fallback_calls": 1,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]


def test_raising_live_socks_manager_observation_uses_authoritative_error():
    case = {"source": _RAISING_LIVE_SOCKS_MANAGER_OBSERVATION_CASE}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    error = {
        "module": "builtins",
        "name": "NameError",
        "args": ["name 'SOCKSProxyManager' is not defined"],
    }

    assert oracle.observations["exception"] is None
    assert oracle.observations["side_effects"] == [
        {
            "error": error,
            "events": ["NameError"],
            "fallback_calls": 0,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [
        {
            "error": error,
            "events": ["observe", "compat", "NameError"],
            "fallback_calls": 1,
            "proxy_managers": 0,
            "main_pools": 0,
        }
    ]


def test_rejected_proxy_attempt_does_not_admit_independently_preused_manager(
    monkeypatch,
):
    import sys

    fallback_calls = 0
    original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND

    def counted_compat_send(*args, **kwargs):
        nonlocal fallback_calls
        fallback_calls += 1
        return original_compat_send(*args, **kwargs)

    monkeypatch.setattr(adapters, "_HTTP_ADAPTER_COMPAT_SEND", counted_compat_send)
    with loopback((200, {}, b"selected")) as (server, proxy_url):
        proxy_root = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        rejected = {}

        def trace(frame, event, arg):
            if (
                "manager" not in rejected
                and event == "return"
                and frame.f_code is HTTPAdapter.proxy_manager_for.__code__
            ):
                rejected["manager"] = arg
                rejected["maxsize"] = arg.connection_pool_kw["maxsize"]
                arg.connection_pool_kw["maxsize"] += 1
            return trace

        sys.settrace(trace)
        try:
            with pytest.raises(
                RuntimeError,
                match="proxy manager state changed after native send commitment",
            ):
                with _rust_adapter_trial():
                    adapter.send(
                        prepared("http://origin.example/"),
                        proxies={"http": proxy_root},
                    )
        finally:
            sys.settrace(None)
            if "manager" in rejected:
                rejected["manager"].connection_pool_kw["maxsize"] = rejected["maxsize"]

        assert server.requests == 0
        assert fallback_calls == 0

        replacement = urllib3.ProxyManager(
            proxy_root,
            num_pools=adapter._pool_connections,
            maxsize=adapter._pool_maxsize,
            block=adapter._pool_block,
        )
        replacement.connection_from_url("http://preused.example/")
        assert len(replacement.pools) == 1
        adapter.proxy_manager[proxy_root] = replacement

        with _rust_adapter_trial():
            response = adapter.send(
                prepared("http://origin.example/"),
                proxies={"http": proxy_root},
            )
        assert response.content == b"selected"
        assert type(response.raw).__module__ == "urllib3.response"
        assert server.requests == 1
        assert fallback_calls == 1
        response.close()
        adapter.close()


_REPLACED_PROXY_MANAGER_MAPPING_CASE = r"""
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from requests import adapters
from requests.adapters import HTTPAdapter, _rust_adapter_trial
from requests.models import PreparedRequest


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.server.requests += 1
        body = b"python"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, format, *args):
        pass


class AlternatingCopyDict(dict):
    def __init__(self):
        super().__init__()
        self.copy_calls = 0

    def copy(self):
        self.copy_calls += 1
        return dict(self) if self.copy_calls % 2 else {}


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.requests = 0
server_thread = threading.Thread(target=server.serve_forever, daemon=True)
server_thread.start()
proxy_url = f"http://127.0.0.1:{server.server_port}"
adapter = HTTPAdapter()
mapping_kind = MAPPING_KIND
adapter.proxy_manager = (
    {} if mapping_kind == "exact replacement" else AlternatingCopyDict()
)
fallback_calls = 0
original_compat_send = adapters._HTTP_ADAPTER_COMPAT_SEND


def counted_compat_send(*args, **kwargs):
    global fallback_calls
    fallback_calls += 1
    return original_compat_send(*args, **kwargs)


adapters._HTTP_ADAPTER_COMPAT_SEND = counted_compat_send
request = PreparedRequest()
request.prepare(method="GET", url="http://origin.example/resource")
with _rust_adapter_trial():
    response = adapter.send(request, proxies={"http": proxy_url})

side_effects.append(
    {
        "mapping_kind": mapping_kind,
        "content": response.content.decode("ascii"),
        "native": type(response.raw).__module__ == "requests._requests_rust",
        "fallback_calls": fallback_calls,
        "requests": server.requests,
        "copy_calls": getattr(adapter.proxy_manager, "copy_calls", None),
    }
)
response.close()
adapter.close()
server.shutdown()
server.server_close()
server_thread.join(3)
result = None
"""


@pytest.mark.parametrize(
    ("mapping_kind", "copy_calls"),
    [("exact replacement", None), ("alternating subclass", 0)],
)
def test_replaced_proxy_manager_mapping_falls_back_promptly(
    monkeypatch, mapping_kind, copy_calls
):
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_TIMEOUT", "3")
    source = _REPLACED_PROXY_MANAGER_MAPPING_CASE.replace(
        "MAPPING_KIND", repr(mapping_kind), 1
    )
    rewrite = run_rewrite_case({"source": source})
    assert rewrite.observations["exception"] is None
    assert rewrite.observations["side_effects"] == [
        {
            "mapping_kind": mapping_kind,
            "content": "python",
            "native": False,
            "fallback_calls": 1,
            "requests": 1,
            "copy_calls": copy_calls,
        }
    ]


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

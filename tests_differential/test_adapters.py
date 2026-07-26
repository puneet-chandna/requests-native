from __future__ import annotations

import gc
import gzip
import pickle
import threading
import weakref
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from urllib3.util.retry import Retry

import requests
from requests import adapters
from requests.adapters import HTTPAdapter, _rust_adapter_trial
from requests.exceptions import RetryError
from requests.models import PreparedRequest


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


def prepared(url, body=None, method="GET"):
    request = PreparedRequest()
    request.prepare(method=method, url=url, headers={"X-Test": "adapter"}, data=body)
    return request


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
            assert chunk == payload[:16]
            assert not release.is_set()
            raw.close()
            release.set()

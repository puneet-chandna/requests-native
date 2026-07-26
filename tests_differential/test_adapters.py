from __future__ import annotations

import gc
import pickle
import threading
import weakref
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
            status, headers, body = server.responses.pop(0)
        if status is None:
            self.close_connection = True
            self.connection.close()
            return
        self.send_response(status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_POST = do_GET

    def log_message(self, format, *args):
        pass


@contextmanager
def loopback(*responses):
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.responses = list(responses)
    server.requests = 0
    server.clients = set()
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

from __future__ import annotations

from types import SimpleNamespace

from tests_differential.test_adapters import loopback, prepared

import requests
import requests.adapters as adapters_module
from requests.adapters import BaseAdapter, HTTPAdapter
from requests.models import PreparedRequest, Response
from requests.sessions import Session


def reset_native_telemetry():
    extension = requests._requests_rust
    extension._public_facade_pump_trial("reset")
    extension._runtime_submission_trial("reset")
    return extension._adapter_pool_side_table_trial()


def assert_no_native_effects(pool_count: int) -> None:
    extension = requests._requests_rust
    public = extension._public_facade_pump_trial("snapshot")
    runtime = extension._runtime_submission_trial("snapshot")
    assert public["submission_ids"] == []
    assert public["adapter_submission_ids"] == []
    assert runtime["events"] == []
    assert runtime["outstanding"] == 0
    assert extension._adapter_pool_side_table_trial() == pool_count


def test_pristine_trial_uses_one_native_pump_and_never_calls_python_send(
    monkeypatch,
) -> None:
    fallback_calls = []

    def forbidden(*args, **kwargs):
        fallback_calls.append((args, kwargs))
        raise AssertionError("pristine trial reached the Python adapter send")

    monkeypatch.setattr(adapters_module, "_HTTP_ADAPTER_COMPAT_SEND", forbidden)
    extension = requests._requests_rust
    public = extension._public_facade_pump_trial
    runtime = extension._runtime_submission_trial
    with loopback((200, {}, b"native")) as (server, url):
        session = Session()
        session.trust_env = False
        public("reset")
        runtime("reset")
        try:
            with requests._rust_public_trial():
                response = session.get(url, stream=True)
            public_observation = public("snapshot")
            runtime_observation = runtime("snapshot")
            content = response.content
            response.close()
        finally:
            session.close()

    assert content == b"native"
    assert type(response.raw).__module__ == "requests._requests_rust"
    assert server.requests == 1
    assert fallback_calls == []
    assert public_observation["outer_entries"] == 1
    assert public_observation["outer_exits"] == 1
    assert public_observation["max_depth"] == 1
    assert public_observation["adapter_leaf_entries"] == 1
    assert public_observation["nested_pump_entries"] == 0
    assert len(public_observation["submission_ids"]) == 1
    assert (
        public_observation["adapter_submission_ids"]
        == public_observation["submission_ids"]
    )
    assert public_observation["submission_parent_ids"] == [None]
    assert runtime_observation["events"] == [
        (
            public_observation["submission_ids"][0],
            None,
            extension._runtime_generation_trial(),
        )
    ]
    assert runtime_observation["outstanding"] == 0


def test_unsupported_default_calls_python_send_exactly_once(monkeypatch) -> None:
    marker = object()
    calls = []

    def compatibility_send(*args, **kwargs):
        calls.append((args, kwargs))
        return marker

    monkeypatch.setattr(
        adapters_module, "_HTTP_ADAPTER_COMPAT_SEND", compatibility_send
    )
    adapter = HTTPAdapter()
    adapter.max_retries = object()
    pool_count = reset_native_telemetry()
    assert adapter.send(prepared("http://example.test/")) is marker
    assert len(calls) == 1
    assert_no_native_effects(pool_count)


def test_subclass_and_custom_adapter_dispatch_to_python_once() -> None:
    events = []

    class CustomAdapter(BaseAdapter):
        def send(self, request, **kwargs):
            events.append(("adapter", request.url))
            response = Response()
            response.status_code = 200
            response.url = request.url
            response.request = request
            response.raw = SimpleNamespace(
                _original_response=None,
                close=lambda: None,
                release_conn=lambda: None,
            )
            response._content = b"custom"
            response._content_consumed = True
            return response

        def close(self):
            pass

    class CustomSession(Session):
        def get_adapter(self, url):
            events.append(("session", url))
            return super().get_adapter(url)

    session = CustomSession()
    session.adapters.clear()
    session.mount("mock://", CustomAdapter())
    pool_count = reset_native_telemetry()
    try:
        response = session.get("mock://resource")
    finally:
        session.close()
    assert response.content == b"custom"
    assert events == [
        ("session", "mock://resource"),
        ("adapter", "mock://resource"),
    ]
    assert_no_native_effects(pool_count)


def test_unsupported_retry_and_live_mutations_each_fall_back_once(monkeypatch) -> None:
    marker = object()
    calls = []

    def compatibility_send(*args, **kwargs):
        calls.append((args, kwargs))
        return marker

    monkeypatch.setattr(
        adapters_module, "_HTTP_ADAPTER_COMPAT_SEND", compatibility_send
    )
    request = prepared("http://example.test/")

    pool_count = reset_native_telemetry()
    adapter = HTTPAdapter()
    adapter.max_retries = object()
    assert adapter.send(request) is marker
    assert_no_native_effects(pool_count)

    pool_count = reset_native_telemetry()
    with monkeypatch.context() as patched:
        patched.setattr(HTTPAdapter, "add_headers", lambda *args, **kwargs: None)
        adapter = HTTPAdapter()
        assert adapter.send(request) is marker
    assert_no_native_effects(pool_count)

    pool_count = reset_native_telemetry()
    with monkeypatch.context() as patched:
        patched.setattr(adapters_module, "select_proxy", lambda *args, **kwargs: None)
        adapter = HTTPAdapter()
        assert adapter.send(request) is marker
    assert_no_native_effects(pool_count)

    assert len(calls) == 3


def test_instance_method_monkeypatch_remains_authoritative() -> None:
    adapter = HTTPAdapter()
    calls = []
    marker = object()
    adapter.send = lambda *args, **kwargs: calls.append((args, kwargs)) or marker
    pool_count = reset_native_telemetry()
    assert adapter.send(PreparedRequest()) is marker
    assert len(calls) == 1
    assert_no_native_effects(pool_count)


def test_restored_dynamic_surfaces_readmit_the_exact_native_path(monkeypatch) -> None:
    original_add_headers = HTTPAdapter.add_headers
    original_select_proxy = adapters_module.select_proxy
    monkeypatch.setattr(HTTPAdapter, "add_headers", lambda *args, **kwargs: None)
    monkeypatch.setattr(adapters_module, "select_proxy", lambda *args, **kwargs: None)
    monkeypatch.setattr(HTTPAdapter, "add_headers", original_add_headers)
    monkeypatch.setattr(adapters_module, "select_proxy", original_select_proxy)

    with loopback((200, {}, b"restored")) as (server, url):
        session = Session()
        session.trust_env = False
        try:
            response = session.get(url)
        finally:
            session.close()
    assert response.content == b"restored"
    assert type(response.raw).__module__ == "requests._requests_rust"
    assert server.requests == 1

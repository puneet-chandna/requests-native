from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case


def test_task17_native_model_and_response_facades_execute_existing_native_paths() -> (
    None
):
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
import sys
from types import SimpleNamespace
import requests
import requests.models as models_module
from requests.models import PreparedRequest, Response

with requests._rust_public_trial():
    compatibility = models_module.to_native_string
    seen = []
    def profile(frame, event, argument):
        if event == "call" and frame.f_code is compatibility.__code__:
            seen.append(frame.f_code.co_name)
    sys.setprofile(profile)
    try:
        prepared = PreparedRequest(); prepared.prepare_method("get")
    finally:
        sys.setprofile(None)
    response = Response(); response.raw = SimpleNamespace(
        stream=lambda amount, decode_content=True: iter((b"native",))
    )
    iterator = response.iter_content(2)
    iterator_module = type(iterator).__module__
    chunks = [chunk.decode() for chunk in iterator]
result = SimpleNamespace(
    method=prepared.method,
    compatibility_frames=seen,
    iterator_module=iterator_module,
    chunks=chunks,
)
"""
            )
        }
    )
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "method": "GET",
        "compatibility_frames": [],
        "iterator_module": "requests._requests_rust",
        "chunks": ["native"],
    }


def test_task17_rebuild_auth_none_request_preserves_oracle_exception() -> None:
    source = r"""
from contextlib import nullcontext
from types import SimpleNamespace
import requests
from requests.models import PreparedRequest, Response
from requests.sessions import Session
trial = getattr(requests, "_rust_public_trial", nullcontext)
prepared = PreparedRequest(); prepared.prepare(method="GET", url="http://example.test/")
response = Response(); response.request = None
with trial():
    Session().rebuild_auth(prepared, response)
result = SimpleNamespace()
"""
    assert run_rewrite_case({"source": dedent(source)}) == run_oracle_case(
        {"source": dedent(source)}
    )


def test_task17_base_adapter_close_preserves_not_implemented_error() -> None:
    source = r"""
from contextlib import nullcontext
from types import SimpleNamespace
import requests
from requests.adapters import BaseAdapter
trial = getattr(requests, "_rust_public_trial", nullcontext)
with trial():
    BaseAdapter().close()
result = SimpleNamespace()
"""
    assert run_rewrite_case({"source": dedent(source)}) == run_oracle_case(
        {"source": dedent(source)}
    )


def test_task17_redirect_operation_dispatches_exactly_once() -> None:
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
from types import SimpleNamespace
import requests
from requests.sessions import SessionRedirectMixin
extension = requests._requests_rust; original = extension._session_facade_trial
events = []
def recorder(subject, operation, args, kwargs):
    events.append(operation); return NotImplemented
extension._session_facade_trial = recorder
try:
    with requests._rust_public_trial():
        value = SessionRedirectMixin().should_strip_auth(
            "http://example.test/", "https://example.test/"
        )
finally:
    extension._session_facade_trial = original
result = SimpleNamespace(value=value, events=events)
"""
            )
        }
    )
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "value": False,
        "events": ["should_strip_auth"],
    }


def test_task17_trial_exit_preserves_user_class_monkeypatch() -> None:
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
from types import SimpleNamespace
import requests
from requests.sessions import Session
replacement = lambda self, url: "patched:" + url
with requests._rust_public_trial():
    Session.get_adapter = replacement
inside = Session.get_adapter
outside = Session.get_adapter
try:
    value = Session().get_adapter("mock://value")
finally:
    del Session.get_adapter
result = SimpleNamespace(same=inside is replacement and outside is replacement, value=value)
"""
            )
        }
    )
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "same": True,
        "value": "patched:mock://value",
    }


def test_task17_overlapping_thread_trials_remain_independently_active() -> None:
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
import threading
from types import SimpleNamespace
import requests
from requests.sessions import Session
extension = requests._requests_rust; original = extension._session_facade_trial
entered = threading.Event(); second_done = threading.Event(); events = []; errors = []
def recorder(subject, operation, args, kwargs):
    if operation == "get_adapter": events.append(threading.current_thread().name)
    return NotImplemented
extension._session_facade_trial = recorder
def first():
    try:
        with requests._rust_public_trial():
            entered.set(); assert second_done.wait(5)
            Session().get_adapter("http://example.test/")
    except BaseException as error: errors.append(type(error).__name__)
def second():
    try:
        assert entered.wait(5)
        with requests._rust_public_trial():
            Session().get_adapter("http://example.test/")
        second_done.set()
    except BaseException as error: errors.append(type(error).__name__); second_done.set()
threads = [threading.Thread(target=first, name="first"), threading.Thread(target=second, name="second")]
try:
    for thread in threads: thread.start()
    for thread in threads: thread.join(6)
finally:
    extension._session_facade_trial = original
result = SimpleNamespace(events=sorted(events), errors=errors, alive=[thread.is_alive() for thread in threads])
"""
            )
        }
    )
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "events": ["first", "second"],
        "errors": [],
        "alive": [False, False],
    }


def test_task17_close_outside_trial_detaches_session_and_adapter_handles() -> None:
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
import weakref
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.sessions import Session
extension = requests._requests_rust; registry = extension._public_facade_registry_trial
session = Session(); adapter = HTTPAdapter()
session_user = weakref.ref(session); adapter_user = weakref.ref(adapter)
with requests._rust_public_trial():
    extension._public_facade_snapshot(session)
    extension._public_facade_snapshot(adapter)
before = [len(weakref.getweakrefs(session)), len(weakref.getweakrefs(adapter))]
session.close(); adapter.close()
after = [len(weakref.getweakrefs(session)), len(weakref.getweakrefs(adapter))]
result = SimpleNamespace(
    before=before,
    after=after,
    users=[weakref.ref(session) is session_user, weakref.ref(adapter) is adapter_user],
    admitted=[registry(session, "admitted"), registry(adapter, "admitted")],
    snapshots=[extension._public_facade_snapshot(session), extension._public_facade_snapshot(adapter)],
)
"""
            )
        }
    )
    assert run.observations["exception"] is None
    result = run.observations["result"]["public_state"]
    assert result["before"] == [2, 2]
    assert result["after"] == [1, 1]
    assert result["users"] == [True, True]
    assert result["admitted"] == [False, False]
    assert [item["live"] for item in result["snapshots"]] == [False, False]


def test_task17_fork_child_starts_with_isolated_registry_generations() -> None:
    run = run_rewrite_case(
        {
            "source": dedent(
                r"""
import json, os
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.sessions import Session
if not hasattr(os, "fork"):
    result = SimpleNamespace(supported=False)
else:
    extension = requests._requests_rust; registry = extension._public_facade_registry_trial
    telemetry = extension._runtime_submission_trial
    telemetry("reset"); extension._panic_boundary_trial(False)
    parent = Session(); parent_adapter = HTTPAdapter()
    with requests._rust_public_trial():
        parent_snapshot = extension._public_facade_snapshot(parent)
        extension._public_facade_snapshot(parent_adapter)
    read_fd, write_fd = os.pipe(); child = os.fork()
    if child == 0:
        try:
            os.close(read_fd)
            inherited = registry(parent, "admitted")
            inherited_adapter_refs = len(__import__("weakref").getweakrefs(parent_adapter))
            inherited_telemetry = telemetry("snapshot")
            telemetry("reset"); extension._panic_boundary_trial(False)
            fresh_telemetry = telemetry("snapshot")
            fresh = Session()
            with requests._rust_public_trial(): snapshot = extension._public_facade_snapshot(fresh)
            payload = {"inherited": inherited, "inherited_adapter_refs": inherited_adapter_refs, "generation": snapshot["owner_generation"], "pid": os.getpid(), "inherited_events": inherited_telemetry["events"], "inherited_outstanding": inherited_telemetry["outstanding"], "fresh_events": fresh_telemetry["events"], "fresh_outstanding": fresh_telemetry["outstanding"]}
            os.write(write_fd, json.dumps(payload).encode())
        finally:
            os.close(write_fd); os._exit(0)
    os.close(write_fd); payload = json.loads(os.read(read_fd, 4096)); os.close(read_fd)
    waited, status = os.waitpid(child, 0)
    result = SimpleNamespace(
        supported=True,
        inherited=payload["inherited"],
        inherited_adapter_refs=payload["inherited_adapter_refs"],
        generation_pid=payload["generation"] >> 32,
        child_pid=payload["pid"],
        inherited_events=payload["inherited_events"],
        inherited_outstanding=payload["inherited_outstanding"],
        fresh_events=payload["fresh_events"],
        fresh_outstanding=payload["fresh_outstanding"],
        exited=os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0 and waited == child,
        parent_live=registry(parent, "admitted"),
        parent_generation=parent_snapshot["owner_generation"],
    )
"""
            )
        }
    )
    assert run.observations["exception"] is None
    result = run.observations["result"]["public_state"]
    if result["supported"]:
        assert result["inherited"] is False
        assert result["inherited_adapter_refs"] == 0
        assert result["generation_pid"] == result["child_pid"]
        assert result["inherited_events"] == []
        assert result["inherited_outstanding"] == 0
        assert len(result["fresh_events"]) == 1
        assert result["fresh_events"][0][0] == 1
        assert result["fresh_outstanding"] == 0
        assert result["exited"] is True
        assert result["parent_live"] is True


def test_task17_adapter_registration_is_stable_until_close_and_readmission() -> None:
    import weakref

    from tests_differential.test_adapters import loopback, prepared

    from requests.adapters import HTTPAdapter, _rust_adapter_trial

    with loopback(
        (200, {}, b"first"),
        (200, {}, b"second"),
        (200, {}, b"third"),
    ) as (server, url):
        adapter = HTTPAdapter()
        initial = weakref.getweakrefs(adapter)
        assert len(initial) == 1

        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"first"
            after_first = weakref.getweakrefs(adapter)
            assert adapter.send(prepared(url)).content == b"second"
            after_second = weakref.getweakrefs(adapter)

        assert after_first == initial
        assert after_second == initial
        assert len(server.clients) == 1

        adapter.close()
        # This list deliberately keeps the old weakref alive; close must release
        # the native ownership, and readmission must therefore add a new one.
        assert weakref.getweakrefs(adapter) == initial

        with _rust_adapter_trial():
            assert adapter.send(prepared(url)).content == b"third"
        readmitted = weakref.getweakrefs(adapter)
        assert len(readmitted) == 2
        assert any(reference is not initial[0] for reference in readmitted)
        assert len(server.clients) == 2


def test_task17_public_admission_adopts_live_proxy_adapter_registration() -> None:
    import weakref

    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter, _rust_adapter_trial

    extension = requests._requests_rust
    with loopback((200, {}, b"proxied")) as (_server, proxy_url):
        proxy_url = proxy_url.rsplit("/", 1)[0]
        adapter = HTTPAdapter()
        with _rust_adapter_trial():
            response = adapter.send(
                prepared("http://origin.example/resource"),
                proxies={"http": proxy_url},
            )
        assert response.content == b"proxied"
        assert list(adapter.proxy_manager) == [proxy_url]

        native_reference = extension._adapter_reference_trial(adapter)
        references_before = weakref.getweakrefs(adapter)
        assert native_reference is not None
        assert references_before == [native_reference]

        with requests._rust_public_trial():
            snapshot = extension._public_facade_snapshot(adapter)

        assert snapshot["live"] is True
        assert extension._public_facade_registry_trial(adapter, "admitted") is True
        assert extension._adapter_reference_trial(adapter) is native_reference
        assert weakref.getweakrefs(adapter) == references_before


def test_task17_stale_adapter_callback_cannot_drop_readmitted_registration() -> None:
    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    adapter = HTTPAdapter()
    stale_reference = extension._adapter_reference_trial(adapter)
    assert stale_reference is not None
    stale_callback = stale_reference.__callback__
    assert stale_callback is not None

    adapter.close()
    with requests._rust_public_trial():
        snapshot = extension._public_facade_snapshot(adapter)
    current_reference = extension._adapter_reference_trial(adapter)
    assert snapshot["live"] is True
    assert current_reference is not None
    assert current_reference is not stale_reference

    stale_callback(stale_reference)

    assert extension._public_facade_registry_trial(adapter, "admitted") is True
    assert extension._public_facade_snapshot(adapter)["live"] is True
    assert extension._adapter_reference_trial(adapter) is current_reference
    assert current_reference() is adapter


def test_task17_rotated_public_generation_releases_native_pool_on_gc() -> None:
    import gc
    import weakref

    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial
    gc.collect()
    baseline = extension._adapter_pool_side_table_trial()

    with loopback((200, {}, b"pooled")) as (_server, url):
        with requests._rust_public_trial():
            adapter = HTTPAdapter()
            response = adapter.send(prepared(url))
        assert response.content == b"pooled"
        assert extension._adapter_pool_side_table_trial() == baseline + 1

        user_reference = weakref.ref(adapter)
        first_generation = registry(adapter, "generation")
        rotated_generation = registry(adapter, "rotate")
        assert rotated_generation != first_generation

        response = None
        del adapter
        gc.collect()

        assert user_reference() is None
        assert extension._adapter_pool_side_table_trial() == baseline


def test_task17_adapter_setstate_rotates_public_and_native_registration() -> None:
    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial
    with loopback((200, {}, b"before"), (200, {}, b"after")) as (_server, url):
        with requests._rust_public_trial():
            adapter = HTTPAdapter()
            assert adapter.send(prepared(url)).content == b"before"
            before = extension._public_facade_snapshot(adapter)

        state = adapter.__getstate__()
        old_reference = extension._adapter_reference_trial(adapter)
        assert old_reference is not None
        old_callback = old_reference.__callback__
        assert old_callback is not None

        adapter.__setstate__(state)
        with requests._rust_public_trial():
            after = extension._public_facade_snapshot(adapter)
        new_reference = extension._adapter_reference_trial(adapter)

        assert new_reference is not None
        assert new_reference is not old_reference
        assert after["owner_generation"] != before["owner_generation"]
        assert after["pool_generation"] != before["pool_generation"]

        old_callback(old_reference)
        assert registry(adapter, "admitted") is True
        assert extension._adapter_reference_trial(adapter) is new_reference
        with requests._rust_public_trial():
            assert adapter.send(prepared(url)).content == b"after"

        failing_adapter = HTTPAdapter()
        with requests._rust_public_trial():
            extension._public_facade_snapshot(failing_adapter)
        state_error = RuntimeError("setstate-items-failed")
        failure_events = []

        class RaisingState(dict):
            def items(self):
                failure_events.append(
                    (
                        extension._public_facade_snapshot(failing_adapter)["live"],
                        extension._adapter_reference_trial(failing_adapter),
                    )
                )
                raise state_error

        try:
            with requests._rust_public_trial():
                failing_adapter.__setstate__(RaisingState())
        except RuntimeError as caught:
            assert caught is state_error
        else:
            raise AssertionError("reentrant state mapping error was swallowed")
        assert failure_events == [(False, None)]
        assert registry(failing_adapter, "admitted") is False
        assert extension._adapter_reference_trial(failing_adapter) is None


def test_task17_adapter_setstate_blocks_reentrant_midtransition_admission() -> None:
    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial
    with loopback(
        (200, {}, b"before"),
        (200, {}, b"during"),
        (200, {}, b"after"),
    ) as (_server, url):
        with requests._rust_public_trial():
            adapter = HTTPAdapter()
            assert adapter.send(prepared(url)).content == b"before"
            before = extension._public_facade_snapshot(adapter)

        old_reference = extension._adapter_reference_trial(adapter)
        assert old_reference is not None
        old_callback = old_reference.__callback__
        assert old_callback is not None
        events = []

        class ReentrantState(dict):
            def items(self):
                snapshot = extension._public_facade_snapshot(adapter)
                reference_before_send = extension._adapter_reference_trial(adapter)
                response = adapter.send(prepared(url))
                reference_after_send = extension._adapter_reference_trial(adapter)
                events.append(
                    (
                        snapshot["live"],
                        reference_before_send,
                        response.content,
                        reference_after_send,
                    )
                )
                return super().items()

        state = ReentrantState(adapter.__getstate__())
        with requests._rust_public_trial():
            adapter.__setstate__(state)
            after = extension._public_facade_snapshot(adapter)

        new_reference = extension._adapter_reference_trial(adapter)
        assert events == [(False, None, b"during", None)]
        assert new_reference is not None
        assert new_reference is not old_reference
        assert after["owner_generation"] != before["owner_generation"]
        assert after["pool_generation"] != before["pool_generation"]

        old_callback(old_reference)
        assert registry(adapter, "admitted") is True
        assert extension._adapter_reference_trial(adapter) is new_reference
        with requests._rust_public_trial():
            assert adapter.send(prepared(url)).content == b"after"


def test_task17_cross_thread_setstate_transition_is_owner_scoped() -> None:
    import gc
    import threading
    import weakref

    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter, _rust_adapter_trial

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial

    def run_case(*, raises: bool, adapter_trial: bool = False) -> None:
        gc.collect()
        baseline = extension._adapter_pool_side_table_trial()
        main_responses = [(200, {}, b"before")]
        if not raises:
            main_responses.extend(
                [
                    (200, {}, b"after-wait"),
                    (200, {}, b"after"),
                ]
            )
        with (
            loopback(*main_responses) as (_main_server, main_url),
            loopback((200, {}, b"unrelated")) as (_other_server, other_url),
        ):
            with requests._rust_public_trial():
                adapter = HTTPAdapter()
                assert adapter.send(prepared(main_url)).content == b"before"
                before = extension._public_facade_snapshot(adapter)

            old_reference = extension._adapter_reference_trial(adapter)
            assert old_reference is not None
            old_callback = old_reference.__callback__
            assert old_callback is not None
            unrelated = HTTPAdapter()
            state = adapter.__getstate__()
            entered = threading.Event()
            release = threading.Event()
            state_error = RuntimeError("cross-thread-setstate-items-failed")
            outcome = []
            send_outcomes = []

            class BlockingState(dict):
                def items(self):
                    entered.set()
                    if not release.wait(5):
                        raise AssertionError("setstate transition was not released")
                    if raises:
                        raise state_error
                    return super().items()

            def restore() -> None:
                try:
                    with requests._rust_public_trial():
                        adapter.__setstate__(BlockingState(state))
                except BaseException as error:
                    outcome.append(error)
                else:
                    outcome.append(None)

            worker = threading.Thread(target=restore)
            worker.start()
            assert entered.wait(5)

            def concurrent_send(*, adapter_trial: bool) -> None:
                try:
                    trial = (
                        _rust_adapter_trial
                        if adapter_trial
                        else requests._rust_public_trial
                    )
                    target = (
                        main_url if adapter_trial else "http://origin.example/resource"
                    )
                    proxy_root = main_url.rsplit("/", 1)[0]
                    kwargs = {} if adapter_trial else {"proxies": {"http": proxy_root}}
                    with trial():
                        response = adapter.send(prepared(target), **kwargs)
                    send_outcomes.append(
                        (type(response.raw).__module__, response.content, None)
                    )
                except BaseException as error:
                    send_outcomes.append((None, None, error))

            send_workers = []
            if not raises:
                send_workers = [
                    threading.Thread(
                        target=concurrent_send,
                        kwargs={"adapter_trial": adapter_trial},
                    ),
                ]
                for send_worker in send_workers:
                    send_worker.start()
            try:
                outside_snapshot = extension._public_facade_snapshot(adapter)
                with requests._rust_public_trial():
                    inside_snapshot = extension._public_facade_snapshot(adapter)
                    admitted = registry(adapter, "admitted")
                    generation = registry(adapter, "generation")
                    rotated = registry(adapter, "rotate")
                    unrelated_snapshot = extension._public_facade_snapshot(unrelated)
                    unrelated_response = unrelated.send(prepared(other_url))
                during_reference = extension._adapter_reference_trial(adapter)
                transition_references = weakref.getweakrefs(adapter)
                for send_worker in send_workers:
                    send_worker.join(timeout=5)
                assert all(
                    send_worker.is_alive() is False for send_worker in send_workers
                )
            finally:
                release.set()
                worker.join(timeout=5)
                for send_worker in send_workers:
                    send_worker.join(timeout=5)

            assert worker.is_alive() is False
            assert outside_snapshot["live"] is False
            assert inside_snapshot["live"] is False
            assert admitted is False
            assert generation is None
            assert rotated is None
            assert during_reference is None
            assert transition_references == [old_reference]
            assert unrelated_snapshot["live"] is True
            assert type(unrelated_response.raw).__module__ == "requests._requests_rust"
            assert unrelated_response.content == b"unrelated"

            if raises:
                assert outcome == [state_error]
                assert registry(adapter, "admitted") is False
                assert extension._adapter_reference_trial(adapter) is None
            else:
                assert outcome == [None]
                assert all(
                    send_worker.is_alive() is False for send_worker in send_workers
                )
                assert len(send_outcomes) == 1
                assert all(error is None for _, _, error in send_outcomes)
                assert all(
                    raw_module != "requests._requests_rust" and content == b"after-wait"
                    for raw_module, content, _ in send_outcomes
                ), send_outcomes
                with requests._rust_public_trial():
                    after = extension._public_facade_snapshot(adapter)
                new_reference = extension._adapter_reference_trial(adapter)
                if adapter_trial:
                    assert new_reference is not None
                    assert new_reference is not old_reference
                    assert after["owner_generation"] != before["owner_generation"]
                    assert after["pool_generation"] != before["pool_generation"]
                    old_callback(old_reference)
                    assert registry(adapter, "admitted") is True
                    assert extension._adapter_reference_trial(adapter) is new_reference
                    with requests._rust_public_trial():
                        assert adapter.send(prepared(main_url)).content == b"after"
                else:
                    assert after["live"] is False
                    assert new_reference is None
                    old_callback(old_reference)
                    assert registry(adapter, "admitted") is False
                    with requests._rust_public_trial():
                        response = adapter.send(prepared(main_url))
                    assert type(response.raw).__module__ != "requests._requests_rust"
                    assert response.content == b"after"

            adapter.close()
            unrelated.close()
            gc.collect()
            assert extension._adapter_pool_side_table_trial() == baseline

    run_case(raises=False)
    run_case(raises=False, adapter_trial=True)
    run_case(raises=True)


def test_task17_setstate_items_can_join_same_adapter_fallback_send() -> None:
    import threading

    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial
    with loopback((200, {}, b"inside-items")) as (_server, proxy_url):
        with requests._rust_public_trial():
            adapter = HTTPAdapter()
            before = extension._public_facade_snapshot(adapter)
        state = adapter.__getstate__()
        old_reference = extension._adapter_reference_trial(adapter)
        assert old_reference is not None
        child_outcome = []
        joined_inside = []

        def child_send() -> None:
            try:
                with requests._rust_public_trial():
                    response = adapter.send(
                        prepared("http://origin.example/resource"),
                        proxies={"http": proxy_url.rsplit("/", 1)[0]},
                    )
                child_outcome.append(
                    (type(response.raw).__module__, response.content, None)
                )
            except BaseException as error:
                child_outcome.append((None, None, error))

        class JoinedState(dict):
            def items(self):
                child = threading.Thread(target=child_send, daemon=True)
                child.start()
                child.join(timeout=2)
                joined_inside.append(child.is_alive() is False)
                return super().items()

        adapter.__setstate__(JoinedState(state))

        assert joined_inside == [True]
        assert child_outcome == [("urllib3.response", b"inside-items", None)]
        assert adapter.proxy_manager
        assert extension._adapter_reference_trial(adapter) is None
        with requests._rust_public_trial():
            after = extension._public_facade_snapshot(adapter)
        assert before["live"] is True
        assert after["live"] is False
        assert registry(adapter, "admitted") is False


def test_task17_pending_transition_cannot_deadlock_active_owner_restore() -> None:
    import threading
    import time

    import requests
    from requests._rust_public import owner_access
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    registry = extension._public_facade_registry_trial
    adapter = HTTPAdapter()
    with requests._rust_public_trial():
        before = extension._public_facade_snapshot(adapter)
    old_reference = extension._adapter_reference_trial(adapter)
    assert old_reference is not None
    old_callback = old_reference.__callback__
    assert old_callback is not None
    state = adapter.__getstate__()
    access_entered = threading.Event()
    invoke_restore = threading.Event()
    restore_inside_access = threading.Event()
    allow_access_exit = threading.Event()
    release_pending = threading.Event()
    pending_items_entered = threading.Event()
    outcomes = []

    class PendingState(dict):
        def items(self):
            pending_items_entered.set()
            if not release_pending.wait(5):
                raise AssertionError("pending transition was not released")
            return super().items()

    def access_owner() -> None:
        try:
            with owner_access(adapter) as allowed:
                assert allowed is True
                access_entered.set()
                if not invoke_restore.wait(5):
                    raise AssertionError("active owner restore was not invoked")
                adapter.__setstate__(adapter.__getstate__())
                restore_inside_access.set()
                if not allow_access_exit.wait(5):
                    raise AssertionError("active owner access was not released")
        except BaseException as error:
            outcomes.append(("access", error))
        else:
            outcomes.append(("access", None))

    def pending_restore() -> None:
        try:
            adapter.__setstate__(PendingState(state))
        except BaseException as error:
            outcomes.append(("pending", error))
        else:
            outcomes.append(("pending", None))

    access_worker = threading.Thread(target=access_owner, daemon=True)
    access_worker.start()
    assert access_entered.wait(5)
    pending_worker = threading.Thread(target=pending_restore, daemon=True)
    pending_worker.start()

    deadline = time.monotonic() + 5
    while extension._public_facade_snapshot(adapter)["live"]:
        if time.monotonic() >= deadline:
            raise AssertionError("pending transition was not published")
        time.sleep(0.001)

    invoke_restore.set()
    assert restore_inside_access.wait(2)
    assert extension._adapter_reference_trial(adapter) is None
    assert registry(adapter, "admitted") is False

    release_pending.set()
    assert pending_items_entered.wait(2)
    pending_worker.join(timeout=2)
    assert pending_worker.is_alive() is False
    assert extension._adapter_reference_trial(adapter) is None
    assert registry(adapter, "admitted") is False

    allow_access_exit.set()
    access_worker.join(timeout=2)
    assert access_worker.is_alive() is False
    assert sorted(name for name, _error in outcomes) == ["access", "pending"]
    assert all(error is None for _name, error in outcomes)
    assert extension._adapter_reference_trial(adapter) is None

    with requests._rust_public_trial():
        after = extension._public_facade_snapshot(adapter)
    new_reference = extension._adapter_reference_trial(adapter)
    assert after["live"] is True
    assert after["owner_generation"] != before["owner_generation"]
    assert after["pool_generation"] != before["pool_generation"]
    assert new_reference is not None
    assert new_reference is not old_reference
    old_callback(old_reference)
    assert registry(adapter, "admitted") is True
    assert extension._adapter_reference_trial(adapter) is new_reference


def test_task17_registration_proof_callbacks_do_not_hold_global_lock() -> None:
    import threading
    import weakref

    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    adapter = HTTPAdapter()
    unrelated = HTTPAdapter()
    with requests._rust_public_trial():
        before = extension._public_facade_snapshot(adapter)
        assert extension._public_facade_snapshot(unrelated)["live"] is True
    state = adapter.__getstate__()
    old_reference = extension._adapter_reference_trial(adapter)
    assert old_reference is not None
    real_ref = weakref.ref
    observations = {}

    def proof_ref(target, callback=None):
        if target is not adapter:
            return real_ref(target, callback)

        def same_owner_snapshot() -> None:
            with requests._rust_public_trial():
                observations["same"] = extension._public_facade_snapshot(adapter)

        def unrelated_snapshot() -> None:
            with requests._rust_public_trial():
                observations["unrelated"] = extension._public_facade_snapshot(unrelated)

        workers = [
            threading.Thread(target=same_owner_snapshot, daemon=True),
            threading.Thread(target=unrelated_snapshot, daemon=True),
        ]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(timeout=2)
        observations["joined"] = [worker.is_alive() is False for worker in workers]
        return real_ref(target, callback)

    weakref.ref = proof_ref
    try:
        adapter.__setstate__(state)
    finally:
        weakref.ref = real_ref

    assert observations["joined"] == [True, True]
    assert observations["same"]["live"] is False
    assert observations["unrelated"]["live"] is True
    assert extension._adapter_reference_trial(adapter) is None

    with requests._rust_public_trial():
        after = extension._public_facade_snapshot(adapter)
    new_reference = extension._adapter_reference_trial(adapter)
    assert after["live"] is True
    assert after["owner_generation"] != before["owner_generation"]
    assert new_reference is not None
    assert new_reference is not old_reference


def test_task17_owner_operation_during_registration_proof_invalidates_attempt() -> None:
    import threading
    import weakref

    import requests
    from requests._rust_public import owner_access
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    adapter = HTTPAdapter()
    with requests._rust_public_trial():
        before = extension._public_facade_snapshot(adapter)
    state = adapter.__getstate__()
    old_reference = extension._adapter_reference_trial(adapter)
    assert old_reference is not None
    real_ref = weakref.ref
    operation_entered = threading.Event()
    release_operation = threading.Event()
    entered_during_proof = []
    operation_allowed = []
    workers = []

    def proof_ref(target, callback=None):
        if target is not adapter:
            return real_ref(target, callback)

        def active_operation() -> None:
            with owner_access(adapter) as allowed:
                operation_allowed.append(allowed)
                operation_entered.set()
                release_operation.wait(5)

        worker = threading.Thread(target=active_operation, daemon=True)
        workers.append(worker)
        worker.start()
        entered_during_proof.append(operation_entered.wait(2))
        return real_ref(target, callback)

    weakref.ref = proof_ref
    try:
        adapter.__setstate__(state)
    finally:
        weakref.ref = real_ref
        release_operation.set()
        for worker in workers:
            worker.join(timeout=5)

    assert entered_during_proof == [True]
    assert operation_allowed == [False]
    assert all(worker.is_alive() is False for worker in workers)
    assert extension._adapter_reference_trial(adapter) is None

    with requests._rust_public_trial():
        after = extension._public_facade_snapshot(adapter)
    new_reference = extension._adapter_reference_trial(adapter)
    assert after["live"] is True
    assert after["pool_generation"] != before["pool_generation"]
    assert new_reference is not None
    assert new_reference is not old_reference


def test_task17_nested_same_owner_setstate_registers_only_final_state() -> None:
    import requests
    from requests.adapters import HTTPAdapter

    extension = requests._requests_rust
    adapter = HTTPAdapter()
    with requests._rust_public_trial():
        before = extension._public_facade_snapshot(adapter)
    old_reference = extension._adapter_reference_trial(adapter)
    assert old_reference is not None
    state = adapter.__getstate__()

    class NestedState(dict):
        def items(self):
            adapter.__setstate__(dict(state))
            assert extension._public_facade_snapshot(adapter)["live"] is False
            assert extension._adapter_reference_trial(adapter) is None
            return super().items()

    with requests._rust_public_trial():
        adapter.__setstate__(NestedState(state))
        after = extension._public_facade_snapshot(adapter)
    new_reference = extension._adapter_reference_trial(adapter)

    assert new_reference is not None
    assert new_reference is not old_reference
    assert after["owner_generation"] != before["owner_generation"]
    assert after["pool_generation"] != before["pool_generation"]

    from requests._rust_public import owner_access

    active_reference = new_reference
    active_callback = active_reference.__callback__
    assert active_callback is not None
    with owner_access(adapter) as allowed:
        assert allowed is True
        adapter.__setstate__(adapter.__getstate__())
    with requests._rust_public_trial():
        active_after = extension._public_facade_snapshot(adapter)
    final_reference = extension._adapter_reference_trial(adapter)
    assert final_reference is not None
    assert final_reference is not active_reference
    assert active_after["owner_generation"] != after["owner_generation"]
    active_callback(active_reference)
    assert extension._adapter_reference_trial(adapter) is final_reference


def test_task17_signal_setstate_during_native_send_matches_oracle() -> None:
    source = r"""
import os, signal, threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

release = threading.Event()
events = []

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.flush()
        os.kill(os.getpid(), signal.SIGALRM)
        if not release.wait(5):
            raise AssertionError("signal handler did not release response")
        self.wfile.write(b"ok")
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
adapter = HTTPAdapter()
request = PreparedRequest()
request.prepare(method="GET", url=f"http://127.0.0.1:{server.server_port}/resource")

def handle_alarm(_signum, _frame):
    events.append("alarm-enter")
    adapter.__setstate__(adapter.__getstate__())
    events.append("setstate-ok")
    release.set()

previous = signal.signal(signal.SIGALRM, handle_alarm)
trial = getattr(requests, "_rust_public_trial", nullcontext)
try:
    with trial():
        response = adapter.send(request)
    result = SimpleNamespace(
        events=events,
        status=response.status_code,
        content=response.content.decode(),
    )
finally:
    release.set()
    signal.signal(signal.SIGALRM, previous)
    server.shutdown()
    server.server_close()
    worker.join(timeout=5)
"""
    oracle = run_oracle_case({"source": dedent(source)})
    rewrite = run_rewrite_case({"source": dedent(source)})
    assert oracle.observations["exception"] is None
    assert oracle.observations["result"]["public_state"] == {
        "events": ["alarm-enter", "setstate-ok"],
        "status": 200,
        "content": "ok",
    }
    assert rewrite == oracle


def test_task17_pre_header_signal_handler_can_release_native_send() -> None:
    source = r"""
import os, signal, threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

release = threading.Event()
events = []

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        os.kill(os.getpid(), signal.SIGALRM)
        released = release.wait(2)
        events.append("server-released" if released else "server-timeout")
        body = b"ok" if released else b"late"
        self.send_response(200 if released else 598)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
sender = __SENDER__
request = PreparedRequest()
request.prepare(method="GET", url=f"http://127.0.0.1:{server.server_port}/resource")

def handle_alarm(_signum, _frame):
    events.append("alarm-enter")
    release.set()

previous = signal.signal(signal.SIGALRM, handle_alarm)
trial = getattr(requests, "_rust_public_trial", nullcontext)
try:
    with trial():
        response = sender.send(request)
    result = SimpleNamespace(
        events=events,
        status=response.status_code,
        content=response.content.decode(),
    )
finally:
    release.set()
    signal.signal(signal.SIGALRM, previous)
    server.shutdown()
    server.server_close()
    worker.join(timeout=5)
"""
    for sender in ("HTTPAdapter()", "requests.Session()"):
        case_source = dedent(source).replace("__SENDER__", sender)
        oracle = run_oracle_case({"source": case_source})
        rewrite = run_rewrite_case({"source": case_source})
        assert oracle.observations["exception"] is None
        assert oracle.observations["result"]["public_state"] == {
            "events": ["alarm-enter", "server-released"],
            "status": 200,
            "content": "ok",
        }
        assert rewrite == oracle


def test_task17_raising_signal_cancels_header_and_body_waits_promptly() -> None:
    source = r"""
import os, signal, threading, time
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

phase = __PHASE__
sender_kind = __SENDER_KIND__
release = threading.Event()
ports = []

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        ports.append(self.client_address[1])
        if self.path == "/blocked":
            if phase == "body":
                self.send_response(200)
                self.send_header("Content-Length", "4")
                self.end_headers()
                self.wfile.flush()
                time.sleep(0.1)
            os.kill(os.getpid(), signal.SIGALRM)
            release.wait()
            try:
                if phase == "header":
                    self.send_response(200)
                    self.send_header("Content-Length", "4")
                    self.end_headers()
                self.wfile.write(b"late")
                self.wfile.flush()
            except OSError:
                pass
            return
        body = b"recovered"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
if sender_kind == "session":
    sender = requests.Session()
    sender.mount("http://", adapter)
elif sender_kind == "root":
    sender = None
else:
    sender = adapter

def prepared(path):
    request = PreparedRequest()
    request.prepare(
        method="GET",
        url=f"http://127.0.0.1:{server.server_port}{path}",
    )
    return request

def send(path, *, stream=False):
    if sender_kind == "root":
        return requests.get(
            f"http://127.0.0.1:{server.server_port}{path}",
            stream=stream,
        )
    return sender.send(prepared(path), stream=stream)

interrupt = KeyboardInterrupt("task17-cancel")
def handle_alarm(_signum, _frame):
    raise interrupt

def supervise():
    time.sleep(1.2)
    release.set()

supervisor = threading.Thread(target=supervise, daemon=True)
previous = signal.signal(signal.SIGALRM, handle_alarm)
extension = getattr(requests, "_requests_rust", None)
generation = getattr(extension, "_runtime_generation_trial", lambda: None)
telemetry = getattr(extension, "_runtime_submission_trial", None)
trial = getattr(requests, "_rust_public_trial", nullcontext)
caught = None
started = time.monotonic()
supervisor.start()
try:
    before_generation = generation()
    if telemetry is not None:
        telemetry("reset")
    try:
        with trial():
            blocked = send("/blocked", stream=phase == "body")
            if phase == "body":
                blocked.content
    except BaseException as error:
        caught = error
    elapsed = time.monotonic() - started
    cancel_telemetry = None if telemetry is None else telemetry("snapshot")
    release.set()
    if telemetry is not None:
        telemetry("reset")
    with trial():
        recovered = send("/recovery")
        recovered_content = recovered.content.decode()
    recovery_telemetry = None if telemetry is None else telemetry("snapshot")
    after_generation = generation()
    result = SimpleNamespace(
        exact=caught is interrupt,
        caught_type=None if caught is None else type(caught).__name__,
        caught_args=None if caught is None else list(caught.args),
        prompt=elapsed < 0.6,
        recovered=recovered_content,
        same_runtime=before_generation == after_generation,
        fresh_connection=len(ports) >= 2 and ports[0] != ports[-1],
        raw_module=type(recovered.raw).__module__,
        cancel_events=None if cancel_telemetry is None else cancel_telemetry["events"],
        cancel_outstanding=None if cancel_telemetry is None else cancel_telemetry["outstanding"],
        recovery_events=None if recovery_telemetry is None else recovery_telemetry["events"],
        recovery_outstanding=None if recovery_telemetry is None else recovery_telemetry["outstanding"],
    )
finally:
    release.set()
    signal.signal(signal.SIGALRM, previous)
    try:
        if sender is not None:
            sender.close()
    finally:
        if sender is not None and sender is not adapter:
            adapter.close()
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
        supervisor.join(timeout=2)
"""
    expected = {
        "exact": True,
        "caught_type": "KeyboardInterrupt",
        "caught_args": ["task17-cancel"],
        "prompt": True,
        "recovered": "recovered",
        "same_runtime": True,
        "fresh_connection": True,
    }
    for phase in ("header", "body"):
        for sender_kind in ("adapter", "session", "root"):
            case_source = (
                dedent(source)
                .replace("__PHASE__", repr(phase))
                .replace("__SENDER_KIND__", repr(sender_kind))
            )
            oracle = run_oracle_case({"source": case_source})
            rewrite = run_rewrite_case({"source": case_source})
            assert oracle.observations["exception"] is None
            oracle_state = oracle.observations["result"]["public_state"]
            assert {key: oracle_state[key] for key in expected} == expected
            assert oracle_state["raw_module"].startswith("urllib3")
            assert oracle_state["cancel_events"] is None
            assert oracle_state["cancel_outstanding"] is None
            assert rewrite.observations["exception"] is None
            rewrite_state = rewrite.observations["result"]["public_state"]
            assert {key: rewrite_state[key] for key in expected} == expected
            assert rewrite_state["raw_module"] == "requests._requests_rust"
            expected_cancel_submissions = 1 if phase == "header" else 2
            assert len(rewrite_state["cancel_events"]) == expected_cancel_submissions
            assert all(event[1] is None for event in rewrite_state["cancel_events"])
            assert len({event[2] for event in rewrite_state["cancel_events"]}) == 1
            assert rewrite_state["cancel_outstanding"] == 0
            assert len(rewrite_state["recovery_events"]) == 3
            assert all(event[1] is None for event in rewrite_state["recovery_events"])
            assert rewrite_state["recovery_outstanding"] == 0


def test_task17_raising_signal_cancels_blocking_pool_capacity_wait() -> None:
    source = r"""
import os, signal, threading, time
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

release_held = threading.Event()
paths = []
ports = []

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        paths.append(self.path)
        ports.append(self.client_address[1])
        body = b"held" if self.path == "/held" else b"recovered"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.flush()
        if self.path == "/held":
            release_held.wait()
        try:
            self.wfile.write(body)
            self.wfile.flush()
        except OSError:
            pass
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)

def prepared(path):
    request = PreparedRequest()
    request.prepare(method="GET", url=f"http://127.0.0.1:{server.server_port}{path}")
    return request

interrupt = KeyboardInterrupt("task17-capacity")
def handle_alarm(_signum, _frame):
    raise interrupt

def alarm_later():
    time.sleep(0.1)
    os.kill(os.getpid(), signal.SIGALRM)

previous = signal.signal(signal.SIGALRM, handle_alarm)
extension = getattr(requests, "_requests_rust", None)
telemetry = getattr(extension, "_runtime_submission_trial", None)
trial = getattr(requests, "_rust_public_trial", nullcontext)
caught = None
try:
    with trial():
        held = adapter.send(prepared("/held"), stream=True)
    if telemetry is not None:
        telemetry("reset")
    alarm = threading.Thread(target=alarm_later, daemon=True)
    alarm.start()
    started = time.monotonic()
    try:
        with trial():
            adapter.send(prepared("/blocked"), stream=True)
    except BaseException as error:
        caught = error
    elapsed = time.monotonic() - started
    blocked_paths = list(paths)
    cancel_telemetry = None if telemetry is None else telemetry("snapshot")
    release_held.set()
    held.close()
    with trial():
        recovered = adapter.send(prepared("/recovery"))
        content = recovered.content.decode()
    result = SimpleNamespace(
        exact=caught is interrupt,
        caught_args=None if caught is None else list(caught.args),
        prompt=elapsed < 0.5,
        blocked_never_reached=blocked_paths == ["/held"],
        recovered=content,
        native=type(recovered.raw).__module__ == "requests._requests_rust",
        fresh_connection=len(ports) >= 2 and ports[0] != ports[-1],
        cancel_events=None if cancel_telemetry is None else cancel_telemetry["events"],
        cancel_outstanding=None if cancel_telemetry is None else cancel_telemetry["outstanding"],
    )
finally:
    release_held.set()
    signal.signal(signal.SIGALRM, previous)
    adapter.close()
    server.shutdown()
    server.server_close()
    worker.join(timeout=5)
"""
    rewrite = run_rewrite_case({"source": dedent(source)})
    expected = {
        "exact": True,
        "caught_args": ["task17-capacity"],
        "prompt": True,
        "blocked_never_reached": True,
        "recovered": "recovered",
        "fresh_connection": True,
    }
    assert rewrite.observations["exception"] is None
    rewrite_state = rewrite.observations["result"]["public_state"]
    assert {key: rewrite_state[key] for key in expected} == expected
    assert rewrite_state["native"] is True
    assert len(rewrite_state["cancel_events"]) == 1
    assert rewrite_state["cancel_events"][0][1] is None
    assert rewrite_state["cancel_outstanding"] == 0


def test_task17_consumed_body_releases_blocking_pool_capacity() -> None:
    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    with loopback((200, {}, b"first"), (200, {}, b"second")) as (_server, url):
        adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
        try:
            with requests._rust_public_trial():
                first = adapter.send(prepared(url))
                assert first.content == b"first"
                second = adapter.send(prepared(url))
                assert second.content == b"second"
            assert type(first.raw).__module__ == "requests._requests_rust"
            assert type(second.raw).__module__ == "requests._requests_rust"
        finally:
            adapter.close()


def test_task17_exact_declared_body_read_releases_blocking_pool_capacity() -> None:
    source = r"""
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

clients = []
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        clients.append(self.client_address[1])
        body = b"body" if self.path == "/first" else b"second"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
trial = getattr(requests, "_rust_public_trial", nullcontext)
def prepared(path):
    request = PreparedRequest()
    request.prepare(method="GET", url=f"http://127.0.0.1:{server.server_port}{path}")
    return request

outcome = []
try:
    with trial():
        first = adapter.send(prepared("/first"), stream=True)
        first_bytes = first.raw.read(4)
    def send_second():
        with trial():
            response = adapter.send(prepared("/second"))
            outcome.append((response.content, type(response.raw).__module__))
    second = threading.Thread(target=send_second, daemon=True)
    second.start()
    second.join(0.5)
    prompt = not second.is_alive()
    first.close()
    second.join(2)
    result = SimpleNamespace(
        first=first_bytes.decode(),
        prompt=prompt,
        second=None if not outcome else outcome[0][0].decode(),
        native=None if not outcome else outcome[0][1] == "requests._requests_rust",
        reused=len(clients) == 2 and clients[0] == clients[1],
    )
finally:
    adapter.close()
    server.shutdown(); server.server_close(); worker.join(5)
"""
    expected = {
        "first": "body",
        "prompt": True,
        "second": "second",
        "reused": True,
    }
    oracle = run_oracle_case({"source": dedent(source)})
    rewrite = run_rewrite_case({"source": dedent(source)})
    assert oracle.observations["exception"] is None
    assert rewrite.observations["exception"] is None
    assert {
        key: oracle.observations["result"]["public_state"][key] for key in expected
    } == expected
    rewrite_state = rewrite.observations["result"]["public_state"]
    assert {key: rewrite_state[key] for key in expected} == expected
    assert rewrite_state["native"] is True


def test_task17_body_completion_capacity_boundaries_match_oracle() -> None:
    source = r"""
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

mode = {mode!r}
amount = {amount}
clients = []
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        clients.append(self.client_address[1])
        if self.path == "/first":
            self.send_response(200)
            if mode in ("partial", "over-read"):
                self.send_header("Content-Length", "4")
            elif mode == "chunked":
                self.send_header("Transfer-Encoding", "chunked")
            else:
                self.send_header("Connection", "close")
                self.close_connection = True
            self.end_headers()
            if mode == "chunked":
                self.wfile.write(b"4\r\nbody\r\n0\r\n\r\n")
            else:
                self.wfile.write(b"body")
            self.wfile.flush()
            return
        self.send_response(200)
        self.send_header("Content-Length", "6")
        self.end_headers()
        self.wfile.write(b"second")
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
trial = getattr(requests, "_rust_public_trial", nullcontext)
def prepared(path):
    request = PreparedRequest()
    request.prepare(method="GET", url=f"http://127.0.0.1:{{server.server_port}}{{path}}")
    return request

outcome = []
try:
    with trial():
        first = adapter.send(prepared("/first"), stream=True)
        first_bytes = first.raw.read(amount)
    def send_second():
        with trial():
            response = adapter.send(prepared("/second"))
            outcome.append((response.content, type(response.raw).__module__))
    second = threading.Thread(target=send_second, daemon=True)
    second.start(); second.join(0.4)
    prompt = not second.is_alive()
    first.close(); second.join(2)
    result = SimpleNamespace(
        first=first_bytes.decode(), prompt=prompt,
        second=None if not outcome else outcome[0][0].decode(),
        native=None if not outcome else outcome[0][1] == "requests._requests_rust",
        reused=len(clients) == 2 and clients[0] == clients[1],
    )
finally:
    adapter.close(); server.shutdown(); server.server_close(); worker.join(5)
"""
    cases = (
        ("partial", 2, {"first": "bo", "prompt": False, "reused": False}),
        ("over-read", 8, {"first": "body", "prompt": True, "reused": True}),
        ("chunked", 4, {"first": "body", "prompt": False, "reused": False}),
        ("unknown", 4, {"first": "body", "prompt": False, "reused": False}),
    )
    for mode, amount, expected in cases:
        case_source = source.format(mode=mode, amount=amount)
        oracle = run_oracle_case({"source": dedent(case_source)})
        rewrite = run_rewrite_case({"source": dedent(case_source)})
        assert oracle.observations["exception"] is None, mode
        assert rewrite.observations["exception"] is None, mode
        oracle_state = oracle.observations["result"]["public_state"]
        rewrite_state = rewrite.observations["result"]["public_state"]
        expected = {**expected, "second": "second"}
        assert {key: oracle_state[key] for key in expected} == expected, mode
        assert {key: rewrite_state[key] for key in expected} == expected, mode
        assert rewrite_state["native"] is True, mode


def test_task17_malformed_declared_length_does_not_strand_pool_capacity() -> None:
    from tests_differential.test_adapters import loopback, prepared

    import requests
    from requests.adapters import HTTPAdapter

    with loopback(
        (200, {"Content-Length": "malformed", "Connection": "close"}, b"x", None, True),
        (200, {}, b"second"),
    ) as (_server, url):
        adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
        try:
            with requests._rust_public_trial():
                try:
                    adapter.send(prepared(url), stream=True)
                except BaseException as error:
                    first_error = type(error).__name__
                second = adapter.send(prepared(url))
                assert second.content == b"second"
            assert first_error
            assert type(second.raw).__module__ == "requests._requests_rust"
        finally:
            adapter.close()


def test_task17_encoded_raw_and_decoded_completion_own_capacity_correctly() -> None:
    source = r"""
import gzip, threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

mode = {mode!r}
wire = b"not-a-gzip-stream" if mode == "malformed" else gzip.compress(b"payload")
clients = []
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        clients.append(self.client_address[1])
        body = wire if self.path == "/first" else b"second"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        if self.path == "/first":
            self.send_header("Content-Encoding", "gzip")
        self.end_headers(); self.wfile.write(body); self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True); worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
trial = getattr(requests, "_rust_public_trial", nullcontext)
def prepared(path):
    request = PreparedRequest()
    request.prepare(method="GET", url=f"http://127.0.0.1:{{server.server_port}}{{path}}")
    return request

outcome = []
try:
    with trial():
        first = adapter.send(prepared("/first"), stream=True)
        amount = {{"raw-exact": len(wire), "raw-partial": len(wire) // 2,
                   "raw-overread": len(wire) + 9,
                   "decoded-exact": len(b"payload"),
                   "decoded-overread": len(b"payload") + 1}}.get(mode)
        try:
            if mode == "malformed":
                list(first.iter_content(3))
                first_bytes = b""
            else:
                first_bytes = first.raw.read(
                    amount, decode_content=mode.startswith("decoded")
                )
            first_error = None
        except BaseException as error:
            first_bytes = b""
            first_error = type(error).__name__
    def send_second():
        with trial():
            response = adapter.send(prepared("/second"))
            outcome.append((response.content, type(response.raw).__module__))
    second = threading.Thread(target=send_second, daemon=True)
    second.start(); second.join(0.5)
    prompt = not second.is_alive()
    first.close(); second.join(2)
    result = SimpleNamespace(
        first_error=first_error,
        first_matches_wire=first_bytes == wire,
        first_decoded=first_bytes.decode(errors="replace"),
        prompt=prompt,
        requests=len(clients),
        second=None if not outcome else outcome[0][0].decode(),
        native=None if not outcome else outcome[0][1] == "requests._requests_rust",
        reused=len(clients) == 2 and clients[0] == clients[1],
    )
finally:
    adapter.close(); server.shutdown(); server.server_close(); worker.join(5)
"""
    cases = {
        "raw-exact": {
            "first_error": None,
            "first_matches_wire": True,
            "prompt": True,
            "requests": 2,
            "reused": True,
        },
        "raw-partial": {
            "first_error": None,
            "first_matches_wire": False,
            "prompt": False,
            "requests": 2,
            "reused": False,
        },
        "raw-overread": {
            "first_error": None,
            "first_matches_wire": True,
            "prompt": True,
            "requests": 2,
            "reused": True,
        },
        "decoded-exact": {
            "first_error": None,
            "first_decoded": "payload",
            "prompt": False,
            "requests": 2,
            "reused": False,
        },
        "decoded-overread": {
            "first_error": None,
            "first_decoded": "payload",
            "prompt": True,
            "requests": 2,
            "reused": True,
        },
        "malformed": {
            "first_error": "ContentDecodingError",
            "prompt": False,
            "requests": 2,
            "reused": False,
        },
    }
    for mode, expected in cases.items():
        case_source = source.format(mode=mode)
        oracle = run_oracle_case({"source": dedent(case_source)})
        rewrite = run_rewrite_case({"source": dedent(case_source)})
        assert oracle.observations["exception"] is None, mode
        assert rewrite.observations["exception"] is None, mode
        oracle_state = oracle.observations["result"]["public_state"]
        rewrite_state = rewrite.observations["result"]["public_state"]
        expected = {**expected, "second": "second"}
        assert {key: oracle_state[key] for key in expected} == expected, mode
        assert {key: rewrite_state[key] for key in expected} == expected, mode
        assert rewrite_state["native"] is True, mode


def test_task17_fragmented_declared_body_fills_requested_amount_or_errors() -> None:
    source = r"""
import threading, time
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest

mode = {mode!r}
clients = []
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        clients.append(self.client_address[1])
        if self.path == "/first":
            self.send_response(200); self.send_header("Content-Length", "4")
            if mode.startswith("truncated"):
                self.send_header("Connection", "close"); self.close_connection = True
            self.end_headers(); self.wfile.write(b"bo"); self.wfile.flush()
            if not mode.startswith("truncated"):
                time.sleep(0.15); self.wfile.write(b"dy"); self.wfile.flush()
            return
        self.send_response(200); self.send_header("Content-Length", "6")
        self.end_headers(); self.wfile.write(b"second"); self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True); worker.start()
adapter = HTTPAdapter(pool_connections=1, pool_maxsize=1, pool_block=True)
trial = getattr(requests, "_rust_public_trial", nullcontext)
def prepared(path):
    request = PreparedRequest()
    request.prepare(method="GET", url=f"http://127.0.0.1:{{server.server_port}}{{path}}")
    return request

outcome = []
try:
    with trial():
        first = adapter.send(prepared("/first"), stream=True)
        try:
            first_bytes = first.raw.read(2 if mode == "partial" else 4)
            first_error = None
        except BaseException as error:
            first_bytes = b""; first_error = type(error).__name__
        second_read_error = None
        if mode == "truncated-error":
            try:
                first.raw.read(4)
            except BaseException as error:
                second_read_error = type(error).__name__
    def send_second():
        with trial():
            response = adapter.send(prepared("/second"))
            outcome.append((response.content, type(response.raw).__module__))
    second = threading.Thread(target=send_second, daemon=True)
    second.start(); second.join(0.5); prompt = not second.is_alive()
    first.close(); second.join(2)
    result = SimpleNamespace(
        first=first_bytes.decode(), first_error=first_error,
        second_read_error=second_read_error, prompt=prompt,
        second=None if not outcome else outcome[0][0].decode(),
        native=None if not outcome else outcome[0][1] == "requests._requests_rust",
        reused=len(clients) == 2 and clients[0] == clients[1],
    )
finally:
    adapter.close(); server.shutdown(); server.server_close(); worker.join(5)
"""
    cases = {
        "exact": {"first": "body", "first_error": None, "prompt": True, "reused": True},
        "partial": {
            "first": "bo",
            "first_error": None,
            "prompt": False,
            "reused": False,
        },
        "truncated": {
            "first": "bo",
            "first_error": None,
            "second_read_error": None,
            "prompt": False,
            "reused": False,
        },
        "truncated-error": {
            "first": "bo",
            "first_error": None,
            "second_read_error": "ProtocolError",
            "prompt": True,
            "reused": False,
        },
    }
    for mode, expected in cases.items():
        case_source = source.format(mode=mode)
        oracle = run_oracle_case({"source": dedent(case_source)})
        rewrite = run_rewrite_case({"source": dedent(case_source)})
        assert oracle.observations["exception"] is None, mode
        assert rewrite.observations["exception"] is None, mode
        oracle_state = oracle.observations["result"]["public_state"]
        rewrite_state = rewrite.observations["result"]["public_state"]
        expected = {**expected, "second": "second"}
        assert {key: oracle_state[key] for key in expected} == expected, mode
        assert {key: rewrite_state[key] for key in expected} == expected, mode
        assert rewrite_state["native"] is True, mode


def test_task17_runtime_telemetry_capture_is_explicit_and_snapshot_disables_it() -> (
    None
):
    source = r"""
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
public = requests._requests_rust._public_facade_pump_trial
runtime = requests._requests_rust._runtime_submission_trial
trial = getattr(requests, "_rust_public_trial", nullcontext)
url = f"http://127.0.0.1:{server.server_port}/"
try:
    with trial():
        requests.get(url).close()
    disabled_public = public("snapshot")
    disabled_runtime = runtime("snapshot")
    public("reset"); runtime("reset")
    session = requests.Session()
    with trial():
        for _ in range(90):
            session.get(url).close()
    session.close()
    enabled_public = public("snapshot")
    enabled_runtime = runtime("snapshot")
    with trial():
        requests.get(url).close()
    after_public = public("snapshot")
    after_runtime = runtime("snapshot")
    result = SimpleNamespace(
        disabled_public=disabled_public,
        disabled_events=disabled_runtime["events"],
        enabled_public=enabled_public,
        enabled_events=enabled_runtime["events"],
        after_public=after_public,
        after_events=after_runtime["events"],
    )
finally:
    server.shutdown(); server.server_close(); worker.join(5)
"""
    run = run_rewrite_case({"source": dedent(source)})
    assert run.observations["exception"] is None
    state = run.observations["result"]["public_state"]
    assert state["disabled_public"]["outer_entries"] == 0
    assert state["disabled_events"] == []
    assert state["enabled_public"]["outer_entries"] == 90
    assert len(state["enabled_public"]["submission_ids"]) == 256
    assert len(state["enabled_events"]) == 256
    assert state["after_public"]["outer_entries"] == 0
    assert state["after_events"] == []

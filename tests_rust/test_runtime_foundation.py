from __future__ import annotations

import gc
import os
import signal
import threading
import time
from pathlib import Path

import pytest

from requests import _requests_rust


def test_worker_bridge_requires_explicit_python_free_payload_types() -> None:
    root = Path(__file__).resolve().parents[1]
    bridge = (root / "crates/requests-python/src/bridge.rs").read_text()
    runtime = (root / "crates/requests-python/src/runtime.rs").read_text()

    assert "trait WorkerPayload" in bridge
    assert "A: WorkerPayload" in bridge
    assert "R: WorkerPayload" in bridge
    assert "impl WorkerPayload for ProbeAction" in runtime
    assert "impl WorkerPayload for ProbeReply" in runtime

    body_path = root / "crates/requests-python/src/body.rs"
    if body_path.exists():
        body = body_path.read_text()
        assert "impl WorkerPayload for BodyAction" in body
        assert "impl WorkerPayload for BodyReply" in body


def test_action_runs_on_entering_thread_and_interpreter_without_holding_python() -> None:
    probe = _requests_rust._runtime_affinity_probe
    action_started = threading.Event()
    observer_ran = threading.Event()

    def observe_released_python() -> None:
        assert action_started.wait(2)
        observer_ran.set()

    observer = threading.Thread(target=observe_released_python)
    observer.start()
    marker = object()
    result = probe(marker, action_started, observer_ran)
    observer.join(2)

    assert not observer.is_alive()
    assert result["value"] is marker
    assert result["observer_ran"] is True
    assert result["entry_thread"] == result["action_thread"]
    assert result["entry_interpreter"] == result["action_interpreter"]


def test_action_propagates_the_exact_original_base_exception() -> None:
    class ProbeBaseException(BaseException):
        pass

    original = ProbeBaseException("identity must survive the pump")

    with pytest.raises(ProbeBaseException) as raised:
        _requests_rust._runtime_error_probe(original)

    assert raised.value is original


def test_injected_signal_wins_over_an_already_ready_result() -> None:
    class ReadySignal(BaseException):
        pass

    original = ReadySignal("ready output must not hide this signal")

    with pytest.raises(ReadySignal) as raised:
        _requests_rust._runtime_ready_error_probe(original)

    assert raised.value is original


def test_cancelled_worker_never_owns_the_python_destructor() -> None:
    class CancelSignal(BaseException):
        pass

    entering_thread = threading.get_ident()
    destructor_threads: list[int] = []

    class Tracked:
        def __del__(self) -> None:
            destructor_threads.append(threading.get_ident())

    holder = [Tracked()]
    original = CancelSignal("cancel while the worker awaits an action reply")

    with pytest.raises(CancelSignal) as raised:
        _requests_rust._runtime_cancel_ownership_probe(holder, original)

    gc.collect()
    assert raised.value is original
    assert holder == []
    assert destructor_threads == [entering_thread]
    assert _requests_rust._runtime_signal_was_cancelled() is True


def test_nested_action_pump_reuses_the_process_runtime() -> None:
    result = _requests_rust._runtime_nested_probe()

    assert result["value"] == "nested"
    assert result["outer_generation"] == result["nested_generation"]
    assert result["entry_thread"] == result["nested_action_thread"]
    assert result["entry_interpreter"] == result["nested_action_interpreter"]


@pytest.mark.skipif(
    os.name != "posix" or not hasattr(signal, "SIGINT"),
    reason="signal injection probe requires POSIX SIGINT",
)
def test_pending_signal_cancels_a_never_completing_future() -> None:
    probe = _requests_rust._runtime_signal_probe

    def interrupt() -> None:
        time.sleep(0.1)
        os.kill(os.getpid(), signal.SIGINT)

    interrupter = threading.Thread(target=interrupt)
    interrupter.start()
    started = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        probe()
    elapsed = time.monotonic() - started
    interrupter.join(2)

    assert not interrupter.is_alive()
    assert elapsed < 2
    assert _requests_rust._runtime_signal_was_cancelled() is True

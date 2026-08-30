from __future__ import annotations

import ast
import gc
import json
import os
import re
import select
import signal
import threading
import time
import weakref
from pathlib import Path
from types import SimpleNamespace

import pytest

from requests import _requests_rust

ROOT = Path(__file__).resolve().parents[1]
DIFFERENTIAL_RUNTIME = ROOT / "tests_differential" / "test_session_runtime.py"
RUNTIME_SOURCE = ROOT / "crates" / "requests-python" / "src" / "runtime.rs"
BRIDGE_SOURCE = ROOT / "crates" / "requests-python" / "src" / "bridge.rs"
PYTHON_LIB_SOURCE = ROOT / "crates" / "requests-python" / "src" / "lib.rs"
SESSION_RUNTIME_SOURCE = ROOT / "crates" / "requests-python" / "src" / "sessions.rs"
SESSION_PAYLOAD_CONTRACT_SOURCE = (
    ROOT / "tests_rust" / "fixtures" / "task16_session_payload_contract.rs"
)
CONNECT_SOURCE = ROOT / "crates" / "requests" / "src" / "transport" / "connect.rs"
CORE_RESPONSE_SOURCE = ROOT / "crates" / "requests" / "src" / "response.rs"
CLIENT_SOURCE = ROOT / "crates" / "requests" / "src" / "client.rs"
POOL_SOURCE = ROOT / "crates" / "requests" / "src" / "transport" / "pool.rs"
EXPECTED_PHASE_B_IDS = tuple(f"B{index:02d}" for index in range(1, 19))
SESSION_ID_CATEGORIES = (
    "RequestId",
    "ResponseId",
    "AdapterId",
    "JarId",
    "HookId",
    "AuthId",
    "CursorId",
    "OpaqueValueId",
)
SESSION_ACTION_SCHEMA = {
    "ReadGlobal": ("authority", "generation", "sequence"),
    "ReadBody": ("request_id", "generation", "sequence"),
    "SendCustomAdapter": (
        "adapter_id",
        "request_id",
        "generation",
        "correlation",
        "sequence",
    ),
    "DispatchHook": (
        "hook_id",
        "response_id",
        "generation",
        "correlation",
        "sequence",
    ),
    "RunAuth": ("auth_id", "request_id", "generation", "sequence"),
    "ExtractCookies": (
        "jar_id",
        "request_id",
        "response_id",
        "generation",
        "sequence",
    ),
    "NestedSubmit": (
        "request_id",
        "generation",
        "parent_correlation",
        "correlation",
        "sequence",
    ),
}
SESSION_REPLY_SCHEMA = {
    "Scalar": ("value", "generation", "correlation", "sequence"),
    "Response": ("response_id", "generation", "correlation", "sequence"),
    "Nested": ("request_id", "generation", "correlation", "sequence"),
    "Raised": ("error_id", "generation", "correlation", "sequence"),
}
NATIVE_TRANSFER_SCHEMA = (
    "method",
    "url",
    "headers",
    "body_id",
    "adapter_id",
    "generation",
    "correlation",
)
SESSION_PAYLOAD_NEGATIVES = (
    "reject_py_handle",
    "reject_pyerr",
    "reject_borrowed_value",
    "reject_python_destructor",
    "reject_unproven_trait_object",
    "reject_origin_owner",
    "reject_wrong_category_id",
    "reject_stale_generation",
    "reject_unchecked_allocation",
    "worker_payload_has_no_blanket_impl",
)
NATIVE_INTERRUPT_CONTRACTS = {
    "B05": (
        (CONNECT_SOURCE, "SessionInjectedConnectorGate"),
        (CONNECT_SOURCE, "ConnectDialEntered"),
        (POOL_SOURCE, "DirtyLeaseIdentity"),
        (RUNTIME_SOURCE, "signal_wins_ready_result"),
        (SESSION_RUNTIME_SOURCE, "recover_same_runtime_generation"),
    ),
    "B06": (
        (CORE_RESPONSE_SOURCE, "ResponseHeadWaitEntered"),
        (POOL_SOURCE, "DirtyLeaseIdentity"),
        (SESSION_RUNTIME_SOURCE, "no_post_head_actions"),
        (SESSION_RUNTIME_SOURCE, "recover_same_runtime_generation"),
    ),
    "B07": (
        (CORE_RESPONSE_SOURCE, "ResponseRemainderWaitEntered"),
        (POOL_SOURCE, "IncompleteBodyLeaseIdentity"),
        (SESSION_RUNTIME_SOURCE, "no_synthetic_eof_or_content"),
        (SESSION_RUNTIME_SOURCE, "recover_same_runtime_generation"),
    ),
    "B08": (
        (CLIENT_SOURCE, "OriginUploadActionEntered"),
        (CLIENT_SOURCE, "UploadQueuedExecutedReplyCounts"),
        (POOL_SOURCE, "DirtyUploadLeaseIdentity"),
        (SESSION_RUNTIME_SOURCE, "worker_drop_before_origin_owner"),
        (SESSION_RUNTIME_SOURCE, "recover_same_runtime_generation"),
    ),
    "B09": (
        (RUNTIME_SOURCE, "CancelBeforePoll"),
        (RUNTIME_SOURCE, "CancelQueuedBeforeDequeue"),
        (RUNTIME_SOURCE, "CancelReplyObserved"),
        (RUNTIME_SOURCE, "CancelTerminalAfterTimeout"),
        (RUNTIME_SOURCE, "CancelPermanentlyNonterminal"),
        (RUNTIME_SOURCE, "OriginQuarantineRetentionAudit"),
        (RUNTIME_SOURCE, "OriginQuarantineReapAudit"),
        (SESSION_RUNTIME_SOURCE, "recover_same_runtime_generation"),
    ),
}
SESSION_RUNTIME_TRIAL = "_session_runtime_trial"
NATIVE_INTERRUPT_PHASES = {
    "B05": ("connect-wait",),
    "B06": ("response-head-wait",),
    "B07": ("response-remainder-wait",),
    "B08": ("origin-upload-action-wait",),
    "B09": (
        "before-poll",
        "queued-before-dequeue",
        "reply-observed",
        "terminal-after-timeout",
        "permanently-nonterminal",
    ),
}
NATIVE_ISOLATION_CONTRACTS = {
    "B10": "fork_after_import_before_driver",
    "B11": "fork_after_live_driver",
    "B12": "fork_after_live_pool_lease",
    "B13": "multi_session_pool_isolation",
    "B14": "outstanding_stream_lease_isolation",
}
NATIVE_COMPLETION_CONTRACTS = {
    "B15": "bounded_session_finalization",
    "B16": "translate_session_worker_panic",
    "B17": "reload_live_python_authority_after_await",
    "B18": "independent_concurrent_session_channels",
}
NATIVE_SESSION_SHARED_CONTRACTS = (
    "SessionSubmission",
    "SessionExecutor",
    "SessionFinalizer",
)


def _literal_assignment(path: Path, name: str) -> object:
    tree = ast.parse(path.read_text())
    assignment = next(
        node
        for node in tree.body
        if isinstance(node, ast.Assign)
        and any(
            isinstance(target, ast.Name) and target.id == name
            for target in node.targets
        )
    )
    return ast.literal_eval(assignment.value)


def test_phase_b_runtime_inventory_is_shared_and_exact() -> None:
    scenarios = _literal_assignment(DIFFERENTIAL_RUNTIME, "PHASE_B_SCENARIOS")
    clusters = _literal_assignment(DIFFERENTIAL_RUNTIME, "PHASE_B_CLUSTERS")
    assert tuple(scenarios) == EXPECTED_PHASE_B_IDS
    assert tuple(case_id for ids in clusters.values() for case_id in ids) == (
        EXPECTED_PHASE_B_IDS
    )


def test_all_native_session_reds_use_one_exact_three_argument_seam() -> None:
    source = Path(__file__).read_text()
    tree = ast.parse(source)
    trial_calls = [
        node
        for node in ast.walk(tree)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "trial"
    ]
    assert trial_calls
    assert all(len(call.args) == 3 and not call.keywords for call in trial_calls)
    seam_assignments = [
        node
        for node in tree.body
        if isinstance(node, ast.Assign)
        and any(
            isinstance(target, ast.Name) and target.id == "SESSION_RUNTIME_TRIAL"
            for target in node.targets
        )
    ]
    assert len(seam_assignments) == 1
    assert ast.literal_eval(seam_assignments[0].value) == "_session_runtime_trial"
    assert "NATIVE_" + "INTERRUPT_TRIAL" not in source


def test_worker_payload_boundary_remains_explicit_and_non_blanket() -> None:
    bridge = BRIDGE_SOURCE.read_text()
    runtime = RUNTIME_SOURCE.read_text()
    assert "trait WorkerPayload: Send + 'static" in bridge
    assert "impl<T: WorkerPayload" not in bridge
    assert "impl WorkerPayload for ProbeAction" in runtime
    assert "impl WorkerPayload for ProbeReply" in runtime


def test_b02_eventual_payload_manifest_is_complete_and_disjoint() -> None:
    assert len(SESSION_ID_CATEGORIES) == len(set(SESSION_ID_CATEGORIES)) == 8
    assert set(SESSION_ACTION_SCHEMA) == {
        "ReadGlobal",
        "ReadBody",
        "SendCustomAdapter",
        "DispatchHook",
        "RunAuth",
        "ExtractCookies",
        "NestedSubmit",
    }
    assert set(SESSION_REPLY_SCHEMA) == {"Scalar", "Response", "Nested", "Raised"}
    assert all(
        "generation" in fields and "sequence" in fields
        for fields in SESSION_ACTION_SCHEMA.values()
    )
    assert all(
        "generation" in fields and "sequence" in fields
        for fields in SESSION_REPLY_SCHEMA.values()
    )
    assert "correlation" in NATIVE_TRANSFER_SCHEMA
    assert len(SESSION_PAYLOAD_NEGATIVES) == len(set(SESSION_PAYLOAD_NEGATIVES)) == 10


def _extract_rust_module(source: str, module_name: str) -> str:
    marker = f"mod {module_name} {{"
    start = source.index(marker)
    brace = source.index("{", start)
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[start : index + 1]
    raise AssertionError(f"unterminated Rust module {module_name}")


def _normalized_rust(source: str) -> str:
    return "".join(source.split())


def _extract_rust_function(source: str, function_name: str) -> str:
    match = re.search(rf"fn\s+{re.escape(function_name)}\s*\(", source)
    if match is None:
        raise AssertionError(f"missing Rust function {function_name}")
    brace = source.index("{", match.start())
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[match.start() : index + 1]
    raise AssertionError(f"unterminated Rust function {function_name}")


def test_b02_session_payload_schema_freezes_executable_native_unit_proofs() -> None:
    expected_source = SESSION_PAYLOAD_CONTRACT_SOURCE.read_text()
    expected_module = _extract_rust_module(
        expected_source, "task16_session_payload_contract"
    )
    required_tests = (
        "all_session_payload_variants_are_worker_payloads",
        "every_action_variant_constructs_and_exhaustively_destructures",
        "every_reply_and_transfer_constructs_and_exhaustively_destructures",
        "python_and_origin_values_are_not_worker_payloads",
        "checked_ids_reject_wrong_categories_and_stale_generations",
        "checked_ids_reject_overflow_and_unchecked_allocation",
    )
    for test_name in required_tests:
        assert expected_module.count(f"fn {test_name}()") == 1
    assert (
        "fn assert_worker_payload<T: WorkerPayload + Send + 'static>()"
        in expected_module
    )
    assert (
        "impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T"
        in expected_module
    )

    assert SESSION_RUNTIME_SOURCE.is_file(), (
        "Task 16 B02 RED: crates/requests-python/src/sessions.rs is absent"
    )
    source = SESSION_RUNTIME_SOURCE.read_text()
    actual_module = _extract_rust_module(source, "task16_session_payload_contract")
    assert _normalized_rust(actual_module) == _normalized_rust(expected_module), (
        "Task 16 B02 RED: sessions.rs must embed the exact frozen executable "
        "task16_session_payload_contract unit-test module"
    )


def test_session_runtime_has_no_row_switch_or_python_pipeline_delegation() -> None:
    base_sources = "\n".join(
        path.read_text() for path in (RUNTIME_SOURCE, BRIDGE_SOURCE, PYTHON_LIB_SOURCE)
    )
    sources = base_sources
    if SESSION_RUNTIME_SOURCE.is_file():
        sources += "\n" + SESSION_RUNTIME_SOURCE.read_text()
    assert not any(case_id in sources for case_id in EXPECTED_PHASE_B_IDS)
    assert "_session_pipeline_trial" not in base_sources
    assert "_session_runtime_interrupt_trial" not in sources
    assert "_session_runtime_trial_for_" not in sources
    assert "requests.sessions.Session.send" not in sources
    if SESSION_RUNTIME_SOURCE.is_file():
        session_source = SESSION_RUNTIME_SOURCE.read_text()
        pipeline_definition = _extract_rust_function(
            session_source, "_session_pipeline_trial"
        )
        pipeline_registration = (
            "module.add_function(wrap_pyfunction!(_session_pipeline_trial, module)?)?;"
        )
        assert (
            len(re.findall(r"fn\s+_session_pipeline_trial\s*\(", session_source)) == 1
        )
        assert session_source.count(pipeline_registration) == 1
        remaining = session_source.replace(pipeline_definition, "", 1).replace(
            pipeline_registration, "", 1
        )
        assert "_session_pipeline_trial" not in remaining
        runtime_trial = _extract_rust_function(session_source, "_session_runtime_trial")
        assert "_session_pipeline_trial" not in runtime_trial
        assert "*args" not in session_source
        assert "**kwargs" not in session_source
        session_trial_names = re.findall(
            r"fn\s+(_session_runtime_[A-Za-z0-9_]+)\s*\(", session_source
        )
        assert session_trial_names == ["_session_runtime_trial"]
        signature = re.search(
            r"fn\s+_session_runtime_trial\s*\((?P<arguments>[^)]*)\)",
            session_source,
            re.DOTALL,
        )
        assert signature is not None
        arguments = signature.group("arguments")
        assert arguments.count("subject") == 1
        assert arguments.count("scenario") == 1
        assert arguments.count("gates") == 1
        for contract in NATIVE_SESSION_SHARED_CONTRACTS:
            assert contract in session_source


def _native_contract_id(contract: tuple[str, Path, str]) -> str:
    case_id, _, symbol = contract
    return case_id + "-" + symbol


@pytest.mark.parametrize(
    "contract",
    tuple(
        (case_id, path, symbol)
        for case_id, contracts in NATIVE_INTERRUPT_CONTRACTS.items()
        for path, symbol in contracts
    ),
    ids=_native_contract_id,
)
def test_b05_b09_native_interrupt_contracts_are_explicit_structural_reds(
    contract: tuple[str, Path, str],
) -> None:
    case_id, path, symbol = contract
    assert path.is_file(), f"Task 16 {case_id} RED: missing native source {path.name}"
    assert symbol in path.read_text(), f"Task 16 {case_id} RED: missing {symbol}"


def _native_interrupt_case_id(case: tuple[str, str, bool]) -> str:
    case_id, phase, ready_first = case
    priority = "ready-first" if ready_first else "blocked"
    return f"{case_id}-{phase}-{priority}"


NATIVE_INTERRUPT_CASES = tuple(
    (case_id, phase, False)
    for case_id, phases in NATIVE_INTERRUPT_PHASES.items()
    for phase in phases
) + (("B05", NATIVE_INTERRUPT_PHASES["B05"][0], True),)


def _run_b05_b09_native_interrupt_trial(
    native_case: tuple[str, str, bool],
) -> dict[str, object]:
    case_id, phase, ready_first = native_case
    trial = getattr(_requests_rust, SESSION_RUNTIME_TRIAL)
    entry_thread = threading.get_ident()
    events: list[tuple[object, ...]] = []
    marker = KeyboardInterrupt("task16-" + case_id.lower())
    dirty = object()
    recovery = object()
    generation = object()
    entered = threading.Event()
    release = threading.Event()
    ready_observed = threading.Event()
    timeout_elapsed = threading.Event()
    ready_read, ready_write = os.pipe()
    signal_ack_read, signal_ack_write = os.pipe()
    state = {
        "connect_dials": 0,
        "dirty_lease_ids": [],
        "request_bytes": b"",
        "head_waits": 0,
        "post_head_actions": 0,
        "declared_length": None,
        "partial_body": b"",
        "remainder_waits": 0,
        "synthetic_eof": 0,
        "synthetic_content": 0,
        "upload_queued": 0,
        "upload_executed": 0,
        "upload_reply": 0,
        "upload_read": 0,
        "upload_close": 0,
        "queued": 0,
        "dequeued": 0,
        "executed": 0,
        "reply_observed": 0,
        "terminal": 0,
        "timeouts": 0,
    }

    class PhaseCollaborator:
        def _block(self) -> None:
            entered.set()

        def connect_dial(self, lease: object) -> object:
            state["connect_dials"] += 1
            state["dirty_lease_ids"].append(id(lease))
            entered.set()
            return dirty

        def request_sent(self, request: bytes) -> None:
            state["request_bytes"] += request

        def response_head_wait(self, lease: object) -> None:
            state["head_waits"] += 1
            state["dirty_lease_ids"].append(id(lease))
            self._block()

        def post_head_action(self) -> None:
            state["post_head_actions"] += 1

        def response_headers(self, declared: int, partial: bytes) -> None:
            state["declared_length"] = declared
            state["partial_body"] += partial

        def response_remainder_wait(self, lease: object) -> None:
            state["remainder_waits"] += 1
            state["dirty_lease_ids"].append(id(lease))
            self._block()

        def synthetic_eof(self) -> None:
            state["synthetic_eof"] += 1

        def synthetic_content(self) -> None:
            state["synthetic_content"] += 1

        def upload_queued(self) -> None:
            state["upload_queued"] += 1

        def upload_executed(self) -> None:
            state["upload_executed"] += 1

        def upload_reply(self) -> None:
            state["upload_reply"] += 1

        def upload_read(self, lease: object) -> None:
            state["upload_read"] += 1
            state["dirty_lease_ids"].append(id(lease))

        def upload_close(self) -> None:
            state["upload_close"] += 1

        def upload_wait(self) -> None:
            self._block()

        def before_poll(self) -> None:
            entered.set()

        def queued(self) -> None:
            state["queued"] += 1

        def dequeued(self) -> None:
            state["dequeued"] += 1

        def executed(self) -> None:
            state["executed"] += 1

        def reply_observed(self) -> None:
            state["reply_observed"] += 1

        def timeout(self) -> None:
            state["timeouts"] += 1
            timeout_elapsed.set()

        def terminal(self) -> None:
            state["terminal"] += 1

        def phase_wait(self) -> None:
            self._block()

    collaborator = PhaseCollaborator()

    class Owner:
        def __del__(self) -> None:
            events.append(("owner-del", phase, threading.get_ident()))

    owner = Owner()
    owner_id = id(owner)
    owner_ref = weakref.ref(owner)

    class AuditSubject:
        def __init__(self) -> None:
            self.marker = marker
            self.dirty = dirty
            self.recovery = recovery
            self.owner = owner

        def cancelled(self, resource: object, runtime_generation: object) -> None:
            events.append(
                (
                    "cancelled",
                    resource,
                    runtime_generation,
                    threading.get_ident(),
                )
            )

        def worker_dropped(self, resource: object) -> None:
            events.append(("worker-drop", resource, threading.get_ident()))

        def quarantined(self, retained: object, at_phase: str) -> None:
            assert retained is self.owner
            events.append(("quarantine", id(retained), at_phase))

        def origin_reaped(self, retained: object, at_phase: str) -> None:
            events.append(
                ("origin-reap", id(retained), at_phase, threading.get_ident())
            )

        def recover(self, resource: object, runtime_generation: object) -> object:
            events.append(
                (
                    "recover",
                    resource,
                    runtime_generation,
                    threading.get_ident(),
                )
            )
            return recovery

    subject = AuditSubject()
    scenario = {
        "case_id": case_id,
        "operation": {
            "connect-wait": "strict-connect-ready-race"
            if ready_first
            else "strict-connect-blocked",
            "response-head-wait": "strict-interrupt-response-head",
            "response-remainder-wait": "strict-interrupt-response-remainder",
            "origin-upload-action-wait": "strict-interrupt-origin-upload",
            "before-poll": "strict-cancel-before-poll",
            "queued-before-dequeue": "strict-cancel-queued-before-dequeue",
            "reply-observed": "strict-cancel-reply-observed",
            "terminal-after-timeout": "strict-cancel-terminal-after-timeout",
            "permanently-nonterminal": "strict-cancel-permanently-nonterminal",
        }[phase],
        "phase": phase,
        "generation": generation,
        "ready_first": ready_first,
        "collaborator": collaborator,
        # In the priority variant the native producer must write only after its
        # real result is ready. It must then wait for the helper's post-kill
        # acknowledgement before allowing the origin-side signal/task poll.
        "native_ready_write_fd": ready_write,
        "signal_ack_read_fd": signal_ack_read,
    }
    gates = SimpleNamespace(entered=entered, release=release)

    def interrupt() -> None:
        if ready_first:
            assert os.read(ready_read, 1) == b"R"
            events.append(("native-ready-observed", phase))
            ready_observed.set()
        else:
            assert entered.wait(2), f"{case_id} never reached {phase}"
        os.kill(os.getpid(), signal.SIGINT)
        if ready_first:
            os.write(signal_ack_write, b"S")

    def raise_exact_marker(_signum: int, _frame: object) -> None:
        raise marker

    previous_handler = signal.getsignal(signal.SIGINT)
    signal.signal(signal.SIGINT, raise_exact_marker)
    interrupter = threading.Thread(target=interrupt, daemon=True)
    interrupter.start()
    try:
        with pytest.raises(KeyboardInterrupt) as raised:
            trial(subject, scenario, gates)
    finally:
        release.set()
        interrupter.join(2)
        signal.signal(signal.SIGINT, previous_handler)
        os.close(ready_read)
        os.close(ready_write)
        os.close(signal_ack_read)
        os.close(signal_ack_write)

    assert not interrupter.is_alive(), f"{case_id} interrupt watchdog expired"
    assert raised.value is marker
    if ready_first:
        assert ready_observed.is_set()
        assert ("native-ready-observed", phase) in events
    else:
        assert entered.is_set()
    assert ("cancelled", dirty, generation, entry_thread) in events
    assert ("worker-drop", dirty, entry_thread) in events
    assert ("quarantine", owner_id, phase) in events
    assert owner_ref() is owner

    if case_id == "B05":
        assert state["connect_dials"] == 1
        assert state["dirty_lease_ids"] == [id(dirty)]
    elif case_id == "B06":
        assert state["request_bytes"] == b"GET /task16 HTTP/1.1\r\n\r\n"
        assert state["head_waits"] == 1
        assert state["post_head_actions"] == 0
        assert state["dirty_lease_ids"] == [id(dirty)]
    elif case_id == "B07":
        assert state["declared_length"] == 5
        assert state["partial_body"] == b"ab"
        assert state["remainder_waits"] == 1
        assert state["synthetic_eof"] == state["synthetic_content"] == 0
        assert state["dirty_lease_ids"] == [id(dirty)]
    elif case_id == "B08":
        assert state["upload_queued"] == state["upload_executed"] == 1
        assert state["upload_read"] == 1
        assert state["upload_reply"] == state["upload_close"] == 0
        assert state["dirty_lease_ids"] == [id(dirty)]
    else:
        expected = {
            "before-poll": (0, 0, 0, 0, 0, 0),
            "queued-before-dequeue": (1, 0, 0, 0, 0, 0),
            "reply-observed": (1, 1, 1, 1, 0, 0),
            "terminal-after-timeout": (1, 1, 1, 1, 1, 1),
            "permanently-nonterminal": (1, 1, 1, 0, 0, 1),
        }[phase]
        assert (
            tuple(
                state[key]
                for key in (
                    "queued",
                    "dequeued",
                    "executed",
                    "reply_observed",
                    "terminal",
                    "timeouts",
                )
            )
            == expected
        )
        expect_timeout = phase in {
            "terminal-after-timeout",
            "permanently-nonterminal",
        }
        assert timeout_elapsed.is_set() is expect_timeout

    # Only the native quarantine may retain the origin-owned object now.
    subject.owner = None
    owner = None
    gc.collect()
    assert owner_ref() is not None

    recovery_scenario = dict(
        scenario,
        operation={
            "connect-wait": "strict-recover-connect-ready-race"
            if ready_first
            else "strict-recover-connect-blocked",
            "response-head-wait": "strict-recover-response-head",
            "response-remainder-wait": "strict-recover-response-remainder",
            "origin-upload-action-wait": "strict-recover-origin-upload",
            "before-poll": "strict-recover-cancel-before-poll",
            "queued-before-dequeue": "strict-recover-cancel-queued-before-dequeue",
            "reply-observed": "strict-recover-cancel-reply-observed",
            "terminal-after-timeout": "strict-recover-cancel-terminal-after-timeout",
            "permanently-nonterminal": "strict-recover-cancel-permanently-nonterminal",
        }[phase],
        ready_first=False,
    )
    recovered = trial(subject, recovery_scenario, gates)
    assert recovered is recovery
    assert dirty is not recovery
    assert ("recover", dirty, generation, entry_thread) in events
    if case_id == "B08":
        assert state["upload_close"] == 1

    worker_drop_index = events.index(("worker-drop", dirty, entry_thread))
    if phase == "permanently-nonterminal":
        assert owner_ref() is not None
        assert not any(event[0] == "origin-reap" for event in events)
    else:
        assert subject.owner is None
        reap = ("origin-reap", owner_id, phase, entry_thread)
        assert reap in events
        assert worker_drop_index < events.index(reap)
        gc.collect()
        assert owner_ref() is None
        assert ("owner-del", phase, entry_thread) in events
    return {
        "case_id": case_id,
        "phase": phase,
        "ready_first": ready_first,
        "exact_marker": raised.value is marker,
        "event_names": [event[0] for event in events],
        "owner_alive": owner_ref() is not None,
        "dirty_recovery_distinct": dirty is not recovery,
    }


def _run_b05_b09_native_interrupt_adversarial(
    case_id: str, mutation: str
) -> dict[str, object]:
    dirty = object()
    recovery = dirty if mutation == "same-resource" else object()
    generation = object()
    subject_generation = object() if mutation == "stale-generation" else generation
    entered = threading.Event()
    release = threading.Event()
    if mutation == "pre-released":
        release.set()
    ready_read, ready_write = os.pipe()
    subject = SimpleNamespace(
        dirty=dirty,
        recovery=recovery,
        generation=subject_generation,
        owner=object(),
    )
    scenario = {
        "case_id": case_id,
        "operation": {
            "connect-wait": "strict-recover-connect-blocked",
            "response-head-wait": "strict-recover-response-head",
            "response-remainder-wait": "strict-recover-response-remainder",
            "origin-upload-action-wait": "strict-recover-origin-upload",
            "before-poll": "strict-recover-cancel-before-poll",
            "queued-before-dequeue": "strict-recover-cancel-queued-before-dequeue",
            "reply-observed": "strict-recover-cancel-reply-observed",
            "terminal-after-timeout": "strict-recover-cancel-terminal-after-timeout",
            "permanently-nonterminal": "strict-recover-cancel-permanently-nonterminal",
        }[NATIVE_INTERRUPT_PHASES[case_id][0]],
        "phase": NATIVE_INTERRUPT_PHASES[case_id][0],
        "generation": generation,
        "ready_first": False,
        "native_ready_write_fd": ready_write,
    }
    try:
        with pytest.raises(ValueError, match="resource|release|generation"):
            getattr(_requests_rust, SESSION_RUNTIME_TRIAL)(
                subject,
                scenario,
                SimpleNamespace(entered=entered, release=release),
            )
    finally:
        os.close(ready_read)
        os.close(ready_write)
    return {"case_id": case_id, "mutation": mutation, "rejected": True}


def _run_supervised_native_assertions(label: str, assertions) -> None:
    read_fd, write_fd = os.pipe()
    child_pid = os.fork()
    if child_pid == 0:
        os.close(read_fd)
        payload: dict[str, object]
        try:
            observations = assertions()
            payload = {"ok": True, "observations": observations}
        except BaseException as error:
            payload = {
                "ok": False,
                "error": f"{type(error).__name__}:{error}",
            }
        try:
            os.write(write_fd, json.dumps(payload).encode())
        finally:
            os.close(write_fd)
            os._exit(0 if payload.get("ok") else 1)

    os.close(write_fd)
    ready, _, _ = select.select([read_fd], [], [], 3.0)
    if not ready:
        os.kill(child_pid, signal.SIGKILL)
        waited_pid, _ = os.waitpid(child_pid, 0)
        os.close(read_fd)
        assert waited_pid == child_pid
        pytest.fail(f"{label} native child watchdog expired")
    payload = json.loads(os.read(read_fd, 65536))
    os.close(read_fd)
    waited_pid, status = os.waitpid(child_pid, 0)
    assert waited_pid == child_pid
    assert os.waitstatus_to_exitcode(status) == 0, payload
    assert payload["ok"] is True
    assert isinstance(payload["observations"], dict)


@pytest.mark.skipif(
    not hasattr(os, "fork") or not hasattr(signal, "SIGINT"),
    reason="native interrupt supervisor requires POSIX fork and SIGINT",
)
@pytest.mark.parametrize(
    "native_case", NATIVE_INTERRUPT_CASES, ids=_native_interrupt_case_id
)
def test_b05_b09_native_interrupt_contracts_have_executable_trial_seams(
    native_case: tuple[str, str, bool],
) -> None:
    case_id = native_case[0]
    _assert_native_session_trial(case_id)
    _run_supervised_native_assertions(
        _native_interrupt_case_id(native_case),
        lambda: _run_b05_b09_native_interrupt_trial(native_case),
    )


@pytest.mark.skipif(
    not hasattr(os, "fork"),
    reason="native adversarial supervisor requires POSIX fork",
)
@pytest.mark.parametrize("case_id", tuple(NATIVE_INTERRUPT_CONTRACTS))
@pytest.mark.parametrize(
    "mutation", ("same-resource", "pre-released", "stale-generation")
)
def test_b05_b09_native_interrupt_audit_rejects_adversarial_collaborators(
    case_id: str, mutation: str
) -> None:
    _assert_native_session_trial(case_id)
    _run_supervised_native_assertions(
        f"{case_id}-{mutation}",
        lambda: _run_b05_b09_native_interrupt_adversarial(case_id, mutation),
    )


def _assert_native_session_trial(case_id: str):
    assert hasattr(_requests_rust, SESSION_RUNTIME_TRIAL), (
        f"Task 16 {case_id} RED: missing executable native audit "
        f"{SESSION_RUNTIME_TRIAL}"
    )
    return getattr(_requests_rust, SESSION_RUNTIME_TRIAL)


@pytest.mark.parametrize("case_id", ("B10", "B11", "B12"))
@pytest.mark.skipif(not hasattr(os, "fork"), reason="fork audit requires os.fork")
def test_b10_b12_native_fork_boundaries_reset_runtime_and_resources(
    case_id: str,
) -> None:
    trial = _assert_native_session_trial(case_id)
    parent_pid = os.getpid()

    class ForkAudit:
        def __init__(self) -> None:
            self.events: list[dict[str, object]] = []

        def runtime(
            self,
            pid: int,
            generation: str,
            driver_id: str,
            driver_thread_id: int,
        ) -> None:
            assert pid == os.getpid()
            assert generation and driver_id
            self.events.append(
                {
                    "kind": "runtime",
                    "pid": pid,
                    "generation": generation,
                    "driver_id": driver_id,
                    "driver_thread_id": driver_thread_id,
                }
            )

        def pool(
            self,
            pid: int,
            generation: str,
            pool_id: str,
            lease_id: str,
            connection_id: str,
        ) -> None:
            assert pid == os.getpid()
            assert pool_id and lease_id and connection_id
            self.events.append(
                {
                    "kind": "pool",
                    "pid": pid,
                    "generation": generation,
                    "pool_id": pool_id,
                    "lease_id": lease_id,
                    "connection_id": connection_id,
                }
            )

        def action(
            self, pid: int, generation: str, correlation: str, value: str
        ) -> None:
            assert pid == os.getpid()
            assert correlation and value
            self.events.append(
                {
                    "kind": "action",
                    "pid": pid,
                    "generation": generation,
                    "correlation": correlation,
                    "value": value,
                }
            )

        def cleanup(
            self,
            pid: int,
            driver_threads: int,
            open_pools: int,
            open_leases: int,
        ) -> None:
            assert pid == os.getpid()
            self.events.append(
                {
                    "kind": "cleanup",
                    "pid": pid,
                    "driver_threads": driver_threads,
                    "open_pools": open_pools,
                    "open_leases": open_leases,
                }
            )

    subject = ForkAudit()
    setup_mode = {"B10": None, "B11": "prepare-driver", "B12": "prepare-pool"}[case_id]
    scenario: dict[str, object] = {
        "case_id": case_id,
        "operation": {
            "B10": "strict-fork-prepare-import",
            "B11": "strict-fork-prepare-driver",
            "B12": "strict-fork-prepare-pool",
        }[case_id],
        "parent_pid": parent_pid,
    }
    if setup_mode is not None:
        trial(subject, scenario, SimpleNamespace())
    parent_before = list(subject.events)

    if case_id == "B10":
        assert parent_before == []
    elif case_id == "B11":
        assert [event["kind"] for event in parent_before] == ["runtime", "action"]
    else:
        assert [event["kind"] for event in parent_before] == [
            "runtime",
            "pool",
            "action",
        ]

    inherited_runtime = next(
        (event for event in parent_before if event["kind"] == "runtime"), None
    )
    inherited_pool = next(
        (event for event in parent_before if event["kind"] == "pool"), None
    )
    read_fd, write_fd = os.pipe()
    child_pid = os.fork()
    if child_pid == 0:
        os.close(read_fd)
        child_payload: dict[str, object]
        try:
            subject.events = []
            child_scenario = {
                "case_id": case_id,
                "operation": {
                    "B10": "strict-fork-child-use-after-import",
                    "B11": "strict-fork-child-use-after-driver",
                    "B12": "strict-fork-child-use-after-pool",
                }[case_id],
                "parent_pid": parent_pid,
                "inherited_generation": None
                if inherited_runtime is None
                else inherited_runtime["generation"],
                "inherited_driver_id": None
                if inherited_runtime is None
                else inherited_runtime["driver_id"],
                "inherited_pool_id": None
                if inherited_pool is None
                else inherited_pool["pool_id"],
                "inherited_connection_id": None
                if inherited_pool is None
                else inherited_pool["connection_id"],
            }
            trial(subject, child_scenario, SimpleNamespace())
            trial(
                subject,
                dict(
                    child_scenario,
                    operation={
                        "B10": "strict-fork-child-cleanup-after-import",
                        "B11": "strict-fork-child-cleanup-after-driver",
                        "B12": "strict-fork-child-cleanup-after-pool",
                    }[case_id],
                ),
                SimpleNamespace(),
            )
            child_payload = {"ok": True, "events": subject.events}
        except BaseException as error:
            child_payload = {
                "ok": False,
                "error": f"{type(error).__name__}:{error}",
            }
        try:
            os.write(write_fd, json.dumps(child_payload).encode())
        finally:
            os.close(write_fd)
            os._exit(0 if child_payload.get("ok") else 1)

    os.close(write_fd)
    ready, _, _ = select.select([read_fd], [], [], 3)
    if not ready:
        os.kill(child_pid, signal.SIGKILL)
        os.waitpid(child_pid, 0)
        os.close(read_fd)
        pytest.fail(f"Task 16 {case_id} child watchdog expired")
    child_payload = json.loads(os.read(read_fd, 65536))
    os.close(read_fd)
    waited_pid, status = os.waitpid(child_pid, 0)
    assert waited_pid == child_pid
    assert os.waitstatus_to_exitcode(status) == 0
    assert child_payload["ok"] is True, child_payload
    child_events = child_payload["events"]
    assert isinstance(child_events, list)
    child_runtime = next(event for event in child_events if event["kind"] == "runtime")
    child_action = next(event for event in child_events if event["kind"] == "action")
    assert child_runtime["pid"] == child_pid != parent_pid
    assert child_action["pid"] == child_pid
    cleanup = [event for event in child_events if event["kind"] == "cleanup"]
    assert cleanup == [
        {
            "kind": "cleanup",
            "pid": child_pid,
            "driver_threads": 0,
            "open_pools": 0,
            "open_leases": 0,
        }
    ]
    if inherited_runtime is not None:
        assert child_runtime["generation"] != inherited_runtime["generation"]
        assert child_runtime["driver_id"] != inherited_runtime["driver_id"]
        assert (
            child_runtime["pid"],
            child_runtime["driver_thread_id"],
        ) != (
            inherited_runtime["pid"],
            inherited_runtime["driver_thread_id"],
        )
    if inherited_pool is not None:
        child_pool = next(event for event in child_events if event["kind"] == "pool")
        assert child_pool["pool_id"] != inherited_pool["pool_id"]
        assert child_pool["lease_id"] != inherited_pool["lease_id"]
        assert child_pool["connection_id"] != inherited_pool["connection_id"]

    subject.events = []
    trial(
        subject,
        {
            "case_id": case_id,
            "operation": {
                "B10": "strict-fork-parent-use-after-import",
                "B11": "strict-fork-parent-use-after-driver",
                "B12": "strict-fork-parent-use-after-pool",
            }[case_id],
            "parent_pid": parent_pid,
        },
        SimpleNamespace(),
    )
    parent_after = subject.events
    parent_runtime = next(event for event in parent_after if event["kind"] == "runtime")
    parent_action = next(event for event in parent_after if event["kind"] == "action")
    assert parent_runtime["pid"] == parent_action["pid"] == parent_pid
    assert parent_action["correlation"] != child_action["correlation"]
    assert parent_runtime["generation"] != child_runtime["generation"]
    assert parent_runtime["driver_id"] != child_runtime["driver_id"]
    if inherited_runtime is not None:
        assert parent_runtime["generation"] == inherited_runtime["generation"]
        assert parent_runtime["driver_id"] == inherited_runtime["driver_id"]
    if inherited_pool is not None:
        parent_pool = next(event for event in parent_after if event["kind"] == "pool")
        assert parent_pool["pool_id"] == inherited_pool["pool_id"]
        assert parent_pool["connection_id"] == inherited_pool["connection_id"]


@pytest.mark.parametrize("adapter_layout", ("shared", "separate"))
def test_b13_native_multi_session_pool_and_action_isolation(
    adapter_layout: str,
) -> None:
    trial = _assert_native_session_trial("B13")
    events: list[tuple[object, ...]] = []

    class SessionOwner:
        pass

    first_owner = SessionOwner()
    second_owner = SessionOwner()
    first_owner_ref = weakref.ref(first_owner)
    second_owner_ref = weakref.ref(second_owner)

    class SessionAudit:
        def resource(
            self,
            session_id: str,
            adapter_id: str,
            pool_id: str,
            lease_id: str,
            correlation: str,
            generation: str,
        ) -> None:
            events.append(
                (
                    "resource",
                    session_id,
                    adapter_id,
                    pool_id,
                    lease_id,
                    correlation,
                    generation,
                )
            )

        def close(self, session_id: str, released_lease_ids: tuple[str, ...]) -> None:
            events.append(("close", session_id, tuple(released_lease_ids)))

        def action(self, session_id: str, correlation: str, value: str) -> None:
            events.append(("action", session_id, correlation, value))

    subject = SimpleNamespace(
        audit=SessionAudit(), first_owner=first_owner, second_owner=second_owner
    )
    trial(
        subject,
        {
            "case_id": "B13",
            "operation": "strict-multi-session-isolation",
            "adapter_layout": adapter_layout,
        },
        SimpleNamespace(),
    )
    resources = [event for event in events if event[0] == "resource"]
    first = next(event for event in resources if event[1] == "first")
    second = next(event for event in resources if event[1] == "second")
    assert first[4] != second[4]
    assert first[5] != second[5]
    assert first[6] == second[6]
    if adapter_layout == "shared":
        assert first[2:4] == second[2:4]
    else:
        assert first[2] != second[2]
        assert first[3] != second[3]
    first_close = next(event for event in events if event[:2] == ("close", "first"))
    assert first_close[2] == (first[4],)
    assert second[4] not in first_close[2]
    actions = [event for event in events if event[0] == "action"]
    assert [event[1] for event in actions] == ["first", "second", "second"]
    assert actions[0][2] == first[5]
    assert actions[1][2] == second[5]
    assert len({event[2] for event in actions}) == 3
    subject.first_owner = None
    first_owner = None
    gc.collect()
    assert first_owner_ref() is None
    assert second_owner_ref() is second_owner


def test_b14_native_outstanding_stream_lease_survives_peer_clear() -> None:
    trial = _assert_native_session_trial("B14")
    events: list[tuple[object, ...]] = []
    stream_open = threading.Event()
    peer_cleared = threading.Event()

    class StreamAudit:
        def opened(self, lease_id: str, connection_id: str, generation: str) -> None:
            assert not stream_open.is_set()
            events.append(("opened", lease_id, connection_id, generation))
            stream_open.set()

        def peer_closed(
            self,
            released_lease_ids: tuple[str, ...],
            peer_connection_ids: tuple[str, ...],
        ) -> None:
            assert stream_open.is_set()
            assert not any(event[0] == "lease-released" for event in events)
            events.append(
                (
                    "peer-closed",
                    tuple(released_lease_ids),
                    tuple(peer_connection_ids),
                )
            )
            peer_cleared.set()

        def chunk(self, lease_id: str, connection_id: str, payload: bytes) -> None:
            assert peer_cleared.is_set()
            events.append(("chunk", lease_id, connection_id, payload))

        def stream_closed(self, lease_id: str, connection_id: str) -> None:
            events.append(("stream-closed", lease_id, connection_id))

        def lease_released(self, lease_id: str, connection_id: str) -> None:
            events.append(("lease-released", lease_id, connection_id))

    trial(
        SimpleNamespace(audit=StreamAudit()),
        {"case_id": "B14", "operation": "strict-outstanding-stream-isolation"},
        SimpleNamespace(stream_open=stream_open, peer_cleared=peer_cleared),
    )
    opened = next(event for event in events if event[0] == "opened")
    peer_close = next(event for event in events if event[0] == "peer-closed")
    chunk = next(event for event in events if event[0] == "chunk")
    stream_close = next(event for event in events if event[0] == "stream-closed")
    released = next(event for event in events if event[0] == "lease-released")
    assert opened[1:3] == chunk[1:3] == stream_close[1:3] == released[1:3]
    assert opened[1] not in peer_close[1]
    assert opened[2] not in peer_close[2]
    assert chunk[3] == b"retained"
    assert events.index(opened) < events.index(peer_close) < events.index(chunk)
    assert events.index(chunk) < events.index(stream_close) < events.index(released)


@pytest.mark.parametrize(
    ("case_id", "mutation"),
    (
        ("B10", "stale-pid"),
        ("B11", "stale-generation"),
        ("B12", "inherited-pool-lease"),
        ("B13", "cross-session-correlation"),
        ("B14", "premature-lease-release"),
    ),
)
def test_b10_b14_native_isolation_audit_rejects_adversarial_state(
    case_id: str, mutation: str
) -> None:
    trial = _assert_native_session_trial(case_id)
    with pytest.raises(ValueError, match="pid|generation|pool|lease|correlation"):
        trial(
            SimpleNamespace(),
            {
                "case_id": case_id,
                "operation": {
                    "stale-pid": "strict-validate-stale-pid",
                    "stale-generation": "strict-validate-stale-generation",
                    "inherited-pool-lease": "strict-validate-inherited-pool",
                    "cross-session-correlation": "strict-validate-duplicate-correlation",
                    "premature-lease-release": "strict-validate-released-lease",
                }[mutation],
                "mutation": mutation,
                "pid": os.getpid() + (1 if mutation == "stale-pid" else 0),
                "generation": "stale" if mutation == "stale-generation" else "g",
                "pool_id": "inherited" if mutation == "inherited-pool-lease" else "p",
                "lease_id": "released"
                if mutation == "premature-lease-release"
                else "l",
                "correlations": ("same", "same")
                if mutation == "cross-session-correlation"
                else ("first", "second"),
            },
            SimpleNamespace(),
        )


def test_b10_b14_native_isolation_contracts_are_structurally_explicit_reds() -> None:
    assert SESSION_RUNTIME_SOURCE.is_file(), (
        "Task 16 B10-B14 RED: crates/requests-python/src/sessions.rs is absent"
    )
    source = SESSION_RUNTIME_SOURCE.read_text()
    for case_id, symbol in NATIVE_ISOLATION_CONTRACTS.items():
        assert symbol in source, f"Task 16 {case_id} RED: missing {symbol}"


@pytest.mark.parametrize("worker_state", ("terminal", "permanently-nonterminal"))
@pytest.mark.skipif(
    not hasattr(os, "fork"),
    reason="B15 independent finalization supervisor requires os.fork",
)
def test_b15_native_finalization_is_bounded_and_origin_owned(
    worker_state: str,
) -> None:
    trial = _assert_native_session_trial("B15")
    shutdown_bound_ms = 500
    read_fd, write_fd = os.pipe()
    child_pid = os.fork()
    if child_pid == 0:
        os.close(read_fd)
        payload: dict[str, object]
        try:
            entry_thread = threading.get_ident()
            events: list[tuple[object, ...]] = []
            worker_started = threading.Event()
            release = threading.Event()

            class Owner:
                def __del__(self) -> None:
                    events.append(("owner-del", threading.get_ident() == entry_thread))

            owner = Owner()
            owner_id = id(owner)
            owner_ref = weakref.ref(owner)

            class FinalizationAudit:
                def __init__(self, retained: object) -> None:
                    self.owner = retained

                def worker_entered(
                    self, runtime_generation: str, owner_token: object
                ) -> None:
                    assert owner_token is self.owner
                    events.append(
                        ("worker-entered", runtime_generation, id(owner_token))
                    )
                    worker_started.set()

                def worker_dropped(self, runtime_generation: str) -> None:
                    events.append(("worker-dropped", runtime_generation))

                def close(self, physical_close_count: int) -> None:
                    events.append(
                        (
                            "close",
                            physical_close_count,
                            threading.get_ident() == entry_thread,
                        )
                    )

                def quarantined(self, owner_token: object, terminal: bool) -> None:
                    assert owner_token is self.owner
                    events.append(("quarantined", id(owner_token), terminal))

                def origin_reaped(self, owner_token: object) -> None:
                    events.append(
                        (
                            "origin-reaped",
                            id(owner_token),
                            threading.get_ident() == entry_thread,
                        )
                    )

            subject = FinalizationAudit(owner)
            releaser = None
            if worker_state == "terminal":

                def release_terminal() -> None:
                    assert worker_started.wait(2)
                    release.set()

                releaser = threading.Thread(target=release_terminal, daemon=True)
                releaser.start()

            started = time.monotonic()
            trial(
                subject,
                {
                    "case_id": "B15",
                    "operation": "strict-finalize-terminal"
                    if worker_state == "terminal"
                    else "strict-finalize-permanently-nonterminal",
                    "worker_state": worker_state,
                    "duplicate_close": True,
                    "shutdown_bound_ms": shutdown_bound_ms,
                },
                SimpleNamespace(worker_started=worker_started, release=release),
            )
            elapsed_ms = (time.monotonic() - started) * 1000
            if releaser is not None:
                releaser.join(2)
                assert not releaser.is_alive()
            assert worker_started.is_set()
            subject.owner = None
            owner = None
            gc.collect()

            names = [str(event[0]) for event in events]
            close_events = [event for event in events if event[0] == "close"]
            if worker_state == "terminal":
                worker_drop_index = names.index("worker-dropped")
                origin_reap_index = names.index("origin-reaped")
            else:
                worker_drop_index = origin_reap_index = -1
            payload = {
                "ok": True,
                "elapsed_ms": elapsed_ms,
                "worker_started": worker_started.is_set(),
                "close_events": close_events,
                "quarantined": (
                    "quarantined",
                    owner_id,
                    worker_state == "terminal",
                )
                in events,
                "owner_alive": owner_ref() is not None,
                "worker_drop_index": worker_drop_index,
                "origin_reap_index": origin_reap_index,
                "origin_reap_on_entry": any(
                    event[0] == "origin-reaped" and event[2] is True for event in events
                ),
                "owner_del_on_entry": any(
                    event == ("owner-del", True) for event in events
                ),
            }
        except BaseException as error:
            payload = {
                "ok": False,
                "error": f"{type(error).__name__}:{error}",
            }
        try:
            os.write(write_fd, json.dumps(payload).encode())
        finally:
            os.close(write_fd)
            os._exit(0 if payload.get("ok") else 1)

    os.close(write_fd)
    # Independent parent deadline: 500 ms native bound + 500 ms process overhead.
    ready, _, _ = select.select([read_fd], [], [], 1.0)
    if not ready:
        os.kill(child_pid, signal.SIGKILL)
        waited_pid, _ = os.waitpid(child_pid, 0)
        os.close(read_fd)
        assert waited_pid == child_pid
        pytest.fail(f"B15 {worker_state} child finalization watchdog expired")
    payload = json.loads(os.read(read_fd, 65536))
    os.close(read_fd)
    waited_pid, status = os.waitpid(child_pid, 0)
    assert waited_pid == child_pid
    assert os.waitstatus_to_exitcode(status) == 0
    assert payload["ok"] is True, payload
    assert payload["elapsed_ms"] <= shutdown_bound_ms + 250
    assert payload["worker_started"] is True
    assert payload["close_events"] == [["close", 1, True]]
    assert payload["quarantined"] is True
    if worker_state == "terminal":
        assert payload["worker_drop_index"] < payload["origin_reap_index"]
        assert payload["owner_alive"] is False
        assert payload["origin_reap_on_entry"] is True
        assert payload["owner_del_on_entry"] is True
    else:
        assert payload["worker_drop_index"] == payload["origin_reap_index"] == -1
        assert payload["owner_alive"] is True
        assert payload["origin_reap_on_entry"] is False
        assert payload["owner_del_on_entry"] is False


def test_b16_real_native_worker_panic_is_translated_cleaned_and_recoverable() -> None:
    trial = _assert_native_session_trial("B16")
    entry_thread = threading.get_ident()
    events: list[tuple[object, ...]] = []
    generation = object()
    driver_id = object()
    panic_id = "task16-rust-session-worker-panic"

    class Owner:
        def __del__(self) -> None:
            events.append(("owner-del", threading.get_ident()))

    owner = Owner()
    owner_ref = weakref.ref(owner)

    class PanicAudit:
        def __init__(self, retained: object) -> None:
            self.owner = retained

        def runtime(self, current_generation: object, current_driver: object) -> None:
            events.append(("runtime", current_generation, current_driver))

        def panic_entered(self, native_panic_id: str) -> None:
            events.append(("panic-entered", native_panic_id))

        def action_state(self, queued: int, outstanding: int) -> None:
            events.append(("action-state", queued, outstanding))

        def worker_dropped(self) -> None:
            events.append(("worker-dropped", threading.get_ident()))

        def owner_reaped(self, retained: object) -> None:
            events.append(("owner-reaped", id(retained), threading.get_ident()))

        def recovered(self, value: object) -> None:
            events.append(("recovered", value, threading.get_ident()))

    subject = PanicAudit(owner)
    scenario = {
        "case_id": "B16",
        "operation": "strict-inject-native-worker-panic",
        "panic_id": panic_id,
        "generation": generation,
        "driver_id": driver_id,
    }
    with pytest.raises(RuntimeError) as raised:
        trial(subject, scenario, SimpleNamespace())
    assert type(raised.value) is RuntimeError
    assert raised.value.args == (f"native session worker panicked: {panic_id}",)
    assert ("panic-entered", panic_id) in events
    assert ("action-state", 0, 0) in events
    assert ("worker-dropped", entry_thread) in events
    owner_reaped = ("owner-reaped", id(owner), entry_thread)
    assert owner_reaped in events
    assert events.index(("worker-dropped", entry_thread)) < events.index(owner_reaped)

    subject.owner = None
    owner = None
    gc.collect()
    assert owner_ref() is None
    assert ("owner-del", entry_thread) in events

    recovery = object()
    returned = trial(
        subject,
        dict(scenario, operation="strict-recover-native-worker-panic", value=recovery),
        SimpleNamespace(),
    )
    assert returned is recovery
    assert ("recovered", recovery, entry_thread) in events
    runtime_events = [event for event in events if event[0] == "runtime"]
    assert runtime_events == [
        ("runtime", generation, driver_id),
        ("runtime", generation, driver_id),
    ]


def test_b17_native_await_reloads_live_python_authority_in_exact_order() -> None:
    trial = _assert_native_session_trial("B17")
    entry_thread = threading.get_ident()
    await_entered = threading.Event()
    release = threading.Event()
    events: list[tuple[object, ...]] = []
    stale_value = object()
    live_value = object()

    def stale_authority() -> object:
        events.append(("stale-call", threading.get_ident()))
        return stale_value

    def live_authority() -> object:
        events.append(("live-call", threading.get_ident()))
        return live_value

    class AuthorityAudit:
        def __init__(self) -> None:
            self.authority = stale_authority

        def native_await_entered(self) -> None:
            events.append(("await-entered", threading.get_ident()))
            await_entered.set()

        def native_await_resumed(self) -> None:
            events.append(("await-resumed", threading.get_ident()))

        def completed(self, value: object) -> None:
            events.append(("completed", value, threading.get_ident()))

    subject = AuthorityAudit()

    def mutate() -> None:
        assert await_entered.wait(2)
        subject.authority = live_authority
        events.append(("mutation", threading.get_ident()))
        release.set()

    mutator = threading.Thread(target=mutate, daemon=True)
    mutator.start()
    returned = trial(
        subject,
        {"case_id": "B17", "operation": "strict-await-live-authority"},
        SimpleNamespace(await_entered=await_entered, release=release),
    )
    mutator.join(2)
    assert not mutator.is_alive()
    assert returned is live_value
    assert [event[0] for event in events] == [
        "await-entered",
        "mutation",
        "await-resumed",
        "live-call",
        "completed",
    ]
    assert not any(event[0] == "stale-call" for event in events)
    for event in events:
        if event[0] in {"await-entered", "await-resumed", "live-call"}:
            assert event[-1] == entry_thread
    assert events[-1] == ("completed", live_value, entry_thread)


def test_b18_concurrent_native_trials_keep_channels_and_affinity_isolated() -> None:
    trial = _assert_native_session_trial("B18")
    entered = {name: threading.Event() for name in ("first", "second")}
    release = {name: threading.Event() for name in ("first", "second")}
    correlations = {"first": object(), "second": object()}
    requests = {"first": object(), "second": object()}
    responses = {"first": object(), "second": object()}
    second_error = RuntimeError("task16-second-channel-error")
    results: dict[str, object] = {}
    errors: dict[str, BaseException] = {}
    entry_threads: dict[str, int] = {}
    entry_interpreters: dict[str, int] = {}
    events: list[tuple[object, ...]] = []

    class ChannelAudit:
        def __init__(self, name: str) -> None:
            self.name = name

        def entered(
            self,
            request: object,
            correlation: object,
            channel_id: str,
            interpreter_id: int,
        ) -> None:
            assert request is requests[self.name]
            assert correlation is correlations[self.name]
            assert channel_id == self.name
            assert interpreter_id == entry_interpreters[self.name]
            events.append(("entered", self.name, correlation, threading.get_ident()))
            entered[self.name].set()

        def replied(
            self,
            response: object,
            correlation: object,
            channel_id: str,
            interpreter_id: int,
        ) -> None:
            assert response is responses[self.name]
            assert correlation is correlations[self.name]
            assert channel_id == self.name
            assert interpreter_id == entry_interpreters[self.name]
            events.append(("replied", self.name, correlation, threading.get_ident()))

        def failed(
            self,
            error: BaseException,
            correlation: object,
            channel_id: str,
            interpreter_id: int,
        ) -> None:
            assert error is second_error
            assert correlation is correlations[self.name]
            assert channel_id == self.name
            assert interpreter_id == entry_interpreters[self.name]
            events.append(("failed", self.name, correlation, threading.get_ident()))

    def run(name: str) -> None:
        entry_threads[name] = threading.get_ident()
        entry_interpreters[name] = id(__import__("builtins"))
        scenario = {
            "case_id": "B18",
            "operation": "strict-concurrent-channel-fail"
            if name == "second"
            else "strict-concurrent-channel-reply",
            "channel_id": name,
            "generation": 18,
            "correlation_id": 1801 if name == "first" else 1802,
            "sequence": 1,
            "request_id": 181 if name == "first" else 182,
            "response_id": 183 if name == "first" else 184,
            "error_id": None if name == "first" else 185,
            "reply_generation": 18,
            "reply_correlation_id": 1801 if name == "first" else 1802,
            "reply_sequence": 1,
            "reply_request_id": 181 if name == "first" else 182,
            "reply_response_id": 183 if name == "first" else 184,
            "reply_error_id": None if name == "first" else 185,
            "peer_correlation_id": 1802 if name == "first" else 1801,
            "request": requests[name],
            "response": responses[name],
            "correlation": correlations[name],
            "error": second_error if name == "second" else None,
            "entry_interpreter": entry_interpreters[name],
        }
        try:
            results[name] = trial(
                ChannelAudit(name),
                scenario,
                SimpleNamespace(entered=entered[name], release=release[name]),
            )
        except BaseException as error:
            errors[name] = error

    threads = {
        name: threading.Thread(target=run, args=(name,), daemon=True)
        for name in ("first", "second")
    }
    threads["first"].start()
    threads["second"].start()
    assert entered["first"].wait(2)
    assert entered["second"].wait(2)
    release["second"].set()
    threads["second"].join(2)
    assert not threads["second"].is_alive()
    release["first"].set()
    threads["first"].join(2)
    assert not threads["first"].is_alive()

    assert results == {"first": responses["first"]}
    assert errors == {"second": second_error}
    assert correlations["first"] is not correlations["second"]
    assert [event[1] for event in events if event[0] == "replied"] == ["first"]
    assert [event[1] for event in events if event[0] == "failed"] == ["second"]
    failed = next(event for event in events if event[0] == "failed")
    replied = next(event for event in events if event[0] == "replied")
    assert events.index(failed) < events.index(replied)
    for event in events:
        assert event[-1] == entry_threads[event[1]]


@pytest.mark.parametrize(
    ("case_id", "mutation"),
    (
        ("B15", "terminal-nonterminal-conflict"),
        ("B16", "python-panic-payload"),
        ("B17", "stale-callable"),
    ),
)
def test_b15_b18_native_completion_audit_rejects_adversarial_state(
    case_id: str, mutation: str
) -> None:
    trial = _assert_native_session_trial(case_id)
    with pytest.raises(ValueError, match="terminal|panic|callable|correlation|reply"):
        trial(
            SimpleNamespace(),
            {
                "case_id": case_id,
                "operation": {
                    "terminal-nonterminal-conflict": "strict-validate-terminal-nonterminal-conflict",
                    "python-panic-payload": "strict-validate-python-panic-payload",
                    "stale-callable": "strict-validate-stale-callable",
                }[mutation],
                "mutation": mutation,
            },
            SimpleNamespace(),
        )


@pytest.mark.parametrize("mismatch", ("duplicate-correlation", "swapped-reply"))
def test_b18_native_channel_rejects_mutated_typed_envelopes(mismatch: str) -> None:
    trial = _assert_native_session_trial("B18")
    correlation_id = 1801
    scenario = {
        "operation": "strict-concurrent-channel-reply",
        "generation": 18,
        "correlation_id": correlation_id,
        "sequence": 1,
        "request_id": 181,
        "response_id": 182,
        "error_id": None,
        "reply_generation": 18,
        "reply_correlation_id": 1802 if mismatch == "swapped-reply" else correlation_id,
        "reply_sequence": 1,
        "reply_request_id": 181,
        "reply_response_id": 182,
        "reply_error_id": None,
        "peer_correlation_id": correlation_id
        if mismatch == "duplicate-correlation"
        else 1802,
    }
    with pytest.raises(ValueError, match="correlation|reply"):
        trial(SimpleNamespace(), scenario, SimpleNamespace())


def test_b15_b18_native_completion_contracts_are_structurally_explicit_reds() -> None:
    assert SESSION_RUNTIME_SOURCE.is_file(), (
        "Task 16 B15-B18 RED: crates/requests-python/src/sessions.rs is absent"
    )
    source = SESSION_RUNTIME_SOURCE.read_text()
    for case_id, symbol in NATIVE_COMPLETION_CONTRACTS.items():
        assert symbol in source, f"Task 16 {case_id} RED: missing {symbol}"


def test_b01_existing_action_pump_preserves_affinity_and_releases_python() -> None:
    started = threading.Event()
    progressed = threading.Event()

    def observer() -> None:
        assert started.wait(2)
        progressed.set()

    thread = threading.Thread(target=observer)
    thread.start()
    result = _requests_rust._runtime_affinity_probe(object(), started, progressed)
    thread.join(2)
    assert not thread.is_alive()
    assert result["observer_ran"] is True
    assert result["entry_thread"] == result["action_thread"]
    assert result["entry_interpreter"] == result["action_interpreter"]


def test_b03_existing_nested_pump_reuses_generation_and_affinity() -> None:
    result = _requests_rust._runtime_nested_probe()
    assert result["outer_generation"] == result["nested_generation"]
    assert result["entry_thread"] == result["nested_action_thread"]
    assert result["entry_interpreter"] == result["nested_action_interpreter"]


def test_b04_existing_pump_preserves_error_identity_then_recovers() -> None:
    marker = BaseException("b04-runtime-control")
    with pytest.raises(BaseException) as raised:
        _requests_rust._runtime_error_probe(marker)
    assert raised.value is marker
    assert _requests_rust._runtime_nested_probe()["value"] == "nested"


def test_private_session_runtime_trial_is_exported() -> None:
    assert hasattr(_requests_rust, "_session_runtime_trial"), (
        "Task 16 Phase B RED: missing private _session_runtime_trial"
    )

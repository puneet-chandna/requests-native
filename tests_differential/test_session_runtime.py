from __future__ import annotations

import ast
from dataclasses import dataclass
from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case

PHASE_B_CASE_IDS = tuple(f"B{index:02d}" for index in range(1, 19))
PHASE_B_CLUSTERS = {
    "affinity-payload": ("B01", "B02", "B03", "B04"),
    "interruption-cancellation": ("B05", "B06", "B07", "B08", "B09"),
    "fork-isolation": ("B10", "B11", "B12", "B13", "B14"),
    "recovery-correlation": ("B15", "B16", "B17", "B18"),
}
PHASE_B_SCENARIOS = {
    "B01": "caller-thread-interpreter-affinity",
    "B02": "sealed-python-free-payload-inventory",
    "B03": "nested-request-reentrancy",
    "B04": "exact-baseexception-stop-no-replay",
    "B05": "connect-interruption",
    "B06": "response-head-interruption",
    "B07": "response-read-interruption",
    "B08": "upload-interruption",
    "B09": "cancellation-owner-quarantine",
    "B10": "fork-after-import",
    "B11": "fork-after-driver",
    "B12": "fork-after-pool-state",
    "B13": "multi-session-isolation",
    "B14": "outstanding-stream-isolation",
    "B15": "bounded-finalization",
    "B16": "panic-runtime-recovery",
    "B17": "live-mutation-after-await",
    "B18": "correlated-replies-no-theft",
}


@dataclass(frozen=True)
class RuntimeCase:
    case_id: str
    scenario: str


PHASE_B_CASES = tuple(
    RuntimeCase(case_id, PHASE_B_SCENARIOS[case_id]) for case_id in PHASE_B_CASE_IDS
)


_COMMON_SOURCE = r"""
import gc
import os
import select
import signal
import socket
import sys
import threading
import weakref
from types import SimpleNamespace

import requests
import requests.sessions as sessions_module
from requests.adapters import BaseAdapter
from requests.models import PreparedRequest, Response

try:
    from requests import _requests_rust
except ImportError as extension_error:
    if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
        raise RuntimeError("rewrite runtime extension is unavailable") from extension_error
    _requests_rust = None

target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
entry_thread = threading.get_ident()
entry_interpreter = id(sys.modules)


def prepared(name):
    request = PreparedRequest()
    request.prepare(method="GET", url="mock://runtime/" + name)
    request.name = name
    return request


def response_for(request, name, content=b"ok"):
    response = Response()
    response.status_code = 200
    response.request = request
    response.url = request.url
    response._content = content
    response._content_consumed = True
    response.history = []
    response.name = name
    return response


def exception_record(error, sentinel=None):
    return {
        "type": [type(error).__module__, type(error).__qualname__],
        "args": list(error.args),
        "identity": error is sentinel,
        "traceback": error.__traceback__ is not None,
        "cause": error.__cause__ is None,
        "context": error.__context__ is None,
        "suppressed": error.__suppress_context__,
    }


class ScriptedAdapter(BaseAdapter):
    def __init__(self, state, name, action=None):
        self.state = state
        self.name = name
        self.action = action
        self.calls = 0

    def send(self, request, **kwargs):
        self.calls += 1
        self.state.events.append([
            "adapter", self.name, self.calls, request.name,
            threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter,
        ])
        if self.action is not None:
            return self.action(request, kwargs)
        return response_for(request, self.name + "-response")

    def close(self):
        self.state.events.append(["adapter-close", self.name])


def mounted_session(adapter):
    session = requests.Session()
    session.mount("mock://", adapter)
    return session


def signal_after_pipe(read_fd, release=None):
    assert os.read(read_fd, 1) == b"1"
    os.kill(os.getpid(), signal.SIGINT)
    if release is not None:
        release()


def bounded_join(thread):
    thread.join(2)
    if thread.is_alive():
        raise TimeoutError("runtime helper thread did not terminate")


def exact_interrupt(marker, ready, operation, release):
    previous = signal.getsignal(signal.SIGINT)
    armed = threading.Event()
    def raise_marker(signum, frame):
        raise marker
    def interrupt():
        assert ready.wait(2)
        assert armed.wait(2)
        os.kill(os.getpid(), signal.SIGINT)
    signal.signal(signal.SIGINT, raise_marker)
    interrupter = threading.Thread(target=interrupt)
    interrupter.start()
    try:
        armed.set()
        operation()
    except BaseException as error:
        if error is not marker:
            raise AssertionError("interrupt replaced exact marker") from error
        record = exception_record(error, marker)
    else:
        raise AssertionError("interrupt marker was not raised")
    finally:
        release()
        bounded_join(interrupter)
        signal.signal(signal.SIGINT, previous)
    return record


def run_runtime_producer(invoke):
    return invoke(subject, scenario, gates)


class CandidateDelegationPoison(BaseException):
    pass


def forbid_python_session_send(*args, **kwargs):
    raise CandidateDelegationPoison("candidate delegated to Python Session.send")


def forbid_frozen_oracle(*args, **kwargs):
    raise CandidateDelegationPoison("candidate delegated to frozen oracle")
"""


_CASE_BODIES = {
    "B01": r"""
events = []
observer_entered = threading.Event()
observer_done = threading.Event()
state = SimpleNamespace(events=events, response=None)
request = prepared("affinity")
response = response_for(request, "affinity-response")
state.response = response

class Body:
    def read(self):
        events.append(["body", threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter])
        return b"payload"

class Auth:
    def __call__(self, prepared_request):
        events.append(["auth", prepared_request is request, threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter])
        return prepared_request

class Clock:
    def __init__(self):
        self.calls = 0
    def __call__(self):
        self.calls += 1
        events.append(["clock", self.calls, threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter])
        return self.calls

class Cookie:
    def __call__(self, returned):
        events.append(["cookie", returned is response, threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter])

class Global:
    def __call__(self):
        events.append(["global", threading.get_ident() == entry_thread, id(sys.modules) == entry_interpreter])
        return "live"

body = Body()
auth = Auth()
clock = Clock()
cookie = Cookie()
global_authority = Global()

def hook(response, **kwargs):
    events.append([
        "hook", response.name, threading.get_ident() == entry_thread,
        id(sys.modules) == entry_interpreter,
    ])
    return response

request.hooks["response"] = [hook]
def send_affinity(prepared_request, kwargs):
    events.append(["worker-wait-entered"])
    observer_entered.set()
    assert observer_done.wait(2)
    events.append(["observer-progress", observer_done.is_set()])
    return response

adapter = ScriptedAdapter(state, "affinity", send_affinity)
session = mounted_session(adapter)
subject = SimpleNamespace(
    session=session, request=request, response=response, adapter=adapter,
    body=body, auth=auth, clock=clock, cookie=cookie,
    global_authority=global_authority, hook=hook, events=events,
)
scenario = {"id": CASE_ID, "operation": "diff-affinity-pipeline", "generation": 1, "program": ("global", "auth", "body", "clock", "adapter", "clock", "hook", "cookie")}
gates = {"entry_thread": entry_thread, "entry_interpreter": entry_interpreter, "observer_entered": observer_entered, "observer_done": observer_done}

def frozen_oracle(subject, scenario, gates):
    def observe_progress():
        assert gates["observer_entered"].wait(2)
        gates["observer_done"].set()
    observer = threading.Thread(target=observe_progress)
    observer.start()
    subject.global_authority()
    subject.auth(subject.request)
    subject.body.read()
    subject.clock()
    returned = subject.adapter.send(subject.request)
    subject.clock()
    returned = subject.hook(returned)
    subject.cookie(returned)
    bounded_join(observer)
    return {
        "response_identity": returned is subject.response,
        "response_name": returned.name,
        "events": subject.events,
        "callback_count": len([event for event in subject.events if event[0] in scenario["program"]]),
        "all_callbacks_on_entry": all(
            all(flag is True for flag in event[-2:])
            for event in subject.events
            if event[0] in {"global", "auth", "body", "clock", "hook", "cookie"}
        ),
        "observer_progress": gates["observer_done"].is_set(),
        "generation": scenario["generation"],
    }
""",
    "B02": r"""
events = []
state = SimpleNamespace(events=events)
request = prepared("payload")
adapter = ScriptedAdapter(state, "payload")
session = mounted_session(adapter)
payload_schema = (
    ("SessionAction", ("request_id", "adapter_id", "generation", "correlation")),
    ("SessionReply", ("response_id", "generation", "correlation")),
    ("NativeTransfer", ("method", "url", "headers", "body_id", "generation")),
)
subject = SimpleNamespace(session=session, request=request, adapter=adapter, events=events)
scenario = {"id": CASE_ID, "operation": "diff-sealed-payload-roundtrip", "generation": 7, "payload_schema": payload_schema}
gates = {"sealed": True, "checked_allocation": 4096}

def frozen_oracle(subject, scenario, gates):
    returned = subject.session.send(subject.request)
    return {
        "response": returned.name,
        "schema": [list(item) for item in scenario["payload_schema"]],
        "sealed": gates["sealed"],
        "allocation": gates["checked_allocation"],
        "events": subject.events,
    }
""",
    "B03": r"""
events = []
generation = object()
outer_correlation = object()
nested_correlation = object()
callback_lock = threading.Lock()
outer_state = SimpleNamespace(events=events)
nested_state = SimpleNamespace(events=events)
outer_request = prepared("outer")
nested_request = prepared("nested")

class NestedDirective:
    def __init__(self, request):
        self.request = request

def nested_send(request, kwargs):
    correlation = kwargs["runtime_correlation"]
    received_generation = kwargs["runtime_generation"]
    events.append(["nested-action", correlation is nested_correlation, received_generation is generation])
    return response_for(request, "nested-response")

nested_adapter = ScriptedAdapter(nested_state, "nested", nested_send)

def nested_hook(response, **kwargs):
    acquired = callback_lock.acquire(blocking=False)
    if acquired:
        callback_lock.release()
    events.append(["nested-hook", response.name, kwargs["runtime_correlation"] is nested_correlation, acquired])
    return response

nested_request.hooks["response"] = [nested_hook]

def outer_hook(response, **kwargs):
    acquired = callback_lock.acquire(blocking=False)
    if acquired:
        callback_lock.release()
    events.append(["outer-hook", response.name, kwargs["runtime_correlation"] is outer_correlation, acquired])
    return NestedDirective(nested_request)

outer_request.hooks["response"] = [outer_hook]
def outer_send(request, kwargs):
    events.append(["outer-action", kwargs["runtime_correlation"] is outer_correlation, kwargs["runtime_generation"] is generation])
    return response_for(request, "outer-response")

outer_adapter = ScriptedAdapter(outer_state, "outer", outer_send)
outer_session = mounted_session(outer_adapter)
subject = SimpleNamespace(
    session=outer_session, request=outer_request, adapter=outer_adapter,
    nested_request=nested_request, nested_adapter=nested_adapter,
    outer_hook=outer_hook, nested_hook=nested_hook, events=events,
)
scenario = {"id": CASE_ID, "operation": "diff-nested-reentrant-send", "generation": generation, "outer_correlation": outer_correlation, "nested_correlation": nested_correlation, "program": ("outer-submit", "outer-action", "outer-hook", "nested-submit", "nested-action", "nested-hook", "outer-resume")}
gates = {"callback_lock": callback_lock}

def frozen_oracle(subject, scenario, gates):
    events.append(["outer-submit"])
    outer_response = subject.adapter.send(
        subject.request, runtime_correlation=scenario["outer_correlation"], runtime_generation=scenario["generation"]
    )
    directive = subject.outer_hook(outer_response, runtime_correlation=scenario["outer_correlation"])
    events.append(["nested-submit"])
    nested_response = subject.nested_adapter.send(
        directive.request, runtime_correlation=scenario["nested_correlation"], runtime_generation=scenario["generation"]
    )
    nested_response = subject.nested_hook(nested_response, runtime_correlation=scenario["nested_correlation"])
    events.append(["outer-resume", nested_response.name])
    return {
        "response": outer_response.name,
        "nested_response": nested_response.name,
        "events": subject.events,
        "outer_calls": subject.adapter.calls,
        "nested_calls": subject.nested_adapter.calls,
        "distinct_correlation": scenario["outer_correlation"] is not scenario["nested_correlation"],
        "same_generation_observed": ["outer-action", True, True] in events and ["nested-action", True, True] in events,
        "locks_free": all(event[-1] is True for event in events if event[0] in {"outer-hook", "nested-hook"}),
        "exact_once": [event[0] for event in events] == ["outer-submit", "adapter", "outer-action", "outer-hook", "nested-submit", "adapter", "nested-action", "nested-hook", "outer-resume"],
    }
""",
    "B04": r"""
events = []
cases = []
for stage in ("early", "middle", "late"):
    for kind in ("exception", "baseexception"):
        marker_type = Exception if kind == "exception" else BaseException
        marker = marker_type("b04-" + stage + "-" + kind)
        request = prepared(stage + "-" + kind)
        response = response_for(request, stage + "-response")
        cases.append(SimpleNamespace(stage=stage, kind=kind, marker=marker, request=request, response=response, calls=0))

clean_request = prepared("recovery")
clean_response = response_for(clean_request, "recovery-response")
def recover(request, kwargs):
    events.append(["recovery-adapter", request is clean_request])
    return clean_response
recovery_adapter = ScriptedAdapter(SimpleNamespace(events=events), "recovery", recover)
recovery_session = mounted_session(recovery_adapter)
subject = SimpleNamespace(
    cases=tuple(cases), request=clean_request, session=recovery_session,
    clean_request=clean_request, clean_response=clean_response,
    recovery_adapter=recovery_adapter, events=events,
)
scenario = {"id": CASE_ID, "operation": "diff-exact-exception-stop-recover", "generation": 4, "stages": ("early", "middle", "late"), "kinds": ("exception", "baseexception")}
gates = {"replay_forbidden": True, "owner_thread": entry_thread}

def frozen_oracle(subject, scenario, gates):
    records = []
    for case in subject.cases:
        case.calls += 1
        events.append(["begin", case.stage, case.kind])
        try:
            if case.stage == "early":
                raise case.marker
            events.append(["adapter", case.stage, case.kind])
            if case.stage == "middle":
                raise case.marker
            events.append(["hook", case.stage, case.kind])
            raise case.marker
        except BaseException as error:
            records.append([case.stage, case.kind, exception_record(error, case.marker), list(events)])
    recovered = subject.recovery_adapter.send(subject.clean_request)
    return {
        "records": records,
        "calls": [case.calls for case in subject.cases],
        "events": events,
        "replay_forbidden": gates["replay_forbidden"],
        "owner_on_origin": gates["owner_thread"] == entry_thread,
        "recovered": recovered.name,
        "recovery_calls": subject.recovery_adapter.calls,
    }
""",
    "B05": "",
    "B06": "",
    "B07": "",
    "B08": "",
    "B09": "",
    "B10": "",
    "B11": "",
    "B12": "",
    "B13": "",
    "B14": "",
    "B15": "",
    "B16": "",
    "B17": "",
    "B18": "",
}


_CASE_BODIES["B05"] = r"""
events = []
dial_entered = threading.Event()
dial_release = threading.Event()
marker = KeyboardInterrupt("b05-connect")
request = prepared("connect")

def dial(request, kwargs):
    events.append(["request-start", request.name])
    events.append(["dial-entered"])
    dial_entered.set()
    assert dial_release.wait(2)
    events.append(["dial-ready-after-cancel"])
    return response_for(request, "dirty-response")

dirty_adapter = ScriptedAdapter(SimpleNamespace(events=events), "connect", dial)
clean_adapter = ScriptedAdapter(SimpleNamespace(events=events), "connect-recovery")
session = mounted_session(dirty_adapter)
subject = SimpleNamespace(
    session=session, request=request, dirty_adapter=dirty_adapter,
    clean_adapter=clean_adapter, events=events, marker=marker,
)
scenario = {"id": CASE_ID, "operation": "diff-interrupt-connect", "phase": "connect", "generation": 5}
gates = {"dial_entered": dial_entered, "dial_release": dial_release}

def frozen_oracle(subject, scenario, gates):
    record = exact_interrupt(
        subject.marker, gates["dial_entered"],
        lambda: subject.dirty_adapter.send(subject.request),
        gates["dial_release"].set,
    )
    events.append(["cancel-observed"])
    recovered = subject.clean_adapter.send(prepared("connect-recovery"))
    return {
        "error": record, "events": events,
        "dirty_adapter_calls": subject.dirty_adapter.calls,
        "clean_adapter_calls": subject.clean_adapter.calls,
        "distinct_adapters": subject.dirty_adapter is not subject.clean_adapter,
        "signal_before_ready": events.index(["cancel-observed"]) < events.index(["adapter", "connect-recovery", 1, "connect-recovery", True, True]),
        "recovered": recovered.name,
    }
"""

_CASE_BODIES["B06"] = r"""
events = []
request_received = threading.Event()
server_release = threading.Event()
marker = KeyboardInterrupt("b06-head")
request_read, request_write = os.pipe()
response_read, response_write = os.pipe()
dirty_response_read = response_read

def server():
    data = b""
    while b"\r\n\r\n" not in data:
        data += os.read(request_read, 4096)
    request_received.set()
    assert server_release.wait(2)
    os.close(request_read)
    os.close(response_write)

server_thread = threading.Thread(target=server, daemon=True)
server_thread.start()
subject = SimpleNamespace(
    request_write=request_write, response_read=response_read,
    dirty_response_read=dirty_response_read, events=events, marker=marker,
)
scenario = {"id": CASE_ID, "operation": "diff-interrupt-response-head", "phase": "response-head", "generation": 6}
gates = {"request_received": request_received, "server_release": server_release, "server_thread": server_thread}

def frozen_oracle(subject, scenario, gates):
    os.write(subject.request_write, b"GET /head HTTP/1.1\r\nHost: local\r\n\r\n")
    os.close(subject.request_write)
    events.append(["request-sent"])
    assert gates["request_received"].wait(2)
    events.append(["request-received", True])
    record = exact_interrupt(
        subject.marker, gates["request_received"],
        lambda: os.read(subject.response_read, 4096),
        gates["server_release"].set,
    )
    os.close(subject.response_read)
    bounded_join(gates["server_thread"])
    clean_read, clean_write = os.pipe()
    distinct = clean_read != subject.dirty_response_read
    os.write(clean_write, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
    os.close(clean_write)
    recovered = os.read(clean_read, 4096)
    os.close(clean_read)
    return {
        "error": record, "events": events,
        "no_post_head_actions": [event[0] for event in events] == ["request-sent", "request-received"],
        "distinct_connection": distinct, "recovered": recovered.endswith(b"ok"),
    }
"""

_CASE_BODIES["B07"] = r"""
events = []
partial_sent = threading.Event()
server_release = threading.Event()
marker = KeyboardInterrupt("b07-read")
request_read, request_write = os.pipe()
response_read, response_write = os.pipe()
dirty_response_read = response_read

def server():
    assert os.read(request_read, 4096).endswith(b"\r\n\r\n")
    os.write(response_write, b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab")
    partial_sent.set()
    assert server_release.wait(2)
    os.close(request_read)
    os.close(response_write)

server_thread = threading.Thread(target=server, daemon=True)
server_thread.start()
subject = SimpleNamespace(
    request_write=request_write, response_read=response_read,
    dirty_response_read=dirty_response_read, events=events, marker=marker,
)
scenario = {"id": CASE_ID, "operation": "diff-interrupt-response-read", "phase": "response-read", "declared": 5, "received": 2, "generation": 7}
gates = {"partial_sent": partial_sent, "server_release": server_release, "server_thread": server_thread}

def frozen_oracle(subject, scenario, gates):
    os.write(subject.request_write, b"GET /read HTTP/1.1\r\nHost: local\r\n\r\n")
    os.close(subject.request_write)
    first = os.read(subject.response_read, 4096)
    assert first.endswith(b"ab")
    assert gates["partial_sent"].wait(2)
    events.append(["partial-body", 2, 5])
    events.append(["read-entered", scenario["received"], scenario["declared"]])
    record = exact_interrupt(
        subject.marker, gates["partial_sent"],
        lambda: os.read(subject.response_read, 4096),
        gates["server_release"].set,
    )
    os.close(subject.response_read)
    bounded_join(gates["server_thread"])
    clean_read, clean_write = os.pipe()
    distinct = clean_read != subject.dirty_response_read
    os.write(clean_write, b"clean")
    os.close(clean_write)
    recovered = os.read(clean_read, 5)
    os.close(clean_read)
    return {
        "error": record, "events": events,
        "no_eof_or_final_content": [event[0] for event in events] == ["partial-body", "read-entered"],
        "distinct_connection": distinct, "recovered": recovered == b"clean",
    }
"""

_CASE_BODIES["B08"] = r"""
events = []
upload_entered = threading.Event()
upload_release = threading.Event()
marker = KeyboardInterrupt("b08-upload")

class UploadBody:
    def __init__(self):
        self.reads = 0
        self.synthetic_closes = 0
    def read(self):
        self.reads += 1
        events.append(["upload-action", "entered", self.reads, threading.get_ident() == entry_thread])
        upload_entered.set()
        assert upload_release.wait(2)
        events.append(["upload-action", "reply-ready", self.reads])
        return b"payload"
    def close(self):
        self.synthetic_closes += 1

body = UploadBody()
clean_body = UploadBody()
subject = SimpleNamespace(body=body, clean_body=clean_body, events=events, marker=marker)
scenario = {"id": CASE_ID, "operation": "diff-interrupt-upload", "phase": "upload", "generation": 8}
gates = {"upload_entered": upload_entered, "upload_release": upload_release}

def frozen_oracle(subject, scenario, gates):
    record = exact_interrupt(
        subject.marker, gates["upload_entered"], subject.body.read,
        gates["upload_release"].set,
    )
    events.append(["cancel-observed"])
    recovered = subject.clean_body.read()
    return {
        "error": record, "events": events,
        "queued": 1, "executed": subject.body.reads, "reply_observed": 0,
        "body_not_reread": subject.body.reads == 1,
        "no_synthetic_close": subject.body.synthetic_closes == 0,
        "distinct_body": subject.body is not subject.clean_body,
        "recovered": recovered == b"payload",
    }
"""

_CASE_BODIES["B09"] = r"""
events = []
destructors = []
leaked = []

class Owner:
    def __init__(self, phase):
        self.phase = phase
    def __del__(self):
        destructors.append([self.phase, threading.get_ident()])

phases = (
    "before-poll", "queued-before-dequeue", "reply-observed",
    "terminal-after-timeout", "permanently-nonterminal",
)
cases = []
for phase in phases:
    cases.append(SimpleNamespace(
        phase=phase, marker=KeyboardInterrupt("b09-" + phase), owner=Owner(phase),
        queued=0, executed=0, replies=0, entered=threading.Event(),
        dequeue=threading.Event(), release=threading.Event(), terminal=threading.Event(),
        reply_delivered=None,
    ))
subject = SimpleNamespace(cases=tuple(cases), events=events, leaked=leaked)
scenario = {"id": CASE_ID, "operation": "diff-cancellation-quarantine-matrix", "generation": 9, "phases": phases}
gates = {"owner_thread": entry_thread}

def frozen_oracle(subject, scenario, gates):
    records = []
    for case in subject.cases:
        case.queued = 0 if case.phase == "before-poll" else 1
        def worker(current=case):
            if current.phase == "queued-before-dequeue":
                current.entered.set()
                assert current.dequeue.wait(2)
                current.terminal.set()
                return
            current.executed += 1
            if current.phase == "reply-observed":
                current.replies += 1
                current.terminal.set()
                current.entered.set()
                return
            if current.phase == "permanently-nonterminal":
                current.entered.set()
                threading.Event().wait()
                return
            if current.phase == "terminal-after-timeout":
                current.reply_delivered = False
            current.entered.set()
            assert current.release.wait(2)
            current.terminal.set()
        worker_thread = None
        if case.phase != "before-poll":
            worker_thread = threading.Thread(target=worker, daemon=case.phase == "permanently-nonterminal")
            worker_thread.start()
        ready = threading.Event()
        if case.phase == "before-poll":
            ready.set()
        else:
            assert case.entered.wait(2)
            ready.set()
        record = exact_interrupt(case.marker, ready, lambda: threading.Event().wait(2), lambda: None)
        events.append(["cancel", case.phase, case.queued, case.executed, case.replies])
        if case.phase == "queued-before-dequeue":
            case.dequeue.set()
        elif case.phase == "terminal-after-timeout":
            case.release.set()
        if case.phase == "permanently-nonterminal":
            leaked.append([case.owner, worker_thread])
            case.owner = None
        elif worker_thread is not None:
            bounded_join(worker_thread)
        owner = case.owner
        owner_ref = weakref.ref(owner) if owner is not None else None
        case.owner = None
        owner = None
        gc.collect()
        records.append([
            case.phase, record, case.queued, case.executed, case.replies,
            None if owner_ref is None else owner_ref() is None,
            case.reply_delivered,
            case.terminal.is_set(),
        ])
    recovery = [case.phase for case in subject.cases]
    return {
        "records": records, "events": events,
        "destructors": [[phase, "entry" if thread_id == gates["owner_thread"] else "other"] for phase, thread_id in destructors],
        "nonterminal_leaked": len(leaked) == 1,
        "recovered": recovery == list(scenario["phases"]),
    }
"""

_FORK_BODY = r"""
events = []
state = SimpleNamespace(events=events)
adapter = ScriptedAdapter(state, "fork")
session = mounted_session(adapter)
if PREFORK_SEND:
    session.send(prepared("prefork"))
parent_pid = os.getpid()
parent_generation = 1
read_fd, write_fd = os.pipe()
subject = SimpleNamespace(session=session, adapter=adapter, events=events)
scenario = {"id": CASE_ID, "operation": "diff-fork-session-roundtrip", "prefork": PREFORK, "parent_pid": parent_pid, "parent_generation": parent_generation}
gates = {"read_fd": read_fd, "write_fd": write_fd}

def frozen_oracle(subject, scenario, gates):
    child_pid = os.fork()
    if child_pid == 0:
        os.close(gates["read_fd"])
        try:
            child_response = subject.session.send(prepared("child"))
            payload = (str(os.getpid()) + ":2:" + child_response.name).encode()
            os.write(gates["write_fd"], payload)
        finally:
            os.close(gates["write_fd"])
            os._exit(0)
    os.close(gates["write_fd"])
    ready, _, _ = select.select([gates["read_fd"]], [], [], 2)
    if not ready:
        os.kill(child_pid, signal.SIGKILL)
        os.waitpid(child_pid, 0)
        raise TimeoutError("fork child did not reach result gate")
    child_payload = os.read(gates["read_fd"], 4096).decode().split(":", 2)
    os.close(gates["read_fd"])
    waited, status = os.waitpid(child_pid, 0)
    parent_response = subject.session.send(prepared("parent"))
    return {
        "prefork": scenario["prefork"], "child_pid_changed": int(child_payload[0]) != scenario["parent_pid"],
        "child_generation": int(child_payload[1]), "parent_generation": scenario["parent_generation"],
        "generation_advanced": int(child_payload[1]) > scenario["parent_generation"],
        "child_response": child_payload[2], "parent_response": parent_response.name,
        "child_exit": os.waitstatus_to_exitcode(status), "waited": waited == child_pid,
        "parent_usable": True,
    }
"""

for _case_id, _prefork, _send in (
    ("B10", "import", False),
    ("B11", "driver", False),
    ("B12", "pool", True),
):
    _CASE_BODIES[_case_id] = (
        f"PREFORK = {_prefork!r}\nPREFORK_SEND = {_send!r}\n" + _FORK_BODY
    )

_CASE_BODIES["B13"] = r"""
events = []
first_adapter = ScriptedAdapter(SimpleNamespace(events=events), "session-one")
second_adapter = ScriptedAdapter(SimpleNamespace(events=events), "session-two")
first = mounted_session(first_adapter)
second = mounted_session(second_adapter)
subject = SimpleNamespace(first=first, second=second, first_adapter=first_adapter, second_adapter=second_adapter, events=events)
scenario = {"id": CASE_ID, "operation": "diff-multi-session-peer-survival", "first_generation": 1, "second_generation": 1}
gates = {"shared_runtime": True, "shared_adapter": False}

def frozen_oracle(subject, scenario, gates):
    first_result = subject.first.send(prepared("one"))
    second_result = subject.second.send(prepared("two"))
    subject.first.close()
    again = subject.second.send(prepared("two-again"))
    return {
        "responses": [first_result.name, second_result.name, again.name], "events": subject.events,
        "counts": [subject.first_adapter.calls, subject.second_adapter.calls],
        "peer_survived": again.name == "session-two-response", "shared_runtime": gates["shared_runtime"],
        "separate_adapters": not gates["shared_adapter"],
    }
"""

_CASE_BODIES["B14"] = r"""
events = []
request = prepared("stream")
stream_response = response_for(request, "stream-response", b"retained")
stream_response._content_consumed = False

def stream_send(request, kwargs):
    events.append(["stream-open", request.name])
    return stream_response

stream_adapter = ScriptedAdapter(SimpleNamespace(events=events), "stream", stream_send)
peer_adapter = ScriptedAdapter(SimpleNamespace(events=events), "peer")
stream_session = mounted_session(stream_adapter)
peer_session = mounted_session(peer_adapter)
subject = SimpleNamespace(stream_session=stream_session, peer_session=peer_session, request=request, response=stream_response, events=events)
scenario = {"id": CASE_ID, "operation": "diff-outstanding-stream-peer-clear", "stream_generation": 1, "peer_generation": 1}
gates = {"clear_peer": True}

def frozen_oracle(subject, scenario, gates):
    outstanding = subject.stream_session.send(subject.request, stream=True)
    subject.peer_session.close()
    payload = outstanding.content
    outstanding.close()
    return {
        "identity": outstanding is subject.response, "payload": payload.decode(), "events": subject.events,
        "peer_clear": gates["clear_peer"], "stream_survived": payload == b"retained",
        "generations": [scenario["stream_generation"], scenario["peer_generation"]],
    }
"""

_CASE_BODIES["B15"] = r"""
events = []
destructors = []
state = SimpleNamespace(events=events)
adapter = ScriptedAdapter(state, "finalize")
session = mounted_session(adapter)

class Finalized:
    def __del__(self):
        destructors.append(threading.get_ident())

tracked = Finalized()
tracked_ref = weakref.ref(tracked)
subject = SimpleNamespace(session=session, adapter=adapter, tracked=tracked, events=events)
scenario = {"id": CASE_ID, "operation": "diff-idempotent-session-close", "generation": 15, "shutdown_bound_ms": 500}
gates = {"terminal": threading.Event()}

def frozen_oracle(subject, scenario, gates):
    subject.session.close()
    return None

def run_runtime_producer(invoke):
    global tracked
    invoke(subject, scenario, gates)
    invoke(subject, scenario, gates)
    subject.tracked = None
    tracked = None
    gc.collect()
    gates["terminal"].set()
    return {
        "events": events, "released": tracked_ref() is None,
        "destructor_threads": ["entry" if item == entry_thread else "other" for item in destructors],
        "terminal": gates["terminal"].is_set(), "bound_ms": scenario["shutdown_bound_ms"],
        "duplicate_close_calls": len([item for item in events if item[0] == "adapter-close"]),
    }
"""

_CASE_BODIES["B16"] = r"""
events = []
state = SimpleNamespace(events=events)
request = prepared("panic")
marker = RuntimeError("native worker panicked")

def panic_once(request, kwargs):
    events.append(["panic", "raised"])
    raise marker

adapter = ScriptedAdapter(state, "panic", panic_once)
session = mounted_session(adapter)
subject = SimpleNamespace(session=session, request=request, adapter=adapter, marker=marker, events=events)
scenario = {"id": CASE_ID, "operation": "diff-panic-translate-recover", "generation": 16, "recovery_generation": 17, "inject_native_panic": True}
gates = {"owner_stranded": False}

def frozen_oracle(subject, scenario, gates):
    try:
        subject.session.send(subject.request)
    except RuntimeError as error:
        record = exception_record(error, subject.marker)
    recovery = ScriptedAdapter(SimpleNamespace(events=events), "recovery")
    subject.session.mount("mock://", recovery)
    returned = subject.session.send(prepared("after-panic"))
    return {
        "error": record, "events": events, "generation": scenario["generation"],
        "recovery_generation": scenario["recovery_generation"],
        "advanced": scenario["recovery_generation"] > scenario["generation"],
        "recovered": returned.name, "owner_stranded": gates["owner_stranded"],
    }
"""

_CASE_BODIES["B17"] = r"""
events = []
state = SimpleNamespace(events=events)
request = prepared("live-mutation")
entered = threading.Event()
release = threading.Event()

class Clock:
    def __init__(self, name, value):
        self.name = name
        self.value = value
    def __call__(self):
        events.append(["clock", self.name])
        if self.name == "initial":
            entered.set()
        return self.value

initial_clock = Clock("initial", 1)
live_clock = Clock("live", 4)

def await_adapter(request, kwargs):
    events.append(["await", "entered"])
    assert release.wait(2)
    return response_for(request, "mutated-response")

adapter = ScriptedAdapter(state, "mutation", await_adapter)
session = mounted_session(adapter)
subject = SimpleNamespace(session=session, request=request, adapter=adapter, events=events)
scenario = {"id": CASE_ID, "operation": "diff-send-reload-preferred-clock", "generation": 17, "mutation": "preferred_clock"}
gates = {"entered": entered, "release": release}

def frozen_oracle(subject, scenario, gates):
    return subject.session.send(subject.request)

def run_runtime_producer(invoke):
    original = sessions_module.preferred_clock
    sessions_module.preferred_clock = initial_clock
    def mutate():
        assert gates["entered"].wait(2)
        sessions_module.preferred_clock = live_clock
        events.append(["mutation", scenario["mutation"]])
        gates["release"].set()
    mutator = threading.Thread(target=mutate)
    mutator.start()
    try:
        returned = invoke(subject, scenario, gates)
    finally:
        bounded_join(mutator)
        sessions_module.preferred_clock = original
    return {"response": returned.name, "events": events, "elapsed": returned.elapsed.total_seconds(), "live_clock_used": ["clock", "live"] in events}
"""

_CASE_BODIES["B18"] = r"""
events = []
traces = {"first": [], "second": []}
first_entered = threading.Event()
second_entered = threading.Event()
first_release = threading.Event()
second_release = threading.Event()

def correlated_action(label, entered, release):
    def action(request, kwargs):
        events.append(["entered", label, request.name])
        traces[label].append(["entered", request.name])
        entered.set()
        assert release.wait(2)
        events.append(["released", label, request.name])
        traces[label].append(["released", request.name])
        return response_for(request, label + "-response", label.encode())
    return action

first_adapter = ScriptedAdapter(SimpleNamespace(events=events), "first", correlated_action("first", first_entered, first_release))
second_adapter = ScriptedAdapter(SimpleNamespace(events=events), "second", correlated_action("second", second_entered, second_release))
first_request = prepared("request-one")
second_request = prepared("request-two")
first_subject = SimpleNamespace(adapter=first_adapter, request=first_request, events=events, channel_id="first")
second_subject = SimpleNamespace(adapter=second_adapter, request=second_request, events=events, channel_id="second")
subject = SimpleNamespace(first=first_subject, second=second_subject, events=events)
scenario = {"id": CASE_ID, "operation": "diff-correlated-adapter-send", "first_correlation": 1801, "second_correlation": 1802, "generation": 18}
gates = {"first_entered": first_entered, "second_entered": second_entered, "first_release": first_release, "second_release": second_release}

def frozen_oracle(subject, scenario, gates):
    return subject.adapter.send(subject.request)

def run_runtime_producer(invoke):
    results = {}
    errors = []
    release_order = []
    completion_order = []
    def run(name, current_subject, correlation, entered, release):
        try:
            results[name] = invoke(
                current_subject,
                {
                    "operation": "diff-correlated-adapter-send",
                    "channel_id": name,
                    "correlation": correlation,
                    "correlation_id": correlation,
                    "generation": scenario["generation"],
                    "sequence": 1,
                    "request_id": 1811 if name == "first" else 1821,
                    "response_id": 1812 if name == "first" else 1822,
                    "error_id": None,
                    "reply_generation": scenario["generation"],
                    "reply_correlation_id": correlation,
                    "reply_sequence": 1,
                    "reply_request_id": 1811 if name == "first" else 1821,
                    "reply_response_id": 1812 if name == "first" else 1822,
                    "reply_error_id": None,
                    "peer_correlation_id": scenario["second_correlation"]
                    if name == "first"
                    else scenario["first_correlation"],
                },
                {"entered": entered, "release": release},
            )
        except BaseException as error:
            errors.append(error)
    first_thread = threading.Thread(target=run, args=("first", subject.first, scenario["first_correlation"], gates["first_entered"], gates["first_release"]))
    second_thread = threading.Thread(target=run, args=("second", subject.second, scenario["second_correlation"], gates["second_entered"], gates["second_release"]))
    first_thread.start()
    second_thread.start()
    assert gates["first_entered"].wait(2)
    assert gates["second_entered"].wait(2)
    both_entered_before_release = (
        gates["first_entered"].is_set() and gates["second_entered"].is_set()
    )
    gates["second_release"].set()
    release_order.append("second")
    bounded_join(second_thread)
    completion_order.append("second")
    gates["first_release"].set()
    release_order.append("first")
    bounded_join(first_thread)
    completion_order.append("first")
    if errors:
        raise errors[0]
    return {
        "traces": {name: list(trace) for name, trace in traces.items()},
        "both_entered_before_release": both_entered_before_release,
        "release_order": release_order,
        "completion_order": completion_order,
        "first_identity": results["first"].request is subject.first.request,
        "second_identity": results["second"].request is subject.second.request,
        "first_payload": results["first"].content.decode(),
        "second_payload": results["second"].content.decode(),
        "correlations": [scenario["first_correlation"], scenario["second_correlation"]],
        "distinct": scenario["first_correlation"] != scenario["second_correlation"],
        "reply_theft": False,
    }
"""


def _cluster_for(case_id: str) -> str:
    return next(
        name for name, case_ids in PHASE_B_CLUSTERS.items() if case_id in case_ids
    )


def runtime_case_source(case: RuntimeCase) -> str:
    assignments = (
        f"CASE_ID = {case.case_id!r}\n"
        f"SCENARIO_NAME = {case.scenario!r}\n"
        f"CLUSTER = {_cluster_for(case.case_id)!r}\n"
    )
    final_call = r"""
if target == "rewrite":
    sessions_module.Session.send = forbid_python_session_send
    frozen_oracle = forbid_frozen_oracle
    candidate_result = run_runtime_producer(
        lambda subject, scenario, gates: _requests_rust._session_runtime_trial(subject, scenario, gates)
    )
else:
    candidate_result = run_runtime_producer(frozen_oracle)
result = candidate_result
"""
    return dedent(
        assignments + _COMMON_SOURCE + _CASE_BODIES[case.case_id] + final_call
    )


def is_missing_session_runtime_trial(observations: dict[str, object]) -> bool:
    exception = observations["exception"]
    if not isinstance(exception, dict):
        return False
    mro = exception.get("mro")
    args = exception.get("args")
    return (
        isinstance(mro, list)
        and bool(mro)
        and mro[0] == {"module": "builtins", "name": "AttributeError"}
        and isinstance(args, list)
        and len(args) == 1
        and "_session_runtime_trial" in str(args[0])
    )


def test_phase_b_inventory_and_candidate_owned_call_are_exact() -> None:
    assert tuple(case.case_id for case in PHASE_B_CASES) == PHASE_B_CASE_IDS
    assert tuple(PHASE_B_SCENARIOS) == PHASE_B_CASE_IDS
    assert (
        tuple(case_id for case_ids in PHASE_B_CLUSTERS.values() for case_id in case_ids)
        == PHASE_B_CASE_IDS
    )
    assert len(set(PHASE_B_CASE_IDS)) == 18
    assert all(_CASE_BODIES[case_id].strip() for case_id in PHASE_B_CASE_IDS)
    for case in PHASE_B_CASES:
        tree = ast.parse(runtime_case_source(case))
        seam_calls = [
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "_session_runtime_trial"
        ]
        assert len(seam_calls) == 1
        last = tree.body[-1]
        assert isinstance(last, ast.Assign)
        assert [
            target.id for target in last.targets if isinstance(target, ast.Name)
        ] == ["result"]
        candidate_assignment = next(
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.Assign)
            and any(
                isinstance(target, ast.Name) and target.id == "candidate_result"
                for target in node.targets
            )
            and seam_calls[0] in list(ast.walk(node.value))
        )
        assert isinstance(candidate_assignment.value, ast.Call)
        assert isinstance(last.value, ast.Name)
        assert last.value.id == "candidate_result"
        poison_assignments = [
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.Assign)
            and any(
                isinstance(target, ast.Attribute)
                and target.attr == "send"
                and isinstance(target.value, ast.Attribute)
                and target.value.attr == "Session"
                for target in node.targets
            )
        ]
        assert len(poison_assignments) == 1


@pytest.mark.parametrize("case", PHASE_B_CASES[:4], ids=lambda case: case.case_id)
def test_b01_b04_python_delegation_counterfeit_hits_active_poison(
    case: RuntimeCase,
) -> None:
    source = runtime_case_source(case)
    anchor = 'if target == "rewrite":\n    sessions_module.Session.send = '
    counterfeit = r"""
if target == "rewrite":
    def counterfeit_trial(subject, scenario, gates):
        return subject.session.send(subject.request)
    _requests_rust._session_runtime_trial = counterfeit_trial
"""
    assert source.count(anchor) == 1
    changed = source.replace(anchor, dedent(counterfeit) + anchor, 1)
    rewrite = run_rewrite_case({"source": changed})
    exception = rewrite.observations["exception"]
    assert isinstance(exception, dict)
    assert exception["mro"][0] == {
        "module": "__differential_case__",
        "name": "CandidateDelegationPoison",
    }
    assert exception["args"] == ["candidate delegated to Python Session.send"]
    assert rewrite.stderr == ""


@pytest.mark.parametrize("case", PHASE_B_CASES, ids=lambda case: case.case_id)
def test_phase_b_clean_oracle_and_native_runtime_available(case: RuntimeCase) -> None:
    payload = {"source": runtime_case_source(case)}
    oracle = run_oracle_case(payload)
    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""
    rewrite = run_rewrite_case(payload)
    assert not is_missing_session_runtime_trial(rewrite.observations)
    assert rewrite.observations["exception"] is None
    assert rewrite.stderr == ""


@pytest.mark.parametrize("case", PHASE_B_CASES, ids=lambda case: case.case_id)
def test_phase_b_runtime_matrix_matches_frozen_oracle(case: RuntimeCase) -> None:
    payload = {"source": runtime_case_source(case)}
    oracle = run_oracle_case(payload)
    rewrite = run_rewrite_case(payload)
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr

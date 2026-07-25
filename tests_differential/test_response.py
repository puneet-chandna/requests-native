from __future__ import annotations

import ast
from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case

_TRIAL_HELPERS = """
import gc
import os
import threading

from requests import Response

try:
    from requests import _requests_rust
except ImportError as extension_error:
    if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
        raise RuntimeError("rewrite response extension is unavailable") from extension_error
    _requests_rust = None


ENTRY_THREAD = threading.get_ident()
_RESPONSE_TRIAL_SYMBOLS = {
    "_response_content_trial",
    "_response_iter_content_trial",
    "_response_iter_lines_trial",
    "_response_text_trial",
    "_response_apparent_encoding_trial",
    "_response_json_trial",
    "_response_metadata_trial",
    "_response_pickle_trial",
    "_response_close_trial",
    "_response_drop_trial",
    "_response_disposition_trial",
    "_response_lifecycle_trial",
    "_response_fields_snapshot",
}


def is_missing_response_trial(error):
    return (
        _requests_rust is not None
        and isinstance(error, AttributeError)
        and getattr(error, "obj", None) is _requests_rust
        and getattr(error, "name", None) in _RESPONSE_TRIAL_SYMBOLS
    )


def same_thread():
    return threading.get_ident() == ENTRY_THREAD


def value_record(value):
    value_type = type(value)
    if value is None:
        payload = None
    elif isinstance(value, bytes):
        payload = ["bytes", bytes(value).hex()]
    elif isinstance(value, str):
        payload = ["str", value]
    elif isinstance(value, (bool, int)):
        payload = value
    else:
        payload = ["opaque", value_type.__module__, value_type.__qualname__]
    return {
        "type": [value_type.__module__, value_type.__qualname__],
        "payload": payload,
    }


def exception_record(error):
    error_type = type(error)
    return {
        "type": [error_type.__module__, error_type.__qualname__],
        "args": [
            value
            if isinstance(value, (bool, int, float, str, bytes, type(None)))
            else value_record(value)
            for value in error.args
        ],
    }


def capture(operation):
    try:
        returned = operation()
    except BaseException as error:
        if is_missing_response_trial(error):
            raise
        return {
            "returned": None,
            "exception": exception_record(error),
            "error": error,
        }
    return {
        "returned": returned,
        "exception": None,
        "error": None,
    }


def local_response_fields_snapshot(subject):
    content = subject._content
    return {
        "content": value_record(content),
        "content_is_false": content is False,
        "content_consumed": subject._content_consumed,
        "raw_is_none": subject.raw is None,
    }


def response_fields_snapshot(subject):
    if _requests_rust is not None:
        return _requests_rust._response_fields_snapshot(subject)
    return local_response_fields_snapshot(subject)


def response_content_call(subject):
    if _requests_rust is not None:
        return _requests_rust._response_content_trial(subject)
    return subject.content


def response_iter_content_call(subject, chunk_size=1, decode_unicode=False):
    if _requests_rust is not None:
        return _requests_rust._response_iter_content_trial(
            subject, chunk_size, decode_unicode
        )
    return subject.iter_content(chunk_size, decode_unicode=decode_unicode)


def response_iter_lines_call(
    subject, chunk_size=512, decode_unicode=False, delimiter=None
):
    if _requests_rust is not None:
        return _requests_rust._response_iter_lines_trial(
            subject, chunk_size, decode_unicode, delimiter
        )
    return subject.iter_lines(
        chunk_size=chunk_size,
        decode_unicode=decode_unicode,
        delimiter=delimiter,
    )


def response_text_call(subject):
    if _requests_rust is not None:
        return _requests_rust._response_text_trial(subject)
    return subject.text


def response_apparent_encoding_call(subject):
    if _requests_rust is not None:
        return _requests_rust._response_apparent_encoding_trial(subject)
    return subject.apparent_encoding


def response_json_call(subject, kwargs):
    if _requests_rust is not None:
        return _requests_rust._response_json_trial(subject, kwargs)
    return subject.json(**kwargs)


def response_metadata_call(subject, operation):
    if _requests_rust is not None:
        return _requests_rust._response_metadata_trial(subject, operation)
    if operation == "repr":
        return repr(subject)
    if operation == "bool":
        return bool(subject)
    if operation == "ok":
        return subject.ok
    if operation == "is_redirect":
        return subject.is_redirect
    if operation == "is_permanent_redirect":
        return subject.is_permanent_redirect
    if operation == "next":
        return subject.next
    if operation == "history":
        return subject.history
    if operation == "raise_for_status":
        return subject.raise_for_status()
    raise AssertionError(operation)


def response_pickle_call(subject, operation, state=None):
    if _requests_rust is not None:
        return _requests_rust._response_pickle_trial(subject, operation, state)
    if operation == "get":
        return subject.__getstate__()
    if operation == "set":
        return subject.__setstate__(state)
    raise AssertionError(operation)


def response_close_call(subject):
    if _requests_rust is not None:
        return _requests_rust._response_close_trial(subject)
    return subject.close()


def response_drop_call(raw, operation, chunk_size=2):
    if _requests_rust is not None:
        return _requests_rust._response_drop_trial(
            raw, operation, chunk_size
        )

    subject = Response()
    subject.status_code = 200
    subject.raw = raw
    iterator = None
    try:
        if operation == "partial":
            iterator = subject.iter_content(chunk_size)
            next(iterator)
        elif operation == "exhausted":
            list(subject.iter_content(chunk_size))
        elif operation == "cached":
            subject.content
        elif operation == "failed":
            iterator = subject.iter_content(chunk_size)
            next(iterator)
        elif operation != "untouched":
            raise AssertionError(operation)
        return {
            "raw_is_original": subject.raw is raw,
            "state": local_response_fields_snapshot(subject),
        }
    finally:
        del iterator
        del subject
        gc.collect()


def local_response_disposition(events):
    state = "Open"
    decision = None
    decision_count = 0
    native_lease = True
    states = [state]
    dirty_events = {
        "close",
        "drop",
        "read-error",
        "decode-error",
        "protocol-error",
        "cancel",
        "action-disconnect",
        "reply-disconnect",
    }
    for event in events:
        if event == "python-exact" and state == "Open":
            state = "PythonExact"
            native_lease = False
        elif state in ("Reusable", "CloseDirty", "PythonExact"):
            pass
        elif event == "partial":
            state = "Partial"
        elif event == "clean-eof":
            state = "Reusable"
            decision = "Reusable"
            decision_count += 1
        elif event in dirty_events:
            state = "CloseDirty"
            decision = "CloseDirty"
            decision_count += 1
        else:
            raise AssertionError(event)
        states.append(state)
    return {
        "states": states,
        "final": state,
        "decision": decision,
        "decision_count": decision_count,
        "native_lease": native_lease,
        "implicit_python_callbacks": 0,
    }


def response_disposition_call(events):
    if _requests_rust is not None:
        return _requests_rust._response_disposition_trial(events)
    return local_response_disposition(events)


def response_lifecycle_call(subject, error, phase, audit):
    if _requests_rust is not None:
        return _requests_rust._response_lifecycle_trial(
            subject, error, phase, audit
        )
    if phase == "before-poll":
        audit.update({
            "queued": 0,
            "execute": 0,
            "reply_observed": False,
            "worker_dropped": True,
            "raw_actions": 0,
        })
    elif phase == "queued-before-dequeue":
        audit.update({
            "queued": 1,
            "execute": 0,
            "reply_observed": False,
            "worker_dropped": True,
            "raw_actions": 0,
        })
    elif phase == "reply-observed":
        next(subject.iter_content(1))
        audit.update({
            "queued": 1,
            "execute": 1,
            "reply_observed": True,
            "worker_dropped": True,
            "raw_actions": 1,
        })
    else:
        raise AssertionError(phase)
    raise error


class ObservedStreamRaw:
    def __init__(self, chunks=(), error=None):
        self.chunks = list(chunks)
        self.error = error
        self.events = []

    def __getattribute__(self, name):
        if name == "stream":
            object.__getattribute__(self, "events").append(
                ["getattr", "stream", same_thread()]
            )
        return object.__getattribute__(self, name)

    def stream(self, chunk_size, decode_content=True):
        self.events.append(
            ["stream", value_record(chunk_size), decode_content, same_thread()]
        )
        for chunk in self.chunks:
            self.events.append(["yield", value_record(chunk), same_thread()])
            yield chunk
        if self.error is not None:
            raise self.error

    def close(self):
        self.events.append(["close", same_thread()])

    def release_conn(self):
        self.events.append(["release", same_thread()])


class ObservedReadRaw:
    def __init__(self, chunks=(), error=None):
        self.chunks = list(chunks)
        self.error = error
        self.events = []

    def __getattribute__(self, name):
        if name == "stream":
            object.__getattribute__(self, "events").append(
                ["getattr-missing", "stream", same_thread()]
            )
        return object.__getattribute__(self, name)

    def read(self, chunk_size):
        self.events.append(
            ["read", value_record(chunk_size), same_thread()]
        )
        if self.error is not None:
            raise self.error
        if self.chunks:
            return self.chunks.pop(0)
        return b""

    def close(self):
        self.events.append(["close", same_thread()])

    def release_conn(self):
        self.events.append(["release", same_thread()])
"""


def _run_matching(source: str):
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == ""
    return _normalize_literal(ast.literal_eval(oracle.observations["result"]["repr"]))


def _run_rewrite_only(source: str):
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert oracle.observations["result"]["repr"] == "{'target': 'oracle-control'}"
    assert oracle.stderr == ""
    assert rewrite.observations["exception"] is None
    assert rewrite.stderr == ""
    return _normalize_literal(ast.literal_eval(rewrite.observations["result"]["repr"]))


def _normalize_literal(value):
    if isinstance(value, (list, tuple)):
        return [_normalize_literal(item) for item in value]
    if isinstance(value, dict):
        return {key: _normalize_literal(item) for key, item in value.items()}
    return value


def test_response_snapshot_pins_public_content_shapes() -> None:
    state = _run_matching(
        """
states = []

initial = Response()
states.append(["streaming-unconsumed", response_fields_snapshot(initial)])

exhausted = Response()
exhausted._content_consumed = True
states.append(["exhausted-uncached", response_fields_snapshot(exhausted)])

cached = Response()
cached._content = b"cached"
cached._content_consumed = True
states.append(["fully-consumed-cached", response_fields_snapshot(cached)])

empty = Response()
empty._content = None
empty._content_consumed = True
states.append(["empty-none", response_fields_snapshot(empty)])

result = states
"""
    )

    by_name = {name: snapshot for name, snapshot in state}
    assert by_name["streaming-unconsumed"]["content_is_false"] is True
    assert by_name["streaming-unconsumed"]["content_consumed"] is False
    assert by_name["exhausted-uncached"]["content_is_false"] is True
    assert by_name["exhausted-uncached"]["content_consumed"] is True
    assert by_name["fully-consumed-cached"]["content"]["payload"] == [
        "bytes",
        b"cached".hex(),
    ]
    assert by_name["fully-consumed-cached"]["content_consumed"] is True
    assert by_name["empty-none"]["content"]["payload"] is None
    assert by_name["empty-none"]["content_consumed"] is True


def test_pristine_exact_candidates_avoid_authoritative_response_frames() -> None:
    state = _run_rewrite_only(
        """
if _requests_rust is None:
    result = {"target": "oracle-control"}
else:
    import sys

    targets = {
        Response.content.fget.__code__,
        Response.iter_content.__code__,
        Response.iter_lines.__code__,
        Response.text.fget.__code__,
        Response.apparent_encoding.fget.__code__,
        Response.json.__code__,
        Response.__repr__.__code__,
        Response.__bool__.__code__,
        Response.ok.fget.__code__,
        Response.is_redirect.fget.__code__,
        Response.is_permanent_redirect.fget.__code__,
        Response.next.fget.__code__,
        Response.raise_for_status.__code__,
        Response.__getstate__.__code__,
        Response.__setstate__.__code__,
        Response.close.__code__,
    }
    seen = []

    def profile(frame, event, argument):
        if event == "call" and frame.f_code in targets:
            seen.append(frame.f_code.co_qualname)

    sys.setprofile(profile)
    try:
        content_subject = Response()
        content_subject.status_code = 200
        content_subject.raw = ObservedReadRaw([b"content"])
        content = response_content_call(content_subject)

        iter_subject = Response()
        iter_subject.raw = ObservedReadRaw([b"ab", b"cd"])
        chunks = list(response_iter_content_call(iter_subject, 2))

        lines_subject = Response()
        lines_subject.raw = ObservedReadRaw([b"a\\nb", b"c\\n"])
        lines = list(response_iter_lines_call(lines_subject, 2))

        unicode_subject = Response()
        unicode_subject.raw = ObservedReadRaw([b"\\xe2", b"\\x82", b"\\xac"])
        unicode_subject.encoding = "utf-8"
        decoded = list(
            response_iter_content_call(unicode_subject, 1, True)
        )

        text_subject = Response()
        text_subject._content = b"text"
        text_subject._content_consumed = True
        text_subject.encoding = "ascii"
        text = response_text_call(text_subject)

        apparent_subject = Response()
        apparent_subject._content = b"plain ascii"
        apparent_subject._content_consumed = True
        apparent = response_apparent_encoding_call(apparent_subject)

        json_subject = Response()
        json_subject._content = b'{"value": 3}'
        json_subject._content_consumed = True
        json_value = response_json_call(json_subject, {})

        metadata_subject = Response()
        metadata_subject.status_code = 302
        metadata_subject.reason = "Found"
        metadata_subject.url = "https://example.test/"
        metadata_subject.headers["Location"] = "/next"
        metadata = [
            response_metadata_call(metadata_subject, operation)
            for operation in (
                "repr",
                "bool",
                "ok",
                "is_redirect",
                "is_permanent_redirect",
                "next",
                "raise_for_status",
            )
        ]

        pickle_subject = Response()
        pickle_subject._content = b"pickle"
        pickle_subject._content_consumed = True
        pickle_state = response_pickle_call(pickle_subject, "get")
        restored = Response.__new__(Response)
        response_pickle_call(restored, "set", pickle_state)

        close_subject = Response()
        close_subject._content_consumed = True
        response_close_call(close_subject)
    finally:
        sys.setprofile(None)

    assert seen == []
    result = {
        "target": "rewrite",
        "content": value_record(content),
        "chunks": [value_record(chunk) for chunk in chunks],
        "lines": [value_record(line) for line in lines],
        "decoded": decoded,
        "text": text,
        "apparent_type": value_record(apparent)["type"],
        "json": json_value,
        "metadata": [
            value_record(value)
            if not isinstance(value, (bool, str, type(None)))
            else value
            for value in metadata
        ],
        "pickle_keys": list(pickle_state),
        "restored_consumed": restored._content_consumed,
        "seen": seen,
    }
"""
    )

    assert state["target"] == "rewrite"
    assert state["content"]["payload"] == ["bytes", b"content".hex()]
    assert [chunk["payload"][1] for chunk in state["chunks"]] == [
        b"ab".hex(),
        b"cd".hex(),
    ]
    assert [line["payload"][1] for line in state["lines"]] == [
        b"a".hex(),
        b"bc".hex(),
    ]
    assert state["decoded"] == ["€"]
    assert state["text"] == "text"
    assert state["json"] == {"value": 3}
    assert state["pickle_keys"][0] == "_content"
    assert state["restored_consumed"] is True
    assert state["seen"] == []


def test_dynamic_candidates_execute_their_authoritative_python_frames() -> None:
    state = _run_matching(
        """
import sys


class DynamicResponse(Response):
    @property
    def content(self):
        return b"dynamic-content"

    def iter_content(self, chunk_size=1, decode_unicode=False):
        yield b"dynamic-iter"

    def iter_lines(
        self,
        chunk_size=512,
        decode_unicode=False,
        delimiter=None,
    ):
        yield b"dynamic-line"

    @property
    def text(self):
        return "dynamic-text"

    @property
    def apparent_encoding(self):
        return "dynamic-encoding"

    def json(self, **kwargs):
        return {"dynamic": kwargs["value"]}

    def __repr__(self):
        return "<dynamic-profile>"

    def __bool__(self):
        return False

    @property
    def ok(self):
        return "dynamic-ok"

    def raise_for_status(self):
        return "dynamic-raise"

    def __getstate__(self):
        return {"dynamic": self}

    def __setstate__(self, state):
        self.dynamic_state = state
        return self

    def close(self):
        return self


dynamic = DynamicResponse()
targets = {
    DynamicResponse.content.fget.__code__: "content",
    DynamicResponse.iter_content.__code__: "iter_content",
    DynamicResponse.iter_lines.__code__: "iter_lines",
    DynamicResponse.text.fget.__code__: "text",
    DynamicResponse.apparent_encoding.fget.__code__: "apparent_encoding",
    DynamicResponse.json.__code__: "json",
    DynamicResponse.__repr__.__code__: "__repr__",
    DynamicResponse.__bool__.__code__: "__bool__",
    DynamicResponse.ok.fget.__code__: "ok",
    DynamicResponse.raise_for_status.__code__: "raise_for_status",
    DynamicResponse.__getstate__.__code__: "__getstate__",
    DynamicResponse.__setstate__.__code__: "__setstate__",
    DynamicResponse.close.__code__: "close",
}
seen = []

def profile(frame, event, argument):
    if event == "call" and frame.f_code in targets:
        seen.append(targets[frame.f_code])

sys.setprofile(profile)
try:
    response_content_call(dynamic)
    list(response_iter_content_call(dynamic, 3))
    list(response_iter_lines_call(dynamic, 4))
    response_text_call(dynamic)
    response_apparent_encoding_call(dynamic)
    response_json_call(dynamic, {"value": 5})
    response_metadata_call(dynamic, "repr")
    response_metadata_call(dynamic, "bool")
    response_metadata_call(dynamic, "ok")
    response_metadata_call(dynamic, "raise_for_status")
    dynamic_state = response_pickle_call(dynamic, "get")
    response_pickle_call(dynamic, "set", dynamic_state)
    response_close_call(dynamic)
finally:
    sys.setprofile(None)

result = seen
"""
    )

    assert state == [
        "content",
        "iter_content",
        "iter_content",
        "iter_lines",
        "iter_lines",
        "text",
        "apparent_encoding",
        "json",
        "__repr__",
        "__bool__",
        "ok",
        "raise_for_status",
        "__getstate__",
        "__setstate__",
        "close",
    ]


def test_abstract_disposition_is_clean_eof_only_monotonic_and_exactly_once() -> None:
    state = _run_matching(
        """
scenarios = {
    "clean": ["clean-eof"],
    "clean-then-close": ["clean-eof", "close", "drop"],
    "partial-open": ["partial"],
    "partial-close": ["partial", "close", "clean-eof"],
    "partial-drop": ["partial", "drop", "clean-eof"],
    "read-error": ["read-error", "clean-eof"],
    "decode-error": ["decode-error", "clean-eof"],
    "protocol-error": ["protocol-error", "clean-eof"],
    "cancel": ["cancel", "clean-eof"],
    "action-disconnect": ["action-disconnect", "clean-eof"],
    "reply-disconnect": ["reply-disconnect", "clean-eof"],
    "python-exact-drop": ["python-exact", "drop"],
}
result = {
    name: response_disposition_call(events)
    for name, events in scenarios.items()
}
"""
    )

    assert state["clean"] == {
        "states": ["Open", "Reusable"],
        "final": "Reusable",
        "decision": "Reusable",
        "decision_count": 1,
        "native_lease": True,
        "implicit_python_callbacks": 0,
    }
    assert state["clean-then-close"]["states"] == [
        "Open",
        "Reusable",
        "Reusable",
        "Reusable",
    ]
    assert state["clean-then-close"]["decision_count"] == 1
    assert state["partial-open"]["states"] == ["Open", "Partial"]
    assert state["partial-open"]["decision"] is None
    assert state["partial-open"]["decision_count"] == 0
    for name in (
        "partial-close",
        "partial-drop",
        "read-error",
        "decode-error",
        "protocol-error",
        "cancel",
        "action-disconnect",
        "reply-disconnect",
    ):
        assert state[name]["final"] == "CloseDirty"
        assert state[name]["decision"] == "CloseDirty"
        assert state[name]["decision_count"] == 1
        assert state[name]["states"][-1] == "CloseDirty"
    assert state["python-exact-drop"] == {
        "states": ["Open", "PythonExact", "PythonExact"],
        "final": "PythonExact",
        "decision": None,
        "decision_count": 0,
        "native_lease": False,
        "implicit_python_callbacks": 0,
    }


def test_native_lifecycle_audit_keeps_origin_owner_until_worker_drop() -> None:
    state = _run_matching(
        """
class LifecycleRaw(ObservedReadRaw):
    def __init__(self, label, audit):
        super().__init__([b"x"])
        self.label = label
        self.audit = audit

    def __del__(self):
        side_effects.append([
            "raw-del",
            self.label,
            self.audit.get("worker_dropped"),
            same_thread(),
        ])


rows = []
for phase in (
    "before-poll",
    "queued-before-dequeue",
    "reply-observed",
):
    audit = {}
    raw = LifecycleRaw(phase, audit)
    subject = Response()
    subject.raw = raw
    del raw
    original = BaseException(phase)
    outcome = capture(
        lambda: response_lifecycle_call(
            subject, original, phase, audit
        )
    )
    rows.append({
        "phase": phase,
        "record": outcome["exception"],
        "is_original": outcome["error"] is original,
        "audit": dict(audit),
        "events": list(subject.raw.events),
    })
    original.__traceback__ = None
    del outcome
    del subject
    gc.collect()
    side_effects.append(["after-subject-drop", phase])

result = {"rows": rows, "side_effects": side_effects}
"""
    )

    rows = {row["phase"]: row for row in state["rows"]}
    assert rows["before-poll"]["audit"] == {
        "queued": 0,
        "execute": 0,
        "reply_observed": False,
        "worker_dropped": True,
        "raw_actions": 0,
    }
    assert rows["before-poll"]["events"] == []
    assert rows["queued-before-dequeue"]["audit"] == {
        "queued": 1,
        "execute": 0,
        "reply_observed": False,
        "worker_dropped": True,
        "raw_actions": 0,
    }
    assert rows["queued-before-dequeue"]["events"] == []
    assert rows["reply-observed"]["audit"] == {
        "queued": 1,
        "execute": 1,
        "reply_observed": True,
        "worker_dropped": True,
        "raw_actions": 1,
    }
    assert rows["reply-observed"]["events"] == [
        ["getattr-missing", "stream", True],
        ["read", {"type": ["builtins", "int"], "payload": 1}, True],
    ]
    assert all(
        row["record"]["type"] == ["builtins", "BaseException"] for row in rows.values()
    )
    assert all(row["is_original"] for row in rows.values())
    assert state["side_effects"] == [
        ["raw-del", "before-poll", True, True],
        ["after-subject-drop", "before-poll"],
        ["raw-del", "queued-before-dequeue", True, True],
        ["after-subject-drop", "queued-before-dequeue"],
        ["raw-del", "reply-observed", True, True],
        ["after-subject-drop", "reply-observed"],
    ]


def test_content_is_lazy_then_caches_or_selects_empty_none() -> None:
    state = _run_matching(
        """
raw = ObservedStreamRaw([b"ab", b"", b"cd"])
subject = Response()
subject.status_code = 200
subject.raw = raw
before = {
    "events": list(raw.events),
    "state": response_fields_snapshot(subject),
}
first = response_content_call(subject)
after_first = {
    "value": value_record(first),
    "is_cached": first is subject._content,
    "events": list(raw.events),
    "state": response_fields_snapshot(subject),
}
second = response_content_call(subject)
after_second = {
    "same_object": second is first,
    "events": list(raw.events),
    "state": response_fields_snapshot(subject),
}

status_zero_raw = ObservedStreamRaw([b"must-not-read"])
status_zero = Response()
status_zero.status_code = 0
status_zero.raw = status_zero_raw
status_zero_value = response_content_call(status_zero)

no_raw = Response()
no_raw.status_code = 200
no_raw_value = response_content_call(no_raw)

result = {
    "before": before,
    "after_first": after_first,
    "after_second": after_second,
    "status_zero": {
        "value": value_record(status_zero_value),
        "events": status_zero_raw.events,
        "state": response_fields_snapshot(status_zero),
    },
    "no_raw": {
        "value": value_record(no_raw_value),
        "state": response_fields_snapshot(no_raw),
    },
}
"""
    )

    assert state["before"]["events"] == []
    assert state["before"]["state"]["content_is_false"] is True
    assert state["before"]["state"]["content_consumed"] is False
    assert state["after_first"]["value"]["payload"] == [
        "bytes",
        b"abcd".hex(),
    ]
    assert state["after_first"]["is_cached"] is True
    assert state["after_first"]["state"]["content_consumed"] is True
    assert state["after_first"]["events"][:3] == [
        ["getattr", "stream", True],
        ["getattr", "stream", True],
        ["stream", {"type": ["builtins", "int"], "payload": 10240}, True, True],
    ]
    assert state["after_second"]["same_object"] is True
    assert state["after_second"]["events"] == state["after_first"]["events"]
    assert state["status_zero"]["value"]["payload"] is None
    assert state["status_zero"]["events"] == []
    assert state["status_zero"]["state"]["content_consumed"] is True
    assert state["no_raw"]["value"]["payload"] is None
    assert state["no_raw"]["state"]["content_consumed"] is True


def test_content_dynamic_subclass_and_failed_read_preserve_origin_identity() -> None:
    state = _run_matching(
        """
class DynamicContent(Response):
    @property
    def content(self):
        side_effects.append(["dynamic-content", same_thread()])
        return self


dynamic = DynamicContent()
dynamic_value = response_content_call(dynamic)

read_error = BaseException("content read failed")
failed_raw = ObservedReadRaw(error=read_error)
failed = Response()
failed.status_code = 200
failed.raw = failed_raw
failure = capture(lambda: response_content_call(failed))

result = {
    "dynamic_identity": dynamic_value is dynamic,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is read_error,
        "context_is_none": failure["error"].__context__ is None,
        "events": failed_raw.events,
        "state": response_fields_snapshot(failed),
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic_identity"] is True
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["failure"]["context_is_none"] is True
    assert state["failure"]["events"] == [
        ["getattr-missing", "stream", True],
        ["read", {"type": ["builtins", "int"], "payload": 10240}, True],
    ]
    assert state["failure"]["state"]["content_is_false"] is True
    assert state["failure"]["state"]["content_consumed"] is False
    assert state["side_effects"] == [["dynamic-content", True]]


def test_iter_content_partial_exhausted_cached_reuse_and_consumed_error() -> None:
    state = _run_matching(
        """
partial_raw = ObservedStreamRaw([b"ab", b"cd"])
partial = Response()
partial.status_code = 200
partial.raw = partial_raw
partial_iterator = response_iter_content_call(partial, 2)
partial_before_next = {
    "events": list(partial_raw.events),
    "state": response_fields_snapshot(partial),
}
partial_first = next(partial_iterator)
partial_after_next = {
    "value": value_record(partial_first),
    "events": list(partial_raw.events),
    "state": response_fields_snapshot(partial),
}
del partial_iterator
gc.collect()
partial_after_drop = {
    "events": list(partial_raw.events),
    "state": response_fields_snapshot(partial),
}

exhausted_raw = ObservedStreamRaw([b"ab", b"cd"])
exhausted = Response()
exhausted.status_code = 200
exhausted.raw = exhausted_raw
exhausted_values = [
    value_record(value)
    for value in response_iter_content_call(exhausted, 2)
]
exhausted_state = response_fields_snapshot(exhausted)
consumed_error = capture(
    lambda: response_iter_content_call(exhausted, "bad")
)

cached = Response()
cached._content = b"abcde"
cached._content_consumed = True
cached_two = [
    value_record(value)
    for value in response_iter_content_call(cached, 2)
]
cached_three = [
    value_record(value)
    for value in response_iter_content_call(cached, 3)
]
cached_none = [
    value_record(value)
    for value in response_iter_content_call(cached, None)
]
cached_bool = [
    value_record(value)
    for value in response_iter_content_call(cached, True)
]
cached_zero = [
    value_record(value)
    for value in response_iter_content_call(cached, 0)
]
cached_negative = [
    value_record(value)
    for value in response_iter_content_call(cached, -2)
]

invalid = Response()
invalid.raw = ObservedReadRaw([b"x"])
invalid_chunk = capture(
    lambda: response_iter_content_call(invalid, "1024")
)

result = {
    "partial_before_next": partial_before_next,
    "partial_after_next": partial_after_next,
    "partial_after_drop": partial_after_drop,
    "exhausted_values": exhausted_values,
    "exhausted_state": exhausted_state,
    "consumed_error": consumed_error["exception"],
    "cached_two": cached_two,
    "cached_three": cached_three,
    "cached_none": cached_none,
    "cached_bool": cached_bool,
    "cached_zero": cached_zero,
    "cached_negative": cached_negative,
    "invalid_chunk": invalid_chunk["exception"],
}
"""
    )

    assert state["partial_before_next"]["events"] == []
    assert state["partial_before_next"]["state"]["content_consumed"] is False
    assert state["partial_after_next"]["value"]["payload"] == [
        "bytes",
        b"ab".hex(),
    ]
    assert state["partial_after_next"]["state"]["content_is_false"] is True
    assert state["partial_after_next"]["state"]["content_consumed"] is False
    assert (
        state["partial_after_drop"]["events"] == state["partial_after_next"]["events"]
    )
    assert state["partial_after_drop"]["state"] == state["partial_after_next"]["state"]
    assert [item["payload"] for item in state["exhausted_values"]] == [
        ["bytes", b"ab".hex()],
        ["bytes", b"cd".hex()],
    ]
    assert state["exhausted_state"]["content_is_false"] is True
    assert state["exhausted_state"]["content_consumed"] is True
    assert state["consumed_error"]["type"] == [
        "requests.exceptions",
        "StreamConsumedError",
    ]
    assert state["consumed_error"]["args"] == []
    assert [item["payload"][1] for item in state["cached_two"]] == [
        b"ab".hex(),
        b"cd".hex(),
        b"e".hex(),
    ]
    assert [item["payload"][1] for item in state["cached_three"]] == [
        b"abc".hex(),
        b"de".hex(),
    ]
    assert [item["payload"][1] for item in state["cached_none"]] == [b"abcde".hex()]
    assert [item["payload"][1] for item in state["cached_bool"]] == [
        b"a".hex(),
        b"b".hex(),
        b"c".hex(),
        b"d".hex(),
        b"e".hex(),
    ]
    assert [item["payload"][1] for item in state["cached_zero"]] == [b"abcde".hex()]
    assert [item["payload"][1] for item in state["cached_negative"]] == [b"abcde".hex()]
    assert state["invalid_chunk"]["type"] == ["builtins", "TypeError"]
    assert state["invalid_chunk"]["args"] == [
        "chunk_size must be an int, it is instead a <class 'str'>."
    ]


def test_iter_content_stream_and_read_protocol_timing_and_exact_errors() -> None:
    state = _run_matching(
        """
stream_raw = ObservedStreamRaw([b"bytes", "text", bytearray(b"mutable")])
stream_subject = Response()
stream_subject.raw = stream_raw
stream_iterator = response_iter_content_call(stream_subject, None)
stream_before = list(stream_raw.events)
stream_values = [value_record(value) for value in stream_iterator]

read_raw = ObservedReadRaw([b"ab", b"cd"])
read_subject = Response()
read_subject.raw = read_raw
read_iterator = response_iter_content_call(read_subject, 2)
read_before = list(read_raw.events)
read_first = next(read_iterator)
read_rest = list(read_iterator)

read_error = BaseException("read failed")
failed_raw = ObservedReadRaw(error=read_error)
failed_subject = Response()
failed_subject.raw = failed_raw
failed = capture(
    lambda: next(response_iter_content_call(failed_subject, 7))
)

result = {
    "stream_before": stream_before,
    "stream_values": stream_values,
    "stream_events": stream_raw.events,
    "stream_state": response_fields_snapshot(stream_subject),
    "read_before": read_before,
    "read_first": value_record(read_first),
    "read_rest": [value_record(value) for value in read_rest],
    "read_events": read_raw.events,
    "read_state": response_fields_snapshot(read_subject),
    "failure": {
        "record": failed["exception"],
        "is_original": failed["error"] is read_error,
        "context_is_none": failed["error"].__context__ is None,
        "events": failed_raw.events,
        "state": response_fields_snapshot(failed_subject),
    },
}
"""
    )

    assert state["stream_before"] == []
    assert [item["type"] for item in state["stream_values"]] == [
        ["builtins", "bytes"],
        ["builtins", "str"],
        ["builtins", "bytearray"],
    ]
    assert state["stream_events"][:3] == [
        ["getattr", "stream", True],
        ["getattr", "stream", True],
        [
            "stream",
            {"type": ["builtins", "NoneType"], "payload": None},
            True,
            True,
        ],
    ]
    assert state["stream_state"]["content_consumed"] is True
    assert state["read_before"] == []
    assert state["read_first"]["payload"] == ["bytes", b"ab".hex()]
    assert [item["payload"] for item in state["read_rest"]] == [["bytes", b"cd".hex()]]
    assert state["read_events"] == [
        ["getattr-missing", "stream", True],
        ["read", {"type": ["builtins", "int"], "payload": 2}, True],
        ["read", {"type": ["builtins", "int"], "payload": 2}, True],
        ["read", {"type": ["builtins", "int"], "payload": 2}, True],
    ]
    assert state["read_state"]["content_consumed"] is True
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["failure"]["context_is_none"] is True
    assert state["failure"]["state"]["content_consumed"] is False


def test_iter_content_generator_close_reaches_stream_iterator_not_raw_close() -> None:
    state = _run_matching(
        """
class CloseObservedRaw:
    def __init__(self):
        self.events = []

    def stream(self, chunk_size, decode_content=True):
        self.events.append([
            "stream",
            chunk_size,
            decode_content,
            same_thread(),
        ])
        try:
            yield b"first"
            yield b"second"
        finally:
            self.events.append(["generator-close", same_thread()])

    def close(self):
        self.events.append(["raw-close", same_thread()])

    def release_conn(self):
        self.events.append(["release", same_thread()])


before_start_raw = CloseObservedRaw()
before_start = Response()
before_start.raw = before_start_raw
before_start_iterator = response_iter_content_call(before_start, 5)
before_start_iterator.close()

partial_raw = CloseObservedRaw()
partial = Response()
partial.raw = partial_raw
partial_iterator = response_iter_content_call(partial, 5)
first = next(partial_iterator)
partial_iterator.close()

clean_raw = CloseObservedRaw()
clean = Response()
clean.raw = clean_raw
clean_values = list(response_iter_content_call(clean, 5))

result = {
    "before_start": {
        "events": before_start_raw.events,
        "state": response_fields_snapshot(before_start),
    },
    "partial": {
        "first": value_record(first),
        "events": partial_raw.events,
        "state": response_fields_snapshot(partial),
    },
    "clean": {
        "values": [value_record(value) for value in clean_values],
        "events": clean_raw.events,
        "state": response_fields_snapshot(clean),
    },
}
"""
    )

    assert state["before_start"]["events"] == []
    assert state["before_start"]["state"]["content_consumed"] is False
    assert state["partial"]["first"]["payload"] == ["bytes", b"first".hex()]
    assert state["partial"]["events"] == [
        ["stream", 5, True, True],
        ["generator-close", True],
    ]
    assert state["partial"]["state"]["content_is_false"] is True
    assert state["partial"]["state"]["content_consumed"] is False
    assert [item["payload"][1] for item in state["clean"]["values"]] == [
        b"first".hex(),
        b"second".hex(),
    ]
    assert state["clean"]["events"] == [
        ["stream", 5, True, True],
        ["generator-close", True],
    ]
    assert state["clean"]["state"]["content_is_false"] is True
    assert state["clean"]["state"]["content_consumed"] is True


def test_iter_content_wraps_urllib3_errors_with_original_context() -> None:
    state = _run_matching(
        """
from urllib3.exceptions import (
    DecodeError,
    ProtocolError,
    ReadTimeoutError,
    SSLError,
)


errors = [
    ("protocol", ProtocolError("protocol failed")),
    ("decode", DecodeError("decode failed")),
    ("timeout", ReadTimeoutError(None, "/resource", "timed out")),
    ("ssl", SSLError("ssl failed")),
]
rows = []
for label, original in errors:
    raw = ObservedStreamRaw(error=original)
    subject = Response()
    subject.raw = raw
    caught = capture(
        lambda subject=subject: next(
            response_iter_content_call(subject, 1024)
        )
    )
    error = caught["error"]
    rows.append({
        "label": label,
        "record": caught["exception"],
        "argument_is_original": error.args[0] is original,
        "context_is_original": error.__context__ is original,
        "cause_is_none": error.__cause__ is None,
        "suppress_context": error.__suppress_context__,
        "state": response_fields_snapshot(subject),
    })
result = rows
"""
    )

    assert [row["record"]["type"] for row in state] == [
        ["requests.exceptions", "ChunkedEncodingError"],
        ["requests.exceptions", "ContentDecodingError"],
        ["requests.exceptions", "ConnectionError"],
        ["requests.exceptions", "SSLError"],
    ]
    assert all(row["argument_is_original"] for row in state)
    assert all(row["context_is_original"] for row in state)
    assert all(row["cause_is_none"] for row in state)
    assert not any(row["suppress_context"] for row in state)
    assert not any(row["state"]["content_consumed"] for row in state)


def test_iter_content_dynamic_subclasses_and_rebound_globals_use_origin() -> None:
    state = _run_matching(
        """
import requests.models as models


class DynamicResponse(Response):
    def iter_content(self, chunk_size=1, decode_unicode=False):
        side_effects.append([
            "dynamic-iter-content",
            value_record(chunk_size),
            decode_unicode,
            same_thread(),
        ])
        yield "dynamic"


dynamic = DynamicResponse()
dynamic_iterator = response_iter_content_call(dynamic, 9, True)
dynamic_before = list(side_effects)
dynamic_values = list(dynamic_iterator)

original_iter_slices = models.iter_slices
def rebound_iter_slices(content, chunk_size):
    side_effects.append([
        "rebound-iter-slices",
        value_record(content),
        value_record(chunk_size),
        same_thread(),
    ])
    return iter((b"rebound",))

models.iter_slices = rebound_iter_slices
try:
    cached = Response()
    cached._content = b"cached"
    cached._content_consumed = True
    rebound_values = list(response_iter_content_call(cached, 3))
finally:
    models.iter_slices = original_iter_slices

original_decoder = models.stream_decode_response_unicode
def rebound_decoder(chunks, subject):
    side_effects.append([
        "rebound-decoder",
        subject is decoded,
        same_thread(),
    ])
    list(chunks)
    yield "decoded"

models.stream_decode_response_unicode = rebound_decoder
try:
    decoded = Response()
    decoded.raw = ObservedStreamRaw([b"ignored"])
    decoded.encoding = "ascii"
    decoded_values = list(
        response_iter_content_call(decoded, 4, True)
    )
finally:
    models.stream_decode_response_unicode = original_decoder

original_error = BaseException("dynamic failure")
class FailingResponse(Response):
    def iter_content(self, chunk_size=1, decode_unicode=False):
        side_effects.append(["dynamic-error", same_thread()])
        raise original_error


failure = capture(
    lambda: response_iter_content_call(FailingResponse(), 1)
)
result = {
    "dynamic_before": dynamic_before,
    "dynamic_values": dynamic_values,
    "rebound_values": [value_record(value) for value in rebound_values],
    "decoded_values": decoded_values,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is original_error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic_before"] == []
    assert state["dynamic_values"] == ["dynamic"]
    assert [item["payload"] for item in state["rebound_values"]] == [
        ["bytes", b"rebound".hex()]
    ]
    assert state["decoded_values"] == ["decoded"]
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        [
            "dynamic-iter-content",
            {"type": ["builtins", "int"], "payload": 9},
            True,
            True,
        ],
        [
            "rebound-iter-slices",
            {"type": ["builtins", "bytes"], "payload": ["bytes", b"cached".hex()]},
            {"type": ["builtins", "int"], "payload": 3},
            True,
        ],
        ["rebound-decoder", True, True],
        ["dynamic-error", True],
    ]


def test_iter_lines_delimiters_pending_and_errors() -> None:
    state = _run_matching(
        """
def lines(chunks, delimiter=None, encoding=None):
    raw = ObservedStreamRaw(chunks)
    subject = Response()
    subject.raw = raw
    subject.encoding = encoding
    values = list(
        response_iter_lines_call(
            subject,
            chunk_size=3,
            decode_unicode=encoding is not None,
            delimiter=delimiter,
        )
    )
    return {
        "values": [value_record(value) for value in values],
        "events": raw.events,
        "state": response_fields_snapshot(subject),
    }


splitlines = lines([b"a\\nb", b"c\\n", b"d"])
explicit = lines([b"a--b-", b"-c--"], delimiter=b"--")
carriage = lines([b"a\\r", b"\\nb\\r", b"c"])
decoded = lines(
    ["a::b:".encode(), ":c::".encode()],
    delimiter="::",
    encoding="utf-8",
)
empty_delimiter = lines([b"a\\nb"], delimiter=b"")

mismatch_subject = Response()
mismatch_subject.raw = ObservedStreamRaw([b"a\\nb"])
mismatch = capture(
    lambda: next(
        response_iter_lines_call(
            mismatch_subject,
            chunk_size=3,
            delimiter="\\n",
        )
    )
)
result = {
    "splitlines": splitlines,
    "explicit": explicit,
    "carriage": carriage,
    "decoded": decoded,
    "empty_delimiter": empty_delimiter,
    "mismatch": {
        "record": mismatch["exception"],
        "state": response_fields_snapshot(mismatch_subject),
    },
}
"""
    )

    assert [item["payload"][1] for item in state["splitlines"]["values"]] == [
        b"a".hex(),
        b"bc".hex(),
        b"d".hex(),
    ]
    assert [item["payload"][1] for item in state["explicit"]["values"]] == [
        b"a".hex(),
        b"b".hex(),
        b"c".hex(),
        b"".hex(),
    ]
    assert [item["payload"][1] for item in state["carriage"]["values"]] == [
        b"a".hex(),
        b"".hex(),
        b"b".hex(),
        b"c".hex(),
    ]
    assert [item["payload"] for item in state["decoded"]["values"]] == [
        ["str", "a"],
        ["str", "b"],
        ["str", "c"],
        ["str", ""],
    ]
    assert [item["payload"][1] for item in state["empty_delimiter"]["values"]] == [
        b"a".hex(),
        b"b".hex(),
    ]
    assert all(
        state[name]["state"]["content_consumed"]
        for name in (
            "splitlines",
            "explicit",
            "carriage",
            "decoded",
            "empty_delimiter",
        )
    )
    assert state["mismatch"]["record"]["type"] == ["builtins", "TypeError"]
    assert state["mismatch"]["state"]["content_consumed"] is False


def test_iter_lines_is_lazy_and_uses_dynamic_iter_content() -> None:
    state = _run_matching(
        """
import requests.models as models


class DynamicLines(Response):
    def iter_content(self, chunk_size=1, decode_unicode=False):
        side_effects.append([
            "dynamic-content",
            chunk_size,
            decode_unicode,
            same_thread(),
        ])
        yield b"a\\nb"
        yield b"c\\n"


dynamic = DynamicLines()
iterator = response_iter_lines_call(dynamic, 5, False, None)
before = list(side_effects)
first = next(iterator)
rest = list(iterator)

original_iter_content = Response.iter_content
def rebound_iter_content(self, chunk_size=1, decode_unicode=False):
    side_effects.append([
        "rebound-content",
        self is rebound,
        chunk_size,
        decode_unicode,
        same_thread(),
    ])
    yield b"x\\ny"

Response.iter_content = rebound_iter_content
try:
    rebound = Response()
    rebound_values = list(
        response_iter_lines_call(rebound, 6, False, None)
    )
finally:
    Response.iter_content = original_iter_content

failure_error = BaseException("line source failed")
class FailingLines(Response):
    def iter_content(self, chunk_size=1, decode_unicode=False):
        side_effects.append(["line-error", same_thread()])
        raise failure_error
        yield


failure = capture(
    lambda: next(response_iter_lines_call(FailingLines()))
)
result = {
    "before": before,
    "first": value_record(first),
    "rest": [value_record(value) for value in rest],
    "rebound": [value_record(value) for value in rebound_values],
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is failure_error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["before"] == []
    assert state["first"]["payload"] == ["bytes", b"a".hex()]
    assert [item["payload"][1] for item in state["rest"]] == [
        b"bc".hex(),
    ]
    assert [item["payload"][1] for item in state["rebound"]] == [
        b"x".hex(),
        b"y".hex(),
    ]
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        ["dynamic-content", 5, False, True],
        ["rebound-content", True, 6, False, True],
        ["line-error", True],
    ]


def test_iter_content_incrementally_decodes_unicode_and_flushes_tail() -> None:
    state = _run_matching(
        """
def decode(chunks, encoding):
    raw = ObservedStreamRaw(chunks)
    subject = Response()
    subject.raw = raw
    subject.encoding = encoding
    captured = capture(
        lambda: list(
            response_iter_content_call(subject, 1, True)
        )
    )
    return {
        "values": None
        if captured["error"] is not None
        else [value_record(value) for value in captured["returned"]],
        "exception": captured["exception"],
        "events": raw.events,
        "state": response_fields_snapshot(subject),
    }


euro = decode([b"\\xe2", b"\\x82", b"\\xac", b"X"], "utf-8")
truncated = decode([b"\\xe2", b"\\x82"], "utf-8")
unset = decode([b"a", b"b"], None)
invalid = decode([b"a"], "not-a-codec")
result = {
    "euro": euro,
    "truncated": truncated,
    "unset": unset,
    "invalid": invalid,
}
"""
    )

    assert [item["payload"] for item in state["euro"]["values"]] == [
        ["str", "€"],
        ["str", "X"],
    ]
    assert state["euro"]["state"]["content_consumed"] is True
    assert [item["payload"] for item in state["truncated"]["values"]] == [["str", "�"]]
    assert state["truncated"]["state"]["content_consumed"] is True
    assert [item["payload"] for item in state["unset"]["values"]] == [
        ["bytes", b"a".hex()],
        ["bytes", b"b".hex()],
    ]
    assert state["invalid"]["exception"] == {
        "type": ["builtins", "LookupError"],
        "args": ["unknown encoding: not-a-codec"],
    }
    assert state["invalid"]["events"] == []
    assert state["invalid"]["state"]["content_consumed"] is False


def test_text_empty_explicit_detected_and_invalid_encoding_paths() -> None:
    state = _run_matching(
        """
import requests.models as models


def cached(content, encoding):
    subject = Response()
    subject._content = content
    subject._content_consumed = True
    subject.encoding = encoding
    return subject


class Detector:
    def __init__(self):
        self.calls = []

    def detect(self, content):
        self.calls.append([
            value_record(content),
            same_thread(),
        ])
        return {"encoding": "utf-8"}


empty = response_text_call(cached(b"", None))
explicit = response_text_call(cached("café".encode("utf-8"), "utf-8"))
replacement = response_text_call(cached(b"a\\xffb", "ascii"))
invalid = response_text_call(cached("café".encode("utf-8"), "not-a-codec"))

detector = Detector()
original_detector = models.chardet
models.chardet = detector
try:
    detected_subject = cached("snowman ☃".encode("utf-8"), None)
    detected = response_text_call(detected_subject)
finally:
    models.chardet = original_detector

result = {
    "empty": empty,
    "explicit": explicit,
    "replacement": replacement,
    "invalid": invalid,
    "detected": detected,
    "detector_calls": detector.calls,
}
"""
    )

    assert state == {
        "empty": "",
        "explicit": "café",
        "replacement": "a�b",
        "invalid": "café",
        "detected": "snowman ☃",
        "detector_calls": [
            [
                {
                    "type": ["builtins", "bytes"],
                    "payload": ["bytes", "snowman ☃".encode().hex()],
                },
                True,
            ]
        ],
    }


def test_text_dynamic_content_and_apparent_encoding_access_order() -> None:
    state = _run_matching(
        """
class DynamicText(Response):
    def __init__(self):
        super().__init__()
        self.encoding = None
        self.content_calls = 0

    @property
    def content(self):
        self.content_calls += 1
        side_effects.append([
            "content",
            self.content_calls,
            same_thread(),
        ])
        return b"dynamic"

    @property
    def apparent_encoding(self):
        side_effects.append(["apparent", same_thread()])
        return "ascii"


dynamic = DynamicText()
text = response_text_call(dynamic)

error = BaseException("content failed")
class FailingText(Response):
    @property
    def content(self):
        side_effects.append(["content-error", same_thread()])
        raise error


failure = capture(lambda: response_text_call(FailingText()))
result = {
    "text": text,
    "content_calls": dynamic.content_calls,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["text"] == "dynamic"
    assert state["content_calls"] == 2
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        ["content", 1, True],
        ["apparent", True],
        ["content", 2, True],
        ["content-error", True],
    ]


def test_apparent_encoding_uses_selected_detector_and_preserves_errors() -> None:
    state = _run_matching(
        """
import requests.models as models


marker = object()
class SelectedDetector:
    def __init__(self, error=None):
        self.error = error
        self.calls = []

    def detect(self, content):
        self.calls.append([
            value_record(content),
            same_thread(),
        ])
        if self.error is not None:
            raise self.error
        return {"encoding": marker}


subject = Response()
subject._content = b"payload"
subject._content_consumed = True
selected = SelectedDetector()
original_detector = models.chardet
models.chardet = selected
try:
    returned = response_apparent_encoding_call(subject)
finally:
    models.chardet = original_detector

no_detector_raw = ObservedStreamRaw([b"must-not-read"])
no_detector = Response()
no_detector.status_code = 200
no_detector.raw = no_detector_raw
models.chardet = None
try:
    fallback = response_apparent_encoding_call(no_detector)
finally:
    models.chardet = original_detector

detector_error = BaseException("detect failed")
failing = SelectedDetector(detector_error)
models.chardet = failing
try:
    failed = capture(
        lambda: response_apparent_encoding_call(subject)
    )
finally:
    models.chardet = original_detector

class DynamicEncoding(Response):
    @property
    def apparent_encoding(self):
        side_effects.append(["dynamic-apparent", same_thread()])
        return "dynamic"


dynamic = response_apparent_encoding_call(DynamicEncoding())
result = {
    "returned_is_marker": returned is marker,
    "calls": selected.calls,
    "fallback": fallback,
    "fallback_events": no_detector_raw.events,
    "fallback_state": response_fields_snapshot(no_detector),
    "failure": {
        "record": failed["exception"],
        "is_original": failed["error"] is detector_error,
    },
    "dynamic": dynamic,
    "side_effects": side_effects,
}
"""
    )

    assert state["returned_is_marker"] is True
    assert state["calls"] == [
        [
            {
                "type": ["builtins", "bytes"],
                "payload": ["bytes", b"payload".hex()],
            },
            True,
        ]
    ]
    assert state["fallback"] == "utf-8"
    assert state["fallback_events"] == []
    assert state["fallback_state"]["content_consumed"] is False
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["dynamic"] == "dynamic"
    assert state["side_effects"] == [["dynamic-apparent", True]]


def test_json_selected_module_bom_kwargs_and_error_context() -> None:
    state = _run_matching(
        """
import requests.models as models


class SelectedJson:
    def __init__(self, returned=None, error=None):
        self.returned = returned
        self.error = error
        self.calls = []

    def loads(self, text, **kwargs):
        self.calls.append([
            value_record(text),
            sorted(kwargs.items()),
            same_thread(),
        ])
        if self.error is not None:
            raise self.error
        return self.returned


def cached(content, encoding=None):
    subject = Response()
    subject._content = content
    subject._content_consumed = True
    subject.encoding = encoding
    return subject


marker = object()
selected = SelectedJson(marker)
original_json = models.complexjson
models.complexjson = selected
try:
    utf16 = response_json_call(
        cached('{"snowman": "☃"}'.encode("utf-16")),
        {"parse_int": "marker"},
    )
finally:
    models.complexjson = original_json

json_error = models.JSONDecodeError("bad json", "document", 2)
failing_json = SelectedJson(error=json_error)
models.complexjson = failing_json
try:
    wrapped = capture(
        lambda: response_json_call(
            cached(b"document", "utf-8"),
            {"strict": False},
        )
    )
finally:
    models.complexjson = original_json
wrapped_error = wrapped["error"]

other_error = BaseException("selected json failed")
other_json = SelectedJson(error=other_error)
models.complexjson = other_json
try:
    passthrough = capture(
        lambda: response_json_call(cached(b"other", "utf-8"), {})
    )
finally:
    models.complexjson = original_json

result = {
    "utf16_is_marker": utf16 is marker,
    "utf16_calls": selected.calls,
    "wrapped": {
        "record": wrapped["exception"],
        "is_original": wrapped_error is json_error,
        "context_is_original": wrapped_error.__context__ is json_error,
        "cause_is_none": wrapped_error.__cause__ is None,
        "suppress_context": wrapped_error.__suppress_context__,
        "response_is_none": wrapped_error.response is None,
        "request_is_none": wrapped_error.request is None,
        "msg": wrapped_error.msg,
        "doc": wrapped_error.doc,
        "pos": wrapped_error.pos,
        "calls": failing_json.calls,
    },
    "passthrough": {
        "record": passthrough["exception"],
        "is_original": passthrough["error"] is other_error,
        "context_is_none": passthrough["error"].__context__ is None,
        "calls": other_json.calls,
    },
}
"""
    )

    assert state["utf16_is_marker"] is True
    assert state["utf16_calls"] == [
        [
            {
                "type": ["builtins", "str"],
                "payload": ["str", '{"snowman": "☃"}'],
            },
            [["parse_int", "marker"]],
            True,
        ]
    ]
    assert state["wrapped"]["record"]["type"] == [
        "requests.exceptions",
        "JSONDecodeError",
    ]
    assert state["wrapped"]["is_original"] is False
    assert state["wrapped"]["context_is_original"] is True
    assert state["wrapped"]["cause_is_none"] is True
    assert state["wrapped"]["suppress_context"] is False
    assert state["wrapped"]["response_is_none"] is True
    assert state["wrapped"]["request_is_none"] is True
    assert state["wrapped"]["msg"] == "bad json"
    assert state["wrapped"]["doc"] == "document"
    assert state["wrapped"]["pos"] == 2
    assert state["wrapped"]["calls"] == [
        [
            {
                "type": ["builtins", "str"],
                "payload": ["str", "document"],
            },
            [["strict", False]],
            True,
        ]
    ]
    assert state["passthrough"]["record"]["type"] == [
        "builtins",
        "BaseException",
    ]
    assert state["passthrough"]["is_original"] is True
    assert state["passthrough"]["context_is_none"] is True
    assert state["passthrough"]["calls"] == [
        [
            {"type": ["builtins", "str"], "payload": ["str", "other"]},
            [],
            True,
        ]
    ]


def test_json_dynamic_subclass_and_rebound_guess_helper_fall_back() -> None:
    state = _run_matching(
        """
import requests.models as models


class DynamicJson(Response):
    def json(self, **kwargs):
        side_effects.append([
            "dynamic-json",
            sorted(kwargs.items()),
            same_thread(),
        ])
        return self


dynamic = DynamicJson()
dynamic_returned = response_json_call(dynamic, {"answer": 42})

class SelectedJson:
    def __init__(self):
        self.calls = []

    def loads(self, text, **kwargs):
        self.calls.append([text, sorted(kwargs.items()), same_thread()])
        return "loaded"


selected = SelectedJson()
original_json = models.complexjson
original_guess = models.guess_json_utf
def rebound_guess(content):
    side_effects.append([
        "rebound-guess",
        value_record(content),
        same_thread(),
    ])
    return "utf-8"

models.complexjson = selected
models.guess_json_utf = rebound_guess
try:
    subject = Response()
    subject._content = b'{"x": 1}'
    subject._content_consumed = True
    loaded = response_json_call(subject, {})
finally:
    models.complexjson = original_json
    models.guess_json_utf = original_guess

error = BaseException("dynamic json failed")
class FailingJson(Response):
    def json(self, **kwargs):
        side_effects.append(["dynamic-json-error", same_thread()])
        raise error


failure = capture(
    lambda: response_json_call(FailingJson(), {})
)
result = {
    "dynamic_identity": dynamic_returned is dynamic,
    "loaded": loaded,
    "selected_calls": selected.calls,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic_identity"] is True
    assert state["loaded"] == "loaded"
    assert state["selected_calls"] == [['{"x": 1}', [], True]]
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        ["dynamic-json", [["answer", 42]], True],
        [
            "rebound-guess",
            {
                "type": ["builtins", "bytes"],
                "payload": ["bytes", b'{"x": 1}'.hex()],
            },
            True,
        ],
        ["dynamic-json-error", True],
    ]


def test_status_redirect_next_history_repr_bool_ok_and_raise() -> None:
    state = _run_matching(
        """
from requests.models import PreparedRequest


def status_row(status, reason, location=None):
    subject = Response()
    subject.status_code = status
    subject.reason = reason
    subject.url = "https://example.test/resource"
    request = PreparedRequest()
    subject.request = request
    next_request = PreparedRequest()
    subject._next = next_request
    first = Response()
    second = Response()
    history = [first, second]
    subject.history = history
    if location is not None:
        subject.headers["Location"] = location

    raised = capture(
        lambda: response_metadata_call(subject, "raise_for_status")
    )
    error = raised["error"]
    return {
        "status": value_record(status),
        "repr": response_metadata_call(subject, "repr"),
        "bool": capture(
            lambda: response_metadata_call(subject, "bool")
        )["returned"],
        "ok": capture(
            lambda: response_metadata_call(subject, "ok")
        )["returned"],
        "redirect": response_metadata_call(subject, "is_redirect"),
        "permanent": response_metadata_call(
            subject, "is_permanent_redirect"
        ),
        "next_identity": (
            response_metadata_call(subject, "next") is next_request
        ),
        "history_identity": (
            response_metadata_call(subject, "history") is history
        ),
        "history_item_identity": (
            response_metadata_call(subject, "history")[0] is first
            and response_metadata_call(subject, "history")[1] is second
        ),
        "raise": {
            "record": raised["exception"],
            "response_identity": (
                error is not None
                and getattr(error, "response", None) is subject
            ),
            "request_identity": (
                error is not None
                and getattr(error, "request", None) is request
            ),
            "context_is_none": (
                error is None or error.__context__ is None
            ),
        },
    }


rows = [
    status_row(199, "Info"),
    status_row(200, "OK"),
    status_row(302, "Found", "/next"),
    status_row(308, "Permanent", "/next"),
    status_row(399, "Odd"),
    status_row(400, "Bad Request"),
    status_row(404, "Komponenttia ei löydy".encode("utf-8")),
    status_row(499, "Client Boundary"),
    status_row(500, b"\\xff"),
    status_row(599, "Network"),
    status_row(600, "Outside"),
]

unset = Response()
unset_bool = capture(
    lambda: response_metadata_call(unset, "bool")
)
unset_ok = capture(
    lambda: response_metadata_call(unset, "ok")
)
unset_raise = capture(
    lambda: response_metadata_call(unset, "raise_for_status")
)
result = {
    "rows": rows,
    "unset": {
        "repr": response_metadata_call(unset, "repr"),
        "bool": unset_bool["exception"],
        "ok": unset_ok["exception"],
        "raise": unset_raise["exception"],
    },
}
"""
    )

    rows = {row["status"]["payload"]: row for row in state["rows"]}
    assert rows[199]["bool"] is True
    assert rows[200]["ok"] is True
    assert rows[302]["redirect"] is True
    assert rows[302]["permanent"] is False
    assert rows[308]["redirect"] is True
    assert rows[308]["permanent"] is True
    assert rows[399]["bool"] is True
    assert rows[400]["bool"] is False
    assert rows[400]["ok"] is False
    assert rows[400]["raise"]["record"]["args"] == [
        "400 Client Error: Bad Request for url: https://example.test/resource"
    ]
    assert rows[404]["raise"]["record"]["args"] == [
        "404 Client Error: Komponenttia ei löydy for url: https://example.test/resource"
    ]
    assert rows[499]["raise"]["record"]["args"] == [
        "499 Client Error: Client Boundary for url: https://example.test/resource"
    ]
    assert rows[500]["raise"]["record"]["args"] == [
        "500 Server Error: ÿ for url: https://example.test/resource"
    ]
    assert rows[599]["raise"]["record"]["args"] == [
        "599 Server Error: Network for url: https://example.test/resource"
    ]
    assert rows[600]["bool"] is True
    assert rows[600]["raise"]["record"] is None
    assert all(row["next_identity"] for row in state["rows"])
    assert all(row["history_identity"] for row in state["rows"])
    assert all(row["history_item_identity"] for row in state["rows"])
    assert all(
        row["raise"]["response_identity"]
        and row["raise"]["request_identity"]
        and row["raise"]["context_is_none"]
        for row in state["rows"]
        if row["raise"]["record"] is not None
    )
    assert state["unset"]["repr"] == "<Response [None]>"
    assert state["unset"]["bool"] == {
        "type": ["builtins", "TypeError"],
        "args": ["'<=' not supported between instances of 'int' and 'NoneType'"],
    }
    assert state["unset"]["ok"] == state["unset"]["bool"]
    assert state["unset"]["raise"] == state["unset"]["bool"]


def test_metadata_dynamic_subclass_and_rebound_method_use_origin() -> None:
    state = _run_matching(
        """
class DynamicMetadata(Response):
    def __repr__(self):
        side_effects.append(["dynamic-repr", same_thread()])
        return "<dynamic-response>"

    def __bool__(self):
        side_effects.append(["dynamic-bool", same_thread()])
        return False

    @property
    def ok(self):
        side_effects.append(["dynamic-ok", same_thread()])
        return "dynamic-ok"

    @property
    def is_redirect(self):
        side_effects.append(["dynamic-redirect", same_thread()])
        return "dynamic-redirect"

    @property
    def is_permanent_redirect(self):
        side_effects.append(["dynamic-permanent", same_thread()])
        return "dynamic-permanent"

    @property
    def next(self):
        side_effects.append(["dynamic-next", same_thread()])
        return self

    def raise_for_status(self):
        side_effects.append(["dynamic-raise", same_thread()])
        return self


dynamic = DynamicMetadata()
dynamic_values = {
    "repr": response_metadata_call(dynamic, "repr"),
    "bool": response_metadata_call(dynamic, "bool"),
    "ok": response_metadata_call(dynamic, "ok"),
    "redirect": response_metadata_call(dynamic, "is_redirect"),
    "permanent": response_metadata_call(
        dynamic, "is_permanent_redirect"
    ),
    "next_identity": (
        response_metadata_call(dynamic, "next") is dynamic
    ),
    "raise_identity": (
        response_metadata_call(dynamic, "raise_for_status") is dynamic
    ),
}

original_raise = Response.raise_for_status
def rebound_raise(self):
    side_effects.append([
        "rebound-raise",
        self is rebound,
        same_thread(),
    ])
    return "rebound"

Response.raise_for_status = rebound_raise
try:
    rebound = Response()
    rebound.status_code = 200
    rebound_raise_result = response_metadata_call(
        rebound, "raise_for_status"
    )
    rebound_ok = response_metadata_call(rebound, "ok")
finally:
    Response.raise_for_status = original_raise

error = BaseException("metadata failed")
class FailingMetadata(Response):
    def raise_for_status(self):
        side_effects.append(["metadata-error", same_thread()])
        raise error


failure = capture(
    lambda: response_metadata_call(
        FailingMetadata(), "raise_for_status"
    )
)
result = {
    "dynamic": dynamic_values,
    "rebound_raise": rebound_raise_result,
    "rebound_ok": rebound_ok,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic"] == {
        "repr": "<dynamic-response>",
        "bool": False,
        "ok": "dynamic-ok",
        "redirect": "dynamic-redirect",
        "permanent": "dynamic-permanent",
        "next_identity": True,
        "raise_identity": True,
    }
    assert state["rebound_raise"] == "rebound"
    assert state["rebound_ok"] is True
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        ["dynamic-repr", True],
        ["dynamic-bool", True],
        ["dynamic-ok", True],
        ["dynamic-redirect", True],
        ["dynamic-permanent", True],
        ["dynamic-next", True],
        ["dynamic-raise", True],
        ["rebound-raise", True, True],
        ["rebound-raise", True, True],
        ["metadata-error", True],
    ]


def test_pickle_getstate_consumes_and_setstate_preserves_identity() -> None:
    state = _run_matching(
        """
from requests.models import PreparedRequest


raw = ObservedStreamRaw([b"abc"])
subject = Response()
subject.raw = raw
subject.status_code = 201
history_item = Response()
history = [history_item]
subject.history = history
request = PreparedRequest()
subject.request = request
next_request = PreparedRequest()
subject._next = next_request
subject.connection = object()
subject.extra_state = object()
before = response_fields_snapshot(subject)
pickled_state = response_pickle_call(subject, "get")
after = response_fields_snapshot(subject)

restored = Response.__new__(Response)
set_result = response_pickle_call(restored, "set", pickled_state)
restored_next = capture(
    lambda: response_metadata_call(restored, "next")
)

class ObservedState(dict):
    def items(self):
        side_effects.append(["state-items", same_thread()])
        return [
            ("status_code", 299),
            ("_content", b"manual"),
            ("history", history),
        ]


manual = Response.__new__(Response)
manual_result = response_pickle_call(manual, "set", ObservedState())

result = {
    "before": before,
    "keys": list(pickled_state),
    "content": value_record(pickled_state["_content"]),
    "raw_absent": "raw" not in pickled_state,
    "consumed_absent": "_content_consumed" not in pickled_state,
    "next_absent": "_next" not in pickled_state,
    "connection_absent": "connection" not in pickled_state,
    "extra_absent": "extra_state" not in pickled_state,
    "history_identity": pickled_state["history"] is history,
    "history_item_identity": pickled_state["history"][0] is history_item,
    "request_identity": pickled_state["request"] is request,
    "events": raw.events,
    "after": after,
    "set_return": value_record(set_result),
    "restored": {
        "raw_is_none": restored.raw is None,
        "consumed": restored._content_consumed,
        "history_identity": restored.history is history,
        "request_identity": restored.request is request,
        "content": value_record(restored._content),
        "has_next": hasattr(restored, "_next"),
        "has_connection": hasattr(restored, "connection"),
        "has_extra": hasattr(restored, "extra_state"),
        "next_error": restored_next["exception"],
    },
    "manual_return": value_record(manual_result),
    "manual": {
        "status": manual.status_code,
        "content": value_record(manual._content),
        "history_identity": manual.history is history,
        "raw_is_none": manual.raw is None,
        "consumed": manual._content_consumed,
        "has_headers": hasattr(manual, "headers"),
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["before"]["content_is_false"] is True
    assert state["before"]["content_consumed"] is False
    assert state["keys"] == [
        "_content",
        "status_code",
        "headers",
        "url",
        "history",
        "encoding",
        "reason",
        "cookies",
        "elapsed",
        "request",
    ]
    assert state["content"]["payload"] == ["bytes", b"abc".hex()]
    assert state["raw_absent"] is True
    assert state["consumed_absent"] is True
    assert state["next_absent"] is True
    assert state["connection_absent"] is True
    assert state["extra_absent"] is True
    assert state["history_identity"] is True
    assert state["history_item_identity"] is True
    assert state["request_identity"] is True
    assert state["events"][:3] == [
        ["getattr", "stream", True],
        ["getattr", "stream", True],
        ["stream", {"type": ["builtins", "int"], "payload": 10240}, True, True],
    ]
    assert state["after"]["content_consumed"] is True
    assert state["set_return"]["payload"] is None
    assert state["restored"] == {
        "raw_is_none": True,
        "consumed": True,
        "history_identity": True,
        "request_identity": True,
        "content": {
            "type": ["builtins", "bytes"],
            "payload": ["bytes", b"abc".hex()],
        },
        "has_next": False,
        "has_connection": False,
        "has_extra": False,
        "next_error": {
            "type": ["builtins", "AttributeError"],
            "args": [
                "'Response' object has no attribute '_next'",
            ],
        },
    }
    assert state["manual_return"]["payload"] is None
    assert state["manual"] == {
        "status": 299,
        "content": {
            "type": ["builtins", "bytes"],
            "payload": ["bytes", b"manual".hex()],
        },
        "history_identity": True,
        "raw_is_none": True,
        "consumed": True,
        "has_headers": False,
    }
    assert state["side_effects"] == [["state-items", True]]


def test_pickle_dynamic_methods_attrs_and_read_errors_preserve_identity() -> None:
    state = _run_matching(
        """
class DynamicPickle(Response):
    def __getstate__(self):
        side_effects.append(["dynamic-getstate", same_thread()])
        return {"owner": self}

    def __setstate__(self, state):
        side_effects.append([
            "dynamic-setstate",
            state["owner"] is self,
            same_thread(),
        ])
        self.restored = state
        return self


dynamic = DynamicPickle()
dynamic_state = response_pickle_call(dynamic, "get")
dynamic_set = response_pickle_call(
    dynamic, "set", {"owner": dynamic}
)

original_attrs = Response.__attrs__
Response.__attrs__ = ["status_code"]
try:
    rebound = Response()
    rebound._content = b"already"
    rebound._content_consumed = True
    rebound.status_code = 207
    rebound_state = response_pickle_call(rebound, "get")
finally:
    Response.__attrs__ = original_attrs

read_error = BaseException("pickle read failed")
failed_raw = ObservedReadRaw(error=read_error)
failed = Response()
failed.raw = failed_raw
failure = capture(lambda: response_pickle_call(failed, "get"))

result = {
    "dynamic_get_identity": dynamic_state["owner"] is dynamic,
    "dynamic_set_identity": dynamic_set is dynamic,
    "dynamic_restored_identity": dynamic.restored["owner"] is dynamic,
    "rebound_state": rebound_state,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is read_error,
        "context_is_none": failure["error"].__context__ is None,
        "events": failed_raw.events,
        "state": response_fields_snapshot(failed),
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic_get_identity"] is True
    assert state["dynamic_set_identity"] is True
    assert state["dynamic_restored_identity"] is True
    assert state["rebound_state"] == {"status_code": 207}
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["failure"]["context_is_none"] is True
    assert state["failure"]["events"] == [
        ["getattr-missing", "stream", True],
        ["read", {"type": ["builtins", "int"], "payload": 10240}, True],
    ]
    assert state["failure"]["state"]["content_is_false"] is True
    assert state["failure"]["state"]["content_consumed"] is False
    assert state["side_effects"] == [
        ["dynamic-getstate", True],
        ["dynamic-setstate", True, True],
    ]


def test_explicit_close_repeats_custom_raw_callbacks_without_state_transition() -> None:
    state = _run_matching(
        """
unconsumed_raw = ObservedStreamRaw([b"unused"])
unconsumed = Response()
unconsumed.raw = unconsumed_raw
unconsumed_identity_before = unconsumed.raw is unconsumed_raw
unconsumed_first = response_close_call(unconsumed)
unconsumed_second = response_close_call(unconsumed)

consumed_raw = ObservedStreamRaw([b"unused"])
consumed = Response()
consumed.raw = consumed_raw
consumed._content_consumed = True
consumed_first = response_close_call(consumed)
consumed_second = response_close_call(consumed)

class CloseOnly:
    def __init__(self):
        self.events = []

    def close(self):
        self.events.append(["close-only", same_thread()])


close_only_raw = CloseOnly()
close_only = Response()
close_only.raw = close_only_raw
response_close_call(close_only)
response_close_call(close_only)

close_error = BaseException("close failed")
class FailingClose:
    def __init__(self):
        self.events = []

    def close(self):
        self.events.append(["close-error", same_thread()])
        raise close_error

    def release_conn(self):
        self.events.append(["release-after-error", same_thread()])


failing_raw = FailingClose()
failing = Response()
failing.raw = failing_raw
failed = capture(lambda: response_close_call(failing))

result = {
    "unconsumed": {
        "identity_before": unconsumed_identity_before,
        "identity_after": unconsumed.raw is unconsumed_raw,
        "first": value_record(unconsumed_first),
        "second": value_record(unconsumed_second),
        "events": unconsumed_raw.events,
        "state": response_fields_snapshot(unconsumed),
    },
    "consumed": {
        "first": value_record(consumed_first),
        "second": value_record(consumed_second),
        "events": consumed_raw.events,
        "state": response_fields_snapshot(consumed),
    },
    "close_only": {
        "events": close_only_raw.events,
        "state": response_fields_snapshot(close_only),
    },
    "failure": {
        "record": failed["exception"],
        "is_original": failed["error"] is close_error,
        "events": failing_raw.events,
        "state": response_fields_snapshot(failing),
    },
}
"""
    )

    assert state["unconsumed"]["identity_before"] is True
    assert state["unconsumed"]["identity_after"] is True
    assert state["unconsumed"]["first"]["payload"] is None
    assert state["unconsumed"]["second"]["payload"] is None
    assert state["unconsumed"]["events"] == [
        ["close", True],
        ["release", True],
        ["close", True],
        ["release", True],
    ]
    assert state["unconsumed"]["state"]["content_is_false"] is True
    assert state["unconsumed"]["state"]["content_consumed"] is False
    assert state["consumed"]["events"] == [
        ["release", True],
        ["release", True],
    ]
    assert state["consumed"]["state"]["content_is_false"] is True
    assert state["consumed"]["state"]["content_consumed"] is True
    assert state["close_only"]["events"] == [
        ["close-only", True],
        ["close-only", True],
    ]
    assert state["close_only"]["state"]["content_consumed"] is False
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["failure"]["events"] == [["close-error", True]]
    assert state["failure"]["state"]["content_consumed"] is False


def test_close_dynamic_subclass_and_rebound_release_lookup_use_origin() -> None:
    state = _run_matching(
        """
class DynamicClose(Response):
    def close(self):
        side_effects.append(["dynamic-close", same_thread()])
        return self


dynamic = DynamicClose()
dynamic_returned = response_close_call(dynamic)

class DynamicRelease:
    def __init__(self):
        self.lookups = 0

    def close(self):
        side_effects.append(["raw-close", same_thread()])

    def __getattribute__(self, name):
        if name == "release_conn":
            current = object.__getattribute__(self, "lookups") + 1
            object.__setattr__(self, "lookups", current)
            side_effects.append([
                "release-lookup",
                current,
                same_thread(),
            ])
            def release():
                side_effects.append([
                    "dynamic-release",
                    current,
                    same_thread(),
                ])
            return release
        return object.__getattribute__(self, name)


raw = DynamicRelease()
subject = Response()
subject.raw = raw
response_close_call(subject)
response_close_call(subject)

error = BaseException("dynamic close failed")
class FailingClose(Response):
    def close(self):
        side_effects.append(["dynamic-close-error", same_thread()])
        raise error


failure = capture(lambda: response_close_call(FailingClose()))
result = {
    "dynamic_identity": dynamic_returned is dynamic,
    "release_lookups": raw.lookups,
    "failure": {
        "record": failure["exception"],
        "is_original": failure["error"] is error,
    },
    "side_effects": side_effects,
}
"""
    )

    assert state["dynamic_identity"] is True
    assert state["release_lookups"] == 2
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["is_original"] is True
    assert state["side_effects"] == [
        ["dynamic-close", True],
        ["raw-close", True],
        ["release-lookup", 1, True],
        ["dynamic-release", 1, True],
        ["raw-close", True],
        ["release-lookup", 2, True],
        ["dynamic-release", 2, True],
        ["dynamic-close-error", True],
    ]


def test_drop_keeps_custom_raw_callbacks_explicit_and_releases_on_origin() -> None:
    state = _run_matching(
        """
class TrackedRaw(ObservedStreamRaw):
    def __init__(self, label, chunks=(), error=None):
        super().__init__(chunks, error)
        self.label = label

    def __del__(self):
        side_effects.append(["raw-del", self.label, same_thread()])


rows = []
for operation in ("untouched", "partial", "exhausted", "cached"):
    raw = TrackedRaw(operation, [b"ab", b"cd"])
    dropped = response_drop_call(raw, operation, 2)
    rows.append({
        "operation": operation,
        "dropped": dropped,
        "events_before_raw_del": list(raw.events),
        "callbacks_absent": not any(
            event[0] in ("close", "release")
            for event in raw.events
        ),
    })
    del raw
    gc.collect()
    side_effects.append(["after-raw-del", operation])

failure_error = BaseException("drop stream failed")
failed_raw = TrackedRaw("failed", [], failure_error)
failed = capture(
    lambda: response_drop_call(failed_raw, "failed", 2)
)
failed_before_del = list(failed_raw.events)
failed_callbacks_absent = not any(
    event[0] in ("close", "release")
    for event in failed_raw.events
)
failure_error.__traceback__ = None
del failed
del failed_raw
gc.collect()
side_effects.append(["after-raw-del", "failed"])

result = {
    "rows": rows,
    "failure": {
        "record": exception_record(failure_error),
        "events_before_raw_del": failed_before_del,
        "callbacks_absent": failed_callbacks_absent,
    },
    "side_effects": side_effects,
}
"""
    )

    rows = {row["operation"]: row for row in state["rows"]}
    assert rows["untouched"]["dropped"]["raw_is_original"] is True
    assert rows["untouched"]["dropped"]["state"]["content_consumed"] is False
    assert rows["untouched"]["events_before_raw_del"] == []
    assert rows["partial"]["dropped"]["state"]["content_is_false"] is True
    assert rows["partial"]["dropped"]["state"]["content_consumed"] is False
    assert rows["partial"]["events_before_raw_del"][-1] == [
        "yield",
        {"type": ["builtins", "bytes"], "payload": ["bytes", b"ab".hex()]},
        True,
    ]
    assert rows["exhausted"]["dropped"]["state"]["content_is_false"] is True
    assert rows["exhausted"]["dropped"]["state"]["content_consumed"] is True
    assert rows["cached"]["dropped"]["state"]["content"]["payload"] == [
        "bytes",
        b"abcd".hex(),
    ]
    assert rows["cached"]["dropped"]["state"]["content_consumed"] is True
    assert all(row["callbacks_absent"] for row in state["rows"])
    assert state["failure"]["record"]["type"] == ["builtins", "BaseException"]
    assert state["failure"]["callbacks_absent"] is True
    assert state["side_effects"] == [
        ["raw-del", "untouched", True],
        ["after-raw-del", "untouched"],
        ["raw-del", "partial", True],
        ["after-raw-del", "partial"],
        ["raw-del", "exhausted", True],
        ["after-raw-del", "exhausted"],
        ["raw-del", "cached", True],
        ["after-raw-del", "cached"],
        ["raw-del", "failed", True],
        ["after-raw-del", "failed"],
    ]


def test_partial_and_interleaved_iterators_share_the_authoritative_cursor() -> None:
    state = _run_matching(
        """
raw = ObservedReadRaw([b"a", b"b", b"c"])
subject = Response()
subject.raw = raw
first_iterator = response_iter_content_call(subject, 1)
first_value = next(first_iterator)
second_iterator = response_iter_content_call(subject, 1)
second_value = next(second_iterator)
third_value = next(first_iterator)
second_tail = list(second_iterator)
first_tail = list(first_iterator)
consumed = capture(
    lambda: response_iter_content_call(subject, 1)
)

line_raw = ObservedReadRaw([b"a\\nb", b"c\\nd", b"e\\n"])
line_subject = Response()
line_subject.raw = line_raw
first_lines = response_iter_lines_call(line_subject, 2)
second_lines = response_iter_lines_call(line_subject, 2)
line_values = [
    next(first_lines),
    next(second_lines),
    next(first_lines),
]
second_line_tail = list(second_lines)
first_line_tail = list(first_lines)

result = {
    "content": {
        "values": [
            value_record(first_value),
            value_record(second_value),
            value_record(third_value),
        ],
        "second_tail": [
            value_record(value) for value in second_tail
        ],
        "first_tail": [
            value_record(value) for value in first_tail
        ],
        "events": raw.events,
        "state": response_fields_snapshot(subject),
        "consumed_error": consumed["exception"],
    },
    "lines": {
        "values": [value_record(value) for value in line_values],
        "second_tail": [
            value_record(value) for value in second_line_tail
        ],
        "first_tail": [
            value_record(value) for value in first_line_tail
        ],
        "events": line_raw.events,
        "state": response_fields_snapshot(line_subject),
    },
}
"""
    )

    assert [value["payload"][1] for value in state["content"]["values"]] == [
        b"a".hex(),
        b"b".hex(),
        b"c".hex(),
    ]
    assert state["content"]["second_tail"] == []
    assert state["content"]["first_tail"] == []
    assert state["content"]["state"]["content_is_false"] is True
    assert state["content"]["state"]["content_consumed"] is True
    assert state["content"]["consumed_error"] == {
        "type": ["requests.exceptions", "StreamConsumedError"],
        "args": [],
    }
    assert [value["payload"][1] for value in state["lines"]["values"]] == [
        b"a".hex(),
        b"c".hex(),
        b"be".hex(),
    ]
    assert [value["payload"][1] for value in state["lines"]["second_tail"]] == [
        b"d".hex()
    ]
    assert state["lines"]["first_tail"] == []
    assert state["lines"]["state"]["content_consumed"] is True


def test_python_exact_stream_failure_can_retry_the_same_raw_successfully() -> None:
    state = _run_matching(
        """
from urllib3.exceptions import ProtocolError


class FailOnceRaw:
    def __init__(self):
        self.attempts = 0
        self.events = []
        self.original = ProtocolError("first attempt failed")

    def stream(self, chunk_size, decode_content=True):
        self.attempts += 1
        attempt = self.attempts
        self.events.append([
            "stream",
            attempt,
            chunk_size,
            decode_content,
            same_thread(),
        ])
        if attempt == 1:
            raise self.original
        yield b"retry-ok"

    def close(self):
        self.events.append(["close", same_thread()])

    def release_conn(self):
        self.events.append(["release", same_thread()])


raw = FailOnceRaw()
subject = Response()
subject.raw = raw
first = capture(
    lambda: next(response_iter_content_call(subject, 4))
)
first_error = first["error"]
after_first = response_fields_snapshot(subject)
second = list(response_iter_content_call(subject, 4))
after_second = response_fields_snapshot(subject)

result = {
    "first": {
        "record": first["exception"],
        "argument_is_original": first_error.args[0] is raw.original,
        "context_is_original": first_error.__context__ is raw.original,
    },
    "second": [value_record(value) for value in second],
    "same_raw": subject.raw is raw,
    "after_first": after_first,
    "after_second": after_second,
    "events": raw.events,
    "implicit_callbacks_absent": not any(
        event[0] in ("close", "release") for event in raw.events
    ),
}
"""
    )

    assert state["first"]["record"]["type"] == [
        "requests.exceptions",
        "ChunkedEncodingError",
    ]
    assert state["first"]["argument_is_original"] is True
    assert state["first"]["context_is_original"] is True
    assert state["after_first"]["content_is_false"] is True
    assert state["after_first"]["content_consumed"] is False
    assert [value["payload"][1] for value in state["second"]] == [b"retry-ok".hex()]
    assert state["same_raw"] is True
    assert state["after_second"]["content_is_false"] is True
    assert state["after_second"]["content_consumed"] is True
    assert state["events"] == [
        ["stream", 1, 4, True, True],
        ["stream", 2, 4, True, True],
    ]
    assert state["implicit_callbacks_absent"] is True


def test_raw_resolution_observes_replacement_at_each_frozen_lookup_boundary() -> None:
    state = _run_matching(
        """
before_first_old = ObservedStreamRaw([b"old"])
before_first_new = ObservedStreamRaw([b"new"])
before_first = Response()
before_first.raw = before_first_old
before_first_iterator = response_iter_content_call(before_first, 1)
before_first.raw = before_first_new
before_first_values = list(before_first_iterator)

between_stream_old = ObservedStreamRaw([b"old-a", b"old-b"])
between_stream_new = ObservedStreamRaw([b"new-stream"])
between_stream = Response()
between_stream.raw = between_stream_old
between_stream_iterator = response_iter_content_call(between_stream, 1)
between_stream_first = next(between_stream_iterator)
between_stream.raw = between_stream_new
between_stream_rest = list(between_stream_iterator)

class ReadAndStreamRaw(ObservedReadRaw):
    def stream(self, chunk_size, decode_content=True):
        self.events.append([
            "unexpected-stream",
            chunk_size,
            decode_content,
            same_thread(),
        ])
        yield b"wrong"


between_read_old = ObservedReadRaw([b"old-read"])
between_read_new = ReadAndStreamRaw([b"new-read"])
between_read = Response()
between_read.raw = between_read_old
between_read_iterator = response_iter_content_call(between_read, 1)
between_read_first = next(between_read_iterator)
between_read.raw = between_read_new
between_read_second = next(between_read_iterator)
between_read_tail = list(between_read_iterator)

result = {
    "before_first": {
        "values": [
            value_record(value) for value in before_first_values
        ],
        "old_events": before_first_old.events,
        "new_events": before_first_new.events,
        "raw_identity": before_first.raw is before_first_new,
    },
    "between_stream": {
        "first": value_record(between_stream_first),
        "rest": [
            value_record(value) for value in between_stream_rest
        ],
        "old_events": between_stream_old.events,
        "new_events": between_stream_new.events,
        "raw_identity": between_stream.raw is between_stream_new,
    },
    "between_read": {
        "first": value_record(between_read_first),
        "second": value_record(between_read_second),
        "tail": [
            value_record(value) for value in between_read_tail
        ],
        "old_events": between_read_old.events,
        "new_events": between_read_new.events,
        "raw_identity": between_read.raw is between_read_new,
    },
}
"""
    )

    assert [value["payload"][1] for value in state["before_first"]["values"]] == [
        b"new".hex()
    ]
    assert state["before_first"]["old_events"] == []
    assert state["before_first"]["new_events"][0] == [
        "getattr",
        "stream",
        True,
    ]
    assert state["before_first"]["raw_identity"] is True
    assert state["between_stream"]["first"]["payload"][1] == b"old-a".hex()
    assert [value["payload"][1] for value in state["between_stream"]["rest"]] == [
        b"old-b".hex()
    ]
    assert state["between_stream"]["new_events"] == []
    assert state["between_stream"]["raw_identity"] is True
    assert state["between_read"]["first"]["payload"][1] == b"old-read".hex()
    assert state["between_read"]["second"]["payload"][1] == b"new-read".hex()
    assert state["between_read"]["tail"] == []
    assert not any(
        event[0] == "unexpected-stream" for event in state["between_read"]["new_events"]
    )
    assert state["between_read"]["raw_identity"] is True


def test_lines_and_unicode_yield_before_consuming_later_raw_chunks() -> None:
    state = _run_matching(
        """
line_raw = ObservedStreamRaw([
    b"first\\npending",
    b"-rest\\n",
    b"tail\\n",
])
line_subject = Response()
line_subject.raw = line_raw
line_iterator = response_iter_lines_call(line_subject, 8)
line_before = list(line_raw.events)
line_first = next(line_iterator)
line_after_first = list(line_raw.events)
line_second = next(line_iterator)
line_after_second = list(line_raw.events)
line_third = next(line_iterator)
line_after_third = list(line_raw.events)
list(line_iterator)

unicode_raw = ObservedStreamRaw([
    b"\\xe2",
    b"\\x82",
    b"\\xacA",
    b"B",
])
unicode_subject = Response()
unicode_subject.raw = unicode_raw
unicode_subject.encoding = "utf-8"
unicode_iterator = response_iter_content_call(
    unicode_subject, 1, True
)
unicode_first = next(unicode_iterator)
unicode_after_first = list(unicode_raw.events)
unicode_second = next(unicode_iterator)
unicode_after_second = list(unicode_raw.events)
list(unicode_iterator)

class TailObservedRaw:
    def __init__(self):
        self.events = []

    def stream(self, chunk_size, decode_content=True):
        try:
            for chunk in (b"A\\xe2", b"\\x82", b"\\xac", b"tail"):
                self.events.append(["yield", value_record(chunk)])
                yield chunk
        finally:
            self.events.append(["generator-close", same_thread()])


abandoned_raw = TailObservedRaw()
abandoned = Response()
abandoned.raw = abandoned_raw
abandoned.encoding = "utf-8"
abandoned_iterator = response_iter_content_call(abandoned, 1, True)
abandoned_first = next(abandoned_iterator)
abandoned_after_first = list(abandoned_raw.events)
abandoned_iterator.close()

result = {
    "lines": {
        "before": line_before,
        "first": value_record(line_first),
        "after_first": line_after_first,
        "second": value_record(line_second),
        "after_second": line_after_second,
        "third": value_record(line_third),
        "after_third": line_after_third,
    },
    "unicode": {
        "first": unicode_first,
        "after_first": unicode_after_first,
        "second": unicode_second,
        "after_second": unicode_after_second,
    },
    "abandoned": {
        "first": abandoned_first,
        "after_first": abandoned_after_first,
        "after_close": abandoned_raw.events,
        "state": response_fields_snapshot(abandoned),
    },
}
"""
    )

    assert state["lines"]["before"] == []
    assert state["lines"]["first"]["payload"][1] == b"first".hex()
    assert sum(event[0] == "yield" for event in state["lines"]["after_first"]) == 1
    assert state["lines"]["second"]["payload"][1] == b"pending-rest".hex()
    assert sum(event[0] == "yield" for event in state["lines"]["after_second"]) == 2
    assert state["lines"]["third"]["payload"][1] == b"tail".hex()
    assert sum(event[0] == "yield" for event in state["lines"]["after_third"]) == 3
    assert state["unicode"]["first"] == "€A"
    assert sum(event[0] == "yield" for event in state["unicode"]["after_first"]) == 3
    assert state["unicode"]["second"] == "B"
    assert sum(event[0] == "yield" for event in state["unicode"]["after_second"]) == 4
    assert state["abandoned"]["first"] == "A"
    assert state["abandoned"]["after_first"] == [
        [
            "yield",
            {
                "type": ["builtins", "bytes"],
                "payload": ["bytes", b"A\xe2".hex()],
            },
        ]
    ]
    assert state["abandoned"]["after_close"][-1] == [
        "generator-close",
        True,
    ]
    assert not any(
        event
        == [
            "yield",
            {
                "type": ["builtins", "bytes"],
                "payload": ["bytes", b"tail".hex()],
            },
        ]
        for event in state["abandoned"]["after_close"]
    )
    assert state["abandoned"]["state"]["content_consumed"] is False


def test_non_bytes_content_join_failure_leaves_exhausted_uncached_state() -> None:
    state = _run_matching(
        """
raw = ObservedStreamRaw([b"bytes", "text"])
subject = Response()
subject.status_code = 200
subject.raw = raw
first = capture(lambda: response_content_call(subject))
after_first = response_fields_snapshot(subject)
second = capture(lambda: response_content_call(subject))
later_iterator = capture(
    lambda: response_iter_content_call(subject, 1)
)
close_result = capture(lambda: response_close_call(subject))

result = {
    "first": first["exception"],
    "after_first": after_first,
    "second": second["exception"],
    "later_iterator": later_iterator["exception"],
    "close": close_result["exception"],
    "events": raw.events,
}
"""
    )

    assert state["first"]["type"] == ["builtins", "TypeError"]
    assert state["first"]["args"] == [
        "sequence item 1: expected a bytes-like object, str found"
    ]
    assert state["after_first"]["content_is_false"] is True
    assert state["after_first"]["content_consumed"] is True
    assert state["second"] == {
        "type": ["builtins", "RuntimeError"],
        "args": ["The content for this response was already consumed"],
    }
    assert state["later_iterator"] == {
        "type": ["requests.exceptions", "StreamConsumedError"],
        "args": [],
    }
    assert state["close"] is None
    assert state["events"][-1] == ["release", True]
    assert not any(event[0] == "close" for event in state["events"])


def test_dynamic_status_and_redirect_globals_remain_authoritative() -> None:
    state = _run_matching(
        """
import requests.models as models


original_http_error = models.HTTPError
class DynamicHTTPError(original_http_error):
    pass


models.HTTPError = DynamicHTTPError
try:
    failed = Response()
    failed.status_code = 499
    failed.reason = "Dynamic"
    failed.url = "https://example.test/dynamic"
    raised = capture(
        lambda: response_metadata_call(failed, "raise_for_status")
    )
    ok = response_metadata_call(failed, "ok")
finally:
    models.HTTPError = original_http_error


redirect_events = []
class DynamicRedirectStatuses:
    def __contains__(self, status):
        redirect_events.append(["contains", status, same_thread()])
        return status == 302


class DynamicCodes:
    @property
    def moved_permanently(self):
        redirect_events.append(["codes", "moved", same_thread()])
        return 301

    @property
    def permanent_redirect(self):
        redirect_events.append(["codes", "permanent", same_thread()])
        return 308


original_redirects = models.REDIRECT_STATI
original_codes = models.codes
models.REDIRECT_STATI = DynamicRedirectStatuses()
models.codes = DynamicCodes()
try:
    no_location = Response()
    no_location.status_code = 302
    no_location_values = [
        response_metadata_call(no_location, "is_redirect"),
        response_metadata_call(no_location, "is_permanent_redirect"),
    ]
    events_after_short_circuit = list(redirect_events)

    temporary = Response()
    temporary.status_code = 302
    temporary.headers["LoCaTiOn"] = "/next"
    redirect = response_metadata_call(temporary, "is_redirect")

    permanent = Response()
    permanent.status_code = 301
    permanent.headers["LOCATION"] = "/forever"
    permanent_value = response_metadata_call(
        permanent, "is_permanent_redirect"
    )
finally:
    models.REDIRECT_STATI = original_redirects
    models.codes = original_codes


raised_error = raised["error"]
result = {
    "status": {
        "record": raised["exception"],
        "dynamic_type": type(raised_error) is DynamicHTTPError,
        "response_identity": raised_error.response is failed,
        "ok": ok,
    },
    "redirect": {
        "no_location": no_location_values,
        "events_after_short_circuit": events_after_short_circuit,
        "temporary": redirect,
        "permanent": permanent_value,
        "events": redirect_events,
    },
}
"""
    )

    assert state["status"]["record"]["type"][1] == "DynamicHTTPError"
    assert state["status"]["record"]["args"] == [
        "499 Client Error: Dynamic for url: https://example.test/dynamic"
    ]
    assert state["status"]["dynamic_type"] is True
    assert state["status"]["response_identity"] is True
    assert state["status"]["ok"] is False
    assert state["redirect"] == {
        "no_location": [False, False],
        "events_after_short_circuit": [],
        "temporary": True,
        "permanent": True,
        "events": [
            ["contains", 302, True],
            ["codes", "moved", True],
            ["codes", "permanent", True],
        ],
    }


def test_dynamic_stream_source_and_wrapper_globals_remain_authoritative() -> None:
    state = _run_matching(
        """
import requests.models as models


class ProtocolSource(Exception):
    pass


class DecodeSource(Exception):
    pass


class TimeoutSource(Exception):
    pass


class SslSource(Exception):
    pass


class ProtocolTarget(Exception):
    pass


class DecodeTarget(Exception):
    pass


class TimeoutTarget(Exception):
    pass


class SslTarget(Exception):
    pass


class DynamicErrorRaw:
    def __init__(self, error):
        self.error = error

    def stream(self, chunk_size, decode_content=True):
        raise self.error
        yield


bindings = [
    ("ProtocolError", ProtocolSource, "ChunkedEncodingError", ProtocolTarget),
    ("DecodeError", DecodeSource, "ContentDecodingError", DecodeTarget),
    ("ReadTimeoutError", TimeoutSource, "ConnectionError", TimeoutTarget),
    ("SSLError", SslSource, "RequestsSSLError", SslTarget),
]
originals = {
    name: getattr(models, name)
    for row in bindings
    for name in (row[0], row[2])
}
rows = []
try:
    for source_name, source_type, target_name, target_type in bindings:
        setattr(models, source_name, source_type)
        setattr(models, target_name, target_type)
        original = source_type(source_name)
        subject = Response()
        subject.raw = DynamicErrorRaw(original)
        caught = capture(
            lambda subject=subject: next(
                response_iter_content_call(subject, 1)
            )
        )
        error = caught["error"]
        rows.append({
            "source": source_name,
            "target": target_name,
            "target_identity": type(error) is target_type,
            "argument_identity": error.args[0] is original,
            "context_identity": error.__context__ is original,
            "cause_is_none": error.__cause__ is None,
            "suppress_context": error.__suppress_context__,
            "state": response_fields_snapshot(subject),
        })
finally:
    for name, value in originals.items():
        setattr(models, name, value)


result = rows
"""
    )

    assert [row["source"] for row in state] == [
        "ProtocolError",
        "DecodeError",
        "ReadTimeoutError",
        "SSLError",
    ]
    assert [row["target"] for row in state] == [
        "ChunkedEncodingError",
        "ContentDecodingError",
        "ConnectionError",
        "RequestsSSLError",
    ]
    assert all(row["target_identity"] for row in state)
    assert all(row["argument_identity"] for row in state)
    assert all(row["context_identity"] for row in state)
    assert all(row["cause_is_none"] for row in state)
    assert not any(row["suppress_context"] for row in state)
    assert not any(row["state"]["content_consumed"] for row in state)


def test_json_short_bom_utf32_and_guess_none_boundaries() -> None:
    state = _run_matching(
        """
import requests.models as models


class SelectedJson:
    def __init__(self):
        self.calls = []

    def loads(self, text, **kwargs):
        self.calls.append([
            value_record(text),
            sorted(kwargs.items()),
            same_thread(),
        ])
        return len(self.calls)


def cached(content):
    subject = Response()
    subject._content = content
    subject._content_consumed = True
    subject.encoding = None
    return subject


selected = SelectedJson()
guess_calls = []
original_json = models.complexjson
original_guess = models.guess_json_utf
original_detector = models.chardet
models.complexjson = selected
models.chardet = None
try:
    short = response_json_call(cached(b"1"), {"marker": "short"})
    bom = response_json_call(
        cached('{"bom": 1}'.encode("utf-8-sig")),
        {"marker": "bom"},
    )
    utf32 = response_json_call(
        cached('{"wide": 2}'.encode("utf-32")),
        {"marker": "utf32"},
    )

    def no_guess(content):
        guess_calls.append([value_record(content), same_thread()])
        return None

    models.guess_json_utf = no_guess
    fallback = response_json_call(
        cached(b'{"fallback": 3}'),
        {"marker": "fallback"},
    )
finally:
    models.complexjson = original_json
    models.guess_json_utf = original_guess
    models.chardet = original_detector


result = {
    "returns": [short, bom, utf32, fallback],
    "calls": selected.calls,
    "guess_calls": guess_calls,
}
"""
    )

    assert state["returns"] == [1, 2, 3, 4]
    assert [call[0]["payload"] for call in state["calls"]] == [
        ["str", "1"],
        ["str", '{"bom": 1}'],
        ["str", '{"wide": 2}'],
        ["str", '{"fallback": 3}'],
    ]
    assert [call[1] for call in state["calls"]] == [
        [["marker", "short"]],
        [["marker", "bom"]],
        [["marker", "utf32"]],
        [["marker", "fallback"]],
    ]
    assert all(call[2] for call in state["calls"])
    assert state["guess_calls"] == [
        [
            {
                "type": ["builtins", "bytes"],
                "payload": ["bytes", b'{"fallback": 3}'.hex()],
            },
            True,
        ]
    ]


def test_history_preserves_order_mutation_aliasing_and_cycles() -> None:
    state = _run_matching(
        """
oldest = Response()
oldest.label = "oldest"
middle = Response()
middle.label = "middle"
subject = Response()
subject.label = "subject"
alias = Response()

shared = [oldest, middle]
subject.history = shared
alias.history = shared
observed = response_metadata_call(subject, "history")
before = [item.label for item in observed]
shared.append(subject)
after = [item.label for item in observed]

result = {
    "identity": observed is shared,
    "alias_identity": (
        response_metadata_call(alias, "history") is observed
    ),
    "before": before,
    "after": after,
    "oldest_identity": observed[0] is oldest,
    "middle_identity": observed[1] is middle,
    "cycle_identity": observed[2] is subject,
    "cycle_back_identity": observed[2].history is observed,
}
"""
    )

    assert state == {
        "identity": True,
        "alias_identity": True,
        "before": ["oldest", "middle"],
        "after": ["oldest", "middle", "subject"],
        "oldest_identity": True,
        "middle_identity": True,
        "cycle_identity": True,
        "cycle_back_identity": True,
    }


def test_pickle_builtin_getattr_and_setattr_globals_remain_authoritative() -> None:
    state = _run_matching(
        """
import builtins
import requests.models as models


events = []
def observed_getattr(owner, name, default=None):
    events.append([
        "getattr",
        owner is subject,
        name,
        value_record(default),
        same_thread(),
    ])
    return builtins.getattr(owner, name, default)


def observed_setattr(owner, name, value):
    events.append([
        "setattr",
        owner is restored,
        name,
        value_record(value),
        same_thread(),
    ])
    return builtins.setattr(owner, name, value)


missing = object()
original_getattr = models.__dict__.get("getattr", missing)
original_setattr = models.__dict__.get("setattr", missing)
models.__dict__["getattr"] = observed_getattr
models.__dict__["setattr"] = observed_setattr
try:
    subject = Response()
    subject._content = b"cached"
    subject._content_consumed = True
    subject.status_code = 207
    subject.__attrs__ = ["status_code", "missing_attribute"]
    state_value = response_pickle_call(subject, "get")

    restored = Response.__new__(Response)
    set_returned = response_pickle_call(
        restored, "set", {"status_code": 299}
    )
finally:
    if original_getattr is missing:
        models.__dict__.pop("getattr", None)
    else:
        models.__dict__["getattr"] = original_getattr
    if original_setattr is missing:
        models.__dict__.pop("setattr", None)
    else:
        models.__dict__["setattr"] = original_setattr


result = {
    "state": state_value,
    "set_return": value_record(set_returned),
    "restored_status": restored.status_code,
    "restored_consumed": restored._content_consumed,
    "restored_raw_is_none": restored.raw is None,
    "events": events,
}
"""
    )

    assert state["state"] == {
        "status_code": 207,
        "missing_attribute": None,
    }
    assert state["set_return"]["payload"] is None
    assert state["restored_status"] == 299
    assert state["restored_consumed"] is True
    assert state["restored_raw_is_none"] is True
    assert state["events"] == [
        [
            "getattr",
            True,
            "status_code",
            {"type": ["builtins", "NoneType"], "payload": None},
            True,
        ],
        [
            "getattr",
            True,
            "missing_attribute",
            {"type": ["builtins", "NoneType"], "payload": None},
            True,
        ],
        [
            "setattr",
            True,
            "status_code",
            {"type": ["builtins", "int"], "payload": 299},
            True,
        ],
        [
            "setattr",
            True,
            "_content_consumed",
            {"type": ["builtins", "bool"], "payload": True},
            True,
        ],
        [
            "setattr",
            True,
            "raw",
            {"type": ["builtins", "NoneType"], "payload": None},
            True,
        ],
    ]


def test_release_failure_and_noncallable_release_preserve_order() -> None:
    state = _run_matching(
        """
class ReleaseFailure:
    def __init__(self, error):
        self.error = error
        self.events = []

    def close(self):
        self.events.append(["close", same_thread()])

    def release_conn(self):
        self.events.append(["release", same_thread()])
        raise self.error


unconsumed_error = BaseException("unconsumed release failed")
unconsumed_raw = ReleaseFailure(unconsumed_error)
unconsumed = Response()
unconsumed.raw = unconsumed_raw
unconsumed_failure = capture(
    lambda: response_close_call(unconsumed)
)

consumed_error = BaseException("consumed release failed")
consumed_raw = ReleaseFailure(consumed_error)
consumed = Response()
consumed.raw = consumed_raw
consumed._content_consumed = True
consumed_failure = capture(
    lambda: response_close_call(consumed)
)

class NonCallableRelease:
    def __init__(self):
        self.events = []
        self.release_conn = object()

    def close(self):
        self.events.append(["close", same_thread()])


noncallable_raw = NonCallableRelease()
noncallable = Response()
noncallable.raw = noncallable_raw
noncallable_failure = capture(
    lambda: response_close_call(noncallable)
)

result = {
    "unconsumed": {
        "record": unconsumed_failure["exception"],
        "identity": unconsumed_failure["error"] is unconsumed_error,
        "events": unconsumed_raw.events,
    },
    "consumed": {
        "record": consumed_failure["exception"],
        "identity": consumed_failure["error"] is consumed_error,
        "events": consumed_raw.events,
    },
    "noncallable": {
        "record": noncallable_failure["exception"],
        "events": noncallable_raw.events,
    },
}
"""
    )

    assert state["unconsumed"] == {
        "record": {
            "type": ["builtins", "BaseException"],
            "args": ["unconsumed release failed"],
        },
        "identity": True,
        "events": [["close", True], ["release", True]],
    }
    assert state["consumed"] == {
        "record": {
            "type": ["builtins", "BaseException"],
            "args": ["consumed release failed"],
        },
        "identity": True,
        "events": [["release", True]],
    }
    assert state["noncallable"] == {
        "record": {
            "type": ["builtins", "TypeError"],
            "args": ["'object' object is not callable"],
        },
        "events": [["close", True]],
    }

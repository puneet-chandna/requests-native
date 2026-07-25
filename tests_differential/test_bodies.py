from __future__ import annotations

import ast
from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case

_TRIAL_HELPERS = """
import threading

from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None

ENTRY_THREAD = threading.get_ident()


_BODY_TRIAL_SYMBOLS = {
    "_prepare_body_trial",
    "_prepare_content_length_trial",
    "_rewind_body_trial",
    "_body_stream_collect_trial",
    "_body_stream_cancel_before_poll_trial",
    "_body_stream_cancel_trial",
    "_body_stream_poll_state_trial",
    "_body_stream_disconnect_trial",
    "_body_fields_snapshot",
}


def is_missing_body_trial(error):
    return (
        _requests_rust is not None
        and isinstance(error, AttributeError)
        and getattr(error, "obj", None) is _requests_rust
        and getattr(error, "name", None) in _BODY_TRIAL_SYMBOLS
    )


def new_subject(method="POST", headers=None):
    subject = PreparedRequest()
    subject.method = method
    subject.headers = CaseInsensitiveDict(headers or {})
    return subject


def prepare_body_call(subject, data=None, files=None, json_value=None):
    if _requests_rust is not None:
        return _requests_rust._prepare_body_trial(
            subject, data, files, json_value
        )
    return subject.prepare_body(data, files, json_value)


def prepare_content_length_call(subject, body):
    if _requests_rust is not None:
        return _requests_rust._prepare_content_length_trial(subject, body)
    return subject.prepare_content_length(body)


def rewind_body_call(subject):
    if _requests_rust is not None:
        return _requests_rust._rewind_body_trial(subject)
    from requests.utils import rewind_body

    return rewind_body(subject)


def collect_body_call(subject, limit):
    if _requests_rust is not None:
        return _requests_rust._body_stream_collect_trial(subject, limit)

    from urllib3.util.request import body_to_chunks

    selected = body_to_chunks(
        subject.body,
        method=subject.method or "POST",
        blocksize=4,
    ).chunks
    if selected is None:
        return []
    chunks = []
    for chunk in selected:
        if not chunk:
            continue
        if isinstance(chunk, str):
            chunk = chunk.encode("utf-8")
        elif not isinstance(chunk, bytes):
            try:
                chunk = bytes(memoryview(chunk))
            except TypeError:
                len(chunk)
                raise
        chunks.append(bytes(chunk))
        if len(chunks) == limit:
            break
    return chunks


def cancel_body_call(subject, error):
    if _requests_rust is not None:
        return _requests_rust._body_stream_cancel_trial(subject, error)
    next(iter(subject.body))
    raise error


def cancel_body_before_poll_call(subject, error):
    if _requests_rust is not None:
        return _requests_rust._body_stream_cancel_before_poll_trial(
            subject, error
        )
    raise error


def poll_state_call(subject):
    if _requests_rust is not None:
        return _requests_rust._body_stream_poll_state_trial(subject)
    return {
        "first_poll_pending": True,
        "second_poll_pending": True,
        "queued_actions": 1,
    }


def disconnect_body_call(subject, mode):
    if _requests_rust is not None:
        return _requests_rust._body_stream_disconnect_trial(subject, mode)
    return mode


def value_record(value):
    value_type = type(value)
    if value is None:
        payload = None
    elif isinstance(value, bytes):
        payload = ["bytes", bytes(value).hex()]
    elif isinstance(value, str):
        payload = ["str", value]
    else:
        payload = ["opaque", value_type.__module__, value_type.__qualname__]
    return {
        "type": [value_type.__module__, value_type.__qualname__],
        "payload": payload,
    }


def local_body_fields_snapshot(subject):
    position = subject._body_position
    return {
        "method": subject.method,
        "headers": list(subject.headers.items()),
        "body": value_record(subject.body),
        "position": value_record(position),
    }


def body_fields_snapshot(subject):
    if _requests_rust is not None:
        return _requests_rust._body_fields_snapshot(subject)
    return local_body_fields_snapshot(subject)


def exception_record(error):
    error_type = type(error)
    return {
        "type": [error_type.__module__, error_type.__qualname__],
        "args": [
            value_record(value)
            if not isinstance(value, (bool, int, float, str, bytes, type(None)))
            else value
            for value in error.args
        ],
    }


def capture(operation):
    try:
        returned = operation()
    except BaseException as error:
        if is_missing_body_trial(error):
            raise
        return {
            "returned": None,
            "exception": exception_record(error),
            "error": error,
        }
    return {
        "returned": value_record(returned),
        "exception": None,
        "error": None,
    }
"""


def _run_matching(source: str):
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == ""
    return _normalize_literal(
        ast.literal_eval(oracle.observations["result"]["repr"])
    )


def _normalize_literal(value):
    if isinstance(value, (list, tuple)):
        return [_normalize_literal(item) for item in value]
    if isinstance(value, dict):
        return {key: _normalize_literal(item) for key, item in value.items()}
    return value


def test_bytes_text_form_json_precedence_and_headers() -> None:
    state = _run_matching(
        """
cases = []

def add(label, subject, data=None, files=None, json_value=None):
    outcome = capture(
        lambda: prepare_body_call(subject, data, files, json_value)
    )
    cases.append({
        "label": label,
        "outcome": {
            "returned": outcome["returned"],
            "exception": outcome["exception"],
        },
        "state": body_fields_snapshot(subject),
        "body_is_input": subject.body is data,
    })


add("get-none", new_subject("GET"))
add("post-none", new_subject("POST"))
raw_bytes = b"raw"
add("bytes", new_subject(), raw_bytes)
raw_text = "snowman \\u2603"
add("text", new_subject(), raw_text)
add(
    "form",
    new_subject(),
    [("a", "one"), ("a", "two words"), ("drop", None), (b"raw", b"\\xff")],
)
add("json", new_subject(), {}, json_value={"life": 42})
add(
    "data-wins",
    new_subject(),
    {"data": "yes"},
    json_value={"ignored": True},
)
add(
    "preserve-headers",
    new_subject("POST", {"Content-Type": "caller", "Content-Length": "caller"}),
)
result = {"cases": cases}
"""
    )

    by_label = {case["label"]: case for case in state["cases"]}
    assert by_label["get-none"]["state"]["headers"] == []
    assert by_label["post-none"]["state"]["headers"] == [["Content-Length", "0"]]
    assert by_label["bytes"]["body_is_input"] is True
    assert by_label["bytes"]["state"]["headers"] == [["Content-Length", "3"]]
    assert by_label["text"]["body_is_input"] is True
    assert by_label["text"]["state"]["headers"] == [["Content-Length", "11"]]
    assert by_label["form"]["state"]["body"]["payload"] == [
        "str",
        "a=one&a=two+words&raw=%FF",
    ]
    assert by_label["form"]["state"]["headers"] == [
        ["Content-Length", "25"],
        ["Content-Type", "application/x-www-form-urlencoded"],
    ]
    assert by_label["json"]["state"]["body"]["payload"] == [
        "bytes",
        b'{"life": 42}'.hex(),
    ]
    assert by_label["json"]["state"]["headers"] == [
        ["Content-Length", "12"],
        ["Content-Type", "application/json"],
    ]
    assert by_label["data-wins"]["state"]["body"]["payload"] == [
        "str",
        "data=yes",
    ]
    assert by_label["preserve-headers"]["state"]["headers"] == [
        ["Content-Type", "caller"],
        ["Content-Length", "caller"],
    ]


def test_json_uses_selected_module_and_preserves_errors() -> None:
    state = _run_matching(
        """
import requests.models as models


class JsonFailure(ValueError):
    pass


class OtherFailure(BaseException):
    pass


value_error = JsonFailure("bad json")
other_error = OtherFailure("other")


class SelectedJson:
    def __init__(self):
        self.mode = "text"

    def dumps(self, value, **kwargs):
        side_effects.append([
            "dumps",
            self.mode,
            value,
            kwargs,
            threading.get_ident() == ENTRY_THREAD,
        ])
        if self.mode == "value-error":
            raise value_error
        if self.mode == "other-error":
            raise other_error
        if self.mode == "bytes":
            return b"selected-bytes"
        return "selected-text"


selected = SelectedJson()
original = models.complexjson
models.complexjson = selected
try:
    rows = []
    for mode in ("text", "bytes", "value-error", "other-error"):
        selected.mode = mode
        subject = new_subject()
        outcome = capture(
            lambda subject=subject: prepare_body_call(
                subject, None, None, {"mode": mode}
            )
        )
        error = outcome["error"]
        rows.append({
            "mode": mode,
            "state": body_fields_snapshot(subject),
            "exception": outcome["exception"],
            "value_error_is_original": (
                error is not None
                and type(error).__name__ == "InvalidJSONError"
                and error.args[0] is value_error
            ),
            "request_is_subject": (
                error is not None
                and getattr(error, "request", None) is subject
            ),
            "other_error_is_original": error is other_error,
        })
finally:
    models.complexjson = original

result = {
    "rows": rows,
    "effects": list(side_effects),
}
"""
    )

    rows = {row["mode"]: row for row in state["rows"]}
    assert rows["text"]["state"]["body"]["payload"] == [
        "bytes",
        b"selected-text".hex(),
    ]
    assert rows["bytes"]["state"]["body"]["payload"] == [
        "bytes",
        b"selected-bytes".hex(),
    ]
    assert rows["value-error"]["value_error_is_original"] is True
    assert rows["value-error"]["request_is_subject"] is True
    assert rows["other-error"]["other_error_is_original"] is True
    assert all(effect[-1] is True for effect in state["effects"])


def test_multipart_is_eager_ordered_and_never_closes_inputs() -> None:
    state = _run_matching(
        """
class File:
    def __init__(self, label, content, failure=None):
        self.label = label
        self.content = content
        self.failure = failure

    def read(self):
        side_effects.append(["read", self.label])
        if self.failure is not None:
            raise self.failure
        return self.content

    def close(self):
        side_effects.append(["close", self.label])


first = File("first", b"FIRST")
second = File("second", b"SECOND")
subject = new_subject()
outcome = capture(
    lambda: prepare_body_call(
        subject,
        [("field", "one"), ("field", "two")],
        [
            (
                "upload",
                (
                    "first.txt",
                    first,
                    "text/plain",
                    {"X-Part": "one"},
                ),
            ),
            ("skip", None),
            ("raw", ("raw.bin", bytearray(b"RAW"))),
            ("upload2", ("second.txt", second)),
        ],
        None,
    )
)
content_type = subject.headers["Content-Type"]
boundary = content_type.split("boundary=", 1)[1]
normalized_body = subject.body.replace(
    boundary.encode("ascii"), b"<BOUNDARY>"
)

failure = RuntimeError("second read failed")
good = File("good", b"GOOD")
bad = File("bad", b"", failure)
failed_subject = new_subject()
failed_subject.body = "before"
failed = capture(
    lambda: prepare_body_call(
        failed_subject,
        None,
        [("good", good), ("bad", bad)],
        None,
    )
)

result = {
    "success_exception": outcome["exception"],
    "headers": [
        [name, "<BOUNDARY>" if name.lower() == "content-type" else value]
        if name.lower() == "content-type"
        else [name, value]
        for name, value in subject.headers.items()
    ],
    "body": normalized_body,
    "failure": failed["exception"],
    "failure_is_original": failed["error"] is failure,
    "failed_body": failed_subject.body,
    "failed_headers": list(failed_subject.headers.items()),
    "effects": list(side_effects),
}
"""
    )

    assert state["success_exception"] is None
    assert b'name="field"\r\n\r\none' in state["body"]
    assert b'name="field"\r\n\r\ntwo' in state["body"]
    assert b'filename="first.txt"' in state["body"]
    assert b"X-Part: one" in state["body"]
    assert b'filename="raw.bin"' in state["body"]
    assert b"RAW" in state["body"]
    assert b'filename="second.txt"' in state["body"]
    assert b'name="skip"' not in state["body"]
    assert state["failure_is_original"] is True
    assert state["failed_body"] == "before"
    assert state["failed_headers"] == []
    assert state["effects"] == [
        ["read", "first"],
        ["read", "second"],
        ["read", "good"],
        ["read", "bad"],
    ]


def test_stream_classification_length_and_preparation_laziness() -> None:
    state = _run_matching(
        """
from collections.abc import Iterator
from io import BytesIO


class Cursor(int):
    pass


cursor = Cursor(2)


class ReadableIterable:
    def __bool__(self):
        side_effects.append("known-bool")
        return True

    def __iter__(self):
        side_effects.append("known-iter")
        return self

    def __next__(self):
        side_effects.append("known-next")
        raise StopIteration

    def __len__(self):
        side_effects.append("known-len")
        return 6

    def tell(self):
        side_effects.append("known-tell")
        return cursor

    def read(self, size=-1):
        side_effects.append(["known-read", size])
        return b""


class Unknown(Iterator):
    def __next__(self):
        side_effects.append("unknown-next")
        raise StopIteration


class ReadOnly:
    def __bool__(self):
        side_effects.append("read-only-bool")
        return True

    def __len__(self):
        side_effects.append("read-only-len")
        return 4

    def read(self, size=-1):
        side_effects.append(["read-only-read", size])
        return b"data"


known = ReadableIterable()
known_subject = new_subject()
prepare_body_call(known_subject, known, None, None)

unknown = Unknown()
unknown_subject = new_subject()
prepare_body_call(unknown_subject, unknown, None, None)

empty = BytesIO(b"")
empty_subject = new_subject()
prepare_body_call(empty_subject, empty, None, None)

read_only = ReadOnly()
read_only_subject = new_subject()
prepare_body_call(read_only_subject, read_only, None, None)

result = {
    "known": body_fields_snapshot(known_subject),
    "known_body_identity": known_subject.body is known,
    "known_cursor_identity": known_subject._body_position is cursor,
    "unknown": body_fields_snapshot(unknown_subject),
    "unknown_body_identity": unknown_subject.body is unknown,
    "empty": body_fields_snapshot(empty_subject),
    "empty_body_identity": empty_subject.body is empty,
    "read_only": body_fields_snapshot(read_only_subject),
    "read_only_body_identity": read_only_subject.body is read_only,
    "events": list(side_effects),
}
"""
    )

    assert state["known"]["headers"] == [["Content-Length", "4"]]
    assert state["known_body_identity"] is True
    assert state["known_cursor_identity"] is True
    assert state["unknown"]["headers"] == [["Transfer-Encoding", "chunked"]]
    assert state["unknown_body_identity"] is True
    assert state["empty"]["headers"] == [["Transfer-Encoding", "chunked"]]
    assert state["empty"]["position"]["payload"] == [
        "opaque",
        "builtins",
        "int",
    ]
    assert state["read_only"]["headers"] == [["Content-Length", "4"]]
    assert state["read_only"]["position"]["payload"] is None
    assert state["read_only_body_identity"] is True
    assert state["events"] == [
        "known-bool",
        "known-len",
        "known-tell",
        "known-tell",
        "read-only-bool",
        "read-only-bool",
        "read-only-len",
    ]


def test_stream_adapter_runs_protocols_on_origin_thread_and_preserves_errors() -> None:
    state = _run_matching(
        """
class Reader:
    def __init__(self):
        self.chunks = [b"read", "snow \\u2603", b"", b"done", b""]
        self.nested = False

    def read(self, size):
        if not self.nested:
            self.nested = True
            if _requests_rust is None:
                nested = "nested"
            else:
                report = _requests_rust._runtime_nested_probe()
                assert report["entry_thread"] == report["nested_action_thread"]
                assert (
                    report["entry_interpreter"]
                    == report["nested_action_interpreter"]
                )
                nested = report["value"]
            side_effects.append(["nested", nested])
        side_effects.append(["read", size, threading.get_ident() == ENTRY_THREAD])
        return self.chunks.pop(0)


class IterableBody:
    def __init__(self, chunks):
        self.chunks = iter(chunks)

    def __iter__(self):
        side_effects.append(["iter", threading.get_ident() == ENTRY_THREAD])
        return self.chunks


class BodyFailure(BaseException):
    pass


original = BodyFailure("body failed")


class FailingBody:
    def __iter__(self):
        return self

    def __next__(self):
        side_effects.append([
            "failing-next",
            threading.get_ident() == ENTRY_THREAD,
        ])
        raise original


reader_subject = new_subject()
reader_subject.body = Reader()
reader = collect_body_call(reader_subject, 3)

iter_subject = new_subject()
iter_subject.body = IterableBody([b"one", "", "two"])
iterator = collect_body_call(iter_subject, 2)

failure_subject = new_subject()
failure_subject.body = FailingBody()
failed = capture(lambda: collect_body_call(failure_subject, 1))

result = {
    "reader": [chunk.hex() for chunk in reader],
    "iterator": [chunk.hex() for chunk in iterator],
    "failure": failed["exception"],
    "failure_is_original": failed["error"] is original,
    "effects": list(side_effects),
}
"""
    )

    assert state["reader"] == [b"read".hex(), "snow ☃".encode().hex()]
    assert state["iterator"] == [b"one".hex(), b"two".hex()]
    assert state["failure_is_original"] is True
    assert ["nested", "nested"] in state["effects"]
    assert all(
        event[-1] is True
        for event in state["effects"]
        if event[0] != "nested"
    )


def test_bad_iterator_chunk_matches_frozen_send_boundary() -> None:
    state = _run_matching(
        """
class BadIterator:
    def __next__(self):
        side_effects.append("bad-next")
        return 7

    def __iter__(self):
        return self


class BadChunks:
    def __iter__(self):
        side_effects.append("bad-iter")
        return BadIterator()


subject = new_subject()
subject.body = BadChunks()
outcome = capture(lambda: collect_body_call(subject, 1))
result = {
    "exception": outcome["exception"],
    "events": list(side_effects),
}
"""
    )

    assert state["exception"] == {
        "type": ["builtins", "TypeError"],
        "args": ["object of type 'int' has no len()"],
    }
    assert state["events"] == ["bad-iter", "bad-next"]


def test_body_position_and_redirect_rewind_preserve_exact_objects() -> None:
    state = _run_matching(
        """
class Cursor(int):
    pass


class Opaque:
    pass


class Body:
    def __init__(self, label, position, tell_error=None, seek_error=None):
        self.label = label
        self.position = position
        self.tell_error = tell_error
        self.seek_error = seek_error

    def __iter__(self):
        return self

    def __next__(self):
        raise StopIteration

    def __len__(self):
        return 10

    def tell(self):
        side_effects.append(["tell", self.label])
        if self.tell_error is not None:
            raise self.tell_error
        return self.position

    def seek(self, position):
        side_effects.append([
            "seek",
            self.label,
            position is self.position,
            value_record(position),
        ])
        if self.seek_error is not None:
            raise self.seek_error


rows = []
for label, position in (
    ("bool", True),
    ("negative", -3),
    ("huge", 10 ** 80),
    ("subclass", Cursor(4)),
):
    body = Body(label, position)
    subject = new_subject()
    prepare_body_call(subject, body, None, None)
    outcome = capture(lambda subject=subject: rewind_body_call(subject))
    rows.append({
        "label": label,
        "position_identity": subject._body_position is position,
        "exception": outcome["exception"],
    })

opaque = Opaque()
opaque_body = Body("opaque", opaque)
opaque_subject = new_subject()
prepare_body_call(opaque_subject, opaque_body, None, None)
opaque_outcome = capture(lambda: rewind_body_call(opaque_subject))

tell_body = Body("tell-failed", 0, tell_error=OSError("tell failed"))
tell_subject = new_subject()
prepare_body_call(tell_subject, tell_body, None, None)
tell_outcome = capture(lambda: rewind_body_call(tell_subject))

seek_oserror = OSError("seek failed")
seek_body = Body("seek-oserror", 0, seek_error=seek_oserror)
seek_subject = new_subject()
prepare_body_call(seek_subject, seek_body, None, None)
seek_outcome = capture(lambda: rewind_body_call(seek_subject))

seek_other = RuntimeError("seek other")
other_body = Body("seek-other", 0, seek_error=seek_other)
other_subject = new_subject()
prepare_body_call(other_subject, other_body, None, None)
other_outcome = capture(lambda: rewind_body_call(other_subject))

result = {
    "rows": rows,
    "opaque_identity": opaque_subject._body_position is opaque,
    "opaque": opaque_outcome["exception"],
    "tell_position_non_none": tell_subject._body_position is not None,
    "tell": tell_outcome["exception"],
    "seek": seek_outcome["exception"],
    "other": other_outcome["exception"],
    "other_is_original": other_outcome["error"] is seek_other,
    "events": list(side_effects),
}
"""
    )

    assert all(row["position_identity"] for row in state["rows"])
    assert all(row["exception"] is None for row in state["rows"])
    assert state["opaque_identity"] is True
    assert state["opaque"]["type"] == [
        "requests.exceptions",
        "UnrewindableBodyError",
    ]
    assert state["opaque"]["args"] == [
        "Unable to rewind request body for redirect."
    ]
    assert state["tell_position_non_none"] is True
    assert state["tell"]["args"] == [
        "Unable to rewind request body for redirect."
    ]
    assert state["seek"]["args"] == [
        "An error occurred when rewinding request body for redirect."
    ]
    assert state["other_is_original"] is True


def test_digest_cursor_characterization_and_cancellation_ownership() -> None:
    state = _run_matching(
        """
import gc

from requests.auth import HTTPDigestAuth
from requests.models import Response


class Cursor:
    pass


cursor = Cursor()


class DigestBody:
    def tell(self):
        side_effects.append([
            "digest-tell",
            threading.get_ident() == ENTRY_THREAD,
        ])
        return cursor

    def seek(self, position):
        side_effects.append([
            "digest-seek",
            position is cursor,
            threading.get_ident() == ENTRY_THREAD,
        ])


digest_subject = new_subject()
digest_subject.body = DigestBody()
auth = HTTPDigestAuth("user", "pass")
auth(digest_subject)
response = Response()
response.status_code = 401
response.request = digest_subject
response.headers = CaseInsensitiveDict()
auth.handle_401(response)


class Cancelled(BaseException):
    pass


original = Cancelled("cancel")


class TrackedBody:
    def __iter__(self):
        return self

    def __next__(self):
        side_effects.append([
            "cancel-next",
            threading.get_ident() == ENTRY_THREAD,
        ])
        return b"first"

    def close(self):
        side_effects.append([
            "cancel-close",
            threading.get_ident() == ENTRY_THREAD,
        ])

    def __del__(self):
        side_effects.append([
            "cancel-del",
            threading.get_ident() == ENTRY_THREAD,
        ])


tracked = TrackedBody()
cancel_subject = new_subject()
cancel_subject.body = tracked
cancelled = capture(lambda: cancel_body_call(cancel_subject, original))
still_owned = cancel_subject.body is tracked

poll_subject = new_subject()
poll_subject.body = tracked
poll_state = poll_state_call(poll_subject)

disconnects = []
for mode in ("action-receiver", "reply-receiver"):
    disconnect_subject = new_subject()
    disconnect_subject.body = tracked
    disconnects.append(disconnect_body_call(disconnect_subject, mode))

cancel_subject.body = None
poll_subject.body = None
disconnect_subject.body = None
tracked = None
gc.collect()

result = {
    "digest_cursor_is_exact": auth._thread_local.pos is cursor,
    "cancel_is_original": cancelled["error"] is original,
    "still_owned": still_owned,
    "poll_state": poll_state,
    "disconnects": disconnects,
    "effects": list(side_effects),
}
"""
    )

    assert state["digest_cursor_is_exact"] is True
    assert state["cancel_is_original"] is True
    assert state["still_owned"] is True
    assert state["poll_state"] == {
        "first_poll_pending": True,
        "second_poll_pending": True,
        "queued_actions": 1,
    }
    assert state["disconnects"] == ["action-receiver", "reply-receiver"]
    assert ["cancel-close", True] not in state["effects"]
    assert ["cancel-del", True] in state["effects"]
    assert all(event[-1] is True for event in state["effects"])


def test_each_body_bridge_race_releases_its_owner_once_on_origin() -> None:
    state = _run_matching(
        """
import gc


class Cancelled(BaseException):
    pass


class TrackedBody:
    def __init__(self, label):
        self.label = label

    def __iter__(self):
        return self

    def __next__(self):
        side_effects.append([
            "next",
            self.label,
            threading.get_ident() == ENTRY_THREAD,
        ])
        return b"chunk"

    def close(self):
        side_effects.append([
            "close",
            self.label,
            threading.get_ident() == ENTRY_THREAD,
        ])

    def __del__(self):
        side_effects.append([
            "del",
            self.label,
            threading.get_ident() == ENTRY_THREAD,
        ])


def exercise(label, operation):
    subject = new_subject()
    subject.body = TrackedBody(label)
    outcome = capture(lambda: operation(subject))
    subject.body = None
    del subject
    gc.collect()
    return outcome


rows = []
rows.append([
    "failed-send",
    exercise(
        "failed-send",
        lambda subject: disconnect_body_call(subject, "action-receiver"),
    )["exception"],
])
rows.append([
    "queued-action",
    exercise("queued-action", poll_state_call)["exception"],
])
rows.append([
    "reply-drop",
    exercise(
        "reply-drop",
        lambda subject: disconnect_body_call(subject, "reply-receiver"),
    )["exception"],
])

before = Cancelled("before first poll")
before_outcome = exercise(
    "cancel-before-poll",
    lambda subject: cancel_body_before_poll_call(subject, before),
)
rows.append([
    "cancel-before-poll",
    before_outcome["exception"],
    before_outcome["error"] is before,
])

after = Cancelled("after reply")
after_outcome = exercise(
    "cancel-after-reply",
    lambda subject: cancel_body_call(subject, after),
)
rows.append([
    "cancel-after-reply",
    after_outcome["exception"],
    after_outcome["error"] is after,
])

result = {
    "rows": rows,
    "effects": list(side_effects),
}
"""
    )

    assert state["rows"][0] == ["failed-send", None]
    assert state["rows"][1] == ["queued-action", None]
    assert state["rows"][2] == ["reply-drop", None]
    assert state["rows"][3][2] is True
    assert state["rows"][4][2] is True

    finalizers = [
        event for event in state["effects"] if event[0] == "del"
    ]
    assert finalizers == [
        ["del", "failed-send", True],
        ["del", "queued-action", True],
        ["del", "reply-drop", True],
        ["del", "cancel-before-poll", True],
        ["del", "cancel-after-reply", True],
    ]
    assert [
        event for event in state["effects"] if event[0] == "next"
    ] == [["next", "cancel-after-reply", True]]
    assert not [event for event in state["effects"] if event[0] == "close"]

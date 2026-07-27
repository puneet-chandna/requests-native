from __future__ import annotations

import ast
from pathlib import Path
from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case

_URLLIB3_126_WHEEL = Path("/tmp/urllib3-1.26.20-py2.py3-none-any.whl")

_HELPERS = """
import os
import pickle

import requests.exceptions as exceptions
import requests.models as models
import requests.utils as utils

try:
    from requests import _requests_rust
except ImportError as extension_error:
    if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
        raise RuntimeError("rewrite exception extension is unavailable") from extension_error
    _requests_rust = None


def type_record(value):
    value_type = type(value)
    return [value_type.__module__, value_type.__qualname__]


def exception_record(error):
    return {
        "type": type_record(error),
        "args": [
            value
            if isinstance(value, (bool, int, float, str, bytes, type(None)))
            else type_record(value)
            for value in error.args
        ],
        "mro": [
            [base.__module__, base.__qualname__]
            for base in type(error).__mro__
        ],
    }


def capture(operation):
    try:
        returned = operation()
    except BaseException as error:
        return {
            "returned": None,
            "record": exception_record(error),
            "error": error,
        }
    return {"returned": returned, "record": None, "error": None}


def oracle_error_mapping(module, site, kind, message, request, response, original):
    if site == "adapter_transport":
        names = {
            "connect_timeout": "ConnectTimeout",
            "read_timeout": "ReadTimeout",
            "proxy": "ProxyError",
            "tls": "SSLError",
            "handshake": "SSLError",
            "invalid_url": "InvalidURL",
        }
        target = getattr(module, names.get(kind, "ConnectionError"))
        raise target(message, request=request)
    if site == "adapter_retry":
        raise module.RetryError(message, request=request)
    if site == "response_stream":
        bindings = [
            ("ProtocolError", "ChunkedEncodingError"),
            ("DecodeError", "ContentDecodingError"),
            ("ReadTimeoutError", "ConnectionError"),
            ("SSLError", "RequestsSSLError"),
        ]
        try:
            raise original
        except BaseException:
            for source_name, target_name in bindings:
                if isinstance(original, getattr(module, source_name)):
                    raise getattr(module, target_name)(original)
            raise
    if site == "response_json":
        try:
            raise original
        except BaseException:
            if not isinstance(original, module.JSONDecodeError):
                raise
            raise module.RequestsJSONDecodeError(
                original.msg, original.doc, original.pos
            )
    if site == "url":
        target_name = "MissingSchema" if kind == "missing_schema" else "InvalidURL"
        raise getattr(module, target_name)(message)
    if site == "header":
        raise module.InvalidHeader(message)
    if site == "passthrough":
        raise original
    raise AssertionError(site)


def error_mapping_call(
    module,
    site,
    kind=None,
    message="",
    request=None,
    response=None,
    original=None,
):
    if _requests_rust is not None:
        return _requests_rust._error_mapping_trial(
            module, site, kind, message, request, response, original
        )
    return oracle_error_mapping(
        module, site, kind, message, request, response, original
    )
"""


def _run_matching(source: str) -> object:
    case = {"source": dedent(_HELPERS + "\n" + source)}
    return _run_case_matching(case)


def _run_plain_matching(source: str) -> object:
    case = {"source": dedent(source)}
    return _run_case_matching(case)


def _run_case_matching(case: dict[str, str]) -> object:
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)
    assert rewrite.stderr == oracle.stderr
    assert rewrite.observations == oracle.observations
    assert oracle.observations["exception"] is None
    return ast.literal_eval(oracle.observations["result"]["repr"])


def test_exception_and_warning_hierarchy_is_exact() -> None:
    state = _run_matching(
        """
names = [
    "RequestException",
    "InvalidJSONError",
    "JSONDecodeError",
    "HTTPError",
    "ConnectionError",
    "ProxyError",
    "SSLError",
    "Timeout",
    "ConnectTimeout",
    "ReadTimeout",
    "URLRequired",
    "TooManyRedirects",
    "MissingSchema",
    "InvalidSchema",
    "InvalidURL",
    "InvalidHeader",
    "InvalidProxyURL",
    "ChunkedEncodingError",
    "ContentDecodingError",
    "StreamConsumedError",
    "RetryError",
    "UnrewindableBodyError",
    "RequestsWarning",
    "FileModeWarning",
    "RequestsDependencyWarning",
]
result = {
    name: [
        [base.__module__, base.__qualname__]
        for base in getattr(exceptions, name).__mro__
    ]
    for name in names
}
"""
    )

    assert list(state) == [
        "RequestException",
        "InvalidJSONError",
        "JSONDecodeError",
        "HTTPError",
        "ConnectionError",
        "ProxyError",
        "SSLError",
        "Timeout",
        "ConnectTimeout",
        "ReadTimeout",
        "URLRequired",
        "TooManyRedirects",
        "MissingSchema",
        "InvalidSchema",
        "InvalidURL",
        "InvalidHeader",
        "InvalidProxyURL",
        "ChunkedEncodingError",
        "ContentDecodingError",
        "StreamConsumedError",
        "RetryError",
        "UnrewindableBodyError",
        "RequestsWarning",
        "FileModeWarning",
        "RequestsDependencyWarning",
    ]
    assert state["ConnectTimeout"][:5] == [
        ["requests.exceptions", "ConnectTimeout"],
        ["requests.exceptions", "ConnectionError"],
        ["requests.exceptions", "Timeout"],
        ["requests.exceptions", "RequestException"],
        ["builtins", "OSError"],
    ]
    assert state["ReadTimeout"][1] == ["requests.exceptions", "Timeout"]
    assert ["builtins", "ValueError"] in state["InvalidHeader"]
    assert ["urllib3.exceptions", "HTTPError"] in state["ContentDecodingError"]
    assert ["builtins", "DeprecationWarning"] in state["FileModeWarning"]


@pytest.mark.skipif(
    not _URLLIB3_126_WHEEL.is_file(),
    reason="pinned official urllib3 1.26.20 wheel is unavailable",
)
def test_content_decoding_error_mro_under_official_urllib3_126() -> None:
    source = f"""
import sys

sys.path.insert(0, {_URLLIB3_126_WHEEL.as_posix()!r})

import urllib3
from requests.exceptions import ContentDecodingError

result = {{
    "version": urllib3.__version__,
    "mro": [
        [base.__module__, base.__qualname__]
        for base in ContentDecodingError.__mro__
    ],
    "http_error_identity": (
        ContentDecodingError.__mro__[3] is urllib3.exceptions.HTTPError
    ),
}}
"""
    state = _run_plain_matching(source)

    assert state["version"] == "1.26.20"
    assert state["mro"][:6] == [
        ["requests.exceptions", "ContentDecodingError"],
        ["requests.exceptions", "RequestException"],
        ["builtins", "OSError"],
        ["urllib3.exceptions", "HTTPError"],
        ["builtins", "Exception"],
        ["builtins", "BaseException"],
    ]
    assert state["http_error_identity"] is True


def test_content_decoding_error_mro_under_urllib3_27() -> None:
    state = _run_matching(
        """
import urllib3

result = {
    "version": urllib3.__version__,
    "mro": [
        [base.__module__, base.__qualname__]
        for base in exceptions.ContentDecodingError.__mro__
    ],
    "http_error_identity": (
        exceptions.ContentDecodingError.__mro__[3]
        is urllib3.exceptions.HTTPError
    ),
}
"""
    )

    assert state["version"] == "2.7.0"
    assert state["mro"][:6] == [
        ["requests.exceptions", "ContentDecodingError"],
        ["requests.exceptions", "RequestException"],
        ["builtins", "OSError"],
        ["urllib3.exceptions", "HTTPError"],
        ["builtins", "Exception"],
        ["builtins", "BaseException"],
    ]
    assert state["http_error_identity"] is True


def test_request_exception_inference_falsey_and_failure_order() -> None:
    state = _run_matching(
        """
class FalseRequest:
    def __bool__(self):
        side_effects.append("request-bool")
        return False


class ResponseWithRequest:
    def __init__(self, request):
        self.request = request


class ExplodingResponse:
    @property
    def request(self):
        side_effects.append("response-request")
        raise BaseException("request property failed")


class ExplodingRequest:
    def __bool__(self):
        side_effects.append("request-bool-error")
        raise KeyboardInterrupt("request bool failed")


false_request = FalseRequest()
inferred_request = object()
response = ResponseWithRequest(inferred_request)
inferred = exceptions.RequestException(
    "inferred", request=false_request, response=response
)
property_failure = capture(
    lambda: exceptions.RequestException("property", response=ExplodingResponse())
)
bool_failure = capture(
    lambda: exceptions.RequestException(
        "bool", request=ExplodingRequest(), response=response
    )
)
unknown_kwarg = capture(
    lambda: exceptions.RequestException("unknown", unexpected=True)
)
result = {
    "inferred": {
        "request_is_response_request": inferred.request is inferred_request,
        "response_identity": inferred.response is response,
        "args": list(inferred.args),
    },
    "property_failure": {
        "record": property_failure["record"],
        "is_base_exception": type(property_failure["error"]) is BaseException,
    },
    "bool_failure": {
        "record": bool_failure["record"],
        "is_keyboard_interrupt": type(bool_failure["error"]) is KeyboardInterrupt,
    },
    "unknown_kwarg": unknown_kwarg["record"],
    "events": side_effects,
}
"""
    )

    assert state["inferred"] == {
        "request_is_response_request": True,
        "response_identity": True,
        "args": ["inferred"],
    }
    assert state["property_failure"]["record"]["type"] == [
        "builtins",
        "BaseException",
    ]
    assert state["property_failure"]["is_base_exception"] is True
    assert state["bool_failure"]["record"]["type"] == [
        "builtins",
        "KeyboardInterrupt",
    ]
    assert state["bool_failure"]["is_keyboard_interrupt"] is True
    assert state["unknown_kwarg"]["type"] == ["builtins", "TypeError"]
    assert state["events"] == [
        "request-bool",
        "response-request",
        "request-bool-error",
    ]


def test_request_exception_pickle_preserves_attached_graph() -> None:
    state = _run_matching(
        """
from types import SimpleNamespace


response = SimpleNamespace(request={"marker": "request"})
error = exceptions.RequestException("boom", 7, response=response)
restored = pickle.loads(pickle.dumps(error))
result = {
    "args": list(restored.args),
    "response_type": type_record(restored.response),
    "request": restored.request,
    "request_is_response_request": restored.request is restored.response.request,
}
"""
    )

    assert state == {
        "args": ["boom", 7],
        "response_type": ["types", "SimpleNamespace"],
        "request": {"marker": "request"},
        "request_is_response_request": True,
    }


def test_json_decode_error_constructor_reduce_and_pickle() -> None:
    state = _run_matching(
        """
request = {"marker": "request"}
response = {"marker": "response"}
error = exceptions.JSONDecodeError(
    "bad json", "document", 2, request=request, response=response
)
reduced = error.__reduce__()
restored = pickle.loads(pickle.dumps(error))
result = {
    "record": exception_record(error),
    "msg": error.msg,
    "doc": error.doc,
    "pos": error.pos,
    "lineno": error.lineno,
    "colno": error.colno,
    "request_identity": error.request is request,
    "response_identity": error.response is response,
    "reduce_callable": [
        reduced[0].__module__,
        reduced[0].__qualname__,
    ],
    "reduce_args": list(reduced[1]),
    "restored": {
        "record": exception_record(restored),
        "msg": restored.msg,
        "doc": restored.doc,
        "pos": restored.pos,
    },
}
"""
    )

    assert state["record"]["type"] == ["requests.exceptions", "JSONDecodeError"]
    assert state["record"]["args"] == ["bad json: line 1 column 3 (char 2)"]
    assert state["msg"] == "bad json"
    assert state["doc"] == "document"
    assert state["pos"] == 2
    assert state["lineno"] == 1
    assert state["colno"] == 3
    assert state["request_identity"] is True
    assert state["response_identity"] is True
    assert state["reduce_callable"] == ["requests.exceptions", "JSONDecodeError"]
    assert state["reduce_args"] == ["bad json", "document", 2]
    assert state["restored"]["msg"] == "bad json"
    assert state["restored"]["doc"] == "document"
    assert state["restored"]["pos"] == 2


def test_adapter_transport_and_retry_mapping_attach_request() -> None:
    state = _run_matching(
        """
request = object()
rows = []
for kind in [
    "connect_timeout",
    "read_timeout",
    "proxy",
    "tls",
    "handshake",
    "invalid_url",
    "connection",
]:
    caught = capture(
        lambda kind=kind: error_mapping_call(
            exceptions,
            "adapter_transport",
            kind,
            "transport failed",
            request=request,
        )
    )
    error = caught["error"]
    rows.append({
        "kind": kind,
        "record": caught["record"],
        "request_identity": error.request is request,
        "response_is_none": error.response is None,
        "context_is_none": error.__context__ is None,
    })
retry = capture(
    lambda: error_mapping_call(
        exceptions,
        "adapter_retry",
        "retry_exhausted",
        "too many 503 responses",
        request=request,
    )
)
result = {
    "rows": rows,
    "retry": {
        "record": retry["record"],
        "request_identity": retry["error"].request is request,
        "response_is_none": retry["error"].response is None,
    },
}
"""
    )

    assert [row["record"]["type"][1] for row in state["rows"]] == [
        "ConnectTimeout",
        "ReadTimeout",
        "ProxyError",
        "SSLError",
        "SSLError",
        "InvalidURL",
        "ConnectionError",
    ]
    assert all(row["record"]["args"] == ["transport failed"] for row in state["rows"])
    assert all(row["request_identity"] for row in state["rows"])
    assert all(row["response_is_none"] for row in state["rows"])
    assert all(row["context_is_none"] for row in state["rows"])
    assert state["retry"]["record"]["type"] == [
        "requests.exceptions",
        "RetryError",
    ]
    assert state["retry"]["record"]["args"] == ["too many 503 responses"]
    assert state["retry"]["request_identity"] is True
    assert state["retry"]["response_is_none"] is True


def test_adapter_mapper_uses_live_target_and_exact_constructor_shape() -> None:
    state = _run_matching(
        """
class DynamicTarget(Exception):
    def __new__(cls, *args, **kwargs):
        side_effects.append([
            "construct",
            list(args),
            sorted(kwargs),
            kwargs.get("request") is request,
        ])
        return super().__new__(cls)

    def __init__(self, *args, **kwargs):
        self.request = kwargs["request"]
        super().__init__(*args)


class ConstructorFailure(BaseException):
    pass


class ExplodingTarget:
    def __new__(cls, *args, **kwargs):
        side_effects.append([
            "explode",
            list(args),
            sorted(kwargs),
            kwargs.get("request") is request,
        ])
        raise constructor_failure


request = object()
constructor_failure = ConstructorFailure("constructor failed")
saved = exceptions.ConnectTimeout
try:
    exceptions.ConnectTimeout = DynamicTarget
    constructed = capture(
        lambda: error_mapping_call(
            exceptions,
            "adapter_transport",
            "connect_timeout",
            "connect failed",
            request=request,
        )
    )
    exceptions.ConnectTimeout = ExplodingTarget
    failed = capture(
        lambda: error_mapping_call(
            exceptions,
            "adapter_transport",
            "connect_timeout",
            "second failure",
            request=request,
        )
    )
finally:
    exceptions.ConnectTimeout = saved

result = {
    "constructed_type": type_record(constructed["error"]),
    "failed_identity": failed["error"] is constructor_failure,
    "failed_context_is_none": failed["error"].__context__ is None,
    "events": side_effects,
}
"""
    )

    assert state == {
        "constructed_type": ["__differential_case__", "DynamicTarget"],
        "failed_identity": True,
        "failed_context_is_none": True,
        "events": [
            ["construct", ["connect failed"], ["request"], True],
            ["explode", ["second failure"], ["request"], True],
        ],
    }


def test_response_stream_mapping_order_and_original_context() -> None:
    state = _run_matching(
        """
from urllib3.exceptions import DecodeError, ProtocolError, ReadTimeoutError, SSLError

originals = [
    ProtocolError("protocol failed"),
    DecodeError("decode failed"),
    ReadTimeoutError(None, "/resource", "timed out"),
    SSLError("ssl failed"),
]
rows = []
for original in originals:
    caught = capture(
        lambda original=original: error_mapping_call(
            models, "response_stream", original=original
        )
    )
    error = caught["error"]
    rows.append({
        "record": caught["record"],
        "argument_identity": error.args[0] is original,
        "context_identity": error.__context__ is original,
        "cause_is_none": error.__cause__ is None,
        "suppress_context": error.__suppress_context__,
    })
result = rows
"""
    )

    assert [row["record"]["type"][1] for row in state] == [
        "ChunkedEncodingError",
        "ContentDecodingError",
        "ConnectionError",
        "SSLError",
    ]
    assert all(row["argument_identity"] for row in state)
    assert all(row["context_identity"] for row in state)
    assert all(row["cause_is_none"] for row in state)
    assert not any(row["suppress_context"] for row in state)


def test_stream_mapper_uses_live_sources_targets_and_constructor_failures() -> None:
    state = _run_matching(
        """
class DynamicSource(Exception):
    pass


class Constructed(Exception):
    def __new__(cls, *args, **kwargs):
        side_effects.append([
            "construct",
            len(args),
            args[0] is original,
            sorted(kwargs),
        ])
        return super().__new__(cls)


class ConstructorFailure(BaseException):
    pass


class ExplodingTarget:
    def __new__(cls, *args, **kwargs):
        side_effects.append([
            "explode",
            len(args),
            args[0] is second_original,
            sorted(kwargs),
        ])
        raise constructor_failure


saved_source = models.ProtocolError
saved_target = models.ChunkedEncodingError
original = DynamicSource("dynamic")
second_original = DynamicSource("second")
constructor_failure = ConstructorFailure("constructor failed")
try:
    models.ProtocolError = DynamicSource
    models.ChunkedEncodingError = Constructed
    wrapped = capture(
        lambda: error_mapping_call(
            models, "response_stream", original=original
        )
    )
    models.ChunkedEncodingError = ExplodingTarget
    failed = capture(
        lambda: error_mapping_call(
            models, "response_stream", original=second_original
        )
    )
finally:
    models.ProtocolError = saved_source
    models.ChunkedEncodingError = saved_target

result = {
    "wrapped": {
        "type": type_record(wrapped["error"]),
        "argument_identity": wrapped["error"].args[0] is original,
        "context_identity": wrapped["error"].__context__ is original,
    },
    "failed": {
        "is_constructor_failure": failed["error"] is constructor_failure,
        "context_identity": failed["error"].__context__ is second_original,
        "cause_is_none": failed["error"].__cause__ is None,
        "suppress_context": failed["error"].__suppress_context__,
    },
    "events": side_effects,
}
"""
    )

    assert state["wrapped"] == {
        "type": ["__differential_case__", "Constructed"],
        "argument_identity": True,
        "context_identity": True,
    }
    assert state["failed"] == {
        "is_constructor_failure": True,
        "context_identity": True,
        "cause_is_none": True,
        "suppress_context": False,
    }
    assert state["events"] == [
        ["construct", 1, True, []],
        ["explode", 1, True, []],
    ]


def test_json_mapper_uses_live_globals_and_preserves_passthrough() -> None:
    state = _run_matching(
        """
class SelectedDecodeError(ValueError):
    def __init__(self, msg, doc, pos):
        self.msg = msg
        self.doc = doc
        self.pos = pos
        super().__init__(msg, doc, pos)


class SelectedTarget(Exception):
    def __init__(self, msg, doc, pos):
        side_effects.append(["target", msg, doc, pos])
        self.msg = msg
        self.doc = doc
        self.pos = pos
        super().__init__(msg, doc, pos)


saved_source = models.JSONDecodeError
saved_target = models.RequestsJSONDecodeError
original = SelectedDecodeError("bad", "document", 3)
passthrough = BaseException("passthrough")
try:
    models.JSONDecodeError = SelectedDecodeError
    models.RequestsJSONDecodeError = SelectedTarget
    wrapped = capture(
        lambda: error_mapping_call(
            models, "response_json", original=original
        )
    )
    untouched = capture(
        lambda: error_mapping_call(
            models, "response_json", original=passthrough
        )
    )
finally:
    models.JSONDecodeError = saved_source
    models.RequestsJSONDecodeError = saved_target

result = {
    "wrapped": {
        "type": type_record(wrapped["error"]),
        "args": list(wrapped["error"].args),
        "context_identity": wrapped["error"].__context__ is original,
        "cause_is_none": wrapped["error"].__cause__ is None,
        "suppress_context": wrapped["error"].__suppress_context__,
    },
    "untouched": {
        "identity": untouched["error"] is passthrough,
        "context_is_none": passthrough.__context__ is None,
    },
    "events": side_effects,
}
"""
    )

    assert state["wrapped"] == {
        "type": ["__differential_case__", "SelectedTarget"],
        "args": ["bad", "document", 3],
        "context_identity": True,
        "cause_is_none": True,
        "suppress_context": False,
    }
    assert state["untouched"] == {
        "identity": True,
        "context_is_none": True,
    }
    assert state["events"] == [["target", "bad", "document", 3]]


def test_response_json_reads_error_fields_sequentially_at_the_real_call_site() -> None:
    state = _run_plain_matching(
        """
import os

import requests.models as models
from requests.models import Response

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


events = []
last_source = None
fail_at = None


class FieldFailure(Exception):
    pass


class ObservedDecodeError(models.JSONDecodeError):
    def __init__(self):
        super().__init__("bad", "document", 3)

    def observe(self, name, value):
        events.append(name)
        if fail_at == name:
            raise FieldFailure(name)
        return value

    @property
    def msg(self):
        return self.observe("msg", self._msg)

    @msg.setter
    def msg(self, value):
        self._msg = value

    @property
    def doc(self):
        return self.observe("doc", self._doc)

    @doc.setter
    def doc(self, value):
        self._doc = value

    @property
    def pos(self):
        return self.observe("pos", self._pos)

    @pos.setter
    def pos(self, value):
        self._pos = value


def loads(*args, **kwargs):
    global last_source
    last_source = ObservedDecodeError()
    raise last_source


def call_json(response):
    if _requests_rust is None:
        return response.json()
    return _requests_rust._response_json_trial(response, {})


saved_loads = models.complexjson.loads
records = []
try:
    models.complexjson.loads = loads
    for selected in ("msg", "doc", "pos"):
        fail_at = selected
        events.clear()
        response = Response()
        response._content = b"{}"
        response._content_consumed = True
        try:
            call_json(response)
        except BaseException as error:
            records.append(
                {
                    "field": selected,
                    "events": list(events),
                    "type": [type(error).__module__, type(error).__qualname__],
                    "args": list(error.args),
                    "context_identity": error.__context__ is last_source,
                    "cause_is_none": error.__cause__ is None,
                    "suppress_context": error.__suppress_context__,
                }
            )
        else:
            raise AssertionError("field failure was not propagated")
finally:
    models.complexjson.loads = saved_loads

result = records
"""
    )

    assert state == [
        {
            "field": "msg",
            "events": ["msg"],
            "type": ["__differential_case__", "FieldFailure"],
            "args": ["msg"],
            "context_identity": True,
            "cause_is_none": True,
            "suppress_context": False,
        },
        {
            "field": "doc",
            "events": ["msg", "doc"],
            "type": ["__differential_case__", "FieldFailure"],
            "args": ["doc"],
            "context_identity": True,
            "cause_is_none": True,
            "suppress_context": False,
        },
        {
            "field": "pos",
            "events": ["msg", "doc", "pos"],
            "type": ["__differential_case__", "FieldFailure"],
            "args": ["pos"],
            "context_identity": True,
            "cause_is_none": True,
            "suppress_context": False,
        },
    ]


def test_response_json_live_target_constructor_failure_keeps_context() -> None:
    state = _run_plain_matching(
        """
import requests.models as models
from requests.models import Response

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


events = []
source = models.JSONDecodeError("bad", "document", 3)


class ConstructorFailure(Exception):
    pass


failure = ConstructorFailure("constructor failed")


class ExplodingTarget:
    def __new__(cls, *args, **kwargs):
        events.append(["construct", list(args), sorted(kwargs)])
        raise failure


saved_loads = models.complexjson.loads
saved_target = models.RequestsJSONDecodeError


def loads(*args, **kwargs):
    models.RequestsJSONDecodeError = ExplodingTarget
    raise source


def call_json(response):
    if _requests_rust is None:
        return response.json()
    return _requests_rust._response_json_trial(response, {})


try:
    models.complexjson.loads = loads
    response = Response()
    response._content = b"{}"
    response._content_consumed = True
    try:
        call_json(response)
    except BaseException as error:
        result = {
            "identity": error is failure,
            "context_identity": error.__context__ is source,
            "cause_is_none": error.__cause__ is None,
            "suppress_context": error.__suppress_context__,
            "events": events,
        }
    else:
        raise AssertionError("target constructor failure was not propagated")
finally:
    models.complexjson.loads = saved_loads
    models.RequestsJSONDecodeError = saved_target
"""
    )

    assert state == {
        "identity": True,
        "context_identity": True,
        "cause_is_none": True,
        "suppress_context": False,
        "events": [["construct", ["bad", "document", 3], []]],
    }


def test_json_mapper_preserves_instance_check_and_constructor_failures() -> None:
    state = _run_matching(
        """
class InstanceFailure(BaseException):
    pass


class ConstructorFailure(BaseException):
    pass


instance_failure = InstanceFailure("instance check failed")
constructor_failure = ConstructorFailure("constructor failed")


class ExplodingMeta(type):
    def __instancecheck__(cls, value):
        side_effects.append(["instance-check", value is first_original])
        raise instance_failure


class ExplodingSource(metaclass=ExplodingMeta):
    pass


class SelectedSource(ValueError):
    def __init__(self, msg, doc, pos):
        self.msg = msg
        self.doc = doc
        self.pos = pos
        super().__init__(msg, doc, pos)


class ExplodingTarget:
    def __new__(cls, *args, **kwargs):
        side_effects.append(["construct", list(args), sorted(kwargs)])
        raise constructor_failure


saved_source = models.JSONDecodeError
saved_target = models.RequestsJSONDecodeError
first_original = ValueError("first")
second_original = SelectedSource("bad", "document", 4)
try:
    models.JSONDecodeError = ExplodingSource
    instance_failed = capture(
        lambda: error_mapping_call(
            models, "response_json", original=first_original
        )
    )
    models.JSONDecodeError = SelectedSource
    models.RequestsJSONDecodeError = ExplodingTarget
    constructor_failed = capture(
        lambda: error_mapping_call(
            models, "response_json", original=second_original
        )
    )
finally:
    models.JSONDecodeError = saved_source
    models.RequestsJSONDecodeError = saved_target

result = {
    "instance": {
        "identity": instance_failed["error"] is instance_failure,
        "context_identity": instance_failed["error"].__context__
        is first_original,
    },
    "constructor": {
        "identity": constructor_failed["error"] is constructor_failure,
        "context_identity": constructor_failed["error"].__context__
        is second_original,
    },
    "events": side_effects,
}
"""
    )

    assert state == {
        "instance": {
            "identity": True,
            "context_identity": True,
        },
        "constructor": {
            "identity": True,
            "context_identity": True,
        },
        "events": [
            ["instance-check", True],
            ["construct", ["bad", "document", 4], []],
        ],
    }


def test_url_and_header_mapping_use_live_targets() -> None:
    state = _run_matching(
        """
class DynamicMissing(Exception):
    pass


class DynamicInvalid(Exception):
    pass


class DynamicHeader(Exception):
    pass


saved = (
    models.MissingSchema,
    models.InvalidURL,
    utils.InvalidHeader,
)
try:
    models.MissingSchema = DynamicMissing
    models.InvalidURL = DynamicInvalid
    utils.InvalidHeader = DynamicHeader
    rows = [
        capture(
            lambda: error_mapping_call(
                models, "url", "missing_schema", "missing scheme"
            )
        ),
        capture(
            lambda: error_mapping_call(
                models, "url", "invalid_url", "invalid URL"
            )
        ),
            capture(
                lambda: error_mapping_call(
                    utils, "header", "invalid_header", "invalid header"
                )
            ),
    ]
finally:
    (
        models.MissingSchema,
        models.InvalidURL,
        utils.InvalidHeader,
    ) = saved

result = [
    {
        "record": row["record"],
        "context_is_none": row["error"].__context__ is None,
    }
    for row in rows
]
"""
    )

    assert [row["record"]["type"][1] for row in state] == [
        "DynamicMissing",
        "DynamicInvalid",
        "DynamicHeader",
    ]
    assert [row["record"]["args"] for row in state] == [
        ["missing scheme"],
        ["invalid URL"],
        ["invalid header"],
    ]
    assert all(row["context_is_none"] for row in state)


def test_user_base_exception_passthrough_preserves_identity_and_traceback() -> None:
    state = _run_matching(
        """
original = BaseException("user failed")
try:
    raise original
except BaseException as caught:
    original_traceback = caught.__traceback__

mapped = capture(
    lambda: error_mapping_call(
        exceptions, "passthrough", original=original
    )
)


def traceback_contains(traceback, target):
    while traceback is not None:
        if traceback is target:
            return True
        traceback = traceback.tb_next
    return False


result = {
    "identity": mapped["error"] is original,
    "traceback_retains_original": traceback_contains(
        mapped["error"].__traceback__, original_traceback
    ),
    "context_is_none": mapped["error"].__context__ is None,
    "cause_is_none": mapped["error"].__cause__ is None,
    "suppress_context": mapped["error"].__suppress_context__,
}
"""
    )

    assert state == {
        "identity": True,
        "traceback_retains_original": True,
        "context_is_none": True,
        "cause_is_none": True,
        "suppress_context": False,
    }


def test_verify_false_falls_back_before_visible_manager_effects() -> None:
    case = {
        "source": dedent(
            _HELPERS
            + """
from requests.adapters import HTTPAdapter
from requests.models import PreparedRequest


adapter = HTTPAdapter()
request = PreparedRequest()
request.prepare(method="GET", url="https://127.0.0.1:1/resource")
manager = adapter.poolmanager
before = list(manager.pools._container.items())
caught = capture(
    lambda: _requests_rust._adapter_send_trial(
        adapter,
        request,
        False,
        0.001,
        False,
        None,
        {},
    )
)
after = list(manager.pools._container.items())
result = {
    "returned_is_not_implemented": caught["returned"] is NotImplemented,
    "exception": caught["record"],
    "pool_items_unchanged": before == after,
    "proxy_manager": list(adapter.proxy_manager),
}
"""
        )
    }
    rewrite = run_rewrite_case(case)
    assert rewrite.observations["exception"] is None
    state = ast.literal_eval(rewrite.observations["result"]["repr"])
    assert state == {
        "returned_is_not_implemented": True,
        "exception": None,
        "pool_items_unchanged": True,
        "proxy_manager": [],
    }


def test_panic_boundary_is_stable_and_reusable() -> None:
    case = {
        "source": dedent(
            _HELPERS
            + """
first = capture(lambda: _requests_rust._panic_boundary_trial(True))
generation = _requests_rust._runtime_generation_trial()
second = capture(lambda: _requests_rust._panic_boundary_trial(False))
result = {
    "first": first["record"],
    "driver_generation": first["error"].driver_generation,
    "recovery_generation": first["error"].recovery_generation,
    "recovery_result": first["error"].recovery_result,
    "generation": generation,
    "second": second["returned"],
    "second_exception": second["record"],
}
"""
        )
    }
    rewrite = run_rewrite_case(case)
    assert rewrite.observations["exception"] is None
    state = ast.literal_eval(rewrite.observations["result"]["repr"])
    assert state == {
        "first": {
            "type": ["builtins", "RuntimeError"],
            "args": ["native requests worker stopped unexpectedly"],
            "mro": [
                ["builtins", "RuntimeError"],
                ["builtins", "Exception"],
                ["builtins", "BaseException"],
                ["builtins", "object"],
            ],
        },
        "driver_generation": state["generation"],
        "recovery_generation": state["generation"],
        "recovery_result": "ok",
        "generation": state["generation"],
        "second": "ok",
        "second_exception": None,
    }

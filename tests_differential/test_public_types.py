from __future__ import annotations

import ast
import re
from dataclasses import dataclass
from pathlib import Path
from textwrap import dedent
from types import MappingProxyType

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case


@dataclass(frozen=True)
class PublicTypeCase:
    case_id: str
    obligation: str
    source: str


_HELPERS = r"""
import copy
import gc
import inspect
import pickle
import re
import weakref
from collections import OrderedDict
from types import SimpleNamespace

import requests
import requests.adapters as adapters_module
import requests.models as models_module
import requests.sessions as sessions_module
from requests.adapters import BaseAdapter, HTTPAdapter
from requests.models import PreparedRequest, Request, Response
from requests.sessions import Session, SessionRedirectMixin

PUBLIC_TYPES = OrderedDict((
    ("Request", Request), ("PreparedRequest", PreparedRequest),
    ("Response", Response), ("SessionRedirectMixin", SessionRedirectMixin),
    ("Session", Session), ("BaseAdapter", BaseAdapter),
    ("HTTPAdapter", HTTPAdapter),
))

def type_name(value):
    cls = value if isinstance(value, type) else type(value)
    return [cls.__module__, cls.__qualname__]

def stable(value, seen=None):
    if value is None or type(value) in {bool, int, float, str, bytes}:
        return value
    seen = set() if seen is None else seen
    if id(value) in seen:
        return {"cycle": type_name(value)}
    seen.add(id(value))
    try:
        if isinstance(value, (list, tuple)):
            return [stable(item, seen) for item in value]
        if isinstance(value, dict):
            return [[stable(key, seen), stable(item, seen)] for key, item in value.items()]
        return {"type": type_name(value)}
    finally:
        seen.remove(id(value))

def error_record(callback, marker=None):
    try:
        returned = callback()
    except BaseException as error:
        return {
            "type": type_name(error), "arg_types": [type_name(arg) for arg in error.args],
            "identity": error is marker, "traceback": error.__traceback__ is not None,
            "cause": None if error.__cause__ is None else type_name(error.__cause__),
            "context": None if error.__context__ is None else type_name(error.__context__),
        }
    return {"returned": stable(returned)}

def semantic_repr(value):
    return re.sub(r" at 0x[0-9A-Fa-f]+(?=>)", " at 0x...", repr(value))

def fresh(name):
    if name == "Request": return Request()
    if name == "PreparedRequest": return PreparedRequest()
    if name == "Response":
        value = Response(); value._content = b"payload"; value._content_consumed = True; return value
    if name == "SessionRedirectMixin": return SessionRedirectMixin()
    if name == "Session": return Session()
    if name == "BaseAdapter": return BaseAdapter()
    if name == "HTTPAdapter": return HTTPAdapter()
    raise AssertionError(name)

def state_record(value):
    return [[name, stable(item)] for name, item in vars(value).items()]

def make_response(request, label):
    response = Response(); response.status_code = 200; response.url = request.url
    response.request = request; response.raw = SimpleNamespace(_original_response=None)
    response._content = label.encode(); response._content_consumed = True
    response.history = []; response.label = label
    return response
"""


_STRUCTURE = r"""
reviewed = OrderedDict((
    ("Request", ("__init__", "prepare", "__repr__")),
    ("PreparedRequest", ("__init__", "prepare", "copy", "prepare_method", "prepare_url", "prepare_headers", "prepare_body", "prepare_content_length", "prepare_auth", "prepare_cookies", "prepare_hooks")),
    ("Response", ("__init__", "__enter__", "__exit__", "__iter__", "__bool__", "iter_content", "iter_lines", "json", "raise_for_status", "close", "content", "text")),
    ("SessionRedirectMixin", ("get_redirect_target", "should_strip_auth", "resolve_redirects", "rebuild_auth", "rebuild_proxies", "rebuild_method")),
    ("Session", ("__init__", "__enter__", "__exit__", "__getstate__", "__setstate__", "prepare_request", "request", "get", "options", "head", "post", "put", "patch", "delete", "send", "merge_environment_settings", "get_adapter", "close", "mount", "__attrs__")),
    ("BaseAdapter", ("send", "close")),
    ("HTTPAdapter", ("__init__", "__getstate__", "__setstate__", "init_poolmanager", "proxy_manager_for", "cert_verify", "build_response", "build_connection_pool_key_attributes", "get_connection_with_tls_context", "get_connection", "close", "request_url", "add_headers", "proxy_headers", "send", "__attrs__")),
))
records = OrderedDict()
for name, cls in PUBLIC_TYPES.items():
    names = reviewed[name]
    records[name] = {
        "identity": [cls.__module__, cls.__qualname__],
        "metaclass": type_name(type(cls)), "bases": [type_name(x) for x in cls.__bases__],
        "mro": [type_name(x) for x in cls.__mro__],
        "requests_visible": [x for x in names if x in cls.__dict__],
        "descriptor_categories": [[x, type_name(cls.__dict__[x])] for x in names if x in cls.__dict__],
        "class_signature": str(inspect.signature(cls)),
        "call_signatures": [[x, str(inspect.signature(getattr(cls, x)))] for x in names if callable(getattr(cls, x, None))],
    }
records["aliases"] = {
    "root_models": [requests.Request is models_module.Request, requests.PreparedRequest is models_module.PreparedRequest, requests.Response is models_module.Response],
    "root_session": requests.Session is sessions_module.Session,
    "session_models": [sessions_module.Request is models_module.Request, sessions_module.PreparedRequest is models_module.PreparedRequest, sessions_module.Response is models_module.Response],
}
result = records
"""


_STATE = r"""
states = OrderedDict()
for name in PUBLIC_TYPES:
    first, second = fresh(name), fresh(name)
    states[name] = {
        "first": state_record(first), "second": state_record(second),
        "fresh_dict": first.__dict__ is not second.__dict__,
        "dict_type": type_name(first.__dict__), "repr_category": semantic_repr(first),
    }
invalid = OrderedDict((
    ("Request.__init__", lambda: Request(*range(20))),
    ("PreparedRequest.__init__", lambda: PreparedRequest(1)),
    ("Response.__init__", lambda: Response(1)),
    ("SessionRedirectMixin.__init__", lambda: SessionRedirectMixin(1)),
    ("Session.__init__", lambda: Session(1)),
    ("BaseAdapter.__init__", lambda: BaseAdapter(1)),
    ("HTTPAdapter.__init__", lambda: HTTPAdapter(1, 2, 3, 4, 5)),
    ("PreparedRequest.prepare_method", lambda: PreparedRequest().prepare_method()),
    ("Response.iter_content", lambda: Response().iter_content(1, False, "extra")),
    ("Session.mount", lambda: Session().mount("only-prefix")),
    ("Session.request", lambda: Session().request("GET")),
    ("BaseAdapter.send", lambda: BaseAdapter().send()),
    ("HTTPAdapter.send", lambda: HTTPAdapter().send()),
))
result = {"states": states, "invalid": [[name, error_record(call)] for name, call in invalid.items()]}
"""


_DYNAMIC = r"""
events = []
class UnhashableRequest(Request): __hash__ = None
class SlottedResponse(Response): __slots__ = ("slot_token",)
class DynamicPrepared(PreparedRequest):
    def __getattribute__(self, name):
        if name == "dynamic_read": events.append("getattribute"); return "read-value"
        return super().__getattribute__(name)
    def __setattr__(self, name, value):
        if name == "dynamic_write": events.append(["setattr", value])
        super().__setattr__(name, value)
class MethodDescriptor:
    def __get__(self, instance, owner):
        events.append(["descriptor-get", instance is not None, owner.__name__])
        if instance is None: return self
        def bound(value): events.append(["descriptor-call", value]); setattr(instance, "method", "descriptor:" + value)
        return bound

request = Request("GET", "http://example.test/"); request.arbitrary = object()
shadow_calls = []; request.prepare = lambda: shadow_calls.append("shadow") or "shadow-result"; shadow_result = request.prepare()
class RequestSubclass(Request):
    def prepare(self): events.append("subclass-prepare"); return "subclass-result"
slotted = SlottedResponse(); slotted.slot_token = "slot"; slotted.dynamic = "dict"
dynamic = DynamicPrepared(); dynamic.dynamic_write = "write-value"; dynamic_read = dynamic.dynamic_read

method = PreparedRequest.prepare_method; PreparedRequest.prepare_method = MethodDescriptor()
try:
    descriptor_subject = PreparedRequest(); descriptor_subject.prepare_method("value")
finally: PreparedRequest.prepare_method = method

code_events = []; code_marker = KeyboardInterrupt("code-stop"); original_code = method.__code__; missing = object()
old_events = models_module.__dict__.get("TASK17_CODE_EVENTS", missing); old_marker = models_module.__dict__.get("TASK17_CODE_MARKER", missing)
models_module.TASK17_CODE_EVENTS = code_events; models_module.TASK17_CODE_MARKER = code_marker
def replacement_code(self, method): TASK17_CODE_EVENTS.append(method); raise TASK17_CODE_MARKER
method.__code__ = replacement_code.__code__
try: code_error = error_record(lambda: PreparedRequest().prepare_method("PATCH"), code_marker)
finally:
    method.__code__ = original_code
    if old_events is missing: del models_module.TASK17_CODE_EVENTS
    else: models_module.TASK17_CODE_EVENTS = old_events
    if old_marker is missing: del models_module.TASK17_CODE_MARKER
    else: models_module.TASK17_CODE_MARKER = old_marker

default_events = []; original_defaults = Response.iter_content.__defaults__; Response.iter_content.__defaults__ = (7, False)
class Raw:
    def stream(self, chunk_size, decode_content=True): default_events.append([chunk_size, decode_content]); yield b"default"
default_subject = Response(); default_subject.status_code = 200; default_subject.raw = Raw()
try: default_content = list(default_subject.iter_content())
finally: Response.iter_content.__defaults__ = original_defaults

global_events = []; old_global = models_module.to_native_string
def live_to_native(value): global_events.append(value); return "GLOBAL:" + value
models_module.to_native_string = live_to_native
try: global_subject = PreparedRequest(); global_subject.prepare_method("get")
finally: models_module.to_native_string = old_global
result = {
    "arbitrary_identity": request.arbitrary is request.__dict__["arbitrary"], "shadow": [shadow_result, shadow_calls],
    "subclass": RequestSubclass().prepare(), "unhashable": error_record(lambda: hash(UnhashableRequest())),
    "slots": [slotted.slot_token, slotted.dynamic, sorted(slotted.__dict__)], "dynamic": [dynamic_read, events],
    "descriptor_method": descriptor_subject.method, "code": [code_events, code_error],
    "default": [default_events, default_content], "global": [global_events, global_subject.method],
}
"""


_COPY = r"""
rows = OrderedDict()
for name in PUBLIC_TYPES:
    original = fresh(name); original.user_value = [name]
    shallow, deep = copy.copy(original), copy.deepcopy(original)
    try: restored = pickle.loads(pickle.dumps(original, protocol=pickle.HIGHEST_PROTOCOL))
    except BaseException as error: pickle_row = {"error": type_name(error), "arg_types": [type_name(arg) for arg in error.args]}
    else:
        has_user = hasattr(restored, "user_value"); before = list(original.user_value)
        if has_user: restored.user_value.append("restored")
        pickle_row = {
            "type": type_name(restored), "dict_distinct": restored.__dict__ is not original.__dict__,
            "visible_state": state_record(restored), "has_user": has_user,
            "user_distinct": restored.user_value is not original.user_value if has_user else None,
            "original_unchanged": original.user_value == before,
        }
    rows[name] = {
        "shallow_type": type_name(shallow), "shallow_distinct": shallow is not original,
        "shallow_dict_distinct": shallow.__dict__ is not original.__dict__,
        "shallow_user_shared": getattr(shallow, "user_value", None) is original.user_value,
        "deep_type": type_name(deep), "deep_distinct": deep is not original,
        "deep_dict_distinct": deep.__dict__ is not original.__dict__,
        "deep_user_distinct": getattr(deep, "user_value", None) is not original.user_value,
        "pickle": pickle_row,
    }

def slotted_pickle(module, base, class_name):
    class Slotted(base): __slots__ = ("slot_value",)
    Slotted.__name__ = Slotted.__qualname__ = class_name; Slotted.__module__ = module.__name__
    missing = object(); previous = getattr(module, class_name, missing); setattr(module, class_name, Slotted)
    try:
        original = Slotted(); original.slot_value = ["slot"]; original.dict_value = ["dict"]
        try: restored = pickle.loads(pickle.dumps(original, protocol=pickle.HIGHEST_PROTOCOL))
        except BaseException as error: return {"error": type_name(error), "arg_types": [type_name(arg) for arg in error.args]}
        return {
            "type": type_name(restored), "slot": stable(getattr(restored, "slot_value", None)),
            "dict": stable(getattr(restored, "dict_value", None)),
            "slot_distinct": restored.slot_value is not original.slot_value if hasattr(restored, "slot_value") else None,
            "dict_distinct": restored.dict_value is not original.dict_value if hasattr(restored, "dict_value") else None,
        }
    finally:
        if previous is missing: delattr(module, class_name)
        else: setattr(module, class_name, previous)

callback_events = []; references = []
for name in PUBLIC_TYPES:
    value = fresh(name)
    first = weakref.ref(value, lambda ref, name=name: callback_events.append([name, "first"]))
    second = weakref.ref(value, lambda ref, name=name: callback_events.append([name, "second"]))
    references.append([name, first, second]); del value
gc.collect()
gc.collect()
gc.collect()
callback_phases = OrderedDict((name, []) for name in PUBLIC_TYPES)
for name, phase in callback_events:
    callback_phases[name].append(phase)
assert all(phases == ["second", "first"] for phases in callback_phases.values())
result = {
    "rows": rows,
    "slotted": [slotted_pickle(models_module, Request, "Task17SlottedRequest"), slotted_pickle(sessions_module, Session, "Task17SlottedSession"), slotted_pickle(adapters_module, HTTPAdapter, "Task17SlottedAdapter")],
    "callbacks": callback_phases,
    "collected": [[name, first() is None, second() is None] for name, first, second in references],
    "user_ref_distinct": [first is not second for _, first, second in references],
}
"""


_SESSION = r"""
events = []
class ScriptedAdapter(BaseAdapter):
    def __init__(self, name): self.name = name; self.sends = 0
    def send(self, request, **kwargs): self.sends += 1; events.append(["send", self.name, self.sends]); return make_response(request, self.name + ":" + str(self.sends))
    def close(self): events.append(["close", self.name])
shared = ScriptedAdapter("shared"); first, second = Session(), Session()
first.adapters = OrderedDict((("mock://long/", ScriptedAdapter("long")), ("mock://", shared)))
second.adapters = OrderedDict((("mock://", shared),)); first.mount("mock://longer/", ScriptedAdapter("longer"))
selected = [first.get_adapter(url).name for url in ("MoCk://LONGER/path", "mock://long/path", "mock://other/path")]
prepared = PreparedRequest(); prepared.prepare(method="GET", url="mock://other/path")
active = first.send(prepared, stream=True); first.close(); active_content = active.content; reused = first.send(prepared); peer = second.send(prepared)
context = Session(); context.adapters = OrderedDict((("mock://", ScriptedAdapter("context")),))
with context as entered: context_identity = entered is context
pickle_source = Session(); pickle_source.headers["X-Public"] = "state"; restored = pickle.loads(pickle.dumps(pickle_source)); restored.headers["X-Restored"] = "yes"
close_events = []; marker = KeyboardInterrupt("first-close")
class ErrorAdapter(BaseAdapter):
    def __init__(self, name, error=None): self.name, self.error = name, error
    def send(self, request, **kwargs): raise AssertionError("not sent")
    def close(self): close_events.append(self.name); (_ for _ in ()).throw(self.error) if self.error else None
broken = Session(); broken.adapters = OrderedDict((("one", ErrorAdapter("one", marker)), ("two", ErrorAdapter("two"))))
close_error = error_record(broken.close, marker)
result = {
    "attrs": list(Session.__attrs__), "constructor_state": state_record(Session()), "mount_order": list(first.adapters), "selected": selected,
    "active": [active.label, active_content], "reuse": reused.label, "peer": peer.label, "context": context_identity,
    "pickle": {"type": type_name(restored), "headers": list(restored.headers.items()), "source_independent": "X-Restored" not in pickle_source.headers, "dict_distinct": restored.__dict__ is not pickle_source.__dict__},
    "error": [close_events, close_error], "events": events,
}
"""


_PYTHON_COMPOSITION = r"""
events = []
python_composed_operations = {
    "model": ("copy",),
    "response": ("bool", "enter", "exit", "text"),
    "session": ("get", "options", "head", "post", "put", "patch", "delete"),
    "adapter": (
        "init_poolmanager", "proxy_manager_for", "cert_verify", "build_response",
        "build_connection_pool_key_attributes", "get_connection_with_tls_context",
        "get_connection", "request_url", "add_headers", "proxy_headers",
    ),
}

prepared = PreparedRequest()
prepared.prepare(method="GET", url="http://example.test/", headers={"X-Test": "one"})
prepared.body = ["body"]
prepared_copy = prepared.copy()
prepared_copy.headers["X-Copy"] = "yes"
prepared_copy.body.append("copy")

class BooleanResponse(Response):
    @property
    def ok(self):
        events.append("response-bool")
        return False

class TextResponse(Response):
    @property
    def content(self):
        events.append("response-text-content")
        return b"text-value"

class ContextResponse(Response):
    def close(self):
        events.append("response-context-close")

boolean_response = BooleanResponse()
boolean_value = bool(boolean_response)
text_response = TextResponse()
text_response.encoding = "utf-8"
text_value = text_response.text
context_response = ContextResponse()
with context_response as entered:
    context_identity = entered is context_response

session = Session()
verb_events = []
sentinel = object()
session.request = lambda *args, **kwargs: verb_events.append(
    [list(args), [[key, value] for key, value in kwargs.items()]]
) or sentinel
verb_results = [
    session.get("mock://get", params={"p": "1"}),
    session.options("mock://options"),
    session.head("mock://head"),
    session.post("mock://post", data="data", json="json"),
    session.put("mock://put", data="data"),
    session.patch("mock://patch", data="data"),
    session.delete("mock://delete"),
]

adapter_helpers = (
    "init_poolmanager", "proxy_manager_for", "cert_verify", "build_response",
    "build_connection_pool_key_attributes", "get_connection_with_tls_context",
    "get_connection", "request_url", "add_headers", "proxy_headers",
)
result = {
    "inventory": python_composed_operations,
    "prepared_copy": {
        "type": type_name(prepared_copy),
        "distinct": prepared_copy is not prepared,
        "headers_distinct": prepared_copy.headers is not prepared.headers,
        "source_headers_unchanged": "X-Copy" not in prepared.headers,
        "body_shallow_shared": prepared_copy.body is prepared.body,
        "body": list(prepared.body),
    },
    "response": [boolean_value, text_value, context_identity, events],
    "verbs": [all(value is sentinel for value in verb_results), verb_events],
    "adapter_helpers": [
        [name, name in HTTPAdapter.__dict__, type_name(HTTPAdapter.__dict__[name]), str(inspect.signature(getattr(HTTPAdapter, name)))]
        for name in adapter_helpers
    ],
}
"""


PUBLIC_TYPE_CASES = (
    PublicTypeCase("P01", "reviewed-type-structure", _STRUCTURE),
    PublicTypeCase("P02", "deterministic-state-invalid-calls", _STATE),
    PublicTypeCase("P03", "live-mutation-one-call-authority", _DYNAMIC),
    PublicTypeCase("P04", "copy-pickle-subclass-weakref-gc", _COPY),
    PublicTypeCase("P05", "session-visible-lifecycle", _SESSION),
    PublicTypeCase(
        "P06", "deliberate-python-composed-public-operations", _PYTHON_COMPOSITION
    ),
)


def case_source(case: PublicTypeCase) -> str:
    return dedent(_HELPERS + case.source)


@pytest.mark.parametrize("case", PUBLIC_TYPE_CASES, ids=lambda case: case.case_id)
def test_public_type_frozen_oracle_contract(case: PublicTypeCase) -> None:
    oracle = run_oracle_case({"source": case_source(case)})
    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""


def test_public_type_oracle_observations_are_repeatable() -> None:
    for case in PUBLIC_TYPE_CASES:
        payload = {"source": case_source(case)}
        assert run_oracle_case(payload) == run_oracle_case(payload), case.case_id


@pytest.mark.parametrize("case", PUBLIC_TYPE_CASES[1:], ids=lambda case: case.case_id)
def test_public_type_already_compatible_surface_matches_oracle(
    case: PublicTypeCase,
) -> None:
    payload = {"source": case_source(case)}
    assert run_rewrite_case(payload) == run_oracle_case(payload)


@pytest.mark.parametrize("case", PUBLIC_TYPE_CASES[:1], ids=lambda case: case.case_id)
def test_task17_red_public_type_structure_matches_oracle(case: PublicTypeCase) -> None:
    payload = {"source": case_source(case)}
    assert run_rewrite_case(payload) == run_oracle_case(payload)


_OPERATIONS = MappingProxyType(
    {
        "model": (
            "request.prepare",
            "prepared.prepare",
            "prepare_method",
            "prepare_url",
            "prepare_headers",
            "prepare_body",
            "prepare_content_length",
            "prepare_auth",
            "prepare_cookies",
            "prepare_hooks",
        ),
        "response": (
            "iter",
            "iter_content",
            "iter_lines",
            "content",
            "json",
            "raise_for_status",
            "close",
            "state",
            "pickle",
        ),
        "session": (
            "construct",
            "prepare_request",
            "request",
            "send",
            "get_redirect_target",
            "should_strip_auth",
            "resolve_redirects",
            "merge_environment_settings",
            "rebuild_auth",
            "rebuild_proxies",
            "rebuild_method",
            "mount",
            "get_adapter",
            "enter",
            "exit",
            "close",
            "state",
            "pickle",
        ),
        "adapter": ("construct", "send", "close", "state", "pickle"),
    }
)

_PYTHON_COMPOSED_OPERATIONS = MappingProxyType(
    {
        "model": ("copy",),
        "response": ("bool", "enter", "exit", "text"),
        "session": ("get", "options", "head", "post", "put", "patch", "delete"),
        "adapter": (
            "init_poolmanager",
            "proxy_manager_for",
            "cert_verify",
            "build_response",
            "build_connection_pool_key_attributes",
            "get_connection_with_tls_context",
            "get_connection",
            "request_url",
            "add_headers",
            "proxy_headers",
        ),
    }
)

_DEFAULT_SOURCE = r"""
from types import SimpleNamespace
import requests
import requests.adapters as adapters_module
import requests.api as api_module
from requests.adapters import HTTPAdapter
from requests.adapters import BaseAdapter
from requests.models import PreparedRequest, Response
from requests.sessions import Session

events = []
extension = requests._requests_rust
seam_names = (
    "_model_facade_trial", "_response_facade_trial",
    "_session_facade_trial", "_adapter_facade_trial",
)
missing = object()
originals = {name: getattr(extension, name, missing) for name in seam_names}
def forbidden(name):
    def dispatch(*args, **kwargs):
        events.append(name)
        raise AssertionError("semantic facade ran outside explicit trial: " + name)
    return dispatch
for name in seam_names:
    setattr(extension, name, forbidden(name))

class ScriptedAdapter(BaseAdapter):
    def send(self, request, **kwargs):
        response = Response()
        response.status_code = 200
        response.url = request.url
        response.request = request
        response.raw = SimpleNamespace(_original_response=None)
        response._content = request.url.encode()
        response._content_consumed = True
        response.history = []
        return response
    def close(self):
        pass

session = Session()
session.adapters.clear()
session.mount("mock://", ScriptedAdapter())
prepared = PreparedRequest()
prepared.prepare(method="GET", url="mock://session")
response_probe = Response()
response_probe._content = b"response"
response_probe._content_consumed = True
compat = adapters_module._HTTP_ADAPTER_COMPAT_SEND
factory = api_module.sessions.Session
adapters_module._HTTP_ADAPTER_COMPAT_SEND = lambda *args, **kwargs: "python-adapter"
api_module.sessions.Session = lambda: session
try:
    prepared.prepare_method("PATCH")
    response_content = response_probe.content.decode()
    session_response = session.send(prepared, allow_redirects=False)
    adapter_result = HTTPAdapter().send(prepared)
    root_response = requests.get("mock://root")
finally:
    adapters_module._HTTP_ADAPTER_COMPAT_SEND = compat
    api_module.sessions.Session = factory
    for name, original in originals.items():
        if original is missing:
            delattr(extension, name)
        else:
            setattr(extension, name, original)
result = SimpleNamespace(**{
    "semantic_events": events,
    "model": prepared.method,
    "response": response_content,
    "session": session_response.content.decode(),
    "adapter": adapter_result,
    "root": root_response.content.decode(),
})
"""


def test_task17_default_backend_remains_python_outside_explicit_trial() -> None:
    run = run_rewrite_case({"source": dedent(_DEFAULT_SOURCE)})
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "semantic_events": [],
        "model": "PATCH",
        "response": "response",
        "session": "mock://session",
        "adapter": "python-adapter",
        "root": "mock://root",
    }


_INVENTORY_SOURCE = r"""
import pickle
from types import SimpleNamespace
import requests
import requests.adapters as adapters_module
from requests.adapters import BaseAdapter, HTTPAdapter
from requests.models import PreparedRequest, Request, Response
from requests.sessions import Session
expected = EXPECTED; events = {name: [] for name in expected}; extension = requests._requests_rust
seams = {name: "_" + name + "_facade_trial" for name in expected}
originals = {name: getattr(extension, attr) for name, attr in seams.items()}
old_trials = {name: getattr(extension, name) for name in ("_adapter_send_trial", "_session_pipeline_trial", "_session_runtime_trial") if hasattr(extension, name)}
def recorder(group):
    def dispatch(subject, operation, args, kwargs): events[group].append(operation); return NotImplemented
    return dispatch
for group, attr in seams.items(): setattr(extension, attr, recorder(group))
for name in old_trials: setattr(extension, name, lambda *args, _name=name, **kwargs: (_ for _ in ()).throw(AssertionError("forbidden:" + _name)))
class ScriptedAdapter(BaseAdapter):
    def send(self, request, **kwargs):
        response = Response(); response.status_code = 200; response.url = request.url; response.request = request
        response.raw = SimpleNamespace(_original_response=None, close=lambda: None, release_conn=lambda: None)
        response._content = b'{"ok": true}'; response._content_consumed = True; response.history = []; return response
    def close(self): pass
try:
    with requests._rust_public_trial():
        request = Request("GET", "mock://resource"); prepared = request.prepare()
        prepared.prepare(method="POST", url="mock://resource", headers={"X":"1"}, data=b"body", auth=("u","p"), cookies={"c":"v"}, hooks={"response":[]})
        prepared.prepare_method("PATCH"); prepared.prepare_url("mock://resource", [("p","1")]); prepared.prepare_headers({"X":"2"})
        prepared.prepare_body(b"body", None, None); prepared.prepare_content_length(b"body"); prepared.prepare_auth(("u","p"), "mock://resource"); prepared.prepare_cookies({"c":"v"}); prepared.prepare_hooks({"response":[]})
        response = Response(); response.status_code = 200; response.url = "mock://resource"; response.request = prepared; response.raw = SimpleNamespace(close=lambda: None, release_conn=lambda: None); response._content = b"one\ntwo"; response._content_consumed = True
        list(iter(response)); list(response.iter_content(2)); list(response.iter_lines()); response.content; response.raise_for_status(); response.__getstate__(); pickle.loads(pickle.dumps(response)); response.close()
        json_response = Response(); json_response._content = b'{"ok":true}'; json_response._content_consumed = True; json_response.json()
        session = Session(); session.adapters.clear(); session.mount("mock://", ScriptedAdapter())
        session.prepare_request(Request("GET", "mock://resource")); session.request("GET", "mock://resource"); session.send(prepared, allow_redirects=False)
        session.get_redirect_target(response); session.should_strip_auth("http://a/", "https://a/"); list(session.resolve_redirects(response, prepared))
        session.merge_environment_settings("mock://resource", {}, False, True, None); session.rebuild_auth(prepared, response); session.rebuild_proxies(prepared, {}); session.rebuild_method(prepared, response)
        session.get_adapter("mock://resource"); session.__getstate__(); pickle.loads(pickle.dumps(session)); session.__enter__(); session.__exit__(None, None, None); session.close()
        base = BaseAdapter()
        try: base.send(prepared)
        except NotImplementedError: pass
        try: base.close()
        except NotImplementedError: pass
        adapter = HTTPAdapter(); old_send = adapters_module._HTTP_ADAPTER_COMPAT_SEND; adapters_module._HTTP_ADAPTER_COMPAT_SEND = lambda adapter, request, **kwargs: ScriptedAdapter().send(request, **kwargs)
        try: adapter.send(prepared)
        finally: adapters_module._HTTP_ADAPTER_COMPAT_SEND = old_send
        adapter.__getstate__(); pickle.loads(pickle.dumps(adapter)); adapter.close()
finally:
    for group, attr in seams.items(): setattr(extension, attr, originals[group])
    for name, original in old_trials.items(): setattr(extension, name, original)
result = SimpleNamespace(**{
    group: sorted(set(values), key=expected[group].index)
    for group, values in events.items()
})
"""


def test_task17_red_all_public_operations_use_semantic_facade_trial() -> None:
    source = _INVENTORY_SOURCE.replace("EXPECTED", repr(dict(_OPERATIONS)), 1)
    run = run_rewrite_case({"source": dedent(source)})
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        name: list(values) for name, values in _OPERATIONS.items()
    }


_REAL_SOURCE = r"""
import copy, pickle, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
import requests
trial_context = requests._rust_public_trial
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        with self.server.lock: body = self.server.bodies.pop(0); self.server.requests += 1
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); self.wfile.flush()
    def log_message(self, format, *args): pass
server = ThreadingHTTPServer(("127.0.0.1", 0), Handler); server.bodies = [b"active", b"reuse", b"copy", b"pickle", b"adapter-copy", b"adapter-pickle", b"peer"]; server.requests = 0; server.lock = threading.Lock()
worker = threading.Thread(target=server.serve_forever, daemon=True); worker.start(); url = f"http://127.0.0.1:{server.server_port}/resource"
extension = requests._requests_rust; sessions = []; responses = []
try:
    with trial_context():
        original = requests.Session(); sessions.append(original); active = original.get(url, stream=True); responses.append(active); original.close(); active_content = active.content; responses.append(original.get(url))
        copied, restored = copy.copy(original), pickle.loads(pickle.dumps(original)); sessions += [copied, restored]
        copy_shares_visible_adapters = copied.adapters is original.adapters
        copy_shares_visible_http_adapter = copied.get_adapter(url) is original.get_adapter(url)
        pickle_has_distinct_visible_adapters = restored.adapters is not original.adapters
        pickle_has_distinct_visible_http_adapter = restored.get_adapter(url) is not original.get_adapter(url)
        responses += [copied.get(url), restored.get(url)]
        adapter = original.get_adapter(url); adapter_copy, adapter_pickle = copy.copy(adapter), pickle.loads(pickle.dumps(adapter))
        for item in (adapter_copy, adapter_pickle):
            session = requests.Session(); session.adapters.clear(); session.mount("http://", item); sessions.append(session); responses.append(session.get(url))
        peer = requests.Session(); sessions.append(peer); responses.append(peer.get(url))
        snapshots = [extension._public_facade_snapshot(item) for item in (original, copied, restored, adapter, adapter_copy, adapter_pickle)]
        generation = extension._runtime_generation_trial()
        contents = [active_content.decode()] + [response.content.decode() for response in responses[1:]]
finally:
    for response in responses: response.close()
    for session in sessions: session.close()
    server.shutdown(); server.server_close(); worker.join(5)
owners = [item["owner_generation"] for item in snapshots]; pools = [item["pool_generation"] for item in snapshots]
result = SimpleNamespace(**{"contents": contents, "native_raw": [type(response.raw).__module__ == "requests._requests_rust" for response in responses], "requests": server.requests, "owners_distinct": len(set(owners)) == len(owners), "pools_distinct": len(set(pools)) == len(pools), "copy_shares_visible_adapters": copy_shares_visible_adapters, "copy_shares_visible_http_adapter": copy_shares_visible_http_adapter, "pickle_has_distinct_visible_adapters": pickle_has_distinct_visible_adapters, "pickle_has_distinct_visible_http_adapter": pickle_has_distinct_visible_http_adapter, "same_driver": generation == extension._runtime_generation_trial()})
"""


def test_task17_red_real_active_response_copy_pickle_close_and_reuse() -> None:
    run = run_rewrite_case({"source": dedent(_REAL_SOURCE)})
    assert run.observations["exception"] is None
    assert run.observations["result"]["public_state"] == {
        "contents": [
            "active",
            "reuse",
            "copy",
            "pickle",
            "adapter-copy",
            "adapter-pickle",
            "peer",
        ],
        "native_raw": [True] * 7,
        "requests": 7,
        "owners_distinct": True,
        "pools_distinct": True,
        "copy_shares_visible_adapters": True,
        "copy_shares_visible_http_adapter": True,
        "pickle_has_distinct_visible_adapters": True,
        "pickle_has_distinct_visible_http_adapter": True,
        "same_driver": True,
    }
    result = run.observations["result"]["public_state"]
    # pool_generation is a Session-owner-local binding generation; the copied
    # Session still shares its visible HTTPAdapter leaf and physical pool.
    assert result["pools_distinct"] and result["copy_shares_visible_http_adapter"]


_SIDE_SOURCE = r"""
from collections.abc import Mapping
from http.cookiejar import CookieJar

import requests
from requests.adapters import HTTPAdapter
from requests.sessions import Session
extension = requests._requests_rust
registry = extension._public_facade_registry_trial

def visible(value, seen=None):
    if value is None or type(value) in {bool, int, float, str, bytes}:
        return value
    seen = set() if seen is None else seen
    identity = id(value)
    if identity in seen:
        return ["cycle", type_name(value)]
    seen.add(identity)
    try:
        if isinstance(value, Mapping):
            return ["mapping", type_name(value), [[visible(key, seen), visible(item, seen)] for key, item in value.items()]]
        if isinstance(value, (list, tuple)):
            return ["sequence", type_name(value), [visible(item, seen) for item in value]]
        if isinstance(value, (set, frozenset)):
            items = [visible(item, seen) for item in value]
            return ["set", type_name(value), sorted(items, key=repr)]
        if isinstance(value, CookieJar):
            return ["cookies", type_name(value), sorted([
                [cookie.domain, cookie.path, cookie.name, cookie.value, cookie.secure, cookie.expires]
                for cookie in value
            ])]
        if type(value).__module__ == "requests.adapters" and hasattr(value, "__getstate__"):
            return ["adapter-state", type_name(value), visible(value.__getstate__(), seen)]
        if type(value).__module__ == "urllib3.util.retry" and hasattr(value, "__dict__"):
            return ["retry-state", type_name(value), visible(value.__dict__, seen)]
        raise AssertionError("opaque visible state: " + repr(type_name(value)))
    finally:
        seen.remove(identity)

def baseline(owner):
    dictionary = list(owner.__dict__.items())
    state = list(owner.__getstate__().items())
    namespace = list(type(owner).__dict__.items())
    payload = pickle.dumps(owner, protocol=pickle.HIGHEST_PROTOCOL)
    restored = pickle.loads(payload)
    return {
        "dictionary": dictionary,
        "state": state,
        "namespace": namespace,
        "payload": payload,
        "restored": visible(restored.__getstate__()),
    }

def baseline_unchanged(owner, before):
    dictionary = list(owner.__dict__.items())
    state = list(owner.__getstate__().items())
    namespace = list(type(owner).__dict__.items())
    payload = pickle.dumps(owner, protocol=pickle.HIGHEST_PROTOCOL)
    restored = pickle.loads(payload)
    return {
        "dict_keys": [key for key, _ in dictionary] == [key for key, _ in before["dictionary"]],
        "dict_values": all(after is prior for (_, after), (_, prior) in zip(dictionary, before["dictionary"])),
        "state_keys": [key for key, _ in state] == [key for key, _ in before["state"]],
        "state_values": all(after is prior for (_, after), (_, prior) in zip(state, before["state"])),
        "namespace_keys": [key for key, _ in namespace] == [key for key, _ in before["namespace"]],
        "namespace_values": all(after is prior for (_, after), (_, prior) in zip(namespace, before["namespace"])),
        "pickle_bytes": payload == before["payload"],
        "restored_visible": visible(restored.__getstate__()) == before["restored"],
    }

def exercise(cls):
    owner = cls()
    user = weakref.ref(owner)
    before = baseline(owner)
    snapshot = extension._public_facade_snapshot(owner)
    key = registry(owner, "key")
    stale_generation = registry(owner, "generation")
    rotated_generation = registry(owner, "rotate")
    current_generation = registry(owner, "generation")
    stale_removed = registry(owner, "drop", key, stale_generation)
    after_stale = extension._public_facade_snapshot(owner)
    copied = copy.copy(owner)
    restored = pickle.loads(pickle.dumps(owner))
    copied_before = baseline(copied)
    restored_before = baseline(restored)
    copied_snapshot = extension._public_facade_snapshot(copied)
    restored_snapshot = extension._public_facade_snapshot(restored)
    gc.collect()
    gc.collect()
    gc.collect()
    internal = [reference for reference in weakref.getweakrefs(owner) if reference is not user]
    return {
        "owner": owner,
        "user": user,
        "internal": len(internal),
        "plain_user_identity": weakref.ref(owner) is user,
        "baseline": baseline_unchanged(owner, before),
        "copy_baseline": baseline_unchanged(copied, copied_before),
        "pickle_baseline": baseline_unchanged(restored, restored_before),
        "stale": {
            "key_stable": registry(owner, "key") == key,
            "generation_rotated": rotated_generation != stale_generation,
            "current_generation": current_generation == after_stale["owner_generation"],
            "stale_removed": stale_removed,
            "live": after_stale["live"],
        },
        "generations": [snapshot["owner_generation"], copied_snapshot["owner_generation"], restored_snapshot["owner_generation"]],
        "pools": [snapshot["pool_generation"], copied_snapshot["pool_generation"], restored_snapshot["pool_generation"]],
        "copy_internal": len(weakref.getweakrefs(copied)),
        "pickle_internal": len(weakref.getweakrefs(restored)),
    }

def subclass_lifecycle(base, use_trial):
    events = []
    class Finalizing(base):
        def __del__(self):
            events.append("del")
    owner = Finalizing()
    user = weakref.ref(owner)
    first = weakref.ref(owner, lambda reference: events.append("first"))
    second = weakref.ref(owner, lambda reference: events.append("second"))
    admitted = False
    internal = 0
    if use_trial:
        extension._public_facade_snapshot(owner)
        admitted = registry(owner, "admitted")
        internal = len([reference for reference in weakref.getweakrefs(owner) if reference not in (user, first, second)])
    plain_user_identity = weakref.ref(owner) is user
    del owner
    gc.collect()
    gc.collect()
    gc.collect()
    return [events, admitted, internal, plain_user_identity, user() is None, first() is None, second() is None]

controls = [subclass_lifecycle(Session, False), subclass_lifecycle(HTTPAdapter, False)]
with requests._rust_public_trial():
    rows = [exercise(Session), exercise(HTTPAdapter)]
    admitted_subclasses = [subclass_lifecycle(Session, True), subclass_lifecycle(HTTPAdapter, True)]

events = []
collected = HTTPAdapter()
extension._public_facade_snapshot(collected)
first = weakref.ref(collected, lambda reference: events.append("first"))
second = weakref.ref(collected, lambda reference: events.append("second"))
enumerated = weakref.getweakrefs(collected)
del collected
gc.collect()
gc.collect()
gc.collect()
result = SimpleNamespace(**{
    "rows": [{
        "internal": row["internal"],
        "plain_user_identity": row["plain_user_identity"],
        "baseline": row["baseline"],
        "copy_baseline": row["copy_baseline"],
        "pickle_baseline": row["pickle_baseline"],
        "stale": row["stale"],
        "independent_generations": len(set(row["generations"])) == 3,
        "independent_pools": len(set(row["pools"])) == 3,
        "copy_internal": row["copy_internal"],
        "pickle_internal": row["pickle_internal"],
    } for row in rows],
    "subclasses": [controls, admitted_subclasses],
    "callbacks": events,
    "user_dead": [first() is None, second() is None],
    "enumerated": [first in enumerated, second in enumerated],
})
"""


def test_task17_red_side_table_generations_stale_callbacks_and_weakref_lifecycle() -> (
    None
):
    run = run_rewrite_case({"source": dedent(_HELPERS + _SIDE_SOURCE)})
    assert run.observations["exception"] is None
    result = run.observations["result"]["public_state"]
    exact_baseline = {
        "dict_keys": True,
        "dict_values": True,
        "state_keys": True,
        "state_values": True,
        "namespace_keys": True,
        "namespace_values": True,
        "pickle_bytes": True,
        "restored_visible": True,
    }
    expected = {
        "internal": 1,
        "plain_user_identity": True,
        "baseline": exact_baseline,
        "copy_baseline": exact_baseline,
        "pickle_baseline": exact_baseline,
        "stale": {
            "key_stable": True,
            "generation_rotated": True,
            "current_generation": True,
            "stale_removed": False,
            "live": True,
        },
        "independent_generations": True,
        "independent_pools": True,
        "copy_internal": 1,
        "pickle_internal": 1,
    }
    assert result["rows"] == [expected, expected]
    expected_control = [["del", "second", "first"], False, 0, True, True, True, True]
    expected_python_owned_subclass = [
        ["del", "second", "first"],
        False,
        0,
        True,
        True,
        True,
        True,
    ]
    assert result["subclasses"] == [
        [expected_control, expected_control],
        [expected_python_owned_subclass, expected_python_owned_subclass],
    ]
    assert (
        result["callbacks"] == ["second", "first"]
        and result["user_dead"] == [True, True]
        and result["enumerated"] == [True, True]
    )


_PUMP_SOURCE = r"""
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import requests

trial_context = requests._rust_public_trial
observer = requests._requests_rust._public_facade_pump_trial
runtime_observer = requests._requests_rust._runtime_submission_trial

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "4")
        self.end_headers()
        self.wfile.write(b"pump")
        self.wfile.flush()
    def log_message(self, format, *args):
        pass

observer("reset")
runtime_observer("reset")
server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
url = f"http://127.0.0.1:{server.server_port}/resource"
session = requests.Session()
try:
    with trial_context():
        response = session.get(url, stream=True)
        observation = observer("snapshot")
        runtime_observation = runtime_observer("snapshot")
        content = response.content
        response.close()
finally:
    session.close()
    server.shutdown()
    server.server_close()
    worker.join(5)
result = SimpleNamespace(**{
    "content": content.decode(),
    "outer_entries": observation["outer_entries"],
    "outer_exits": observation["outer_exits"],
    "max_depth": observation["max_depth"],
    "adapter_leaf_entries": observation["adapter_leaf_entries"],
    "nested_pump_entries": observation["nested_pump_entries"],
    "submission_ids": observation["submission_ids"],
    "submission_parent_ids": observation["submission_parent_ids"],
    "adapter_submission_ids": observation["adapter_submission_ids"],
    "runtime_events": runtime_observation["events"],
    "runtime_outstanding": runtime_observation["outstanding"],
    "runtime_generation": requests._requests_rust._runtime_generation_trial(),
})
"""


def _assert_one_outer_pump_runtime() -> None:
    run = run_rewrite_case({"source": dedent(_PUMP_SOURCE)})
    assert run.observations["exception"] is None
    result = run.observations["result"]["public_state"]
    assert {
        key: result[key]
        for key in (
            "content",
            "outer_entries",
            "outer_exits",
            "max_depth",
            "adapter_leaf_entries",
            "nested_pump_entries",
        )
    } == {
        "content": "pump",
        "outer_entries": 1,
        "outer_exits": 1,
        "max_depth": 1,
        "adapter_leaf_entries": 1,
        "nested_pump_entries": 0,
    }
    assert len(result["submission_ids"]) == 1
    assert result["submission_parent_ids"] == [None]
    assert result["adapter_submission_ids"] == result["submission_ids"]
    assert len(result["runtime_events"]) == 1
    assert result["runtime_events"][0][:2] == [result["submission_ids"][0], None]
    assert result["runtime_events"][0][2] == result["runtime_generation"]
    assert result["runtime_outstanding"] == 0


def _rust_function(source: str, name: str) -> str:
    start = source.index(f"fn {name}")
    brace = source.index("{", start)
    depth = 0
    for index in range(brace, len(source)):
        depth += (source[index] == "{") - (source[index] == "}")
        if depth == 0:
            return source[start : index + 1]
    raise AssertionError(name)


def _reachable_rust_functions(sources: tuple[str, ...], root: str) -> dict[str, str]:
    defined: set[str] = set()
    for source in sources:
        defined.update(re.findall(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(", source))

    def definition(name: str) -> str:
        for source in sources:
            try:
                return _rust_function(source, name)
            except ValueError:
                continue
        raise AssertionError(name)

    reachable: dict[str, str] = {}
    pending = [root]
    while pending:
        name = pending.pop()
        if name in reachable:
            continue
        block = definition(name)
        reachable[name] = block
        calls = set(re.findall(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(", block))
        pending.extend(sorted((calls & defined) - reachable.keys()))
    return reachable


def test_task17_red_outer_pump_runtime_and_static_call_graph_share_adapter_leaf() -> (
    None
):
    _assert_one_outer_pump_runtime()
    root = Path(__file__).resolve().parents[1]
    sources = {
        "model": (root / "src/requests/models.py").read_text(),
        "response": (root / "src/requests/models.py").read_text(),
        "session": (root / "src/requests/sessions.py").read_text(),
        "adapter": (root / "src/requests/adapters.py").read_text(),
    }
    assert "def _rust_public_trial" in sources["session"]
    for group, operations in _OPERATIONS.items():
        assert f"_{group}_facade_trial" in sources[group]
        assert all(
            repr(operation) in sources[group] or f'"{operation}"' in sources[group]
            for operation in operations
        )
    sessions_rust = (root / "crates/requests-python/src/sessions.rs").read_text()
    adapters_rust = (root / "crates/requests-python/src/adapters.rs").read_text()
    facade = _rust_function(sessions_rust, "_session_facade_trial")
    reachable = _reachable_rust_functions(
        (sessions_rust, adapters_rust), "run_session_facade"
    )
    joined = "\n".join(reachable.values())
    assert "run_session_facade" in facade
    assert (
        sum(block.count("run_with_owned_actions(") for block in reachable.values()) == 0
    )
    assert "run_with_actions" not in _rust_function(sessions_rust, "run_session_facade")
    adapter_leaf = _rust_function(adapters_rust, "native_adapter_leaf")
    assert adapter_leaf.count("run_with_actions_and_signal_checker(") == 1
    assert "send_async(" in adapter_leaf
    assert "pool.send(" not in adapter_leaf
    assert "send_from_session" in reachable
    assert all(
        forbidden not in joined
        for forbidden in (
            "_adapter_send_trial",
            "_session_pipeline_trial",
            "_session_runtime_trial",
            "PyModule::from_code",
            "py.eval",
            "py.run",
            "compile(",
        )
    )
    assert "run_with_owned_actions(" not in _rust_function(
        adapters_rust, "send_from_session"
    )
    assert "send_from_session(" in _rust_function(adapters_rust, "_adapter_send_trial")


def test_task17_public_type_inventory_and_mutation_guards_are_consolidated() -> None:
    assert tuple(case.case_id for case in PUBLIC_TYPE_CASES) == (
        "P01",
        "P02",
        "P03",
        "P04",
        "P05",
        "P06",
    )
    assert tuple(_OPERATIONS) == ("model", "response", "session", "adapter")
    assert tuple(_PYTHON_COMPOSED_OPERATIONS) == tuple(_OPERATIONS)
    for operations in _PYTHON_COMPOSED_OPERATIONS.values():
        assert all(operation in _PYTHON_COMPOSITION for operation in operations)
    for case in PUBLIC_TYPE_CASES:
        tree = ast.parse(case_source(case))
        assignments = [
            node
            for node in tree.body
            if isinstance(node, ast.Assign)
            and any(
                isinstance(target, ast.Name) and target.id == "result"
                for target in node.targets
            )
        ]
        assert len(assignments) == 1
    for source in (
        _DEFAULT_SOURCE,
        _INVENTORY_SOURCE,
        _REAL_SOURCE,
        _SIDE_SOURCE,
        _PUMP_SOURCE,
    ):
        tree = ast.parse(dedent(source))
        assert "REQUESTS_DIFFERENTIAL_TARGET" not in source
        assert not any(
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr in {"sleep", "eval", "exec"}
            for node in ast.walk(tree)
        )
    assert all(
        name in _INVENTORY_SOURCE
        for name in (
            "_adapter_send_trial",
            "_session_pipeline_trial",
            "_session_runtime_trial",
        )
    )


def test_task17_reuses_task7_through_task16_matrices() -> None:
    root = Path(__file__).resolve().parent
    expected = {
        "test_prepare_request.py": ("prepare_method", "prepare_url"),
        "test_response.py": ("iter_content", "raise_for_status"),
        "test_adapters.py": ("_rust_adapter_trial", "loopback"),
        "test_sessions_redirects.py": (
            "_CASES",
            "_ADVERSARIAL_CASES",
            "A01",
            "M06",
            "T06",
        ),
    }
    for name, markers in expected.items():
        source = (root / name).read_text()
        assert all(marker in source for marker in markers)

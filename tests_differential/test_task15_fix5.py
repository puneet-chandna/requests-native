from __future__ import annotations

from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case
from tests_differential.test_cookies import _COOKIE_TRIAL


def _assert_equal(source: str, *, allow_oracle_exception: bool = False) -> None:
    """Run the frozen oracle first, then require an identical rewrite record."""
    case = {"source": dedent(_COOKIE_TRIAL + source)}
    oracle = run_oracle_case(case)
    if oracle.observations["exception"] is not None and not allow_oracle_exception:
        raise AssertionError(
            f"oracle unexpectedly raised {oracle.observations['exception']!r}"
        )
    rewrite = run_rewrite_case(case)

    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


_PIPELINE_SETUP = """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response

jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
cookie = next(iter(jar))
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
response = Response()
response.raw = SimpleNamespace(_original_response=None)
response.request = request
audit = []
"""


@pytest.mark.parametrize("kind", ["object", "falsey", "str-subclass", "int-zero"])
def test_pipeline_header_preserves_arbitrary_object_identity(kind: str) -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
events = []


class FalseyHeader:
    def __bool__(self):
        events.append("header-bool")
        return False


class HeaderText(str):
    pass


if {kind!r} == "object":
    header = object()
elif {kind!r} == "falsey":
    header = FalseyHeader()
elif {kind!r} == "str-subclass":
    header = HeaderText("header-text")
else:
    header = 0

original_header = cookies_module.get_cookie_header


def hook(selected):
    cookies_module.get_cookie_header = lambda authoritative, prepared: header
    return selected


try:
    value = pipeline_once(
        jar,
        request,
        response,
        hook,
        lambda selected, authoritative: selected,
        audit,
    )
finally:
    cookies_module.get_cookie_header = original_header

assert_counts(rewrite_compat=0)
assert value["header"] is header
assert type(value["header"]) is type(header)
assert events == []
result = ({kind!r}, value["header"] is header, type(value["header"]).__qualname__, events)
"""
    )


@pytest.mark.parametrize("header", [None, "exact-header"])
def test_pipeline_header_pristine_scalar_controls(header: object) -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
header = {header!r}
original_header = cookies_module.get_cookie_header


def hook(selected):
    cookies_module.get_cookie_header = lambda authoritative, prepared: header
    return selected


try:
    value = pipeline_once(
        jar,
        request,
        response,
        hook,
        lambda selected, authoritative: selected,
        audit,
    )
finally:
    cookies_module.get_cookie_header = original_header

assert_counts(rewrite_compat=0)
assert value["header"] is header
result = (value["header"], value["audit"])
"""
    )


def test_pipeline_snapshots_preserve_sequential_cookie_object_identity() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []


class Tagged(str):
    pass


first = [Tagged("n1"), Tagged("v1"), Tagged("a.test"), Tagged("/")]
second = [Tagged("n2"), Tagged("v2"), Tagged("a.test"), Tagged("/")]
rest_first = object()
rest_second = object()
original_deepvalues = cookies_module.cookielib.deepvalues


def observed_deepvalues(mapping):
    events.append("deepvalues")
    yield from original_deepvalues(mapping)


def hook(selected):
    cookie.name, cookie.value, cookie.domain, cookie.path = first
    cookie._rest = {"marker": rest_first}
    cookies_module.cookielib.deepvalues = observed_deepvalues
    return selected


def digest(selected, authoritative):
    cookie.name, cookie.value, cookie.domain, cookie.path = second
    cookie._rest = {"marker": rest_second}
    return selected


try:
    value = pipeline_once(jar, request, response, hook, digest, audit)
finally:
    cookies_module.cookielib.deepvalues = original_deepvalues

assert_counts(rewrite_compat=0)
assert all(value["snapshots"][1][0][index] is first[index] for index in range(4))
assert all(value["snapshots"][2][0][index] is first[index] for index in range(4))
assert all(value["snapshots"][3][0][index] is second[index] for index in range(4))
assert all(value["snapshots"][4][0][index] is second[index] for index in range(4))
assert all(value["cookies"][0][index] is second[index] for index in range(4))
assert cookie._rest["marker"] is rest_second
assert len({id(rows) for rows in value["snapshots"]}) == 5
assert len({id(rows[0]) for rows in value["snapshots"]}) == 5
assert id(value["cookies"]) not in {id(rows) for rows in value["snapshots"]}
assert id(value["cookies"][0]) not in {
    id(rows[0]) for rows in value["snapshots"]
}
assert events
result = (
    value["audit"],
    [len(rows) for rows in value["snapshots"]],
    len(events),
    cookie._rest["marker"] is rest_second,
)
"""
    )


def test_pipeline_arbitrary_name_and_rest_are_live_in_every_later_row() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []


name = object()
rest = object()


def hook(selected):
    cookie.name = name
    cookie._rest = {"marker": rest}
    return selected


value = pipeline_once(
    jar,
    request,
    response,
    hook,
    lambda selected, authoritative: selected,
    audit,
)
assert_counts(rewrite_compat=0)
assert value["snapshots"][1][0][0] is name
assert value["snapshots"][4][0][0] is name
assert value["cookies"][0][0] is name
assert cookie._rest["marker"] is rest
assert events == []
result = (value["audit"], events)
"""
    )


@pytest.mark.parametrize("field", ["value", "domain", "path", "secure", "expires"])
def test_pipeline_unsupported_field_fails_only_at_live_header_stage(field: str) -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
events = []
marker = LookupError({field!r} + "-sentinel")


class Probe:
    def __bool__(self):
        events.append("probe-bool")
        raise marker

    def __le__(self, other):
        events.append("probe-le")
        raise marker

    def startswith(self, other):
        events.append("probe-startswith")
        raise marker

    def __len__(self):
        events.append("probe-len")
        raise marker


probe = object() if {field!r} == "value" else Probe()


def hook(selected):
    setattr(cookie, {field!r}, probe)
    return selected


try:
    pipeline_once(
        jar,
        request,
        response,
        hook,
        lambda selected, authoritative: selected,
        audit,
    )
except BaseException as error:
    observed = (
        error is marker,
        type(error).__module__,
        type(error).__qualname__,
        error.args,
    )

assert_counts(rewrite_compat=0)
assert audit == ["prepare", "hook", "extract", "digest", "header"]
if {field!r} == "value":
    assert observed[1:3] == ("builtins", "TypeError")
else:
    assert observed[0]
    assert len(events) == 1
result = (audit, events, observed)
"""
    )


def test_pipeline_live_cookie_descriptor_error_occurs_mid_row_once() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
import http.cookiejar as cookiejar_module

events = []
marker = LookupError("live-value")
original_value = cookie.value


class ValueDescriptor:
    def __get__(self, instance, owner):
        events.append("value-get")
        raise marker

    def __set__(self, instance, value):
        instance.__dict__["value"] = value


def hook(selected):
    cookie.__dict__.pop("value")
    cookiejar_module.Cookie.value = ValueDescriptor()
    return selected


try:
    try:
        pipeline_once(
            jar,
            request,
            response,
            hook,
            lambda selected, authoritative: selected,
            audit,
        )
    except BaseException as error:
        observed = (error is marker, type(error).__qualname__, error.args)
finally:
    del cookiejar_module.Cookie.value
    cookie.value = original_value

assert_counts(rewrite_compat=0)
assert observed[0]
assert events == ["value-get"]
assert audit == ["prepare", "hook"]
result = (events, observed, audit)
"""
    )


@pytest.mark.parametrize(
    ("operation", "owner", "target"),
    [
        ("inspect", "jar", "keys"),
        ("mutate", "jar", "set_cookie"),
        ("copy-pickle", "jar", "copy"),
        ("header", "jar", "add_cookie_header"),
        ("extract-header", "jar", "extract_cookies"),
        ("pipeline", "request", "prepare_cookies"),
    ],
)
def test_raw_instance_collision_raises_once_in_compat_order(
    operation: str, owner: str, target: str
) -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
from email.message import Message
from http.cookies import SimpleCookie
from types import SimpleNamespace

from requests.cookies import create_cookie

events = []
marker = LookupError("raw-key")


class CollisionKey:
    def __hash__(self):
        return hash({target!r})

    def __eq__(self, other):
        events.append(("eq", other))
        raise marker


subject = jar if {owner!r} == "jar" else request
subject.__dict__[CollisionKey()] = object()
morsels = SimpleCookie()
morsels["morsel"] = "value"
quoted = create_cookie("quoted", '"value"')
try:
    if {operation!r} == "inspect":
        cookie_once(jar, "inspect", (("token", None, None, "missing"),))
    elif {operation!r} == "mutate":
        cookie_once(jar, "mutate", (morsels["morsel"], quoted, "new"))
    elif {operation!r} == "copy-pickle":
        cookie_once(jar, "copy-pickle")
    elif {operation!r} == "header":
        bridge_once(jar, request, response.raw, "header", audit)
    elif {operation!r} == "extract-header":
        response.raw._original_response = SimpleNamespace(msg=Message())
        bridge_once(jar, request, response.raw, "extract-header", audit)
    else:
        pipeline_once(
            jar,
            request,
            response,
            lambda selected: selected,
            lambda selected, authoritative: selected,
            audit,
        )
except BaseException as error:
    observed = (error is marker, type(error).__qualname__, error.args)

assert_counts(rewrite_compat=1)
assert observed[0]
assert events == [("eq", {target!r})]
result = (events, observed)
"""
    )


@pytest.mark.parametrize("behavior", ["false", "mutate"])
def test_raw_instance_collision_presence_forces_fallback_without_probe_callbacks(
    behavior: str,
) -> None:
    _assert_equal(
        f"""
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")


class CollisionKey:
    def __hash__(self):
        return hash("keys")

    def __eq__(self, other):
        events.append(("eq", other))
        if {behavior!r} == "mutate":
            jar.__dict__.pop(self, None)
        return False


key = CollisionKey()
jar.__dict__[key] = object()
value = cookie_once(jar, "inspect", (("token", None, None, "missing"),))
assert_counts(rewrite_compat=1)
assert events == [("eq", "keys")]
result = (events, value["keys"])
"""
    )


def test_raw_instance_collision_true_falls_back_before_shadowed_method() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")


class CollisionKey:
    def __hash__(self):
        return hash("keys")

    def __eq__(self, other):
        events.append(("eq", other))
        return other == "keys"


jar.__dict__[CollisionKey()] = lambda: ["shadowed-keys"]
value = cookie_once(jar, "inspect", (("token", None, None, "missing"),))
assert_counts(rewrite_compat=1)
assert events == [("eq", "keys")]
assert value["keys"] == ["shadowed-keys"]
result = (events, value["keys"])
"""
    )


def test_unrelated_exact_string_instance_key_remains_native() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
jar.__dict__["unrelated-task15-key"] = object()
value = cookie_once(jar, "inspect", (("token", None, None, "missing"),))
assert_counts(rewrite_compat=0)
result = value
"""
    )


@pytest.mark.parametrize("subject_kind", ["jar-storage", "cookie-field"])
def test_raw_data_collision_raises_once_before_named_lookup(subject_kind: str) -> None:
    target = "_cookies" if subject_kind == "jar-storage" else "value"
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
events = side_effects
marker = LookupError("raw-data-key")
exception_identities = {{"marker": marker}}


class CollisionKey:
    def __hash__(self):
        return hash({target!r})

    def __eq__(self, other):
        events.append(("eq", other))
        raise marker


subject = jar if {subject_kind!r} == "jar-storage" else cookie
stored = subject.__dict__.pop({target!r})
subject.__dict__[CollisionKey()] = stored
try:
    cookie_once(jar, "inspect", (("token", None, None, "missing"),))
finally:
    assert_counts(rewrite_compat=1)
""",
        allow_oracle_exception=True,
    )


def test_morsel_mapping_collision_raises_once_before_reserved_lookup() -> None:
    _assert_equal(
        """
from http.cookies import SimpleCookie

from requests.cookies import RequestsCookieJar

events = []
marker = LookupError("morsel-key")
jar = RequestsCookieJar()
morsels = SimpleCookie()
morsels["name"] = "value"
morsel = morsels["name"]


class CollisionKey:
    def __hash__(self):
        return hash("path")

    def __eq__(self, other):
        events.append(("eq", other))
        raise marker


stored = dict.__getitem__(morsel, "path")
dict.__delitem__(morsel, "path")
dict.__setitem__(morsel, CollisionKey(), stored)
try:
    cookie_once(jar, "morsel", (morsel,))
except BaseException as error:
    observed = (error is marker, type(error).__qualname__, error.args)

assert_counts(rewrite_compat=1)
assert observed[0]
assert events == [("eq", "path")]
result = (events, observed)
"""
    )


def test_module_dictionary_collision_raises_once_before_function_lookup() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []
marker = LookupError("module-key")
module_dict = cookies_module.__dict__


class CollisionKey:
    def __hash__(self):
        return hash("get_cookie_header")

    def __eq__(self, other):
        events.append(("eq", other))
        raise marker


original = module_dict.pop("get_cookie_header")
module_dict[CollisionKey()] = original
try:
    bridge_once(jar, request, response.raw, "header", audit)
except BaseException as error:
    observed = (error is marker, type(error).__qualname__, error.args)

assert_counts(rewrite_compat=1)
assert observed[0]
assert events == [("eq", "get_cookie_header")]
result = (events, observed)
"""
    )


def test_class_method_descriptor_is_not_invoked_during_admission() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
marker = LookupError("class-descriptor")
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
original_keys = RequestsCookieJar.keys


class Descriptor:
    def __get__(self, instance, owner):
        events.append(("descriptor", instance is None))
        raise marker


RequestsCookieJar.keys = Descriptor()
try:
    try:
        cookie_once(jar, "inspect", (("token", None, None, "missing"),))
    except BaseException as error:
        observed = (error is marker, type(error).__qualname__, error.args)
finally:
    RequestsCookieJar.keys = original_keys

assert_counts(rewrite_compat=1)
assert observed[0]
assert events == [("descriptor", False)]
result = (events, observed)
"""
    )


def test_pipeline_response_collision_is_not_replayed_by_admission() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = side_effects
marker = LookupError("response-raw")
exception_identities = {"marker": marker}


class CollisionKey:
    def __hash__(self):
        return hash("raw")

    def __eq__(self, other):
        events.append(("eq", other))
        raise marker


stored = response.__dict__.pop("raw")
response.__dict__[CollisionKey()] = stored
try:
    pipeline_once(
        jar,
        request,
        response,
        lambda selected: selected,
        lambda selected, authoritative: selected,
        audit,
    )
finally:
    assert_counts(rewrite_compat=0)
""",
        allow_oracle_exception=True,
    )


def test_pipeline_extract_resolves_live_helper_before_response_raw() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []
original_extract = cookies_module.extract_cookies_to_jar


def observed_extract(authoritative, prepared, raw):
    events.append("extract-call")
    return original_extract(authoritative, prepared, raw)


def module_getattr(name):
    events.append(("module-getattr", name))
    if name == "extract_cookies_to_jar":
        return observed_extract
    raise AttributeError(name)


class Replacement:
    @property
    def raw(self):
        events.append("raw")
        return response.raw


replacement = Replacement()


def hook(selected):
    del cookies_module.extract_cookies_to_jar
    cookies_module.__getattr__ = module_getattr
    return replacement


try:
    value = pipeline_once(
        jar,
        request,
        response,
        hook,
        lambda selected, authoritative: selected,
        audit,
    )
finally:
    cookies_module.extract_cookies_to_jar = original_extract
    cookies_module.__dict__.pop("__getattr__", None)

assert_counts(rewrite_compat=0)
assert events[:2] == [
    ("module-getattr", "extract_cookies_to_jar"),
    "raw",
]
result = (events, value["audit"])
"""
    )


@pytest.mark.parametrize("kind", ["object", "str-subclass"])
def test_direct_bridge_preserves_live_header_and_cookie_row_objects(kind: str) -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + f"""
events = []


class HeaderText(str):
    pass


class Tagged(str):
    pass


header = object() if {kind!r} == "object" else HeaderText("bridge-header")
fields = [Tagged("name"), Tagged("value"), Tagged("a.test"), Tagged("/")]
rest = object()
original_header = cookies_module.get_cookie_header


def live_header(authoritative, prepared):
    events.append("live-header")
    return header


class OriginalResponse:
    def __bool__(self):
        events.append("original-bool")
        cookie.name, cookie.value, cookie.domain, cookie.path = fields
        cookie._rest = {{"marker": rest}}
        cookies_module.get_cookie_header = live_header
        return False


response.raw._original_response = OriginalResponse()
try:
    value = bridge_once(jar, request, response.raw, "extract-header", audit)
finally:
    cookies_module.get_cookie_header = original_header

assert_counts(rewrite_compat=0)
assert value[0] is header
assert all(value[1][0][index] is fields[index] for index in range(4))
assert cookie._rest["marker"] is rest
assert events == ["original-bool", "live-header"]
result = ({kind!r}, value[0] is header, type(value[0]).__qualname__, events)
"""
    )


def test_pipeline_stored_snapshot_object_drops_on_origin_after_header_error() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
import gc

origin_thread = threading.get_ident()
events = []
marker = LookupError("header-domain")


def record_thread(label):
    events.append((label, threading.get_ident() == origin_thread))


class TrackedName:
    def __str__(self):
        return "tracked"

    def __del__(self):
        record_thread("tracked-del")


class DomainProbe(str):
    def startswith(self, prefix, *args):
        record_thread("domain-startswith")
        raise marker


def hook(selected):
    cookie.name = TrackedName()
    return selected


def digest(selected, authoritative):
    cookie.name = "final-name"
    cookie.domain = DomainProbe("a.test")
    return selected


try:
    pipeline_once(jar, request, response, hook, digest, audit)
except BaseException as error:
    observed = (error is marker, type(error).__qualname__, error.args)
    marker.__traceback__ = None
finally:
    cookie.name = "cleanup-name"
    cookie.domain = "a.test"
    gc.collect()

assert_counts(rewrite_compat=0)
assert observed[0]
assert all(same_thread for _, same_thread in events)
result = ([label for label, _ in events], all(flag for _, flag in events), observed, audit)
"""
    )


def test_custom_lock_rebinds_entire_cookielib_before_live_constructor() -> None:
    _assert_equal(
        """
from http.cookies import SimpleCookie
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie

events = []
jar = RequestsCookieJar()
original_cookielib = cookies_module.cookielib
original_cookie = original_cookielib.Cookie


def live_cookie(*args, **kwargs):
    events.append("live-cookie")
    return original_cookie(*args, **kwargs)


replacement_cookielib = SimpleNamespace(
    Cookie=live_cookie,
    deepvalues=original_cookielib.deepvalues,
)


class CallbackLock:
    def acquire(self):
        events.append("acquire")
        cookies_module.cookielib = replacement_cookielib

    def release(self):
        events.append("release")


jar._cookies_lock = CallbackLock()
morsels = SimpleCookie()
morsels["morsel"] = "value"
quoted = create_cookie("quoted", '"value"')
try:
    value = cookie_once(jar, "mutate", (morsels["morsel"], quoted, "new"))
finally:
    cookies_module.cookielib = original_cookielib

assert_counts(rewrite_compat=1)
assert "live-cookie" in events
assert events.index("acquire") < events.index("live-cookie")
result = (events, value)
"""
    )


def test_committed_mutate_uses_live_cookielib_after_trace_callback() -> None:
    _assert_equal(
        """
import sys
from http.cookies import SimpleCookie
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie

events = []
jar = RequestsCookieJar()
original_cookielib = cookies_module.cookielib
original_cookie = original_cookielib.Cookie
original_set_cookie = cookies_module.CookieJar.set_cookie


def live_cookie(*args, **kwargs):
    events.append("live-cookie")
    return original_cookie(*args, **kwargs)


replacement_cookielib = SimpleNamespace(
    Cookie=live_cookie,
    deepvalues=original_cookielib.deepvalues,
)


def trace(frame, event, arg):
    if event == "call" and frame.f_code is original_set_cookie.__code__:
        events.append("trace-rebind")
        cookies_module.cookielib = replacement_cookielib
        sys.settrace(None)
    return trace


morsels = SimpleCookie()
morsels["morsel"] = "value"
quoted = create_cookie("quoted", '"value"')
sys.settrace(trace)
try:
    value = cookie_once(jar, "mutate", (morsels["morsel"], quoted, "new"))
finally:
    sys.settrace(None)
    cookies_module.cookielib = original_cookielib

assert_counts(rewrite_compat=0)
assert events[0] == "trace-rebind"
assert "live-cookie" in events
result = (events, value)
"""
    )


def test_bad_create_uses_callback_time_cookielib_alias_in_field_order() -> None:
    _assert_equal(
        """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
original_cookielib = cookies_module.cookielib
original_cookie = original_cookielib.Cookie


def cookie_one(*args, **kwargs):
    events.append("Cookie-one")
    return original_cookie(*args, **kwargs)


def cookie_two(*args, **kwargs):
    events.append("Cookie-two")
    return original_cookie(*args, **kwargs)


module_one = SimpleNamespace(Cookie=cookie_one, deepvalues=original_cookielib.deepvalues)
module_two = SimpleNamespace(Cookie=cookie_two, deepvalues=original_cookielib.deepvalues)


class Port(int):
    def __bool__(self):
        events.append("port-bool")
        cookies_module.cookielib = module_one
        return True


class Domain(str):
    def __bool__(self):
        events.append("domain-bool")
        return True

    def startswith(self, value):
        events.append("domain-startswith")
        return False


class Path(str):
    def __bool__(self):
        events.append("path-bool")
        cookies_module.cookielib = module_two
        return True


try:
    value = cookie_once(
        jar,
        "bad-create",
        (
            "name",
            "value",
            {"port": Port(80), "domain": Domain("a.test"), "path": Path("/")},
        ),
    )
finally:
    cookies_module.cookielib = original_cookielib

assert_counts(rewrite_compat=1)
assert events == [
    "port-bool",
    "domain-bool",
    "domain-startswith",
    "path-bool",
    "Cookie-two",
]
result = (events, value.name, value.value)
"""
    )


def test_bad_create_missing_live_cookielib_has_exact_name_error() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
original_cookielib = cookies_module.cookielib


class Port:
    def __bool__(self):
        events.append("port-bool")
        del cookies_module.cookielib
        return False


try:
    try:
        cookie_once(jar, "bad-create", ("name", "value", {"port": Port()}))
    except BaseException as error:
        observed = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
            getattr(error, "name", None),
        )
finally:
    cookies_module.cookielib = original_cookielib

assert_counts(rewrite_compat=1)
assert events == ["port-bool"]
assert observed == (
    "builtins",
    "NameError",
    ("name 'cookielib' is not defined",),
    "cookielib",
)
result = (events, observed)
"""
    )


def test_exact_pristine_rlock_mutation_remains_native() -> None:
    _assert_equal(
        """
from http.cookies import SimpleCookie

from requests.cookies import RequestsCookieJar, create_cookie

jar = RequestsCookieJar()
assert type(jar._cookies_lock) is type(threading.RLock())
morsels = SimpleCookie()
morsels["morsel"] = "value"
quoted = create_cookie("quoted", '"value"')
value = cookie_once(jar, "mutate", (morsels["morsel"], quoted, "new"))
assert_counts(rewrite_compat=0)
result = value
"""
    )


def test_copy_pickle_rebound_threading_rlock_falls_back_before_effects() -> None:
    _assert_equal(
        """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
original_threading = cookies_module.threading


def live_rlock():
    events.append("live-rlock")
    return original_threading.RLock()


cookies_module.threading = SimpleNamespace(RLock=live_rlock)
try:
    value = cookie_once(jar, "copy-pickle")
finally:
    cookies_module.threading = original_threading

assert_counts(rewrite_compat=1)
assert events
result = (len(events), events, value)
"""
    )


def test_copy_pickle_same_module_rlock_rebind_falls_back_before_effects() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
original_rlock = threading.RLock


def live_rlock():
    events.append("live-rlock")
    return original_rlock()


threading.RLock = live_rlock
try:
    value = cookie_once(jar, "copy-pickle")
finally:
    threading.RLock = original_rlock

assert_counts(rewrite_compat=1)
assert events
result = (len(events), events, value)
"""
    )


def test_copy_pickle_same_rlock_code_mutation_falls_back_before_effects() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
original_rlock = threading.RLock
original_code = original_rlock.__code__


def replacement_code():
    _task15_events.append("live-rlock-code")
    return _task15_original_rlock()


threading._task15_events = events
threading._task15_original_rlock = type(jar._cookies_lock)
original_rlock.__code__ = replacement_code.__code__
try:
    value = cookie_once(jar, "copy-pickle")
finally:
    original_rlock.__code__ = original_code
    del threading._task15_events
    del threading._task15_original_rlock

assert_counts(rewrite_compat=1)
assert events
result = (len(events), events, value)
"""
    )


def test_copy_pickle_custom_policy_falls_back_before_pickle_callback() -> None:
    _assert_equal(
        """
from http.cookiejar import DefaultCookiePolicy

from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")


class CallbackPolicy(DefaultCookiePolicy):
    def __reduce__(self):
        events.append("policy-reduce")
        return (DefaultCookiePolicy, ())


jar.set_policy(CallbackPolicy())
value = cookie_once(jar, "copy-pickle")
assert_counts(rewrite_compat=1)
assert events == ["policy-reduce"]
result = (events, value)
"""
    )


def test_pipeline_response_handles_are_identity_based_and_tagged() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []


class EqualResponse(Response):
    def __eq__(self, other):
        events.append("response-eq")
        return True


first = EqualResponse()
first.raw = response.raw
first.request = request
second = EqualResponse()
second.raw = response.raw
second.request = request


def hook(selected):
    return first


def digest(selected, authoritative):
    return second


value = pipeline_once(jar, request, response, hook, digest, audit)
assert_counts(rewrite_compat=0)
assert value["replacement"] is False
assert events == []
result = (value["replacement"], events, value["audit"])
"""
    )


def test_pipeline_integer_digest_result_cannot_alias_response_id_zero() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
value = pipeline_once(
    jar,
    request,
    response,
    lambda selected: None,
    lambda selected, authoritative: 0,
    audit,
)
assert_counts(rewrite_compat=0)
assert value["replacement"] is False
result = (value["replacement"], type(value["replacement"]) is bool, value["audit"])
"""
    )


def test_pipeline_response_subclass_index_protocol_is_never_used_for_ids() -> None:
    _assert_equal(
        _PIPELINE_SETUP
        + """
events = []


class IndexedResponse(Response):
    def __index__(self):
        events.append("response-index")
        raise AssertionError("response object must remain opaque")


replacement = IndexedResponse()
replacement.raw = response.raw
replacement.request = request
value = pipeline_once(
    jar,
    request,
    response,
    lambda selected: replacement,
    lambda selected, authoritative: replacement,
    audit,
)
assert_counts(rewrite_compat=0)
assert value["replacement"] is True
assert events == []
result = (value["replacement"], events, value["audit"])
"""
    )

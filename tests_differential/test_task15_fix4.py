from __future__ import annotations

from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case
from tests_differential.test_cookies import _COOKIE_TRIAL


def _assert_equal(source: str) -> None:
    case = {"source": dedent(_COOKIE_TRIAL + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    if oracle.observations["exception"] is not None:
        raise AssertionError(
            f"oracle unexpectedly raised {oracle.observations['exception']!r}"
        )
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


@pytest.mark.parametrize("level", ["domain", "path", "name"])
def test_nested_cookie_storage_requires_exact_dicts_at_every_level(level: str) -> None:
    _assert_equal(
        f"""
from requests.cookies import RequestsCookieJar

events = []


class ObservedDict(dict):
    def values(self):
        events.append({level!r})
        return super().values()


jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
if {level!r} == "domain":
    jar._cookies = ObservedDict(jar._cookies)
elif {level!r} == "path":
    jar._cookies["a.test"] = ObservedDict(jar._cookies["a.test"])
else:
    jar._cookies["a.test"]["/"] = ObservedDict(
        jar._cookies["a.test"]["/"]
    )

value = cookie_once(
    jar,
    "inspect",
    (("token", None, None, "missing"),),
)
assert_counts(rewrite_compat=1)
assert events
result = (len(events), value)
"""
    )


@pytest.mark.parametrize(
    ("operation", "expected_rewrite_compat"),
    [
        ("inspect", 1),
        ("mutate", 1),
        ("copy-pickle", 1),
        ("header", 0),
        ("extract-header", 0),
        ("pipeline", 0),
    ],
)
def test_live_deepvalues_is_observed_by_every_iterator_operation(
    operation: str, expected_rewrite_compat: int
) -> None:
    _assert_equal(
        f"""
from http.cookies import SimpleCookie
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
raw = SimpleNamespace(_original_response=None)
response = Response()
response.raw = raw
response.request = request
audit = []

original_deepvalues = cookies_module.cookielib.deepvalues


def observed_deepvalues(mapping):
    events.append(len(mapping))
    yield from original_deepvalues(mapping)


cookies_module.cookielib.deepvalues = observed_deepvalues
try:
    if {operation!r} == "inspect":
        value = cookie_once(
            jar,
            "inspect",
            (("token", None, None, "missing"),),
        )
    elif {operation!r} == "mutate":
        morsels = SimpleCookie()
        morsels["morsel"] = "value"
        quoted = create_cookie("quoted", '"value"')
        value = cookie_once(
            jar, "mutate", (morsels["morsel"], quoted, "new")
        )
    elif {operation!r} == "copy-pickle":
        value = cookie_once(jar, "copy-pickle")
    elif {operation!r} in ("header", "extract-header"):
        value = bridge_once(
            jar, request, raw, {operation!r}, audit
        )
    else:
        value = pipeline_once(
            jar,
            request,
            response,
            lambda selected: selected,
            lambda selected, authoritative: selected,
            audit,
        )
finally:
    cookies_module.cookielib.deepvalues = original_deepvalues

assert_counts(rewrite_compat={expected_rewrite_compat})
assert events
result = (len(events), value)
"""
    )


def test_unsupported_live_deepvalues_yield_preserves_oracle_error() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
original_deepvalues = cookies_module.cookielib.deepvalues


class UnsupportedCookie:
    @property
    def name(self):
        events.append("unsupported-name")
        raise LookupError("unsupported-cookie")


def unsupported_deepvalues(mapping):
    events.append("deepvalues")
    yield UnsupportedCookie()


cookies_module.cookielib.deepvalues = unsupported_deepvalues
try:
    try:
        cookie_once(
            jar,
            "inspect",
            (("token", None, None, "missing"),),
        )
    except BaseException as error:
        observed = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        )
finally:
    cookies_module.cookielib.deepvalues = original_deepvalues

assert_counts(rewrite_compat=1)
assert events == [
    "deepvalues",
    "unsupported-name",
    "deepvalues",
    "unsupported-name",
    "deepvalues",
    "unsupported-name",
]
result = observed
"""
    )


@pytest.mark.parametrize(
    ("operation", "owner", "method"),
    [
        ("inspect", "jar", "keys"),
        ("mutate", "jar", "set_cookie"),
        ("copy-pickle", "jar", "copy"),
        ("header", "jar", "add_cookie_header"),
        ("extract-header", "jar", "extract_cookies"),
        ("pipeline", "request", "prepare_cookies"),
    ],
)
def test_operation_specific_instance_method_shadows_fallback_before_effects(
    operation: str, owner: str, method: str
) -> None:
    _assert_equal(
        f"""
from email.message import Message
from http.cookies import SimpleCookie
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
raw = SimpleNamespace(_original_response=None)
if {operation!r} == "extract-header":
    raw._original_response = SimpleNamespace(msg=Message())
response = Response()
response.raw = raw
response.request = request
audit = []
subject = jar if {owner!r} == "jar" else request
original = getattr(subject, {method!r})


def observed(*args, **kwargs):
    events.append(({owner!r}, {method!r}))
    if {operation!r} == "copy-pickle":
        del subject.__dict__[{method!r}]
    return original(*args, **kwargs)


subject.__dict__[{method!r}] = observed
if {operation!r} == "inspect":
    value = cookie_once(
        jar,
        "inspect",
        (("token", None, None, "missing"),),
    )
elif {operation!r} == "mutate":
    morsels = SimpleCookie()
    morsels["morsel"] = "value"
    quoted = create_cookie("quoted", '"value"')
    value = cookie_once(
        jar, "mutate", (morsels["morsel"], quoted, "new")
    )
elif {operation!r} == "copy-pickle":
    value = cookie_once(jar, "copy-pickle")
elif {operation!r} in ("header", "extract-header"):
    value = bridge_once(jar, request, raw, {operation!r}, audit)
else:
    value = pipeline_once(
        jar,
        request,
        response,
        lambda selected: selected,
        lambda selected, authoritative: selected,
        audit,
    )

assert_counts(rewrite_compat=1)
assert events
result = (events, value)
"""
    )


@pytest.mark.parametrize("shape", ["outer", "row", "name", "domain", "path", "default"])
def test_inspect_callback_capable_argument_shapes_fallback_in_oracle_order(
    shape: str,
) -> None:
    _assert_equal(
        f"""
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")


class Outer(list):
    def __iter__(self):
        events.append("outer-iter")
        return super().__iter__()


class Row(tuple):
    def __iter__(self):
        events.append("row-iter")
        return super().__iter__()


class Compared(str):
    def __new__(cls, value, label):
        instance = super().__new__(cls, value)
        instance.label = label
        return instance

    def __eq__(self, other):
        events.append((self.label, "eq", other))
        return super().__eq__(other)

    __hash__ = str.__hash__


class Default:
    pass


name = (
    Compared("token", "name")
    if {shape!r} == "name"
    else ("missing" if {shape!r} == "default" else "token")
)
domain = (
    Compared("a.test", "domain")
    if {shape!r} == "domain"
    else "a.test"
)
path = Compared("/", "path") if {shape!r} == "path" else "/"
default = Default() if {shape!r} == "default" else "missing"
row = (name, domain, path, default)
if {shape!r} == "row":
    row = Row(row)
queries = [row]
if {shape!r} == "outer":
    queries = Outer(queries)

value = cookie_once(jar, "inspect", queries)
assert_counts(rewrite_compat=1)
if {shape!r} == "outer":
    assert events == ["outer-iter"]
elif {shape!r} == "row":
    assert events == ["row-iter"]
elif {shape!r} in ("name", "domain", "path"):
    assert events
else:
    assert value["lookups"][0][-1] is default
result = (events, value)
"""
    )


@pytest.mark.parametrize("shape", ["name", "value", "key"])
def test_bad_create_exact_name_value_and_key_shape_controls(shape: str) -> None:
    _assert_equal(
        f"""
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()


class Text(str):
    pass


name = Text("name") if {shape!r} == "name" else "name"
value = Text("value") if {shape!r} == "value" else "value"
key = Text("domain") if {shape!r} == "key" else "domain"
kwargs = {{key: "a.test"}}
cookie = cookie_once(jar, "bad-create", (name, value, kwargs))
assert_counts(rewrite_compat=1)
result = (cookie.name, cookie.value, cookie.domain)
"""
    )


def test_bad_create_preserves_port_domain_path_order_and_exception_identity() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
marker = RuntimeError("domain-startswith")
jar = RequestsCookieJar()


class Port:
    def __bool__(self):
        events.append("port-bool")
        return False


class Domain(str):
    def __bool__(self):
        events.append("domain-bool")
        return True

    def startswith(self, prefix, *args):
        events.append(("domain-startswith", prefix, args))
        raise marker


class Path:
    def __bool__(self):
        events.append("path-bool")
        return True


try:
    cookie_once(
        jar,
        "bad-create",
        (
            "name",
            "value",
            {"port": Port(), "domain": Domain(".a.test"), "path": Path()},
        ),
    )
except BaseException as error:
    observed = (
        error is marker,
        type(error).__module__,
        type(error).__qualname__,
        error.args,
    )

assert_counts(rewrite_compat=1)
assert events == [
    "port-bool",
    "domain-bool",
    ("domain-startswith", ".", ()),
]
assert observed[0]
result = (events, observed)
"""
    )


def test_port_truthiness_deleting_cookie_constructor_is_observed_later() -> None:
    _assert_equal(
        """
from requests.cookies import RequestsCookieJar

events = []
jar = RequestsCookieJar()
original_cookie = cookies_module.cookielib.Cookie


class Port:
    def __bool__(self):
        events.append("port-bool")
        del cookies_module.cookielib.Cookie
        return False


try:
    observed = ("no-error",)
    try:
        cookie_once(
            jar,
            "bad-create",
            ("name", "value", {"port": Port()}),
        )
    except BaseException as error:
        observed = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        )
finally:
    cookies_module.cookielib.Cookie = original_cookie

assert_counts(rewrite_compat=1)
assert events == ["port-bool"]
result = observed
"""
    )


def test_original_response_truthiness_deleting_mock_request_is_observed_later() -> None:
    _assert_equal(
        """
from email.message import Message

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest

events = []
jar = RequestsCookieJar()
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
original_mock_request = cookies_module.MockRequest


class Original:
    msg = Message()

    def __bool__(self):
        events.append("original-bool")
        del cookies_module.MockRequest
        return True


class Raw:
    _original_response = Original()


try:
    observed = ("no-error",)
    try:
        bridge_once(jar, request, Raw(), "extract-header", events)
    except BaseException as error:
        observed = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        )
finally:
    cookies_module.MockRequest = original_mock_request

assert_counts(rewrite_compat=0)
assert events == ["original-bool"]
result = observed
"""
    )


@pytest.mark.parametrize(
    "mutation", ["get_cookie_header", "extract_cookies_to_jar", "add_cookie_header"]
)
def test_pipeline_hook_mutations_are_resolved_at_the_later_stage(
    mutation: str,
) -> None:
    _assert_equal(
        f"""
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
response = Response()
response.raw = SimpleNamespace(_original_response=None)
response.request = request
audit = []
original_header = cookies_module.get_cookie_header
original_extract = cookies_module.extract_cookies_to_jar


def observed_header(authoritative, prepared):
    events.append("live-header")
    return "hook-header"


def observed_extract(authoritative, prepared, raw):
    events.append("live-extract")
    authoritative.set("hook-extract", "yes")


original_add_header = jar.add_cookie_header


def observed_add_header(prepared):
    events.append("live-add-cookie-header")
    return original_add_header(prepared)


def hook(selected):
    events.append("hook")
    if {mutation!r} == "get_cookie_header":
        cookies_module.get_cookie_header = observed_header
    elif {mutation!r} == "extract_cookies_to_jar":
        cookies_module.extract_cookies_to_jar = observed_extract
    else:
        jar.__dict__["add_cookie_header"] = observed_add_header
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
    cookies_module.extract_cookies_to_jar = original_extract

assert_counts(rewrite_compat=0)
assert events[0] == "hook"
assert len(events) == 2
result = (events, value)
"""
    )


def test_pipeline_hook_live_deepvalues_control_remains_exact() -> None:
    _assert_equal(
        """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response

events = []
jar = RequestsCookieJar()
jar.set("token", "value", domain="a.test", path="/")
request = PreparedRequest()
request.prepare(method="GET", url="http://a.test/")
response = Response()
response.raw = SimpleNamespace(_original_response=None)
response.request = request
audit = []
original_deepvalues = cookies_module.cookielib.deepvalues


def observed_deepvalues(mapping):
    events.append("deepvalues")
    yield from original_deepvalues(mapping)


def hook(selected):
    cookies_module.cookielib.deepvalues = observed_deepvalues
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
    cookies_module.cookielib.deepvalues = original_deepvalues

assert_counts(rewrite_compat=0)
assert events
result = (len(events), value)
"""
    )

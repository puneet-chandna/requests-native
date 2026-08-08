from __future__ import annotations

from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case
from tests_differential.test_cookies import _COOKIE_TRIAL
from tests_differential.test_hooks_auth import _AUTH_TRIAL


def _assert_equal(prefix: str, source: str) -> None:
    case = {"source": dedent(prefix + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_hook_missing_live_callable_preserves_name_and_does_not_replay() -> None:
    _assert_equal(
        """
import os

import requests.hooks as hooks_module
from requests.hooks import dispatch_hook as compat_dispatch_hook

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
candidate_calls = 0
compat_calls = 0
events = []


class Selected:
    def __bool__(self):
        events.append("selected-bool")
        return True


class Hooks:
    def __bool__(self):
        events.append("hooks-bool")
        return True

    def get(self, key):
        events.append(("hooks-get", key))
        return Selected()


def compat_dispatch():
    global compat_calls
    compat_calls += 1
    return compat_dispatch_hook("response", Hooks(), object())


original_callable = hooks_module.Callable
del hooks_module.Callable
try:
    try:
        if target == "rewrite":
            candidate_calls += 1
            _requests_rust._dispatch_hook_trial(
                compat_dispatch, "response", Hooks(), object(), {}
            )
        else:
            compat_dispatch()
    except BaseException as error:
        observed = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
            getattr(error, "name", None),
        )
finally:
    hooks_module.Callable = original_callable

assert observed[3] == "Callable"
assert events == ["hooks-bool", ("hooks-get", "response"), "selected-bool"]
if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 0
else:
    assert candidate_calls == 0
    assert compat_calls == 1
result = observed
""",
        "",
    )


@pytest.mark.parametrize(
    ("url", "expected_uri", "rewrite_compat_calls"),
    [
        ("http://example.test/path?", "/path", 0),
        ("http://example.test?", "/", 0),
        ("http://[::1]/a;b?x=1#f", "/a?x=1", 0),
        ("http://example.test/a;b/c;d?x=1", "/a;b/c?x=1", 0),
        ("http://example.test/a/../b?", "/a/../b", 1),
    ],
)
def test_digest_request_target_matches_urlparse_path_params_query(
    url: str,
    expected_uri: str,
    rewrite_compat_calls: int,
) -> None:
    _assert_equal(
        _AUTH_TRIAL,
        f"""
import hashlib
import re

subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {{
    "realm": "realm",
    "nonce": "nonce",
    "qop": "auth",
    "algorithm": "MD5",
}}
compat_pre_states = []


def observed_compat_digest():
    global compat_calls
    compat_calls += 1
    compat_pre_states.append(
        (
            subject._thread_local.nonce_count,
            subject._thread_local.last_nonce,
        )
    )
    return subject.build_digest_header("GET", {url!r})


original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    if target == "rewrite":
        candidate_calls += 1
        header = _requests_rust._digest_auth_trial(
            observed_compat_digest,
            subject,
            "build_digest_header",
            ("GET", {url!r}),
        )
    else:
        header = observed_compat_digest()
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

uri = re.search(r'uri="([^"]*)"', header).group(1)
response = re.search(r'response="([0-9a-f]+)"', header).group(1)
ha1 = hashlib.md5(
    b"user:realm:password", usedforsecurity=False
).hexdigest()
ha2 = hashlib.md5(
    ("GET:" + {expected_uri!r}).encode(), usedforsecurity=False
).hexdigest()
cnonce = hashlib.sha1(
    b"1noncefixed-time01234567", usedforsecurity=False
).hexdigest()[:16]
expected_response = hashlib.md5(
    f"{{ha1}}:nonce:00000001:{{cnonce}}:auth:{{ha2}}".encode(),
    usedforsecurity=False,
).hexdigest()

assert uri == {expected_uri!r}
assert response == expected_response
assert (
    subject._thread_local.nonce_count,
    subject._thread_local.last_nonce,
) == (1, "nonce")
if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == {rewrite_compat_calls}
    assert compat_pre_states == (
        [(0, "")] if {rewrite_compat_calls} else []
    )
else:
    assert candidate_calls == 0
    assert compat_calls == 1
    assert compat_pre_states == [(0, "")]
result = {{
    "uri": uri,
    "response": response,
    "nonce-count": subject._thread_local.nonce_count,
    "last-nonce": subject._thread_local.last_nonce,
}}
""",
    )


def test_cookie_snapshot_proof_does_not_read_live_value_descriptor() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
import http.cookiejar

from requests.cookies import RequestsCookieJar, create_cookie


class Value(str):
    pass


events = []
jar = RequestsCookieJar()
jar.set_cookie(create_cookie("name", "value"))


class ObservedValue:
    def __get__(self, instance, owner=None):
        if instance is None:
            return self
        events.append("value-get")
        return Value(instance.__dict__["value"])

    def __set__(self, instance, value):
        instance.__dict__["value"] = value


sentinel = object()
original = vars(http.cookiejar.Cookie).get("value", sentinel)
http.cookiejar.Cookie.value = ObservedValue()
try:
    value = cookie_once(
        jar,
        "inspect",
        (("name", None, None, "missing"),),
    )
    assert_counts(1)
finally:
    if original is sentinel:
        del http.cookiejar.Cookie.value
    else:
        http.cookiejar.Cookie.value = original

assert events == ["value-get"] * 6
result = value
""",
    )


@pytest.mark.parametrize(
    ("method_name", "expected_events"),
    [("__iter__", 3), ("get_dict", 1)],
)
def test_cookie_mutate_proves_operation_specific_live_methods_before_effects(
    method_name: str,
    expected_events: int,
) -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        f"""
from http.cookies import SimpleCookie

from requests.cookies import RequestsCookieJar, create_cookie


events = []
morsel = SimpleCookie()
morsel["token"] = "morsel"
morsel["token"]["domain"] = "a.test"
morsel["token"]["path"] = "/m"
jar = RequestsCookieJar()
quoted = create_cookie(
    "quoted", '"a\\\\\\"b"', domain="a.test", path="/"
)
original_method = getattr(RequestsCookieJar, {method_name!r})


def observed(self, *args, **kwargs):
    events.append({method_name!r})
    return original_method(self, *args, **kwargs)


setattr(RequestsCookieJar, {method_name!r}, observed)
try:
    value = cookie_once(
        jar,
        "mutate",
        (morsel["token"], quoted, "new"),
    )
    assert_counts(1)
finally:
    setattr(RequestsCookieJar, {method_name!r}, original_method)

assert events == [{method_name!r}] * {expected_events}
result = value
""",
    )


def test_cookie_mutate_rows_preserve_rest_identity_and_subclass() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from http.cookies import SimpleCookie

from requests.cookies import RequestsCookieJar, create_cookie


class RestDict(dict):
    pass


morsel = SimpleCookie()
morsel["token"] = "morsel"
morsel["token"]["domain"] = "a.test"
morsel["token"]["path"] = "/m"
jar = RequestsCookieJar()
rest = RestDict({"marker": "yes"})
quoted = create_cookie(
    "quoted",
    '"a\\\\\\"b"',
    domain="a.test",
    path="/",
    rest=rest,
)
value = cookie_once(
    jar,
    "mutate",
    (morsel["token"], quoted, "new"),
)
assert_counts()
quoted_row = next(row for row in value["cookies"] if row[0] == "quoted")
assert quoted_row[7] is quoted._rest
assert type(quoted_row[7]) is RestDict
result = {
    "rest-is-cookie-rest": quoted_row[7] is quoted._rest,
    "rest-type": type(quoted_row[7]).__qualname__,
    "value": value,
}
""",
    )


@pytest.mark.parametrize(
    ("operation", "dependency", "expected_events"),
    [
        ("mutate", "morsel_to_cookie", 1),
        ("mutate", "RequestsCookieJar.set", 4),
        ("mutate", "CookieJar.clear", 1),
        ("copy-pickle", "RequestsCookieJar.__contains__", 2),
    ],
)
def test_cookie_operation_dependency_mutations_fallback_before_effects(
    operation: str,
    dependency: str,
    expected_events: int,
) -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        f"""
import http.cookiejar
from http.cookies import SimpleCookie

from requests.cookies import RequestsCookieJar, create_cookie


events = []
compat_entries = []
jar = RequestsCookieJar()
if {operation!r} == "copy-pickle":
    jar.set_cookie(create_cookie("original", "yes"))

if {dependency!r} == "morsel_to_cookie":
    owner = cookies_module
    name = "morsel_to_cookie"
elif {dependency!r} == "RequestsCookieJar.set":
    owner = RequestsCookieJar
    name = "set"
elif {dependency!r} == "CookieJar.clear":
    owner = http.cookiejar.CookieJar
    name = "clear"
else:
    owner = RequestsCookieJar
    name = "__contains__"

original_dependency = getattr(owner, name)
original_cookie_compat = cookie_compat


def observed_dependency(*args, **kwargs):
    events.append({dependency!r})
    return original_dependency(*args, **kwargs)


def observed_cookie_compat(subject, selected_operation, arguments):
    compat_entries.append([cookie.name for cookie in subject])
    return original_cookie_compat(subject, selected_operation, arguments)


setattr(owner, name, observed_dependency)
cookie_compat = observed_cookie_compat
try:
    if {operation!r} == "mutate":
        morsel = SimpleCookie()
        morsel["token"] = "morsel"
        morsel["token"]["domain"] = "a.test"
        morsel["token"]["path"] = "/m"
        quoted = create_cookie(
            "quoted", '"a\\\\\\"b"', domain="a.test", path="/"
        )
        value = cookie_once(
            jar,
            "mutate",
            (morsel["token"], quoted, "new"),
        )
    else:
        value = cookie_once(jar, "copy-pickle")
    assert_counts(1)
finally:
    cookie_compat = original_cookie_compat
    setattr(owner, name, original_dependency)

expected_entry = [] if {operation!r} == "mutate" else ["original"]
assert compat_entries == [expected_entry]
assert events == [{dependency!r}] * {expected_events}
result = value
""",
    )

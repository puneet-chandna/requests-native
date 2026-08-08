from __future__ import annotations

from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case
from tests_differential.test_cookies import _COOKIE_TRIAL
from tests_differential.test_hooks_auth import _AUTH_TRIAL, _HOOK_TRIAL


def _assert_equal(prefix: str, source: str) -> None:
    case = {"source": dedent(prefix + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_basic_non_string_coercion_can_delete_later_b64encode_lookup() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
original_b64encode = auth_module.b64encode


class Username:
    def __repr__(self):
        side_effects.append("username-repr-deletes-b64encode")
        del auth_module.b64encode
        return "Username()"

    def __str__(self):
        side_effects.append("username-str")
        return "user"


try:
    try:
        value = basic_once(Username(), "password")
    except BaseException as error:
        value = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        )
finally:
    auth_module.b64encode = original_b64encode

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
result = value
""",
    )


def test_digest_401_falsey_live_builder_leaves_authorization_absent() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
from email.message import Message

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response
from requests.structures import CaseInsensitiveDict

subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.pos = object()
subject._thread_local.num_401_calls = 1


def falsey_build(method, url):
    side_effects.append(("falsey-build", method, url))
    return None


class Body:
    def seek(self, position):
        assert position is subject._thread_local.pos
        subject.build_digest_header = falsey_build


class Raw:
    decode_content = False

    def __init__(self):
        original = type("Original", (), {})()
        original.msg = Message()
        self._original_response = original

    def stream(self, chunk_size, decode_content=True):
        return iter(())

    def close(self):
        pass


sent = Response()


class Connection:
    def send(self, prepared, **kwargs):
        sent.request = prepared
        return sent


request = PreparedRequest()
request.method = "GET"
request.url = "https://example.test/path"
request.headers = CaseInsensitiveDict()
request.body = Body()
request._cookies = RequestsCookieJar()
response = Response()
response.status_code = 401
response.headers = CaseInsensitiveDict(
    {"www-authenticate": 'Digest realm="realm", nonce="nonce", qop="auth"'}
)
response.request = request
response.raw = Raw()
response.connection = Connection()

returned = digest_401_once(subject, response, {}, [])
if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 0
result = {
    "returned_is_sent": returned is sent,
    "authorization_present": "Authorization" in returned.request.headers,
    "authorization": returned.request.headers.get("Authorization", "missing"),
    "side_effects": side_effects,
}
""",
    )


def test_digest_401_builder_truthiness_error_precedes_send() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
from email.message import Message

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response
from requests.structures import CaseInsensitiveDict


class TruthFailure(BaseException):
    pass


marker = TruthFailure("truth")
exception_identities = {"marker": marker}
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.pos = object()
subject._thread_local.num_401_calls = 1


class RaisingTruth:
    def __bool__(self):
        side_effects.append("header-bool")
        raise marker


def raising_truth_build(method, url):
    side_effects.append(("build", method, url))
    return RaisingTruth()


class Body:
    def seek(self, position):
        assert position is subject._thread_local.pos
        subject.build_digest_header = raising_truth_build


class Raw:
    decode_content = False

    def __init__(self):
        original = type("Original", (), {})()
        original.msg = Message()
        self._original_response = original

    def stream(self, chunk_size, decode_content=True):
        return iter(())

    def close(self):
        pass


class Connection:
    def send(self, prepared, **kwargs):
        side_effects.append("send")
        sent = Response()
        sent.request = prepared
        return sent


request = PreparedRequest()
request.method = "GET"
request.url = "https://example.test/path"
request.headers = CaseInsensitiveDict()
request.body = Body()
request._cookies = RequestsCookieJar()
response = Response()
response.status_code = 401
response.headers = CaseInsensitiveDict(
    {"www-authenticate": 'Digest realm="realm", nonce="nonce", qop="auth"'}
)
response.request = request
response.raw = Raw()
response.connection = Connection()

result = digest_401_once(subject, response, {}, [])
""",
    )


def test_hook_get_mutation_is_observed_before_live_callable_lookup() -> None:
    _assert_equal(
        _HOOK_TRIAL,
        """
import requests.hooks as hooks_module

original_callable = hooks_module.Callable
original = object()


def hook(value):
    side_effects.append(("hook", value is original))
    return value


class Hooks:
    def __bool__(self):
        side_effects.append("hooks-bool")
        return True

    def get(self, key):
        side_effects.append(("hooks-get", key))
        hooks_module.Callable = int
        return hook


try:
    try:
        value = dispatch_once("response", Hooks(), original)
    except BaseException as error:
        value = (
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        )
finally:
    hooks_module.Callable = original_callable

result = {
    "value_is_original": value is original,
    "value": value,
    "candidate_calls": candidate_calls,
    "side_effects": side_effects,
}
""",
    )


def test_digest_malformed_absolute_looking_url_uses_oracle_request_target() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {
    "realm": "realm",
    "nonce": "nonce",
    "qop": "auth",
    "algorithm": "MD5",
}
original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    value = digest_once(subject, "build_digest_header", "GET", "http://")
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
result = {
    "value": value,
    "nonce_count": subject._thread_local.nonce_count,
    "last_nonce": subject._thread_local.last_nonce,
}
""",
    )


@pytest.mark.parametrize(
    ("url", "rewrite_compat_calls"),
    [
        ("http://example.test", 0),
        ("https://example.test/path?x=1", 0),
        ("http://", 1),
        ("https://", 1),
        ("http:///path", 1),
        ("/relative", 1),
        ("ftp://example.test/path", 1),
        ("http://example.test/a/../b?x=1", 1),
        ("http://example.test/a b?x=1", 1),
    ],
)
def test_digest_url_admission_is_conservative_before_state(
    url: str,
    rewrite_compat_calls: int,
) -> None:
    _assert_equal(
        _AUTH_TRIAL,
        f"""
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {{
    "realm": "realm",
    "nonce": "nonce",
    "qop": "auth",
    "algorithm": "MD5",
}}
original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    value = digest_once(
        subject,
        "build_digest_header",
        "GET",
        {url!r},
    )
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert compat_calls == {rewrite_compat_calls}
result = (
    value,
    subject._thread_local.nonce_count,
    subject._thread_local.last_nonce,
)
""",
    )


def test_exact_cookie_with_non_scalar_rest_selects_compatibility_before_inspect() -> (
    None
):
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar, create_cookie

jar = RequestsCookieJar()
jar.set_cookie(create_cookie("name", "value", rest={"x": object()}))
value = cookie_once(
    jar,
    "inspect",
    (("name", None, None, "missing"),),
)
assert_counts(1)
result = value
""",
    )


@pytest.mark.parametrize(
    "method_name",
    ["__iter__", "iterkeys", "itervalues", "iteritems"],
)
def test_cookie_inspect_transitive_live_method_mutation_selects_compatibility(
    method_name: str,
) -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        f"""
from requests.cookies import RequestsCookieJar, create_cookie

original_method = getattr(RequestsCookieJar, {method_name!r})


def observed(self):
    side_effects.append({method_name!r})
    return original_method(self)


setattr(RequestsCookieJar, {method_name!r}, observed)
try:
    jar = RequestsCookieJar()
    jar.set_cookie(create_cookie("name", "value"))
    value = cookie_once(
        jar,
        "inspect",
        (("name", None, None, "missing"),),
    )
    assert_counts(1)
finally:
    setattr(RequestsCookieJar, {method_name!r}, original_method)

result = value
""",
    )

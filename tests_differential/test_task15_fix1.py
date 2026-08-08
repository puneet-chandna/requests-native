from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case
from tests_differential.test_cookies import _COOKIE_TRIAL
from tests_differential.test_hooks_auth import _AUTH_TRIAL


def _runs(prefix: str, source: str):
    case = {"source": dedent(prefix + source)}
    return run_oracle_case(case), run_rewrite_case(case)


def _assert_equal(prefix: str, source: str) -> None:
    oracle, rewrite = _runs(prefix, source)
    assert oracle.observations == rewrite.observations
    assert oracle.stderr == rewrite.stderr == ""


def test_missing_basic_global_selects_compatibility_once() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
original = auth_module.b64encode
del auth_module.b64encode
try:
    try:
        basic_once("user", "password")
    except BaseException as error:
        result = (type(error).__module__, type(error).__qualname__, error.args)
finally:
    auth_module.b64encode = original

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
""",
    )


def test_basic_non_exact_credentials_select_compatibility_once() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
class Username:
    @property
    def __class__(self):
        side_effects.append("username-isinstance")
        return object

    def __repr__(self):
        side_effects.append("username-repr")
        return "Username()"

    def __str__(self):
        side_effects.append("username-str")
        return "user"


class Password:
    @property
    def __class__(self):
        side_effects.append("password-isinstance")
        return object

    def __str__(self):
        side_effects.append("password-str")
        return "password"


result = basic_once(Username(), Password())
if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
""",
    )


def test_basic_apply_live_class_method_mutation_falls_back_once() -> None:
    _assert_equal(
        _AUTH_TRIAL,
        """
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict

original = auth_module.HTTPBasicAuth.__call__


def observed(self, request):
    global compat_calls
    compat_calls += 1
    side_effects.append("basic-call")
    return original(self, request)


auth_module.HTTPBasicAuth.__call__ = observed
try:
    request = PreparedRequest()
    request.headers = CaseInsensitiveDict()
    result = basic_apply_once(request, "user", "password", False)
finally:
    auth_module.HTTPBasicAuth.__call__ = original

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
""",
    )


def test_digest_live_str_mutation_selects_compatibility_once() -> None:
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
original_str = auth_module.str


def observed_str(value):
    side_effects.append(("str", value))
    return original_str(value)


auth_module.str = observed_str
try:
    try:
        digest_once(subject, "build_digest_header", "GET", "https://example.test/")
    except BaseException as error:
        result = (type(error).__module__, type(error).__qualname__, error.args)
finally:
    auth_module.str = original_str

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
""",
    )


def test_digest_relative_request_target_selects_compatibility_once() -> None:
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
    result = digest_once(subject, "build_digest_header", "GET", "/relative?x=1")
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 1
""",
    )


def test_digest_401_rechecks_live_build_method_after_callbacks_without_replay() -> None:
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


def mutated_build(method, url):
    side_effects.append(("mutated-build", method, url))
    return "Digest callback-selected"


class Body:
    def seek(self, position):
        assert position is subject._thread_local.pos
        subject.build_digest_header = mutated_build


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
original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    returned = digest_401_once(subject, response, {}, [])
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert candidate_calls == 1
    assert compat_calls == 0
result = (
    returned is sent,
    returned.request.headers["Authorization"],
    side_effects,
)
""",
    )


def test_hook_instance_call_attribute_uses_live_callable_isinstance() -> None:
    source = """
import os
import requests.hooks as hooks_module

from requests.hooks import dispatch_hook

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None

target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
candidate_calls = 0
compat_calls = 0


def compat():
    global compat_calls
    compat_calls += 1
    return dispatch_hook("response", {"response": hooks}, original)


class IterableHooks:
    def __iter__(self):
        return iter((hook,))


def hook(value):
    side_effects.append(("hook", value is original))
    return replacement


original = object()
replacement = object()
hooks = IterableHooks()
hooks.__call__ = hook
if target == "rewrite":
    candidate_calls += 1
    result = _requests_rust._dispatch_hook_trial(
        "response", {"response": hooks}, original, {}
    )
    assert candidate_calls == 1
    assert compat_calls == 0
else:
    result = compat()
    assert compat_calls == 1
result = (result is replacement, side_effects)
"""
    _assert_equal("", source)


def test_missing_live_hook_callable_is_authoritative_without_replay() -> None:
    source = """
import os
import requests.hooks as hooks_module

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None

target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
compat_calls = 0
original_callable = hooks_module.Callable


def hook(value):
    return value


def compat():
    global compat_calls
    compat_calls += 1
    return hooks_module.dispatch_hook(
        "response", {"response": hook}, object()
    )


del hooks_module.Callable
try:
    if target == "rewrite":
        _requests_rust._dispatch_hook_trial(
            compat, "response", {"response": hook}, object(), {}
        )
    else:
        compat()
finally:
    hooks_module.Callable = original_callable
    if target == "rewrite":
        assert compat_calls == 0
    else:
        assert compat_calls == 1
"""
    _assert_equal("", source)


def test_live_cookie_constructor_missing_selects_compatibility_once() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar

original = cookies_module.cookielib.Cookie
del cookies_module.cookielib.Cookie
try:
    try:
        cookie_once(RequestsCookieJar(), "bad-create", ("name", "value", {}))
    except BaseException as error:
        result = (type(error).__module__, type(error).__qualname__, error.args)
finally:
    cookies_module.cookielib.Cookie = original

assert_counts(1)
""",
    )


def test_live_cookie_constructor_exception_preserves_exact_graph_and_identity() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar


class ConstructorFailure(BaseException):
    pass


marker = ConstructorFailure("constructor")
exception_identities = {"marker": marker}
original = cookies_module.cookielib.Cookie


def raising_constructor(**kwargs):
    side_effects.append("constructor")
    raise marker


cookies_module.cookielib.Cookie = raising_constructor
try:
    cookie_once(RequestsCookieJar(), "bad-create", ("name", "value", {}))
finally:
    cookies_module.cookielib.Cookie = original
""",
    )


def test_cookie_inspect_live_keys_mutation_selects_compatibility_once() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar, create_cookie

original = RequestsCookieJar.keys


def observed(self):
    side_effects.append("keys")
    return original(self)


RequestsCookieJar.keys = observed
try:
    jar = RequestsCookieJar()
    jar.set_cookie(create_cookie("name", "value"))
    result = cookie_once(jar, "inspect", (("name", None, None, "missing"),))
finally:
    RequestsCookieJar.keys = original

assert_counts(1)
""",
    )


def test_unsupported_cookie_like_snapshot_shape_falls_back_before_effects() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar


class CookieLike:
    name = "name"
    value = "value"
    domain = ""
    path = "/"


jar = RequestsCookieJar()
jar._cookies.setdefault("", {}).setdefault("/", {})["name"] = CookieLike()
result = cookie_once(jar, "inspect", (("name", None, None, "missing"),))
assert_counts(1)
""",
    )


def test_non_none_then_none_duplicate_uses_oracle_sentinel_order() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from requests.cookies import RequestsCookieJar, create_cookie

jar = RequestsCookieJar()
jar.set_cookie(create_cookie("name", "value", domain="a.test", path="/"))
jar.set_cookie(create_cookie("name", None, domain="b.test", path="/"))
result = cookie_once(jar, "inspect", (("name", None, None, "missing"),))
assert_counts()
""",
    )


def test_non_exact_pipeline_policy_falls_back_before_pipeline_effects() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from http.cookiejar import DefaultCookiePolicy
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response


class Policy(DefaultCookiePolicy):
    pass


jar = RequestsCookieJar(policy=Policy())
jar.set_cookie(create_cookie("name", "value"))
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/")
response = Response()
response.request = request
response.raw = SimpleNamespace(_original_response=None)
audit = []


def hook(value):
    return value


def digest(value, authoritative_jar):
    return value


result = pipeline_once(jar, request, response, hook, digest, audit)
assert_counts(1)
""",
    )


def test_none_hook_then_digest_replacement_tracks_hook_selected_response() -> None:
    _assert_equal(
        _COOKIE_TRIAL,
        """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response

jar = RequestsCookieJar()
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/")
original = Response()
original.request = request
original.raw = SimpleNamespace(_original_response=None)
digest_response = Response()
digest_response.request = request
audit = []


def hook(value):
    return None


def digest(value, authoritative_jar):
    return digest_response


result = pipeline_once(jar, request, original, hook, digest, audit)
assert_counts()
""",
    )

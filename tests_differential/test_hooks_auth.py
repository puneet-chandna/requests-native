from __future__ import annotations

from textwrap import dedent

import pytest
from tests_differential.runner import run_oracle_case, run_rewrite_case

_HOOK_TRIAL = """
import os
import threading

from requests.hooks import dispatch_hook as compat_dispatch_hook

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


candidate_calls = 0


def dispatch_once(key, hooks, hook_data, **kwargs):
    global candidate_calls
    candidate_calls += 1
    if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
        return _requests_rust._dispatch_hook_trial(
            key, hooks, hook_data, kwargs
        )
    return compat_dispatch_hook(key, hooks, hook_data, **kwargs)


def deregister_once(subject, event, hook):
    global candidate_calls
    candidate_calls += 1
    if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
        return _requests_rust._deregister_hook_trial(
            subject, event, hook
        )
    return subject.deregister_hook(event, hook)
"""

_AUTH_TRIAL = """
import os

import requests.auth as auth_module
from requests.auth import HTTPDigestAuth

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
candidate_calls = 0
compat_calls = 0


def compat_basic(username, password):
    global compat_calls
    compat_calls += 1
    return auth_module._basic_auth_str(username, password)


def basic_once(username, password):
    global candidate_calls
    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._basic_auth_trial(
            compat_basic, username, password
        )
        assert candidate_calls == 1
        return value
    value = compat_basic(username, password)
    assert compat_calls == 1
    return value


def basic_apply_once(subject, username, password, proxy):
    global candidate_calls
    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._basic_auth_apply_trial(
            subject, username, password, proxy
        )
        assert candidate_calls == 1
        return value
    handler_type = (
        auth_module.HTTPProxyAuth if proxy else auth_module.HTTPBasicAuth
    )
    value = handler_type(username, password)(subject)
    assert candidate_calls == 0
    return value


def prepare_auth_once(subject, auth, url):
    global candidate_calls, compat_calls
    def compat_prepare():
        global compat_calls
        compat_calls += 1
        return subject.prepare_auth(auth, url)

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._prepare_auth_trial(
            compat_prepare, subject, auth, url
        )
        assert candidate_calls == 1
        return value
    value = compat_prepare()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value


def digest_once(subject, operation, *arguments):
    global candidate_calls, compat_calls
    def compat_digest():
        global compat_calls
        compat_calls += 1
        if operation == "build_digest_header":
            return subject.build_digest_header(*arguments)
        if operation == "handle_401":
            return subject.handle_401(arguments[0], **arguments[1])
        raise AssertionError(operation)

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._digest_auth_trial(
            compat_digest, subject, operation, arguments
        )
        assert candidate_calls == 1
        return value
    value = compat_digest()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value


def digest_401_once(subject, response, kwargs, audit):
    global candidate_calls, compat_calls

    def compat_digest_401():
        global compat_calls
        compat_calls += 1
        return subject.handle_401(response, **kwargs)

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._digest_401_trial(
            compat_digest_401, subject, response, kwargs, audit
        )
        assert candidate_calls == 1
        return value
    value = compat_digest_401()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(_HOOK_TRIAL + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def _assert_auth_matches_oracle(source: str) -> None:
    case = {"source": dedent(_AUTH_TRIAL + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_hook_order_falsey_replacement_kwargs_identity_and_origin_thread() -> None:
    _assert_matches_oracle(
        """
class FalseyReplacement:
    def __bool__(self):
        return False


entering_native_id = threading.get_native_id()
marker = object()
original = object()
replacement = FalseyReplacement()
trace = []


def first(value, *, token):
    trace.append(
        (
            "first",
            value is original,
            token is marker,
            threading.get_native_id() == entering_native_id,
        )
    )
    return replacement


def second(value, *, token):
    trace.append(
        (
            "second",
            value is replacement,
            token is marker,
            threading.get_native_id() == entering_native_id,
        )
    )
    return None


final = dispatch_once(
    "response",
    {"response": [first, second]},
    original,
    token=marker,
)
result = {
    "candidate_calls": candidate_calls,
    "final_is_replacement": final is replacement,
    "final_is_falsey": not bool(final),
    "trace": trace,
}
"""
    )


def test_hook_exception_identity_stops_later_callbacks() -> None:
    _assert_matches_oracle(
        """
class HookFailure(Exception):
    pass


marker_error = HookFailure("stop")
original = object()
replacement = object()
trace = []


def first(value, **kwargs):
    trace.append(("first", value is original))
    return replacement


def second(value, **kwargs):
    trace.append(("second", value is replacement))
    raise marker_error


def forbidden(value, **kwargs):
    trace.append(("forbidden",))


try:
    dispatch_once(
        "response",
        {"response": [first, second, forbidden]},
        original,
    )
except HookFailure as error:
    result = {
        "candidate_calls": candidate_calls,
        "error_is_marker": error is marker_error,
        "trace": trace,
    }
"""
    )


def test_hook_list_mutation_and_compat_reentrancy_follow_python_iteration() -> None:
    _assert_matches_oracle(
        """
trace = []
original = object()
outer_replacement = object()
inner_replacement = object()
hooks = []


def inner(value, *, label):
    trace.append(("inner", value is outer_replacement, label))
    return inner_replacement


def appended(value, **kwargs):
    trace.append(("appended", value is outer_replacement))


def first(value, **kwargs):
    trace.append(("first", value is original))
    hooks.append(appended)
    nested = compat_dispatch_hook(
        "response",
        {"response": inner},
        outer_replacement,
        label="nested",
    )
    trace.append(("nested-return", nested is inner_replacement))
    return outer_replacement


hooks.append(first)
final = dispatch_once("response", {"response": hooks}, original)
result = {
    "candidate_calls": candidate_calls,
    "final_is_outer": final is outer_replacement,
    "trace": trace,
}
"""
    )


def test_deregister_hook_preserves_raising_equality_identity_and_stop_order() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class RaisingEquality:
    def __init__(self, label, error=None):
        self.label = label
        self.error = error

    def __eq__(self, other):
        side_effects.append(("eq", self.label))
        if self.error is not None:
            raise self.error
        return False


class EqualityFailure(Exception):
    pass


marker_error = EqualityFailure("stop")
subject = PreparedRequest()
subject.hooks["response"] = [
    RaisingEquality("first"),
    RaisingEquality("raising", marker_error),
    RaisingEquality("forbidden"),
]
target_hook = object()
try:
    deregister_once(subject, "response", target_hook)
except EqualityFailure as error:
    result = {
        "candidate_calls": candidate_calls,
        "error_is_marker": error is marker_error,
        "remaining": len(subject.hooks["response"]),
    }
"""
    )


def test_basic_auth_str_and_bytes_are_native_compatible() -> None:
    _assert_auth_matches_oracle(
        """
result = {
    "text": basic_once("Aladdin", "open sesame"),
}
"""
    )
    _assert_auth_matches_oracle(
        """
result = {
    "bytes": basic_once(b"Aladdin", b"open sesame"),
}
"""
    )


def test_basic_auth_legacy_coercion_warning_and_evaluation_order() -> None:
    _assert_auth_matches_oracle(
        """
class Username:
    def __repr__(self):
        side_effects.append("username-repr")
        return "Username()"

    def __str__(self):
        side_effects.append("username-str")
        return "user"


class Password:
    def __str__(self):
        side_effects.append("password-str")
        return "password"


result = basic_once(Username(), Password())
"""
    )


def test_basic_auth_live_global_mutation_uses_exact_compatibility_fallback() -> None:
    _assert_auth_matches_oracle(
        """
original = auth_module.b64encode


def observed_b64encode(value):
    side_effects.append(("b64encode", value))
    return original(value)


auth_module.b64encode = observed_b64encode
try:
    result = basic_once("user", "password")
finally:
    auth_module.b64encode = original

if target == "rewrite":
    assert compat_calls == 1
"""
    )


def test_basic_auth_class_apply_header_slot_and_return_identity() -> None:
    _assert_auth_matches_oracle(
        """
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


subject = PreparedRequest()
subject.headers = CaseInsensitiveDict()
returned = basic_apply_once(subject, "Aladdin", "open sesame", False)
result = {
    "returned_is_subject": returned is subject,
    "headers": list(subject.headers.items()),
}
"""
    )
    _assert_auth_matches_oracle(
        """
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


subject = PreparedRequest()
subject.headers = CaseInsensitiveDict()
returned = basic_apply_once(subject, b"user", b"password", True)
result = {
    "returned_is_subject": returned is subject,
    "headers": list(subject.headers.items()),
}
"""
    )


def test_prepare_auth_arbitrary_callable_identity_update_and_length_recompute() -> None:
    _assert_auth_matches_oracle(
        """
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


sentinel = object()
subject = PreparedRequest()
subject.method = "POST"
subject.url = "https://example.test/"
subject.headers = CaseInsensitiveDict()
subject.body = b"old"


class ArbitraryAuth:
    def __call__(self, request):
        side_effects.append(("auth", request is subject))
        replacement = PreparedRequest()
        replacement.__dict__.update(request.__dict__)
        replacement.body = b"new-body"
        replacement.auth_marker = sentinel
        return replacement


returned = prepare_auth_once(subject, ArbitraryAuth(), "")
if target == "rewrite":
    assert compat_calls == 1
result = {
    "returned": returned,
    "marker_is_sentinel": subject.auth_marker is sentinel,
    "body": subject.body,
    "content_length": subject.headers["Content-Length"],
}
"""
    )


@pytest.mark.parametrize(
    "algorithm",
    ["MD5", "MD5-SESS", "SHA", "SHA-256", "SHA-512"],
)
def test_digest_header_algorithms_nonce_and_deterministic_globals(
    algorithm: str,
) -> None:
    _assert_auth_matches_oracle(
        f"""
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {{
    "realm": "realm",
    "nonce": "nonce",
    "qop": "auth",
    "algorithm": {algorithm!r},
    "opaque": "opaque",
}}

original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    header = digest_once(
        subject,
        "build_digest_header",
        "GET",
        "https://example.test/path?x=1",
    )
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

result = {{
    "header": header,
    "last_nonce": subject._thread_local.last_nonce,
    "nonce_count": subject._thread_local.nonce_count,
}}
"""
    )


def test_digest_unsupported_qop_preserves_partial_nonce_mutation() -> None:
    _assert_auth_matches_oracle(
        """
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.last_nonce = "old"
subject._thread_local.nonce_count = 4
subject._thread_local.chal = {
    "realm": "realm",
    "nonce": "new",
    "qop": "auth-int",
    "algorithm": "MD5",
}

original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    header = digest_once(
        subject,
        "build_digest_header",
        "POST",
        "https://example.test/",
    )
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

result = {
    "header": header,
    "last_nonce": subject._thread_local.last_nonce,
    "nonce_count": subject._thread_local.nonce_count,
}
"""
    )


def test_digest_missing_challenge_key_preserves_exact_exception_and_state() -> None:
    _assert_auth_matches_oracle(
        """
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {"nonce": "nonce"}
try:
    digest_once(
        subject,
        "build_digest_header",
        "GET",
        "https://example.test/",
    )
except KeyError as error:
    result = {
        "args": error.args,
        "nonce_count": subject._thread_local.nonce_count,
        "last_nonce": subject._thread_local.last_nonce,
    }
"""
    )


def test_digest_shared_auth_keeps_main_and_worker_thread_state_isolated() -> None:
    _assert_auth_matches_oracle(
        """
import threading


subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.chal = {"realm": "main", "nonce": "main"}
subject._thread_local.last_nonce = "main"
subject._thread_local.nonce_count = 7
worker_result = {}

original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"


def worker():
    subject.init_per_thread_state()
    subject._thread_local.chal = {
        "realm": "worker",
        "nonce": "worker",
        "algorithm": "MD5",
    }
    worker_result["header"] = digest_once(
        subject,
        "build_digest_header",
        "GET",
        "https://example.test/worker",
    )
    worker_result["state"] = (
        subject._thread_local.last_nonce,
        subject._thread_local.nonce_count,
    )


thread = threading.Thread(target=worker)
thread.start()
thread.join()
auth_module.time.ctime = original_ctime
auth_module.os.urandom = original_urandom

result = {
    "worker": worker_result,
    "main": (
        subject._thread_local.chal,
        subject._thread_local.last_nonce,
        subject._thread_local.nonce_count,
    ),
}
"""
    )


def test_digest_401_compat_lane_preserves_seek_and_scripted_resend_order() -> None:
    _assert_auth_matches_oracle(
        """
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.pos = 3
subject._thread_local.num_401_calls = 1


class Body:
    def seek(self, position):
        side_effects.append(("seek", position))


class Headers(dict):
    def get(self, key, default=None):
        side_effects.append(("challenge", key))
        return super().get(key, default)


class Prepared:
    def __init__(self):
        self.body = Body()
        self._cookies = object()
        self.method = "GET"
        self.url = "https://example.test/"
        self.headers = {}

    def copy(self):
        side_effects.append("copy")
        copied = Prepared()
        copied.body = self.body
        copied._cookies = self._cookies
        return copied

    def prepare_cookies(self, jar):
        side_effects.append(("prepare-cookies", jar is self._cookies))


class History(list):
    def append(self, value):
        side_effects.append(("history", value is response))
        return super().append(value)


class SentResponse:
    def __init__(self):
        object.__setattr__(self, "history", History())

    def __setattr__(self, name, value):
        if name == "request":
            side_effects.append(("request", value.headers["Authorization"]))
        object.__setattr__(self, name, value)


class Connection:
    def send(self, request, **kwargs):
        side_effects.append(("send", kwargs, request.headers["Authorization"]))
        return SentResponse()


class Response:
    status_code = 401

    def __init__(self):
        self.request = Prepared()
        self.headers = Headers(
            {"www-authenticate": 'Digest realm="realm", nonce="nonce"'}
        )
        self.raw = object()
        self.connection = Connection()

    @property
    def content(self):
        side_effects.append("content")
        return b""

    def close(self):
        side_effects.append("close")


response = Response()
original_extract = auth_module.extract_cookies_to_jar
original_build = subject.build_digest_header


def extract(jar, request, raw):
    side_effects.append(("extract", jar is response.request._cookies))


def build(method, url):
    side_effects.append(("build", method, url))
    return "Digest token"


auth_module.extract_cookies_to_jar = extract
subject.build_digest_header = build
try:
    returned = digest_once(
        subject,
        "handle_401",
        response,
        {"stream": True},
    )
finally:
    auth_module.extract_cookies_to_jar = original_extract
    subject.build_digest_header = original_build

if target == "rewrite":
    assert compat_calls == 1
result = {
    "returned_is_new": returned is not response,
    "num_401_calls": subject._thread_local.num_401_calls,
}
"""
    )


def test_digest_401_admitted_candidate_uses_ordered_origin_actions() -> None:
    _assert_auth_matches_oracle(
        """
from email.message import Message

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response
from requests.structures import CaseInsensitiveDict


audit = []
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
subject._thread_local.pos = object()
subject._thread_local.num_401_calls = 1


class Body:
    def seek(self, position):
        if target == "oracle":
            audit.append("seek")
        assert position is subject._thread_local.pos


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


class AuditHeaders(CaseInsensitiveDict):
    def __setitem__(self, key, value):
        if target == "oracle" and key == "Authorization":
            audit.append("header")
        return super().__setitem__(key, value)


class ChallengeHeaders(CaseInsensitiveDict):
    def get(self, key, default=None):
        if target == "oracle":
            audit.append("challenge")
        return super().get(key, default)


class OraclePrepared(PreparedRequest):
    def copy(self):
        audit.append("copy")
        copied = super().copy()
        copied.__class__ = OraclePrepared
        copied.headers = AuditHeaders(copied.headers)
        return copied

    def prepare_cookies(self, jar):
        audit.append("prepare-cookies")
        return super().prepare_cookies(jar)


class OracleResponse(Response):
    @property
    def content(self):
        audit.append("content")
        return Response.content.__get__(self, OracleResponse)

    def close(self):
        audit.append("close")
        return super().close()


class OracleHistory(list):
    def append(self, value):
        audit.append("history")
        return super().append(value)


class OracleSentResponse(Response):
    def __init__(self):
        object.__setattr__(self, "_audit_ready", False)
        super().__init__()
        self.history = OracleHistory()
        object.__setattr__(self, "_audit_ready", True)

    def __setattr__(self, name, value):
        if (
            name == "request"
            and getattr(self, "_audit_ready", False)
            and target == "oracle"
        ):
            audit.append("request")
        return super().__setattr__(name, value)


sent = OracleSentResponse() if target == "oracle" else Response()


class Connection:
    def send(self, request, **kwargs):
        if target == "oracle":
            audit.append("send")
        assert kwargs == {"stream": True}
        return sent


request_type = OraclePrepared if target == "oracle" else PreparedRequest
response_type = OracleResponse if target == "oracle" else Response
request_headers_type = AuditHeaders if target == "oracle" else CaseInsensitiveDict
challenge_headers_type = (
    ChallengeHeaders if target == "oracle" else CaseInsensitiveDict
)
request = request_type()
request.method = "GET"
request.url = "https://example.test/path"
request.headers = request_headers_type()
request.body = Body()
request._cookies = RequestsCookieJar()
response = response_type()
response.status_code = 401
response.headers = challenge_headers_type(
    {
        "www-authenticate": (
            'Digest realm="realm", nonce="nonce", '
            'qop="auth", algorithm="MD5"'
        )
    }
)
response.request = request
response.raw = Raw()
response.connection = Connection()

original_extract = auth_module.extract_cookies_to_jar
original_build = subject.build_digest_header
original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom


def oracle_extract(jar, prepared, raw):
    audit.append("extract")
    return original_extract(jar, prepared, raw)


def oracle_build(method, url):
    audit.append("build")
    return original_build(method, url)


if target == "oracle":
    auth_module.extract_cookies_to_jar = oracle_extract
    subject.build_digest_header = oracle_build
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    returned = digest_401_once(
        subject,
        response,
        {"stream": True},
        audit,
    )
finally:
    auth_module.extract_cookies_to_jar = original_extract
    subject.build_digest_header = original_build
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert compat_calls == 0
result = {
    "returned_is_sent": returned is sent,
    "request_replaced": returned.request is not request,
    "history": returned.history == [response],
    "authorization": returned.request.headers["Authorization"],
    "num_401_calls": subject._thread_local.num_401_calls,
    "audit": audit,
}
"""
    )


def test_digest_401_admitted_send_error_keeps_exact_error_and_partial_state() -> None:
    _assert_auth_matches_oracle(
        """
from email.message import Message

from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response
from requests.structures import CaseInsensitiveDict


class SendFailure(BaseException):
    pass


marker_error = SendFailure("send failed")
audit = []
subject = HTTPDigestAuth("user", "password")
subject.init_per_thread_state()
cursor = object()
subject._thread_local.pos = cursor
subject._thread_local.num_401_calls = 1


class Body:
    def seek(self, position):
        assert position is cursor


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
    def __init__(self):
        self.calls = 0
        self.prepared = None

    def send(self, prepared, **kwargs):
        self.calls += 1
        self.prepared = prepared
        assert kwargs == {"stream": True}
        raise marker_error


request = PreparedRequest()
request.method = "GET"
request.url = "https://example.test/path"
request.headers = CaseInsensitiveDict()
request.body = Body()
request._cookies = RequestsCookieJar()
connection = Connection()
response = Response()
response.status_code = 401
response.headers = CaseInsensitiveDict(
    {
        "www-authenticate": (
            'Digest realm="realm", nonce="nonce", '
            'qop="auth", algorithm="MD5"'
        )
    }
)
response.request = request
response.raw = Raw()
response.connection = connection

original_ctime = auth_module.time.ctime
original_urandom = auth_module.os.urandom
auth_module.time.ctime = lambda: "fixed-time"
auth_module.os.urandom = lambda size: b"01234567"
try:
    try:
        digest_401_once(
            subject,
            response,
            {"stream": True},
            audit,
        )
    except SendFailure as error:
        caught_is_marker = error is marker_error
finally:
    auth_module.time.ctime = original_ctime
    auth_module.os.urandom = original_urandom

if target == "rewrite":
    assert compat_calls == 0
    assert audit == [
        "seek",
        "challenge",
        "content",
        "close",
        "copy",
        "extract",
        "prepare-cookies",
        "build",
        "header",
        "send",
    ]
result = {
    "caught_is_marker": caught_is_marker,
    "calls": connection.calls,
    "num_401_calls": subject._thread_local.num_401_calls,
    "last_nonce": subject._thread_local.last_nonce,
    "authorization": connection.prepared.headers["Authorization"],
    "original_untouched": "Authorization" not in request.headers,
}
"""
    )

from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case

_COOKIE_TRIAL = """
import os
import pickle
import threading

import requests.cookies as cookies_module

try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


target = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]
candidate_calls = 0
compat_calls = 0


def assert_counts(rewrite_compat=0):
    if target == "rewrite":
        assert candidate_calls == 1
        assert compat_calls == rewrite_compat
    else:
        assert candidate_calls == 0
        assert compat_calls == 1


def cookie_once(jar, operation, arguments=()):
    global candidate_calls, compat_calls

    def compat_cookie():
        global compat_calls
        compat_calls += 1
        return cookie_compat(jar, operation, arguments)

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._cookie_jar_trial(
            compat_cookie, jar, operation, arguments
        )
        assert candidate_calls == 1
        return value
    value = compat_cookie()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value


def cookie_compat(jar, operation, arguments):
    if operation == "inspect":
        names = arguments
        lookups = []
        for name, domain, path, default in names:
            try:
                item = jar[name]
            except BaseException as error:
                item = (type(error).__name__, error.args)
            try:
                selected = jar.get(
                    name, default, domain=domain, path=path
                )
            except BaseException as error:
                selected = (type(error).__name__, error.args)
            lookups.append((name, domain, path, item, selected))
        return {
            "cookies": [
                (c.name, c.value, c.domain, c.path) for c in jar
            ],
            "keys": jar.keys(),
            "values": jar.values(),
            "items": jar.items(),
            "lookups": lookups,
            "dict": jar.get_dict(),
            "a-root": jar.get_dict(domain="a.test", path="/"),
            "domains": jar.list_domains(),
            "paths": jar.list_paths(),
            "multiple": jar.multiple_domains(),
        }
    if operation == "mutate":
        morsel, quoted, replacement = arguments
        converted = cookies_module.morsel_to_cookie(morsel)
        jar.set_cookie(converted)
        jar.set_cookie(quoted)
        jar.set("replace", "old", domain="a.test", path="/")
        jar.set("replace", replacement, domain="a.test", path="/")
        jar.set("remove", "gone", domain="a.test", path="/")
        jar.set("remove", None, domain="a.test", path="/")
        return {
            "cookies": [
                (
                    c.name,
                    c.value,
                    c.domain,
                    c.path,
                    c.secure,
                    c.expires,
                    c.discard,
                    c._rest,
                )
                for c in jar
            ],
            "dict": jar.get_dict(),
        }
    if operation == "bad-create":
        name, value, kwargs = arguments
        return cookies_module.create_cookie(name, value, **kwargs)
    if operation == "morsel":
        cookie = cookies_module.morsel_to_cookie(arguments[0])
        return (
            cookie.name,
            cookie.value,
            cookie.expires,
            cookie.discard,
        )
    if operation == "copy-pickle":
        copied = jar.copy()
        original_cookie = next(iter(jar))
        copied_cookie = next(iter(copied))
        copied.set("copy-only", "yes")
        state = jar.__getstate__()
        restored = pickle.loads(pickle.dumps(jar))
        return {
            "copy-policy": copied.get_policy() is jar.get_policy(),
            "copy-cookie-object": copied_cookie is original_cookie,
            "copy-isolation": "copy-only" not in jar and "copy-only" in copied,
            "lock-omitted": "_cookies_lock" not in state,
            "restored-lock-new": restored._cookies_lock is not jar._cookies_lock,
            "restored-policy-type": type(restored.get_policy()).__qualname__,
            "restored": [
                (c.name, c.value, c.domain, c.path) for c in restored
            ],
        }
    raise AssertionError(operation)


def bridge_once(jar, request, response, operation, audit):
    global candidate_calls, compat_calls

    def compat_bridge():
        global compat_calls
        compat_calls += 1
        return bridge_compat(jar, request, response, operation, audit)

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._cookie_bridge_trial(
            compat_bridge, jar, request, response, operation, audit
        )
        assert candidate_calls == 1
        return value
    value = compat_bridge()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value


def bridge_compat(jar, request, response, operation, audit):
    if operation == "header":
        value = cookies_module.get_cookie_header(jar, request)
        return value, [
            (c.name, c.value, c.domain, c.path) for c in jar
        ]
    if operation == "extract-header":
        cookies_module.extract_cookies_to_jar(jar, request, response)
        value = cookies_module.get_cookie_header(jar, request)
        return value, [
            (c.name, c.value, c.domain, c.path) for c in jar
        ]
    raise AssertionError(operation)


def pipeline_once(jar, request, response, hook, digest, audit):
    global candidate_calls, compat_calls

    def compat_pipeline():
        global compat_calls
        compat_calls += 1
        return pipeline_compat(
            jar, request, response, hook, digest, audit
        )

    if target == "rewrite":
        candidate_calls += 1
        value = _requests_rust._cookie_pipeline_trial(
            compat_pipeline,
            jar,
            request,
            response,
            hook,
            digest,
            audit,
        )
        assert candidate_calls == 1
        return value
    value = compat_pipeline()
    assert candidate_calls == 0
    assert compat_calls == 1
    return value


def pipeline_compat(jar, request, response, hook, digest, audit):
    snapshots = []

    def resnapshot():
        snapshots.append(
            [(c.name, c.value, c.domain, c.path) for c in jar]
        )

    audit.append("prepare")
    request.prepare_cookies(jar)
    resnapshot()
    audit.append("hook")
    replacement = hook(response)
    if replacement is None:
        replacement = response
    resnapshot()
    audit.append("extract")
    cookies_module.extract_cookies_to_jar(
        jar, request, replacement.raw
    )
    resnapshot()
    audit.append("digest")
    final = digest(replacement, jar)
    resnapshot()
    audit.append("header")
    header = cookies_module.get_cookie_header(jar, request)
    resnapshot()
    return {
        "replacement": final is replacement,
        "request-jar": request._cookies is jar,
        "header": header,
        "cookies": [
            (c.name, c.value, c.domain, c.path) for c in jar
        ],
        "audit": list(audit),
        "snapshots": snapshots,
    }
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(_COOKIE_TRIAL + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_builtin_duplicate_domain_path_none_order_and_mapping_oddities() -> None:
    _assert_matches_oracle(
        """
from requests.cookies import RequestsCookieJar, create_cookie

jar = RequestsCookieJar()
for cookie in (
    create_cookie("sid", "root", domain="a.test", path="/"),
    create_cookie("sid", "nested", domain="a.test", path="/nested"),
    create_cookie("sid", "other", domain="b.test", path="/"),
    create_cookie("empty", None, domain="a.test", path="/"),
    create_cookie("tail", "last", domain="a.test", path="/"),
):
    jar.set_cookie(cookie)

result = cookie_once(
    jar,
    "inspect",
    (
        ("sid", "a.test", "/", "missing"),
        ("sid", None, None, "missing"),
        ("empty", "a.test", "/", "missing"),
        ("absent", None, None, "missing"),
    ),
)
assert_counts()
"""
    )


def test_morsel_quotes_expiry_mutation_and_unknown_create_keyword() -> None:
    _assert_matches_oracle(
        """
from http.cookies import SimpleCookie
from requests.cookies import RequestsCookieJar, create_cookie

morsel = SimpleCookie()
morsel["token"] = "morsel"
morsel["token"]["domain"] = "a.test"
morsel["token"]["path"] = "/m"
morsel["token"]["secure"] = True
morsel["token"]["expires"] = "Wed, 09-Jun-2027 10:18:14 GMT"
morsel["token"]["httponly"] = True

jar = RequestsCookieJar()
quoted = create_cookie(
    "quoted", '"a\\\\\\"b"', domain="a.test", path="/"
)
mutation = cookie_once(
    jar,
    "mutate",
    (morsel["token"], quoted, "new"),
)
result = mutation
assert_counts()
"""
    )


def test_create_cookie_unknown_keywords_preserve_error_surface() -> None:
    _assert_matches_oracle(
        """
from requests.cookies import RequestsCookieJar

jar = RequestsCookieJar()
try:
    cookie_once(
        jar,
        "bad-create",
        ("bad", "value", {"zeta": 1, "alpha": 2}),
    )
except BaseException as error:
    bad = (
        type(error).__module__,
        type(error).__qualname__,
        error.args,
    )
else:
    bad = None

result = bad
assert_counts()
"""
    )


def test_live_time_global_uses_exactly_one_compatibility_call() -> None:
    _assert_matches_oracle(
        """
from http.cookies import SimpleCookie
from requests.cookies import RequestsCookieJar

morsel = SimpleCookie()
morsel["short"] = "value"
morsel["short"]["max-age"] = "5"
original_time = cookies_module.time.time
cookies_module.time.time = lambda: 1000
try:
    result = cookie_once(
        RequestsCookieJar(), "morsel", (morsel["short"],)
    )
    assert_counts(1)
finally:
    cookies_module.time.time = original_time
"""
    )


def test_copy_pickle_lock_policy_identity_and_mutation_isolation() -> None:
    _assert_matches_oracle(
        """
from requests.cookies import RequestsCookieJar, create_cookie

jar = RequestsCookieJar()
jar.set_cookie(
    create_cookie("original", "yes", domain="a.test", path="/")
)
result = cookie_once(jar, "copy-pickle")
assert_counts()
"""
    )


def test_generic_external_jar_header_extract_and_direct_nested_mutation() -> None:
    _assert_matches_oracle(
        """
from email.message import Message
from http.cookiejar import CookieJar
from types import SimpleNamespace

from requests.cookies import create_cookie
from requests.models import PreparedRequest

entering_native_id = threading.get_native_id()
jar = CookieJar()
direct = create_cookie(
    "direct", "before", domain="example.test", path="/"
)
jar._cookies.setdefault("example.test", {}).setdefault("/", {})[
    "direct"
] = direct

request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/path")
message = Message()
message.add_header(
    "Set-Cookie", "server=after; Domain=example.test; Path=/"
)
raw = SimpleNamespace(
    _original_response=SimpleNamespace(msg=message)
)
audit = []
result = bridge_once(
    jar, request, raw, "extract-header", audit
)
assert threading.get_native_id() == entering_native_id
assert_counts()
"""
    )


def test_custom_policy_subclass_reentrancy_falls_back_once_on_origin_thread() -> None:
    _assert_matches_oracle(
        """
from http.cookiejar import DefaultCookiePolicy

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest

entering_native_id = threading.get_native_id()
nested_jar = RequestsCookieJar()
nested_jar.set_cookie(create_cookie("nested", "yes"))


class Policy(DefaultCookiePolicy):
    def return_ok(self, cookie, request):
        nested = _requests_rust._cookie_jar_trial(
            lambda: (_ for _ in ()).throw(
                AssertionError("nested fallback")
            ),
            nested_jar,
            "inspect",
            (("nested", None, None, "missing"),),
        ) if target == "rewrite" else cookie_compat(
            nested_jar,
            "inspect",
            (("nested", None, None, "missing"),),
        )
        audit.append(
            (
                "policy",
                threading.get_native_id() == entering_native_id,
                nested["dict"],
            )
        )
        return super().return_ok(cookie, request)


jar = RequestsCookieJar(policy=Policy())
jar.set_cookie(
    create_cookie("custom", "yes", domain="example.test", path="/")
)
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/path")
audit = []
result = bridge_once(jar, request, None, "header", audit), list(audit)
assert_counts(1)
"""
    )


def test_cookie_jar_subclass_falls_back_once() -> None:
    _assert_matches_oracle(
        """
from requests.cookies import RequestsCookieJar, create_cookie


class Jar(RequestsCookieJar):
    pass


jar = Jar()
jar.set_cookie(create_cookie("subclass", "yes"))
result = cookie_once(
    jar,
    "inspect",
    (("subclass", None, None, "missing"),),
)
assert_counts(1)
"""
    )


def test_private_hook_digest_cookie_replacement_and_writeback_pipeline() -> None:
    _assert_matches_oracle(
        """
from email.message import Message
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response

entering_native_id = threading.get_native_id()
jar = RequestsCookieJar()
jar.set_cookie(
    create_cookie("initial", "one", domain="example.test", path="/")
)
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/path")
original = Response()
original.request = request
replacement = Response()
replacement.request = request
message = Message()
message.add_header(
    "Set-Cookie", "server=two; Domain=example.test; Path=/"
)
replacement.raw = SimpleNamespace(
    _original_response=SimpleNamespace(msg=message)
)
audit = []


def hook(response):
    audit.append(
        ("hook-callback", response is original,
         threading.get_native_id() == entering_native_id)
    )
    return replacement


def digest(response, authoritative_jar):
    audit.append(
        ("digest-callback", response is replacement,
         authoritative_jar is jar,
         threading.get_native_id() == entering_native_id)
    )
    authoritative_jar.set_cookie(
        create_cookie(
            "digest", "three", domain="example.test", path="/"
        )
    )
    return response


result = pipeline_once(
    jar, request, original, hook, digest, audit
)
assert_counts()
"""
    )


def test_private_pipeline_resnapshots_each_cookie_policy_observable_call() -> None:
    _assert_matches_oracle(
        """
from email.message import Message
from http.cookiejar import DefaultCookiePolicy
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response

jar = None
policy_calls = 0


class MutatingPolicy(DefaultCookiePolicy):
    def return_ok(self, cookie, policy_request):
        global policy_calls
        policy_calls += 1
        stage = "prepare" if policy_calls == 1 else "header"
        jar._cookies["example.test"]["/"]["initial"].value = stage
        audit.append(("policy-return", stage))
        return super().return_ok(cookie, policy_request)

    def set_ok(self, cookie, policy_request):
        jar._cookies["example.test"]["/"]["initial"].value = "extract"
        audit.append(("policy-set", cookie.name))
        return super().set_ok(cookie, policy_request)


jar = RequestsCookieJar(policy=MutatingPolicy())
jar.set_cookie(
    create_cookie("initial", "one", domain="example.test", path="/")
)
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/path")
response = Response()
response.request = request
message = Message()
message.add_header(
    "Set-Cookie", "server=two; Domain=example.test; Path=/"
)
response.raw = SimpleNamespace(
    _original_response=SimpleNamespace(msg=message)
)
audit = []


def hook(value):
    audit.append("hook-callback")
    return value


def digest(value, authoritative_jar):
    audit.append(("digest-callback", authoritative_jar is jar))
    request.headers.pop("Cookie", None)
    return value


result = pipeline_once(
    jar, request, response, hook, digest, audit
)
assert_counts(1)
"""
    )


def test_private_pipeline_none_hook_never_indexes_a_missing_replacement() -> None:
    _assert_matches_oracle(
        """
from types import SimpleNamespace

from requests.cookies import RequestsCookieJar, create_cookie
from requests.models import PreparedRequest, Response

jar = RequestsCookieJar()
jar.set_cookie(create_cookie("only", "one"))
request = PreparedRequest()
request.prepare(method="GET", url="https://example.test/path")
response = Response()
response.request = request
response.raw = SimpleNamespace(_original_response=None)
audit = []


def hook(value):
    audit.append(("hook-none", value is response))
    return None


def digest(value, authoritative_jar):
    audit.append(
        ("digest-original", value is response, authoritative_jar is jar)
    )
    return value


result = pipeline_once(
    jar, request, response, hook, digest, audit
)
assert_counts()
"""
    )

from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case

_TRIAL_HELPERS = """
try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


_TRIAL_SYMBOLS = {
    "_prepare_method_trial",
    "_prepare_url_trial",
    "_prepare_headers_trial",
    "_prepared_fields_snapshot",
}


def prepare_method_call(subject, method):
    if _requests_rust is not None:
        return _requests_rust._prepare_method_trial(subject, method)
    return subject.prepare_method(method)


def prepare_url_call(subject, url, params):
    if _requests_rust is not None:
        return _requests_rust._prepare_url_trial(subject, url, params)
    return subject.prepare_url(url, params)


def prepare_headers_call(subject, headers):
    if _requests_rust is not None:
        return _requests_rust._prepare_headers_trial(subject, headers)
    return subject.prepare_headers(headers)


def prepared_fields_snapshot(subject):
    if _requests_rust is not None:
        return _requests_rust._prepared_fields_snapshot(subject)
    headers = subject.headers
    return {
        "method": subject.method,
        "url": subject.url,
        "headers": None if headers is None else list(headers.items()),
    }


def value_record(value):
    value_type = type(value)
    return {
        "type": [value_type.__module__, value_type.__qualname__],
        "repr": repr(value),
    }


def exception_record(error):
    error_type = type(error)
    return {
        "type": [error_type.__module__, error_type.__qualname__],
        "mro": [
            [base.__module__, base.__qualname__]
            for base in error_type.__mro__
        ],
        "args": error.args,
        "public_state": {
            name: value
            for name, value in vars(error).items()
            if not name.startswith("_")
        },
    }


def is_missing_trial_symbol(error):
    return (
        _requests_rust is not None
        and isinstance(error, AttributeError)
        and getattr(error, "obj", None) is _requests_rust
        and getattr(error, "name", None) in _TRIAL_SYMBOLS
    )


def capture(label, subject, operation):
    effect_start = len(side_effects)
    try:
        returned = operation()
    except BaseException as error:
        if is_missing_trial_symbol(error):
            raise
        outcome = {
            "label": label,
            "returned": None,
            "exception": exception_record(error),
        }
    else:
        outcome = {
            "label": label,
            "returned": value_record(returned),
            "exception": None,
        }
    outcome["state"] = prepared_fields_snapshot(subject)
    outcome["side_effects"] = side_effects[effect_start:]
    return outcome
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == ""


def test_prepare_method_normalization_errors_and_dynamic_upper() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class DynamicMethod(str):
    def upper(self):
        side_effects.append(["method-upper", str.__str__(self)])
        return b"TRACE"


class BrokenMethod:
    def upper(self):
        side_effects.append("broken-method-upper")
        raise RuntimeError("method upper failed")

    def __repr__(self):
        return "<broken-method>"


cases = [
    ("none", None),
    ("mixed-case", "pAtCh"),
    ("bytes", b"delete"),
    ("lone-surrogate", "\\ud800x"),
    ("dynamic-upper", DynamicMethod("ignored")),
    ("upper-error", BrokenMethod()),
]
result = []
for label, method in cases:
    subject = PreparedRequest()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, method=method: prepare_method_call(
                subject, method
            ),
        )
    )
"""
    )


def test_prepare_method_shadowed_descriptor_is_resolved_once() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class DynamicPrepareMethod:
    def __get__(self, subject, owner):
        side_effects.append("prepare-method-get")

        def invoke(method):
            side_effects.append(["prepare-method-call", method])
            subject.method = "SHADOWED"

        return invoke


original = PreparedRequest.prepare_method
PreparedRequest.prepare_method = DynamicPrepareMethod()
try:
    subject = PreparedRequest()
    result = capture(
        "shadowed-prepare-method",
        subject,
        lambda: prepare_method_call(subject, "get"),
    )
finally:
    PreparedRequest.prepare_method = original
"""
    )


def test_rebound_prepared_request_getattribute_falls_back_once() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


original_getattribute = PreparedRequest.__getattribute__


def observed_getattribute(subject, name):
    if name in {"prepare_method", "prepare_headers", "headers"}:
        side_effects.append(["prepared-request-getattribute", name])
    return original_getattribute(subject, name)


PreparedRequest.__getattribute__ = observed_getattribute
try:
    method_subject = PreparedRequest()
    method = capture(
        "rebound-getattribute-method",
        method_subject,
        lambda: prepare_method_call(method_subject, "get"),
    )

    header_subject = PreparedRequest()
    headers = capture(
        "rebound-getattribute-headers",
        header_subject,
        lambda: prepare_headers_call(header_subject, {"Name": "value"}),
    )
finally:
    PreparedRequest.__getattribute__ = original_getattribute

result = [method, headers]
"""
    )


def test_preparation_rebound_module_dependencies_fall_back_once() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


original_to_native_string = models.to_native_string
models.to_native_string = lambda value: (
    side_effects.append(["to-native-string", value]) or "GLOBAL-METHOD"
)
try:
    method_subject = PreparedRequest()
    method = capture(
        "rebound-method-helper",
        method_subject,
        lambda: prepare_method_call(method_subject, "get"),
    )
finally:
    models.to_native_string = original_to_native_string


original_requote_uri = models.requote_uri
models.requote_uri = lambda value: (
    side_effects.append(["requote-uri", value])
    or "http://global.example/result"
)
try:
    url_subject = PreparedRequest()
    url = capture(
        "rebound-url-helper",
        url_subject,
        lambda: prepare_url_call(
            url_subject, "http://example.com/path", None
        ),
    )
finally:
    models.requote_uri = original_requote_uri


shadowed_params_subject = PreparedRequest()
shadowed_params_subject._encode_params = lambda value: (
    side_effects.append(["shadowed-encode-params", value])
    or "shadowed=yes"
)
shadowed_params = capture(
    "shadowed-encode-params",
    shadowed_params_subject,
    lambda: prepare_url_call(
        shadowed_params_subject,
        "http://example.com/path",
        {"ignored": "value"},
    ),
)


original_check_header_validity = models.check_header_validity
models.check_header_validity = lambda header: side_effects.append(
    ["check-header", header]
)
try:
    header_subject = PreparedRequest()
    headers = capture(
        "rebound-header-helper",
        header_subject,
        lambda: prepare_headers_call(
            header_subject, {"Name": " leading"}
        ),
    )
finally:
    models.check_header_validity = original_check_header_validity

result = [method, url, shadowed_params, headers]
"""
    )


def test_prepare_url_does_not_resolve_unused_encode_params_descriptor() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class ExplodingEncodeParams:
    def __get__(self, subject, owner):
        side_effects.append(
            [
                "encode-params-get",
                None if subject is None else type(subject).__name__,
            ]
        )
        raise RuntimeError("unused _encode_params was resolved")


PreparedRequest._encode_params = ExplodingEncodeParams()
try:
    non_http_subject = PreparedRequest()
    non_http = capture(
        "non-http-does-not-resolve-encode-params",
        non_http_subject,
        lambda: prepare_url_call(
            non_http_subject, "mailto:user@example.org", {"unused": "value"}
        ),
    )

    no_params_subject = PreparedRequest()
    no_params = capture(
        "http-none-does-not-resolve-encode-params",
        no_params_subject,
        lambda: prepare_url_call(
            no_params_subject, "http://example.com/path", None
        ),
    )
finally:
    del PreparedRequest._encode_params

result = [non_http, no_params]
"""
    )


def test_preparation_preserves_destructor_stage_order() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


original_to_native_string = models.to_native_string


def rebound_to_native_string(value):
    side_effects.append(["destructor-to-native-string", value])
    return "DESTRUCTOR-METHOD"


class OldMethod:
    def __del__(self):
        side_effects.append("old-method-del")
        models.to_native_string = rebound_to_native_string


method_subject = PreparedRequest()
method_subject.method = OldMethod()
try:
    method = capture(
        "method-destructor-before-conversion",
        method_subject,
        lambda: prepare_method_call(method_subject, "get"),
    )
finally:
    models.to_native_string = original_to_native_string


input_headers = {"Original": "one"}


class OldHeaders:
    def __del__(self):
        side_effects.append("old-headers-del")
        input_headers["Added-By-Destructor"] = "two"


headers_subject = PreparedRequest()
headers_subject.headers = OldHeaders()
headers = capture(
    "headers-destructor-before-iteration",
    headers_subject,
    lambda: prepare_headers_call(headers_subject, input_headers),
)
result = [method, headers]
"""
    )


def test_prepare_headers_rebound_internal_validators_fall_back_once() -> None:
    _assert_matches_oracle(
        """
import requests.utils as utils
from requests.models import PreparedRequest


original_validate_header_part = utils._validate_header_part
utils._validate_header_part = lambda header, part, index: side_effects.append(
    ["validate-header-part", part, index]
)
try:
    helper_subject = PreparedRequest()
    helper = capture(
        "rebound-validate-header-part",
        helper_subject,
        lambda: prepare_headers_call(
            helper_subject, {"Name": " leading-is-accepted"}
        ),
    )
finally:
    utils._validate_header_part = original_validate_header_part


class AcceptAll:
    def __init__(self, label):
        self.label = label

    def match(self, value):
        side_effects.append(["validator-match", self.label, value])
        return True


original_validators = utils._HEADER_VALIDATORS_STR
utils._HEADER_VALIDATORS_STR = (
    AcceptAll("name"),
    AcceptAll("value"),
)
try:
    validators_subject = PreparedRequest()
    validators = capture(
        "rebound-validator-tuple",
        validators_subject,
        lambda: prepare_headers_call(
            validators_subject, {" Name": " leading-is-accepted"}
        ),
    )
finally:
    utils._HEADER_VALIDATORS_STR = original_validators

result = [helper, validators]
"""
    )


def test_prepare_url_canonicalization_params_and_percent_escapes() -> None:
    _assert_matches_oracle(
        """
from collections.abc import Mapping
from requests.models import PreparedRequest


class ObservedParams(Mapping):
    def __iter__(self):
        side_effects.append("params-iter")
        return iter(("repeat", b"raw", "skip"))

    def __len__(self):
        side_effects.append("params-len")
        return 3

    def __getitem__(self, key):
        side_effects.append(["params-getitem", key])
        return {
            "repeat": ["one", "two"],
            b"raw": b"\\xff",
            "skip": None,
        }[key]


cases = [
    (
        "lstrip-mixed-case-bytes",
        b" \\tHtTp://Example.COM/a path?escaped=%7e&reserved=%2f",
        None,
    ),
    (
        "params-before-fragment-repeated-bytes",
        "http://example.com/path?first=1#frag",
        ObservedParams(),
    ),
    (
        "raw-bytes-params",
        "http://example.com/path#frag",
        b"raw=%2f&words=two words",
    ),
    (
        "invalid-percent-fallback",
        "http://example.com/%zz?q=%",
        None,
    ),
    (
        "encoded-dot-segment",
        "http://example.com/a/%2e%2e/b",
        None,
    ),
    ("explicit-default-port", "http://example.com:080/path", None),
    (
        "raw-backslash",
        "http://example.com/" + chr(92) + "path",
        None,
    ),
    ("leading-zero-ipv4", "http://127.000.000.001/path", None),
    ("empty-query", "http://example.com/path?", None),
    ("empty-fragment", "http://example.com/path#", None),
    ("empty-password", "http://user:@example.com/path", None),
    ("expanded-ipv6", "http://[0:0:0:0:0:0:0:1]/", None),
    ("embedded-tab", "http://example.com/a\\tb", None),
    ("incomplete-percent", "http://example.com/%0", None),
    ("trailing-spaces", "http://example.com/path  ", None),
    ("path-square-brackets", "http://example.com/a[b]/", None),
    ("query-apostrophe", "http://example.com/path?a'b", None),
    ("multiple-fragment-hashes", "http://example.com/path#one#two", None),
    ("unsupported-http-prefix-scheme", "httpx://EXAMPLE.com", None),
    ("bare-http-unix-authority", "http+unix://%2Fsocket", None),
    (
        "http-unix-authority-null",
        "http+unix://socket" + chr(0) + "name/path",
        None,
    ),
    ("http-unix-authority-pipe", "http+unix://socket|name/path", None),
    ("empty-authority-three-slashes", "http:///path", None),
    ("empty-authority-four-slashes", "http:////example.com", None),
    ("single-slash-after-scheme", "http:/example.com", None),
    ("no-slashes-after-scheme", "http:example.com", None),
    ("authority-space", "http://example .com/path", None),
    ("authority-percent-space", "http://example%20.com/path", None),
    ("authority-punctuation", "http://exam|ple.com/path", None),
    ("all-whitespace", "   ", None),
]
result = []
for label, url, params in cases:
    subject = PreparedRequest()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, url=url, params=params: prepare_url_call(
                subject, url, params
            ),
        )
    )
"""
    )


def test_prepare_url_non_http_bypass_and_dynamic_inputs() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class ForbiddenParams:
    def __iter__(self):
        side_effects.append("forbidden-params-iter")
        raise AssertionError("non-HTTP parameters were inspected")


class DynamicURL:
    def __str__(self):
        side_effects.append("dynamic-url-str")
        return " \\tmailto:user@example.org"


class BrokenURL:
    def __str__(self):
        side_effects.append("broken-url-str")
        raise RuntimeError("url stringification failed")

    def __repr__(self):
        return "<broken-url>"


cases = [
    (
        "bytes-mailto-bypasses-params",
        b" \\tmailto:user@example.org",
        ForbiddenParams(),
    ),
    ("dynamic-mailto", DynamicURL(), ForbiddenParams()),
    (
        "http-prefix-nonstandard-is-prepared",
        "http+unix://%2Fvar%2Frun%2Fsocket/path%7E",
        {"key": "value"},
    ),
    ("dynamic-string-error", BrokenURL(), None),
]
result = []
for label, url, params in cases:
    subject = PreparedRequest()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, url=url, params=params: prepare_url_call(
                subject, url, params
            ),
        )
    )
"""
    )


def test_prepare_url_non_http_preserves_exact_string_identity_and_lstrip() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


cases = [
    ("unchanged-mailto", "mailto:user@example.org"),
    ("unchanged-data", "data:text/plain,payload"),
    ("unchanged-one-letter-scheme", "x:value"),
    ("leading-space", " mailto:user@example.org"),
    ("leading-file-separator", "\\u001cmailto:user@example.org"),
    ("leading-unit-separator", "\\u001fdata:text/plain,payload"),
]
result = []
for label, url in cases:
    subject = PreparedRequest()
    prepared = capture(
        label,
        subject,
        lambda subject=subject, url=url: prepare_url_call(subject, url, None),
    )
    prepared["url_identity"] = subject.url is url
    result.append(prepared)
"""
    )


def test_prepare_url_idna_invalid_labels_and_url_errors() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class CustomIDNARequest(PreparedRequest):
    @staticmethod
    def _get_idna_encoded_host(host):
        side_effects.append(["custom-idna", host])
        return "custom.example"


cases = [
    (
        "unicode-host-and-path",
        PreparedRequest,
        "http://stra\\u00dfe.de/stra\\u00dfe",
    ),
    (
        "subclass-idna-fallback",
        CustomIDNARequest,
        "http://t\\u00e4st.example/path",
    ),
    ("unicode-invalid-label", PreparedRequest, "http://\\u2603.net/"),
    ("wildcard-invalid-label", PreparedRequest, "http://*.example.com/"),
    ("leading-dot-invalid-label", PreparedRequest, "http://.example.com/"),
    ("missing-schema", PreparedRequest, "example.com/path"),
    ("missing-host", PreparedRequest, "http://"),
    ("parser-error", PreparedRequest, "http://[::1"),
    ("invalid-utf8-bytes", PreparedRequest, b"http://\\xff"),
]
result = []
for label, subject_type, url in cases:
    subject = subject_type()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, url=url: prepare_url_call(
                subject, url, None
            ),
        )
    )
"""
    )


def test_prepare_headers_order_casing_bytes_and_subclasses() -> None:
    _assert_matches_oracle(
        """
from collections import OrderedDict
from requests.models import PreparedRequest


class HeaderText(str):
    pass


class HeaderBytes(bytes):
    pass


headers = OrderedDict(
    [
        ("First", "one"),
        ("second", "two"),
        ("FIRST", "three"),
        (b"Byte-Key", b"byte-value"),
        (HeaderText("Subclass-Key"), HeaderBytes(b"subclass-value")),
        ("Empty", ""),
    ]
)
subject = PreparedRequest()
prepared = capture(
    "ordered-valid-headers",
    subject,
    lambda: prepare_headers_call(subject, headers),
)
prepared["header_types"] = [
    [
        [type(name).__module__, type(name).__qualname__],
        [type(value).__module__, type(value).__qualname__],
    ]
    for name, value in subject.headers.items()
]

subject.headers = {"Stale": "value"}
reset = capture(
    "none-resets-to-empty",
    subject,
    lambda: prepare_headers_call(subject, None),
)

exact_subject = PreparedRequest()
exact_headers = OrderedDict(
    [
        ("First", "one"),
        ("second", b"two"),
        ("FIRST", "three"),
    ]
)
exact = capture(
    "exact-builtins-native-candidate",
    exact_subject,
    lambda: prepare_headers_call(exact_subject, exact_headers),
)
result = [prepared, reset, exact]
"""
    )


def test_prepare_headers_invalid_and_dynamic_mapping_timing() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class DynamicHeaders:
    def __bool__(self):
        side_effects.append("headers-bool")
        return True

    def items(self):
        side_effects.append("headers-items")

        def rows():
            values = [
                ("Good", "one"),
                ("Bad", " leading-space"),
                ("Never", "reached"),
            ]
            for name, value in values:
                side_effects.append(["headers-yield", name])
                yield name, value

        return rows()


cases = [
    ("invalid-name-type", {7: "value"}),
    ("invalid-value-type", {"Name": ["value"]}),
    ("newline-name", {"Bad\\nName": "value"}),
    ("return-value", {"Name": "bad\\rvalue"}),
    ("leading-space-name", {" Name": "value"}),
    ("leading-tab-value-bytes", {b"Name": b"\\tvalue"}),
    ("leading-vertical-tab-name-bytes", {b"\\x0bName": b"value"}),
    ("leading-vertical-tab-value-bytes", {b"Name": b"\\x0bvalue"}),
    ("text-file-separator-name", {"\\u001cName": "value"}),
    ("text-file-separator-value", {"Name": "\\u001cvalue"}),
    ("dynamic-partial-mapping", DynamicHeaders()),
]
result = []
for label, headers in cases:
    subject = PreparedRequest()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, headers=headers: prepare_headers_call(
                subject, headers
            ),
        )
    )
"""
    )


def test_prepare_headers_ordered_dict_custom_key_has_no_probe_lookup() -> None:
    _assert_matches_oracle(
        """
from collections import OrderedDict
from requests.models import PreparedRequest


class ObservedKey:
    def __hash__(self):
        side_effects.append("custom-key-hash")
        return 17

    def __eq__(self, other):
        side_effects.append(["custom-key-eq", type(other).__name__])
        return self is other

    def __repr__(self):
        return "<observed-key>"


headers = OrderedDict([(ObservedKey(), "value")])
subject = PreparedRequest()
result = capture(
    "ordered-dict-custom-key",
    subject,
    lambda: prepare_headers_call(subject, headers),
)
"""
    )


def test_prepared_fields_resnapshot_later_public_mutation() -> None:
    _assert_matches_oracle(
        """
from collections.abc import Mapping
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


class Marker:
    def __init__(self, label):
        self.label = label

    def __repr__(self):
        return f"<marker {self.label}>"


class MutatedHeaders(Mapping):
    def __init__(self):
        self.rows = [
            ("First", Marker("first-value")),
            ("Second", Marker("second-value")),
        ]

    def __iter__(self):
        side_effects.append("mutated-headers-iter")
        return iter(name for name, _ in self.rows)

    def __len__(self):
        side_effects.append("mutated-headers-len")
        return len(self.rows)

    def __getitem__(self, key):
        side_effects.append(["mutated-headers-getitem", key])
        return dict(self.rows)[key]


subject = PreparedRequest()
subject.method = "GET"
subject.url = "http://example.com/original"
subject.headers = CaseInsensitiveDict({"Original": "value"})
initial = prepared_fields_snapshot(subject)

method_marker = Marker("method")
url_marker = Marker("url")
headers = MutatedHeaders()
subject.method = method_marker
subject.url = url_marker
subject.headers = headers
mutated = prepared_fields_snapshot(subject)

headers.rows[:] = [
    ("Second", Marker("replacement")),
    ("Third", Marker("third-value")),
]
resnapshot = prepared_fields_snapshot(subject)


class ObservedPreparedRequest(PreparedRequest):
    def __getattribute__(self, name):
        if name in {"headers", "method", "url"}:
            side_effects.append(["snapshot-getattr", name])
        return super().__getattribute__(name)


observed = ObservedPreparedRequest()
observed.method = "GET"
observed.url = "http://example.com/"
observed.headers = CaseInsensitiveDict({"Name": "value"})
observed_snapshot = prepared_fields_snapshot(observed)
result = {
    "initial": initial,
    "mutated": mutated,
    "resnapshot": resnapshot,
    "observed_snapshot": observed_snapshot,
    "method_identity": mutated["method"] is method_marker,
    "url_identity": mutated["url"] is url_marker,
    "headers_replaced": mutated["headers"] != resnapshot["headers"],
}
"""
    )

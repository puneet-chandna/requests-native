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

_PREIMPORT_CAPTURE_HELPER = """
def capture_preimport(subject, operation):
    effect_start = len(side_effects)
    try:
        returned = operation()
    except BaseException as error:
        outcome = [
            type(error).__module__,
            type(error).__qualname__,
            error.args,
        ]
    else:
        outcome = ["returned", returned]
    return {
        "outcome": outcome,
        "method": subject.method,
        "url": subject.url,
        "side_effects": side_effects[effect_start:],
    }
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert oracle.stderr == ""
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == ""


def _assert_matches_oracle_before_extension_import(source: str) -> None:
    case = {"source": dedent(source)}
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


def test_prepare_method_one_character_identity_matches_frozen_python() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


text_method = "G"
text_subject = PreparedRequest()
text = capture(
    "uppercase-one-character-text",
    text_subject,
    lambda: prepare_method_call(text_subject, text_method),
)
text["is_input"] = text_subject.method is text_method


bytes_method = b"G"
canonical_text = "G"
bytes_subject = PreparedRequest()
bytes_result = capture(
    "uppercase-one-character-bytes",
    bytes_subject,
    lambda: prepare_method_call(bytes_subject, bytes_method),
)
bytes_result["is_input"] = bytes_subject.method is bytes_method
bytes_result["is_canonical_text"] = bytes_subject.method is canonical_text

result = [text, bytes_result]
"""
    )


def test_prepare_method_rebound_internal_isinstance_preserves_partial_state() -> None:
    _assert_matches_oracle(
        """
import requests._internal_utils as internal_utils
from requests.models import PreparedRequest


def rebound_isinstance(value, expected):
    side_effects.append(["internal-isinstance", repr(value)])
    return False


internal_utils.isinstance = rebound_isinstance
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-internal-isinstance",
        subject,
        lambda: prepare_method_call(subject, "get"),
    )
finally:
    del internal_utils.isinstance
"""
    )


def test_prepare_method_same_function_replaced_code_delegates() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


original_code = PreparedRequest.prepare_method.__code__


def code_patched(subject, method):
    subject.method = "CODE-PATCHED"


PreparedRequest.prepare_method.__code__ = code_patched.__code__
try:
    subject = PreparedRequest()
    result = capture(
        "same-function-code-replaced",
        subject,
        lambda: prepare_method_call(subject, "get"),
    )
finally:
    PreparedRequest.prepare_method.__code__ = original_code
"""
    )


def test_prepare_method_same_helper_replaced_code_delegates() -> None:
    _assert_matches_oracle(
        """
import requests._internal_utils as internal_utils
from requests.models import PreparedRequest


original_code = internal_utils.to_native_string.__code__


def code_patched(string, encoding="ascii"):
    return "HELPER-CODE-PATCHED"


internal_utils.to_native_string.__code__ = code_patched.__code__
try:
    subject = PreparedRequest()
    result = capture(
        "same-helper-code-replaced",
        subject,
        lambda: prepare_method_call(subject, "get"),
    )
finally:
    internal_utils.to_native_string.__code__ = original_code
"""
    )


def test_prepare_method_helper_default_replacement_delegates() -> None:
    _assert_matches_oracle(
        """
import requests._internal_utils as internal_utils
from requests.models import PreparedRequest


original_defaults = internal_utils.to_native_string.__defaults__
internal_utils.to_native_string.__defaults__ = ("utf-16",)
try:
    subject = PreparedRequest()
    result = capture(
        "helper-default-replaced",
        subject,
        lambda: prepare_method_call(subject, b"get"),
    )
finally:
    internal_utils.to_native_string.__defaults__ = original_defaults
"""
    )


def test_prepare_method_replaced_before_first_extension_import_delegates() -> None:
    _assert_matches_oracle_before_extension_import(
        """
import requests.models as models
from requests.models import PreparedRequest


original = PreparedRequest.prepare_method


def replacement(subject, method):
    side_effects.append(["replacement-called", method])
    subject.method = "CUSTOM"


PreparedRequest.prepare_method = replacement
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    if _requests_rust is None:
        returned = subject.prepare_method("get")
    else:
        returned = _requests_rust._prepare_method_trial(subject, "get")
    result = {
        "returned": returned,
        "method": subject.method,
        "side_effects": list(side_effects),
    }
finally:
    PreparedRequest.prepare_method = original
"""
    )


def test_prepare_method_builtin_isinstance_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import builtins
from requests.models import PreparedRequest


original_isinstance = builtins.isinstance
original_str = builtins.str


class ReplacedIsinstance:
    def __call__(self, value, expected):
        if original_isinstance(value, original_str) and value == "GET":
            side_effects.append(["isinstance", value])
            return False
        return original_isinstance(value, expected)


builtins.isinstance = ReplacedIsinstance()
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_method("get")
            if _requests_rust is None
            else _requests_rust._prepare_method_trial(subject, "get")
        ),
    )
finally:
    builtins.isinstance = original_isinstance
"""
    )


def test_pristine_exact_candidates_execute_native_trials() -> None:
    case = {
        "source": dedent(
            _TRIAL_HELPERS
            + """
import sys
from requests.models import PreparedRequest, RequestEncodingMixin
from requests.utils import to_key_val_list


targets = {
    PreparedRequest.prepare_method.__code__,
    PreparedRequest.prepare_url.__code__,
    PreparedRequest.prepare_headers.__code__,
    RequestEncodingMixin._encode_params.__code__,
    to_key_val_list.__code__,
}
seen = []


def profile(frame, event, argument):
    if event == "call" and frame.f_code in targets:
        seen.append(frame.f_code.co_qualname)


sys.setprofile(profile)
try:
    method_subject = PreparedRequest()
    prepare_method_call(method_subject, "get")

    url_subject = PreparedRequest()
    prepare_url_call(
        url_subject,
        "http://example.com/a path",
        {"x": "a b"},
    )

    headers_subject = PreparedRequest()
    prepare_headers_call(headers_subject, {"Name": "value"})
finally:
    sys.setprofile(None)

assert seen == []
assert method_subject.method == "GET"
assert url_subject.url == "http://example.com/a%20path?x=a+b"
assert list(headers_subject.headers.items()) == [("Name", "value")]
result = seen
"""
        )
    }
    rewrite = run_rewrite_case(case)

    assert rewrite.observations["exception"] is None
    assert rewrite.stderr == ""


def test_pristine_trials_do_not_emit_sensitive_function_attribute_events() -> None:
    _assert_matches_oracle(
        """
import sys
from requests.models import PreparedRequest


sensitive_names = {
    "__builtins__",
    "__code__",
    "__defaults__",
    "__globals__",
    "__kwdefaults__",
}


def observe(event, arguments):
    if (
        event == "object.__getattr__"
        and len(arguments) > 1
        and arguments[1] in sensitive_names
    ):
        side_effects.append(["sensitive-function-read", arguments[1]])


sys.addaudithook(observe)

method_subject = PreparedRequest()
method = capture(
    "method",
    method_subject,
    lambda: prepare_method_call(method_subject, "get"),
)

url_subject = PreparedRequest()
url = capture(
    "url",
    url_subject,
    lambda: prepare_url_call(
        url_subject,
        "http://example.com/a path",
        {"x": "a b"},
    ),
)

headers_subject = PreparedRequest()
headers = capture(
    "headers",
    headers_subject,
    lambda: prepare_headers_call(headers_subject, {"Name": "value"}),
)

result = [method, url, headers]
"""
    )


def test_sensitive_function_attribute_event_error_preserves_frozen_behavior() -> None:
    _assert_matches_oracle(
        """
import sys
from requests.models import PreparedRequest


sensitive_names = {
    "__builtins__",
    "__code__",
    "__defaults__",
    "__globals__",
    "__kwdefaults__",
}


def block(event, arguments):
    if (
        event == "object.__getattr__"
        and len(arguments) > 1
        and arguments[1] in sensitive_names
    ):
        raise RuntimeError("blocked sensitive function read")


sys.addaudithook(block)

method_subject = PreparedRequest()
method = capture(
    "method",
    method_subject,
    lambda: prepare_method_call(method_subject, "get"),
)

url_subject = PreparedRequest()
url = capture(
    "url",
    url_subject,
    lambda: prepare_url_call(
        url_subject,
        "http://example.com/a path",
        {"x": "a b"},
    ),
)

headers_subject = PreparedRequest()
headers = capture(
    "headers",
    headers_subject,
    lambda: prepare_headers_call(headers_subject, {"Name": "value"}),
)

result = [method, url, headers]
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


def test_rebound_special_method_descriptors_have_no_class_probe_callbacks() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


class ObservedSpecialMethod:
    def __init__(self, label, function):
        self.label = label
        self.function = function

    def __get__(self, subject, owner):
        side_effects.append([self.label, subject is None])
        if subject is None:
            return self.function
        return self.function.__get__(subject, owner)


PreparedRequest.__getattribute__ = ObservedSpecialMethod(
    "getattribute-bind", object.__getattribute__
)
try:
    get_subject = PreparedRequest()
    getattribute = capture(
        "rebound-getattribute-descriptor",
        get_subject,
        lambda: prepare_method_call(get_subject, "get"),
    )
finally:
    del PreparedRequest.__getattribute__


PreparedRequest.__setattr__ = ObservedSpecialMethod(
    "setattr-bind", object.__setattr__
)
try:
    set_subject = PreparedRequest()
    setattr = capture(
        "rebound-setattr-descriptor",
        set_subject,
        lambda: prepare_method_call(set_subject, "post"),
    )
finally:
    del PreparedRequest.__setattr__

result = [getattribute, setattr]
"""
    )


def test_bound_method_trust_rejects_decoy_and_forged_instance_shadows() -> None:
    _assert_matches_oracle(
        """
import types
from requests.models import PreparedRequest


target = PreparedRequest()
decoy = PreparedRequest()
target.prepare_method = types.MethodType(
    PreparedRequest.prepare_method,
    decoy,
)
decoy_bound = capture(
    "instance-shadow-bound-to-decoy",
    target,
    lambda: prepare_method_call(target, "get"),
)
decoy_bound["decoy_method"] = decoy.method


class ForgedCallable:
    __func__ = PreparedRequest.prepare_method

    def __call__(self, method):
        side_effects.append(["forged-call", method])
        forged_target.method = "FORGED"


forged_target = PreparedRequest()
forged_target.prepare_method = ForgedCallable()
forged = capture(
    "instance-shadow-forged-func",
    forged_target,
    lambda: prepare_method_call(forged_target, "post"),
)
result = [decoy_bound, forged]
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


def test_raw_module_dependency_checks_preserve_frozen_lookup_order() -> None:
    _assert_matches_oracle(
        """
import types
import requests.models as models
from requests.models import PreparedRequest


class ObservedModelsModule(types.ModuleType):
    def __getattribute__(self, name):
        if name in {
            "to_native_string",
            "parse_url",
            "InvalidURL",
            "MissingSchema",
        }:
            side_effects.append(["models-module-getattribute", name])
        return super().__getattribute__(name)


original_module_type = models.__class__
models.__class__ = ObservedModelsModule
try:
    module_subject = PreparedRequest()
    module_class = capture(
        "custom-models-module-class",
        module_subject,
        lambda: prepare_method_call(module_subject, "get"),
    )
finally:
    models.__class__ = original_module_type


original_to_native_string = models.to_native_string
del models.to_native_string
try:
    missing_helper_subject = PreparedRequest()
    missing_helper = capture(
        "deleted-used-helper",
        missing_helper_subject,
        lambda: prepare_method_call(missing_helper_subject, "post"),
    )
finally:
    models.to_native_string = original_to_native_string


original_invalid_url = models.InvalidURL
del models.InvalidURL
try:
    unused_exception_subject = PreparedRequest()
    unused_exception = capture(
        "deleted-unused-exception",
        unused_exception_subject,
        lambda: prepare_url_call(
            unused_exception_subject,
            "http://example.com/path",
            None,
        ),
    )
finally:
    models.InvalidURL = original_invalid_url

result = [module_class, missing_helper, unused_exception]
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


def test_prepare_url_rebound_has_read_delegates_to_authoritative_encoder() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


original_has_read = models._t.has_read
models._t.has_read = lambda value: True
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-has-read",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    models._t.has_read = original_has_read
"""
    )


def test_prepare_url_builtin_conversion_shadows_delegate_before_bypass() -> None:
    _assert_matches_oracle(
        """
import builtins
import requests.models as models
from requests.models import PreparedRequest


def observed_isinstance(value, expected):
    side_effects.append(["models-isinstance", repr(value)])
    return builtins.isinstance(value, expected)


models.isinstance = observed_isinstance
try:
    isinstance_subject = PreparedRequest()
    rebound_isinstance = capture(
        "rebound-models-isinstance",
        isinstance_subject,
        lambda: prepare_url_call(
            isinstance_subject,
            "http://example.com/path",
            None,
        ),
    )
finally:
    del models.isinstance


def rewritten_str(value):
    side_effects.append(["models-str", repr(value)])
    if str(value).startswith("mailto:"):
        return "mailto:rewritten@example.org"
    return str(value)


models.str = rewritten_str
try:
    str_http_subject = PreparedRequest()
    str_http = capture(
        "rebound-models-str-http",
        str_http_subject,
        lambda: prepare_url_call(
            str_http_subject,
            "http://example.com/path",
            None,
        ),
    )
    str_non_http_subject = PreparedRequest()
    str_non_http = capture(
        "rebound-models-str-non-http",
        str_non_http_subject,
        lambda: prepare_url_call(
            str_non_http_subject,
            "mailto:original@example.org",
            None,
        ),
    )
finally:
    del models.str


models.bytes = lambda value: value
try:
    bytes_subject = PreparedRequest()
    rebound_bytes = capture(
        "rebound-models-bytes",
        bytes_subject,
        lambda: prepare_url_call(
            bytes_subject,
            "http://example.com/path",
            None,
        ),
    )
finally:
    del models.bytes

result = [rebound_isinstance, str_http, str_non_http, rebound_bytes]
"""
    )


def test_prepare_url_builtin_str_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import builtins
from requests.models import PreparedRequest


original_isinstance = builtins.isinstance
original_str = builtins.str
target = "http://example.com/path"


class ReplacedStrMeta(type):
    def __instancecheck__(cls, value):
        return original_isinstance(value, original_str)


class ReplacedStr(metaclass=ReplacedStrMeta):
    def __new__(cls, value="", *args, **kwargs):
        if original_isinstance(value, original_str) and value == target:
            side_effects.append(["str", value])
            return "http://rewritten.example/path"
        return original_str(value, *args, **kwargs)


builtins.str = ReplacedStr
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url(target, None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(subject, target, None)
        ),
    )
finally:
    builtins.str = original_str
"""
    )


def test_prepare_url_builtin_bytes_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import builtins
from requests.models import PreparedRequest


original_bytes = builtins.bytes
original_isinstance = builtins.isinstance
target = b"http://example.com/path"


class ReplacedBytesMeta(type):
    def __instancecheck__(cls, value):
        if original_isinstance(value, original_bytes) and value == target:
            side_effects.append(["bytes-instancecheck", value])
            return False
        return original_isinstance(value, original_bytes)


class ReplacedBytes(metaclass=ReplacedBytesMeta):
    def __new__(cls, *args, **kwargs):
        return original_bytes(*args, **kwargs)


builtins.bytes = ReplacedBytes
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url(target, None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(subject, target, None)
        ),
    )
finally:
    builtins.bytes = original_bytes
"""
    )


def test_prepare_url_basestring_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import requests.models as models
from requests.models import PreparedRequest


original_basestring = models.basestring
models.basestring = ()
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url(
                "http://example.com/path",
                {"x": "ab"},
            )
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://example.com/path",
                {"x": "ab"},
            )
        ),
    )
finally:
    models.basestring = original_basestring
"""
    )


def test_prepare_url_parameter_builtin_and_transitive_shadows_delegate() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
import requests.utils as utils
from requests.models import PreparedRequest


models.hasattr = lambda value, name: False
try:
    hasattr_subject = PreparedRequest()
    rebound_hasattr = capture(
        "rebound-models-hasattr",
        hasattr_subject,
        lambda: prepare_url_call(
            hasattr_subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    del models.hasattr


utils.isinstance = lambda value, expected: False
try:
    isinstance_subject = PreparedRequest()
    transitive_isinstance = capture(
        "rebound-utils-isinstance-params",
        isinstance_subject,
        lambda: prepare_url_call(
            isinstance_subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    del utils.isinstance


original_supports_items = utils._SupportsItems
utils._SupportsItems = str
try:
    supports_items_subject = PreparedRequest()
    rebound_supports_items = capture(
        "rebound-supports-items",
        supports_items_subject,
        lambda: prepare_url_call(
            supports_items_subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    utils._SupportsItems = original_supports_items


utils.list = lambda value: [("rewritten", "value")]
try:
    list_subject = PreparedRequest()
    rebound_list = capture(
        "rebound-utils-list",
        list_subject,
        lambda: prepare_url_call(
            list_subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    del utils.list

result = [
    rebound_hasattr,
    transitive_isinstance,
    rebound_supports_items,
    rebound_list,
]
"""
    )


def test_prepare_url_parse_url_transitive_host_helper_delegates() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


globals_dict = models.parse_url.__globals__
original = globals_dict["_normalize_host"]


def rewritten_host(host, scheme):
    side_effects.append(["normalize-host", host, scheme])
    return "rewritten.example"


globals_dict["_normalize_host"] = rewritten_host
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-normalize-host",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/path",
            None,
        ),
    )
finally:
    globals_dict["_normalize_host"] = original
"""
    )


def test_prepare_url_nested_nonfunction_global_mutation_delegates() -> None:
    _assert_matches_oracle(
        """
import re
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


primer = PreparedRequest()
prepare_method_call(primer, "get")
original = url_utils._IPV4_RE
url_utils._IPV4_RE = re.compile(".*")
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-ipv4-pattern",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://EXAMPLE.com/path",
            None,
        ),
    )
finally:
    url_utils._IPV4_RE = original
"""
    )


def test_prepare_url_nested_nonfunction_global_replaced_before_extension_import() -> (
    None
):
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import re
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


original = url_utils._IPV4_RE
url_utils._IPV4_RE = re.compile(".*")
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url(
                "http://EXAMPLE.com/path",
                None,
            )
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://EXAMPLE.com/path",
                None,
            )
        ),
    )
finally:
    url_utils._IPV4_RE = original
"""
    )


def test_prepare_url_scheme_tuple_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


original = url_utils._NORMALIZABLE_SCHEMES
url_utils._NORMALIZABLE_SCHEMES = ()
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url(
                "http://EXAMPLE.com/path",
                None,
            )
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://EXAMPLE.com/path",
                None,
            )
        ),
    )
finally:
    url_utils._NORMALIZABLE_SCHEMES = original
"""
    )


def test_prepare_url_internal_unicode_builtin_shadow_delegates() -> None:
    _assert_matches_oracle(
        """
import requests._internal_utils as internal_utils
from requests.models import PreparedRequest


internal_utils.isinstance = lambda value, expected: False
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-unicode-is-ascii-isinstance",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/path",
            None,
        ),
    )
finally:
    del internal_utils.isinstance
"""
    )


def test_prepare_url_requote_transitive_quote_delegates() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


globals_dict = models.requote_uri.__globals__
original = globals_dict["quote"]


def rewritten_quote(value, safe):
    side_effects.append(["quote", value, safe])
    return "mailto:quote-rewritten@example.org"


globals_dict["quote"] = rewritten_quote
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-quote",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/a path",
            None,
        ),
    )
finally:
    globals_dict["quote"] = original
"""
    )


def test_prepare_url_urlencode_transitive_isinstance_delegates() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest


globals_dict = models.urlencode.__globals__


def rebound_isinstance(value, expected):
    side_effects.append(["urlencode-isinstance", repr(value)])
    return False


globals_dict["isinstance"] = rebound_isinstance
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-urlencode-isinstance",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    del globals_dict["isinstance"]
"""
    )


def test_prepare_url_unlisted_direct_global_shadow_delegates() -> None:
    _assert_matches_oracle(
        """
import requests.utils as utils
from requests.models import PreparedRequest


utils.bool = dict
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-utils-bool",
        subject,
        lambda: prepare_url_call(
            subject,
            "http://example.com/path",
            {"x": "a b"},
        ),
    )
finally:
    del utils.bool
"""
    )


def test_prepare_url_direct_global_shadow_before_extension_import_delegates() -> None:
    _assert_matches_oracle_before_extension_import(
        """
import requests.utils as utils
from requests.models import PreparedRequest


utils.bool = dict
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    try:
        if _requests_rust is None:
            returned = subject.prepare_url(
                "http://example.com/path",
                {"x": "a b"},
            )
        else:
            returned = _requests_rust._prepare_url_trial(
                subject,
                "http://example.com/path",
                {"x": "a b"},
            )
    except BaseException as error:
        outcome = {
            "returned": None,
            "exception": {
                "type": [type(error).__module__, type(error).__qualname__],
                "args": error.args,
            },
        }
    else:
        outcome = {
            "returned": returned,
            "exception": None,
        }
    result = {
        "outcome": outcome,
        "url": subject.url,
    }
finally:
    del utils.bool
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


def test_prepare_headers_preflights_before_observable_construction() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
import requests.structures as structures
from requests.models import PreparedRequest


original_ordered_dict = structures.OrderedDict


class ObservedStore(original_ordered_dict):
    events = side_effects

    def __init__(self, label):
        self.label = label
        super().__init__()

    def __del__(self):
        self.events.append(["ordered-dict-del", self.label])


construction_count = 0


def observed_factory():
    global construction_count
    construction_count += 1
    label = f"unsupported-{construction_count}"
    side_effects.append(["ordered-dict-construct", label])
    return ObservedStore(label)


structures.OrderedDict = observed_factory
try:
    unsupported_subject = PreparedRequest()
    unsupported = capture(
        "unsupported-exact-dict",
        unsupported_subject,
        lambda: prepare_headers_call(unsupported_subject, {7: "value"}),
    )
finally:
    structures.OrderedDict = original_ordered_dict


original_check_header_validity = models.check_header_validity
dynamic_count = 0


def observed_check(header):
    side_effects.append(["dynamic-check", header])


def mutating_factory():
    global dynamic_count
    dynamic_count += 1
    side_effects.append(["mutating-ordered-dict", dynamic_count])
    models.check_header_validity = observed_check
    return original_ordered_dict()


structures.OrderedDict = mutating_factory
try:
    dynamic_subject = PreparedRequest()
    dynamic = capture(
        "dynamic-constructor-dependency",
        dynamic_subject,
        lambda: prepare_headers_call(
            dynamic_subject,
            {"Name": " leading-is-accepted"},
        ),
    )
finally:
    structures.OrderedDict = original_ordered_dict
    models.check_header_validity = original_check_header_validity

result = [unsupported, dynamic]
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


def test_prepare_headers_rebound_utils_isinstance_preserves_empty_mapping() -> None:
    _assert_matches_oracle(
        """
import requests.utils as utils
from requests.models import PreparedRequest


utils.isinstance = lambda value, expected: False
try:
    subject = PreparedRequest()
    result = capture(
        "rebound-utils-isinstance-headers",
        subject,
        lambda: prepare_headers_call(subject, {"Name": "value"}),
    )
finally:
    del utils.isinstance
"""
    )


def test_prepare_headers_dynamic_store_delegates_before_validation() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


original_check = models.check_header_validity


def rebound_check(header):
    side_effects.append(["rebound-check", header])


class DynamicStore:
    def __set__(self, subject, value):
        subject.__dict__["dynamic_store"] = value

    def __get__(self, subject, owner):
        if subject is None:
            return self
        side_effects.append("store-get")
        models.check_header_validity = rebound_check
        return subject.__dict__["dynamic_store"]


CaseInsensitiveDict._store = DynamicStore()
try:
    subject = PreparedRequest()
    result = capture(
        "dynamic-store",
        subject,
        lambda: prepare_headers_call(
            subject,
            {"Good": "one", "Bad": " leading"},
        ),
    )
finally:
    del CaseInsensitiveDict._store
    models.check_header_validity = original_check
"""
    )


def test_prepare_headers_in_place_init_code_mutation_preserves_stage_order() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
import requests.structures as structures
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


primer = PreparedRequest()
prepare_method_call(primer, "get")
original_code = CaseInsensitiveDict.__init__.__code__
original_check = models.check_header_validity


def accept(header):
    side_effects.append(["accepted", header])


def patched_init(self, data=None, **kwargs):
    _task7_effects.append("patched-init")
    self._store = OrderedDict()
    _task7_models.check_header_validity = _task7_accept


structures._task7_effects = side_effects
structures._task7_models = models
structures._task7_accept = accept
CaseInsensitiveDict.__init__.__code__ = patched_init.__code__
try:
    subject = PreparedRequest()
    result = capture(
        "in-place-init-code",
        subject,
        lambda: prepare_headers_call(subject, {"Bad": " leading"}),
    )
finally:
    CaseInsensitiveDict.__init__.__code__ = original_code
    models.check_header_validity = original_check
    del structures._task7_effects
    del structures._task7_models
    del structures._task7_accept
"""
    )


def test_prepare_headers_in_place_init_defaults_preserves_stage_order() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


primer = PreparedRequest()
prepare_method_call(primer, "get")
original_defaults = CaseInsensitiveDict.__init__.__defaults__
original_check = models.check_header_validity


def accept(header):
    side_effects.append(["accepted", header])


class MutatingDefault:
    def keys(self):
        side_effects.append("default-keys")
        models.check_header_validity = accept
        return ()


CaseInsensitiveDict.__init__.__defaults__ = (MutatingDefault(),)
try:
    subject = PreparedRequest()
    result = capture(
        "in-place-init-defaults",
        subject,
        lambda: prepare_headers_call(subject, {"Bad": " leading"}),
    )
finally:
    CaseInsensitiveDict.__init__.__defaults__ = original_defaults
    models.check_header_validity = original_check
"""
    )


def test_prepare_headers_in_place_setitem_code_mutation_preserves_stage_order() -> None:
    _assert_matches_oracle(
        """
import requests.models as models
import requests.structures as structures
from requests.models import PreparedRequest
from requests.structures import CaseInsensitiveDict


primer = PreparedRequest()
prepare_method_call(primer, "get")
original_code = CaseInsensitiveDict.__setitem__.__code__
original_check = models.check_header_validity


def accept(header):
    side_effects.append(["accepted", header])


def patched_setitem(self, key, value):
    _task7_effects.append(["patched-setitem", key, value])
    _task7_models.check_header_validity = _task7_accept
    self._store[key.lower()] = (key, value)


structures._task7_effects = side_effects
structures._task7_models = models
structures._task7_accept = accept
CaseInsensitiveDict.__setitem__.__code__ = patched_setitem.__code__
try:
    subject = PreparedRequest()
    result = capture(
        "in-place-setitem-code",
        subject,
        lambda: prepare_headers_call(
            subject,
            {"First": "one", "Bad": " leading"},
        ),
    )
finally:
    CaseInsensitiveDict.__setitem__.__code__ = original_code
    models.check_header_validity = original_check
    del structures._task7_effects
    del structures._task7_models
    del structures._task7_accept
"""
    )


def test_prepare_headers_validators_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import re
import requests.utils as utils
from requests.models import PreparedRequest


original = utils._HEADER_VALIDATORS_STR
utils._HEADER_VALIDATORS_STR = (
    re.compile(".*"),
    re.compile(".*"),
)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_headers({"Bad": " leading"})
            if _requests_rust is None
            else _requests_rust._prepare_headers_trial(
                subject,
                {"Bad": " leading"},
            )
        ),
    )
    result["headers"] = (
        None if subject.headers is None else list(subject.headers.items())
    )
finally:
    utils._HEADER_VALIDATORS_STR = original
"""
    )


def test_prepare_url_scheme_regex_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


class SchemePattern:
    def search(self, value):
        side_effects.append(["scheme-search", value])
        raise RuntimeError("scheme callback")


original = url_utils._SCHEME_RE
url_utils._SCHEME_RE = SchemePattern()
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url("http://example.com/a path", None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://example.com/a path",
                None,
            )
        ),
    )
finally:
    url_utils._SCHEME_RE = original
"""
    )


def test_prepare_url_uri_regex_replaced_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


class UriPattern:
    def match(self, value):
        side_effects.append(["uri-match", value])
        raise RuntimeError("uri callback")


original = url_utils._URI_RE
url_utils._URI_RE = UriPattern()
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url("http://example.com/a path", None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://example.com/a path",
                None,
            )
        ),
    )
finally:
    url_utils._URI_RE = original
"""
    )


def test_prepare_url_path_chars_subclass_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


class PathChars(set):
    def __contains__(self, value):
        side_effects.append(["path-contains", value])
        return False


original = url_utils._PATH_CHARS
url_utils._PATH_CHARS = PathChars(original)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url("http://example.com/path", None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://example.com/path",
                None,
            )
        ),
    )
finally:
    url_utils._PATH_CHARS = original
"""
    )


def test_prepare_url_scheme_tuple_subclass_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


class Schemes(tuple):
    def __contains__(self, value):
        side_effects.append(["scheme-contains", value])
        return False


original = url_utils._NORMALIZABLE_SCHEMES
url_utils._NORMALIZABLE_SCHEMES = Schemes(original)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url("http://EXAMPLE.com/path", None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://EXAMPLE.com/path",
                None,
            )
        ),
    )
finally:
    url_utils._NORMALIZABLE_SCHEMES = original
"""
    )


def test_prepare_url_scheme_string_subclass_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import urllib3.util.url as url_utils
from requests.models import PreparedRequest


class Scheme(str):
    def __eq__(self, other):
        side_effects.append(["scheme-eq", str.__str__(self), other])
        return False


original = url_utils._NORMALIZABLE_SCHEMES
url_utils._NORMALIZABLE_SCHEMES = (
    Scheme("http"),
    "https",
    None,
)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_url("http://EXAMPLE.com/path", None)
            if _requests_rust is None
            else _requests_rust._prepare_url_trial(
                subject,
                "http://EXAMPLE.com/path",
                None,
            )
        ),
    )
finally:
    url_utils._NORMALIZABLE_SCHEMES = original
"""
    )


def test_prepare_headers_validator_tuple_subclass_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import requests.utils as utils
from requests.models import PreparedRequest


class Validators(tuple):
    def __getitem__(self, index):
        side_effects.append(["validator-getitem", index])
        raise RuntimeError("validator callback")


original = utils._HEADER_VALIDATORS_STR
utils._HEADER_VALIDATORS_STR = Validators(original)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_headers({"Name": "value"})
            if _requests_rust is None
            else _requests_rust._prepare_headers_trial(
                subject,
                {"Name": "value"},
            )
        ),
    )
finally:
    utils._HEADER_VALIDATORS_STR = original
"""
    )


def test_prepare_headers_byte_validator_tuple_subclass_before_extension_import() -> (
    None
):
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import requests.utils as utils
from requests.models import PreparedRequest


class Validators(tuple):
    def __getitem__(self, index):
        side_effects.append(["byte-validator-getitem", index])
        raise RuntimeError("byte validator callback")


original = utils._HEADER_VALIDATORS_BYTE
utils._HEADER_VALIDATORS_BYTE = Validators(original)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_headers({b"Name": b"value"})
            if _requests_rust is None
            else _requests_rust._prepare_headers_trial(
                subject,
                {b"Name": b"value"},
            )
        ),
    )
finally:
    utils._HEADER_VALIDATORS_BYTE = original
"""
    )


def test_prepare_headers_redirected_pattern_type_before_extension_import() -> None:
    _assert_matches_oracle_before_extension_import(
        _PREIMPORT_CAPTURE_HELPER
        + """
import re
import requests.utils as utils
from requests.models import PreparedRequest


class Pattern:
    def __init__(self, pattern, flags):
        self.pattern = pattern
        self.flags = flags

    def match(self, value):
        side_effects.append(["fake-pattern-match", self.pattern, value])
        return True


original_pattern_type = re.Pattern
original_text_validators = utils._HEADER_VALIDATORS_STR
original_byte_validators = utils._HEADER_VALIDATORS_BYTE
re.Pattern = Pattern
utils._HEADER_VALIDATORS_STR = (
    Pattern(r"^[^:\\s][^:\\r\\n]*\\Z", 32),
    Pattern(r"^\\S[^\\r\\n]*\\Z|^\\Z", 32),
)
utils._HEADER_VALIDATORS_BYTE = (
    Pattern(br"^[^:\\s][^:\\r\\n]*\\Z", 0),
    Pattern(br"^\\S[^\\r\\n]*\\Z|^\\Z", 0),
)
try:
    try:
        from requests import _requests_rust
    except ImportError:
        _requests_rust = None

    subject = PreparedRequest()
    result = capture_preimport(
        subject,
        lambda: (
            subject.prepare_headers({"Bad": " leading"})
            if _requests_rust is None
            else _requests_rust._prepare_headers_trial(
                subject,
                {"Bad": " leading"},
            )
        ),
    )
finally:
    utils._HEADER_VALIDATORS_STR = original_text_validators
    utils._HEADER_VALIDATORS_BYTE = original_byte_validators
    re.Pattern = original_pattern_type
"""
    )


def test_prepare_headers_does_not_import_re_during_trial() -> None:
    _assert_matches_oracle(
        """
import sys
from requests.models import PreparedRequest


original = sys.modules["re"]
sys.modules["re"] = object()
try:
    subject = PreparedRequest()
    result = capture(
        "mutated-sys-modules-re",
        subject,
        lambda: prepare_headers_call(subject, {"Name": "value"}),
    )
finally:
    sys.modules["re"] = original
"""
    )


def test_http_unix_preserves_lowercase_reserved_percent_escapes() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


cases = [
    (
        "ordinary-http-parser-only",
        "HtTp://Example.COM/a%2fb?q=%2f#part=%2f",
    ),
    (
        "authority",
        "http+unix://%2fvar%2frun%2fsock/path",
    ),
    (
        "path",
        "http+unix://%2Fvar%2Frun%2Fsock/a%2fb",
    ),
    (
        "query",
        "http+unix://%2Fvar%2Frun%2Fsock/path?q=%2f",
    ),
    (
        "fragment",
        "http+unix://%2Fvar%2Frun%2Fsock/path#part=%2f",
    ),
]
result = []
for label, url in cases:
    subject = PreparedRequest()
    result.append(
        capture(
            label,
            subject,
            lambda subject=subject, url=url: prepare_url_call(
                subject,
                url,
                None,
            ),
        )
    )
"""
    )


def test_prepare_url_numeric_final_hostname_labels_delegate() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


result = []
for host in ("example.1", "example.01", "example.0x1"):
    subject = PreparedRequest()
    result.append(
        capture(
            host,
            subject,
            lambda subject=subject, host=host: prepare_url_call(
                subject,
                f"http://{host}/path",
                None,
            ),
        )
    )
"""
    )


def test_prepare_url_literal_final_dot_components_delegate() -> None:
    _assert_matches_oracle(
        """
from requests.models import PreparedRequest


urls = [
    "http://example.com/a/.?q=1",
    "http://example.com/a/..?q=1",
    "http://example.com/a/.#f",
    "http://example.com/a/..#f",
]
result = []
for url in urls:
    subject = PreparedRequest()
    result.append(
        capture(
            url,
            subject,
            lambda subject=subject, url=url: prepare_url_call(
                subject,
                url,
                None,
            ),
        )
    )
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

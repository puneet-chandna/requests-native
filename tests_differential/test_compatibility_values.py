from __future__ import annotations

import ast
import hashlib
import io
import json
import os
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path
from textwrap import dedent

import pytest
from tests_differential.runner import (
    REPOSITORY_ROOT,
    run_oracle_case,
    run_rewrite_case,
)

_SIMPLEJSON_VERSION = "4.1.1"
_SIMPLEJSON_URL = (
    "https://files.pythonhosted.org/packages/78/91/"
    "3635cdb13318cb0a328abaa69e2b91251caad39d6779aa308098f341f6cb/"
    "simplejson-4.1.1-cp314-cp314-manylinux1_x86_64."
    "manylinux_2_28_x86_64.manylinux_2_5_x86_64.whl"
)
_SIMPLEJSON_SHA256 = "3851658d642c1184d2023f0e6c9ce44a21eb1629e74e7c84ef956b128841fe12"


def _simplejson_wheel_bytes() -> bytes:
    supplied = os.environ.get("REQUESTS_SIMPLEJSON_WHEEL")
    if supplied is not None:
        content = Path(supplied).read_bytes()
    else:
        with urllib.request.urlopen(_SIMPLEJSON_URL, timeout=30) as response:
            content = response.read()
    if hashlib.sha256(content).hexdigest() != _SIMPLEJSON_SHA256:
        raise ValueError("simplejson wheel SHA-256 mismatch")
    return content


_TRIAL_HELPERS = """
try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


def internal_call(operation, *arguments):
    if _requests_rust is not None:
        return _requests_rust._internal_utils_trial(operation, arguments)
    from requests import _internal_utils
    return getattr(_internal_utils, operation)(*arguments)


def status_call(subject, operation, *arguments):
    if _requests_rust is not None:
        return _requests_rust._status_codes_trial(subject, operation, arguments)
    if operation == "getitem":
        return subject[arguments[0]]
    if operation == "get":
        return subject.get(*arguments)
    if operation == "getattr":
        return getattr(subject, arguments[0])
    if operation == "repr":
        return repr(subject)
    raise AssertionError(operation)
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def _matching_result(source: str) -> object:
    case = {"source": dedent(source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""
    return ast.literal_eval(oracle.observations["result"]["repr"])


def _assert_trial_matches_oracle(source: str) -> None:
    _assert_matches_oracle(_TRIAL_HELPERS + source)


def test_header_validators_preserve_keys_tuples_regex_identity_pattern_and_flags() -> (
    None
):
    _assert_matches_oracle(
        """
import re
from requests import _internal_utils as module
from requests import utils

byte_name, byte_value = module.HEADER_VALIDATORS[bytes]
text_name, text_value = module.HEADER_VALIDATORS[str]
result = {
    "keys": [key.__name__ for key in module.HEADER_VALIDATORS],
    "mapping_identity": utils.HEADER_VALIDATORS is module.HEADER_VALIDATORS,
    "tuple_identity": [
        module.HEADER_VALIDATORS[bytes] is module._HEADER_VALIDATORS_BYTE,
        module.HEADER_VALIDATORS[str] is module._HEADER_VALIDATORS_STR,
    ],
    "regex_identity": [
        byte_name is module._VALID_HEADER_NAME_RE_BYTE,
        byte_value is module._VALID_HEADER_VALUE_RE_BYTE,
        text_name is module._VALID_HEADER_NAME_RE_STR,
        text_value is module._VALID_HEADER_VALUE_RE_STR,
    ],
    "patterns": [
        byte_name.pattern.decode("ascii"),
        byte_value.pattern.decode("ascii"),
        text_name.pattern,
        text_value.pattern,
    ],
    "flags": [byte_name.flags, byte_value.flags, text_name.flags, text_value.flags],
    "unicode_flag": re.UNICODE,
}
"""
    )


def test_header_validators_accept_nul_high_bytes_and_unicode_and_share_mutation() -> (
    None
):
    result = _matching_result(
        """
from requests import _internal_utils as module
from requests import utils

byte_name, byte_value = module.HEADER_VALIDATORS[bytes]
text_name, text_value = module.HEADER_VALIDATORS[str]
accepted = [
    byte_name.fullmatch(b"\\x00name") is not None,
    byte_name.fullmatch(b"\\xffname") is not None,
    byte_value.fullmatch(b"\\x00value\\xff") is not None,
    text_name.fullmatch("\\x00é") is not None,
    text_value.fullmatch("\\x00é") is not None,
]
marker = (object(), object())
original = module.HEADER_VALIDATORS[str]
module.HEADER_VALIDATORS[str] = marker
try:
    shared = utils.HEADER_VALIDATORS[str] is marker
finally:
    module.HEADER_VALIDATORS[str] = original
result = {"accepted": accepted, "shared": shared}
"""
    )

    assert result == {"accepted": [True, True, True, True, True], "shared": True}


def test_to_native_string_exact_subclass_custom_decode_and_live_global_semantics() -> (
    None
):
    _assert_trial_matches_oracle(
        """
from requests import _internal_utils as module


class Text(str):
    def decode(self, encoding):
        side_effects.append(["text-decode", encoding])
        return "wrong"


class Bytes(bytes):
    def decode(self, encoding):
        side_effects.append(["bytes-decode", encoding])
        return "subclass-decoded"


class Custom:
    def decode(self, encoding):
        side_effects.append(["custom-decode", encoding])
        return "custom-decoded"


exact = "native"
subclass = Text("subclass")
result = {
    "exact_identity": internal_call("to_native_string", exact) is exact,
    "subclass_identity": internal_call("to_native_string", subclass) is subclass,
    "bytes": internal_call("to_native_string", b"caf\\xc3\\xa9", "utf-8"),
    "bytes_subclass": internal_call("to_native_string", Bytes(b"x"), "latin-1"),
    "custom": internal_call("to_native_string", Custom(), "utf-16"),
}

original = module.builtin_str
module.builtin_str = bytes
try:
    live = internal_call("to_native_string", b"unchanged")
finally:
    module.builtin_str = original
result["live_global_type"] = type(live).__name__
result["live_global_identity"] = live is b"unchanged"
"""
    )


def test_to_native_string_preserves_decode_exception_identity() -> None:
    _assert_trial_matches_oracle(
        """
class MarkerError(Exception):
    pass


marker = MarkerError("decode failed")


class Custom:
    def decode(self, encoding):
        side_effects.append(["decode", encoding])
        raise marker


try:
    internal_call("to_native_string", Custom(), "utf-8")
except BaseException as error:
    result = {
        "identity": error is marker,
        "type": type(error).__name__,
        "args": list(error.args),
    }
"""
    )


def test_internal_utils_trial_falls_back_for_replaced_and_in_place_mutated_functions() -> (
    None
):
    _assert_trial_matches_oracle(
        """
from requests import _internal_utils as module

original_to_native = module.to_native_string
original_code = original_to_native.__code__
_TASK14_EVENTS = side_effects
module._TASK14_EVENTS = side_effects


def replacement(*arguments):
    _TASK14_EVENTS.append(["replacement", len(arguments)])
    return "replacement"


module.to_native_string = replacement
try:
    replaced = internal_call("to_native_string", b"value")
finally:
    module.to_native_string = original_to_native

try:
    original_to_native.__code__ = replacement.__code__
    mutated = internal_call("to_native_string", b"value")
finally:
    original_to_native.__code__ = original_code

restored = internal_call("to_native_string", b"value")
del module._TASK14_EVENTS
result = [replaced, mutated, restored]
"""
    )


def test_unicode_is_ascii_assert_subclass_encode_and_exception_boundary() -> None:
    _assert_trial_matches_oracle(
        """
class Observed(str):
    def __new__(cls, value, mode):
        instance = super().__new__(cls, value)
        instance.mode = mode
        return instance

    def encode(self, encoding):
        side_effects.append(["encode", encoding, self.mode])
        if self.mode == "unicode":
            raise UnicodeEncodeError("ascii", "é", 0, 1, "ordinal")
        if self.mode == "value":
            raise ValueError("custom failure")
        return object()


values = [
    internal_call("unicode_is_ascii", "ascii"),
    internal_call("unicode_is_ascii", "é"),
    internal_call("unicode_is_ascii", Observed("x", "ok")),
    internal_call("unicode_is_ascii", Observed("x", "unicode")),
]
failures = []
for value in (object(), Observed("x", "value")):
    try:
        internal_call("unicode_is_ascii", value)
    except BaseException as error:
        failures.append(
            {
                "type": type(error).__name__,
                "args": list(error.args),
            }
        )
result = {"values": values, "failures": failures}
"""
    )


def test_unicode_is_ascii_uses_live_module_then_builtin_str_lookup() -> None:
    _assert_trial_matches_oracle(
        """
import builtins
from requests import _internal_utils as module


def outcome(value):
    try:
        if _requests_rust is None:
            returned = module.unicode_is_ascii(value)
        else:
            returned = _requests_rust._internal_utils_trial(
                "unicode_is_ascii", (value,)
            )
        return ["return", returned]
    except BaseException as error:
        return [
            "raise",
            type(error).__module__,
            type(error).__name__,
            list(error.args),
        ]


canonical_str = builtins.str
had_module_str = "str" in module.__dict__
saved_module_str = module.__dict__.get("str")
states = []
try:
    module.str = canonical_str
    states.append(["inserted-canonical", outcome("ascii")])

    module.str = bytes
    states.append(["rebound-bytes", outcome("ascii")])

    module.str = ()
    states.append(["rebound-tuple", outcome("ascii")])

    module.str = object()
    states.append(["rebound-sentinel", outcome("ascii")])

    del module.str
    states.append(["deleted", outcome("ascii")])

    module.str = canonical_str
    builtins.str = bytes
    try:
        states.append(["module-shadows-rebound-builtin", outcome("ascii")])
    finally:
        builtins.str = canonical_str

    del module.str
    builtins.str = bytes
    try:
        states.append(["missing-module-rebound-builtin", outcome("ascii")])
    finally:
        builtins.str = canonical_str
finally:
    if had_module_str:
        module.str = saved_module_str
    else:
        module.__dict__.pop("str", None)
    builtins.str = canonical_str

result = states
"""
    )


def test_status_codes_exact_rows_alias_order_collisions_and_generated_doc() -> None:
    _assert_matches_oracle(
        """
import requests
from requests import status_codes

attributes = vars(status_codes.codes)
doc_lines = [
    line for line in status_codes.__doc__.splitlines() if line.startswith("* ")
]
result = {
    "singleton": requests.codes is status_codes.codes,
    "rows": len(status_codes._codes),
    "row_order": list(status_codes._codes),
    "alias_count": len(attributes) - 1,
    "alias_order": [name for name in attributes if name != "name"],
    "collisions": {
        "uri_too_long": status_codes.codes.uri_too_long,
        "URI_TOO_LONG": status_codes.codes.URI_TOO_LONG,
        "precondition": status_codes.codes.precondition,
        "PRECONDITION": status_codes.codes.PRECONDITION,
    },
    "symbols": [
        status_codes.codes["\\\\o/"],
        status_codes.codes["✓"],
        status_codes.codes["-o-"],
        status_codes.codes["/o\\\\"],
        status_codes.codes["✗"],
    ],
    "doc_codes": [int(line.split(":", 1)[0][2:]) for line in doc_lines],
    "doc_lines": doc_lines,
    "dict_base": [len(status_codes.codes), list(status_codes.codes)],
}
assert result["rows"] == 68
assert result["alias_count"] == 243
"""
    )


def test_status_codes_frozen_hashes_and_empty_base_dict_contract() -> None:
    result = _matching_result(
        """
import hashlib
from requests import status_codes

codes = status_codes.codes
dict.__setitem__(codes, "base-only", 799)
try:
    result = {
        "codes_hash": hashlib.sha256(
            repr(status_codes._codes).encode("utf-8")
        ).hexdigest(),
        "aliases_hash": hashlib.sha256(
            repr(list(codes.__dict__.items())).encode("utf-8")
        ).hexdigest(),
        "doc_length": len(status_codes.__doc__),
        "doc_hash": hashlib.sha256(
            status_codes.__doc__.encode("utf-8")
        ).hexdigest(),
        "base": [
            dict.__getitem__(codes, "base-only"),
            codes["base-only"],
            codes.get("base-only"),
            "base-only" in codes,
        ],
    }
finally:
    dict.__delitem__(codes, "base-only")
"""
    )

    assert result == {
        "codes_hash": "bb20fcd913fbde9863d1b6b26fe3600d0f3dcd9992f0c981815d7f904417ff63",
        "aliases_hash": "eb21acace5186e25ce8b2a9b3ff8878723dfaf110d192c4ffe7e8eb06fa8165a",
        "doc_length": 3378,
        "doc_hash": "cb19cba7fc6a23f269cba03e1b461b2f52904fdbd1a0d957516d11f2280701c1",
        "base": [799, None, None, True],
    }


def test_status_codes_reload_creates_new_submodule_singleton_and_keeps_root_stale() -> (
    None
):
    _assert_matches_oracle(
        """
import importlib
import requests
from requests import status_codes

root_codes = requests.codes
module_codes = status_codes.codes
reloaded = importlib.reload(status_codes)
result = {
    "old_root": root_codes is module_codes,
    "new_module": reloaded.codes is module_codes,
    "root_stale": requests.codes is root_codes and requests.codes is not reloaded.codes,
    "new_repr": repr(reloaded.codes),
}
"""
    )


def test_status_lookup_item_get_attribute_mutation_repr_and_unknown_semantics() -> None:
    _assert_trial_matches_oracle(
        """
from requests import status_codes

codes = status_codes.codes
before = {
    "item": status_call(codes, "getitem", "ok"),
    "get": status_call(codes, "get", "missing"),
    "default": status_call(codes, "get", "missing", "fallback"),
    "repr": status_call(codes, "repr"),
}
try:
    status_call(codes, "getattr", "missing")
except BaseException as error:
    unknown = {"type": type(error).__name__, "args": list(error.args)}

codes.mutable = 701
mutated = [
    status_call(codes, "getitem", "mutable"),
    status_call(codes, "get", "mutable"),
    status_call(codes, "getattr", "mutable"),
]
del codes.mutable
result = {"before": before, "unknown": unknown, "mutated": mutated}
"""
    )


def test_status_lookup_trial_falls_back_for_mutated_lookupdict_behavior() -> None:
    _assert_trial_matches_oracle(
        """
from requests import status_codes
from requests.structures import LookupDict

original = LookupDict.__getitem__


def replacement(self, key):
    side_effects.append(["getitem", key])
    return 799


LookupDict.__getitem__ = replacement
try:
    mutated = status_call(status_codes.codes, "getitem", "ok")
finally:
    LookupDict.__getitem__ = original
restored = status_call(status_codes.codes, "getitem", "ok")
result = [mutated, restored]
"""
    )


@pytest.mark.parametrize(
    ("version_expression", "expected"),
    [
        ("'1.26.20'", True),
        ("'2.7.0'", False),
        ("None", True),
        ("VersionTypeError()", True),
    ],
)
def test_compat_urllib3_version_parsing_and_documented_fallback(
    version_expression: str, expected: bool
) -> None:
    _assert_matches_oracle(
        f"""
import importlib
import requests.compat as compat
import urllib3


class VersionTypeError:
    def split(self, separator):
        return [object()]


original = urllib3.__version__
urllib3.__version__ = {version_expression}
try:
    compat = importlib.reload(compat)
    result = compat.is_urllib3_1
finally:
    urllib3.__version__ = original
    importlib.reload(compat)
assert result is {expected!r}
"""
    )


def test_compat_urllib3_version_only_catches_type_and_attribute_errors() -> None:
    _assert_matches_oracle(
        """
import importlib
import requests.compat as compat
import urllib3


class ExplodingVersion:
    def split(self, separator):
        side_effects.append(["split", separator])
        raise RuntimeError("version hook failed")


original = urllib3.__version__
urllib3.__version__ = ExplodingVersion()
try:
    try:
        importlib.reload(compat)
    except BaseException as error:
        result = {"type": type(error).__name__, "args": list(error.args)}
finally:
    urllib3.__version__ = original
    importlib.reload(compat)
"""
    )


def test_compat_urllib3_nonnumeric_major_propagates_value_error() -> None:
    _assert_matches_oracle(
        """
import importlib
import requests.compat as compat
import urllib3

original = urllib3.__version__
urllib3.__version__ = "dev"
try:
    try:
        importlib.reload(compat)
    except BaseException as error:
        result = {"type": type(error).__name__, "args": list(error.args)}
finally:
    urllib3.__version__ = original
    importlib.reload(compat)
"""
    )


def test_compat_character_detector_prefers_chardet_then_charset_normalizer() -> None:
    _assert_matches_oracle(
        """
import types
import requests.compat as compat

original = compat.importlib.import_module
first = types.ModuleType("first-chardet")
second = types.ModuleType("second-charset")


def prefer_first(name):
    side_effects.append(["first", name])
    return first if name == "chardet" else second


def prefer_second(name):
    side_effects.append(["second", name])
    if name == "chardet":
        raise ImportError("missing")
    return second


try:
    compat.importlib.import_module = prefer_first
    selected_first = compat._resolve_char_detection()
    compat.importlib.import_module = prefer_second
    selected_second = compat._resolve_char_detection()
finally:
    compat.importlib.import_module = original
result = [
    selected_first.__name__,
    selected_second.__name__,
]
"""
    )


def test_compat_character_detector_catches_import_error_only() -> None:
    _assert_matches_oracle(
        """
import requests.compat as compat

original = compat.importlib.import_module


def explode(name):
    side_effects.append(["import", name])
    raise RuntimeError("finder failed")


try:
    compat.importlib.import_module = explode
    try:
        compat._resolve_char_detection()
    except BaseException as error:
        result = {"type": type(error).__name__, "args": list(error.args)}
finally:
    compat.importlib.import_module = original
"""
    )


def test_both_character_detectors_missing_selects_none_and_warns_once() -> None:
    case = {
        "source": dedent(
            """
import importlib.abc
import sys


class BlockDetectors(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname in {"chardet", "charset_normalizer"}:
            raise ModuleNotFoundError(fullname)
        return None


sys.meta_path.insert(0, BlockDetectors())
import requests
import requests.compat as compat
result = {
    "selected": compat.chardet,
    "version": requests.__version__,
}
"""
        )
    }
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""
    assert oracle.observations["warnings"] == [
        {
            "category": {
                "module": "requests.exceptions",
                "name": "RequestsDependencyWarning",
            },
            "message": (
                "Unable to find acceptable character detection dependency "
                "(chardet or charset_normalizer)."
            ),
        }
    ]


@pytest.mark.parametrize("blocked", ["chardet", "simplejson"])
def test_broken_optional_imports_propagate_non_import_errors(blocked: str) -> None:
    _assert_matches_oracle(
        f"""
import importlib.abc
import sys


class BreakOptional(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname == {blocked!r}:
            raise RuntimeError("broken optional import")
        return None


sys.meta_path.insert(0, BreakOptional())
import requests
result = requests.__version__
"""
    )


@pytest.mark.parametrize("simplejson", [False, True])
def test_compat_json_module_and_decode_error_are_selected_together(
    simplejson: bool,
) -> None:
    setup = ""
    if simplejson:
        setup = """
import sys
import types

simplejson = types.ModuleType("simplejson")


class SelectedJSONDecodeError(ValueError):
    pass


SelectedJSONDecodeError.__module__ = "simplejson.errors"
simplejson.JSONDecodeError = SelectedJSONDecodeError
sys.modules["simplejson"] = simplejson
"""
    _assert_matches_oracle(
        setup
        + """
import requests.compat as compat

result = {
    "has_simplejson": compat.has_simplejson,
    "json_name": compat.json.__name__,
    "error_module": compat.JSONDecodeError.__module__,
    "selected_together": compat.JSONDecodeError is compat.json.JSONDecodeError,
}
"""
    )


def test_compat_reexports_and_legacy_type_tuples_preserve_identity_and_order() -> None:
    _assert_matches_oracle(
        """
import collections
import collections.abc
import http.cookiejar
import http.cookies
import io
import urllib.parse
import urllib.request
import requests.compat as compat

result = {
    "types": [
        compat.builtin_str is str,
        compat.str is str,
        compat.bytes is bytes,
        compat.basestring == (str, bytes),
        compat.numeric_types == (int, float),
        compat.integer_types == (int,),
    ],
    "objects": [
        compat.OrderedDict is collections.OrderedDict,
        compat.Callable is collections.abc.Callable,
        compat.Mapping is collections.abc.Mapping,
        compat.MutableMapping is collections.abc.MutableMapping,
        compat.cookielib is http.cookiejar,
        compat.Morsel is http.cookies.Morsel,
        compat.StringIO is io.StringIO,
        compat.urlparse is urllib.parse.urlparse,
        compat.getproxies is urllib.request.getproxies,
    ],
    "versions": [compat.is_py2, compat.is_py3],
}
"""
    )


def test_packages_preserve_loaded_alias_identity_and_import_timing() -> None:
    _assert_matches_oracle(
        """
import sys
import types
import urllib3.util.retry
import idna.core
import charset_normalizer

probe = types.ModuleType("charset_normalizer.task14_probe")
sys.modules[probe.__name__] = probe

import requests
import requests.packages as packages

late = types.ModuleType("idna.task14_late")
sys.modules[late.__name__] = late
result = {
    "module_attrs": [
        packages.urllib3 is sys.modules["urllib3"],
        packages.idna is sys.modules["idna"],
        packages.chardet is charset_normalizer,
    ],
    "loaded_aliases": [
        sys.modules["requests.packages.urllib3.util.retry"]
        is sys.modules["urllib3.util.retry"],
        sys.modules["requests.packages.idna.core"] is sys.modules["idna.core"],
        sys.modules["requests.packages.charset_normalizer.task14_probe"] is probe,
        sys.modules["requests.packages.chardet.task14_probe"] is probe,
    ],
    "late_alias_absent": "requests.packages.idna.task14_late" not in sys.modules,
}
"""
    )


def test_certs_version_root_exports_filters_logger_and_compatibility_checks() -> None:
    _assert_matches_oracle(
        """
import logging
import warnings
import certifi
import requests
import requests.certs as certs
import requests.exceptions as exceptions
from urllib3.exceptions import DependencyWarning

compatibility = []
for arguments in [
    ("2.7.0", None, "3.4.3"),
    ("1.21.1", "7.0.0", None),
    ("1.20.0", "7.0.0", None),
    ("2.7.0", None, None),
]:
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        try:
            requests.check_compatibility(*arguments)
            error = None
        except BaseException as caught_error:
            error = [type(caught_error).__name__, list(caught_error.args)]
    compatibility.append(
        {
            "error": error,
            "warnings": [
                [item.category.__module__, item.category.__name__, str(item.message)]
                for item in caught
            ],
        }
    )

filters = [
    [action, category.__module__, category.__name__]
    for action, _message, category, _module, _lineno in warnings.filters
    if category in (DependencyWarning, exceptions.FileModeWarning)
]
result = {
    "cert_identity": certs.where is certifi.where,
    "cert_value": certs.where(),
    "version": [
        requests.__version__,
        requests.__build__,
        requests.__title__,
        requests.__description__,
        requests.__url__,
        requests.__author__,
        requests.__author_email__,
        requests.__license__,
        requests.__copyright__,
        requests.__cake__,
    ],
    "all": list(requests.__all__),
    "all_count": len(requests.__all__),
    "compatibility": compatibility,
    "filters": filters,
    "null_handlers": [
        type(handler).__module__ + "." + type(handler).__name__
        for handler in logging.getLogger("requests").handlers
    ],
}
assert result["all_count"] == 25
assert result["version"][0] == "2.34.2"
"""
    )


def test_root_import_dependency_warning_has_exact_category_message_and_filtering() -> (
    None
):
    _assert_matches_oracle(
        """
import urllib3

urllib3.__version__ = "1.20.0"
import requests
result = requests.__version__
"""
    )


def test_certs_module_cli_prints_certifi_path(oracle_root: Path) -> None:
    environment = {
        name: os.environ[name]
        for name in ("PATH", "LD_LIBRARY_PATH", "LANG", "LC_ALL", "LC_CTYPE")
        if name in os.environ
    }
    records = []
    for package_root in (oracle_root, REPOSITORY_ROOT.resolve()):
        child_environment = environment | {
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONNOUSERSITE": "1",
            "PYTHONPATH": str(package_root / "src"),
        }
        subprocess.run(
            [
                sys.executable,
                "-c",
                "import requests, sys; from pathlib import Path; "
                "assert Path(requests.__file__).resolve() == "
                "Path(sys.argv[1]) / 'src/requests/__init__.py'",
                str(package_root),
            ],
            cwd=REPOSITORY_ROOT,
            env=child_environment,
            capture_output=True,
            text=True,
            check=True,
        )
        completed = subprocess.run(
            [sys.executable, "-m", "requests.certs"],
            cwd=REPOSITORY_ROOT,
            env=child_environment,
            text=True,
            capture_output=True,
            check=False,
        )
        records.append((completed.returncode, completed.stdout, completed.stderr))

    assert records[1] == records[0]
    assert records[0][0] == 0
    assert records[0][1].strip()


def test_simplejson_fixture_accepts_only_verified_offline_bytes(
    monkeypatch, tmp_path: Path
) -> None:
    content = b"isolated fixture bytes"
    wheel = tmp_path / "simplejson.whl"
    wheel.write_bytes(content)
    monkeypatch.setenv("REQUESTS_SIMPLEJSON_WHEEL", str(wheel))
    monkeypatch.setitem(
        globals(), "_SIMPLEJSON_SHA256", hashlib.sha256(content).hexdigest()
    )
    monkeypatch.setattr(
        urllib.request,
        "urlopen",
        lambda *args, **kwargs: pytest.fail("offline fixture must not use the network"),
    )
    assert _simplejson_wheel_bytes() == content
    wheel.write_bytes(b"changed fixture bytes")
    with pytest.raises(ValueError, match="SHA-256 mismatch"):
        _simplejson_wheel_bytes()


def test_configured_oracle_must_exist_before_comparison(monkeypatch, tmp_path):
    from tests_differential.conftest import oracle_root

    monkeypatch.setenv("REQUESTS_ORACLE_ROOT", str(tmp_path / "missing-oracle"))
    with pytest.raises(pytest.fail.Exception, match="configured oracle package"):
        oracle_root.__wrapped__()


def test_real_simplejson_constructor_and_pickle_lane(
    tmp_path: Path, oracle_root: Path
) -> None:
    with zipfile.ZipFile(io.BytesIO(_simplejson_wheel_bytes())) as archive:
        archive.extractall(tmp_path)

    source = dedent(
        f"""
        import json
        import os
        import pickle
        from pathlib import Path
        import requests
        import requests.compat as compat
        import requests.exceptions as exceptions
        import simplejson

        assert Path(requests.__file__).resolve() == (
            Path(os.environ["EXPECTED_REQUESTS_ROOT"]) / "src/requests/__init__.py"
        )
        assert Path(simplejson.__file__).resolve().is_relative_to(Path({str(tmp_path)!r}))

        error = exceptions.JSONDecodeError(
            "broken", "{{", 1, request="request-marker"
        )
        restored = pickle.loads(pickle.dumps(error))
        result = {{
            "provenance": {{
                "version": simplejson.__version__,
                "url": {_SIMPLEJSON_URL!r},
                "sha256": {_SIMPLEJSON_SHA256!r},
            }},
            "selection": [
                compat.has_simplejson,
                compat.json is simplejson,
                compat.JSONDecodeError is simplejson.JSONDecodeError,
            ],
            "mro": [
                [base.__module__, base.__name__]
                for base in exceptions.JSONDecodeError.__mro__
            ],
            "fields": [
                error.args,
                error.msg,
                error.doc,
                error.pos,
                error.lineno,
                error.colno,
                error.request,
                error.response,
            ],
            "restored": [
                restored.args,
                restored.msg,
                restored.doc,
                restored.pos,
                restored.lineno,
                restored.colno,
                restored.request,
                restored.response,
            ],
        }}
        print(json.dumps(result, ensure_ascii=False))
        """
    )
    records = []
    for package_root in (oracle_root, REPOSITORY_ROOT.resolve()):
        environment = os.environ.copy()
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        environment["PYTHONNOUSERSITE"] = "1"
        environment["EXPECTED_REQUESTS_ROOT"] = str(package_root)
        environment["PYTHONPATH"] = os.pathsep.join(
            (str(tmp_path), str(package_root / "src"))
        )
        completed = subprocess.run(
            [sys.executable, "-c", source],
            cwd=REPOSITORY_ROOT,
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )
        assert completed.returncode == 0, completed.stderr
        assert completed.stderr == ""
        records.append(json.loads(completed.stdout))

    assert records[1] == records[0]
    assert records[0]["provenance"] == {
        "version": _SIMPLEJSON_VERSION,
        "url": _SIMPLEJSON_URL,
        "sha256": _SIMPLEJSON_SHA256,
    }

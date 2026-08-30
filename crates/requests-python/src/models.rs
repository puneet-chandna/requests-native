use std::collections::HashSet;
use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBool, PyBytes, PyBytesMethods, PyCFunction, PyCode, PyDict,
    PyDictMethods, PyFrozenSet, PyFunction, PyList, PyListMethods, PyModule, PySet, PySetMethods,
    PyString, PyTuple, PyTupleMethods, PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::utils::{encode_query_pairs, trim_python_whitespace_start};
use requests::{
    ErrorKind, HeaderInput, HeaderPart, HeaderPreparationError, InvalidHeaderPart, PreparedHeader,
    UrlPreparationError, append_url_params, is_non_http_url, prepare_headers, prepare_method,
    prepare_method_bytes, prepare_url, url_is_native_safe,
};

use crate::body::{_prepare_body_trial, _prepare_content_length_trial};
use crate::errors::{MappingSite, map_typed_message};

struct CanonicalFunction {
    code: Py<PyCode>,
    globals: Py<PyDict>,
    builtins_module: Py<PyModule>,
    builtins: Py<PyDict>,
    defaults: CanonicalDefaults,
    dependencies: Vec<CanonicalGlobal>,
}

enum CanonicalDefaults {
    None,
    Captured {
        defaults: Option<Py<PyAny>>,
        kwdefaults: Option<Py<PyAny>>,
    },
    ToNativeString,
    SingleNone,
    SingleEmptyTuple,
}

struct CanonicalGlobal {
    name: String,
    resolution: CanonicalGlobalResolution,
}

enum CanonicalGlobalResolution {
    Module {
        expected: Py<PyAny>,
        function: Option<Box<CanonicalFunction>>,
    },
    KnownModule {
        value: KnownValue,
        code: Option<KnownCode>,
    },
    IntrinsicBuiltin(IntrinsicBuiltin),
    Missing,
    Unprovable,
}

#[derive(Clone, Copy)]
enum IntrinsicBuiltin {
    Function(&'static str),
    Str,
    Bytes,
    Bool,
    Int,
    List,
    Tuple,
    ByteArray,
    Range,
    Map,
    Type,
    TypeError,
    ValueError,
    AttributeError,
    RuntimeError,
    LookupError,
    UnicodeDecodeError,
    UnicodeEncodeError,
    ImportError,
    KeyError,
}

#[derive(Clone, Copy)]
enum KnownRegexPattern {
    Text(&'static str),
    Bytes(&'static [u8]),
    Generated(GeneratedRegex),
}

#[derive(Clone, Copy)]
enum GeneratedRegex {
    Ipv6AddressWithZone,
    ZoneId,
    HostPort,
}

#[derive(Clone, Copy)]
struct KnownRegex {
    pattern: KnownRegexPattern,
    flags: i64,
}

#[derive(Clone, Copy)]
enum KnownValue {
    Intrinsic(IntrinsicBuiltin),
    IntrinsicPair(IntrinsicBuiltin, IntrinsicBuiltin),
    TextPairAndNone(&'static str, &'static str),
    TextSet(&'static str),
    TextFrozenSet(&'static str),
    TextListContaining(&'static [&'static str]),
    Bytes(&'static [u8]),
    EmptyDict,
    UrlType,
    ByteQuoterFactory,
    QuoterType {
        name: &'static str,
        default_dict_base: bool,
    },
    Regex(KnownRegex),
    RegexPair(KnownRegex, KnownRegex),
}

enum KnownCode {
    Url {
        new: Py<PyCode>,
        globals: Py<PyDict>,
        builtins: Py<PyDict>,
    },
    ByteQuoterFactory {
        factory: Py<PyCode>,
        quoter_init: Py<PyCode>,
        quoter_missing: Py<PyCode>,
        globals: Py<PyDict>,
    },
    QuoterType {
        init: Py<PyCode>,
        missing: Py<PyCode>,
        globals: Py<PyDict>,
    },
}

#[derive(Clone, Copy)]
enum IntrinsicDescriptorType {
    Method,
    Member,
}

#[derive(Clone, Copy)]
enum DefaultPolicy {
    None,
    Captured,
    ToNativeString,
    SingleNone,
    SingleEmptyTuple,
}

struct ModelsState {
    internal_utils: Py<PyModule>,
    builtins: Py<PyModule>,
    models: Py<PyModule>,
    structures: Py<PyModule>,
    utils: Py<PyModule>,
    prepared_request: Py<PyType>,
    method_type: Py<PyType>,
    case_insensitive_dict: Py<PyType>,
    case_insensitive_dict_init: Py<PyAny>,
    case_insensitive_dict_new: Py<PyAny>,
    case_insensitive_dict_getattribute: Py<PyAny>,
    case_insensitive_dict_setattr: Py<PyAny>,
    case_insensitive_dict_update: Py<PyAny>,
    case_insensitive_dict_setitem: Py<PyAny>,
    ordered_dict: Py<PyAny>,
    check_header_validity: Py<PyAny>,
    validate_header_part: Py<PyAny>,
    header_validators_str: Py<PyAny>,
    header_validators_byte: Py<PyAny>,
    utils_str: Py<PyAny>,
    utils_bytes: Py<PyAny>,
    to_native_string: Py<PyAny>,
    invalid_header: Py<PyAny>,
    invalid_url: Py<PyAny>,
    location_parse_error: Py<PyAny>,
    missing_schema: Py<PyAny>,
    parse_url: Py<PyAny>,
    requote_uri: Py<PyAny>,
    unicode_is_ascii: Py<PyAny>,
    urlunparse: Py<PyAny>,
    encode_params_descriptor: Py<PyAny>,
    builtin_str: Py<PyAny>,
    prepare_method: Py<PyAny>,
    prepare_headers: Py<PyAny>,
    prepare_url: Py<PyAny>,
    object_getattribute: Py<PyAny>,
    object_setattr: Py<PyAny>,
    prepare_method_trust: CanonicalFunction,
    prepare_headers_trust: CanonicalFunction,
    prepare_url_trust: CanonicalFunction,
    encode_params_function: Py<PyAny>,
    encode_params_trust: CanonicalFunction,
    to_native_string_trust: CanonicalFunction,
    parse_url_trust: CanonicalFunction,
    requote_uri_trust: CanonicalFunction,
    unicode_is_ascii_trust: CanonicalFunction,
    urlunparse_trust: CanonicalFunction,
    check_header_validity_trust: CanonicalFunction,
    validate_header_part_trust: CanonicalFunction,
    case_insensitive_dict_init_trust: CanonicalFunction,
    case_insensitive_dict_update_trust: CanonicalFunction,
    case_insensitive_dict_setitem_trust: CanonicalFunction,
}

static MODELS_STATE: PyOnceLock<ModelsState> = PyOnceLock::new();

struct PrepareBodyState {
    prepare_body: Py<PyAny>,
    prepare_body_trust: CanonicalFunction,
}

static PREPARE_BODY_STATE: PyOnceLock<PrepareBodyState> = PyOnceLock::new();

struct PrepareContentLengthState {
    prepare_content_length: Py<PyAny>,
    prepare_content_length_trust: CanonicalFunction,
}

static PREPARE_CONTENT_LENGTH_STATE: PyOnceLock<PrepareContentLengthState> = PyOnceLock::new();

struct RewindBodyState {
    rewind_body: Py<PyAny>,
    rewind_body_trust: CanonicalFunction,
}

static REWIND_BODY_STATE: PyOnceLock<RewindBodyState> = PyOnceLock::new();

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn function_code<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyCode>> {
    // SAFETY: CPython returns a borrowed reference owned by this exact
    // PyFunction while both values remain attached to `py`.
    unsafe {
        Ok(
            Bound::from_borrowed_ptr(py, pyo3::ffi::PyFunction_GetCode(function.as_ptr()))
                .cast_into::<PyCode>()?,
        )
    }
}

#[cfg(PyPy)]
fn function_code<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyCode>> {
    Ok(function.getattr("__code__")?.cast_into::<PyCode>()?)
}

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn function_globals<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyDict>> {
    // SAFETY: CPython returns a borrowed reference owned by this exact
    // PyFunction while both values remain attached to `py`.
    unsafe {
        Ok(
            Bound::from_borrowed_ptr(py, pyo3::ffi::PyFunction_GetGlobals(function.as_ptr()))
                .cast_into::<PyDict>()?,
        )
    }
}

#[cfg(PyPy)]
fn function_globals<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyDict>> {
    Ok(function.getattr("__globals__")?.cast_into::<PyDict>()?)
}

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn function_defaults<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by
    // this exact PyFunction while both values remain attached to `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyFunction_GetDefaults(function.as_ptr()))
    }
}

#[cfg(PyPy)]
fn function_defaults<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    function
        .getattr("__defaults__")
        .ok()
        .filter(|value| !value.is_none())
}

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn function_kwdefaults<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by
    // this exact PyFunction while both values remain attached to `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyFunction_GetKwDefaults(function.as_ptr()))
    }
}

#[cfg(PyPy)]
fn function_kwdefaults<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    function
        .getattr("__kwdefaults__")
        .ok()
        .filter(|value| !value.is_none())
}

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn function_closure<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by
    // this exact PyFunction while both values remain attached to `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyFunction_GetClosure(function.as_ptr()))
    }
}

#[cfg(PyPy)]
fn function_closure<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    function
        .getattr("__closure__")
        .ok()
        .filter(|value| !value.is_none())
}

#[cfg(not(PyPy))]
#[allow(unsafe_code)]
fn c_function_self<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyCFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by
    // this exact PyCFunction while both values remain attached to `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyCFunction_GetSelf(function.as_ptr()))
    }
}

#[cfg(PyPy)]
fn c_function_self<'py>(
    _py: Python<'py>,
    function: &Bound<'py, PyCFunction>,
) -> Option<Bound<'py, PyAny>> {
    function
        .getattr("__self__")
        .ok()
        .filter(|value| !value.is_none())
}

fn intrinsic_builtin_is(
    py: Python<'_>,
    state_builtins: &Bound<'_, PyModule>,
    current: &Bound<'_, PyAny>,
    intrinsic: IntrinsicBuiltin,
) -> PyResult<bool> {
    match intrinsic {
        IntrinsicBuiltin::Str => Ok(current.is(py.get_type::<PyString>())),
        IntrinsicBuiltin::Bytes => Ok(current.is(py.get_type::<PyBytes>())),
        IntrinsicBuiltin::Bool => Ok(current.is(py.get_type::<PyBool>())),
        IntrinsicBuiltin::Int => Ok(current.is(py.get_type::<pyo3::types::PyInt>())),
        IntrinsicBuiltin::List => Ok(current.is(py.get_type::<PyList>())),
        IntrinsicBuiltin::Tuple => Ok(current.is(py.get_type::<PyTuple>())),
        IntrinsicBuiltin::ByteArray => Ok(current.is(py.get_type::<pyo3::types::PyByteArray>())),
        IntrinsicBuiltin::Type => Ok(current.is(py.get_type::<PyType>())),
        IntrinsicBuiltin::TypeError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyTypeError>()))
        }
        IntrinsicBuiltin::ValueError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyValueError>()))
        }
        IntrinsicBuiltin::AttributeError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyAttributeError>()))
        }
        IntrinsicBuiltin::RuntimeError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyRuntimeError>()))
        }
        IntrinsicBuiltin::LookupError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyLookupError>()))
        }
        IntrinsicBuiltin::UnicodeDecodeError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyUnicodeDecodeError>()))
        }
        IntrinsicBuiltin::UnicodeEncodeError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyUnicodeEncodeError>()))
        }
        IntrinsicBuiltin::ImportError => {
            Ok(current.is(py.get_type::<pyo3::exceptions::PyImportError>()))
        }
        IntrinsicBuiltin::KeyError => Ok(current.is(py.get_type::<pyo3::exceptions::PyKeyError>())),
        // Neither builtin has a portable immutable identity API.  Reading the
        // live module binding would let a pre-admission rebound become the
        // supposed canonical value, so conservatively keep these paths in
        // Python.
        IntrinsicBuiltin::Range | IntrinsicBuiltin::Map => Ok(false),
        IntrinsicBuiltin::Function(expected_name) => {
            let Ok(function) = current.cast::<PyCFunction>() else {
                return Ok(false);
            };
            Ok(
                function.getattr("__name__")?.extract::<String>()? == expected_name
                    && function.getattr("__module__")?.extract::<String>()? == "builtins"
                    && c_function_self(py, function).is_some_and(|owner| owner.is(state_builtins)),
            )
        }
    }
}

fn intrinsic_builtin_for_name(name: &str) -> Option<IntrinsicBuiltin> {
    match name {
        "getattr" | "setattr" | "isinstance" | "hasattr" | "len" | "ord" | "hex" | "chr" => {
            Some(IntrinsicBuiltin::Function(match name {
                "getattr" => "getattr",
                "setattr" => "setattr",
                "isinstance" => "isinstance",
                "hasattr" => "hasattr",
                "len" => "len",
                "ord" => "ord",
                "hex" => "hex",
                "chr" => "chr",
                _ => unreachable!(),
            }))
        }
        "str" => Some(IntrinsicBuiltin::Str),
        "bytes" => Some(IntrinsicBuiltin::Bytes),
        "bool" => Some(IntrinsicBuiltin::Bool),
        "int" => Some(IntrinsicBuiltin::Int),
        "list" => Some(IntrinsicBuiltin::List),
        "tuple" => Some(IntrinsicBuiltin::Tuple),
        "bytearray" => Some(IntrinsicBuiltin::ByteArray),
        "range" => Some(IntrinsicBuiltin::Range),
        "map" => Some(IntrinsicBuiltin::Map),
        "type" => Some(IntrinsicBuiltin::Type),
        "TypeError" => Some(IntrinsicBuiltin::TypeError),
        "ValueError" => Some(IntrinsicBuiltin::ValueError),
        "AttributeError" => Some(IntrinsicBuiltin::AttributeError),
        "RuntimeError" => Some(IntrinsicBuiltin::RuntimeError),
        "LookupError" => Some(IntrinsicBuiltin::LookupError),
        "UnicodeDecodeError" => Some(IntrinsicBuiltin::UnicodeDecodeError),
        "UnicodeEncodeError" => Some(IntrinsicBuiltin::UnicodeEncodeError),
        "ImportError" => Some(IntrinsicBuiltin::ImportError),
        "KeyError" => Some(IntrinsicBuiltin::KeyError),
        _ => None,
    }
}

pub(crate) fn intrinsic_builtin_name_is(
    py: Python<'_>,
    builtins: &Bound<'_, PyDict>,
    name: &str,
) -> PyResult<bool> {
    let Some(intrinsic) = intrinsic_builtin_for_name(name) else {
        return Ok(false);
    };
    let Some(current) = builtins.get_item(name)? else {
        return Ok(false);
    };
    intrinsic_builtin_is(py, &PyModule::import(py, "builtins")?, &current, intrinsic)
}

fn urllib3_ipv6_pattern() -> String {
    const HEX: &str = "[0-9A-Fa-f]{1,4}";
    const IPV4: &str = r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}";
    const VARIATIONS: [&str; 9] = [
        "(?:%(hex)s:){6}%(ls32)s",
        "::(?:%(hex)s:){5}%(ls32)s",
        "(?:%(hex)s)?::(?:%(hex)s:){4}%(ls32)s",
        "(?:(?:%(hex)s:)?%(hex)s)?::(?:%(hex)s:){3}%(ls32)s",
        "(?:(?:%(hex)s:){0,2}%(hex)s)?::(?:%(hex)s:){2}%(ls32)s",
        "(?:(?:%(hex)s:){0,3}%(hex)s)?::%(hex)s:%(ls32)s",
        "(?:(?:%(hex)s:){0,4}%(hex)s)?::%(ls32)s",
        "(?:(?:%(hex)s:){0,5}%(hex)s)?::%(hex)s",
        "(?:(?:%(hex)s:){0,6}%(hex)s)?::",
    ];
    let ls32 = format!("(?:{HEX}:{HEX}|{IPV4})");
    let alternatives = VARIATIONS
        .iter()
        .map(|variation| variation.replace("%(hex)s", HEX).replace("%(ls32)s", &ls32))
        .collect::<Vec<_>>()
        .join("|");
    format!("(?:{alternatives})")
}

fn urllib3_zone_id_pattern() -> String {
    let mut pattern = String::from("(?:%25|%)(?:[");
    pattern.push_str(r"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._\-~");
    pattern.push_str("]|%[a-fA-F0-9]{2})+");
    pattern
}

fn urllib3_ipv6_address_with_zone_body() -> String {
    format!(
        r"\[{}(?:{})?\]",
        urllib3_ipv6_pattern(),
        urllib3_zone_id_pattern()
    )
}

fn generated_regex_pattern(regex: GeneratedRegex) -> &'static str {
    static IPV6_ADDRESS_WITH_ZONE: OnceLock<String> = OnceLock::new();
    static ZONE_ID: OnceLock<String> = OnceLock::new();
    static HOST_PORT: OnceLock<String> = OnceLock::new();

    match regex {
        GeneratedRegex::Ipv6AddressWithZone => IPV6_ADDRESS_WITH_ZONE
            .get_or_init(|| format!("^{}$", urllib3_ipv6_address_with_zone_body())),
        GeneratedRegex::ZoneId => {
            ZONE_ID.get_or_init(|| format!(r"({})\]$", urllib3_zone_id_pattern()))
        }
        GeneratedRegex::HostPort => HOST_PORT.get_or_init(|| {
            const REG_NAME: &str = r"(?:[^\[\]%:/?#]|%[a-fA-F0-9]{2})*";
            const IPV4: &str = r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}";
            format!(
                "^({REG_NAME}|{IPV4}|{})(?::0*?(|0|[1-9][0-9]{{0,4}}))?$",
                urllib3_ipv6_address_with_zone_body()
            )
        }),
    }
}

fn known_module_value(module: &str, name: &str) -> Option<KnownValue> {
    const HEADER_NAME_TEXT: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(r"^[^:\s][^:\r\n]*\Z"),
        flags: 32,
    };
    const HEADER_VALUE_TEXT: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(r"^\S[^\r\n]*\Z|^\Z"),
        flags: 32,
    };
    const HEADER_NAME_BYTES: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Bytes(br"^[^:\s][^:\r\n]*\Z"),
        flags: 0,
    };
    const HEADER_VALUE_BYTES: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Bytes(br"^\S[^\r\n]*\Z|^\Z"),
        flags: 0,
    };
    const IPV4: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(r"^(?:[0-9]{1,3}\.){3}[0-9]{1,3}$"),
        flags: 32,
    };
    const SCHEME: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(r"^(?:[a-zA-Z][a-zA-Z0-9+-]*:|/)"),
        flags: 32,
    };
    const PERCENT: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(r"%[a-fA-F0-9]{2}"),
        flags: 32,
    };
    const URI: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Text(
            r"^(?:([a-zA-Z][a-zA-Z0-9+.-]*):)?(?://([^\\/?#]*))?([^?#]*)(?:\?([^#]*))?(?:#(.*))?$",
        ),
        flags: 48,
    };
    const IPV6_ADDRESS_WITH_ZONE: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Generated(GeneratedRegex::Ipv6AddressWithZone),
        flags: 32,
    };
    const ZONE_ID: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Generated(GeneratedRegex::ZoneId),
        flags: 32,
    };
    const HOST_PORT: KnownRegex = KnownRegex {
        pattern: KnownRegexPattern::Generated(GeneratedRegex::HostPort),
        flags: 48,
    };
    const UNRESERVED: &str = "-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
    const USERINFO: &str =
        "!$&'()*+,-.0123456789:;=ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
    const PATH: &str =
        "!$&'()*+,-./0123456789:;=@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
    const QUERY_OR_FRAGMENT: &str =
        "!$&'()*+,-./0123456789:;=?@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
    const NATIVE_NETLOC_SCHEMES: &[&str] = &["http", "https"];

    match (module, name) {
        ("requests.models", "basestring") => Some(KnownValue::IntrinsicPair(
            IntrinsicBuiltin::Str,
            IntrinsicBuiltin::Bytes,
        )),
        ("requests._internal_utils", "builtin_str") | ("requests.utils", "str") => {
            Some(KnownValue::Intrinsic(IntrinsicBuiltin::Str))
        }
        ("requests.utils", "bytes") => Some(KnownValue::Intrinsic(IntrinsicBuiltin::Bytes)),
        ("requests.utils", "_HEADER_VALIDATORS_STR") => {
            Some(KnownValue::RegexPair(HEADER_NAME_TEXT, HEADER_VALUE_TEXT))
        }
        ("requests.utils", "_HEADER_VALIDATORS_BYTE") => {
            Some(KnownValue::RegexPair(HEADER_NAME_BYTES, HEADER_VALUE_BYTES))
        }
        ("requests.utils", "UNRESERVED_SET") => Some(KnownValue::TextFrozenSet(UNRESERVED)),
        ("urllib3.util.url", "_PERCENT_RE") => Some(KnownValue::Regex(PERCENT)),
        ("urllib3.util.url", "_IPV4_RE") => Some(KnownValue::Regex(IPV4)),
        ("urllib3.util.url", "_SCHEME_RE") => Some(KnownValue::Regex(SCHEME)),
        ("urllib3.util.url", "_URI_RE") => Some(KnownValue::Regex(URI)),
        ("urllib3.util.url", "_IPV6_ADDRZ_RE") => Some(KnownValue::Regex(IPV6_ADDRESS_WITH_ZONE)),
        ("urllib3.util.url", "_ZONE_ID_RE") => Some(KnownValue::Regex(ZONE_ID)),
        ("urllib3.util.url", "_HOST_PORT_RE") => Some(KnownValue::Regex(HOST_PORT)),
        ("urllib3.util.url", "_UNRESERVED_CHARS") => Some(KnownValue::TextSet(UNRESERVED)),
        ("urllib3.util.url", "_USERINFO_CHARS") => Some(KnownValue::TextSet(USERINFO)),
        ("urllib3.util.url", "_PATH_CHARS") => Some(KnownValue::TextSet(PATH)),
        ("urllib3.util.url", "_QUERY_CHARS" | "_FRAGMENT_CHARS") => {
            Some(KnownValue::TextSet(QUERY_OR_FRAGMENT))
        }
        ("urllib3.util.url", "_NORMALIZABLE_SCHEMES") => {
            Some(KnownValue::TextPairAndNone("http", "https"))
        }
        ("urllib3.util.url", "Url") => Some(KnownValue::UrlType),
        ("urllib.parse", "_ALWAYS_SAFE_BYTES") => Some(KnownValue::Bytes(
            b"-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~",
        )),
        ("urllib.parse", "_byte_quoter_factory") => Some(KnownValue::ByteQuoterFactory),
        ("urllib.parse", "_safe_quoters") => Some(KnownValue::EmptyDict),
        ("urllib.parse", "Quoter") => Some(KnownValue::QuoterType {
            name: "Quoter",
            default_dict_base: true,
        }),
        ("urllib.parse", "uses_netloc") => {
            Some(KnownValue::TextListContaining(NATIVE_NETLOC_SCHEMES))
        }
        _ => None,
    }
}

fn known_python_function(module: &str, name: &str) -> Option<(&'static str, &'static str)> {
    match (module, name) {
        ("requests.models", "to_key_val_list") => Some(("requests.utils", "to_key_val_list")),
        ("requests.models", "urlencode") => Some(("urllib.parse", "urlencode")),
        ("requests.utils", "_validate_header_part") => {
            Some(("requests.utils", "_validate_header_part"))
        }
        ("requests.utils", "quote") => Some(("urllib.parse", "quote")),
        ("requests.utils", "unquote_unreserved") => Some(("requests.utils", "unquote_unreserved")),
        ("urllib.parse", "_coerce_args") => Some(("urllib.parse", "_coerce_args")),
        ("urllib.parse", "_decode_args") => Some(("urllib.parse", "_decode_args")),
        ("urllib.parse", "_encode_result") => Some(("urllib.parse", "_encode_result")),
        ("urllib.parse", "_noop") => Some(("urllib.parse", "_noop")),
        ("urllib.parse", "_urlunsplit") => Some(("urllib.parse", "_urlunsplit")),
        ("urllib.parse", "urlunsplit") => Some(("urllib.parse", "urlunsplit")),
        ("urllib.parse", "quote") => Some(("urllib.parse", "quote")),
        ("urllib.parse", "quote_from_bytes") => Some(("urllib.parse", "quote_from_bytes")),
        ("urllib3.util.url", "_encode_invalid_chars") => {
            Some(("urllib3.util.url", "_encode_invalid_chars"))
        }
        ("urllib3.util.url", "_idna_encode") => Some(("urllib3.util.url", "_idna_encode")),
        ("urllib3.util.url", "_normalize_host") => Some(("urllib3.util.url", "_normalize_host")),
        ("urllib3.util.url", "_remove_path_dot_segments") => {
            Some(("urllib3.util.url", "_remove_path_dot_segments"))
        }
        ("urllib3.util.url", "to_str") => Some(("urllib3.util.util", "to_str")),
        _ => None,
    }
}

fn intrinsic_descriptor_type_is(
    py: Python<'_>,
    descriptor: &Bound<'_, PyAny>,
    expected: IntrinsicDescriptorType,
) -> PyResult<bool> {
    let name = match expected {
        IntrinsicDescriptorType::Method => "MethodDescriptorType",
        IntrinsicDescriptorType::Member => "MemberDescriptorType",
    };
    let expected = PyModule::import(py, "types")?.getattr(name)?;
    Ok(descriptor.get_type().as_any().is(&expected))
}

fn exact_type_identity_is(current: &Bound<'_, PyAny>, module: &str, name: &str) -> PyResult<bool> {
    if !current.is_exact_instance_of::<PyType>() {
        return Ok(false);
    }
    let current = current.cast::<PyType>()?;
    let current_module = current.getattr("__module__")?;
    let current_name = current.getattr("__name__")?;
    Ok(current_module.is_exact_instance_of::<PyString>()
        && current_name.is_exact_instance_of::<PyString>()
        && current_module.cast::<PyString>()?.to_str()? == module
        && current_name.cast::<PyString>()?.to_str()? == name)
}

fn exact_source_function_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    module: &str,
    qualname: &str,
    code: &Py<PyCode>,
    globals: &Bound<'_, PyDict>,
    single_ellipsis_default: bool,
) -> PyResult<bool> {
    if !current.is_exact_instance_of::<PyFunction>() {
        return Ok(false);
    }
    let current = current.cast::<PyFunction>()?;
    if current.getattr("__module__")?.extract::<String>()? != module
        || current.getattr("__qualname__")?.extract::<String>()? != qualname
        || !function_code(py, current)?.eq(code.bind(py))?
        || !function_globals(py, current)?.is(globals)
        || function_kwdefaults(py, current).is_some()
        || function_closure(py, current).is_some()
    {
        return Ok(false);
    }
    let defaults_match = match function_defaults(py, current) {
        None => !single_ellipsis_default,
        Some(defaults) if single_ellipsis_default => {
            defaults.is_exact_instance_of::<PyTuple>()
                && defaults.cast::<PyTuple>()?.len() == 1
                && defaults.cast::<PyTuple>()?.get_item(0)?.is(py.Ellipsis())
        }
        Some(_) => false,
    };
    Ok(defaults_match)
}

fn static_type_is(current: &Bound<'_, PyType>, module: &str, name: &str) -> PyResult<bool> {
    const IMMUTABLE_TYPE: u64 = 1 << 8;
    let flags = current.getattr("__flags__")?.extract::<u64>()?;
    Ok(flags & IMMUTABLE_TYPE != 0
        && current.module()?.to_str()? == module
        && current.qualname()?.to_str()? == name
        && current.mro().len() == 2
        && current.mro().get_item(0)?.is(current)
        && current
            .mro()
            .get_item(1)?
            .is(current.py().get_type::<PyAny>()))
}

fn always_safe_frozenset_is(current: &Bound<'_, PyAny>) -> PyResult<bool> {
    const ALWAYS_SAFE: &[u8] =
        b"-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
    if !current.is_exact_instance_of::<PyFrozenSet>() {
        return Ok(false);
    }
    let current = current.cast::<PyFrozenSet>()?;
    if current.len() != ALWAYS_SAFE.len() {
        return Ok(false);
    }
    for value in current.iter() {
        let Ok(value) = value.extract::<u8>() else {
            return Ok(false);
        };
        if !ALWAYS_SAFE.contains(&value) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn known_quoter_type_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    globals: &Bound<'_, PyDict>,
    name: &str,
    default_dict_base: bool,
    init_code: &Py<PyCode>,
    missing_code: &Py<PyCode>,
) -> PyResult<bool> {
    if !exact_type_identity_is(current, "urllib.parse", name)? {
        return Ok(false);
    }
    let class = current.cast::<PyType>()?;
    let mro = class.mro();
    let mro_matches = if default_dict_base {
        if mro.len() != 4
            || !mro.get_item(0)?.is(class)
            || !mro.get_item(2)?.is(py.get_type::<PyDict>())
            || !mro.get_item(3)?.is(py.get_type::<PyAny>())
        {
            false
        } else {
            let base = mro.get_item(1)?.cast_into::<PyType>()?;
            let flags = base.getattr("__flags__")?.extract::<u64>()?;
            const IMMUTABLE_TYPE: u64 = 1 << 8;
            flags & IMMUTABLE_TYPE != 0
                && base.module()?.to_str()? == "collections"
                && base.qualname()?.to_str()? == "defaultdict"
                && base.mro().len() == 3
                && base.mro().get_item(0)?.is(&base)
                && base.mro().get_item(1)?.is(py.get_type::<PyDict>())
                && base.mro().get_item(2)?.is(py.get_type::<PyAny>())
        }
    } else {
        mro.len() == 3
            && mro.get_item(0)?.is(class)
            && mro.get_item(1)?.is(py.get_type::<PyDict>())
            && mro.get_item(2)?.is(py.get_type::<PyAny>())
    };
    if !mro_matches {
        return Ok(false);
    }
    let namespace = class.getattr("__dict__")?;
    if ["__new__", "__getitem__", "__setitem__", "__getattribute__"]
        .iter()
        .any(|name| namespace.contains(*name).unwrap_or(true))
    {
        return Ok(false);
    }
    if !exact_source_function_is(
        py,
        &namespace.get_item("__init__")?,
        "urllib.parse",
        &format!("{name}.__init__"),
        init_code,
        globals,
        false,
    )? || !exact_source_function_is(
        py,
        &namespace.get_item("__missing__")?,
        "urllib.parse",
        &format!("{name}.__missing__"),
        missing_code,
        globals,
        false,
    )? {
        return Ok(false);
    }
    always_safe_frozenset_is(&globals.get_item("_ALWAYS_SAFE")?.ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err("urllib.parse._ALWAYS_SAFE is missing")
    })?)
}

fn known_byte_quoter_factory_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    factory_code: &Py<PyCode>,
    quoter_init_code: &Py<PyCode>,
    quoter_missing_code: &Py<PyCode>,
    canonical_globals: &Py<PyDict>,
) -> PyResult<bool> {
    let wrapper_type = current.get_type();
    if !static_type_is(&wrapper_type, "functools", "_lru_cache_wrapper")? {
        return Ok(false);
    }
    let wrapper_namespace = wrapper_type.getattr("__dict__")?;
    let cache_info_descriptor = wrapper_namespace.get_item("cache_info")?;
    let cache_info = cache_info_descriptor
        .call_method1("__get__", (current, &wrapper_type))?
        .call0()?;
    if !cache_info.is_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let cache_info = cache_info.cast::<PyTuple>()?;
    if cache_info.len() != 4
        || cache_info.get_item(0)?.extract::<usize>()? != 0
        || cache_info.get_item(1)?.extract::<usize>()? != 0
        || cache_info.get_item(2)?.extract::<usize>()? != 128
        || cache_info.get_item(3)?.extract::<usize>()? != 0
    {
        return Ok(false);
    }

    let wrapper_dict = current.getattr("__dict__")?.cast_into::<PyDict>()?;
    let wrapped = wrapper_dict
        .get_item("__wrapped__")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("missing __wrapped__"))?;
    let globals = canonical_globals.bind(py);
    if !exact_source_function_is(
        py,
        &wrapped,
        "urllib.parse",
        "_byte_quoter_factory",
        factory_code,
        globals,
        false,
    )? {
        return Ok(false);
    }
    let quoter = globals
        .get_item("_Quoter")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("urllib.parse._Quoter missing"))?;
    known_quoter_type_is(
        py,
        &quoter,
        globals,
        "_Quoter",
        false,
        quoter_init_code,
        quoter_missing_code,
    )
}

fn known_tuple_getter_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    index: usize,
) -> PyResult<bool> {
    let descriptor_type = current.get_type();
    let flags = descriptor_type.getattr("__flags__")?.extract::<u64>()?;
    const IMMUTABLE_TYPE: u64 = 1 << 8;
    let module = descriptor_type.module()?;
    if flags & IMMUTABLE_TYPE == 0
        || (module.to_str()? != "collections" && module.to_str()? != "_collections")
        || descriptor_type.qualname()?.to_str()? != "_tuplegetter"
        || descriptor_type.mro().len() != 2
        || !descriptor_type.mro().get_item(0)?.is(&descriptor_type)
        || !descriptor_type
            .mro()
            .get_item(1)?
            .is(py.get_type::<PyAny>())
    {
        return Ok(false);
    }
    let reduced = current.call_method0("__reduce__")?;
    if !reduced.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let reduced = reduced.cast::<PyTuple>()?;
    if reduced.len() != 2 || !reduced.get_item(0)?.is(&descriptor_type) {
        return Ok(false);
    }
    let arguments = reduced.get_item(1)?;
    if !arguments.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let arguments = arguments.cast::<PyTuple>()?;
    Ok(arguments.len() == 2
        && arguments.get_item(0)?.extract::<usize>()? == index
        && arguments.get_item(1)?.is_exact_instance_of::<PyString>()
        && arguments.get_item(1)?.cast::<PyString>()?.to_str()?
            == format!("Alias for field number {index}"))
}

fn known_namedtuple_url_base_is(
    py: Python<'_>,
    base: &Bound<'_, PyType>,
    fields: &[&str],
) -> PyResult<bool> {
    let namespace = base.getattr("__dict__")?;
    if ["__getattribute__", "__getitem__", "__iter__", "__len__"]
        .iter()
        .any(|name| namespace.contains(*name).unwrap_or(true))
    {
        return Ok(false);
    }
    for (index, field) in fields.iter().enumerate() {
        if !known_tuple_getter_is(py, &namespace.get_item(*field)?, index)? {
            return Ok(false);
        }
    }

    let new_descriptor = namespace.get_item("__new__")?;
    let staticmethod_anchor = py
        .get_type::<PyString>()
        .getattr("__dict__")?
        .get_item("maketrans")?;
    if !new_descriptor.get_type().is(staticmethod_anchor.get_type()) {
        return Ok(false);
    }
    let new_function = new_descriptor.getattr("__func__")?;
    if !new_function.is_exact_instance_of::<PyFunction>() {
        return Ok(false);
    }
    let new_function = new_function.cast::<PyFunction>()?;
    if new_function.getattr("__module__")?.extract::<String>()? != "namedtuple_Url"
        || new_function.getattr("__qualname__")?.extract::<String>()? != "Url.__new__"
        || function_kwdefaults(py, new_function).is_some()
        || function_closure(py, new_function).is_some()
    {
        return Ok(false);
    }
    let Some(defaults) = function_defaults(py, new_function) else {
        return Ok(false);
    };
    if !defaults.is_exact_instance_of::<PyTuple>() || !defaults.cast::<PyTuple>()?.is_empty() {
        return Ok(false);
    }
    let code = function_code(py, new_function)?;
    let names = code.getattr("co_names")?;
    let constants = code.getattr("co_consts")?;
    if !names.is_exact_instance_of::<PyTuple>()
        || names.cast::<PyTuple>()?.len() != 1
        || names.cast::<PyTuple>()?.get_item(0)?.extract::<String>()? != "_tuple_new"
        || !constants.is_exact_instance_of::<PyTuple>()
        || constants.cast::<PyTuple>()?.len() != 1
        || !constants.cast::<PyTuple>()?.get_item(0)?.is_none()
        || code.getattr("co_argcount")?.extract::<usize>()? != fields.len() + 1
        || code.getattr("co_kwonlyargcount")?.extract::<usize>()? != 0
        || !code
            .getattr("co_freevars")?
            .cast_into::<PyTuple>()?
            .is_empty()
    {
        return Ok(false);
    }

    let globals = function_globals(py, new_function)?;
    if globals.len() != 3 {
        return Ok(false);
    }
    let tuple_new = globals
        .get_item("_tuple_new")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("missing _tuple_new"))?;
    let Ok(tuple_new) = tuple_new.cast::<PyCFunction>() else {
        return Ok(false);
    };
    let generated_builtins = globals
        .get_item("__builtins__")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("missing __builtins__"))?;
    let generated_name = globals
        .get_item("__name__")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("missing __name__"))?;
    Ok(
        tuple_new.getattr("__name__")?.extract::<String>()? == "__new__"
            && tuple_new.getattr("__module__")?.is_none()
            && c_function_self(py, tuple_new)
                .is_some_and(|owner| owner.is(py.get_type::<PyTuple>()))
            && generated_builtins.is_exact_instance_of::<PyDict>()
            && generated_builtins.cast::<PyDict>()?.is_empty()
            && generated_name.is_exact_instance_of::<PyString>()
            && generated_name.cast::<PyString>()?.to_str()? == "namedtuple_Url",
    )
}

fn known_url_type_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    expected_code: &Py<PyCode>,
    expected_globals: &Py<PyDict>,
    expected_builtins: &Py<PyDict>,
) -> PyResult<bool> {
    if !exact_type_identity_is(current, "urllib3.util.url", "Url")? {
        return Ok(false);
    }
    let class = current.cast::<PyType>()?;
    let mro = class.mro();
    if mro.len() != 4
        || !mro.get_item(0)?.is(class)
        || !mro.get_item(2)?.is(py.get_type::<PyTuple>())
        || !mro.get_item(3)?.is(py.get_type::<PyAny>())
    {
        return Ok(false);
    }

    let base = mro.get_item(1)?;
    if base.is(class) || !exact_type_identity_is(&base, "urllib3.util.url", "Url")? {
        return Ok(false);
    }
    let base = base.cast::<PyType>()?;
    let fields = base.getattr("_fields")?;
    if !fields.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let fields = fields.cast::<PyTuple>()?;
    const EXPECTED_FIELDS: [&str; 7] = [
        "scheme", "auth", "host", "port", "path", "query", "fragment",
    ];
    if fields.len() != EXPECTED_FIELDS.len() {
        return Ok(false);
    }
    for (field, expected) in fields.iter().zip(EXPECTED_FIELDS) {
        if !field.is_exact_instance_of::<PyString>()
            || field.cast::<PyString>()?.to_str()? != expected
        {
            return Ok(false);
        }
    }
    if !known_namedtuple_url_base_is(py, base, &EXPECTED_FIELDS)? {
        return Ok(false);
    }

    let namespace = class.getattr("__dict__")?;
    if EXPECTED_FIELDS
        .iter()
        .any(|field| namespace.contains(*field).unwrap_or(true))
    {
        return Ok(false);
    }
    let new_descriptor = namespace.get_item("__new__")?;
    let string_namespace = py.get_type::<PyString>().getattr("__dict__")?;
    let staticmethod_anchor = string_namespace.get_item("maketrans")?;
    if !new_descriptor.get_type().is(staticmethod_anchor.get_type()) {
        return Ok(false);
    }
    let new_function = new_descriptor.getattr("__func__")?;
    if !new_function.is_exact_instance_of::<PyFunction>() {
        return Ok(false);
    }
    let new_function = new_function.cast::<PyFunction>()?;
    if new_function.getattr("__module__")?.extract::<String>()? != "urllib3.util.url"
        || new_function.getattr("__qualname__")?.extract::<String>()? != "Url.__new__"
        || !function_code(py, new_function)?.eq(expected_code.bind(py))?
        || !function_globals(py, new_function)?.is(expected_globals.bind(py))
        || !new_function
            .getattr("__builtins__")?
            .cast_into::<PyDict>()?
            .is(expected_builtins.bind(py))
        || function_kwdefaults(py, new_function).is_some()
        || expected_globals.bind(py).contains("super")?
    {
        return Ok(false);
    }
    let Some(super_type) = expected_builtins.bind(py).get_item("super")? else {
        return Ok(false);
    };
    if !super_type.is(py.get_type::<pyo3::types::PySuper>()) {
        return Ok(false);
    }
    let Some(defaults) = function_defaults(py, new_function) else {
        return Ok(false);
    };
    if !defaults.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let defaults = defaults.cast::<PyTuple>()?;
    if defaults.len() != EXPECTED_FIELDS.len() || defaults.iter().any(|value| !value.is_none()) {
        return Ok(false);
    }
    let Some(closure) = function_closure(py, new_function) else {
        return Ok(false);
    };
    if !closure.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let closure = closure.cast::<PyTuple>()?;
    if closure.len() != 1 || !closure.get_item(0)?.getattr("cell_contents")?.is(class) {
        return Ok(false);
    }

    let tuple_getattribute =
        required_raw_type_entry(&py.get_type::<PyTuple>(), "__getattribute__")?;
    Ok(raw_type_entry(class, "__getattribute__")?
        .is_some_and(|current| current.is(&tuple_getattribute)))
}

fn known_regex_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    expected: KnownRegex,
) -> PyResult<bool> {
    let pattern_type = current.get_type();
    if !pattern_type.get_type().is(py.get_type::<PyType>()) {
        return Ok(false);
    }
    if pattern_type.name()?.to_str()? != "Pattern"
        || !pattern_type
            .getattr("__module__")?
            .is_exact_instance_of::<PyString>()
        || pattern_type
            .getattr("__module__")?
            .cast_into::<PyString>()?
            .to_str()?
            != "re"
    {
        return Ok(false);
    }

    let namespace = pattern_type.getattr("__dict__")?;
    for (name, descriptor_type) in [
        ("match", IntrinsicDescriptorType::Method),
        ("search", IntrinsicDescriptorType::Method),
        ("pattern", IntrinsicDescriptorType::Member),
        ("flags", IntrinsicDescriptorType::Member),
    ] {
        let descriptor = namespace.get_item(name)?;
        if !intrinsic_descriptor_type_is(py, &descriptor, descriptor_type)?
            || !descriptor.getattr("__objclass__")?.is(&pattern_type)
            || descriptor.getattr("__name__")?.extract::<String>()? != name
        {
            return Ok(false);
        }
    }

    let pattern = current.getattr("pattern")?;
    let pattern_matches = match expected.pattern {
        KnownRegexPattern::Text(expected) => {
            pattern.is_exact_instance_of::<PyString>()
                && pattern
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == expected))
        }
        KnownRegexPattern::Bytes(expected) => {
            pattern.is_exact_instance_of::<PyBytes>()
                && pattern
                    .cast::<PyBytes>()
                    .is_ok_and(|value| value.as_bytes() == expected)
        }
        KnownRegexPattern::Generated(expected) => {
            let expected = generated_regex_pattern(expected);
            pattern.is_exact_instance_of::<PyString>()
                && pattern
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == expected))
        }
    };
    Ok(pattern_matches && current.getattr("flags")?.extract::<i64>()? == expected.flags)
}

fn known_single_character_is(value: &Bound<'_, PyAny>, expected: &str) -> PyResult<bool> {
    Ok(value.is_exact_instance_of::<PyString>()
        && value
            .cast::<PyString>()?
            .to_str()
            .is_ok_and(|value| value.len() == 1 && expected.contains(value)))
}

fn conservative_structural_proof(result: PyResult<bool>) -> PyResult<bool> {
    Ok(result.unwrap_or(false))
}

fn known_value_is(
    py: Python<'_>,
    builtins: &Bound<'_, PyModule>,
    current: &Bound<'_, PyAny>,
    expected: KnownValue,
    expected_code: Option<&KnownCode>,
) -> PyResult<bool> {
    match expected {
        KnownValue::Intrinsic(expected) => intrinsic_builtin_is(py, builtins, current, expected),
        KnownValue::IntrinsicPair(first, second) => {
            if !current.is_exact_instance_of::<PyTuple>() {
                return Ok(false);
            }
            let values = current.cast::<PyTuple>()?;
            Ok(values.len() == 2
                && intrinsic_builtin_is(py, builtins, &values.get_item(0)?, first)?
                && intrinsic_builtin_is(py, builtins, &values.get_item(1)?, second)?)
        }
        KnownValue::TextPairAndNone(first, second) => {
            if !current.is_exact_instance_of::<PyTuple>() {
                return Ok(false);
            }
            let values = current.cast::<PyTuple>()?;
            if values.len() != 3 {
                return Ok(false);
            }
            let first_value = values.get_item(0)?;
            let second_value = values.get_item(1)?;
            Ok(first_value.is_exact_instance_of::<PyString>()
                && first_value
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == first))
                && second_value.is_exact_instance_of::<PyString>()
                && second_value
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == second))
                && values.get_item(2)?.is_none())
        }
        KnownValue::TextSet(expected) => {
            if !current.is_exact_instance_of::<PySet>() {
                return Ok(false);
            }
            let values = current.cast::<PySet>()?;
            if values.len() != expected.chars().count() {
                return Ok(false);
            }
            for value in values.iter() {
                if !known_single_character_is(&value, expected)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        KnownValue::TextFrozenSet(expected) => {
            if !current.is_exact_instance_of::<PyFrozenSet>() {
                return Ok(false);
            }
            let values = current.cast::<PyFrozenSet>()?;
            if values.len() != expected.chars().count() {
                return Ok(false);
            }
            for value in values.iter() {
                if !known_single_character_is(&value, expected)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        KnownValue::TextListContaining(required) => {
            if !current.is_exact_instance_of::<PyList>() {
                return Ok(false);
            }
            let values = current.cast::<PyList>()?;
            let mut found = vec![false; required.len()];
            for value in values.iter() {
                if !value.is_exact_instance_of::<PyString>() {
                    return Ok(false);
                }
                let value = value.cast::<PyString>()?.to_str()?;
                for (index, required) in required.iter().enumerate() {
                    found[index] |= value == *required;
                }
            }
            Ok(found.into_iter().all(|found| found))
        }
        KnownValue::Bytes(expected) => Ok(current.is_exact_instance_of::<PyBytes>()
            && current.cast::<PyBytes>()?.as_bytes() == expected),
        KnownValue::EmptyDict => {
            Ok(current.is_exact_instance_of::<PyDict>() && current.cast::<PyDict>()?.is_empty())
        }
        KnownValue::UrlType => {
            let Some(KnownCode::Url {
                new,
                globals,
                builtins,
            }) = expected_code
            else {
                return Ok(false);
            };
            conservative_structural_proof(known_url_type_is(py, current, new, globals, builtins))
        }
        KnownValue::ByteQuoterFactory => {
            let Some(KnownCode::ByteQuoterFactory {
                factory,
                quoter_init,
                quoter_missing,
                globals,
            }) = expected_code
            else {
                return Ok(false);
            };
            conservative_structural_proof(known_byte_quoter_factory_is(
                py,
                current,
                factory,
                quoter_init,
                quoter_missing,
                globals,
            ))
        }
        KnownValue::QuoterType {
            name,
            default_dict_base,
        } => {
            let Some(KnownCode::QuoterType {
                init,
                missing,
                globals,
            }) = expected_code
            else {
                return Ok(false);
            };
            conservative_structural_proof(known_quoter_type_is(
                py,
                current,
                globals.bind(py),
                name,
                default_dict_base,
                init,
                missing,
            ))
        }
        KnownValue::Regex(expected) => known_regex_is(py, current, expected),
        KnownValue::RegexPair(first, second) => {
            if !current.is_exact_instance_of::<PyTuple>() {
                return Ok(false);
            }
            let values = current.cast::<PyTuple>()?;
            Ok(values.len() == 2
                && known_regex_is(py, &values.get_item(0)?, first)?
                && known_regex_is(py, &values.get_item(1)?, second)?)
        }
    }
}

fn raw_type_entry<'py>(
    class: &Bound<'py, PyType>,
    name: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    for owner in class.mro().iter() {
        let namespace = owner.getattr("__dict__")?;
        if namespace.contains(name)? {
            return Ok(Some(namespace.get_item(name)?));
        }
    }
    Ok(None)
}

fn required_raw_type_entry<'py>(
    class: &Bound<'py, PyType>,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    raw_type_entry(class, name)?.ok_or_else(|| {
        pyo3::exceptions::PyAttributeError::new_err(format!("type has no raw {name} descriptor"))
    })
}

fn required_module_entry<'py>(
    module: &Bound<'py, PyModule>,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    module.dict().get_item(name)?.ok_or_else(|| {
        pyo3::exceptions::PyAttributeError::new_err(format!("module has no raw {name} entry"))
    })
}

fn direct_nested_code<'py>(
    scope: &Bound<'py, PyCode>,
    name: &str,
) -> PyResult<Option<Bound<'py, PyCode>>> {
    let mut found = None;
    for constant in scope.getattr("co_consts")?.cast_into::<PyTuple>()?.iter() {
        let Ok(nested) = constant.cast_into::<PyCode>() else {
            continue;
        };
        if nested.getattr("co_name")?.extract::<String>()? == name {
            found = Some(nested);
        }
    }
    Ok(found)
}

fn find_code_by_scope<'py>(
    root: &Bound<'py, PyCode>,
    qualname: &str,
) -> PyResult<Option<Bound<'py, PyCode>>> {
    let mut scope = root.clone();
    for component in qualname.split('.').filter(|part| *part != "<locals>") {
        let Some(nested) = direct_nested_code(&scope, component)? else {
            return Ok(None);
        };
        scope = nested;
    }
    Ok(Some(scope))
}

fn canonical_module_code_from<'py>(
    module: &Bound<'py, PyModule>,
    module_name: &str,
) -> PyResult<Bound<'py, PyCode>> {
    let spec = required_module_entry(module, "__spec__")?;
    let loader = spec.getattr("loader")?;
    Ok(loader
        .call_method1("get_code", (module_name,))?
        .cast_into::<PyCode>()?)
}

fn canonical_module_code<'py>(py: Python<'py>, module_name: &str) -> PyResult<Bound<'py, PyCode>> {
    let module = PyModule::import(py, module_name)?;
    canonical_module_code_from(&module, module_name)
}

fn canonical_code_from_module<'py>(
    module: &Bound<'py, PyModule>,
    module_name: &str,
    qualname: &str,
) -> PyResult<Bound<'py, PyCode>> {
    let root = canonical_module_code_from(module, module_name)?;
    find_code_by_scope(&root, qualname)?.ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "canonical code not found for {module_name}.{qualname}"
        ))
    })
}

pub(crate) fn canonical_code<'py>(
    py: Python<'py>,
    module_name: &str,
    qualname: &str,
) -> PyResult<Bound<'py, PyCode>> {
    let module = PyModule::import(py, module_name)?;
    canonical_code_from_module(&module, module_name, qualname)
}

fn build_canonical_function(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    module_name: &str,
    qualname: &str,
    policy: DefaultPolicy,
    include_globals: bool,
    ignored_globals: &[&str],
) -> PyResult<CanonicalFunction> {
    build_canonical_function_inner(
        py,
        current,
        module_name,
        qualname,
        policy,
        include_globals,
        ignored_globals,
        &HashSet::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_canonical_function_inner(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    module_name: &str,
    qualname: &str,
    policy: DefaultPolicy,
    include_globals: bool,
    ignored_globals: &[&str],
    ancestors: &HashSet<(String, String)>,
) -> PyResult<CanonicalFunction> {
    let mut descendants = ancestors.clone();
    descendants.insert((module_name.to_owned(), qualname.to_owned()));
    let module = PyModule::import(py, module_name)?;
    let builtins_module = PyModule::import(py, "builtins")?;
    let code = canonical_code(py, module_name, qualname)?;
    let builtins = required_module_entry(&module, "__builtins__")?.cast_into::<PyDict>()?;
    let module_globals = canonical_module_globals(py, module_name)?;
    let defaults = match policy {
        DefaultPolicy::None => CanonicalDefaults::None,
        DefaultPolicy::Captured => {
            let function = current.cast::<PyFunction>()?;
            CanonicalDefaults::Captured {
                defaults: function_defaults(py, function).map(Bound::unbind),
                kwdefaults: function_kwdefaults(py, function).map(Bound::unbind),
            }
        }
        DefaultPolicy::ToNativeString => CanonicalDefaults::ToNativeString,
        DefaultPolicy::SingleNone => CanonicalDefaults::SingleNone,
        DefaultPolicy::SingleEmptyTuple => CanonicalDefaults::SingleEmptyTuple,
    };
    let dependencies = if include_globals {
        build_canonical_globals(
            py,
            &module,
            &builtins,
            &code,
            &module_globals,
            ignored_globals,
            &descendants,
        )?
    } else {
        Vec::new()
    };

    Ok(CanonicalFunction {
        code: code.unbind(),
        globals: module.dict().unbind(),
        builtins_module: builtins_module.unbind(),
        builtins: builtins.unbind(),
        defaults,
        dependencies,
    })
}

fn canonical_module_globals(py: Python<'_>, module_name: &str) -> PyResult<Vec<String>> {
    let code = canonical_module_code(py, module_name)?;
    let instructions = PyModule::import(py, "dis")?
        .getattr("get_instructions")?
        .call1((&code,))?;
    let mut names = Vec::new();
    for instruction in instructions.try_iter()? {
        let instruction = instruction?;
        let opname = instruction.getattr("opname")?.extract::<String>()?;
        if opname != "STORE_NAME" && opname != "STORE_GLOBAL" {
            continue;
        }
        let name = instruction.getattr("argval")?.extract::<String>()?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

fn build_known_code(
    current_module: &Bound<'_, PyModule>,
    builtins: &Bound<'_, PyDict>,
    known: KnownValue,
) -> PyResult<Option<KnownCode>> {
    match known {
        KnownValue::UrlType => Ok(Some(KnownCode::Url {
            new: canonical_code_from_module(current_module, "urllib3.util.url", "Url.__new__")?
                .unbind(),
            globals: current_module.dict().unbind(),
            builtins: builtins.clone().unbind(),
        })),
        KnownValue::ByteQuoterFactory => Ok(Some(KnownCode::ByteQuoterFactory {
            factory: canonical_code_from_module(
                current_module,
                "urllib.parse",
                "_byte_quoter_factory",
            )?
            .unbind(),
            quoter_init: canonical_code_from_module(
                current_module,
                "urllib.parse",
                "_Quoter.__init__",
            )?
            .unbind(),
            quoter_missing: canonical_code_from_module(
                current_module,
                "urllib.parse",
                "_Quoter.__missing__",
            )?
            .unbind(),
            globals: current_module.dict().unbind(),
        })),
        KnownValue::QuoterType { name, .. } => Ok(Some(KnownCode::QuoterType {
            init: canonical_code_from_module(
                current_module,
                "urllib.parse",
                &format!("{name}.__init__"),
            )?
            .unbind(),
            missing: canonical_code_from_module(
                current_module,
                "urllib.parse",
                &format!("{name}.__missing__"),
            )?
            .unbind(),
            globals: current_module.dict().unbind(),
        })),
        _ => Ok(None),
    }
}

fn build_canonical_globals(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    builtins: &Bound<'_, PyDict>,
    code: &Bound<'_, PyCode>,
    module_globals: &[String],
    ignored: &[&str],
    ancestors: &HashSet<(String, String)>,
) -> PyResult<Vec<CanonicalGlobal>> {
    let instructions = PyModule::import(py, "dis")?
        .getattr("get_instructions")?
        .call1((code,))?;
    let module_name = module.name()?.to_str()?.to_owned();
    let mut dependencies = Vec::new();
    for instruction in instructions.try_iter()? {
        let instruction = instruction?;
        if instruction.getattr("opname")?.extract::<String>()? != "LOAD_GLOBAL" {
            continue;
        }
        let name = instruction.getattr("argval")?.extract::<String>()?;
        if ignored.contains(&name.as_str())
            || dependencies
                .iter()
                .any(|dependency: &CanonicalGlobal| dependency.name == name)
        {
            continue;
        }
        let resolution = if module_globals.contains(&name) {
            if let Some(known) = known_module_value(&module_name, &name) {
                let code = build_known_code(module, builtins, known)?;
                CanonicalGlobalResolution::KnownModule { value: known, code }
            } else if let Some(value) = module.dict().get_item(&name)? {
                let Some((expected_module, expected_qualname)) =
                    known_python_function(&module_name, &name)
                else {
                    dependencies.push(CanonicalGlobal {
                        name,
                        resolution: CanonicalGlobalResolution::Unprovable,
                    });
                    continue;
                };
                if !value.is_exact_instance_of::<PyFunction>()
                    || value.getattr("__module__")?.extract::<String>()? != expected_module
                    || value.getattr("__qualname__")?.extract::<String>()? != expected_qualname
                {
                    CanonicalGlobalResolution::Unprovable
                } else {
                    match direct_function_trust(py, &value, ancestors) {
                        Ok(function) => CanonicalGlobalResolution::Module {
                            function,
                            expected: value.unbind(),
                        },
                        Err(_) => CanonicalGlobalResolution::Unprovable,
                    }
                }
            } else {
                CanonicalGlobalResolution::Unprovable
            }
        } else if let Some(intrinsic) = intrinsic_builtin_for_name(&name) {
            CanonicalGlobalResolution::IntrinsicBuiltin(intrinsic)
        } else if builtins.contains(&name)? {
            CanonicalGlobalResolution::Unprovable
        } else {
            CanonicalGlobalResolution::Missing
        };
        dependencies.push(CanonicalGlobal { name, resolution });
    }
    Ok(dependencies)
}

fn direct_function_trust(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    ancestors: &HashSet<(String, String)>,
) -> PyResult<Option<Box<CanonicalFunction>>> {
    if !value.is_exact_instance_of::<PyFunction>() {
        return Ok(None);
    }
    let module_name = value.getattr("__module__")?.extract::<String>()?;
    let qualname = value.getattr("__qualname__")?.extract::<String>()?;
    if ancestors.contains(&(module_name.clone(), qualname.clone())) {
        return Ok(None);
    }
    let ignored_globals: &[&str] = match (module_name.as_str(), qualname.as_str()) {
        ("requests.utils", "_validate_header_part") => &["InvalidHeader"],
        ("requests.utils", "unquote_unreserved") => &["InvalidURL"],
        ("urllib.parse", "quote_from_bytes") => &["math"],
        ("urllib3.util.url", "_idna_encode") => &["LocationParseError"],
        _ => &[],
    };
    Ok(Some(Box::new(build_canonical_function_inner(
        py,
        value,
        &module_name,
        &qualname,
        DefaultPolicy::Captured,
        true,
        ignored_globals,
        ancestors,
    )?)))
}

fn canonical_defaults_are_current(
    py: Python<'_>,
    function: &Bound<'_, PyFunction>,
    expected: &CanonicalDefaults,
) -> PyResult<bool> {
    let defaults = function_defaults(py, function);
    let kwdefaults = function_kwdefaults(py, function);
    match expected {
        CanonicalDefaults::None => Ok(defaults.is_none() && kwdefaults.is_none()),
        CanonicalDefaults::Captured {
            defaults: expected_defaults,
            kwdefaults: expected_kwdefaults,
        } => {
            let defaults_match = match (defaults.as_ref(), expected_defaults.as_ref()) {
                (None, None) => true,
                (Some(actual), Some(expected)) => actual.is(expected.bind(py)),
                _ => false,
            };
            let kwdefaults_match = match (kwdefaults.as_ref(), expected_kwdefaults.as_ref()) {
                (None, None) => true,
                (Some(actual), Some(expected)) => actual.is(expected.bind(py)),
                _ => false,
            };
            Ok(defaults_match && kwdefaults_match)
        }
        CanonicalDefaults::ToNativeString => {
            if kwdefaults.is_some() {
                return Ok(false);
            }
            let Some(defaults) = defaults else {
                return Ok(false);
            };
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            Ok(defaults.len() == 1
                && defaults
                    .get_item(0)?
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == "ascii")))
        }
        CanonicalDefaults::SingleNone => {
            if kwdefaults.is_some() {
                return Ok(false);
            }
            let Some(defaults) = defaults else {
                return Ok(false);
            };
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            Ok(defaults.len() == 1 && defaults.get_item(0)?.is_none())
        }
        CanonicalDefaults::SingleEmptyTuple => {
            if kwdefaults.is_some() {
                return Ok(false);
            }
            let Some(defaults) = defaults else {
                return Ok(false);
            };
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            Ok(defaults.len() == 1
                && defaults
                    .get_item(0)?
                    .cast::<PyTuple>()
                    .is_ok_and(PyTupleMethods::is_empty))
        }
    }
}

fn canonical_function_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    expected: &CanonicalFunction,
) -> PyResult<bool> {
    if !canonical_function_shape_is(py, current, expected)? {
        return Ok(false);
    }
    let Ok(function) = current.cast::<PyFunction>() else {
        return Ok(false);
    };
    let globals = function_globals(py, function)?;
    let builtins = expected.builtins.bind(py);
    for dependency in &expected.dependencies {
        let current_global = globals.get_item(&dependency.name)?;
        let (current, function) = match &dependency.resolution {
            CanonicalGlobalResolution::Module { expected, function } => {
                let Some(current) = current_global else {
                    return Ok(false);
                };
                if !current.is(expected.bind(py)) {
                    return Ok(false);
                }
                (current, function)
            }
            CanonicalGlobalResolution::KnownModule { value, code } => {
                let Some(current) = current_global else {
                    return Ok(false);
                };
                if !known_value_is(
                    py,
                    expected.builtins_module.bind(py),
                    &current,
                    *value,
                    code.as_ref(),
                )? {
                    return Ok(false);
                }
                continue;
            }
            CanonicalGlobalResolution::IntrinsicBuiltin(intrinsic) => {
                if current_global.is_some() {
                    return Ok(false);
                }
                let Some(current) = builtins.get_item(&dependency.name)? else {
                    return Ok(false);
                };
                if !intrinsic_builtin_is(
                    py,
                    expected.builtins_module.bind(py),
                    &current,
                    *intrinsic,
                )? {
                    return Ok(false);
                }
                continue;
            }
            CanonicalGlobalResolution::Missing => {
                if current_global.is_some() || builtins.contains(&dependency.name)? {
                    return Ok(false);
                }
                continue;
            }
            CanonicalGlobalResolution::Unprovable => return Ok(false),
        };
        if let Some(function) = function
            && !canonical_function_is(py, &current, function)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn canonical_function_shape_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    expected: &CanonicalFunction,
) -> PyResult<bool> {
    let Ok(function) = current.cast::<PyFunction>() else {
        return Ok(false);
    };
    let code = function_code(py, function)?;
    let globals = function_globals(py, function)?;
    let builtins = function.getattr("__builtins__")?.cast_into::<PyDict>()?;
    Ok(code.eq(expected.code.bind(py))?
        && globals.is(expected.globals.bind(py))
        && builtins.is(expected.builtins.bind(py))
        && canonical_defaults_are_current(py, function, &expected.defaults)?)
}

fn initialize_models_state(py: Python<'_>) -> PyResult<ModelsState> {
    let internal_utils = PyModule::import(py, "requests._internal_utils")?;
    let builtins = PyModule::import(py, "builtins")?;
    let models = PyModule::import(py, "requests.models")?;
    let structures = PyModule::import(py, "requests.structures")?;
    let utils = PyModule::import(py, "requests.utils")?;
    let method_type = PyModule::import(py, "types")?
        .getattr("MethodType")?
        .cast_into::<PyType>()?;
    let prepared_request =
        required_module_entry(&models, "PreparedRequest")?.cast_into::<PyType>()?;
    let case_insensitive_dict =
        required_module_entry(&models, "CaseInsensitiveDict")?.cast_into::<PyType>()?;
    let case_insensitive_dict_init = required_raw_type_entry(&case_insensitive_dict, "__init__")?;
    let case_insensitive_dict_new = required_raw_type_entry(&case_insensitive_dict, "__new__")?;
    let case_insensitive_dict_getattribute =
        required_raw_type_entry(&case_insensitive_dict, "__getattribute__")?;
    let case_insensitive_dict_setattr =
        required_raw_type_entry(&case_insensitive_dict, "__setattr__")?;
    let case_insensitive_dict_update = required_raw_type_entry(&case_insensitive_dict, "update")?;
    let case_insensitive_dict_setitem =
        required_raw_type_entry(&case_insensitive_dict, "__setitem__")?;
    let ordered_dict = required_module_entry(&structures, "OrderedDict")?;
    let check_header_validity = required_module_entry(&models, "check_header_validity")?;
    let validate_header_part = required_module_entry(&utils, "_validate_header_part")?;
    let header_validators_str = required_module_entry(&utils, "_HEADER_VALIDATORS_STR")?;
    let header_validators_byte = required_module_entry(&utils, "_HEADER_VALIDATORS_BYTE")?;
    let utils_str = py.get_type::<PyString>().into_any();
    let utils_bytes = py.get_type::<PyBytes>().into_any();
    let to_native_string = required_module_entry(&models, "to_native_string")?;
    let invalid_header = required_module_entry(&utils, "InvalidHeader")?;
    let invalid_url = required_module_entry(&models, "InvalidURL")?;
    let location_parse_error = required_module_entry(&models, "LocationParseError")?;
    let missing_schema = required_module_entry(&models, "MissingSchema")?;
    let parse_url = required_module_entry(&models, "parse_url")?;
    let requote_uri = required_module_entry(&models, "requote_uri")?;
    let unicode_is_ascii = required_module_entry(&models, "unicode_is_ascii")?;
    let urlunparse = required_module_entry(&models, "urlunparse")?;
    let request_encoding_mixin =
        required_module_entry(&models, "RequestEncodingMixin")?.cast_into::<PyType>()?;
    let encode_params_descriptor = request_encoding_mixin
        .getattr("__dict__")?
        .get_item("_encode_params")?;
    let encode_params_function = encode_params_descriptor.getattr("__func__")?;
    let builtin_str = py.get_type::<PyString>().into_any();
    let prepare_method = required_raw_type_entry(&prepared_request, "prepare_method")?;
    let prepare_headers = required_raw_type_entry(&prepared_request, "prepare_headers")?;
    let prepare_url = required_raw_type_entry(&prepared_request, "prepare_url")?;
    let object_type = py.get_type::<PyAny>();
    let object_getattribute = required_raw_type_entry(&object_type, "__getattribute__")?;
    let object_setattr = required_raw_type_entry(&object_type, "__setattr__")?;
    let prepare_method_trust = build_canonical_function(
        py,
        &prepare_method,
        "requests.models",
        "PreparedRequest.prepare_method",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    let prepare_headers_trust = build_canonical_function(
        py,
        &prepare_headers,
        "requests.models",
        "PreparedRequest.prepare_headers",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    let prepare_url_trust = build_canonical_function(
        py,
        &prepare_url,
        "requests.models",
        "PreparedRequest.prepare_url",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    let encode_params_trust = build_canonical_function(
        py,
        &encode_params_function,
        "requests.models",
        "RequestEncodingMixin._encode_params",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    let to_native_string_trust = build_canonical_function(
        py,
        &to_native_string,
        "requests._internal_utils",
        "to_native_string",
        DefaultPolicy::ToNativeString,
        true,
        &[],
    )?;
    let parse_url_trust = build_canonical_function(
        py,
        &parse_url,
        "urllib3.util.url",
        "parse_url",
        DefaultPolicy::None,
        true,
        &["LocationParseError", "ValueError", "AttributeError"],
    )?;
    let requote_uri_trust = build_canonical_function(
        py,
        &requote_uri,
        "requests.utils",
        "requote_uri",
        DefaultPolicy::None,
        true,
        &["InvalidURL"],
    )?;
    let unicode_is_ascii_trust = build_canonical_function(
        py,
        &unicode_is_ascii,
        "requests._internal_utils",
        "unicode_is_ascii",
        DefaultPolicy::None,
        true,
        &["UnicodeEncodeError"],
    )?;
    let urlunparse_trust = build_canonical_function(
        py,
        &urlunparse,
        "urllib.parse",
        "urlunparse",
        DefaultPolicy::None,
        true,
        &[],
    )?;
    let check_header_validity_trust = build_canonical_function(
        py,
        &check_header_validity,
        "requests.utils",
        "check_header_validity",
        DefaultPolicy::None,
        true,
        &[],
    )?;
    let validate_header_part_trust = build_canonical_function(
        py,
        &validate_header_part,
        "requests.utils",
        "_validate_header_part",
        DefaultPolicy::None,
        true,
        &["InvalidHeader", "type"],
    )?;
    let case_insensitive_dict_init_trust = build_canonical_function(
        py,
        &case_insensitive_dict_init,
        "requests.structures",
        "CaseInsensitiveDict.__init__",
        DefaultPolicy::SingleNone,
        true,
        &["OrderedDict"],
    )?;
    let case_insensitive_dict_update_trust = build_canonical_function(
        py,
        &case_insensitive_dict_update,
        "_collections_abc",
        "MutableMapping.update",
        DefaultPolicy::SingleEmptyTuple,
        true,
        &["Mapping"],
    )?;
    let case_insensitive_dict_setitem_trust = build_canonical_function(
        py,
        &case_insensitive_dict_setitem,
        "requests.structures",
        "CaseInsensitiveDict.__setitem__",
        DefaultPolicy::None,
        true,
        &[],
    )?;
    Ok(ModelsState {
        internal_utils: internal_utils.unbind(),
        builtins: builtins.unbind(),
        models: models.unbind(),
        structures: structures.unbind(),
        utils: utils.unbind(),
        prepared_request: prepared_request.unbind(),
        method_type: method_type.unbind(),
        case_insensitive_dict: case_insensitive_dict.unbind(),
        case_insensitive_dict_init: case_insensitive_dict_init.unbind(),
        case_insensitive_dict_new: case_insensitive_dict_new.unbind(),
        case_insensitive_dict_getattribute: case_insensitive_dict_getattribute.unbind(),
        case_insensitive_dict_setattr: case_insensitive_dict_setattr.unbind(),
        case_insensitive_dict_update: case_insensitive_dict_update.unbind(),
        case_insensitive_dict_setitem: case_insensitive_dict_setitem.unbind(),
        ordered_dict: ordered_dict.unbind(),
        check_header_validity: check_header_validity.unbind(),
        validate_header_part: validate_header_part.unbind(),
        header_validators_str: header_validators_str.unbind(),
        header_validators_byte: header_validators_byte.unbind(),
        utils_str: utils_str.unbind(),
        utils_bytes: utils_bytes.unbind(),
        to_native_string: to_native_string.unbind(),
        invalid_header: invalid_header.unbind(),
        invalid_url: invalid_url.unbind(),
        location_parse_error: location_parse_error.unbind(),
        missing_schema: missing_schema.unbind(),
        parse_url: parse_url.unbind(),
        requote_uri: requote_uri.unbind(),
        unicode_is_ascii: unicode_is_ascii.unbind(),
        urlunparse: urlunparse.unbind(),
        encode_params_descriptor: encode_params_descriptor.unbind(),
        builtin_str: builtin_str.unbind(),
        prepare_method: prepare_method.unbind(),
        prepare_headers: prepare_headers.unbind(),
        prepare_url: prepare_url.unbind(),
        object_getattribute: object_getattribute.unbind(),
        object_setattr: object_setattr.unbind(),
        prepare_method_trust,
        prepare_headers_trust,
        prepare_url_trust,
        encode_params_function: encode_params_function.unbind(),
        encode_params_trust,
        to_native_string_trust,
        parse_url_trust,
        requote_uri_trust,
        unicode_is_ascii_trust,
        urlunparse_trust,
        check_header_validity_trust,
        validate_header_part_trust,
        case_insensitive_dict_init_trust,
        case_insensitive_dict_update_trust,
        case_insensitive_dict_setitem_trust,
    })
}

fn models_state(py: Python<'_>) -> PyResult<&ModelsState> {
    MODELS_STATE.get_or_try_init(py, || initialize_models_state(py))
}

fn initialize_prepare_body_state(py: Python<'_>) -> PyResult<PrepareBodyState> {
    let state = models_state(py)?;
    let prepared_request = state.prepared_request.bind(py);
    let prepare_body = required_raw_type_entry(prepared_request, "prepare_body")?;
    let prepare_body_trust = build_canonical_function(
        py,
        &prepare_body,
        "requests.models",
        "PreparedRequest.prepare_body",
        DefaultPolicy::SingleNone,
        false,
        &[],
    )?;
    Ok(PrepareBodyState {
        prepare_body: prepare_body.unbind(),
        prepare_body_trust,
    })
}

fn prepare_body_state(py: Python<'_>) -> PyResult<&PrepareBodyState> {
    PREPARE_BODY_STATE.get_or_try_init(py, || initialize_prepare_body_state(py))
}

fn initialize_prepare_content_length_state(py: Python<'_>) -> PyResult<PrepareContentLengthState> {
    let state = models_state(py)?;
    let prepare_content_length =
        required_raw_type_entry(state.prepared_request.bind(py), "prepare_content_length")?;
    let prepare_content_length_trust = build_canonical_function(
        py,
        &prepare_content_length,
        "requests.models",
        "PreparedRequest.prepare_content_length",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    Ok(PrepareContentLengthState {
        prepare_content_length: prepare_content_length.unbind(),
        prepare_content_length_trust,
    })
}

fn prepare_content_length_state(py: Python<'_>) -> PyResult<&PrepareContentLengthState> {
    PREPARE_CONTENT_LENGTH_STATE.get_or_try_init(py, || initialize_prepare_content_length_state(py))
}

fn initialize_rewind_body_state(py: Python<'_>) -> PyResult<RewindBodyState> {
    let state = models_state(py)?;
    let rewind_body = required_module_entry(state.utils.bind(py), "rewind_body")?;
    let rewind_body_trust = build_canonical_function(
        py,
        &rewind_body,
        "requests.utils",
        "rewind_body",
        DefaultPolicy::None,
        false,
        &[],
    )?;
    Ok(RewindBodyState {
        rewind_body: rewind_body.unbind(),
        rewind_body_trust,
    })
}

fn rewind_body_state(py: Python<'_>) -> PyResult<&RewindBodyState> {
    REWIND_BODY_STATE.get_or_try_init(py, || initialize_rewind_body_state(py))
}

fn raw_module_entry_is(
    py: Python<'_>,
    module: &Py<PyModule>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
    Ok(module
        .bind(py)
        .dict()
        .get_item(name)?
        .is_some_and(|value| value.is(expected.bind(py))))
}

fn raw_intrinsic_builtin_fallback_is(
    py: Python<'_>,
    state: &ModelsState,
    module: &Py<PyModule>,
    name: &str,
    intrinsic: IntrinsicBuiltin,
) -> PyResult<bool> {
    if module.bind(py).dict().contains(name)? {
        return Ok(false);
    }
    let Some(current) = state.builtins.bind(py).dict().get_item(name)? else {
        return Ok(false);
    };
    intrinsic_builtin_is(py, state.builtins.bind(py), &current, intrinsic)
}

fn raw_type_entry_is(
    py: Python<'_>,
    class: &Bound<'_, PyType>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
    Ok(raw_type_entry(class, name)?.is_some_and(|value| value.is(expected.bind(py))))
}

fn raw_instance_dict<'py>(
    py: Python<'py>,
    state: &ModelsState,
    subject: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    Ok(state
        .object_getattribute
        .bind(py)
        .call1((subject, "__dict__"))?
        .cast_into::<PyDict>()?)
}

fn trusted_bound_method_with_callable<'py>(
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
    name: &str,
    callable: Bound<'py, PyAny>,
    expected: &Py<PyAny>,
    expected_trust: &CanonicalFunction,
) -> PyResult<(Bound<'py, PyAny>, bool)> {
    let state = models_state(py)?;
    if !subject.get_type().is(state.prepared_request.bind(py))
        || !raw_type_entry_is(
            py,
            &subject.get_type(),
            "__getattribute__",
            &state.object_getattribute,
        )?
        || !raw_type_entry_is(
            py,
            &subject.get_type(),
            "__setattr__",
            &state.object_setattr,
        )?
        || !raw_type_entry_is(py, &subject.get_type(), name, expected)?
        || raw_instance_dict(py, state, subject)?.contains(name)?
        || !callable.get_type().is(state.method_type.bind(py))
    {
        return Ok((callable, false));
    }

    let function = callable.getattr("__func__")?;
    let trusted = callable.getattr("__self__")?.is(subject)
        && function.is(expected.bind(py))
        && canonical_function_is(py, &function, expected_trust)?;
    Ok((callable, trusted))
}

#[derive(Clone, Copy)]
pub(crate) enum PreparedBodyMethod {
    PrepareBody,
    PrepareContentLength,
}

pub(crate) fn trusted_prepared_body_method<'py>(
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
    method: PreparedBodyMethod,
) -> PyResult<(Bound<'py, PyAny>, bool)> {
    match method {
        PreparedBodyMethod::PrepareBody => {
            let callable = subject.getattr("prepare_body")?;
            let Ok(state) = prepare_body_state(py) else {
                return Ok((callable, false));
            };
            trusted_bound_method_with_callable(
                py,
                subject,
                "prepare_body",
                callable,
                &state.prepare_body,
                &state.prepare_body_trust,
            )
        }
        PreparedBodyMethod::PrepareContentLength => {
            let callable = subject.getattr("prepare_content_length")?;
            let Ok(state) = prepare_content_length_state(py) else {
                return Ok((callable, false));
            };
            trusted_bound_method_with_callable(
                py,
                subject,
                "prepare_content_length",
                callable,
                &state.prepare_content_length,
                &state.prepare_content_length_trust,
            )
        }
    }
}

pub(crate) fn trusted_rewind_body(py: Python<'_>) -> PyResult<(Bound<'_, PyAny>, bool)> {
    let callable = PyModule::import(py, "requests.utils")?.getattr("rewind_body")?;
    let Ok(models) = models_state(py) else {
        return Ok((callable, false));
    };
    let Ok(state) = rewind_body_state(py) else {
        return Ok((callable, false));
    };
    let trusted = raw_module_entry_is(py, &models.utils, "rewind_body", &state.rewind_body)?
        && callable.is(state.rewind_body.bind(py))
        && canonical_function_is(py, &callable, &state.rewind_body_trust)?;
    Ok((callable, trusted))
}

#[pyfunction]
fn _prepare_method_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    method: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let callable = subject.getattr("prepare_method")?;
    let Ok(state) = models_state(py) else {
        return Ok(callable.call1((method,))?.unbind());
    };
    let (callable, trusted) = trusted_bound_method_with_callable(
        py,
        subject,
        "prepare_method",
        callable,
        &state.prepare_method,
        &state.prepare_method_trust,
    )?;
    if !trusted || !authoritative_field_is_none(py, state, subject, "method")? {
        return Ok(callable.call1((method,))?.unbind());
    }

    if method.is_none() {
        subject.setattr("method", method)?;
        return Ok(py.None());
    }

    let is_one_character = if method.is_exact_instance_of::<PyString>() {
        method.cast::<PyString>()?.len()? == 1
    } else if method.is_exact_instance_of::<PyBytes>() {
        method.cast::<PyBytes>()?.len()? == 1
    } else {
        false
    };
    if is_one_character {
        return Ok(callable.call1((method,))?.unbind());
    }

    let prepared = if method.is_exact_instance_of::<PyString>() {
        if let Ok(text) = method.cast::<PyString>()?.to_str()
            && text.is_ascii()
        {
            Some(prepare_method(text))
        } else {
            None
        }
    } else if method.is_exact_instance_of::<PyBytes>() {
        let bytes = method.cast::<PyBytes>()?.as_bytes();
        if bytes.is_ascii() {
            let prepared = prepare_method_bytes(bytes);
            Some(String::from_utf8(prepared).expect("ASCII stays UTF-8"))
        } else {
            None
        }
    } else {
        None
    };
    let Some(prepared) = prepared else {
        return Ok(callable.call1((method,))?.unbind());
    };
    if !trusted_native_string_dependencies(py, state)? {
        return Ok(callable.call1((method,))?.unbind());
    }

    subject.setattr("method", method)?;
    subject.setattr("method", prepared)?;
    Ok(py.None())
}

#[pyfunction]
fn _prepare_url_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    url: &Bound<'_, PyAny>,
    params: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let callable = subject.getattr("prepare_url")?;
    let Ok(state) = models_state(py) else {
        return Ok(callable.call1((url, params))?.unbind());
    };
    let (callable, trusted) = trusted_bound_method_with_callable(
        py,
        subject,
        "prepare_url",
        callable,
        &state.prepare_url,
        &state.prepare_url_trust,
    )?;
    if !trusted {
        return Ok(callable.call1((url, params))?.unbind());
    }
    if !trusted_url_input_dependencies(py, state)? {
        return Ok(callable.call1((url, params))?.unbind());
    }

    let Some(raw_url) = exact_url_text(url)? else {
        return Ok(callable.call1((url, params))?.unbind());
    };
    let trimmed_url = trim_python_whitespace_start(&raw_url);
    let preserve_input_identity =
        url.is_exact_instance_of::<PyString>() && trimmed_url.len() == raw_url.len();
    let raw_url = trimmed_url.to_owned();
    let url_repr = PyString::new(py, &raw_url).repr()?.to_str()?.to_owned();
    if is_non_http_url(&raw_url) {
        if preserve_input_identity {
            subject.setattr("url", url)?;
        } else {
            subject.setattr("url", &raw_url)?;
        }
        return Ok(py.None());
    }
    if raw_url.len() >= 200_000 {
        return Ok(callable.call1((url, params))?.unbind());
    }
    if !url_is_native_safe(&raw_url) {
        return Ok(callable.call1((url, params))?.unbind());
    }
    if !trusted_url_parse_dependencies(py, state)? {
        return Ok(callable.call1((url, params))?.unbind());
    }
    let base_url = match prepare_url(&raw_url, "") {
        Ok(prepared) => prepared,
        Err(error) => {
            if !trusted_url_error_dependencies(py, state, error)? {
                return Ok(callable.call1((url, params))?.unbind());
            }
            return Err(url_preparation_error(
                py, state, error, &raw_url, &url_repr,
            )?);
        }
    };
    let encoded_params = if params.is_none() {
        String::new()
    } else {
        if !trusted_url_parameter_dependencies(py, state, subject, params)? {
            return Ok(callable.call1((url, params))?.unbind());
        }
        let Some(encoded_params) = exact_encoded_params(params)? else {
            return Ok(callable.call1((url, params))?.unbind());
        };
        encoded_params
    };
    subject.setattr("url", append_url_params(&base_url, &encoded_params))?;
    Ok(py.None())
}

fn trusted_native_string_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    Ok(raw_module_entry_is(
        py,
        &state.models,
        "to_native_string",
        &state.to_native_string,
    )? && raw_module_entry_is(py, &state.internal_utils, "builtin_str", &state.builtin_str)?
        && canonical_function_is(
            py,
            state.to_native_string.bind(py),
            &state.to_native_string_trust,
        )?)
}

fn trusted_url_input_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    Ok(raw_intrinsic_builtin_fallback_is(
        py,
        state,
        &state.models,
        "isinstance",
        IntrinsicBuiltin::Function("isinstance"),
    )? && raw_intrinsic_builtin_fallback_is(
        py,
        state,
        &state.models,
        "bytes",
        IntrinsicBuiltin::Bytes,
    )? && raw_intrinsic_builtin_fallback_is(
        py,
        state,
        &state.models,
        "str",
        IntrinsicBuiltin::Str,
    )?)
}

fn trusted_url_parse_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    Ok(
        raw_module_entry_is(py, &state.models, "parse_url", &state.parse_url)?
            && raw_module_entry_is(py, &state.models, "requote_uri", &state.requote_uri)?
            && raw_module_entry_is(
                py,
                &state.models,
                "unicode_is_ascii",
                &state.unicode_is_ascii,
            )?
            && raw_module_entry_is(py, &state.models, "urlunparse", &state.urlunparse)?
            && canonical_function_is(py, state.parse_url.bind(py), &state.parse_url_trust)?
            && canonical_function_is(py, state.requote_uri.bind(py), &state.requote_uri_trust)?
            && canonical_function_is(
                py,
                state.unicode_is_ascii.bind(py),
                &state.unicode_is_ascii_trust,
            )?
            && canonical_function_is(py, state.urlunparse.bind(py), &state.urlunparse_trust)?,
    )
}

fn trusted_url_error_dependencies(
    py: Python<'_>,
    state: &ModelsState,
    error: UrlPreparationError,
) -> PyResult<bool> {
    match error {
        UrlPreparationError::InvalidLabel | UrlPreparationError::MissingHost => {
            raw_module_entry_is(py, &state.models, "InvalidURL", &state.invalid_url)
        }
        UrlPreparationError::MissingScheme => {
            raw_module_entry_is(py, &state.models, "MissingSchema", &state.missing_schema)
        }
        UrlPreparationError::Parse => {
            Ok(raw_module_entry_is(
                py,
                &state.models,
                "LocationParseError",
                &state.location_parse_error,
            )? && raw_module_entry_is(py, &state.models, "InvalidURL", &state.invalid_url)?)
        }
    }
}

fn trusted_url_parameter_dependencies(
    py: Python<'_>,
    state: &ModelsState,
    subject: &Bound<'_, PyAny>,
    params: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !params.is_exact_instance_of::<PyString>() && !params.is_exact_instance_of::<PyBytes>() {
        return Ok(false);
    }
    if !has_original_encode_params_descriptor(py, state, subject)?
        || !canonical_function_shape_is(
            py,
            state.encode_params_function.bind(py),
            &state.encode_params_trust,
        )?
    {
        return Ok(false);
    }
    if !raw_intrinsic_builtin_fallback_is(
        py,
        state,
        &state.models,
        "isinstance",
        IntrinsicBuiltin::Function("isinstance"),
    )? {
        return Ok(false);
    }
    let Some(basestring) = state.models.bind(py).dict().get_item("basestring")? else {
        return Ok(false);
    };
    known_value_is(
        py,
        state.builtins.bind(py),
        &basestring,
        KnownValue::IntrinsicPair(IntrinsicBuiltin::Str, IntrinsicBuiltin::Bytes),
        None,
    )
}

fn has_original_encode_params_descriptor(
    py: Python<'_>,
    state: &ModelsState,
    subject: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let instance_dict = raw_instance_dict(py, state, subject)?;
    if instance_dict.contains("_encode_params")? {
        return Ok(false);
    }

    Ok(raw_type_entry(&subject.get_type(), "_encode_params")?
        .is_some_and(|descriptor| descriptor.is(state.encode_params_descriptor.bind(py))))
}

fn exact_url_text(url: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    let text = if url.is_exact_instance_of::<PyString>() {
        url.cast::<PyString>()?.clone()
    } else if url.is_exact_instance_of::<PyBytes>() {
        url.call_method1("decode", ("utf8",))?
            .cast_into::<PyString>()?
    } else {
        return Ok(None);
    };
    let Ok(raw_url) = text.to_str() else {
        return Ok(None);
    };
    Ok(Some(raw_url.to_owned()))
}

fn exact_encoded_params(params: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if params.is_none() {
        return Ok(Some(String::new()));
    }
    if params.is_exact_instance_of::<PyString>() {
        return Ok(params.cast::<PyString>()?.to_str().ok().map(str::to_owned));
    }
    if params.is_exact_instance_of::<PyBytes>() {
        return Ok(Some(
            params
                .call_method1("decode", ("ascii",))?
                .cast_into::<PyString>()?
                .to_str()?
                .to_owned(),
        ));
    }
    if !params.is_exact_instance_of::<PyDict>() {
        return Ok(None);
    }

    let mut pairs = Vec::new();
    for (name, values) in params.cast::<PyDict>()?.iter() {
        let Some(name) = exact_parameter_bytes(&name)? else {
            return Ok(None);
        };
        if values.is_none() {
            continue;
        }
        if let Some(value) = exact_parameter_bytes(&values)? {
            pairs.push((name, value));
            continue;
        }

        if values.is_exact_instance_of::<PyList>() {
            for value in values.cast::<PyList>()?.iter() {
                if value.is_none() {
                    continue;
                }
                let Some(value) = exact_parameter_bytes(&value)? else {
                    return Ok(None);
                };
                pairs.push((name.clone(), value));
            }
        } else if values.is_exact_instance_of::<PyTuple>() {
            for value in values.cast::<PyTuple>()?.iter() {
                if value.is_none() {
                    continue;
                }
                let Some(value) = exact_parameter_bytes(&value)? else {
                    return Ok(None);
                };
                pairs.push((name.clone(), value));
            }
        } else {
            return Ok(None);
        }
    }
    Ok(Some(encode_query_pairs(&pairs)))
}

fn exact_parameter_bytes(value: &Bound<'_, PyAny>) -> PyResult<Option<Vec<u8>>> {
    if value.is_exact_instance_of::<PyBytes>() {
        let value = value.cast::<PyBytes>()?.as_bytes();
        return Ok((value.len() < 200_000).then(|| value.to_vec()));
    }
    if value.is_exact_instance_of::<PyString>() {
        return Ok(value
            .cast::<PyString>()?
            .to_str()
            .ok()
            .filter(|value| value.len() < 200_000)
            .map(|value| value.as_bytes().to_vec()));
    }
    Ok(None)
}

fn url_preparation_error(
    py: Python<'_>,
    state: &ModelsState,
    error: UrlPreparationError,
    raw_url: &str,
    url_repr: &str,
) -> PyResult<PyErr> {
    let (kind, message) = match error {
        UrlPreparationError::InvalidLabel => (
            ErrorKind::InvalidUrl,
            "URL has an invalid label.".to_owned(),
        ),
        UrlPreparationError::MissingHost => (
            ErrorKind::InvalidUrl,
            format!("Invalid URL {url_repr}: No host supplied"),
        ),
        UrlPreparationError::MissingScheme => (
            ErrorKind::MissingSchema,
            format!(
                "Invalid URL {url_repr}: No scheme supplied. Perhaps you meant https://{raw_url}?"
            ),
        ),
        UrlPreparationError::Parse => {
            (ErrorKind::InvalidUrl, format!("Failed to parse: {raw_url}"))
        }
    };
    Ok(map_typed_message(
        py,
        state.models.bind(py),
        MappingSite::UrlPreparation,
        kind,
        &message,
        None,
        None,
    ))
}

struct PythonHeaderRow {
    name: Py<PyAny>,
    value: Py<PyAny>,
    name_was_bytes: bool,
}

#[pyfunction]
fn _prepare_headers_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    headers: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let callable = subject.getattr("prepare_headers")?;
    let Ok(state) = models_state(py) else {
        return Ok(callable.call1((headers,))?.unbind());
    };
    let (callable, trusted) = trusted_bound_method_with_callable(
        py,
        subject,
        "prepare_headers",
        callable,
        &state.prepare_headers,
        &state.prepare_headers_trust,
    )?;
    if !trusted || !authoritative_field_is_none(py, state, subject, "headers")? {
        return Ok(callable.call1((headers,))?.unbind());
    }
    if !is_exact_header_candidate(py, headers)? {
        return Ok(callable.call1((headers,))?.unbind());
    }
    if !trusted_header_constructor_dependencies(py, state)? {
        return Ok(callable.call1((headers,))?.unbind());
    }
    let has_rows = !headers.is_none() && headers.len()? != 0;
    if has_rows
        && (!trusted_header_validation_dependencies(py, state)?
            || !trusted_header_materialization_dependencies(py, state)?)
    {
        return Ok(callable.call1((headers,))?.unbind());
    }

    let Some((native_headers, python_rows)) = exact_header_rows(py, headers)? else {
        return Ok(callable.call1((headers,))?.unbind());
    };
    let result = prepare_headers(&native_headers);
    let (prepared, error) = match result {
        Ok(prepared) => (prepared, None),
        Err(error) => (error.prepared.clone(), Some(error)),
    };
    if error.is_some()
        && !raw_module_entry_is(py, &state.utils, "InvalidHeader", &state.invalid_header)?
    {
        return Ok(callable.call1((headers,))?.unbind());
    }

    let output = state.case_insensitive_dict.bind(py).call0()?;
    subject.setattr("headers", &output)?;
    materialize_headers(py, &output, &python_rows, &prepared)?;

    if let Some(error) = error {
        return Err(header_preparation_error(py, state, &python_rows, &error)?);
    }
    Ok(py.None())
}

fn authoritative_field_is_none(
    py: Python<'_>,
    state: &ModelsState,
    subject: &Bound<'_, PyAny>,
    name: &str,
) -> PyResult<bool> {
    let instance_dict = raw_instance_dict(py, state, subject)?;
    Ok(instance_dict
        .get_item(name)?
        .is_some_and(|value| value.is_none()))
}

fn trusted_header_validation_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    Ok(raw_module_entry_is(
        py,
        &state.models,
        "check_header_validity",
        &state.check_header_validity,
    )? && raw_module_entry_is(
        py,
        &state.utils,
        "_validate_header_part",
        &state.validate_header_part,
    )? && raw_module_entry_is(
        py,
        &state.utils,
        "_HEADER_VALIDATORS_STR",
        &state.header_validators_str,
    )? && raw_module_entry_is(
        py,
        &state.utils,
        "_HEADER_VALIDATORS_BYTE",
        &state.header_validators_byte,
    )? && raw_module_entry_is(py, &state.utils, "str", &state.utils_str)?
        && raw_module_entry_is(py, &state.utils, "bytes", &state.utils_bytes)?
        && canonical_function_is(
            py,
            state.check_header_validity.bind(py),
            &state.check_header_validity_trust,
        )?
        && canonical_function_is(
            py,
            state.validate_header_part.bind(py),
            &state.validate_header_part_trust,
        )?)
}

fn is_exact_header_candidate(py: Python<'_>, headers: &Bound<'_, PyAny>) -> PyResult<bool> {
    if headers.is_none() || headers.is_exact_instance_of::<PyDict>() {
        return Ok(true);
    }
    if !is_trusted_ordered_dict(py, headers)? {
        return Ok(false);
    }
    let instance_dict = headers.getattr("__dict__")?.cast_into::<PyDict>()?;
    Ok(!instance_dict.contains("items")?)
}

fn trusted_header_constructor_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    let class = state.case_insensitive_dict.bind(py);
    Ok(state
        .models
        .bind(py)
        .dict()
        .get_item("CaseInsensitiveDict")?
        .is_some_and(|value| value.is(class))
        && raw_type_entry(class, "_store")?.is_none()
        && raw_module_entry_is(py, &state.structures, "OrderedDict", &state.ordered_dict)?
        && raw_type_entry_is(py, class, "__init__", &state.case_insensitive_dict_init)?
        && raw_type_entry_is(py, class, "__new__", &state.case_insensitive_dict_new)?
        && raw_type_entry_is(
            py,
            class,
            "__getattribute__",
            &state.case_insensitive_dict_getattribute,
        )?
        && raw_type_entry_is(
            py,
            class,
            "__setattr__",
            &state.case_insensitive_dict_setattr,
        )?
        && raw_type_entry_is(py, class, "update", &state.case_insensitive_dict_update)?
        && canonical_function_is(
            py,
            state.case_insensitive_dict_init.bind(py),
            &state.case_insensitive_dict_init_trust,
        )?
        && canonical_function_is(
            py,
            state.case_insensitive_dict_update.bind(py),
            &state.case_insensitive_dict_update_trust,
        )?)
}

fn trusted_header_materialization_dependencies(
    py: Python<'_>,
    state: &ModelsState,
) -> PyResult<bool> {
    Ok(trusted_native_string_dependencies(py, state)?
        && raw_type_entry_is(
            py,
            state.case_insensitive_dict.bind(py),
            "__setitem__",
            &state.case_insensitive_dict_setitem,
        )?
        && canonical_function_is(
            py,
            state.case_insensitive_dict_setitem.bind(py),
            &state.case_insensitive_dict_setitem_trust,
        )?)
}

fn exact_header_rows(
    py: Python<'_>,
    headers: &Bound<'_, PyAny>,
) -> PyResult<Option<(Vec<HeaderInput>, Vec<PythonHeaderRow>)>> {
    if headers.is_none() {
        return Ok(Some((Vec::new(), Vec::new())));
    }

    let mut native = Vec::new();
    let mut python = Vec::new();
    if headers.is_exact_instance_of::<PyDict>() {
        for (name, value) in headers.cast::<PyDict>()?.iter() {
            if !push_exact_header(py, &name, &value, &mut native, &mut python)? {
                return Ok(None);
            }
        }
        return Ok(Some((native, python)));
    }
    if !is_exact_header_candidate(py, headers)? {
        return Ok(None);
    }
    for (name, _) in headers.cast::<PyDict>()?.iter() {
        if !exact_header_name_is_native(&name)? {
            return Ok(None);
        }
    }
    for item in headers.call_method0("items")?.try_iter()? {
        let item = item?.cast_into::<PyTuple>()?;
        let name = item.get_item(0)?;
        let value = item.get_item(1)?;
        if !push_exact_header(py, &name, &value, &mut native, &mut python)? {
            return Ok(None);
        }
    }
    Ok(Some((native, python)))
}

fn exact_header_name_is_native(name: &Bound<'_, PyAny>) -> PyResult<bool> {
    if name.is_exact_instance_of::<PyString>() {
        return Ok(name.cast::<PyString>()?.to_str().is_ok_and(str::is_ascii));
    }
    if name.is_exact_instance_of::<PyBytes>() {
        return Ok(name.cast::<PyBytes>()?.as_bytes().is_ascii());
    }
    Ok(false)
}

fn push_exact_header(
    py: Python<'_>,
    name: &Bound<'_, PyAny>,
    value: &Bound<'_, PyAny>,
    native: &mut Vec<HeaderInput>,
    python: &mut Vec<PythonHeaderRow>,
) -> PyResult<bool> {
    let (native_name, name_was_bytes) = if name.is_exact_instance_of::<PyString>() {
        let Ok(text) = name.cast::<PyString>()?.to_str() else {
            return Ok(false);
        };
        if !text.is_ascii() {
            return Ok(false);
        }
        (HeaderPart::Text(text.to_owned()), false)
    } else if name.is_exact_instance_of::<PyBytes>() {
        let bytes = name.cast::<PyBytes>()?.as_bytes();
        if !bytes.is_ascii() {
            return Ok(false);
        }
        (HeaderPart::Bytes(bytes.to_vec()), true)
    } else {
        return Ok(false);
    };

    let native_value = if value.is_exact_instance_of::<PyString>() {
        let Ok(text) = value.cast::<PyString>()?.to_str() else {
            return Ok(false);
        };
        if !text.is_ascii() {
            return Ok(false);
        }
        HeaderPart::Text(text.to_owned())
    } else if value.is_exact_instance_of::<PyBytes>() {
        HeaderPart::Bytes(value.cast::<PyBytes>()?.as_bytes().to_vec())
    } else {
        return Ok(false);
    };

    native.push(HeaderInput {
        name: native_name,
        value: native_value,
    });
    python.push(PythonHeaderRow {
        name: name.clone().unbind(),
        value: value.clone().unbind(),
        name_was_bytes,
    });
    let _ = py;
    Ok(true)
}

fn materialize_headers(
    py: Python<'_>,
    output: &Bound<'_, PyAny>,
    rows: &[PythonHeaderRow],
    prepared: &[PreparedHeader],
) -> PyResult<()> {
    for header in prepared {
        let row = &rows[header.source_index];
        if row.name_was_bytes {
            output.set_item(PyString::new(py, &header.name), row.value.bind(py))?;
        } else {
            output.set_item(row.name.bind(py), row.value.bind(py))?;
        }
    }
    Ok(())
}

fn header_preparation_error(
    py: Python<'_>,
    state: &ModelsState,
    rows: &[PythonHeaderRow],
    error: &HeaderPreparationError,
) -> PyResult<PyErr> {
    let row = &rows[error.source_index];
    let part = match error.part {
        InvalidHeaderPart::Name => row.name.bind(py),
        InvalidHeaderPart::Value => row.value.bind(py),
    };
    let kind = match error.part {
        InvalidHeaderPart::Name => "name",
        InvalidHeaderPart::Value => "value",
    };
    let message = format!(
        "Invalid leading whitespace, reserved character(s), or return character(s) in header {kind}: {}",
        part.repr()?.to_str()?
    );
    Ok(map_typed_message(
        py,
        state.utils.bind(py),
        MappingSite::HeaderPreparation,
        ErrorKind::InvalidHeader,
        &message,
        None,
        None,
    ))
}

fn is_trusted_ordered_dict(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<bool> {
    let value_type = value.get_type();
    if !value_type.get_type().is(py.get_type::<PyType>()) {
        return Ok(false);
    }

    let flags = value_type.getattr("__flags__")?.extract::<u64>()?;
    const IMMUTABLE_TYPE: u64 = 1 << 8;
    const HEAP_TYPE: u64 = 1 << 9;
    if flags & HEAP_TYPE != 0
        || flags & IMMUTABLE_TYPE == 0
        || value_type.module()?.to_str()? != "collections"
        || value_type.qualname()?.to_str()? != "OrderedDict"
    {
        return Ok(false);
    }
    let bases = value_type.bases();
    Ok(bases.len() == 1 && bases.get_item(0)?.is(py.get_type::<PyDict>()))
}

#[pyfunction]
fn _prepared_fields_snapshot(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let headers = subject.getattr("headers")?;
    let method = subject.getattr("method")?;
    let url = subject.getattr("url")?;
    let header_rows = if headers.is_none() {
        py.None()
    } else {
        py.get_type::<PyList>()
            .call1((headers.call_method0("items")?,))?
            .unbind()
    };

    let snapshot = PyDict::new(py);
    snapshot.set_item("method", method)?;
    snapshot.set_item("url", url)?;
    snapshot.set_item("headers", header_rows)?;
    Ok(snapshot.into_any().unbind())
}

#[pyfunction]
fn _prepare_auth_trial(
    compat: &Bound<'_, PyAny>,
    _subject: &Bound<'_, PyAny>,
    _auth: &Bound<'_, PyAny>,
    _url: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(compat.call0()?.unbind())
}

#[pyfunction]
fn _model_facade_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    args: &Bound<'_, PyAny>,
    _kwargs: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let args = args.cast::<PyTuple>()?;
    match operation {
        "prepare_method" if args.len() == 1 => {
            _prepare_method_trial(py, subject, &args.get_item(0)?)
        }
        "prepare_url" if args.len() == 2 => {
            _prepare_url_trial(py, subject, &args.get_item(0)?, &args.get_item(1)?)
        }
        "prepare_headers" if args.len() == 1 => {
            _prepare_headers_trial(py, subject, &args.get_item(0)?)
        }
        "prepare_body" if args.len() == 3 => _prepare_body_trial(
            py,
            subject,
            &args.get_item(0)?,
            &args.get_item(1)?,
            &args.get_item(2)?,
        ),
        "prepare_content_length" if args.len() == 1 => {
            _prepare_content_length_trial(py, subject, &args.get_item(0)?)
        }
        _ => Ok(py.NotImplemented()),
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_prepare_method_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_url_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_headers_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepared_fields_snapshot, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_auth_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_model_facade_trial, module)?)?;
    Ok(())
}

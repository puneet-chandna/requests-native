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
    HeaderInput, HeaderPart, HeaderPreparationError, InvalidHeaderPart, PreparedHeader,
    UrlPreparationError, append_url_params, is_non_http_url, prepare_headers, prepare_method,
    prepare_method_bytes, prepare_url, url_is_native_safe,
};

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
    Quote {
        safe: &'static str,
    },
    Urlencode {
        quote_plus: Py<PyAny>,
        quote_plus_trust: Box<CanonicalFunction>,
        quote: Py<PyAny>,
        quote_trust: Box<CanonicalFunction>,
    },
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
    KnownModule(KnownValue),
    Builtin {
        expected: Py<PyAny>,
        function: Option<Box<CanonicalFunction>>,
    },
    IntrinsicBuiltin(IntrinsicBuiltin),
    Missing,
    Unprovable,
}

#[derive(Clone, Copy)]
enum IntrinsicBuiltin {
    IsInstance,
    Str,
    Bytes,
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
    Regex(KnownRegex),
    RegexPair(KnownRegex, KnownRegex),
}

#[derive(Clone, Copy)]
enum DefaultPolicy {
    None,
    Captured,
    ToNativeString,
    SingleNone,
    SingleEmptyTuple,
    Quote(&'static str),
    Urlencode,
}

struct ModelsState {
    internal_utils: Py<PyModule>,
    builtins: Py<PyModule>,
    models: Py<PyModule>,
    model_types: Py<PyModule>,
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
    basestring: Py<PyAny>,
    has_read: Py<PyAny>,
    to_key_val_list: Py<PyAny>,
    unicode_is_ascii: Py<PyAny>,
    urlencode: Py<PyAny>,
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
    has_read_trust: CanonicalFunction,
    to_key_val_list_trust: CanonicalFunction,
    urlencode_trust: CanonicalFunction,
    check_header_validity_trust: CanonicalFunction,
    validate_header_part_trust: CanonicalFunction,
    case_insensitive_dict_init_trust: CanonicalFunction,
    case_insensitive_dict_update_trust: CanonicalFunction,
    case_insensitive_dict_setitem_trust: CanonicalFunction,
}

static MODELS_STATE: PyOnceLock<ModelsState> = PyOnceLock::new();

#[allow(unsafe_code)]
fn function_code<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyCode>> {
    // SAFETY: CPython returns a borrowed reference owned by the exact
    // PyFunction, and both the function and returned Bound stay under `py`.
    unsafe {
        Ok(
            Bound::from_borrowed_ptr(py, pyo3::ffi::PyFunction_GetCode(function.as_ptr()))
                .cast_into::<PyCode>()?,
        )
    }
}

#[allow(unsafe_code)]
fn function_globals<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> PyResult<Bound<'py, PyDict>> {
    // SAFETY: CPython returns a borrowed reference owned by the exact
    // PyFunction, and both the function and returned Bound stay under `py`.
    unsafe {
        Ok(
            Bound::from_borrowed_ptr(py, pyo3::ffi::PyFunction_GetGlobals(function.as_ptr()))
                .cast_into::<PyDict>()?,
        )
    }
}

#[allow(unsafe_code)]
fn function_defaults<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by the
    // exact PyFunction, and both the function and Bound stay under `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyFunction_GetDefaults(function.as_ptr()))
    }
}

#[allow(unsafe_code)]
fn function_kwdefaults<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: CPython returns either null or a borrowed reference owned by the
    // exact PyFunction, and both the function and Bound stay under `py`.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyFunction_GetKwDefaults(function.as_ptr()))
    }
}

#[allow(unsafe_code)]
fn c_function_self<'py>(
    py: Python<'py>,
    function: &Bound<'py, PyCFunction>,
) -> Option<Bound<'py, PyAny>> {
    // SAFETY: PyCFunction_GetSelf returns either null or a borrowed reference
    // owned by the exact PyCFunction while `py` is attached.
    unsafe {
        Bound::from_borrowed_ptr_or_opt(py, pyo3::ffi::PyCFunction_GetSelf(function.as_ptr()))
    }
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
        IntrinsicBuiltin::IsInstance => {
            let Ok(function) = current.cast::<PyCFunction>() else {
                return Ok(false);
            };
            Ok(
                function.getattr("__name__")?.extract::<String>()? == "isinstance"
                    && function.getattr("__module__")?.extract::<String>()? == "builtins"
                    && c_function_self(py, function).is_some_and(|owner| owner.is(state_builtins)),
            )
        }
    }
}

fn intrinsic_builtin_for_name(name: &str) -> Option<IntrinsicBuiltin> {
    match name {
        "isinstance" => Some(IntrinsicBuiltin::IsInstance),
        "str" => Some(IntrinsicBuiltin::Str),
        "bytes" => Some(IntrinsicBuiltin::Bytes),
        _ => None,
    }
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
        ("urllib.parse", "_ALWAYS_SAFE_BYTES") => Some(KnownValue::Bytes(
            b"-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~",
        )),
        ("urllib.parse", "uses_netloc") => {
            Some(KnownValue::TextListContaining(NATIVE_NETLOC_SCHEMES))
        }
        _ => None,
    }
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
        ("match", "method_descriptor"),
        ("search", "method_descriptor"),
        ("pattern", "member_descriptor"),
        ("flags", "member_descriptor"),
    ] {
        let descriptor = namespace.get_item(name)?;
        if descriptor.get_type().name()?.to_str()? != descriptor_type
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

fn known_value_is(
    py: Python<'_>,
    builtins: &Bound<'_, PyModule>,
    current: &Bound<'_, PyAny>,
    expected: KnownValue,
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

fn canonical_module_code<'py>(py: Python<'py>, module_name: &str) -> PyResult<Bound<'py, PyCode>> {
    let module = PyModule::import(py, module_name)?;
    let spec = required_module_entry(&module, "__spec__")?;
    let loader = spec.getattr("loader")?;
    Ok(loader
        .call_method1("get_code", (module_name,))?
        .cast_into::<PyCode>()?)
}

fn canonical_code<'py>(
    py: Python<'py>,
    module_name: &str,
    qualname: &str,
) -> PyResult<Bound<'py, PyCode>> {
    let root = canonical_module_code(py, module_name)?;
    find_code_by_scope(&root, qualname)?.ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "canonical code not found for {module_name}.{qualname}"
        ))
    })
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
        DefaultPolicy::Quote(safe) => CanonicalDefaults::Quote { safe },
        DefaultPolicy::Urlencode => {
            let urllib_parse = PyModule::import(py, "urllib.parse")?;
            let quote_plus = required_module_entry(&urllib_parse, "quote_plus")?;
            let quote = required_module_entry(&urllib_parse, "quote")?;
            let quote_plus_trust = build_canonical_function_inner(
                py,
                &quote_plus,
                "urllib.parse",
                "quote_plus",
                DefaultPolicy::Quote(""),
                true,
                &[],
                &descendants,
            )?;
            let quote_trust = build_canonical_function_inner(
                py,
                &quote,
                "urllib.parse",
                "quote",
                DefaultPolicy::Quote("/"),
                true,
                &["TypeError"],
                &descendants,
            )?;
            CanonicalDefaults::Urlencode {
                quote_plus: quote_plus.unbind(),
                quote_plus_trust: Box::new(quote_plus_trust),
                quote: quote.unbind(),
                quote_trust: Box::new(quote_trust),
            }
        }
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
                CanonicalGlobalResolution::KnownModule(known)
            } else if let Some(value) = module.dict().get_item(&name)? {
                match direct_function_trust(py, &value, ancestors) {
                    Ok(function) => CanonicalGlobalResolution::Module {
                        function,
                        expected: value.unbind(),
                    },
                    Err(_) => CanonicalGlobalResolution::Unprovable,
                }
            } else {
                CanonicalGlobalResolution::Unprovable
            }
        } else if let Some(intrinsic) = intrinsic_builtin_for_name(&name) {
            CanonicalGlobalResolution::IntrinsicBuiltin(intrinsic)
        } else if let Some(value) = builtins.get_item(&name)? {
            match direct_function_trust(py, &value, ancestors) {
                Ok(function) => CanonicalGlobalResolution::Builtin {
                    function,
                    expected: value.unbind(),
                },
                Err(_) => CanonicalGlobalResolution::Unprovable,
            }
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
    Ok(Some(Box::new(build_canonical_function_inner(
        py,
        value,
        &module_name,
        &qualname,
        DefaultPolicy::Captured,
        true,
        &[],
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
        CanonicalDefaults::Quote { safe } => {
            if kwdefaults.is_some() {
                return Ok(false);
            }
            let Some(defaults) = defaults else {
                return Ok(false);
            };
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            Ok(defaults.len() == 3
                && defaults
                    .get_item(0)?
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == *safe))
                && defaults.get_item(1)?.is_none()
                && defaults.get_item(2)?.is_none())
        }
        CanonicalDefaults::Urlencode {
            quote_plus,
            quote_plus_trust,
            quote,
            quote_trust,
        } => {
            if kwdefaults.is_some() {
                return Ok(false);
            }
            let Some(defaults) = defaults else {
                return Ok(false);
            };
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            if defaults.len() != 5
                || !defaults.get_item(0)?.is_exact_instance_of::<PyBool>()
                || defaults.get_item(0)?.extract::<bool>()?
                || !defaults
                    .get_item(1)?
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(str::is_empty))
                || !defaults.get_item(2)?.is_none()
                || !defaults.get_item(3)?.is_none()
            {
                return Ok(false);
            }
            let actual_quote_plus = defaults.get_item(4)?;
            Ok(actual_quote_plus.is(quote_plus.bind(py))
                && canonical_function_is(py, &actual_quote_plus, quote_plus_trust)?
                && canonical_function_is(py, quote.bind(py), quote_trust)?)
        }
    }
}

fn canonical_function_is(
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
    if !code.eq(expected.code.bind(py))?
        || !globals.is(expected.globals.bind(py))
        || !builtins.is(expected.builtins.bind(py))
        || !canonical_defaults_are_current(py, function, &expected.defaults)?
    {
        return Ok(false);
    }
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
            CanonicalGlobalResolution::KnownModule(expected_value) => {
                let Some(current) = current_global else {
                    return Ok(false);
                };
                if !known_value_is(
                    py,
                    expected.builtins_module.bind(py),
                    &current,
                    *expected_value,
                )? {
                    return Ok(false);
                }
                continue;
            }
            CanonicalGlobalResolution::Builtin {
                expected: expected_value,
                function,
            } => {
                if current_global.is_some() {
                    return Ok(false);
                }
                let Some(current) = builtins.get_item(&dependency.name)? else {
                    return Ok(false);
                };
                if !current.is(expected_value.bind(py)) {
                    return Ok(false);
                }
                (current, function)
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

fn initialize_models_state(py: Python<'_>) -> PyResult<ModelsState> {
    let internal_utils = PyModule::import(py, "requests._internal_utils")?;
    let builtins = PyModule::import(py, "builtins")?;
    let models = PyModule::import(py, "requests.models")?;
    let model_types = required_module_entry(&models, "_t")?.cast_into::<PyModule>()?;
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
    let basestring = required_module_entry(&models, "basestring")?;
    let has_read = required_module_entry(&model_types, "has_read")?;
    let to_key_val_list = required_module_entry(&models, "to_key_val_list")?;
    let unicode_is_ascii = required_module_entry(&models, "unicode_is_ascii")?;
    let urlencode = required_module_entry(&models, "urlencode")?;
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
        true,
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
    let has_read_trust = build_canonical_function(
        py,
        &has_read,
        "requests._types",
        "has_read",
        DefaultPolicy::None,
        true,
        &[],
    )?;
    let to_key_val_list_trust = build_canonical_function(
        py,
        &to_key_val_list,
        "requests.utils",
        "to_key_val_list",
        DefaultPolicy::None,
        true,
        &["ValueError"],
    )?;
    let urlencode_trust = build_canonical_function(
        py,
        &urlencode,
        "urllib.parse",
        "urlencode",
        DefaultPolicy::Urlencode,
        true,
        &["TypeError"],
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
        &[],
    )?;
    let case_insensitive_dict_update_trust = build_canonical_function(
        py,
        &case_insensitive_dict_update,
        "_collections_abc",
        "MutableMapping.update",
        DefaultPolicy::SingleEmptyTuple,
        true,
        &[],
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
        model_types: model_types.unbind(),
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
        basestring: basestring.unbind(),
        has_read: has_read.unbind(),
        to_key_val_list: to_key_val_list.unbind(),
        unicode_is_ascii: unicode_is_ascii.unbind(),
        urlencode: urlencode.unbind(),
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
        has_read_trust,
        to_key_val_list_trust,
        urlencode_trust,
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

fn trusted_bound_method<'py>(
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
    name: &str,
    expected: &Py<PyAny>,
    expected_trust: &CanonicalFunction,
) -> PyResult<(Bound<'py, PyAny>, bool)> {
    let callable = subject.getattr(name)?;
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

#[pyfunction]
fn _prepare_method_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    method: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = models_state(py)?;
    let (callable, trusted) = trusted_bound_method(
        py,
        subject,
        "prepare_method",
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
    let state = models_state(py)?;
    let (callable, trusted) = trusted_bound_method(
        py,
        subject,
        "prepare_url",
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
        IntrinsicBuiltin::IsInstance,
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
    if !has_original_encode_params_descriptor(py, state, subject)?
        || !canonical_function_is(
            py,
            state.encode_params_function.bind(py),
            &state.encode_params_trust,
        )?
    {
        return Ok(false);
    }
    if params.is_exact_instance_of::<PyString>() || params.is_exact_instance_of::<PyBytes>() {
        return trusted_native_string_dependencies(py, state);
    }
    if !params.is_exact_instance_of::<PyDict>() {
        return Ok(false);
    }

    let model_types_is_original = state
        .models
        .bind(py)
        .dict()
        .get_item("_t")?
        .is_some_and(|value| value.is(state.model_types.bind(py)));
    Ok(model_types_is_original
        && raw_module_entry_is(py, &state.model_types, "has_read", &state.has_read)?
        && raw_module_entry_is(py, &state.models, "basestring", &state.basestring)?
        && raw_module_entry_is(py, &state.models, "to_key_val_list", &state.to_key_val_list)?
        && raw_module_entry_is(py, &state.models, "urlencode", &state.urlencode)?
        && canonical_function_is(py, state.has_read.bind(py), &state.has_read_trust)?
        && canonical_function_is(
            py,
            state.to_key_val_list.bind(py),
            &state.to_key_val_list_trust,
        )?
        && canonical_function_is(py, state.urlencode.bind(py), &state.urlencode_trust)?)
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
        return Ok(Some(value.cast::<PyBytes>()?.as_bytes().to_vec()));
    }
    if value.is_exact_instance_of::<PyString>() {
        return Ok(value
            .cast::<PyString>()?
            .to_str()
            .ok()
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
    let (exception, message) = match error {
        UrlPreparationError::InvalidLabel => {
            (&state.invalid_url, "URL has an invalid label.".to_owned())
        }
        UrlPreparationError::MissingHost => (
            &state.invalid_url,
            format!("Invalid URL {url_repr}: No host supplied"),
        ),
        UrlPreparationError::MissingScheme => (
            &state.missing_schema,
            format!(
                "Invalid URL {url_repr}: No scheme supplied. Perhaps you meant https://{raw_url}?"
            ),
        ),
        UrlPreparationError::Parse => (&state.invalid_url, format!("Failed to parse: {raw_url}")),
    };
    let instance = exception.bind(py).call1((message,))?;
    Ok(PyErr::from_value(instance))
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
    let state = models_state(py)?;
    let (callable, trusted) = trusted_bound_method(
        py,
        subject,
        "prepare_headers",
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
    let instance = state.invalid_header.bind(py).call1((message,))?;
    Ok(PyErr::from_value(instance))
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

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    let _ = models_state(py)?;
    module.add_function(wrap_pyfunction!(_prepare_method_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_url_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_headers_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepared_fields_snapshot, module)?)?;
    Ok(())
}

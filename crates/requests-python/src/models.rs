use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBool, PyBytes, PyBytesMethods, PyCode, PyDict, PyDictMethods,
    PyFunction, PyList, PyListMethods, PyModule, PyString, PyTuple, PyTupleMethods, PyType,
    PyTypeMethods,
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
    builtins: Py<PyDict>,
    defaults: CanonicalDefaults,
    dependencies: Vec<CanonicalGlobal>,
}

enum CanonicalDefaults {
    None,
    Captured {
        defaults: Py<PyAny>,
        kwdefaults: Py<PyAny>,
    },
    ToNativeString,
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
    Builtin {
        expected: Py<PyAny>,
        function: Option<Box<CanonicalFunction>>,
    },
    Missing,
    Unprovable,
}

#[derive(Clone, Copy)]
enum DefaultPolicy {
    None,
    Captured,
    ToNativeString,
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
    python_str: Py<PyAny>,
    python_bytes: Py<PyAny>,
    builtin_isinstance: Py<PyAny>,
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
}

static MODELS_STATE: PyOnceLock<ModelsState> = PyOnceLock::new();

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

fn find_code_by_qualname<'py>(
    code: &Bound<'py, PyCode>,
    qualname: &str,
) -> PyResult<Option<Bound<'py, PyCode>>> {
    let (code_name, qualified) = match code.getattr("co_qualname") {
        Ok(name) => (name.extract::<String>()?, true),
        Err(_) => (code.getattr("co_name")?.extract::<String>()?, false),
    };
    let expected_name = qualname.rsplit('.').next().unwrap_or(qualname);
    let matches = if qualified {
        code_name == qualname
    } else {
        code_name == expected_name
    };
    let mut found = matches.then(|| code.clone());
    for constant in code.getattr("co_consts")?.cast_into::<PyTuple>()?.iter() {
        let Ok(nested) = constant.cast_into::<PyCode>() else {
            continue;
        };
        if let Some(nested) = find_code_by_qualname(&nested, qualname)? {
            found = Some(nested);
        }
    }
    Ok(found)
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
    find_code_by_qualname(&root, qualname)?.ok_or_else(|| {
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
    let module = PyModule::import(py, module_name)?;
    let code = canonical_code(py, module_name, qualname)?;
    let builtins = required_module_entry(&module, "__builtins__")?.cast_into::<PyDict>()?;
    let module_globals = canonical_module_globals(py, module_name)?;
    let defaults = match policy {
        DefaultPolicy::None => CanonicalDefaults::None,
        DefaultPolicy::Captured => {
            let function = current.cast::<PyFunction>()?;
            CanonicalDefaults::Captured {
                defaults: function.getattr("__defaults__")?.unbind(),
                kwdefaults: function.getattr("__kwdefaults__")?.unbind(),
            }
        }
        DefaultPolicy::ToNativeString => CanonicalDefaults::ToNativeString,
        DefaultPolicy::Quote(safe) => CanonicalDefaults::Quote { safe },
        DefaultPolicy::Urlencode => {
            let urllib_parse = PyModule::import(py, "urllib.parse")?;
            let quote_plus = required_module_entry(&urllib_parse, "quote_plus")?;
            let quote = required_module_entry(&urllib_parse, "quote")?;
            let quote_plus_trust = build_canonical_function(
                py,
                &quote_plus,
                "urllib.parse",
                "quote_plus",
                DefaultPolicy::Quote(""),
                true,
                &[],
            )?;
            let quote_trust = build_canonical_function(
                py,
                &quote,
                "urllib.parse",
                "quote",
                DefaultPolicy::Quote("/"),
                true,
                &["TypeError"],
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
        )?
    } else {
        Vec::new()
    };

    Ok(CanonicalFunction {
        code: code.unbind(),
        globals: module.dict().unbind(),
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
) -> PyResult<Vec<CanonicalGlobal>> {
    let instructions = PyModule::import(py, "dis")?
        .getattr("get_instructions")?
        .call1((code,))?;
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
            if let Some(value) = module.dict().get_item(&name)? {
                match direct_function_trust(py, &value) {
                    Ok(function) => CanonicalGlobalResolution::Module {
                        function,
                        expected: value.unbind(),
                    },
                    Err(_) => CanonicalGlobalResolution::Unprovable,
                }
            } else {
                CanonicalGlobalResolution::Unprovable
            }
        } else if let Some(value) = builtins.get_item(&name)? {
            match direct_function_trust(py, &value) {
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
) -> PyResult<Option<Box<CanonicalFunction>>> {
    if !value.is_exact_instance_of::<PyFunction>() {
        return Ok(None);
    }
    let module_name = value.getattr("__module__")?.extract::<String>()?;
    let qualname = value.getattr("__qualname__")?.extract::<String>()?;
    Ok(Some(Box::new(build_canonical_function(
        py,
        value,
        &module_name,
        &qualname,
        DefaultPolicy::Captured,
        false,
        &[],
    )?)))
}

fn canonical_defaults_are_current(
    py: Python<'_>,
    function: &Bound<'_, PyFunction>,
    expected: &CanonicalDefaults,
) -> PyResult<bool> {
    let defaults = function.getattr("__defaults__")?;
    let kwdefaults = function.getattr("__kwdefaults__")?;
    match expected {
        CanonicalDefaults::None => Ok(defaults.is_none() && kwdefaults.is_none()),
        CanonicalDefaults::Captured {
            defaults: expected_defaults,
            kwdefaults: expected_kwdefaults,
        } => {
            Ok(defaults.is(expected_defaults.bind(py))
                && kwdefaults.is(expected_kwdefaults.bind(py)))
        }
        CanonicalDefaults::ToNativeString => {
            if !kwdefaults.is_none() {
                return Ok(false);
            }
            let Ok(defaults) = defaults.cast_into::<PyTuple>() else {
                return Ok(false);
            };
            Ok(defaults.len() == 1
                && defaults
                    .get_item(0)?
                    .cast::<PyString>()
                    .is_ok_and(|value| value.to_str().is_ok_and(|value| value == "ascii")))
        }
        CanonicalDefaults::Quote { safe } => {
            if !kwdefaults.is_none() {
                return Ok(false);
            }
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
            if !kwdefaults.is_none() {
                return Ok(false);
            }
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
    let Ok(code) = function.getattr("__code__")?.cast_into::<PyCode>() else {
        return Ok(false);
    };
    let globals = function.getattr("__globals__")?.cast_into::<PyDict>()?;
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
            CanonicalGlobalResolution::Builtin { expected, function } => {
                if current_global.is_some() {
                    return Ok(false);
                }
                let Some(current) = builtins.get_item(&dependency.name)? else {
                    return Ok(false);
                };
                if !current.is(expected.bind(py)) {
                    return Ok(false);
                }
                (current, function)
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
    let utils_str = required_module_entry(&utils, "str")?;
    let utils_bytes = required_module_entry(&utils, "bytes")?;
    let to_native_string = required_module_entry(&models, "to_native_string")?;
    let invalid_header = required_module_entry(&utils, "InvalidHeader")?;
    let invalid_url = required_module_entry(&models, "InvalidURL")?;
    let location_parse_error = required_module_entry(&models, "LocationParseError")?;
    let missing_schema = required_module_entry(&models, "MissingSchema")?;
    let parse_url = required_module_entry(&models, "parse_url")?;
    let requote_uri = required_module_entry(&models, "requote_uri")?;
    let basestring = required_module_entry(&models, "basestring")?;
    let python_str = required_module_entry(&builtins, "str")?;
    let python_bytes = required_module_entry(&builtins, "bytes")?;
    let builtin_isinstance = required_module_entry(&builtins, "isinstance")?;
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
    let builtin_str = required_module_entry(&internal_utils, "builtin_str")?;
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
        python_str: python_str.unbind(),
        python_bytes: python_bytes.unbind(),
        builtin_isinstance: builtin_isinstance.unbind(),
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

fn raw_builtin_fallback_is(
    py: Python<'_>,
    state: &ModelsState,
    module: &Py<PyModule>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
    Ok(!module.bind(py).dict().contains(name)?
        && raw_module_entry_is(py, &state.builtins, name, expected)?)
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
    Ok(raw_builtin_fallback_is(
        py,
        state,
        &state.models,
        "isinstance",
        &state.builtin_isinstance,
    )? && raw_builtin_fallback_is(py, state, &state.models, "bytes", &state.python_bytes)?
        && raw_builtin_fallback_is(py, state, &state.models, "str", &state.python_str)?)
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

    let Some((native_headers, python_rows)) = exact_header_rows(py, headers)? else {
        return Ok(callable.call1((headers,))?.unbind());
    };
    let result = prepare_headers(&native_headers);
    let (prepared, error) = match result {
        Ok(prepared) => (prepared, None),
        Err(error) => (error.prepared.clone(), Some(error)),
    };
    if !trusted_header_constructor_dependencies(py, state)?
        || (!native_headers.is_empty() && !trusted_header_validation_dependencies(py, state)?)
        || (!prepared.is_empty() && !trusted_header_materialization_dependencies(py, state)?)
        || (error.is_some()
            && !raw_module_entry_is(py, &state.utils, "InvalidHeader", &state.invalid_header)?)
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
        && raw_type_entry_is(py, class, "update", &state.case_insensitive_dict_update)?)
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

use std::marker::PhantomData;
use std::rc::Rc;

use pyo3::exceptions::{PyNameError, PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyDict, PyFunction, PyList, PyModule, PySet, PyTuple, PyType};
use pyo3::wrap_pyfunction;
use requests::cookies::{
    CookiePipelineStage, CookieScalar, CookieSnapshot, private_pipeline_stages,
};

use crate::bridge::{BridgeClosed, WorkerPayload};
use crate::errors::exception_matches;
use crate::runtime::run_with_owned_actions;

struct CookieState {
    module: Py<PyModule>,
    jar_type: Py<PyType>,
    std_jar_type: Py<PyType>,
    default_policy_type: Py<PyType>,
    cookie_type: Py<PyType>,
    cookielib: Py<PyAny>,
    cookie_constructor: Py<PyAny>,
    create_cookie_builtins: Py<PyDict>,
    deepvalues: Py<PyAny>,
    deepvalues_code: Py<PyAny>,
    morsel_type: Py<PyType>,
    morsel_mro: Vec<Py<PyType>>,
    morsel_getitem: Py<PyAny>,
    morsel_key: Py<PyAny>,
    morsel_value: Py<PyAny>,
    prepared_type: Py<PyType>,
    response_type: Py<PyType>,
    jar_inspect_methods: Vec<(&'static str, Py<PyAny>)>,
    jar_operation_methods: Vec<(&'static str, Py<PyAny>)>,
    std_operation_methods: Vec<(&'static str, Py<PyAny>)>,
    module_functions: Vec<(&'static str, Py<PyAny>, Py<PyAny>)>,
    std_add_header: Py<PyAny>,
    std_extract: Py<PyAny>,
    prepared_cookies: Py<PyAny>,
    mock_request_type: Py<PyType>,
    mock_response_type: Py<PyType>,
    time_module: Py<PyAny>,
    time_time: Py<PyAny>,
    time_strptime: Py<PyAny>,
    calendar_module: Py<PyAny>,
    calendar_timegm: Py<PyAny>,
    copy_function: Py<PyAny>,
    copy_module: Py<PyModule>,
    pickle_dumps: Py<PyAny>,
    pickle_loads: Py<PyAny>,
    pickle_module: Py<PyModule>,
    threading_module: Py<PyModule>,
    threading_rlock: Py<PyAny>,
    threading_rlock_code: Py<PyAny>,
    rlock_type: Py<PyType>,
    object_getattribute: Py<PyAny>,
    cookie_mro: Vec<Py<PyType>>,
    jar_mro: Vec<Py<PyType>>,
    std_jar_mro: Vec<Py<PyType>>,
    prepared_mro: Vec<Py<PyType>>,
    response_mro: Vec<Py<PyType>>,
}

struct FunctionGlobalOwner {
    globals: Py<PyDict>,
    builtins: Py<PyDict>,
}

#[allow(unsafe_code)]
fn exact_function_global_owner(
    py: Python<'_>,
    callable: &Bound<'_, PyAny>,
) -> PyResult<Option<FunctionGlobalOwner>> {
    if !callable.is_exact_instance_of::<PyFunction>() {
        return Ok(None);
    }
    let function = callable.cast::<PyFunction>()?;
    // SAFETY: PyFunction_GetGlobals returns a borrowed reference owned by the
    // exact PyFunction while both the function and returned Bound stay under
    // the attached interpreter token.
    let globals = unsafe {
        Bound::from_borrowed_ptr(py, pyo3::ffi::PyFunction_GetGlobals(function.as_ptr()))
            .cast_into::<PyDict>()?
    };
    let builtins = function.getattr("__builtins__")?;
    if !builtins.is_exact_instance_of::<PyDict>() {
        return Ok(None);
    }
    Ok(Some(FunctionGlobalOwner {
        globals: globals.unbind(),
        builtins: builtins.cast_into::<PyDict>()?.unbind(),
    }))
}

static COOKIE_STATE: PyOnceLock<CookieState> = PyOnceLock::new();

fn initialize_cookie_state(py: Python<'_>) -> PyResult<CookieState> {
    let module = PyModule::import(py, "requests.cookies")?;
    let jar_type = module.getattr("RequestsCookieJar")?.cast_into::<PyType>()?;
    let cookiejar = PyModule::import(py, "http.cookiejar")?;
    let std_jar_type = cookiejar.getattr("CookieJar")?.cast_into::<PyType>()?;
    let default_policy_type = cookiejar
        .getattr("DefaultCookiePolicy")?
        .cast_into::<PyType>()?;
    let cookie_type = cookiejar.getattr("Cookie")?.cast_into::<PyType>()?;
    let cookielib = module.getattr("cookielib")?;
    let cookie_constructor = cookielib.getattr("Cookie")?;
    let create_cookie_builtins = module
        .getattr("create_cookie")?
        .getattr("__builtins__")?
        .cast_into::<PyDict>()?;
    let deepvalues = cookielib.getattr("deepvalues")?;
    let deepvalues_code = deepvalues.getattr("__code__")?;
    let morsel_type = PyModule::import(py, "http.cookies")?
        .getattr("Morsel")?
        .cast_into::<PyType>()?;
    let models = PyModule::import(py, "requests.models")?;
    let prepared_type = models.getattr("PreparedRequest")?.cast_into::<PyType>()?;
    let response_type = models.getattr("Response")?.cast_into::<PyType>()?;
    let time_module = module.getattr("time")?;
    let calendar_module = module.getattr("calendar")?;
    let copy_module = PyModule::import(py, "copy")?;
    let pickle = PyModule::import(py, "pickle")?;
    let threading = module.getattr("threading")?.cast_into::<PyModule>()?;
    let threading_rlock = threading.getattr("RLock")?;
    let threading_rlock_code = threading_rlock.getattr("__code__")?;
    let rlock_type = threading_rlock.call0()?.get_type().unbind();
    let module_functions = [
        "create_cookie",
        "morsel_to_cookie",
        "remove_cookie_by_name",
        "get_cookie_header",
        "extract_cookies_to_jar",
    ]
    .into_iter()
    .map(|name| {
        let function = module.getattr(name)?;
        let code = function.getattr("__code__")?;
        Ok((name, function.unbind(), code.unbind()))
    })
    .collect::<PyResult<Vec<_>>>()?;
    let jar_operation_methods = [
        "__getattribute__",
        "__iter__",
        "__contains__",
        "set",
        "set_cookie",
        "get_dict",
        "copy",
        "update",
        "set_policy",
        "get_policy",
        "__getstate__",
        "__setstate__",
    ]
    .into_iter()
    .map(|name| Ok((name, jar_type.getattr(name)?.unbind())))
    .collect::<PyResult<Vec<_>>>()?;
    let std_operation_methods = [
        "__getattribute__",
        "__iter__",
        "set_cookie",
        "clear",
        "add_cookie_header",
        "extract_cookies",
        "set_policy",
    ]
    .into_iter()
    .map(|name| Ok((name, std_jar_type.getattr(name)?.unbind())))
    .collect::<PyResult<Vec<_>>>()?;
    let mro = |class: &Bound<'_, PyType>| -> PyResult<Vec<Py<PyType>>> {
        class
            .mro()
            .iter()
            .map(|owner| Ok(owner.cast_into::<PyType>()?.unbind()))
            .collect()
    };
    Ok(CookieState {
        module: module.clone().unbind(),
        jar_inspect_methods: [
            "__iter__",
            "iterkeys",
            "keys",
            "itervalues",
            "values",
            "iteritems",
            "items",
            "list_domains",
            "list_paths",
            "multiple_domains",
            "get_dict",
            "__getitem__",
            "get",
            "_find_no_duplicates",
        ]
        .into_iter()
        .map(|name| Ok((name, jar_type.getattr(name)?.unbind())))
        .collect::<PyResult<Vec<_>>>()?,
        jar_operation_methods,
        std_operation_methods,
        module_functions,
        std_add_header: std_jar_type.getattr("add_cookie_header")?.unbind(),
        std_extract: std_jar_type.getattr("extract_cookies")?.unbind(),
        prepared_cookies: prepared_type.getattr("prepare_cookies")?.unbind(),
        mock_request_type: module
            .getattr("MockRequest")?
            .cast_into::<PyType>()?
            .unbind(),
        mock_response_type: module
            .getattr("MockResponse")?
            .cast_into::<PyType>()?
            .unbind(),
        time_time: time_module.getattr("time")?.unbind(),
        time_strptime: time_module.getattr("strptime")?.unbind(),
        time_module: time_module.unbind(),
        calendar_timegm: calendar_module.getattr("timegm")?.unbind(),
        calendar_module: calendar_module.unbind(),
        copy_function: copy_module.getattr("copy")?.unbind(),
        copy_module: copy_module.clone().unbind(),
        pickle_dumps: pickle.getattr("dumps")?.unbind(),
        pickle_loads: pickle.getattr("loads")?.unbind(),
        pickle_module: pickle.clone().unbind(),
        threading_module: threading.clone().unbind(),
        threading_rlock: threading_rlock.clone().unbind(),
        threading_rlock_code: threading_rlock_code.unbind(),
        rlock_type,
        object_getattribute: py.get_type::<PyAny>().getattr("__getattribute__")?.unbind(),
        cookie_mro: mro(&cookie_type)?,
        jar_mro: mro(&jar_type)?,
        std_jar_mro: mro(&std_jar_type)?,
        prepared_mro: mro(&prepared_type)?,
        response_mro: mro(&response_type)?,
        morsel_mro: mro(&morsel_type)?,
        morsel_getitem: morsel_type.getattr("__getitem__")?.unbind(),
        morsel_key: morsel_type.getattr("key")?.unbind(),
        morsel_value: morsel_type.getattr("value")?.unbind(),
        jar_type: jar_type.unbind(),
        std_jar_type: std_jar_type.unbind(),
        default_policy_type: default_policy_type.unbind(),
        cookie_type: cookie_type.unbind(),
        cookielib: cookielib.unbind(),
        cookie_constructor: cookie_constructor.unbind(),
        create_cookie_builtins: create_cookie_builtins.unbind(),
        deepvalues: deepvalues.unbind(),
        deepvalues_code: deepvalues_code.unbind(),
        morsel_type: morsel_type.unbind(),
        prepared_type: prepared_type.unbind(),
        response_type: response_type.unbind(),
    })
}

fn cookie_state(py: Python<'_>) -> PyResult<&CookieState> {
    COOKIE_STATE.get_or_try_init(py, || initialize_cookie_state(py))
}

fn module_entry_is(
    module: &Bound<'_, PyModule>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
    if !exact_builtin_string_keys(&module.dict()) {
        return Ok(false);
    }
    Ok(module
        .dict()
        .get_item(name)?
        .is_some_and(|value| value.is(expected.bind(module.py()))))
}

fn mro_is(class: &Bound<'_, PyType>, expected: &[Py<PyType>]) -> PyResult<bool> {
    let current = class.mro();
    Ok(current.len() == expected.len()
        && current
            .iter()
            .zip(expected)
            .all(|(owner, expected)| owner.is(expected.bind(class.py()))))
}

fn raw_mro_type_entry<'py>(
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

fn method_is(ty: &Bound<'_, PyType>, name: &str, expected: &Py<PyAny>) -> PyResult<bool> {
    Ok(raw_mro_type_entry(ty, name)?.is_some_and(|value| value.is(expected.bind(ty.py()))))
}

fn exact_builtin_string_keys(dictionary: &Bound<'_, PyDict>) -> bool {
    dictionary.keys().iter().all(|key| exact_string_item(&key))
}

fn raw_instance_dict<'py>(value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    Ok(value.getattr("__dict__")?.cast_into::<PyDict>()?)
}

fn instance_methods_are_unshadowed(value: &Bound<'_, PyAny>, names: &[&str]) -> PyResult<bool> {
    let dictionary = raw_instance_dict(value)?;
    if !exact_builtin_string_keys(&dictionary) {
        return Ok(false);
    }
    for name in names {
        if dictionary.contains(name)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn expected_method<'a>(methods: &'a [(&'static str, Py<PyAny>)], name: &str) -> &'a Py<PyAny> {
    &methods
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing cached cookie method {name}"))
        .1
}

fn methods_are_pristine(
    class: &Bound<'_, PyType>,
    methods: &[(&'static str, Py<PyAny>)],
    names: &[&str],
) -> PyResult<bool> {
    for name in names {
        if !method_is(class, name, expected_method(methods, name))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn module_functions_are_pristine(
    py: Python<'_>,
    state: &CookieState,
    names: &[&str],
) -> PyResult<bool> {
    if !exact_builtin_string_keys(&state.module.bind(py).dict()) {
        return Ok(false);
    }
    let module = state.module.bind(py);
    for name in names {
        let (_, expected, code) = state
            .module_functions
            .iter()
            .find(|(candidate, _, _)| candidate == name)
            .unwrap_or_else(|| panic!("missing cached cookie function {name}"));
        let Some(current) = module.dict().get_item(name)? else {
            return Ok(false);
        };
        if !current.is(expected.bind(py)) || !current.getattr("__code__")?.is(code.bind(py)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn source_module_function_lookup_is_callback_free(
    py: Python<'_>,
    state: &CookieState,
    globals: &FunctionGlobalOwner,
    name: &str,
) -> PyResult<bool> {
    if !exact_builtin_string_keys(globals.globals.bind(py))
        || !exact_builtin_string_keys(globals.builtins.bind(py))
    {
        return Ok(false);
    }
    let source_module = match globals.globals.bind(py).get_item("cookies_module")? {
        Some(value) => value,
        None => match globals.builtins.bind(py).get_item("cookies_module")? {
            Some(value) => value,
            None => return Ok(false),
        },
    };
    if !source_module.is(state.module.bind(py)) {
        return Ok(false);
    }
    if !source_module.get_type().is(py.get_type::<PyModule>()) {
        return Ok(false);
    }
    let source_module = source_module.cast::<PyModule>()?;
    if !exact_builtin_string_keys(&source_module.dict()) {
        return Ok(false);
    }
    let (_, expected, code) = state
        .module_functions
        .iter()
        .find(|(candidate, _, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing cached cookie function {name}"));
    let Some(function) = source_module.dict().get_item(name)? else {
        return Ok(false);
    };
    Ok(function.is(expected.bind(py)) && function.getattr("__code__")?.is(code.bind(py)))
}

fn deepvalues_is_pristine(py: Python<'_>, state: &CookieState) -> PyResult<bool> {
    let module = state.module.bind(py);
    if !exact_builtin_string_keys(&module.dict()) {
        return Ok(false);
    }
    let Some(cookielib) = module.dict().get_item("cookielib")? else {
        return Ok(false);
    };
    if !cookielib.is(state.cookielib.bind(py)) {
        return Ok(false);
    }
    let cookielib = cookielib.cast_into::<PyModule>()?;
    if !exact_builtin_string_keys(&cookielib.dict()) {
        return Ok(false);
    }
    let Some(current) = cookielib.dict().get_item("deepvalues")? else {
        return Ok(false);
    };
    Ok(current.is(state.deepvalues.bind(py))
        && current
            .getattr("__code__")?
            .is(state.deepvalues_code.bind(py)))
}

fn inspect_methods_are_pristine(py: Python<'_>, state: &CookieState) -> PyResult<bool> {
    let jar_type = state.jar_type.bind(py);
    for (name, expected) in &state.jar_inspect_methods {
        if !method_is(jar_type, name, expected)? {
            return Ok(false);
        }
    }
    Ok(true)
}

const COOKIE_FIELDS: &[&str] = &[
    "_rest", "name", "value", "domain", "path", "secure", "expires", "discard",
];

fn type_layout_is_pristine(
    state: &CookieState,
    class: &Bound<'_, PyType>,
    expected_mro: &[Py<PyType>],
    shadowable_fields: &[&str],
) -> PyResult<bool> {
    if !mro_is(class, expected_mro)?
        || !method_is(class, "__getattribute__", &state.object_getattribute)?
    {
        return Ok(false);
    }
    for name in shadowable_fields {
        if raw_mro_type_entry(class, name)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn jar_layout_is_pristine(state: &CookieState, jar: &Bound<'_, PyAny>) -> PyResult<bool> {
    let class = jar.get_type();
    let expected_mro = if class.is(state.jar_type.bind(jar.py())) {
        &state.jar_mro
    } else if class.is(state.std_jar_type.bind(jar.py())) {
        &state.std_jar_mro
    } else {
        return Ok(false);
    };
    if !type_layout_is_pristine(state, &class, expected_mro, &["_cookies", "_policy"])? {
        return Ok(false);
    }
    let dictionary = raw_instance_dict(jar)?;
    if !exact_builtin_string_keys(&dictionary) {
        return Ok(false);
    }
    Ok(dictionary
        .get_item("_cookies")?
        .is_some_and(|value| value.is_exact_instance_of::<PyDict>()))
}

fn cookie_layout_is_pristine(state: &CookieState, cookie: &Bound<'_, PyAny>) -> PyResult<bool> {
    let class = cookie.get_type();
    Ok(class.is(state.cookie_type.bind(cookie.py()))
        && type_layout_is_pristine(state, &class, &state.cookie_mro, COOKIE_FIELDS)?)
}

fn morsel_layout_is_pristine(state: &CookieState, morsel: &Bound<'_, PyAny>) -> PyResult<bool> {
    let class = morsel.get_type();
    Ok(class.is(state.morsel_type.bind(morsel.py()))
        && mro_is(&class, &state.morsel_mro)?
        && method_is(&class, "__getattribute__", &state.object_getattribute)?
        && method_is(&class, "__getitem__", &state.morsel_getitem)?
        && raw_mro_type_entry(&class, "key")?
            .is_some_and(|value| value.is(state.morsel_key.bind(morsel.py())))
        && raw_mro_type_entry(&class, "value")?
            .is_some_and(|value| value.is(state.morsel_value.bind(morsel.py()))))
}

fn exact_string_item(value: &Bound<'_, PyAny>) -> bool {
    value.is_exact_instance_of::<pyo3::types::PyString>()
}

fn morsel_shape_is_supported(state: &CookieState, morsel: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !morsel_layout_is_pristine(state, morsel)? {
        return Ok(false);
    }
    let dictionary = morsel.cast::<PyDict>()?;
    if !exact_builtin_string_keys(dictionary) {
        return Ok(false);
    }
    for name in ["max-age", "expires", "version", "domain", "path", "comment"] {
        if !exact_string_item(&morsel.get_item(name)?) {
            return Ok(false);
        }
    }
    for name in ["secure", "httponly"] {
        let value = morsel.get_item(name)?;
        if !exact_string_item(&value) && !value.is_exact_instance_of::<pyo3::types::PyBool>() {
            return Ok(false);
        }
    }
    Ok(exact_string_item(&morsel.getattr("key")?) && exact_string_item(&morsel.getattr("value")?))
}

fn raw_required_field<'py>(
    dictionary: &Bound<'py, PyDict>,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    dictionary.get_item(name)?.ok_or_else(|| {
        pyo3::exceptions::PyAttributeError::new_err(format!(
            "exact Cookie instance is missing raw field {name}"
        ))
    })
}

fn raw_cookie_snapshot(state: &CookieState, cookie: &Bound<'_, PyAny>) -> PyResult<CookieSnapshot> {
    if !cookie_layout_is_pristine(state, cookie)? {
        return Err(PyTypeError::new_err("cookie layout is not pristine"));
    }
    let dictionary = raw_instance_dict(cookie)?;
    if !exact_builtin_string_keys(&dictionary) {
        return Err(PyTypeError::new_err(
            "cookie instance dictionary has a non-string key",
        ));
    }
    let rest = raw_required_field(&dictionary, "_rest")?.cast_into::<PyDict>()?;
    let mut rest_values = Vec::with_capacity(rest.len());
    for (key, value) in rest.iter() {
        rest_values.push((required_string(&key)?, scalar_from_python(&value)?));
    }
    Ok(CookieSnapshot {
        name: required_string(&raw_required_field(&dictionary, "name")?)?,
        value: optional_string(&raw_required_field(&dictionary, "value")?)?,
        domain: optional_string(&raw_required_field(&dictionary, "domain")?)?,
        path: optional_string(&raw_required_field(&dictionary, "path")?)?,
        secure: required_bool(&raw_required_field(&dictionary, "secure")?)?,
        expires: optional_integer(&raw_required_field(&dictionary, "expires")?)?,
        discard: required_bool(&raw_required_field(&dictionary, "discard")?)?,
        rest: rest_values,
    })
}

fn snapshot_shape_is_supported(state: &CookieState, jar: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !jar_layout_is_pristine(state, jar)? {
        return Ok(false);
    }
    let jar_dictionary = raw_instance_dict(jar)?;
    let domains = raw_required_field(&jar_dictionary, "_cookies")?.cast_into::<PyDict>()?;
    for (domain, paths) in domains.iter() {
        if !exact_string_item(&domain) || !paths.is_exact_instance_of::<PyDict>() {
            return Ok(false);
        }
        let paths = paths.cast_into::<PyDict>()?;
        for (path, names) in paths.iter() {
            if !exact_string_item(&path) || !names.is_exact_instance_of::<PyDict>() {
                return Ok(false);
            }
            let names = names.cast_into::<PyDict>()?;
            for (name, cookie) in names.iter() {
                if !exact_string_item(&name) {
                    return Ok(false);
                }
                if raw_cookie_snapshot(state, &cookie).is_err() {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn scalar_from_python(value: &Bound<'_, PyAny>) -> PyResult<CookieScalar> {
    if value.is_none() {
        Ok(CookieScalar::None)
    } else if value.is_exact_instance_of::<pyo3::types::PyBool>() {
        Ok(CookieScalar::Bool(value.extract()?))
    } else if value.is_exact_instance_of::<pyo3::types::PyInt>() {
        Ok(CookieScalar::Integer(value.extract()?))
    } else if value.is_exact_instance_of::<pyo3::types::PyString>() {
        Ok(CookieScalar::Text(value.extract()?))
    } else {
        Err(PyTypeError::new_err(
            "cookie snapshot encountered an unsupported rest value",
        ))
    }
}

fn optional_string(value: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if value.is_none() {
        Ok(None)
    } else if value.is_exact_instance_of::<pyo3::types::PyString>() {
        Ok(Some(value.extract()?))
    } else {
        Err(PyTypeError::new_err(
            "cookie snapshot requires exact string or None fields",
        ))
    }
}

fn required_string(value: &Bound<'_, PyAny>) -> PyResult<String> {
    if value.is_exact_instance_of::<pyo3::types::PyString>() {
        value.extract()
    } else {
        Err(PyTypeError::new_err(
            "cookie snapshot requires exact string fields",
        ))
    }
}

fn optional_integer(value: &Bound<'_, PyAny>) -> PyResult<Option<i64>> {
    if value.is_none() {
        Ok(None)
    } else if value.is_exact_instance_of::<pyo3::types::PyInt>() {
        Ok(Some(value.extract()?))
    } else {
        Err(PyTypeError::new_err(
            "cookie snapshot requires an exact integer or None expiry",
        ))
    }
}

fn required_bool(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    if value.is_exact_instance_of::<pyo3::types::PyBool>() {
        value.extract()
    } else {
        Err(PyTypeError::new_err(
            "cookie snapshot requires exact bool fields",
        ))
    }
}

fn live_simple_cookie_rows(py: Python<'_>, jar: &Bound<'_, PyAny>) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for cookie in jar.try_iter()? {
        let cookie = cookie?;
        let name = cookie.getattr("name")?;
        let value = cookie.getattr("value")?;
        let domain = cookie.getattr("domain")?;
        let path = cookie.getattr("path")?;
        rows.append((name, value, domain, path))?;
    }
    Ok(rows.unbind())
}

fn detailed_cookie_rows(py: Python<'_>, jar: &Bound<'_, PyAny>) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for cookie in jar.try_iter()? {
        let cookie = cookie?;
        let name = cookie.getattr("name")?;
        let value = cookie.getattr("value")?;
        let domain = cookie.getattr("domain")?;
        let path = cookie.getattr("path")?;
        let secure = cookie.getattr("secure")?;
        let expires = cookie.getattr("expires")?;
        let discard = cookie.getattr("discard")?;
        let rest = cookie.getattr("_rest")?;
        rows.append((name, value, domain, path, secure, expires, discard, rest))?;
    }
    Ok(rows.unbind())
}

fn value_or_error_tuple(
    py: Python<'_>,
    globals: &FunctionGlobalOwner,
    result: PyResult<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    match result {
        Ok(value) => Ok(value.unbind()),
        Err(original) => {
            let handled = (|| -> PyResult<Option<Py<PyAny>>> {
                let base_exception = load_live_function_global(py, globals, "BaseException")?;
                if !exception_matches(py, &original, base_exception.bind(py))? {
                    return Ok(None);
                }
                let type_function = load_live_function_global(py, globals, "type")?;
                let value = original.value(py);
                Ok(Some(
                    (
                        type_function
                            .bind(py)
                            .call1((value,))?
                            .getattr("__name__")?,
                        value.getattr("args")?,
                    )
                        .into_pyobject(py)?
                        .into_any()
                        .unbind(),
                ))
            })();
            match handled {
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(original),
                Err(error) => {
                    error.set_context(py, Some(original));
                    Err(error)
                }
            }
        }
    }
}

fn inspect_result(
    py: Python<'_>,
    jar: &Bound<'_, PyAny>,
    arguments: &Bound<'_, PyAny>,
    globals: &FunctionGlobalOwner,
) -> PyResult<Py<PyAny>> {
    let lookups = PyList::empty(py);
    for item in arguments.try_iter()? {
        let row = item?.cast_into::<PyTuple>()?;
        let name = row.get_item(0)?;
        let domain = row.get_item(1)?;
        let path = row.get_item(2)?;
        let default = row.get_item(3)?;
        let item_value = value_or_error_tuple(py, globals, jar.get_item(&name))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("domain", &domain)?;
        kwargs.set_item("path", &path)?;
        let selected_value = value_or_error_tuple(
            py,
            globals,
            jar.call_method("get", (&name, &default), Some(&kwargs)),
        )?;
        lookups.append((name, domain, path, item_value, selected_value))?;
    }
    let record = PyDict::new(py);
    record.set_item("cookies", live_simple_cookie_rows(py, jar)?)?;
    record.set_item("keys", jar.call_method0("keys")?)?;
    record.set_item("values", jar.call_method0("values")?)?;
    record.set_item("items", jar.call_method0("items")?)?;
    record.set_item("lookups", lookups)?;
    record.set_item("dict", jar.call_method0("get_dict")?)?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("domain", "a.test")?;
    kwargs.set_item("path", "/")?;
    record.set_item("a-root", jar.call_method("get_dict", (), Some(&kwargs))?)?;
    record.set_item("domains", jar.call_method0("list_domains")?)?;
    record.set_item("paths", jar.call_method0("list_paths")?)?;
    record.set_item("multiple", jar.call_method0("multiple_domains")?)?;
    Ok(record.into_any().unbind())
}

fn cookie_constructor(
    py: Python<'_>,
    state: &CookieState,
    name: &Bound<'_, PyAny>,
    value: &Bound<'_, PyAny>,
    overrides: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("version", 0)?;
    kwargs.set_item("name", name)?;
    kwargs.set_item("value", value)?;
    kwargs.set_item("port", py.None())?;
    kwargs.set_item("domain", "")?;
    kwargs.set_item("path", "/")?;
    kwargs.set_item("secure", false)?;
    kwargs.set_item("expires", py.None())?;
    kwargs.set_item("discard", true)?;
    kwargs.set_item("comment", py.None())?;
    kwargs.set_item("comment_url", py.None())?;
    let rest = PyDict::new(py);
    rest.set_item("HttpOnly", py.None())?;
    kwargs.set_item("rest", rest)?;
    kwargs.set_item("rfc2109", false)?;
    if let Some(overrides) = overrides {
        for (key, value) in overrides {
            kwargs.set_item(key, value)?;
        }
    }
    let port = kwargs.get_item("port")?.expect("default port");
    let domain = kwargs.get_item("domain")?.expect("default domain");
    let path = kwargs.get_item("path")?.expect("default path");
    kwargs.set_item("port_specified", port.is_truthy()?)?;
    kwargs.set_item("domain_specified", domain.is_truthy()?)?;
    kwargs.set_item(
        "domain_initial_dot",
        domain.extract::<String>()?.starts_with('.'),
    )?;
    kwargs.set_item("path_specified", path.is_truthy()?)?;
    Ok(load_live_cookielib_global(py, state)?
        .bind(py)
        .getattr("Cookie")?
        .call((), Some(&kwargs))?
        .unbind())
}

fn load_live_cookielib_global(py: Python<'_>, state: &CookieState) -> PyResult<Py<PyAny>> {
    let globals = state.module.bind(py).dict();
    if let Some(cookielib) = globals.get_item("cookielib")? {
        return Ok(cookielib.unbind());
    }
    if let Some(cookielib) = state
        .create_cookie_builtins
        .bind(py)
        .get_item("cookielib")?
    {
        return Ok(cookielib.unbind());
    }
    let error = PyNameError::new_err("name 'cookielib' is not defined");
    error.value(py).setattr("name", "cookielib")?;
    Err(error)
}

fn load_live_function_global(
    py: Python<'_>,
    owner: &FunctionGlobalOwner,
    name: &str,
) -> PyResult<Py<PyAny>> {
    if let Some(value) = owner.globals.bind(py).get_item(name)? {
        return Ok(value.unbind());
    }
    if let Some(value) = owner.builtins.bind(py).get_item(name)? {
        return Ok(value.unbind());
    }
    let error = PyNameError::new_err(format!("name '{name}' is not defined"));
    error.value(py).setattr("name", name)?;
    Err(error)
}

fn allowed_create_keys() -> [&'static str; 12] {
    [
        "version",
        "port",
        "domain",
        "path",
        "secure",
        "expires",
        "discard",
        "comment",
        "comment_url",
        "rest",
        "rfc2109",
        "value",
    ]
}

fn validate_create_kwargs(py: Python<'_>, kwargs: &Bound<'_, PyDict>) -> PyResult<()> {
    let allowed = allowed_create_keys();
    let keys = PySet::empty(py)?;
    for (key, _) in kwargs {
        if !allowed
            .iter()
            .any(|allowed| key.eq(*allowed).unwrap_or(false))
        {
            keys.add(key)?;
        }
    }
    if keys.is_empty() {
        return Ok(());
    }
    let values = PyList::empty(py);
    for key in keys.iter() {
        values.append(key)?;
    }
    Err(PyTypeError::new_err(format!(
        "create_cookie() got unexpected keyword arguments: {}",
        values.repr()?.to_str()?
    )))
}

fn morsel_cookie(
    py: Python<'_>,
    state: &CookieState,
    morsel: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    if !morsel.is_instance(state.morsel_type.bind(py))? {
        return Err(PyTypeError::new_err("expected a Morsel"));
    }
    let max_age = morsel.get_item("max-age")?;
    let expires_header = morsel.get_item("expires")?;
    let expires: Py<PyAny> = if max_age.is_truthy()? {
        let now: f64 = state.time_time.bind(py).call0()?.extract()?;
        let seconds: i64 = max_age.extract::<String>()?.parse().map_err(|_| {
            PyTypeError::new_err(format!(
                "max-age: {} must be integer",
                max_age.extract::<String>().unwrap_or_default()
            ))
        })?;
        ((now + seconds as f64) as i64)
            .into_pyobject(py)?
            .into_any()
            .unbind()
    } else if expires_header.is_truthy()? {
        let parsed = state
            .time_strptime
            .bind(py)
            .call1((expires_header, "%a, %d-%b-%Y %H:%M:%S GMT"))?;
        state.calendar_timegm.bind(py).call1((parsed,))?.unbind()
    } else {
        py.None()
    };
    let overrides = PyDict::new(py);
    let version = morsel.get_item("version")?;
    if version.is_truthy()? {
        overrides.set_item("version", version)?;
    } else {
        overrides.set_item("version", 0)?;
    }
    overrides.set_item("port", py.None())?;
    overrides.set_item("domain", morsel.get_item("domain")?)?;
    overrides.set_item("path", morsel.get_item("path")?)?;
    overrides.set_item("secure", morsel.get_item("secure")?.is_truthy()?)?;
    overrides.set_item("expires", expires)?;
    overrides.set_item("discard", false)?;
    overrides.set_item("comment", morsel.get_item("comment")?)?;
    overrides.set_item("comment_url", morsel.get_item("comment")?.is_truthy()?)?;
    let rest = PyDict::new(py);
    rest.set_item("HttpOnly", morsel.get_item("httponly")?)?;
    overrides.set_item("rest", rest)?;
    overrides.set_item("rfc2109", false)?;
    cookie_constructor(
        py,
        state,
        &morsel.getattr("key")?,
        &morsel.getattr("value")?,
        Some(&overrides),
    )
}

fn source_morsel_cookie(
    py: Python<'_>,
    state: &CookieState,
    globals: &FunctionGlobalOwner,
    morsel: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    if source_module_function_lookup_is_callback_free(py, state, globals, "morsel_to_cookie")? {
        return morsel_cookie(py, state, morsel);
    }
    let cookies_module = load_live_function_global(py, globals, "cookies_module")?;
    let converter = cookies_module.bind(py).getattr("morsel_to_cookie")?;
    Ok(converter.call1((morsel,))?.unbind())
}

fn base_set_cookie(
    _py: Python<'_>,
    _state: &CookieState,
    jar: &Bound<'_, PyAny>,
    cookie: &Bound<'_, PyAny>,
) -> PyResult<()> {
    jar.call_method1("set_cookie", (cookie,))?;
    Ok(())
}

fn mutate_jar(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    arguments: &Bound<'_, PyAny>,
    globals: &FunctionGlobalOwner,
) -> PyResult<Py<PyAny>> {
    let arguments = arguments.cast::<PyTuple>()?;
    let morsel = source_morsel_cookie(py, state, globals, &arguments.get_item(0)?)?;
    base_set_cookie(py, state, jar, morsel.bind(py))?;
    base_set_cookie(py, state, jar, &arguments.get_item(1)?)?;

    let overrides = PyDict::new(py);
    overrides.set_item("domain", "a.test")?;
    overrides.set_item("path", "/")?;
    jar.call_method("set", ("replace", "old"), Some(&overrides))?;
    let replacement = arguments.get_item(2)?;
    jar.call_method("set", ("replace", replacement), Some(&overrides))?;
    jar.call_method("set", ("remove", "gone"), Some(&overrides))?;
    jar.call_method("set", ("remove", py.None()), Some(&overrides))?;

    let record = PyDict::new(py);
    record.set_item("cookies", detailed_cookie_rows(py, jar)?)?;
    record.set_item("dict", jar.call_method0("get_dict")?)?;
    Ok(record.into_any().unbind())
}

fn native_bad_create(
    py: Python<'_>,
    state: &CookieState,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let arguments = arguments.cast::<PyTuple>()?;
    let kwargs = arguments.get_item(2)?.cast_into::<PyDict>()?;
    validate_create_kwargs(py, &kwargs)?;
    cookie_constructor(
        py,
        state,
        &arguments.get_item(0)?,
        &arguments.get_item(1)?,
        Some(&kwargs),
    )
}

fn source_bad_create(
    py: Python<'_>,
    state: &CookieState,
    arguments: &Bound<'_, PyAny>,
    globals: &FunctionGlobalOwner,
) -> PyResult<Py<PyAny>> {
    let arguments = arguments.cast::<PyTuple>()?;
    let kwargs = arguments.get_item(2)?.cast_into::<PyDict>()?;
    if source_module_function_lookup_is_callback_free(py, state, globals, "create_cookie")? {
        return native_bad_create(py, state, arguments.as_any());
    }
    let cookies_module = load_live_function_global(py, globals, "cookies_module")?;
    let create_cookie = cookies_module.bind(py).getattr("create_cookie")?;
    Ok(create_cookie
        .call(
            (&arguments.get_item(0)?, &arguments.get_item(1)?),
            Some(&kwargs),
        )?
        .unbind())
}

fn copy_pickle_result(
    py: Python<'_>,
    jar: &Bound<'_, PyAny>,
    globals: &FunctionGlobalOwner,
) -> PyResult<Py<PyAny>> {
    let copied = jar.call_method0("copy")?;
    let next_function = load_live_function_global(py, globals, "next")?;
    let iter_function = load_live_function_global(py, globals, "iter")?;
    let original_iterator = iter_function.bind(py).call1((jar,))?;
    let original_cookie = next_function.bind(py).call1((original_iterator,))?;
    let next_function = load_live_function_global(py, globals, "next")?;
    let iter_function = load_live_function_global(py, globals, "iter")?;
    let copied_iterator = iter_function.bind(py).call1((&copied,))?;
    let copied_cookie = next_function.bind(py).call1((copied_iterator,))?;
    let copy_cookie_object = copied_cookie.is(&original_cookie);

    copied.call_method1("set", ("copy-only", "yes"))?;
    let state_object = jar.call_method0("__getstate__")?;
    let outer_pickle = load_live_function_global(py, globals, "pickle")?;
    let loads = outer_pickle.bind(py).getattr("loads")?;
    let inner_pickle = load_live_function_global(py, globals, "pickle")?;
    let dumps = inner_pickle.bind(py).getattr("dumps")?;
    let data = dumps.call1((jar,))?;
    let restored = loads.call1((data,))?;
    let record = PyDict::new(py);
    let copied_policy = copied.call_method0("get_policy")?;
    let original_policy = jar.call_method0("get_policy")?;
    record.set_item("copy-policy", copied_policy.is(&original_policy))?;
    record.set_item("copy-cookie-object", copy_cookie_object)?;
    record.set_item(
        "copy-isolation",
        !jar.contains("copy-only")? && copied.contains("copy-only")?,
    )?;
    record.set_item("lock-omitted", !state_object.contains("_cookies_lock")?)?;
    record.set_item(
        "restored-lock-new",
        !restored
            .getattr("_cookies_lock")?
            .is(&jar.getattr("_cookies_lock")?),
    )?;
    let type_function = load_live_function_global(py, globals, "type")?;
    let restored_policy = restored.call_method0("get_policy")?;
    let restored_policy_type = type_function
        .bind(py)
        .call1((restored_policy,))?
        .getattr("__qualname__")?;
    record.set_item("restored-policy-type", restored_policy_type)?;
    record.set_item("restored", live_simple_cookie_rows(py, &restored)?)?;
    Ok(record.into_any().unbind())
}

fn exact_requests_jar_for_operation(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    Ok(jar.get_type().is(state.jar_type.bind(py)) && jar_layout_is_pristine(state, jar)?)
}

fn conversion_globals_are_pristine(
    py: Python<'_>,
    state: &CookieState,
    functions: &[&str],
) -> PyResult<bool> {
    let module = state.module.bind(py);
    if !module_functions_are_pristine(py, state, functions)? {
        return Ok(false);
    }
    let Some(cookielib) = module.dict().get_item("cookielib")? else {
        return Ok(false);
    };
    if !cookielib.is(state.cookielib.bind(py)) {
        return Ok(false);
    }
    let cookielib = cookielib.cast_into::<PyModule>()?;
    if !exact_builtin_string_keys(&cookielib.dict())
        || !cookielib
            .dict()
            .get_item("Cookie")?
            .is_some_and(|value| value.is(state.cookie_constructor.bind(py)))
    {
        return Ok(false);
    }
    if !functions.contains(&"morsel_to_cookie") {
        return Ok(true);
    }
    Ok(module_entry_is(module, "time", &state.time_module)?
        && state
            .time_module
            .bind(py)
            .getattr("time")?
            .is(state.time_time.bind(py))
        && state
            .time_module
            .bind(py)
            .getattr("strptime")?
            .is(state.time_strptime.bind(py))
        && module_entry_is(module, "calendar", &state.calendar_module)?
        && state
            .calendar_module
            .bind(py)
            .getattr("timegm")?
            .is(state.calendar_timegm.bind(py)))
}

fn mutate_arguments_are_supported(
    state: &CookieState,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !arguments.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let arguments = arguments.cast::<PyTuple>()?;
    Ok(arguments.len() == 3
        && morsel_shape_is_supported(state, &arguments.get_item(0)?)?
        && raw_cookie_snapshot(state, &arguments.get_item(1)?).is_ok()
        && exact_string_item(&arguments.get_item(2)?))
}

fn inspect_arguments_are_supported(arguments: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !arguments.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let arguments = arguments.cast::<PyTuple>()?;
    for row in arguments.iter() {
        if !row.is_exact_instance_of::<PyTuple>() {
            return Ok(false);
        }
        let row = row.cast_into::<PyTuple>()?;
        if row.len() != 4
            || !exact_string_item(&row.get_item(0)?)
            || !matches_exact_optional_string(&row.get_item(1)?)
            || !matches_exact_optional_string(&row.get_item(2)?)
            || scalar_from_python(&row.get_item(3)?).is_err()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn matches_exact_optional_string(value: &Bound<'_, PyAny>) -> bool {
    value.is_none() || exact_string_item(value)
}

fn exact_rest_shape(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !value.is_exact_instance_of::<PyDict>() {
        return Ok(false);
    }
    for (key, value) in value.cast::<PyDict>()? {
        if !exact_string_item(&key) || scalar_from_python(&value).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn create_value_is_supported(key: &str, value: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(match key {
        "version" => value.is_exact_instance_of::<pyo3::types::PyInt>(),
        "port" => {
            value.is_none()
                || exact_string_item(value)
                || value.is_exact_instance_of::<pyo3::types::PyInt>()
        }
        "domain" | "path" => exact_string_item(value),
        "secure" | "discard" | "rfc2109" => value.is_exact_instance_of::<pyo3::types::PyBool>(),
        "expires" => value.is_none() || value.is_exact_instance_of::<pyo3::types::PyInt>(),
        "comment" | "comment_url" | "value" => value.is_none() || exact_string_item(value),
        "rest" => exact_rest_shape(value)?,
        _ => scalar_from_python(value).is_ok(),
    })
}

fn bad_create_arguments_are_supported(arguments: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !arguments.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    let arguments = arguments.cast::<PyTuple>()?;
    if arguments.len() != 3
        || !exact_string_item(&arguments.get_item(0)?)
        || !exact_string_item(&arguments.get_item(1)?)
        || !arguments.get_item(2)?.is_exact_instance_of::<PyDict>()
    {
        return Ok(false);
    }
    let kwargs = arguments.get_item(2)?.cast_into::<PyDict>()?;
    for (key, value) in kwargs.iter() {
        if !exact_string_item(&key) {
            return Ok(false);
        }
        let key_name: String = key.extract()?;
        if !create_value_is_supported(&key_name, &value)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn copy_pickle_dependencies_are_pristine(py: Python<'_>, state: &CookieState) -> PyResult<bool> {
    Ok(methods_are_pristine(
        state.jar_type.bind(py),
        &state.jar_operation_methods,
        &[
            "__getattribute__",
            "__iter__",
            "__contains__",
            "set",
            "set_cookie",
            "copy",
            "update",
            "set_policy",
            "get_policy",
            "__getstate__",
            "__setstate__",
        ],
    )? && methods_are_pristine(
        state.std_jar_type.bind(py),
        &state.std_operation_methods,
        &["set_cookie", "set_policy"],
    )? && module_entry_is(state.copy_module.bind(py), "copy", &state.copy_function)?
        && module_entry_is(state.pickle_module.bind(py), "dumps", &state.pickle_dumps)?
        && module_entry_is(state.pickle_module.bind(py), "loads", &state.pickle_loads)?)
}

fn exact_lock_is_pristine(state: &CookieState, jar: &Bound<'_, PyAny>) -> PyResult<bool> {
    let dictionary = raw_instance_dict(jar)?;
    if !exact_builtin_string_keys(&dictionary) {
        return Ok(false);
    }
    Ok(dictionary
        .get_item("_cookies_lock")?
        .is_some_and(|lock| lock.get_type().is(state.rlock_type.bind(jar.py()))))
}

fn callback_free_policy_value(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    if value.is_none()
        || value.is_exact_instance_of::<pyo3::types::PyBool>()
        || value.is_exact_instance_of::<pyo3::types::PyInt>()
        || exact_string_item(value)
    {
        return Ok(true);
    }
    if !value.is_exact_instance_of::<PyTuple>() {
        return Ok(false);
    }
    Ok(value
        .cast::<PyTuple>()?
        .iter()
        .all(|item| exact_string_item(&item)))
}

fn exact_default_policy_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let jar_dictionary = raw_instance_dict(jar)?;
    if !exact_builtin_string_keys(&jar_dictionary) {
        return Ok(false);
    }
    let Some(policy) = jar_dictionary.get_item("_policy")? else {
        return Ok(false);
    };
    if !policy.get_type().is(state.default_policy_type.bind(py))
        || !method_is(
            &policy.get_type(),
            "__getattribute__",
            &state.object_getattribute,
        )?
    {
        return Ok(false);
    }
    let dictionary = raw_instance_dict(&policy)?;
    if !exact_builtin_string_keys(&dictionary) {
        return Ok(false);
    }
    for (_, value) in dictionary.iter() {
        if !callback_free_policy_value(&value)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn threading_rlock_is_pristine(py: Python<'_>, state: &CookieState) -> PyResult<bool> {
    let module = state.module.bind(py);
    if !exact_builtin_string_keys(&module.dict())
        || !module
            .dict()
            .get_item("threading")?
            .is_some_and(|value| value.is(state.threading_module.bind(py)))
    {
        return Ok(false);
    }
    let threading = state.threading_module.bind(py);
    if !exact_builtin_string_keys(&threading.dict()) {
        return Ok(false);
    }
    let Some(rlock) = threading.dict().get_item("RLock")? else {
        return Ok(false);
    };
    Ok(rlock.is(state.threading_rlock.bind(py))
        && rlock
            .getattr("__code__")?
            .is(state.threading_rlock_code.bind(py)))
}

fn empty_arguments_are_supported(arguments: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(arguments.is_exact_instance_of::<PyTuple>() && arguments.cast::<PyTuple>()?.is_empty())
}

fn cookie_operation_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !exact_requests_jar_for_operation(py, state, jar)? {
        return Ok(false);
    }
    match operation {
        "inspect" => Ok(inspect_methods_are_pristine(py, state)?
            && instance_methods_are_unshadowed(
                jar,
                &[
                    "get",
                    "_find_no_duplicates",
                    "keys",
                    "iterkeys",
                    "values",
                    "itervalues",
                    "items",
                    "iteritems",
                    "get_dict",
                    "list_domains",
                    "list_paths",
                    "multiple_domains",
                ],
            )?
            && deepvalues_is_pristine(py, state)?
            && snapshot_shape_is_supported(state, jar)?
            && inspect_arguments_are_supported(arguments)?),
        "mutate" => Ok(conversion_globals_are_pristine(
            py,
            state,
            &["create_cookie", "morsel_to_cookie", "remove_cookie_by_name"],
        )? && methods_are_pristine(
            state.jar_type.bind(py),
            &state.jar_operation_methods,
            &[
                "__getattribute__",
                "__iter__",
                "set",
                "set_cookie",
                "get_dict",
            ],
        )? && methods_are_pristine(
            state.std_jar_type.bind(py),
            &state.std_operation_methods,
            &["set_cookie", "clear"],
        )? && instance_methods_are_unshadowed(
            jar,
            &["set_cookie", "set", "clear", "get_dict"],
        )? && exact_lock_is_pristine(state, jar)?
            && deepvalues_is_pristine(py, state)?
            && snapshot_shape_is_supported(state, jar)?
            && mutate_arguments_are_supported(state, arguments)?),
        "bad-create" => Ok(
            conversion_globals_are_pristine(py, state, &["create_cookie"])?
                && snapshot_shape_is_supported(state, jar)?
                && bad_create_arguments_are_supported(arguments)?,
        ),
        "morsel" => {
            if !arguments.is_exact_instance_of::<PyTuple>() {
                return Ok(false);
            }
            let arguments = arguments.cast::<PyTuple>()?;
            Ok(arguments.len() == 1
                && conversion_globals_are_pristine(
                    py,
                    state,
                    &["create_cookie", "morsel_to_cookie"],
                )?
                && snapshot_shape_is_supported(state, jar)?
                && morsel_shape_is_supported(state, &arguments.get_item(0)?)?)
        }
        "copy-pickle" => Ok(
            conversion_globals_are_pristine(py, state, &["create_cookie"])?
                && copy_pickle_dependencies_are_pristine(py, state)?
                && instance_methods_are_unshadowed(jar, &["copy", "get_policy", "__getstate__"])?
                && exact_lock_is_pristine(state, jar)?
                && exact_default_policy_is_pristine(py, state, jar)?
                && threading_rlock_is_pristine(py, state)?
                && deepvalues_is_pristine(py, state)?
                && snapshot_shape_is_supported(state, jar)?
                && empty_arguments_are_supported(arguments)?,
        ),
        _ => Ok(false),
    }
}

#[pyfunction]
fn _cookie_jar_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    jar: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = cookie_state(py)?;
    if !matches!(
        cookie_operation_is_pristine(py, state, jar, operation, arguments),
        Ok(true)
    ) {
        return Ok(compat.call0()?.unbind());
    }
    let Some(function_globals) = exact_function_global_owner(py, compat)? else {
        return Ok(compat.call0()?.unbind());
    };
    match operation {
        "inspect" => inspect_result(py, jar, arguments, &function_globals),
        "mutate" => mutate_jar(py, state, jar, arguments, &function_globals),
        "bad-create" => source_bad_create(py, state, arguments, &function_globals),
        "morsel" => {
            let arguments = arguments.cast::<PyTuple>()?;
            let cookie =
                source_morsel_cookie(py, state, &function_globals, &arguments.get_item(0)?)?;
            let cookie = cookie.bind(py);
            Ok((
                cookie.getattr("name")?,
                cookie.getattr("value")?,
                cookie.getattr("expires")?,
                cookie.getattr("discard")?,
            )
                .into_pyobject(py)?
                .into_any()
                .unbind())
        }
        "copy-pickle" => copy_pickle_result(py, jar, &function_globals),
        _ => unreachable!("unsupported cookie operations select compatibility"),
    }
}

fn exact_bridge_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<bool> {
    let jar_type = jar.get_type();
    let exact_jar =
        jar_type.is(state.jar_type.bind(py)) || jar_type.is(state.std_jar_type.bind(py));
    if !exact_jar
        || !request.get_type().is(state.prepared_type.bind(py))
        || !jar_layout_is_pristine(state, jar)?
        || !type_layout_is_pristine(
            state,
            &request.get_type(),
            &state.prepared_mro,
            &["_cookies", "headers", "url", "method"],
        )?
        || !module_functions_are_pristine(
            py,
            state,
            if operation == "extract-header" {
                &["get_cookie_header", "extract_cookies_to_jar"]
            } else {
                &["get_cookie_header"]
            },
        )?
    {
        return Ok(false);
    }
    if !exact_builtin_string_keys(&raw_instance_dict(request)?) {
        return Ok(false);
    }
    let module = state.module.bind(py);
    if !module
        .dict()
        .get_item("MockRequest")?
        .is_some_and(|value| value.is(state.mock_request_type.bind(py)))
        || !module
            .dict()
            .get_item("MockResponse")?
            .is_some_and(|value| value.is(state.mock_response_type.bind(py)))
    {
        return Ok(false);
    }
    let iterator_is_pristine = if jar_type.is(state.jar_type.bind(py)) {
        methods_are_pristine(
            state.jar_type.bind(py),
            &state.jar_operation_methods,
            &["__getattribute__", "__iter__"],
        )?
    } else {
        methods_are_pristine(
            state.std_jar_type.bind(py),
            &state.std_operation_methods,
            &["__getattribute__", "__iter__"],
        )?
    };
    let instance_methods = if operation == "extract-header" {
        &["add_cookie_header", "extract_cookies"][..]
    } else {
        &["add_cookie_header"][..]
    };
    Ok(iterator_is_pristine
        && exact_lock_is_pristine(state, jar)?
        && exact_default_policy_is_pristine(py, state, jar)?
        && instance_methods_are_unshadowed(jar, instance_methods)?
        && method_is(
            state.std_jar_type.bind(py),
            "add_cookie_header",
            &state.std_add_header,
        )?
        && (operation != "extract-header"
            || method_is(
                state.std_jar_type.bind(py),
                "extract_cookies",
                &state.std_extract,
            )?))
}

fn cookie_header(
    py: Python<'_>,
    globals: &FunctionGlobalOwner,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(load_live_function_global(py, globals, "cookies_module")?
        .bind(py)
        .getattr("get_cookie_header")?
        .call1((jar, request))?
        .unbind())
}

fn extract_cookies(
    py: Python<'_>,
    globals: &FunctionGlobalOwner,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
) -> PyResult<()> {
    load_live_function_global(py, globals, "cookies_module")?
        .bind(py)
        .getattr("extract_cookies_to_jar")?
        .call1((jar, request, response))?;
    Ok(())
}

fn bridge_result(
    py: Python<'_>,
    globals: &FunctionGlobalOwner,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<Py<PyAny>> {
    if operation == "extract-header" {
        extract_cookies(py, globals, jar, request, response)?;
    }
    let header = cookie_header(py, globals, jar, request)?;
    let rows = live_simple_cookie_rows(py, jar)?;
    Ok((header, rows).into_pyobject(py)?.into_any().unbind())
}

#[pyfunction]
fn _cookie_bridge_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
    operation: &str,
    _audit: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = cookie_state(py)?;
    if !matches!(operation, "header" | "extract-header")
        || !matches!(
            exact_bridge_is_pristine(py, state, jar, request, operation),
            Ok(true)
        )
        || !matches!(snapshot_shape_is_supported(state, jar), Ok(true))
    {
        return Ok(compat.call0()?.unbind());
    }
    let Some(function_globals) = exact_function_global_owner(py, compat)? else {
        return Ok(compat.call0()?.unbind());
    };
    bridge_result(py, &function_globals, jar, request, response, operation)
}

#[derive(Clone, Debug)]
enum CookiePipelineAction {
    Execute(CookiePipelineStage),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResponseId(usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SnapshotId(usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeaderId(usize);

#[derive(Debug)]
enum CookiePipelineReply {
    Ack,
    Response(ResponseId),
    Snapshot(SnapshotId),
    Header(HeaderId),
    Failed,
}

impl WorkerPayload for CookiePipelineAction {}
impl WorkerPayload for CookiePipelineReply {}

#[derive(Debug)]
enum CookiePipelineOutcome {
    Complete {
        response: ResponseId,
        hook_response: ResponseId,
        snapshots: Vec<SnapshotId>,
        header: Option<HeaderId>,
    },
    HandlerFailed,
    BridgeClosed(BridgeClosed),
}

struct OriginCookiePipelineOwner {
    jar: Py<PyAny>,
    request: Py<PyAny>,
    responses: Vec<Py<PyAny>>,
    snapshots: Vec<Py<PyList>>,
    headers: Vec<Py<PyAny>>,
    hook: Py<PyAny>,
    digest: Py<PyAny>,
    audit: Py<PyAny>,
    function_globals: FunctionGlobalOwner,
    handler_error: Option<PyErr>,
    origin_only: PhantomData<Rc<()>>,
}

fn response_index(id: ResponseId, len: usize) -> Option<usize> {
    (id.0 < len).then_some(id.0)
}

fn snapshot_index(id: SnapshotId, len: usize) -> Option<usize> {
    (id.0 < len).then_some(id.0)
}

fn header_index(id: HeaderId, len: usize) -> Option<usize> {
    (id.0 < len).then_some(id.0)
}

impl OriginCookiePipelineOwner {
    fn store_response(&mut self, value: Py<PyAny>) -> ResponseId {
        let id = ResponseId(self.responses.len());
        self.responses.push(value);
        id
    }

    fn response(&self, id: ResponseId) -> PyResult<&Py<PyAny>> {
        response_index(id, self.responses.len())
            .and_then(|index| self.responses.get(index))
            .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline response ID is out of range"))
    }

    fn store_snapshot(&mut self, value: Py<PyList>) -> SnapshotId {
        let id = SnapshotId(self.snapshots.len());
        self.snapshots.push(value);
        id
    }

    fn snapshot(&self, id: SnapshotId) -> PyResult<&Py<PyList>> {
        snapshot_index(id, self.snapshots.len())
            .and_then(|index| self.snapshots.get(index))
            .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline snapshot ID is out of range"))
    }

    fn store_header(&mut self, value: Py<PyAny>) -> HeaderId {
        let id = HeaderId(self.headers.len());
        self.headers.push(value);
        id
    }

    fn header(&self, id: HeaderId) -> PyResult<&Py<PyAny>> {
        header_index(id, self.headers.len())
            .and_then(|index| self.headers.get(index))
            .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline header ID is out of range"))
    }
}

fn append_audit(owner: &OriginCookiePipelineOwner, py: Python<'_>, value: &str) -> PyResult<()> {
    owner.audit.bind(py).call_method1("append", (value,))?;
    Ok(())
}

fn execute_pipeline_action(
    py: Python<'_>,
    owner: &mut OriginCookiePipelineOwner,
    action: CookiePipelineAction,
) -> PyResult<CookiePipelineReply> {
    let CookiePipelineAction::Execute(stage) = action;
    match stage {
        CookiePipelineStage::Prepare => {
            append_audit(owner, py, "prepare")?;
            owner
                .request
                .bind(py)
                .call_method1("prepare_cookies", (owner.jar.bind(py),))?;
            Ok(CookiePipelineReply::Ack)
        }
        CookiePipelineStage::Hook => {
            append_audit(owner, py, "hook")?;
            let replacement = owner.hook.bind(py).call1((owner.responses[0].bind(py),))?;
            if replacement.is_none() {
                Ok(CookiePipelineReply::Response(ResponseId(0)))
            } else {
                let id = owner.store_response(replacement.unbind());
                Ok(CookiePipelineReply::Response(id))
            }
        }
        CookiePipelineStage::SnapshotAfterPrepare
        | CookiePipelineStage::SnapshotAfterHook
        | CookiePipelineStage::SnapshotAfterExtract
        | CookiePipelineStage::SnapshotAfterDigest
        | CookiePipelineStage::SnapshotAfterHeader => {
            let snapshot = live_simple_cookie_rows(py, owner.jar.bind(py))?;
            Ok(CookiePipelineReply::Snapshot(
                owner.store_snapshot(snapshot),
            ))
        }
        CookiePipelineStage::Extract => {
            append_audit(owner, py, "extract")?;
            let response = owner
                .responses
                .last()
                .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline lost its response"))?;
            let cookies_module =
                load_live_function_global(py, &owner.function_globals, "cookies_module")?;
            let extract = cookies_module.bind(py).getattr("extract_cookies_to_jar")?;
            let raw = response.bind(py).getattr("raw")?;
            extract.call1((owner.jar.bind(py), owner.request.bind(py), raw))?;
            Ok(CookiePipelineReply::Ack)
        }
        CookiePipelineStage::Digest => {
            append_audit(owner, py, "digest")?;
            let response = owner
                .responses
                .last()
                .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline lost its response"))?;
            let replacement = owner
                .digest
                .bind(py)
                .call1((response.bind(py), owner.jar.bind(py)))?;
            if replacement.is(response.bind(py)) {
                Ok(CookiePipelineReply::Response(ResponseId(
                    owner.responses.len() - 1,
                )))
            } else {
                let id = owner.store_response(replacement.unbind());
                Ok(CookiePipelineReply::Response(id))
            }
        }
        CookiePipelineStage::Header => {
            append_audit(owner, py, "header")?;
            let header = cookie_header(
                py,
                &owner.function_globals,
                owner.jar.bind(py),
                owner.request.bind(py),
            )?;
            Ok(CookiePipelineReply::Header(owner.store_header(header)))
        }
    }
}

async fn pipeline_worker(
    actions: crate::bridge::ActionSender<CookiePipelineAction, CookiePipelineReply>,
) -> CookiePipelineOutcome {
    let mut response = ResponseId(0);
    let mut hook_response = ResponseId(0);
    let mut snapshots = Vec::new();
    let mut header = None;
    for stage in private_pipeline_stages() {
        let reply = match actions.request(CookiePipelineAction::Execute(stage)).await {
            Ok(reply) => reply,
            Err(error) => return CookiePipelineOutcome::BridgeClosed(error),
        };
        match reply {
            CookiePipelineReply::Ack => {}
            CookiePipelineReply::Response(value) => {
                response = value;
                if stage == CookiePipelineStage::Hook {
                    hook_response = value;
                }
            }
            CookiePipelineReply::Snapshot(value) => {
                snapshots.push(value);
            }
            CookiePipelineReply::Header(value) => header = Some(value),
            CookiePipelineReply::Failed => return CookiePipelineOutcome::HandlerFailed,
        }
    }
    CookiePipelineOutcome::Complete {
        response,
        hook_response,
        snapshots,
        header,
    }
}

fn pipeline_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    Ok(
        exact_bridge_is_pristine(py, state, jar, request, "extract-header")?
            && snapshot_shape_is_supported(state, jar)?
            && response.get_type().is(state.response_type.bind(py))
            && type_layout_is_pristine(
                state,
                &response.get_type(),
                &state.response_mro,
                &["raw", "request"],
            )?
            && method_is(
                state.prepared_type.bind(py),
                "prepare_cookies",
                &state.prepared_cookies,
            )?
            && instance_methods_are_unshadowed(request, &["prepare_cookies"])?,
    )
}

#[pyfunction]
#[allow(clippy::too_many_arguments)] // Private parity seam mirrors the Python interaction tuple.
fn _cookie_pipeline_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    jar: Py<PyAny>,
    request: Py<PyAny>,
    response: Py<PyAny>,
    hook: Py<PyAny>,
    digest: Py<PyAny>,
    audit: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = cookie_state(py)?;
    if !matches!(
        pipeline_is_pristine(py, state, jar.bind(py), request.bind(py), response.bind(py)),
        Ok(true)
    ) {
        return Ok(compat.call0()?.unbind());
    }
    let Some(function_globals) = exact_function_global_owner(py, compat)? else {
        return Ok(compat.call0()?.unbind());
    };
    let owner = OriginCookiePipelineOwner {
        jar,
        request,
        responses: vec![response],
        snapshots: Vec::new(),
        headers: Vec::new(),
        hook,
        digest,
        audit,
        function_globals,
        handler_error: None,
        origin_only: PhantomData,
    };
    let (outcome, mut owner) =
        run_with_owned_actions(py, owner, pipeline_worker, |py, action, owner| {
            match execute_pipeline_action(py, owner, action) {
                Ok(reply) => reply,
                Err(error) => {
                    if owner.handler_error.is_none() {
                        owner.handler_error = Some(error);
                    }
                    CookiePipelineReply::Failed
                }
            }
        })?;
    if let Some(error) = owner.handler_error.take() {
        return Err(error);
    }
    match outcome {
        CookiePipelineOutcome::Complete {
            response,
            hook_response,
            snapshots,
            header,
        } => {
            let final_response = owner.response(response)?;
            let hook_response = owner.response(hook_response)?;
            let header = header.ok_or_else(|| {
                PyRuntimeError::new_err("cookie pipeline lost its header object ID")
            })?;
            let header = owner.header(header)?;
            let replacement = final_response.is(hook_response.bind(py));
            let request_jar = owner
                .request
                .bind(py)
                .getattr("_cookies")?
                .is(owner.jar.bind(py));
            let record = PyDict::new(py);
            record.set_item("replacement", replacement)?;
            record.set_item("request-jar", request_jar)?;
            record.set_item("header", header.bind(py))?;
            record.set_item("cookies", live_simple_cookie_rows(py, owner.jar.bind(py))?)?;
            let list_function = load_live_function_global(py, &owner.function_globals, "list")?;
            record.set_item(
                "audit",
                list_function.bind(py).call1((owner.audit.bind(py),))?,
            )?;
            let history = PyList::empty(py);
            for snapshot in snapshots {
                history.append(owner.snapshot(snapshot)?.bind(py))?;
            }
            record.set_item("snapshots", history)?;
            Ok(record.into_any().unbind())
        }
        CookiePipelineOutcome::HandlerFailed => Err(PyRuntimeError::new_err(
            "cookie pipeline action failed without preserving its Python exception",
        )),
        CookiePipelineOutcome::BridgeClosed(error) => {
            Err(PyRuntimeError::new_err(error.to_string()))
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    COOKIE_STATE.get_or_try_init(module.py(), || initialize_cookie_state(module.py()))?;
    module.add_function(wrap_pyfunction!(_cookie_jar_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_cookie_bridge_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_cookie_pipeline_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CookiePipelineAction, CookiePipelineReply, HeaderId, OriginCookiePipelineOwner, ResponseId,
        SnapshotId, header_index, response_index, snapshot_index,
    };
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    trait AmbiguousIfSend<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

    trait AmbiguousIfSync<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

    #[test]
    fn cookie_pipeline_payloads_are_worker_safe() {
        assert_worker_payload::<CookiePipelineAction>();
        assert_worker_payload::<CookiePipelineReply>();
    }

    #[test]
    fn cookie_origin_owner_is_not_a_worker_payload() {
        let _ = <OriginCookiePipelineOwner as AmbiguousIfWorkerPayload<_>>::marker;
    }

    #[test]
    fn cookie_origin_owner_is_not_send_or_sync() {
        let _ = <OriginCookiePipelineOwner as AmbiguousIfSend<_>>::marker;
        let _ = <OriginCookiePipelineOwner as AmbiguousIfSync<_>>::marker;
    }

    #[test]
    fn cookie_pipeline_ids_are_category_specific_and_range_checked() {
        assert_eq!(response_index(ResponseId(0), 1), Some(0));
        assert_eq!(response_index(ResponseId(1), 1), None);
        assert_eq!(snapshot_index(SnapshotId(0), 1), Some(0));
        assert_eq!(snapshot_index(SnapshotId(1), 1), None);
        assert_eq!(header_index(HeaderId(0), 1), Some(0));
        assert_eq!(header_index(HeaderId(1), 1), None);
    }
}

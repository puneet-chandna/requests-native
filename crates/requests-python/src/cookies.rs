use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyDict, PyList, PyModule, PySet, PyTuple, PyType};
use pyo3::wrap_pyfunction;
use requests::cookies::{
    CookiePipelineStage, CookieScalar, CookieSnapshot, JarSnapshot, LookupQuery, LookupValue,
    SnapshotDelta, private_pipeline_stages,
};

use crate::bridge::{BridgeClosed, WorkerPayload};
use crate::runtime::run_with_owned_actions;

struct CookieState {
    module: Py<PyModule>,
    jar_type: Py<PyType>,
    std_jar_type: Py<PyType>,
    default_policy_type: Py<PyType>,
    cookie_type: Py<PyType>,
    cookielib: Py<PyAny>,
    cookie_constructor: Py<PyAny>,
    morsel_type: Py<PyType>,
    morsel_mro: Vec<Py<PyType>>,
    morsel_getitem: Py<PyAny>,
    morsel_key: Py<PyAny>,
    morsel_value: Py<PyAny>,
    prepared_type: Py<PyType>,
    response_type: Py<PyType>,
    jar_getstate: Py<PyAny>,
    jar_inspect_methods: Vec<(&'static str, Py<PyAny>)>,
    jar_operation_methods: Vec<(&'static str, Py<PyAny>)>,
    std_operation_methods: Vec<(&'static str, Py<PyAny>)>,
    module_functions: Vec<(&'static str, Py<PyAny>, Py<PyAny>)>,
    std_set_cookie: Py<PyAny>,
    std_clear: Py<PyAny>,
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
    object_getattribute: Py<PyAny>,
    cookie_mro: Vec<Py<PyType>>,
    jar_mro: Vec<Py<PyType>>,
    std_jar_mro: Vec<Py<PyType>>,
    prepared_mro: Vec<Py<PyType>>,
    response_mro: Vec<Py<PyType>>,
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
        jar_getstate: jar_type.getattr("__getstate__")?.unbind(),
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
        std_set_cookie: std_jar_type.getattr("set_cookie")?.unbind(),
        std_clear: std_jar_type.getattr("clear")?.unbind(),
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
        morsel_type: morsel_type.unbind(),
        prepared_type: prepared_type.unbind(),
        response_type: response_type.unbind(),
    })
}

fn cookie_state(py: Python<'_>) -> PyResult<&CookieState> {
    COOKIE_STATE.get_or_try_init(py, || initialize_cookie_state(py))
}

fn method_is(ty: &Bound<'_, PyType>, name: &str, expected: &Py<PyAny>) -> PyResult<bool> {
    Ok(ty.getattr(name)?.is(expected.bind(ty.py())))
}

fn module_entry_is(
    module: &Bound<'_, PyModule>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
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

fn raw_instance_dict<'py>(value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    Ok(value.getattr("__dict__")?.cast_into::<PyDict>()?)
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
        || !class
            .getattr("__getattribute__")?
            .is(state.object_getattribute.bind(class.py()))
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
    Ok(
        type_layout_is_pristine(state, &class, expected_mro, &["_cookies", "_policy"])?
            && raw_instance_dict(jar)?
                .get_item("_cookies")?
                .is_some_and(|value| value.is_exact_instance_of::<PyDict>()),
    )
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
        && class
            .getattr("__getattribute__")?
            .is(state.object_getattribute.bind(morsel.py()))
        && class
            .getattr("__getitem__")?
            .is(state.morsel_getitem.bind(morsel.py()))
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
    for (_, paths) in domains.iter() {
        let paths = paths.cast_into::<PyDict>()?;
        for (_, names) in paths.iter() {
            let names = names.cast_into::<PyDict>()?;
            for (_, cookie) in names.iter() {
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

fn cookie_snapshot(cookie: &Bound<'_, PyAny>) -> PyResult<CookieSnapshot> {
    let rest = cookie.getattr("_rest")?.cast_into::<PyDict>()?;
    let mut rest_values = Vec::with_capacity(rest.len());
    for (key, value) in rest.iter() {
        rest_values.push((required_string(&key)?, scalar_from_python(&value)?));
    }
    Ok(CookieSnapshot {
        name: required_string(&cookie.getattr("name")?)?,
        value: optional_string(&cookie.getattr("value")?)?,
        domain: optional_string(&cookie.getattr("domain")?)?,
        path: optional_string(&cookie.getattr("path")?)?,
        secure: required_bool(&cookie.getattr("secure")?)?,
        expires: optional_integer(&cookie.getattr("expires")?)?,
        discard: required_bool(&cookie.getattr("discard")?)?,
        rest: rest_values,
    })
}

fn snapshot_jar(jar: &Bound<'_, PyAny>) -> PyResult<JarSnapshot> {
    let mut cookies = Vec::new();
    for item in jar.try_iter()? {
        let cookie = item?;
        cookies.push(cookie_snapshot(&cookie)?);
    }
    Ok(JarSnapshot::new(cookies))
}

fn simple_cookie_rows(py: Python<'_>, snapshot: &JarSnapshot) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for cookie in snapshot.cookies() {
        rows.append((
            cookie.name.as_str(),
            cookie.value.as_deref(),
            cookie.domain.as_deref(),
            cookie.path.as_deref(),
        ))?;
    }
    Ok(rows.unbind())
}

fn detailed_cookie_rows(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for cookie in jar.try_iter()? {
        let cookie = cookie?;
        if !cookie_layout_is_pristine(state, &cookie)? {
            return Err(PyTypeError::new_err(
                "cookie layout changed after native mutation committed",
            ));
        }
        let dictionary = raw_instance_dict(&cookie)?;
        rows.append((
            raw_required_field(&dictionary, "name")?,
            raw_required_field(&dictionary, "value")?,
            raw_required_field(&dictionary, "domain")?,
            raw_required_field(&dictionary, "path")?,
            raw_required_field(&dictionary, "secure")?,
            raw_required_field(&dictionary, "expires")?,
            raw_required_field(&dictionary, "discard")?,
            raw_required_field(&dictionary, "_rest")?,
        ))?;
    }
    Ok(rows.unbind())
}

fn snapshot_history_rows(py: Python<'_>, snapshots: &[JarSnapshot]) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for snapshot in snapshots {
        rows.append(simple_cookie_rows(py, snapshot)?)?;
    }
    Ok(rows.unbind())
}

fn dict_from_pairs(py: Python<'_>, values: Vec<(String, Option<String>)>) -> PyResult<Py<PyDict>> {
    let dictionary = PyDict::new(py);
    for (name, value) in values {
        dictionary.set_item(name, value)?;
    }
    Ok(dictionary.unbind())
}

fn lookup_result(
    py: Python<'_>,
    query: &LookupQuery,
    value: LookupValue,
    item: bool,
) -> PyResult<Py<PyAny>> {
    match value {
        LookupValue::Value(value) => Ok(value.into_pyobject(py)?.into_any().unbind()),
        LookupValue::Missing if !item => {
            Ok(query.default.clone().into_pyobject(py)?.into_any().unbind())
        }
        LookupValue::Missing => {
            let name_repr = query
                .name
                .as_str()
                .into_pyobject(py)?
                .repr()?
                .extract::<String>()?;
            let message = format!("name={name_repr}, domain=None, path=None");
            Ok(("KeyError", (message,))
                .into_pyobject(py)?
                .into_any()
                .unbind())
        }
        LookupValue::Conflict => {
            let name_repr = query
                .name
                .as_str()
                .into_pyobject(py)?
                .repr()?
                .extract::<String>()?;
            let message = format!("There are multiple cookies with name, {name_repr}");
            Ok(("CookieConflictError", (message,))
                .into_pyobject(py)?
                .into_any()
                .unbind())
        }
    }
}

fn inspect_result(
    py: Python<'_>,
    snapshot: &JarSnapshot,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let record = PyDict::new(py);
    record.set_item("cookies", simple_cookie_rows(py, snapshot)?)?;
    record.set_item("keys", snapshot.keys())?;
    record.set_item("values", snapshot.values())?;
    record.set_item("items", snapshot.items())?;
    let lookups = PyList::empty(py);
    for item in arguments.try_iter()? {
        let row = item?.cast_into::<PyTuple>()?;
        let query = LookupQuery {
            name: row.get_item(0)?.extract()?,
            domain: row.get_item(1)?.extract()?,
            path: row.get_item(2)?.extract()?,
            default: row.get_item(3)?.extract()?,
        };
        let selected = snapshot.find_no_duplicates(&query);
        let item_query = LookupQuery {
            name: query.name.clone(),
            domain: None,
            path: None,
            default: query.default.clone(),
        };
        let item_value = lookup_result(
            py,
            &item_query,
            snapshot.find_no_duplicates(&item_query),
            true,
        )?;
        let selected_value = lookup_result(py, &query, selected, false)?;
        lookups.append((
            query.name,
            query.domain,
            query.path,
            item_value,
            selected_value,
        ))?;
    }
    record.set_item("lookups", lookups)?;
    record.set_item("dict", dict_from_pairs(py, snapshot.get_dict(None, None))?)?;
    record.set_item(
        "a-root",
        dict_from_pairs(py, snapshot.get_dict(Some("a.test"), Some("/")))?,
    )?;
    record.set_item("domains", snapshot.domains())?;
    record.set_item("paths", snapshot.paths())?;
    record.set_item("multiple", snapshot.multiple_domains())?;
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
    Ok(state.cookie_type.bind(py).call((), Some(&kwargs))?.unbind())
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

fn base_set_cookie(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    cookie: &Bound<'_, PyAny>,
) -> PyResult<()> {
    if let Some(value) = optional_string(&cookie.getattr("value")?)?
        && value.starts_with('"')
        && value.ends_with('"')
    {
        cookie.setattr("value", value.replace("\\\"", ""))?;
    }
    state.std_set_cookie.bind(py).call1((jar, cookie))?;
    Ok(())
}

fn remove_cookie(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    name: &str,
    domain: Option<&str>,
    path: Option<&str>,
) -> PyResult<()> {
    let snapshot = snapshot_jar(jar)?;
    for cookie in snapshot.cookies() {
        if cookie.name != name
            || domain.is_some_and(|value| cookie.domain.as_deref() != Some(value))
            || path.is_some_and(|value| cookie.path.as_deref() != Some(value))
        {
            continue;
        }
        state.std_clear.bind(py).call1((
            jar,
            cookie.domain.as_deref(),
            cookie.path.as_deref(),
            cookie.name.as_str(),
        ))?;
    }
    Ok(())
}

fn mutate_jar(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let arguments = arguments.cast::<PyTuple>()?;
    let morsel = morsel_cookie(py, state, &arguments.get_item(0)?)?;
    base_set_cookie(py, state, jar, morsel.bind(py))?;
    base_set_cookie(py, state, jar, &arguments.get_item(1)?)?;

    let overrides = PyDict::new(py);
    overrides.set_item("domain", "a.test")?;
    overrides.set_item("path", "/")?;
    let replace_name = "replace".into_pyobject(py)?;
    let old = "old".into_pyobject(py)?;
    let old_cookie = cookie_constructor(
        py,
        state,
        replace_name.as_any(),
        old.as_any(),
        Some(&overrides),
    )?;
    base_set_cookie(py, state, jar, old_cookie.bind(py))?;
    let replacement = arguments.get_item(2)?;
    let replacement_cookie = cookie_constructor(
        py,
        state,
        replace_name.as_any(),
        &replacement,
        Some(&overrides),
    )?;
    base_set_cookie(py, state, jar, replacement_cookie.bind(py))?;

    let remove_name = "remove".into_pyobject(py)?;
    let gone = "gone".into_pyobject(py)?;
    let remove_cookie_value = cookie_constructor(
        py,
        state,
        remove_name.as_any(),
        gone.as_any(),
        Some(&overrides),
    )?;
    base_set_cookie(py, state, jar, remove_cookie_value.bind(py))?;
    remove_cookie(py, state, jar, "remove", Some("a.test"), Some("/"))?;

    let record = PyDict::new(py);
    record.set_item("cookies", detailed_cookie_rows(py, state, jar)?)?;
    record.set_item("dict", jar.call_method0("get_dict")?)?;
    Ok(record.into_any().unbind())
}

fn bad_create(
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

fn copy_pickle_result(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let copied = state.jar_type.bind(py).call0()?;
    let policy = jar.getattr("_policy")?;
    copied.call_method1("set_policy", (&policy,))?;
    let originals: Vec<Py<PyAny>> = jar
        .try_iter()?
        .map(|item| item.map(Bound::unbind))
        .collect::<PyResult<_>>()?;
    for cookie in &originals {
        let copied_cookie = state.copy_function.bind(py).call1((cookie.bind(py),))?;
        base_set_cookie(py, state, &copied, &copied_cookie)?;
    }
    let copied_first =
        copied.try_iter()?.next().transpose()?.ok_or_else(|| {
            PyRuntimeError::new_err("cookie copy unexpectedly lost its first cookie")
        })?;
    let copy_cookie_object = copied_first.is(originals[0].bind(py));

    let copy_only_name = "copy-only".into_pyobject(py)?;
    let copy_only_value = "yes".into_pyobject(py)?;
    let copy_only = cookie_constructor(
        py,
        state,
        copy_only_name.as_any(),
        copy_only_value.as_any(),
        None,
    )?;
    base_set_cookie(py, state, &copied, copy_only.bind(py))?;
    let original_after = snapshot_jar(jar)?;
    let copied_after = snapshot_jar(&copied)?;

    let state_dict = state
        .jar_getstate
        .bind(py)
        .call1((jar,))?
        .cast_into::<PyDict>()?;
    let data = state.pickle_dumps.bind(py).call1((jar,))?;
    let restored = state.pickle_loads.bind(py).call1((data,))?;
    let restored_snapshot = snapshot_jar(&restored)?;
    let restored_policy_type = restored.getattr("_policy")?.get_type().qualname()?;

    let record = PyDict::new(py);
    record.set_item("copy-policy", copied.getattr("_policy")?.is(&policy))?;
    record.set_item("copy-cookie-object", copy_cookie_object)?;
    record.set_item(
        "copy-isolation",
        !original_after
            .cookies()
            .iter()
            .any(|cookie| cookie.name == "copy-only")
            && copied_after
                .cookies()
                .iter()
                .any(|cookie| cookie.name == "copy-only"),
    )?;
    record.set_item("lock-omitted", !state_dict.contains("_cookies_lock")?)?;
    record.set_item(
        "restored-lock-new",
        !restored
            .getattr("_cookies_lock")?
            .is(&jar.getattr("_cookies_lock")?),
    )?;
    record.set_item("restored-policy-type", restored_policy_type)?;
    record.set_item("restored", simple_cookie_rows(py, &restored_snapshot)?)?;
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
    if !module_functions_are_pristine(py, state, functions)?
        || !module_entry_is(module, "cookielib", &state.cookielib)?
        || !state
            .cookielib
            .bind(py)
            .getattr("Cookie")?
            .is(state.cookie_constructor.bind(py))
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
    let Ok(arguments) = arguments.cast::<PyTuple>() else {
        return Ok(false);
    };
    Ok(arguments.len() == 3
        && morsel_shape_is_supported(state, &arguments.get_item(0)?)?
        && raw_cookie_snapshot(state, &arguments.get_item(1)?).is_ok()
        && exact_string_item(&arguments.get_item(2)?))
}

fn bad_create_arguments_are_supported(arguments: &Bound<'_, PyAny>) -> PyResult<bool> {
    let Ok(arguments) = arguments.cast::<PyTuple>() else {
        return Ok(false);
    };
    if arguments.len() != 3
        || !exact_string_item(&arguments.get_item(0)?)
        || !exact_string_item(&arguments.get_item(1)?)
        || !arguments.get_item(2)?.is_exact_instance_of::<PyDict>()
    {
        return Ok(false);
    }
    let kwargs = arguments.get_item(2)?.cast_into::<PyDict>()?;
    Ok(kwargs.iter().all(|(key, _)| exact_string_item(&key)))
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
        "inspect" => Ok(
            inspect_methods_are_pristine(py, state)? && snapshot_shape_is_supported(state, jar)?
        ),
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
        )? && snapshot_shape_is_supported(state, jar)?
            && mutate_arguments_are_supported(state, arguments)?),
        "bad-create" => Ok(
            conversion_globals_are_pristine(py, state, &["create_cookie"])?
                && snapshot_shape_is_supported(state, jar)?
                && bad_create_arguments_are_supported(arguments)?,
        ),
        "morsel" => {
            let Ok(arguments) = arguments.cast::<PyTuple>() else {
                return Ok(false);
            };
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
                && snapshot_shape_is_supported(state, jar)?,
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
    match operation {
        "inspect" => inspect_result(py, &snapshot_jar(jar)?, arguments),
        "mutate" => mutate_jar(py, state, jar, arguments),
        "bad-create" => bad_create(py, state, arguments),
        "morsel" => {
            let arguments = arguments.cast::<PyTuple>()?;
            let cookie = morsel_cookie(py, state, &arguments.get_item(0)?)?;
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
        "copy-pickle" => copy_pickle_result(py, state, jar),
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
    let policy = jar.getattr("_policy")?;
    Ok(iterator_is_pristine
        && policy.get_type().is(state.default_policy_type.bind(py))
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
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let wrapper = state.mock_request_type.bind(py).call1((request,))?;
    state.std_add_header.bind(py).call1((jar, &wrapper))?;
    let headers = wrapper.call_method0("get_new_headers")?;
    Ok(headers.call_method1("get", ("Cookie",))?.unbind())
}

fn extract_cookies(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let has_original = response.hasattr("_original_response")?;
    if !has_original {
        return Ok(());
    }
    let original = response.getattr("_original_response")?;
    if !original.is_truthy()? {
        return Ok(());
    }
    let request_wrapper = state.mock_request_type.bind(py).call1((request,))?;
    let response_wrapper = state
        .mock_response_type
        .bind(py)
        .call1((original.getattr("msg")?,))?;
    state
        .std_extract
        .bind(py)
        .call1((jar, response_wrapper, request_wrapper))?;
    Ok(())
}

fn bridge_result(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<Py<PyAny>> {
    if operation == "extract-header" {
        let before = snapshot_jar(jar)?;
        extract_cookies(py, state, jar, request, response)?;
        let after = snapshot_jar(jar)?;
        let _delta = SnapshotDelta::new(before, after);
    }
    let header = cookie_header(py, state, jar, request)?;
    let snapshot = snapshot_jar(jar)?;
    Ok((header, simple_cookie_rows(py, &snapshot)?)
        .into_pyobject(py)?
        .into_any()
        .unbind())
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
    bridge_result(py, state, jar, request, response, operation)
}

#[derive(Clone, Debug)]
enum CookiePipelineAction {
    Execute(CookiePipelineStage),
}

#[derive(Debug)]
enum CookiePipelineReply {
    Ack,
    Response(usize),
    Snapshot(JarSnapshot),
    Header(Option<String>),
    Failed,
}

impl WorkerPayload for CookiePipelineAction {}
impl WorkerPayload for CookiePipelineReply {}

#[derive(Debug)]
enum CookiePipelineOutcome {
    Complete {
        response: usize,
        hook_response: usize,
        snapshot: JarSnapshot,
        snapshots: Vec<JarSnapshot>,
        header: Option<String>,
    },
    HandlerFailed,
    BridgeClosed(BridgeClosed),
}

struct OriginCookiePipelineOwner {
    jar: Py<PyAny>,
    request: Py<PyAny>,
    responses: Vec<Py<PyAny>>,
    hook: Py<PyAny>,
    digest: Py<PyAny>,
    audit: Py<PyAny>,
    handler_error: Option<PyErr>,
}

fn append_audit(owner: &OriginCookiePipelineOwner, py: Python<'_>, value: &str) -> PyResult<()> {
    owner.audit.bind(py).call_method1("append", (value,))?;
    Ok(())
}

fn execute_pipeline_action(
    py: Python<'_>,
    state: &CookieState,
    owner: &mut OriginCookiePipelineOwner,
    action: CookiePipelineAction,
) -> PyResult<CookiePipelineReply> {
    let CookiePipelineAction::Execute(stage) = action;
    match stage {
        CookiePipelineStage::Prepare => {
            append_audit(owner, py, "prepare")?;
            state
                .prepared_cookies
                .bind(py)
                .call1((owner.request.bind(py), owner.jar.bind(py)))?;
            Ok(CookiePipelineReply::Ack)
        }
        CookiePipelineStage::Hook => {
            append_audit(owner, py, "hook")?;
            let replacement = owner.hook.bind(py).call1((owner.responses[0].bind(py),))?;
            if replacement.is_none() {
                Ok(CookiePipelineReply::Response(0))
            } else {
                let id = owner.responses.len();
                owner.responses.push(replacement.unbind());
                Ok(CookiePipelineReply::Response(id))
            }
        }
        CookiePipelineStage::SnapshotAfterPrepare
        | CookiePipelineStage::SnapshotAfterHook
        | CookiePipelineStage::SnapshotAfterExtract
        | CookiePipelineStage::SnapshotAfterDigest
        | CookiePipelineStage::SnapshotAfterHeader => Ok(CookiePipelineReply::Snapshot(
            snapshot_jar(owner.jar.bind(py))?,
        )),
        CookiePipelineStage::Extract => {
            append_audit(owner, py, "extract")?;
            let response = owner
                .responses
                .last()
                .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline lost its response"))?;
            extract_cookies(
                py,
                state,
                owner.jar.bind(py),
                owner.request.bind(py),
                &response.bind(py).getattr("raw")?,
            )?;
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
                Ok(CookiePipelineReply::Response(owner.responses.len() - 1))
            } else {
                let id = owner.responses.len();
                owner.responses.push(replacement.unbind());
                Ok(CookiePipelineReply::Response(id))
            }
        }
        CookiePipelineStage::Header => {
            append_audit(owner, py, "header")?;
            let header = cookie_header(py, state, owner.jar.bind(py), owner.request.bind(py))?;
            Ok(CookiePipelineReply::Header(header.bind(py).extract()?))
        }
    }
}

async fn pipeline_worker(
    actions: crate::bridge::ActionSender<CookiePipelineAction, CookiePipelineReply>,
) -> CookiePipelineOutcome {
    let mut response = 0;
    let mut hook_response = 0;
    let mut snapshot = JarSnapshot::new(Vec::new());
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
                snapshot = value.clone();
                snapshots.push(value);
            }
            CookiePipelineReply::Header(value) => header = value,
            CookiePipelineReply::Failed => return CookiePipelineOutcome::HandlerFailed,
        }
    }
    CookiePipelineOutcome::Complete {
        response,
        hook_response,
        snapshot,
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
            )?,
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
    let owner = OriginCookiePipelineOwner {
        jar,
        request,
        responses: vec![response],
        hook,
        digest,
        audit,
        handler_error: None,
    };
    let (outcome, mut owner) =
        run_with_owned_actions(py, owner, pipeline_worker, |py, action, owner| {
            match execute_pipeline_action(py, state, owner, action) {
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
            snapshot,
            snapshots,
            header,
        } => {
            let final_response = owner.responses.get(response).ok_or_else(|| {
                PyRuntimeError::new_err("cookie pipeline lost its final response")
            })?;
            let hook_response = owner
                .responses
                .get(hook_response)
                .ok_or_else(|| PyRuntimeError::new_err("cookie pipeline lost its hook response"))?;
            let replacement = final_response.is(hook_response.bind(py));
            let request_jar = owner
                .request
                .bind(py)
                .getattr("_cookies")?
                .is(owner.jar.bind(py));
            let record = PyDict::new(py);
            record.set_item("replacement", replacement)?;
            record.set_item("request-jar", request_jar)?;
            record.set_item("header", header)?;
            record.set_item("cookies", simple_cookie_rows(py, &snapshot)?)?;
            record.set_item("audit", owner.audit.bind(py).call_method0("copy")?)?;
            record.set_item("snapshots", snapshot_history_rows(py, &snapshots)?)?;
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
    use super::{CookiePipelineAction, CookiePipelineReply, OriginCookiePipelineOwner};
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    #[test]
    fn cookie_pipeline_payloads_are_worker_safe() {
        assert_worker_payload::<CookiePipelineAction>();
        assert_worker_payload::<CookiePipelineReply>();
    }

    #[test]
    fn cookie_origin_owner_is_not_a_worker_payload() {
        let _ = <OriginCookiePipelineOwner as AmbiguousIfWorkerPayload<_>>::marker;
    }
}

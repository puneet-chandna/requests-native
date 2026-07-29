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
use crate::runtime::run_with_actions;

struct CookieState {
    module: Py<PyModule>,
    jar_type: Py<PyType>,
    std_jar_type: Py<PyType>,
    default_policy_type: Py<PyType>,
    cookie_type: Py<PyType>,
    morsel_type: Py<PyType>,
    prepared_type: Py<PyType>,
    response_type: Py<PyType>,
    create_cookie: Py<PyAny>,
    create_cookie_code: Py<PyAny>,
    morsel_to_cookie: Py<PyAny>,
    morsel_to_cookie_code: Py<PyAny>,
    jar_set_cookie: Py<PyAny>,
    jar_copy: Py<PyAny>,
    jar_getstate: Py<PyAny>,
    jar_setstate: Py<PyAny>,
    jar_get_policy: Py<PyAny>,
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
    pickle_dumps: Py<PyAny>,
    pickle_loads: Py<PyAny>,
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
    let morsel_type = PyModule::import(py, "http.cookies")?
        .getattr("Morsel")?
        .cast_into::<PyType>()?;
    let models = PyModule::import(py, "requests.models")?;
    let prepared_type = models.getattr("PreparedRequest")?.cast_into::<PyType>()?;
    let response_type = models.getattr("Response")?.cast_into::<PyType>()?;
    let create_cookie = module.getattr("create_cookie")?;
    let morsel_to_cookie = module.getattr("morsel_to_cookie")?;
    let time_module = module.getattr("time")?;
    let calendar_module = module.getattr("calendar")?;
    let copy_module = PyModule::import(py, "copy")?;
    let pickle = PyModule::import(py, "pickle")?;
    Ok(CookieState {
        module: module.clone().unbind(),
        jar_set_cookie: jar_type.getattr("set_cookie")?.unbind(),
        jar_copy: jar_type.getattr("copy")?.unbind(),
        jar_getstate: jar_type.getattr("__getstate__")?.unbind(),
        jar_setstate: jar_type.getattr("__setstate__")?.unbind(),
        jar_get_policy: jar_type.getattr("get_policy")?.unbind(),
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
        create_cookie_code: create_cookie.getattr("__code__")?.unbind(),
        create_cookie: create_cookie.unbind(),
        morsel_to_cookie_code: morsel_to_cookie.getattr("__code__")?.unbind(),
        morsel_to_cookie: morsel_to_cookie.unbind(),
        time_time: time_module.getattr("time")?.unbind(),
        time_strptime: time_module.getattr("strptime")?.unbind(),
        time_module: time_module.unbind(),
        calendar_timegm: calendar_module.getattr("timegm")?.unbind(),
        calendar_module: calendar_module.unbind(),
        copy_function: copy_module.getattr("copy")?.unbind(),
        pickle_dumps: pickle.getattr("dumps")?.unbind(),
        pickle_loads: pickle.getattr("loads")?.unbind(),
        jar_type: jar_type.unbind(),
        std_jar_type: std_jar_type.unbind(),
        default_policy_type: default_policy_type.unbind(),
        cookie_type: cookie_type.unbind(),
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
    Ok(module.getattr(name)?.is(expected.bind(module.py())))
}

fn cookie_module_is_pristine(py: Python<'_>, state: &CookieState) -> PyResult<bool> {
    let module = state.module.bind(py);
    let create_cookie = module.getattr("create_cookie")?;
    let morsel_to_cookie = module.getattr("morsel_to_cookie")?;
    Ok(create_cookie.is(state.create_cookie.bind(py))
        && create_cookie
            .getattr("__code__")?
            .is(state.create_cookie_code.bind(py))
        && morsel_to_cookie.is(state.morsel_to_cookie.bind(py))
        && morsel_to_cookie
            .getattr("__code__")?
            .is(state.morsel_to_cookie_code.bind(py))
        && module_entry_is(module, "time", &state.time_module)?
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

fn exact_requests_jar_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let jar_type = state.jar_type.bind(py);
    Ok(jar.get_type().is(jar_type)
        && cookie_module_is_pristine(py, state)?
        && method_is(jar_type, "set_cookie", &state.jar_set_cookie)?
        && method_is(jar_type, "copy", &state.jar_copy)?
        && method_is(jar_type, "__getstate__", &state.jar_getstate)?
        && method_is(jar_type, "__setstate__", &state.jar_setstate)?
        && method_is(jar_type, "get_policy", &state.jar_get_policy)?)
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

fn snapshot_jar(jar: &Bound<'_, PyAny>) -> PyResult<JarSnapshot> {
    let mut cookies = Vec::new();
    for item in jar.try_iter()? {
        let cookie = item?;
        let rest = cookie.getattr("_rest")?.cast_into::<PyDict>()?;
        let mut rest_values = Vec::with_capacity(rest.len());
        for (key, value) in rest.iter() {
            rest_values.push((key.extract()?, scalar_from_python(&value)?));
        }
        cookies.push(CookieSnapshot {
            name: cookie.getattr("name")?.extract()?,
            value: optional_string(&cookie.getattr("value")?)?,
            domain: optional_string(&cookie.getattr("domain")?)?,
            path: optional_string(&cookie.getattr("path")?)?,
            secure: cookie.getattr("secure")?.extract()?,
            expires: cookie.getattr("expires")?.extract()?,
            discard: cookie.getattr("discard")?.extract()?,
            rest: rest_values,
        });
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

fn scalar_to_python(py: Python<'_>, scalar: &CookieScalar) -> PyResult<Py<PyAny>> {
    match scalar {
        CookieScalar::None => Ok(py.None()),
        CookieScalar::Bool(value) => Ok(value.into_pyobject(py)?.to_owned().into_any().unbind()),
        CookieScalar::Integer(value) => Ok(value.into_pyobject(py)?.to_owned().into_any().unbind()),
        CookieScalar::Text(value) => Ok(value.into_pyobject(py)?.to_owned().into_any().unbind()),
    }
}

fn detailed_cookie_rows(py: Python<'_>, snapshot: &JarSnapshot) -> PyResult<Py<PyList>> {
    let rows = PyList::empty(py);
    for cookie in snapshot.cookies() {
        let rest = PyDict::new(py);
        for (name, value) in &cookie.rest {
            rest.set_item(name, scalar_to_python(py, value)?)?;
        }
        rows.append((
            cookie.name.as_str(),
            cookie.value.as_deref(),
            cookie.domain.as_deref(),
            cookie.path.as_deref(),
            cookie.secure,
            cookie.expires,
            cookie.discard,
            rest,
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

    let snapshot = snapshot_jar(jar)?;
    let record = PyDict::new(py);
    record.set_item("cookies", detailed_cookie_rows(py, &snapshot)?)?;
    record.set_item("dict", dict_from_pairs(py, snapshot.get_dict(None, None))?)?;
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

#[pyfunction]
fn _cookie_jar_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    jar: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = cookie_state(py)?;
    if !exact_requests_jar_is_pristine(py, state, jar)? {
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
        _ => Ok(compat.call0()?.unbind()),
    }
}

fn exact_bridge_is_pristine(
    py: Python<'_>,
    state: &CookieState,
    jar: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let jar_type = jar.get_type();
    let exact_jar =
        jar_type.is(state.jar_type.bind(py)) || jar_type.is(state.std_jar_type.bind(py));
    if !exact_jar
        || !request.get_type().is(state.prepared_type.bind(py))
        || !cookie_module_is_pristine(py, state)?
    {
        return Ok(false);
    }
    let policy = jar.getattr("_policy")?;
    Ok(policy.get_type().is(state.default_policy_type.bind(py))
        && method_is(
            state.std_jar_type.bind(py),
            "add_cookie_header",
            &state.std_add_header,
        )?
        && method_is(
            state.std_jar_type.bind(py),
            "extract_cookies",
            &state.std_extract,
        )?)
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
        || !exact_bridge_is_pristine(py, state, jar, request)?
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
            CookiePipelineReply::Response(value) => response = value,
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
    Ok(exact_requests_jar_is_pristine(py, state, jar)?
        && request.get_type().is(state.prepared_type.bind(py))
        && response.get_type().is(state.response_type.bind(py))
        && method_is(
            state.prepared_type.bind(py),
            "prepare_cookies",
            &state.prepared_cookies,
        )?)
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
    if !pipeline_is_pristine(py, state, jar.bind(py), request.bind(py), response.bind(py))? {
        return Ok(compat.call0()?.unbind());
    }
    let mut owner = OriginCookiePipelineOwner {
        jar,
        request,
        responses: vec![response],
        hook,
        digest,
        audit,
    };
    let mut handler_error = None;
    let outcome = run_with_actions(
        py,
        pipeline_worker,
        |py, action| match execute_pipeline_action(py, state, &mut owner, action) {
            Ok(reply) => reply,
            Err(error) => {
                if handler_error.is_none() {
                    handler_error = Some(error);
                }
                CookiePipelineReply::Failed
            }
        },
    )?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    match outcome {
        CookiePipelineOutcome::Complete {
            response,
            snapshot,
            snapshots,
            header,
        } => {
            let final_response = owner.responses.get(response).ok_or_else(|| {
                PyRuntimeError::new_err("cookie pipeline lost its final response")
            })?;
            let replacement = owner.responses.get(1).map_or_else(
                || final_response.is(owner.responses[0].bind(py)),
                |hooked| final_response.is(hooked.bind(py)),
            );
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

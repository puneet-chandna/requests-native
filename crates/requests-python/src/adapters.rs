use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyModule, PyString, PyTuple,
};
use pyo3::wrap_pyfunction;
use requests::adapters::AdapterPool;
use requests::retry::{
    BackoffPolicy, MethodSet, RetryCount, RetryHistory, RetryPolicy, RetryReason, RetryState,
    StatusSet,
};
use requests::{
    BodySource, CertificateSource, ErrorKind, HeaderMap, HeaderName, HeaderValue, Identity, Method,
    Proxy, Timeout, TlsConfig, Uri,
};

struct RetrySnapshot {
    version: String,
    policy: RetryPolicy,
    history: Vec<HistorySnapshot>,
    retry_after_max: Option<u64>,
}

struct HistorySnapshot {
    method: String,
    url: String,
    status: Option<u16>,
    redirect_location: Option<String>,
}

struct RetryStateGuard {
    retry_type: Py<PyAny>,
    retry_module: Py<PyAny>,
    methods: Vec<(String, Py<PyAny>)>,
}

static RETRY_STATE: PyOnceLock<RetryStateGuard> = PyOnceLock::new();

struct AdapterState {
    adapters_module: Py<PyAny>,
    adapter_type: Py<PyAny>,
    prepared_request_type: Py<PyAny>,
    methods: Vec<(String, Py<PyAny>)>,
    globals: Vec<(String, Py<PyAny>)>,
}

static ADAPTER_STATE: PyOnceLock<AdapterState> = PyOnceLock::new();

struct SideEntry {
    weak_adapter: Py<PyAny>,
    poolmanager: Py<PyAny>,
    pools: HashMap<String, Arc<AdapterPool>>,
}

static ADAPTER_POOLS: OnceLock<Mutex<HashMap<usize, SideEntry>>> = OnceLock::new();

#[pyclass(module = "requests._requests_rust", unsendable)]
struct NativeAdapterRaw {
    body: Option<requests::blocking::ResponseBody>,
    status: u16,
    reason: String,
    headers: Py<PyAny>,
    closed: bool,
}

#[pyclass(module = "requests._requests_rust", unsendable)]
struct NativeAdapterStream {
    raw: Py<NativeAdapterRaw>,
    amount: usize,
    done: bool,
}

#[pyfunction]
fn _select_proxy_trial(
    py: Python<'_>,
    url: &str,
    proxies: &Bound<'_, PyAny>,
    trust_env: bool,
) -> PyResult<Py<PyAny>> {
    let utils = PyModule::import(py, "requests.utils")?;
    let request = PyModule::import(py, "requests.models")?
        .getattr("PreparedRequest")?
        .call0()?;
    request.setattr("url", url)?;
    let resolved = utils
        .getattr("resolve_proxies")?
        .call1((&request, proxies, trust_env))?;
    let selected = utils.getattr("select_proxy")?.call1((url, &resolved))?;
    Ok(PyTuple::new(py, [resolved, selected])?.into_any().unbind())
}

#[pyfunction]
fn _retry_policy_snapshot_trial(py: Python<'_>, retry: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let record = PyDict::new(py);
    let snapshot = match retry_snapshot(py, retry)? {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            record.set_item("eligible", false)?;
            record.set_item("reason", reason)?;
            return Ok(record.into_any().unbind());
        }
    };
    record.set_item("eligible", true)?;
    record.set_item("version", &snapshot.version)?;
    record.set_item("total", counter_value(py, snapshot.policy.total)?)?;
    record.set_item("connect", counter_value(py, snapshot.policy.connect)?)?;
    record.set_item("read", counter_value(py, snapshot.policy.read)?)?;
    record.set_item("status", counter_value(py, snapshot.policy.status)?)?;
    record.set_item("redirect", counter_value(py, snapshot.policy.redirect)?)?;
    record.set_item("other", counter_value(py, snapshot.policy.other)?)?;
    match &snapshot.policy.allowed_methods {
        Some(methods) => {
            let mut values = methods_for_record(methods);
            values.sort();
            record.set_item("allowed_methods", values)?;
        }
        None => record.set_item("allowed_methods", py.None())?,
    }
    let mut statuses = statuses_for_record(&snapshot.policy.status_forcelist);
    statuses.sort_unstable();
    record.set_item("status_forcelist", statuses)?;
    record.set_item("backoff_factor", snapshot.policy.backoff.factor)?;
    record.set_item("backoff_max", snapshot.policy.backoff.maximum)?;
    record.set_item("backoff_jitter", snapshot.policy.backoff.jitter)?;
    record.set_item(
        "respect_retry_after_header",
        snapshot.policy.respect_retry_after,
    )?;
    record.set_item("raise_on_status", snapshot.policy.raise_on_status)?;
    record.set_item("raise_on_redirect", snapshot.policy.raise_on_redirect)?;
    record.set_item("retry_after_max", snapshot.retry_after_max)?;
    let history = PyList::empty(py);
    for item in snapshot.history {
        let row = PyDict::new(py);
        row.set_item("method", item.method)?;
        row.set_item("url", item.url)?;
        row.set_item("status", item.status)?;
        row.set_item("redirect_location", item.redirect_location)?;
        history.append(row)?;
    }
    record.set_item("history", history)?;
    Ok(record.into_any().unbind())
}

fn retry_snapshot(
    py: Python<'_>,
    retry: &Bound<'_, PyAny>,
) -> PyResult<Result<RetrySnapshot, String>> {
    let guard = retry_state(py)?;
    let retry_module = PyModule::import(py, "urllib3.util.retry")?;
    let retry_type = guard.retry_type.bind(py);
    if !retry_module.as_any().is(guard.retry_module.bind(py))
        || !retry_module.getattr("Retry")?.is(retry_type)
        || !retry.get_type().as_any().is(retry_type)
        || guard.methods.iter().any(|(name, original)| {
            retry_type.getattr(name.as_str()).is_err_and(|_| true)
                || retry_type
                    .getattr(name.as_str())
                    .is_ok_and(|current| !current.is(original.bind(py)))
        })
    {
        return Ok(Err(
            "Retry must have the exact urllib3.util.retry.Retry type".to_owned(),
        ));
    }
    let version = PyModule::import(py, "urllib3")?
        .getattr("__version__")?
        .extract::<String>()?;
    let Some(total) = retry_count(retry, "total")? else {
        return Ok(Err(
            "total is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let Some(connect) = retry_count(retry, "connect")? else {
        return Ok(Err(
            "connect is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let Some(read) = retry_count(retry, "read")? else {
        return Ok(Err(
            "read is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let Some(status) = retry_count(retry, "status")? else {
        return Ok(Err(
            "status is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let Some(redirect) = retry_count(retry, "redirect")? else {
        return Ok(Err(
            "redirect is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let Some(other) = retry_count(retry, "other")? else {
        return Ok(Err(
            "other is not None, bool, or a nonnegative int".to_owned()
        ));
    };
    let methods_name = if retry.hasattr("allowed_methods")? {
        "allowed_methods"
    } else {
        "method_whitelist"
    };
    let Some(allowed_methods) = string_set(retry.getattr(methods_name)?)? else {
        return Ok(Err(format!(
            "{methods_name} is not None or an iterable of exact strings"
        )));
    };
    let Some(status_forcelist) = status_set(retry.getattr("status_forcelist")?)? else {
        return Ok(Err(
            "status_forcelist is not an iterable of status integers".to_owned(),
        ));
    };
    let Some(backoff_factor) = nonnegative_float(retry.getattr("backoff_factor")?)? else {
        return Ok(Err("backoff_factor is not a nonnegative number".to_owned()));
    };
    let backoff_max = if retry.hasattr("backoff_max")? {
        nonnegative_float(retry.getattr("backoff_max")?)?
    } else {
        nonnegative_float(retry_type.getattr("DEFAULT_BACKOFF_MAX")?)?
    };
    let Some(backoff_max) = backoff_max else {
        return Ok(Err("backoff_max is not a nonnegative number".to_owned()));
    };
    let backoff_jitter = if retry.hasattr("backoff_jitter")? {
        let Some(jitter) = nonnegative_float(retry.getattr("backoff_jitter")?)? else {
            return Ok(Err("backoff_jitter is not a nonnegative number".to_owned()));
        };
        jitter
    } else {
        0.0
    };
    let Some(respect_retry_after) = exact_bool(retry.getattr("respect_retry_after_header")?)?
    else {
        return Ok(Err(
            "respect_retry_after_header is not an exact bool".to_owned()
        ));
    };
    let Some(raise_on_status) = exact_bool(retry.getattr("raise_on_status")?)? else {
        return Ok(Err("raise_on_status is not an exact bool".to_owned()));
    };
    let Some(raise_on_redirect) = exact_bool(retry.getattr("raise_on_redirect")?)? else {
        return Ok(Err("raise_on_redirect is not an exact bool".to_owned()));
    };
    let retry_after_max = if retry.hasattr("retry_after_max")? {
        let value = retry.getattr("retry_after_max")?;
        if !value.is_exact_instance_of::<PyInt>() || value.is_exact_instance_of::<PyBool>() {
            return Ok(Err("retry_after_max is not a nonnegative int".to_owned()));
        }
        match value.extract::<i64>() {
            Ok(value) if value >= 0 => Some(value as u64),
            _ => return Ok(Err("retry_after_max is not a nonnegative int".to_owned())),
        }
    } else {
        None
    };
    let history = match history_snapshot(retry.getattr("history")?)? {
        Ok(history) => history,
        Err(reason) => return Ok(Err(reason)),
    };

    Ok(Ok(RetrySnapshot {
        version,
        policy: RetryPolicy {
            total,
            connect,
            read,
            status,
            redirect,
            other,
            allowed_methods: allowed_methods.map(MethodSet::new),
            status_forcelist: StatusSet::new(status_forcelist),
            backoff: BackoffPolicy {
                factor: backoff_factor,
                maximum: Some(backoff_max),
                jitter: backoff_jitter,
            },
            respect_retry_after,
            raise_on_status,
            raise_on_redirect,
        },
        history,
        retry_after_max,
    }))
}

fn initialize_retry_state(py: Python<'_>) -> PyResult<RetryStateGuard> {
    let retry_module = PyModule::import(py, "urllib3.util.retry")?;
    let retry_type = retry_module.getattr("Retry")?;
    let methods = [
        "increment",
        "is_retry",
        "get_retry_after",
        "get_backoff_time",
    ]
    .into_iter()
    .map(|name| {
        retry_type
            .getattr(name)
            .map(|value| (name.to_owned(), value.unbind()))
    })
    .collect::<PyResult<Vec<_>>>()?;
    Ok(RetryStateGuard {
        retry_type: retry_type.unbind(),
        retry_module: retry_module.into_any().unbind(),
        methods,
    })
}

fn retry_state(py: Python<'_>) -> PyResult<&RetryStateGuard> {
    RETRY_STATE.get_or_try_init(py, || initialize_retry_state(py))
}

fn retry_count(retry: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<RetryCount>> {
    let value = retry.getattr(name)?;
    if value.is_none() {
        return Ok(Some(RetryCount::Unlimited));
    }
    if value.is_exact_instance_of::<PyBool>() {
        return value.extract::<bool>().map(RetryCount::Boolean).map(Some);
    }
    if !value.is_exact_instance_of::<PyInt>() {
        return Ok(None);
    }
    Ok(match value.extract::<i64>() {
        Ok(value) if value >= 0 => u32::try_from(value).ok().map(RetryCount::Limited),
        _ => None,
    })
}

fn exact_bool(value: Bound<'_, PyAny>) -> PyResult<Option<bool>> {
    if !value.is_exact_instance_of::<PyBool>() {
        return Ok(None);
    }
    value.extract::<bool>().map(Some)
}

fn nonnegative_float(value: Bound<'_, PyAny>) -> PyResult<Option<f64>> {
    if value.is_exact_instance_of::<PyBool>() {
        return Ok(None);
    }
    let Ok(value) = value.extract::<f64>() else {
        return Ok(None);
    };
    Ok((value.is_finite() && value >= 0.0).then_some(value))
}

fn string_set(value: Bound<'_, PyAny>) -> PyResult<Option<Option<Vec<String>>>> {
    if value.is_none() {
        return Ok(Some(None));
    }
    let Ok(iterator) = value.try_iter() else {
        return Ok(None);
    };
    let mut methods = Vec::new();
    for item in iterator {
        let item = item?;
        if !item.is_exact_instance_of::<PyString>() {
            return Ok(None);
        }
        methods.push(item.extract::<String>()?);
    }
    Ok(Some(Some(methods)))
}

fn status_set(value: Bound<'_, PyAny>) -> PyResult<Option<Vec<u16>>> {
    let Ok(iterator) = value.try_iter() else {
        return Ok(None);
    };
    let mut statuses = Vec::new();
    for item in iterator {
        let item = item?;
        if !item.is_exact_instance_of::<PyInt>() || item.is_exact_instance_of::<PyBool>() {
            return Ok(None);
        }
        let Ok(status) = item.extract::<u16>() else {
            return Ok(None);
        };
        statuses.push(status);
    }
    Ok(Some(statuses))
}

fn history_snapshot(value: Bound<'_, PyAny>) -> PyResult<Result<Vec<HistorySnapshot>, String>> {
    let Ok(iterator) = value.try_iter() else {
        return Ok(Err("history is not iterable".to_owned()));
    };
    let mut history = Vec::new();
    for item in iterator {
        let item = item?;
        if !item.getattr("error")?.is_none() {
            return Ok(Err("history contains a Python error object".to_owned()));
        }
        let method = item.getattr("method")?.extract::<String>()?;
        let url = item.getattr("url")?.extract::<String>()?;
        let status = item.getattr("status")?;
        let status = if status.is_none() {
            None
        } else {
            Some(status.extract::<u16>()?)
        };
        let redirect = item.getattr("redirect_location")?;
        let redirect_location = if redirect.is_none() {
            None
        } else {
            Some(redirect.extract::<String>()?)
        };
        history.push(HistorySnapshot {
            method,
            url,
            status,
            redirect_location,
        });
    }
    Ok(Ok(history))
}

fn counter_value(py: Python<'_>, count: RetryCount) -> PyResult<Py<PyAny>> {
    match count {
        RetryCount::Unlimited => Ok(py.None()),
        RetryCount::Boolean(value) => Ok(value.into_pyobject(py)?.to_owned().into_any().unbind()),
        RetryCount::Limited(value) => Ok(value.into_pyobject(py)?.into_any().unbind()),
    }
}

fn methods_for_record(methods: &MethodSet) -> Vec<String> {
    methods.iter().map(str::to_owned).collect()
}

fn statuses_for_record(statuses: &StatusSet) -> Vec<u16> {
    statuses.iter().collect()
}

struct NativeSendInput {
    method: Method,
    method_name: String,
    url: String,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    timeout: Timeout,
    tls: TlsConfig,
    proxy: Option<Proxy>,
    selected_proxy: Option<String>,
    pool_key: String,
    pool_maxsize: usize,
    retry: RetrySnapshot,
}

fn initialize_adapter_state(py: Python<'_>) -> PyResult<AdapterState> {
    let adapters = PyModule::import(py, "requests.adapters")?;
    let adapter_type = adapters.getattr("HTTPAdapter")?;
    let prepared_request_type =
        PyModule::import(py, "requests.models")?.getattr("PreparedRequest")?;
    let methods = [
        "send",
        "close",
        "build_response",
        "get_connection_with_tls_context",
        "cert_verify",
        "request_url",
        "add_headers",
        "proxy_headers",
        "proxy_manager_for",
        "build_connection_pool_key_attributes",
    ]
    .into_iter()
    .map(|name| {
        adapter_type
            .getattr(name)
            .map(|value| (name.to_owned(), value.unbind()))
    })
    .collect::<PyResult<Vec<_>>>()?;
    let globals = [
        "PoolManager",
        "proxy_from_url",
        "TimeoutSauce",
        "parse_url",
        "Retry",
        "_basic_auth_str",
        "extract_cookies_to_jar",
        "Response",
        "CaseInsensitiveDict",
        "get_auth_from_url",
        "get_encoding_from_headers",
        "prepend_scheme_if_needed",
        "select_proxy",
        "urldefragauth",
    ]
    .into_iter()
    .map(|name| {
        adapters
            .getattr(name)
            .map(|value| (name.to_owned(), value.unbind()))
    })
    .collect::<PyResult<Vec<_>>>()?;
    Ok(AdapterState {
        adapters_module: adapters.into_any().unbind(),
        adapter_type: adapter_type.unbind(),
        prepared_request_type: prepared_request_type.unbind(),
        methods,
        globals,
    })
}

fn adapter_state(py: Python<'_>) -> PyResult<&AdapterState> {
    ADAPTER_STATE.get_or_try_init(py, || initialize_adapter_state(py))
}

fn adapter_identity_is_pristine(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let state = adapter_state(py)?;
    let adapter_type = state.adapter_type.bind(py);
    if !adapter.get_type().as_any().is(adapter_type)
        || !request
            .get_type()
            .as_any()
            .is(state.prepared_request_type.bind(py))
    {
        return Ok(false);
    }
    for (name, original) in &state.methods {
        if !adapter_type.getattr(name.as_str())?.is(original.bind(py)) {
            return Ok(false);
        }
    }
    let module = state.adapters_module.bind(py);
    for (name, original) in &state.globals {
        if !module.getattr(name.as_str())?.is(original.bind(py)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn exact_usize(value: Bound<'_, PyAny>) -> Option<usize> {
    if !value.is_exact_instance_of::<PyInt>() || value.is_exact_instance_of::<PyBool>() {
        return None;
    }
    value.extract::<usize>().ok()
}

fn duration_value(value: &Bound<'_, PyAny>) -> PyResult<Option<Option<Duration>>> {
    if value.is_none() {
        return Ok(Some(None));
    }
    if value.is_exact_instance_of::<PyBool>()
        || (!value.is_exact_instance_of::<PyInt>() && !value.is_exact_instance_of::<PyFloat>())
    {
        return Ok(None);
    }
    let Ok(seconds) = value.extract::<f64>() else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Ok(None);
    }
    Ok(Some(Some(Duration::from_secs_f64(seconds))))
}

fn timeout_value(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Option<Timeout>> {
    if value.is_none()
        || value.is_exact_instance_of::<PyInt>()
        || value.is_exact_instance_of::<PyFloat>()
    {
        let Some(duration) = duration_value(value)? else {
            return Ok(None);
        };
        return Ok(Some(Timeout {
            connect: duration,
            read: duration,
            total: None,
        }));
    }
    if value.is_exact_instance_of::<PyTuple>() {
        let tuple = value.cast::<PyTuple>()?;
        if tuple.len() != 2 {
            return Ok(None);
        }
        let Some(connect) = duration_value(&tuple.get_item(0)?)? else {
            return Ok(None);
        };
        let Some(read) = duration_value(&tuple.get_item(1)?)? else {
            return Ok(None);
        };
        return Ok(Some(Timeout {
            connect,
            read,
            total: None,
        }));
    }
    let timeout_type = adapter_state(py)?
        .adapters_module
        .bind(py)
        .getattr("TimeoutSauce")?;
    if !value.get_type().as_any().is(&timeout_type) {
        return Ok(None);
    }
    let connect = value.getattr("connect_timeout")?;
    let read = value.getattr("read_timeout")?;
    let Some(connect) = duration_value(&connect)? else {
        return Ok(None);
    };
    let Some(read) = duration_value(&read)? else {
        return Ok(None);
    };
    Ok(Some(Timeout {
        connect,
        read,
        total: None,
    }))
}

fn tls_value(verify: &Bound<'_, PyAny>, cert: &Bound<'_, PyAny>) -> PyResult<Option<TlsConfig>> {
    let roots = if verify.is_exact_instance_of::<PyBool>() {
        if verify.extract::<bool>()? {
            CertificateSource::Platform
        } else {
            CertificateSource::Disabled
        }
    } else if verify.is_exact_instance_of::<PyString>() {
        let path = PathBuf::from(verify.extract::<String>()?);
        if path.is_dir() {
            CertificateSource::PemDirectory(path)
        } else {
            CertificateSource::PemBundle(path)
        }
    } else {
        return Ok(None);
    };
    let identity = if cert.is_none() {
        None
    } else if cert.is_exact_instance_of::<PyString>() {
        Some(Identity {
            certificate_chain: PathBuf::from(cert.extract::<String>()?),
            private_key: None,
        })
    } else if cert.is_exact_instance_of::<PyTuple>() {
        let tuple = cert.cast::<PyTuple>()?;
        if tuple.len() != 2
            || !tuple.get_item(0)?.is_exact_instance_of::<PyString>()
            || !tuple.get_item(1)?.is_exact_instance_of::<PyString>()
        {
            return Ok(None);
        }
        Some(Identity {
            certificate_chain: PathBuf::from(tuple.get_item(0)?.extract::<String>()?),
            private_key: Some(PathBuf::from(tuple.get_item(1)?.extract::<String>()?)),
        })
    } else {
        return Ok(None);
    };
    Ok(Some(TlsConfig { roots, identity }))
}

fn proxy_value(
    py: Python<'_>,
    request_url: &str,
    proxies: &Bound<'_, PyAny>,
) -> PyResult<Option<(Option<Proxy>, Option<String>)>> {
    if !proxies.is_none() && !proxies.is_exact_instance_of::<PyDict>() {
        return Ok(None);
    }
    let module = adapter_state(py)?.adapters_module.bind(py);
    let selected = module
        .getattr("select_proxy")?
        .call1((request_url, proxies))?;
    if selected.is_none() {
        return Ok(Some((None, None)));
    }
    if !selected.is_exact_instance_of::<PyString>() {
        return Ok(None);
    }
    let selected = module
        .getattr("prepend_scheme_if_needed")?
        .call1((selected, "http"))?
        .extract::<String>()?;
    let uri = match Uri::from_str(&selected) {
        Ok(uri) => uri,
        Err(_) => return Ok(None),
    };
    let Some(scheme) = uri.scheme_str().map(str::to_ascii_lowercase) else {
        return Ok(None);
    };
    let proxy = match scheme.as_str() {
        "http" => Proxy::Http(uri),
        "https" => Proxy::Https(uri),
        "socks4" | "socks4a" => Proxy::Socks4(uri),
        "socks5" => Proxy::Socks5 {
            uri,
            remote_dns: false,
        },
        "socks5h" => Proxy::Socks5 {
            uri,
            remote_dns: true,
        },
        _ => return Ok(None),
    };
    Ok(Some((Some(proxy), Some(selected))))
}

fn request_headers(request: &Bound<'_, PyAny>) -> PyResult<Option<HeaderMap>> {
    let headers = request.getattr("headers")?;
    let items = headers.call_method0("items")?;
    let mut native = HeaderMap::new();
    for item in items.try_iter()? {
        let item = item?;
        let pair = item.cast::<PyTuple>()?;
        if pair.len() != 2
            || !pair.get_item(0)?.is_exact_instance_of::<PyString>()
            || !pair.get_item(1)?.is_exact_instance_of::<PyString>()
        {
            return Ok(None);
        }
        let name = match HeaderName::from_bytes(pair.get_item(0)?.extract::<String>()?.as_bytes()) {
            Ok(name) => name,
            Err(_) => return Ok(None),
        };
        let value = match HeaderValue::from_str(&pair.get_item(1)?.extract::<String>()?) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        native.append(name, value);
    }
    Ok(Some(native))
}

fn request_body(request: &Bound<'_, PyAny>) -> PyResult<Option<Option<Vec<u8>>>> {
    let body = request.getattr("body")?;
    if body.is_none() {
        return Ok(Some(None));
    }
    if body.is_exact_instance_of::<PyBytes>() {
        return Ok(Some(Some(body.cast::<PyBytes>()?.as_bytes().to_vec())));
    }
    Ok(None)
}

fn native_send_input(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    timeout: &Bound<'_, PyAny>,
    verify: &Bound<'_, PyAny>,
    cert: &Bound<'_, PyAny>,
    proxies: &Bound<'_, PyAny>,
) -> PyResult<Result<NativeSendInput, String>> {
    if !adapter_identity_is_pristine(py, adapter, request)? {
        return Ok(Err("adapter identities are not pristine".to_owned()));
    }
    let config = adapter.getattr("config")?;
    if !config.is_exact_instance_of::<PyDict>() || !config.is_empty()? {
        return Ok(Err("adapter config is not an empty exact dict".to_owned()));
    }
    let Some(pool_maxsize) = exact_usize(adapter.getattr("_pool_maxsize")?) else {
        return Ok(Err("pool maximum is unsupported".to_owned()));
    };
    if exact_usize(adapter.getattr("_pool_connections")?).is_none()
        || exact_bool(adapter.getattr("_pool_block")?)?.is_none()
    {
        return Ok(Err("pool settings are unsupported".to_owned()));
    }
    let retry = match retry_snapshot(py, &adapter.getattr("max_retries")?)? {
        Ok(retry) => retry,
        Err(reason) => return Ok(Err(reason)),
    };
    let method_name = match request.getattr("method")?.extract::<String>() {
        Ok(method) => method,
        Err(_) => return Ok(Err("request method is unsupported".to_owned())),
    };
    let method = match Method::from_bytes(method_name.as_bytes()) {
        Ok(method) => method,
        Err(_) => return Ok(Err("request method is unsupported".to_owned())),
    };
    let url = match request.getattr("url")?.extract::<String>() {
        Ok(url) => url,
        Err(_) => return Ok(Err("request URL is unsupported".to_owned())),
    };
    let Some(headers) = request_headers(request)? else {
        return Ok(Err("request headers are unsupported".to_owned()));
    };
    let Some(body) = request_body(request)? else {
        return Ok(Err("request body is not proven replayable".to_owned()));
    };
    let Some(timeout) = timeout_value(py, timeout)? else {
        return Ok(Err("timeout is unsupported".to_owned()));
    };
    let Some(tls) = tls_value(verify, cert)? else {
        return Ok(Err("TLS settings are unsupported".to_owned()));
    };
    let Some((proxy, selected_proxy)) = proxy_value(py, &url, proxies)? else {
        return Ok(Err("proxy settings are unsupported".to_owned()));
    };
    let pool_key = format!("{proxy:?}|{tls:?}|{timeout:?}|{pool_maxsize}");
    Ok(Ok(NativeSendInput {
        method,
        method_name,
        url,
        headers,
        body,
        timeout,
        tls,
        proxy,
        selected_proxy,
        pool_key,
        pool_maxsize,
        retry,
    }))
}

fn adapter_id(py: Python<'_>, adapter: &Bound<'_, PyAny>) -> PyResult<usize> {
    PyModule::import(py, "builtins")?
        .getattr("id")?
        .call1((adapter,))?
        .extract()
}

fn reap_adapter_pools(py: Python<'_>, table: &mut HashMap<usize, SideEntry>) -> PyResult<()> {
    let mut dead = Vec::new();
    for (identity, entry) in table.iter() {
        if entry.weak_adapter.bind(py).call0()?.is_none() {
            dead.push(*identity);
        }
    }
    for identity in dead {
        table.remove(&identity);
    }
    Ok(())
}

fn adapter_pool(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    input: &NativeSendInput,
) -> PyResult<Result<Arc<AdapterPool>, String>> {
    let identity = adapter_id(py, adapter)?;
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    reap_adapter_pools(py, &mut table)?;
    let poolmanager = adapter.getattr("poolmanager")?;
    if let Some(entry) = table.get_mut(&identity) {
        let referent = entry.weak_adapter.bind(py).call0()?;
        if !referent.is(adapter) || !entry.poolmanager.bind(py).is(&poolmanager) {
            return Ok(Err("visible pool manager identity changed".to_owned()));
        }
        if let Some(pool) = entry.pools.get(&input.pool_key) {
            return Ok(Ok(Arc::clone(pool)));
        }
        let pool = Arc::new(
            AdapterPool::new(
                input.pool_maxsize,
                input.proxy.clone(),
                input.tls.clone(),
                input.timeout,
            )
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
        );
        entry
            .pools
            .insert(input.pool_key.clone(), Arc::clone(&pool));
        return Ok(Ok(pool));
    }
    let weak_adapter = PyModule::import(py, "weakref")?
        .getattr("ref")?
        .call1((adapter,))?
        .unbind();
    let pool = Arc::new(
        AdapterPool::new(
            input.pool_maxsize,
            input.proxy.clone(),
            input.tls.clone(),
            input.timeout,
        )
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
    );
    table.insert(
        identity,
        SideEntry {
            weak_adapter,
            poolmanager: poolmanager.unbind(),
            pools: HashMap::from([(input.pool_key.clone(), Arc::clone(&pool))]),
        },
    );
    Ok(Ok(pool))
}

fn core_history(snapshot: &[HistorySnapshot]) -> Vec<RetryHistory> {
    snapshot
        .iter()
        .map(|item| RetryHistory {
            reason: match (item.status, item.redirect_location.as_ref()) {
                (Some(status), Some(_)) => RetryReason::Redirect { status },
                (Some(status), None) => RetryReason::Status { status },
                (None, _) => RetryReason::Other,
            },
            method: item.method.clone(),
            url: item.url.clone(),
            redirect_location: item.redirect_location.clone(),
        })
        .collect()
}

fn response_headers(py: Python<'_>, headers: &HeaderMap) -> PyResult<Py<PyAny>> {
    let result = PyModule::import(py, "urllib3._collections")?
        .getattr("HTTPHeaderDict")?
        .call0()?;
    for (name, value) in headers {
        result.call_method1(
            "add",
            (
                name.as_str(),
                value
                    .to_str()
                    .map_err(|error| PyValueError::new_err(error.to_string()))?,
            ),
        )?;
    }
    Ok(result.unbind())
}

fn retry_after(py: Python<'_>, retry: &Bound<'_, PyAny>, headers: &HeaderMap) -> PyResult<f64> {
    if !headers.contains_key("retry-after") {
        return Ok(0.0);
    }
    let kwargs = PyDict::new(py);
    kwargs.set_item("headers", response_headers(py, headers)?)?;
    let response = PyModule::import(py, "types")?
        .getattr("SimpleNamespace")?
        .call((), Some(&kwargs))?;
    let value = retry.call_method1("get_retry_after", (response,))?;
    if value.is_none() {
        Ok(0.0)
    } else {
        value.extract::<f64>()
    }
}

fn sleep_before_retry(py: Python<'_>, state: &RetryState, retry_after: f64) -> PyResult<()> {
    let random_unit = if state.remaining().backoff.jitter > 0.0 {
        PyModule::import(py, "random")?
            .getattr("random")?
            .call0()?
            .extract::<f64>()?
    } else {
        0.0
    };
    let backoff = state.backoff(random_unit);
    let delay = if retry_after > 0.0 {
        retry_after
    } else {
        backoff
    };
    if delay > 0.0 {
        PyModule::import(py, "time")?
            .getattr("sleep")?
            .call1((delay,))?;
    }
    Ok(())
}

fn requests_exception(
    py: Python<'_>,
    name: &str,
    message: String,
    request: &Bound<'_, PyAny>,
) -> PyErr {
    let result = (|| -> PyResult<PyErr> {
        let class = PyModule::import(py, "requests.exceptions")?.getattr(name)?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("request", request)?;
        let value = class.call((message,), Some(&kwargs))?;
        Ok(PyErr::from_value(value))
    })();
    result.unwrap_or_else(|error| error)
}

fn mapped_transport_error(
    py: Python<'_>,
    error: requests::Error,
    request: &Bound<'_, PyAny>,
) -> PyErr {
    let name = match error.kind() {
        ErrorKind::ConnectTimeout => "ConnectTimeout",
        ErrorKind::ReadTimeout => "ReadTimeout",
        ErrorKind::Proxy => "ProxyError",
        ErrorKind::Tls | ErrorKind::Handshake => "SSLError",
        ErrorKind::InvalidUrl => "InvalidURL",
        _ => "ConnectionError",
    };
    requests_exception(py, name, error.to_string(), request)
}

fn retry_reason(error: &requests::Error) -> RetryReason {
    match error.kind() {
        ErrorKind::Connect
        | ErrorKind::ConnectTimeout
        | ErrorKind::Dns
        | ErrorKind::Handshake
        | ErrorKind::Proxy
        | ErrorKind::Tls => RetryReason::Connect,
        ErrorKind::ReadTimeout
        | ErrorKind::ResponseBody
        | ErrorKind::ChunkedEncoding
        | ErrorKind::Connection
        | ErrorKind::Send => RetryReason::Read,
        _ => RetryReason::Other,
    }
}

fn build_python_response(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    response: requests::blocking::Response,
) -> PyResult<Py<PyAny>> {
    let status = response.status().as_u16();
    let reason = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_owned();
    let headers = response_headers(py, response.headers())?;
    let raw = Py::new(
        py,
        NativeAdapterRaw {
            body: Some(response.into_body()),
            status,
            reason,
            headers,
            closed: false,
        },
    )?;
    Ok(adapter
        .call_method1("build_response", (request, raw))?
        .unbind())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)] // Mirrors HTTPAdapter.send's public signature.
fn _adapter_send_trial(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    request: &Bound<'_, PyAny>,
    stream: bool,
    timeout: &Bound<'_, PyAny>,
    verify: &Bound<'_, PyAny>,
    cert: &Bound<'_, PyAny>,
    proxies: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let _ = stream;
    let input = match native_send_input(py, adapter, request, timeout, verify, cert, proxies)? {
        Ok(input) => input,
        Err(_) => return Ok(py.NotImplemented()),
    };
    if let Some(proxy) = &input.selected_proxy {
        adapter.call_method1("proxy_manager_for", (proxy,))?;
    }
    let pool = match adapter_pool(py, adapter, &input)? {
        Ok(pool) => pool,
        Err(_) => return Ok(py.NotImplemented()),
    };
    let retry_object = adapter.getattr("max_retries")?;
    let mut retry_state = RetryState::with_history(
        input.retry.policy.clone(),
        core_history(&input.retry.history),
    );

    loop {
        let method = input.method.clone();
        let url = input.url.clone();
        let headers = input.headers.clone();
        let body = input
            .body
            .clone()
            .map_or(BodySource::Empty, BodySource::from);
        let timeout = input.timeout;
        let pool = Arc::clone(&pool);
        let attempt = py.detach(move || pool.send(method, &url, headers, body, timeout));
        let response = match attempt {
            Ok(response) => response,
            Err(error) => {
                let reason = retry_reason(&error);
                if !retry_state.allows_method(&input.method_name) {
                    return Err(mapped_transport_error(py, error, request));
                }
                match retry_state.increment(reason, &input.method_name, &input.url, None) {
                    Ok(next) => {
                        retry_state = next;
                        sleep_before_retry(py, &retry_state, 0.0)?;
                        continue;
                    }
                    Err(_) => return Err(mapped_transport_error(py, error, request)),
                }
            }
        };
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let has_retry_after = response.headers().contains_key("retry-after");
        if !retry_state.is_retry(&input.method_name, status, has_retry_after) {
            return build_python_response(py, adapter, request, response);
        }
        let retry_after = retry_after(py, &retry_object, response.headers())?;
        let incremented = retry_state.increment(
            RetryReason::Status { status },
            &input.method_name,
            &input.url,
            location.as_deref(),
        );
        let next = match incremented {
            Ok(next) => next,
            Err(_) if input.retry.policy.raise_on_status => {
                return Err(requests_exception(
                    py,
                    "RetryError",
                    format!("too many {status} responses"),
                    request,
                ));
            }
            Err(_) => return build_python_response(py, adapter, request, response),
        };
        py.detach(move || response.bytes())
            .map_err(|error| mapped_transport_error(py, error, request))?;
        retry_state = next;
        sleep_before_retry(py, &retry_state, retry_after)?;
    }
}

#[pyfunction]
fn _adapter_close_trial(py: Python<'_>, adapter: &Bound<'_, PyAny>) -> PyResult<usize> {
    if !adapter
        .get_type()
        .as_any()
        .is(adapter_state(py)?.adapter_type.bind(py))
    {
        return Ok(0);
    }
    let identity = adapter_id(py, adapter)?;
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    reap_adapter_pools(py, &mut table)?;
    let Some(entry) = table.remove(&identity) else {
        return Ok(0);
    };
    let count = entry.pools.len();
    for pool in entry.pools.values() {
        pool.clear();
    }
    Ok(count)
}

#[pyfunction]
fn _adapter_pool_side_table_trial(py: Python<'_>) -> PyResult<usize> {
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    reap_adapter_pools(py, &mut table)?;
    Ok(table.len())
}

impl NativeAdapterRaw {
    fn read_amount(&mut self, py: Python<'_>, amount: Option<usize>) -> PyResult<Py<PyAny>> {
        if self.closed {
            return Ok(PyBytes::new(py, b"").into_any().unbind());
        }
        let Some(body) = self.body.as_mut() else {
            self.closed = true;
            return Ok(PyBytes::new(py, b"").into_any().unbind());
        };
        let mut bytes = Vec::new();
        let result = match amount {
            Some(amount) => {
                bytes.resize(amount, 0);
                let read = py.detach(|| body.read(&mut bytes));
                match read {
                    Ok(read) => {
                        bytes.truncate(read);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            None => py.detach(|| body.read_to_end(&mut bytes)).map(|_| ()),
        };
        result.map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        if bytes.is_empty() {
            self.closed = true;
            self.body = None;
        }
        Ok(PyBytes::new(py, &bytes).into_any().unbind())
    }
}

#[pymethods]
impl NativeAdapterRaw {
    #[getter]
    fn status(&self) -> u16 {
        self.status
    }

    #[getter]
    fn reason(&self) -> &str {
        &self.reason
    }

    #[getter]
    fn headers(&self, py: Python<'_>) -> Py<PyAny> {
        self.headers.clone_ref(py)
    }

    #[getter]
    fn closed(&self) -> bool {
        self.closed
    }

    #[pyo3(signature = (amt=None, decode_content=false, cache_content=false))]
    fn read(
        &mut self,
        py: Python<'_>,
        amt: Option<&Bound<'_, PyAny>>,
        decode_content: bool,
        cache_content: bool,
    ) -> PyResult<Py<PyAny>> {
        let _ = (decode_content, cache_content);
        let amount = match amt {
            None => None,
            Some(value) if value.is_none() => None,
            Some(value) => Some(value.extract::<usize>()?),
        };
        self.read_amount(py, amount)
    }

    #[pyo3(signature = (amt=65_536, decode_content=None))]
    fn stream(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        amt: usize,
        decode_content: Option<bool>,
    ) -> PyResult<Py<NativeAdapterStream>> {
        let _ = decode_content;
        Py::new(
            py,
            NativeAdapterStream {
                raw: slf.into_pyobject(py)?.unbind(),
                amount: amt.max(1),
                done: false,
            },
        )
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        if let Some(body) = self.body.take() {
            py.detach(|| body.close())
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        }
        self.closed = true;
        Ok(())
    }

    fn release_conn(&mut self) {}
}

#[pymethods]
impl NativeAdapterStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if self.done {
            return Ok(None);
        }
        let chunk = self
            .raw
            .bind(py)
            .borrow_mut()
            .read_amount(py, Some(self.amount))?;
        if chunk.bind(py).len()? == 0 {
            self.done = true;
            Ok(None)
        } else {
            Ok(Some(chunk))
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        self.raw.bind(py).borrow_mut().close(py)?;
        self.done = true;
        Ok(())
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    let _ = retry_state(py)?;
    let _ = adapter_state(py)?;
    module.add_class::<NativeAdapterRaw>()?;
    module.add_class::<NativeAdapterStream>()?;
    module.add_function(wrap_pyfunction!(_select_proxy_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_retry_policy_snapshot_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_send_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_close_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_pool_side_table_trial, module)?)?;
    Ok(())
}

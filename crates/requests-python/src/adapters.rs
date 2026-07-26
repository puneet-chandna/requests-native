use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyBool, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PyModule, PySet, PyString,
    PyTuple,
};
use pyo3::wrap_pyfunction;
use requests::adapters::{AdapterPool, AdapterResponse, AdapterResponseBody};
use requests::retry::{
    BackoffPolicy, MethodSet, RetryCount, RetryHistory, RetryPolicy, RetryReason, RetryState,
    StatusSet,
};
use requests::{
    BodySource, CertificateSource, ErrorKind, HeaderMap, HeaderName, HeaderValue, Identity, Method,
    Proxy, Timeout, TlsConfig, Uri,
};

#[derive(PartialEq)]
struct RetrySnapshot {
    version: String,
    policy: RetryPolicy,
    history: Vec<HistorySnapshot>,
    retry_after_max: Option<u64>,
}

#[derive(PartialEq)]
struct HistorySnapshot {
    method: String,
    url: String,
    status: Option<u16>,
    redirect_location: Option<String>,
}

struct RetryStateGuard {
    urllib3_module: Py<PyAny>,
    urllib3_version: Py<PyAny>,
    retry_type: Py<PyAny>,
    history_type: Py<PyAny>,
    retry_module: Py<PyAny>,
    class_dict: DictProof,
    module_dict: DictProof,
}

static RETRY_STATE: PyOnceLock<RetryStateGuard> = PyOnceLock::new();

struct AdapterState {
    adapters_module: Py<PyAny>,
    adapter_type: Py<PyAny>,
    prepared_request_type: Py<PyAny>,
    prepared_getattribute: Py<PyAny>,
    poolmanager_type: Py<PyAny>,
    poolmanager_behavior: BehaviorProof,
    poolmanager_module: Py<PyAny>,
    proxy_manager_behavior: BehaviorProof,
    socks_manager_behavior: BehaviorProof,
    methods: Vec<(String, Py<PyAny>)>,
    globals: Vec<(String, Py<PyAny>)>,
}

static ADAPTER_STATE: PyOnceLock<AdapterState> = PyOnceLock::new();

struct SideEntry {
    weak_adapter: Py<PyAny>,
    poolmanager: Py<PyAny>,
    manager_proof: ManagerProof,
    visible_pool_count: usize,
    direct_pools: PoolRealm,
    proxy_pools: HashMap<String, PoolRealm>,
    proxy_managers: HashMap<String, ManagerProof>,
}

#[derive(Default)]
struct PoolRealm {
    pools: HashMap<String, Arc<AdapterPool>>,
    order: VecDeque<String>,
}

type ObjectItems = Vec<(Py<PyAny>, Py<PyAny>)>;

struct CallableProof {
    function: Py<PyAny>,
    code: Py<PyAny>,
    defaults: Py<PyAny>,
    kwdefaults: Py<PyAny>,
    kwdefault_items: Option<MappingProof>,
    closure: Py<PyAny>,
    closure_cells: Vec<(Py<PyAny>, Option<Py<PyAny>>)>,
    attributes: MappingProof,
    annotations: MappingProof,
}

struct DictProof {
    items: Vec<(String, Py<PyAny>)>,
    callables: Vec<CallableProof>,
}

struct MappingProof {
    mapping: Py<PyAny>,
    items: ObjectItems,
    nested: Vec<(Py<PyAny>, Box<MappingProof>)>,
}

struct BehaviorProof {
    object: Py<PyAny>,
    class_dict: Option<DictProof>,
    callable: Option<CallableProof>,
}

struct ManagerProof {
    manager: Py<PyAny>,
    manager_type: Py<PyAny>,
    class_dict: DictProof,
    objects: DictProof,
    mappings: Vec<(String, MappingProof)>,
    pools_dict: DictProof,
    pool_container: MappingProof,
    visible_pools: Vec<(Py<PyAny>, Py<PyAny>)>,
}

static ADAPTER_POOLS: OnceLock<Mutex<HashMap<usize, SideEntry>>> = OnceLock::new();

#[pyclass(module = "requests._requests_rust", unsendable)]
struct NativeAdapterRaw {
    body: Option<AdapterResponseBody>,
    content_encoding: Option<String>,
    decoder: Option<Py<PyAny>>,
    decoded: Vec<u8>,
    decoded_offset: usize,
    decoder_eof: bool,
    decode_started: bool,
    status: u16,
    reason: String,
    headers: Py<PyAny>,
    closed: bool,
}

#[pyclass(module = "requests._requests_rust", unsendable)]
struct NativeAdapterStream {
    raw: Py<NativeAdapterRaw>,
    amount: Option<usize>,
    decode_content: bool,
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
    let urllib3_module = PyModule::import(py, "urllib3")?;
    let retry_type = guard.retry_type.bind(py);
    if !urllib3_module.as_any().is(guard.urllib3_module.bind(py))
        || !urllib3_module
            .getattr("__version__")?
            .is(guard.urllib3_version.bind(py))
        || !retry_module.as_any().is(guard.retry_module.bind(py))
        || !retry_module.getattr("Retry")?.is(retry_type)
        || !retry.get_type().as_any().is(retry_type)
        || !dict_proof_is_pristine(py, &retry_type.getattr("__dict__")?, &guard.class_dict)?
        || !dict_proof_is_pristine(py, retry_module.dict().as_any(), &guard.module_dict)?
    {
        return Ok(Err(
            "Retry must have the exact urllib3.util.retry.Retry type".to_owned(),
        ));
    }
    let instance_dict = retry.getattr("__dict__")?;
    let instance_dict = match instance_dict.cast::<PyDict>() {
        Ok(value) => value,
        Err(_) => return Ok(Err("Retry instance dictionary is unsupported".to_owned())),
    };
    if guard
        .class_dict
        .items
        .iter()
        .any(|(name, _)| instance_dict.contains(name).unwrap_or(true))
    {
        return Ok(Err(
            "Retry instance shadows a guarded method or constant".to_owned()
        ));
    }
    let version = guard.urllib3_version.bind(py).extract::<String>()?;
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
    let (methods_name, methods_value) =
        if version.starts_with("1.26.") && instance_dict.contains("method_whitelist")? {
            (
                "method_whitelist",
                instance_dict
                    .get_item("method_whitelist")?
                    .expect("contains checked"),
            )
        } else {
            ("allowed_methods", retry.getattr("allowed_methods")?)
        };
    let Some(allowed_methods) = string_set(methods_value)? else {
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
    let history = match history_snapshot(retry.getattr("history")?, guard.history_type.bind(py))? {
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
    let urllib3_module = PyModule::import(py, "urllib3")?;
    let retry_module = PyModule::import(py, "urllib3.util.retry")?;
    let retry_type = retry_module.getattr("Retry")?;
    // Pickle lazily caches this standard-library metadata on slotted classes.
    // Materialize it before freezing the class dictionary so a later pickle
    // round-trip does not look like user mutation.
    PyModule::import(py, "copyreg")?
        .getattr("_slotnames")?
        .call1((&retry_type,))?;
    let class_dict = dict_proof(py, &retry_type.getattr("__dict__")?)?;
    let module_dict = dict_proof(py, retry_module.dict().as_any())?;
    Ok(RetryStateGuard {
        urllib3_version: urllib3_module.getattr("__version__")?.unbind(),
        urllib3_module: urllib3_module.into_any().unbind(),
        retry_type: retry_type.unbind(),
        history_type: retry_module.getattr("RequestHistory")?.unbind(),
        retry_module: retry_module.into_any().unbind(),
        class_dict,
        module_dict,
    })
}

fn dict_snapshot(value: &Bound<'_, PyAny>) -> PyResult<Vec<(String, Py<PyAny>)>> {
    value
        .call_method0("items")?
        .try_iter()?
        .map(|item| {
            let item = item?;
            let pair = item.cast::<PyTuple>()?;
            Ok((pair.get_item(0)?.extract()?, pair.get_item(1)?.unbind()))
        })
        .collect()
}

fn exact_dict_snapshot(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    expected: &[(String, Py<PyAny>)],
) -> PyResult<bool> {
    if value.len()? != expected.len() {
        return Ok(false);
    }
    for (name, original) in expected {
        let Ok(current) = value.get_item(name) else {
            return Ok(false);
        };
        if !current.is(original.bind(py)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn callable_proof(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Option<CallableProof>> {
    let function_type = PyModule::import(py, "types")?.getattr("FunctionType")?;
    if !value.is_instance(&function_type)? {
        return Ok(None);
    }
    let closure = value.getattr("__closure__")?;
    let closure_cells = if closure.is_none() {
        Vec::new()
    } else {
        closure
            .try_iter()?
            .map(|cell| {
                let cell = cell?;
                let contents = cell.getattr("cell_contents").ok().map(Bound::unbind);
                Ok((cell.unbind(), contents))
            })
            .collect::<PyResult<Vec<_>>>()?
    };
    let kwdefaults = value.getattr("__kwdefaults__")?;
    let kwdefault_items = if kwdefaults.is_none() {
        None
    } else {
        Some(mapping_proof(&kwdefaults)?)
    };
    Ok(Some(CallableProof {
        function: value.clone().unbind(),
        code: value.getattr("__code__")?.unbind(),
        defaults: value.getattr("__defaults__")?.unbind(),
        kwdefaults: kwdefaults.unbind(),
        kwdefault_items,
        closure: closure.unbind(),
        closure_cells,
        attributes: mapping_proof(&value.getattr("__dict__")?)?,
        annotations: mapping_proof(&value.getattr("__annotations__")?)?,
    }))
}

fn callable_proof_is_pristine(py: Python<'_>, proof: &CallableProof) -> PyResult<bool> {
    let function = proof.function.bind(py);
    if !function.getattr("__code__")?.is(proof.code.bind(py))
        || !function
            .getattr("__defaults__")?
            .is(proof.defaults.bind(py))
        || !function
            .getattr("__kwdefaults__")?
            .is(proof.kwdefaults.bind(py))
        || !function.getattr("__closure__")?.is(proof.closure.bind(py))
        || !mapping_proof_is_pristine(py, &function.getattr("__dict__")?, &proof.attributes)?
        || !mapping_proof_is_pristine(
            py,
            &function.getattr("__annotations__")?,
            &proof.annotations,
        )?
    {
        return Ok(false);
    }
    if let Some(expected) = &proof.kwdefault_items
        && !mapping_proof_is_pristine(py, &function.getattr("__kwdefaults__")?, expected)?
    {
        return Ok(false);
    }
    let closure = function.getattr("__closure__")?;
    if closure.is_none() {
        return Ok(proof.closure_cells.is_empty());
    }
    let current = closure.try_iter()?.collect::<PyResult<Vec<_>>>()?;
    if current.len() != proof.closure_cells.len() {
        return Ok(false);
    }
    for (cell, (expected_cell, expected_contents)) in current.iter().zip(&proof.closure_cells) {
        if !cell.is(expected_cell.bind(py)) {
            return Ok(false);
        }
        let contents = cell.getattr("cell_contents").ok();
        match (contents, expected_contents) {
            (None, None) => {}
            (Some(contents), Some(expected)) if contents.is(expected.bind(py)) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn dict_proof(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<DictProof> {
    let items = dict_snapshot(value)?;
    let callables = items
        .iter()
        .filter_map(|(_, item)| callable_proof(py, item.bind(py)).transpose())
        .collect::<PyResult<Vec<_>>>()?;
    Ok(DictProof { items, callables })
}

fn dict_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &DictProof,
) -> PyResult<bool> {
    if !exact_dict_snapshot(py, value, &proof.items)? {
        return Ok(false);
    }
    for callable in &proof.callables {
        if !callable_proof_is_pristine(py, callable)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn behavior_proof(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<BehaviorProof> {
    let type_type = PyModule::import(py, "builtins")?.getattr("type")?;
    let class_dict = value
        .is_instance(&type_type)?
        .then(|| dict_proof(py, &value.getattr("__dict__")?))
        .transpose()?;
    Ok(BehaviorProof {
        object: value.clone().unbind(),
        class_dict,
        callable: callable_proof(py, value)?,
    })
}

fn behavior_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &BehaviorProof,
) -> PyResult<bool> {
    if !value.is(proof.object.bind(py)) {
        return Ok(false);
    }
    if let Some(class_dict) = &proof.class_dict
        && !dict_proof_is_pristine(py, &value.getattr("__dict__")?, class_dict)?
    {
        return Ok(false);
    }
    if let Some(callable) = &proof.callable
        && !callable_proof_is_pristine(py, callable)?
    {
        return Ok(false);
    }
    Ok(true)
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
    if !value.is_exact_instance_of::<PyTuple>()
        && !value.is_exact_instance_of::<PyList>()
        && !value.is_exact_instance_of::<PySet>()
        && !value.is_exact_instance_of::<PyFrozenSet>()
    {
        return Ok(None);
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
    if !value.is_exact_instance_of::<PyTuple>()
        && !value.is_exact_instance_of::<PyList>()
        && !value.is_exact_instance_of::<PySet>()
        && !value.is_exact_instance_of::<PyFrozenSet>()
    {
        return Ok(None);
    }
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

fn history_snapshot(
    value: Bound<'_, PyAny>,
    history_type: &Bound<'_, PyAny>,
) -> PyResult<Result<Vec<HistorySnapshot>, String>> {
    if !value.is_exact_instance_of::<PyTuple>() {
        return Ok(Err("history is not an exact tuple".to_owned()));
    }
    let iterator = value.try_iter()?;
    let mut history = Vec::new();
    for item in iterator {
        let item = item?;
        if !item.get_type().as_any().is(history_type) {
            return Ok(Err("history contains an unsupported row".to_owned()));
        }
        let Ok(error) = item.getattr("error") else {
            return Ok(Err("history row has an unsupported shape".to_owned()));
        };
        if !error.is_none() {
            return Ok(Err("history contains a Python error object".to_owned()));
        }
        let Ok(method) = item
            .getattr("method")
            .and_then(|value| value.extract::<String>())
        else {
            return Ok(Err("history row method is unsupported".to_owned()));
        };
        let Ok(url) = item
            .getattr("url")
            .and_then(|value| value.extract::<String>())
        else {
            return Ok(Err("history row URL is unsupported".to_owned()));
        };
        let Ok(status) = item.getattr("status") else {
            return Ok(Err("history row status is unsupported".to_owned()));
        };
        let status = if status.is_none() {
            None
        } else {
            let Ok(status) = status.extract::<u16>() else {
                return Ok(Err("history row status is unsupported".to_owned()));
            };
            Some(status)
        };
        let Ok(redirect) = item.getattr("redirect_location") else {
            return Ok(Err("history row redirect is unsupported".to_owned()));
        };
        let redirect_location = if redirect.is_none() {
            None
        } else {
            let Ok(redirect) = redirect.extract::<String>() else {
                return Ok(Err("history row redirect is unsupported".to_owned()));
            };
            Some(redirect)
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
    pool_connections: usize,
    pool_block: bool,
    retry: RetrySnapshot,
}

fn initialize_adapter_state(py: Python<'_>) -> PyResult<AdapterState> {
    let adapters = PyModule::import(py, "requests.adapters")?;
    let adapter_type = adapters.getattr("HTTPAdapter")?;
    let prepared_request_type =
        PyModule::import(py, "requests.models")?.getattr("PreparedRequest")?;
    let methods = [
        "__getattribute__",
        "__setstate__",
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
        "DEFAULT_CA_BUNDLE_PATH",
        "SOCKSProxyManager",
    ]
    .into_iter()
    .map(|name| {
        adapters
            .getattr(name)
            .map(|value| (name.to_owned(), value.unbind()))
    })
    .collect::<PyResult<Vec<_>>>()?;
    let poolmanager_type = adapters.getattr("PoolManager")?;
    let poolmanager_module = PyModule::import(py, "urllib3.poolmanager")?;
    let poolmanager_behavior = behavior_proof(py, &poolmanager_type)?;
    let proxy_manager_behavior = behavior_proof(py, &poolmanager_module.getattr("ProxyManager")?)?;
    let socks_manager_behavior = behavior_proof(py, &adapters.getattr("SOCKSProxyManager")?)?;
    let prepared_getattribute = prepared_request_type.getattr("__getattribute__")?.unbind();
    Ok(AdapterState {
        adapters_module: adapters.into_any().unbind(),
        adapter_type: adapter_type.unbind(),
        prepared_request_type: prepared_request_type.unbind(),
        prepared_getattribute,
        poolmanager_type: poolmanager_type.unbind(),
        poolmanager_behavior,
        poolmanager_module: poolmanager_module.into_any().unbind(),
        proxy_manager_behavior,
        socks_manager_behavior,
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
    let adapter_dict = adapter.getattr("__dict__")?;
    let Ok(adapter_dict) = adapter_dict.cast::<PyDict>() else {
        return Ok(false);
    };
    if state
        .methods
        .iter()
        .any(|(name, _)| adapter_dict.contains(name).unwrap_or(true))
    {
        return Ok(false);
    }
    let request_type = state.prepared_request_type.bind(py);
    if !request_type
        .getattr("__getattribute__")?
        .is(state.prepared_getattribute.bind(py))
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
    let poolmanager_module = PyModule::import(py, "urllib3.poolmanager")?;
    if !poolmanager_module
        .as_any()
        .is(state.poolmanager_module.bind(py))
        || !behavior_proof_is_pristine(
            py,
            &poolmanager_module.getattr("ProxyManager")?,
            &state.proxy_manager_behavior,
        )?
        || !behavior_proof_is_pristine(
            py,
            &module.getattr("SOCKSProxyManager")?,
            &state.socks_manager_behavior,
        )?
    {
        return Ok(false);
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
    if !seconds.is_finite() || seconds <= 0.0 {
        return Ok(None);
    }
    Ok(Duration::try_from_secs_f64(seconds).ok().map(Some))
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
    if !value.getattr("total")?.is_none() {
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

fn tls_value(
    py: Python<'_>,
    verify: &Bound<'_, PyAny>,
    cert: &Bound<'_, PyAny>,
) -> PyResult<Option<TlsConfig>> {
    let roots = if verify.is_exact_instance_of::<PyBool>() {
        if verify.extract::<bool>()? {
            let bundle = adapter_state(py)?
                .adapters_module
                .bind(py)
                .getattr("DEFAULT_CA_BUNDLE_PATH")?;
            if !bundle.is_exact_instance_of::<PyString>() {
                return Ok(None);
            }
            CertificateSource::PemBundle(PathBuf::from(bundle.extract::<String>()?))
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

fn request_headers(py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<Option<HeaderMap>> {
    let headers = request.getattr("headers")?;
    let header_type = adapter_state(py)?
        .adapters_module
        .bind(py)
        .getattr("CaseInsensitiveDict")?;
    if !headers.get_type().as_any().is(&header_type) {
        return Ok(None);
    }
    let store = headers.getattr("_store")?;
    let ordered_dict = PyModule::import(py, "collections")?.getattr("OrderedDict")?;
    if !store.get_type().as_any().is(&ordered_dict) {
        return Ok(None);
    }
    let items = store.call_method0("values")?;
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
    if !registered_adapter_pristine(py, adapter)? {
        return Ok(Err(
            "adapter is not registered with its original pool manager".to_owned(),
        ));
    }
    let config = adapter.getattr("config")?;
    if !config.is_exact_instance_of::<PyDict>() || !config.is_empty()? {
        return Ok(Err("adapter config is not an empty exact dict".to_owned()));
    }
    let Some(pool_maxsize) = exact_usize(adapter.getattr("_pool_maxsize")?) else {
        return Ok(Err("pool maximum is unsupported".to_owned()));
    };
    if pool_maxsize == 0 {
        return Ok(Err("zero-sized native pools are unsupported".to_owned()));
    }
    let Some(pool_connections) = exact_usize(adapter.getattr("_pool_connections")?) else {
        return Ok(Err("pool settings are unsupported".to_owned()));
    };
    let Some(pool_block) = exact_bool(adapter.getattr("_pool_block")?)? else {
        return Ok(Err("pool settings are unsupported".to_owned()));
    };
    let method_value = request.getattr("method")?;
    if !method_value.is_exact_instance_of::<PyString>() {
        return Ok(Err("request method is unsupported".to_owned()));
    }
    let method_name = method_value.extract::<String>()?;
    let method = match Method::from_bytes(method_name.as_bytes()) {
        Ok(method) => method,
        Err(_) => return Ok(Err("request method is unsupported".to_owned())),
    };
    let url_value = request.getattr("url")?;
    if !url_value.is_exact_instance_of::<PyString>() {
        return Ok(Err("request URL is unsupported".to_owned()));
    }
    let url = url_value.extract::<String>()?;
    let request_uri = match Uri::from_str(&url) {
        Ok(uri) => uri,
        Err(_) => return Ok(Err("request URL is unsupported".to_owned())),
    };
    let Some(scheme) = request_uri.scheme_str() else {
        return Ok(Err("request URL is unsupported".to_owned()));
    };
    let Some(authority) = request_uri.authority() else {
        return Ok(Err("request URL is unsupported".to_owned()));
    };
    let Some(headers) = request_headers(py, request)? else {
        return Ok(Err("request headers are unsupported".to_owned()));
    };
    let Some(body) = request_body(request)? else {
        return Ok(Err("request body is not proven replayable".to_owned()));
    };
    let retry_object = adapter.getattr("max_retries")?;
    let retry = match retry_snapshot(py, &retry_object)? {
        Ok(retry) => retry,
        Err(reason) => return Ok(Err(reason)),
    };
    let Some(timeout) = timeout_value(py, timeout)? else {
        return Ok(Err("timeout is unsupported".to_owned()));
    };
    let Some(tls) = tls_value(py, verify, cert)? else {
        return Ok(Err("TLS settings are unsupported".to_owned()));
    };
    let Some((proxy, selected_proxy)) = proxy_value(py, &url, proxies)? else {
        return Ok(Err("proxy settings are unsupported".to_owned()));
    };
    if !adapter_identity_is_pristine(py, adapter, request)?
        || !registered_adapter_pristine(py, adapter)?
    {
        return Ok(Err("adapter identities changed during admission".to_owned()));
    }
    let revalidated_retry = match retry_snapshot(py, &adapter.getattr("max_retries")?)? {
        Ok(retry) => retry,
        Err(reason) => return Ok(Err(reason)),
    };
    if revalidated_retry != retry {
        return Ok(Err("Retry state changed during admission".to_owned()));
    }
    let normalized_scheme = scheme.to_ascii_lowercase();
    let normalized_host = request_uri
        .host()
        .unwrap_or(authority.host())
        .to_ascii_lowercase();
    let explicit_port = request_uri.port_u16();
    let effective_port = explicit_port.or(match normalized_scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    });
    let pool_key = format!(
        "{normalized_scheme}://{normalized_host}:{}|{proxy:?}|{tls:?}|{pool_maxsize}",
        effective_port.map_or_else(String::new, |port| port.to_string())
    );
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
        pool_connections,
        pool_block,
        retry,
    }))
}

fn adapter_id(py: Python<'_>, adapter: &Bound<'_, PyAny>) -> PyResult<usize> {
    PyModule::import(py, "builtins")?
        .getattr("id")?
        .call1((adapter,))?
        .extract()
}

fn manager_pool_count(manager: &Bound<'_, PyAny>) -> PyResult<usize> {
    manager.getattr("pools")?.len()
}

fn manager_proof(manager: &Bound<'_, PyAny>) -> PyResult<ManagerProof> {
    let py = manager.py();
    let mut mappings = Vec::new();
    for name in [
        "headers",
        "connection_pool_kw",
        "pool_classes_by_scheme",
        "key_fn_by_scheme",
        "proxy_headers",
    ] {
        if let Ok(mapping) = manager.getattr(name) {
            mappings.push((name.to_owned(), mapping_proof(&mapping)?));
        }
    }
    let pools = manager.getattr("pools")?;
    Ok(ManagerProof {
        manager: manager.clone().unbind(),
        manager_type: manager.get_type().into_any().unbind(),
        class_dict: dict_proof(py, &manager.get_type().getattr("__dict__")?)?,
        objects: dict_proof(py, &manager.getattr("__dict__")?)?,
        mappings,
        pools_dict: dict_proof(py, &pools.getattr("__dict__")?)?,
        pool_container: mapping_proof(&pools.getattr("_container")?)?,
        visible_pools: visible_pools(manager)?,
    })
}

fn object_items(value: &Bound<'_, PyAny>) -> PyResult<ObjectItems> {
    value
        .call_method0("items")?
        .try_iter()?
        .map(|item| {
            let item = item?;
            let pair = item.cast::<PyTuple>()?;
            Ok((pair.get_item(0)?.unbind(), pair.get_item(1)?.unbind()))
        })
        .collect()
}

fn mapping_proof(value: &Bound<'_, PyAny>) -> PyResult<MappingProof> {
    let items = object_items(value)?;
    let mut nested = Vec::new();
    for (_, item) in &items {
        let item = item.bind(value.py());
        if item.hasattr("items")? {
            nested.push((item.clone().unbind(), Box::new(mapping_proof(item)?)));
        }
    }
    Ok(MappingProof {
        mapping: value.clone().unbind(),
        items,
        nested,
    })
}

fn mapping_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &MappingProof,
) -> PyResult<bool> {
    if !value.is(proof.mapping.bind(py)) {
        return Ok(false);
    }
    let current = object_items(value)?;
    if current.len() != proof.items.len()
        || current
            .iter()
            .zip(&proof.items)
            .any(|((key, value), (expected_key, expected_value))| {
                !key.bind(py).is(expected_key.bind(py))
                    || !value.bind(py).is(expected_value.bind(py))
            })
    {
        return Ok(false);
    }
    for (nested_value, nested_proof) in &proof.nested {
        if !mapping_proof_is_pristine(py, nested_value.bind(py), nested_proof)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn manager_proof_is_pristine(
    py: Python<'_>,
    manager: &Bound<'_, PyAny>,
    proof: &ManagerProof,
) -> PyResult<bool> {
    if !manager.is(proof.manager.bind(py))
        || !manager.get_type().as_any().is(proof.manager_type.bind(py))
        || !dict_proof_is_pristine(
            py,
            &manager.get_type().getattr("__dict__")?,
            &proof.class_dict,
        )?
        || !dict_proof_is_pristine(py, &manager.getattr("__dict__")?, &proof.objects)?
    {
        return Ok(false);
    }
    for (name, expected) in &proof.mappings {
        if !mapping_proof_is_pristine(py, &manager.getattr(name.as_str())?, expected)? {
            return Ok(false);
        }
    }
    let pools = manager.getattr("pools")?;
    if !dict_proof_is_pristine(py, &pools.getattr("__dict__")?, &proof.pools_dict)?
        || !mapping_proof_is_pristine(py, &pools.getattr("_container")?, &proof.pool_container)?
    {
        return Ok(false);
    }
    let pools = visible_pools(manager)?;
    Ok(pools.len() == proof.visible_pools.len()
        && pools.iter().zip(&proof.visible_pools).all(
            |((key, value), (expected_key, expected_value))| {
                key.bind(py).is(expected_key.bind(py)) && value.bind(py).is(expected_value.bind(py))
            },
        ))
}

fn visible_pools(manager: &Bound<'_, PyAny>) -> PyResult<Vec<(Py<PyAny>, Py<PyAny>)>> {
    manager
        .getattr("pools")?
        .getattr("_container")?
        .call_method0("items")?
        .try_iter()?
        .map(|item| {
            let item = item?;
            let pair = item.cast::<PyTuple>()?;
            Ok((pair.get_item(0)?.unbind(), pair.get_item(1)?.unbind()))
        })
        .collect()
}

fn refresh_manager_pools(py: Python<'_>, proof: &mut ManagerProof) -> PyResult<()> {
    let manager = proof.manager.bind(py);
    let pools = manager.getattr("pools")?;
    proof.pools_dict = dict_proof(py, &pools.getattr("__dict__")?)?;
    proof.pool_container = mapping_proof(&pools.getattr("_container")?)?;
    proof.visible_pools = visible_pools(manager)?;
    Ok(())
}

fn manager_identity_is_pristine(py: Python<'_>, manager: &Bound<'_, PyAny>) -> PyResult<bool> {
    let state = adapter_state(py)?;
    let manager_type = state.poolmanager_type.bind(py);
    if !manager.get_type().as_any().is(manager_type) {
        return Ok(false);
    }
    if !behavior_proof_is_pristine(py, manager_type, &state.poolmanager_behavior)? {
        return Ok(false);
    }
    let dictionary = manager.getattr("__dict__")?;
    let Ok(_dictionary) = dictionary.cast::<PyDict>() else {
        return Ok(false);
    };
    Ok(true)
}

fn manager_configuration_is_pristine(
    adapter: &Bound<'_, PyAny>,
    manager: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let kwargs = manager.getattr("connection_pool_kw")?;
    let Ok(kwargs) = kwargs.cast::<PyDict>() else {
        return Ok(false);
    };
    if kwargs.len() != 2 {
        return Ok(false);
    }
    let Some(maxsize) = kwargs.get_item("maxsize")? else {
        return Ok(false);
    };
    let Some(block) = kwargs.get_item("block")? else {
        return Ok(false);
    };
    let configured_block = exact_bool(block)?;
    if exact_usize(maxsize) != exact_usize(adapter.getattr("_pool_maxsize")?)
        || configured_block.is_none()
        || configured_block != exact_bool(adapter.getattr("_pool_block")?)?
    {
        return Ok(false);
    }
    let headers = manager.getattr("headers")?;
    if !headers.is_exact_instance_of::<PyDict>() || !headers.is_empty()? {
        return Ok(false);
    }
    let pools = manager.getattr("pools")?;
    let Some(maximum_pools) = exact_usize(pools.getattr("_maxsize")?) else {
        return Ok(false);
    };
    Ok(
        maximum_pools == exact_usize(adapter.getattr("_pool_connections")?).unwrap_or(usize::MAX)
            && pools.getattr("dispose_func")?.is_none(),
    )
}

fn registered_adapter_pristine(py: Python<'_>, adapter: &Bound<'_, PyAny>) -> PyResult<bool> {
    let identity = adapter_id(py, adapter)?;
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let Some(entry) = table.get(&identity) else {
        return Ok(false);
    };
    let referent = entry.weak_adapter.bind(py).call0()?;
    let manager = adapter.getattr("poolmanager")?;
    let proxy_managers = adapter.getattr("proxy_manager")?;
    let Ok(proxy_managers) = proxy_managers.cast::<PyDict>() else {
        return Ok(false);
    };
    if proxy_managers.len() != entry.proxy_managers.len() {
        return Ok(false);
    }
    for (url, expected) in &entry.proxy_managers {
        let Some(current) = proxy_managers.get_item(url)? else {
            return Ok(false);
        };
        if !manager_proof_is_pristine(py, &current, expected)? {
            return Ok(false);
        }
    }
    if !manager_proof_is_pristine(py, &manager, &entry.manager_proof)? {
        return Ok(false);
    }
    Ok(referent.is(adapter)
        && entry.poolmanager.bind(py).is(&manager)
        && manager_identity_is_pristine(py, &manager)?
        && manager_configuration_is_pristine(adapter, &manager)?
        && manager_pool_count(&manager)? == entry.visible_pool_count)
}

#[pyfunction]
fn _adapter_register_trial(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    callback: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !adapter
        .get_type()
        .as_any()
        .is(adapter_state(py)?.adapter_type.bind(py))
    {
        return Ok(false);
    }
    let identity = adapter_id(py, adapter)?;
    let manager = adapter.getattr("poolmanager")?;
    if !manager_identity_is_pristine(py, &manager)?
        || !manager_configuration_is_pristine(adapter, &manager)?
    {
        return Ok(false);
    }
    let proxy_managers = adapter.getattr("proxy_manager")?;
    if !proxy_managers.is_exact_instance_of::<PyDict>() || !proxy_managers.is_empty()? {
        return Ok(false);
    }
    let visible_pool_count = manager_pool_count(&manager)?;
    let weak_adapter = PyModule::import(py, "weakref")?
        .getattr("ref")?
        .call1((adapter, callback))?
        .unbind();
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    if let Some(previous) = table.insert(
        identity,
        SideEntry {
            weak_adapter,
            poolmanager: manager.clone().unbind(),
            manager_proof: manager_proof(&manager)?,
            visible_pool_count,
            direct_pools: PoolRealm::default(),
            proxy_pools: HashMap::new(),
            proxy_managers: HashMap::new(),
        },
    ) {
        clear_realms(previous);
    }
    Ok(true)
}

fn record_visible_proxy_manager(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    proxy_url: &str,
) -> PyResult<bool> {
    let proxy_managers = adapter.getattr("proxy_manager")?;
    let Ok(proxy_managers) = proxy_managers.cast::<PyDict>() else {
        return Ok(false);
    };
    let Some(manager) = proxy_managers.get_item(proxy_url)? else {
        return Ok(false);
    };
    let identity = adapter_id(py, adapter)?;
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let Some(entry) = table.get_mut(&identity) else {
        return Ok(false);
    };
    match entry.proxy_managers.get(proxy_url) {
        Some(expected) => manager_proof_is_pristine(py, &manager, expected),
        None if proxy_managers.len() == entry.proxy_managers.len() + 1 => {
            entry
                .proxy_managers
                .insert(proxy_url.to_owned(), manager_proof(&manager)?);
            Ok(true)
        }
        None => Ok(false),
    }
}

#[pyfunction]
fn _adapter_drop_trial(identity: usize) -> PyResult<usize> {
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let Some(entry) = table.remove(&identity) else {
        return Ok(0);
    };
    let count = realm_pool_count(&entry);
    clear_realms(entry);
    Ok(count)
}

fn realm_pool_count(entry: &SideEntry) -> usize {
    entry.direct_pools.pools.len()
        + entry
            .proxy_pools
            .values()
            .map(|realm| realm.pools.len())
            .sum::<usize>()
}

fn clear_realms(entry: SideEntry) {
    for pool in entry.direct_pools.pools.values() {
        pool.clear();
    }
    for realm in entry.proxy_pools.values() {
        for pool in realm.pools.values() {
            pool.clear();
        }
    }
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
    if let Some(entry) = table.get_mut(&identity) {
        let referent = entry.weak_adapter.bind(py).call0()?;
        let poolmanager = adapter.getattr("poolmanager")?;
        if !referent.is(adapter) || !entry.poolmanager.bind(py).is(&poolmanager) {
            return Ok(Err("visible pool manager identity changed".to_owned()));
        }
        let realm = match input.selected_proxy.as_deref() {
            Some(proxy) => entry.proxy_pools.entry(proxy.to_owned()).or_default(),
            None => &mut entry.direct_pools,
        };
        if let Some(pool) = realm.pools.get(&input.pool_key) {
            let pool = Arc::clone(pool);
            realm.order.retain(|key| key != &input.pool_key);
            realm.order.push_back(input.pool_key.clone());
            return Ok(Ok(pool));
        }
        let pool = Arc::new(
            AdapterPool::new(
                input.pool_maxsize,
                input.pool_block,
                input.proxy.clone(),
                input.tls.clone(),
                input.timeout,
            )
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
        );
        while realm.pools.len() >= input.pool_connections && input.pool_connections > 0 {
            let Some(evicted_key) = realm.order.pop_front() else {
                break;
            };
            if let Some(evicted) = realm.pools.remove(&evicted_key) {
                evicted.clear();
            }
        }
        if input.pool_connections > 0 {
            realm
                .pools
                .insert(input.pool_key.clone(), Arc::clone(&pool));
            realm.order.push_back(input.pool_key.clone());
        }
        return Ok(Ok(pool));
    }
    Ok(Err(
        "adapter was not registered at initialization".to_owned()
    ))
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
    if retry_after > 0.0 {
        PyModule::import(py, "time")?
            .getattr("sleep")?
            .call1((retry_after,))?;
        return Ok(());
    }
    let random_unit = if state.should_observe_random() {
        PyModule::import(py, "random")?
            .getattr("random")?
            .call0()?
            .extract::<f64>()?
    } else {
        0.0
    };
    let backoff = state.backoff(random_unit);
    let delay = backoff;
    if delay > 0.0 {
        PyModule::import(py, "time")?
            .getattr("sleep")?
            .call1((delay,))?;
    }
    Ok(())
}

fn drain_response(py: Python<'_>, response: AdapterResponse) {
    let mut body = response.into_raw_body();
    let _ = py.detach(move || {
        let mut drained = Vec::new();
        body.read_to_end(&mut drained)
    });
}

fn redirect_location(status: u16, headers: &HeaderMap) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
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
        ErrorKind::Connect | ErrorKind::ConnectTimeout | ErrorKind::Dns => RetryReason::Connect,
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
    response: AdapterResponse,
) -> PyResult<Py<PyAny>> {
    let status = response.status().as_u16();
    let reason = response.reason().to_owned();
    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let headers = response_headers(py, response.headers())?;
    let raw = Py::new(
        py,
        NativeAdapterRaw {
            body: Some(response.into_raw_body()),
            content_encoding,
            decoder: None,
            decoded: Vec::new(),
            decoded_offset: 0,
            decoder_eof: false,
            decode_started: false,
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
        if !record_visible_proxy_manager(py, adapter, proxy)? {
            return Ok(py.NotImplemented());
        }
    } else {
        adapter
            .getattr("poolmanager")?
            .call_method1("connection_from_url", (&input.url,))?;
        let identity = adapter_id(py, adapter)?;
        let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut table = table
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get_mut(&identity) else {
            return Ok(py.NotImplemented());
        };
        entry.visible_pool_count = manager_pool_count(entry.poolmanager.bind(py))?;
        refresh_manager_pools(py, &mut entry.manager_proof)?;
    }
    if !adapter_identity_is_pristine(py, adapter, request)?
        || !registered_adapter_pristine(py, adapter)?
    {
        return Ok(py.NotImplemented());
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
                if matches!(reason, RetryReason::Read)
                    && !retry_state.allows_method(&input.method_name)
                {
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
        let location = redirect_location(status, response.headers());
        let has_retry_after = response
            .headers()
            .get("retry-after")
            .is_some_and(|value| !value.as_bytes().is_empty());
        if !retry_state.is_retry(&input.method_name, status, has_retry_after) {
            return build_python_response(py, adapter, request, response);
        }
        let incremented = retry_state.increment(
            RetryReason::Status { status },
            &input.method_name,
            &input.url,
            location.as_deref(),
        );
        let next = match incremented {
            Ok(next) => next,
            Err(_) if input.retry.policy.raise_on_status => {
                drain_response(py, response);
                let message = format!("too many {status} responses");
                return Err(requests_exception(py, "RetryError", message, request));
            }
            Err(_) => return build_python_response(py, adapter, request, response),
        };
        let headers = response.headers().clone();
        drain_response(py, response);
        let retry_after = if input.retry.policy.respect_retry_after && has_retry_after {
            retry_after(py, &retry_object, &headers)?
        } else {
            0.0
        };
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
    let Some(entry) = table.get_mut(&identity) else {
        return Ok(0);
    };
    let count = realm_pool_count(entry);
    for pool in entry.direct_pools.pools.values() {
        pool.clear();
    }
    entry.direct_pools.pools.clear();
    entry.direct_pools.order.clear();
    for realm in entry.proxy_pools.values_mut() {
        for pool in realm.pools.values() {
            pool.clear();
        }
        realm.pools.clear();
        realm.order.clear();
    }
    entry.visible_pool_count = manager_pool_count(entry.poolmanager.bind(py))?;
    refresh_manager_pools(py, &mut entry.manager_proof)?;
    Ok(count)
}

#[pyfunction]
fn _adapter_pool_side_table_trial(py: Python<'_>) -> PyResult<usize> {
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let _ = py;
    Ok(table
        .values()
        .filter(|entry| realm_pool_count(entry) != 0)
        .count())
}

impl NativeAdapterRaw {
    fn decoder(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let Some(encoding) = self.content_encoding.as_deref() else {
            return Ok(None);
        };
        let encoding = encoding.to_ascii_lowercase();
        let response_module = PyModule::import(py, "urllib3.response")?;
        let content_decoders = response_module
            .getattr("HTTPResponse")?
            .getattr("CONTENT_DECODERS")?;
        let supported = |coding: &str| content_decoders.contains(coding);
        if !supported(&encoding)?
            && (!encoding.contains(',')
                || !encoding
                    .split(',')
                    .map(str::trim)
                    .any(|coding| supported(coding).unwrap_or(false)))
        {
            return Ok(None);
        }
        if self.decoder.is_none() {
            self.decoder = Some(
                response_module
                    .getattr("_get_decoder")?
                    .call1((&encoding,))?
                    .unbind(),
            );
        }
        Ok(self.decoder.as_ref().map(|decoder| decoder.clone_ref(py)))
    }

    fn decoder_is_bounded(decoder: &Bound<'_, PyAny>) -> PyResult<bool> {
        decoder.hasattr("has_unconsumed_tail")
    }

    fn decompress(
        decoder: &Bound<'_, PyAny>,
        py: Python<'_>,
        wire: &[u8],
        maximum: isize,
    ) -> PyResult<Vec<u8>> {
        let wire = PyBytes::new(py, wire);
        if Self::decoder_is_bounded(decoder)? {
            decoder
                .call_method1("decompress", (wire, maximum))?
                .extract()
        } else {
            decoder.call_method1("decompress", (wire,))?.extract()
        }
    }

    fn fill_decoded(&mut self, py: Python<'_>, wanted: Option<usize>) -> PyResult<()> {
        if self.decoded_offset > 0 {
            self.decoded.drain(..self.decoded_offset);
            self.decoded_offset = 0;
        }
        while !self.decoder_eof && wanted.is_none_or(|wanted| self.decoded.len() < wanted) {
            let decoder = self.decoder(py)?;
            let has_tail = match &decoder {
                Some(decoder) if Self::decoder_is_bounded(decoder.bind(py))? => decoder
                    .bind(py)
                    .getattr("has_unconsumed_tail")?
                    .is_truthy()?,
                _ => false,
            };
            let mut wire = if has_tail { Vec::new() } else { vec![0; 8192] };
            let read = if has_tail {
                0
            } else {
                match self.body.as_mut() {
                    Some(body) => py
                        .detach(|| body.read(&mut wire))
                        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
                    None => 0,
                }
            };
            wire.truncate(read);
            if read == 0 && !has_tail {
                self.body = None;
                if let Some(decoder) = decoder {
                    self.decoded
                        .extend(Self::decompress(decoder.bind(py), py, b"", -1)?);
                    self.decoded.extend(
                        decoder
                            .bind(py)
                            .call_method0("flush")?
                            .extract::<Vec<u8>>()?,
                    );
                }
                self.decoder_eof = true;
                break;
            }
            if let Some(decoder) = decoder {
                if Self::decoder_is_bounded(decoder.bind(py))? {
                    self.decode_started = true;
                }
                let maximum = wanted
                    .map(|wanted| {
                        isize::try_from(wanted.saturating_sub(self.decoded.len()))
                            .unwrap_or(isize::MAX)
                    })
                    .unwrap_or(-1);
                self.decoded
                    .extend(Self::decompress(decoder.bind(py), py, &wire, maximum)?);
            } else {
                self.decoded.extend(wire);
            }
        }
        Ok(())
    }

    fn read_amount(
        &mut self,
        py: Python<'_>,
        amount: Option<usize>,
        decode_content: bool,
    ) -> PyResult<Py<PyAny>> {
        if amount == Some(0) {
            return Ok(PyBytes::new(py, b"").into_any().unbind());
        }
        if self.closed {
            return Ok(PyBytes::new(py, b"").into_any().unbind());
        }
        if decode_content && self.decoder(py)?.is_some() {
            self.fill_decoded(py, amount)?;
            let end = amount
                .map(|amount| {
                    self.decoded_offset
                        .saturating_add(amount)
                        .min(self.decoded.len())
                })
                .unwrap_or(self.decoded.len());
            let bytes = &self.decoded[self.decoded_offset..end];
            self.decoded_offset = end;
            if self.decoder_eof && self.decoded_offset == self.decoded.len() {
                self.closed = true;
            }
            return Ok(PyBytes::new(py, bytes).into_any().unbind());
        }
        if self.decode_started {
            return Err(PyRuntimeError::new_err(
                "Calling read(decode_content=False) is not supported after read(decode_content=True) was called.",
            ));
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
        let _ = cache_content;
        let amount = match amt {
            None => None,
            Some(value) if value.is_none() => None,
            Some(value) => match value.extract::<isize>()? {
                value if value < 0 => None,
                value => Some(value as usize),
            },
        };
        self.read_amount(py, amount, decode_content)
    }

    #[pyo3(signature = (amt=65_536, decode_content=None))]
    fn stream(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        amt: Option<isize>,
        decode_content: Option<bool>,
    ) -> PyResult<Py<NativeAdapterStream>> {
        let amount = match amt {
            None => None,
            Some(value) if value < 0 => None,
            Some(value) => Some(value as usize),
        };
        Py::new(
            py,
            NativeAdapterStream {
                raw: slf.into_pyobject(py)?.unbind(),
                amount,
                decode_content: decode_content.unwrap_or(false),
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

    fn _retained_decoded_bytes_trial(&self) -> usize {
        self.decoded.len().saturating_sub(self.decoded_offset)
    }
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
        if self.amount == Some(0) {
            self.done = true;
            return Ok(None);
        }
        let chunk =
            self.raw
                .bind(py)
                .borrow_mut()
                .read_amount(py, self.amount, self.decode_content)?;
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
    module.add_function(wrap_pyfunction!(_adapter_register_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_drop_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_send_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_close_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_adapter_pool_side_table_trial, module)?)?;
    Ok(())
}

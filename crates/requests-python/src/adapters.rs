use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pyo3::exceptions::{PyNameError, PyRuntimeError, PyTypeError, PyValueError};
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

use crate::errors::{exception_matches, map_decoder_error, map_typed_response_error};

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
    http_pool_classes_by_scheme: MappingProof,
    socks_pool_classes_by_scheme: Option<MappingProof>,
    key_fn_by_scheme: MappingProof,
    methods: Vec<(String, BehaviorProof)>,
    globals: Vec<(String, BehaviorProof)>,
    send_globals: Py<PyDict>,
    send_builtins: Py<PyDict>,
    exception_globals: Vec<AdapterGlobalProof>,
}

#[derive(Clone, Copy)]
enum GlobalLocation {
    Module,
    Builtins,
}

struct AdapterGlobalProof {
    name: String,
    location: GlobalLocation,
    value: Py<PyAny>,
    behavior: BehaviorProof,
}

static ADAPTER_STATE: PyOnceLock<AdapterState> = PyOnceLock::new();

struct SideEntry {
    lifecycle_epoch: u64,
    admission_revision: u64,
    proof_revision: u64,
    weak_adapter: Arc<Py<PyAny>>,
    proxy_mapping: Arc<Py<PyAny>>,
    manager: ManagerRecord,
    direct_pools: PoolRealm,
    proxy_pools: HashMap<String, PoolRealm>,
    proxy_managers: HashMap<String, ManagerRecord>,
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
    default_items: Option<SequenceProof>,
    kwdefaults: Py<PyAny>,
    kwdefault_items: Option<MappingProof>,
    closure: Py<PyAny>,
    closure_cells: Vec<ClosureCellProof>,
    attributes: MappingProof,
    annotations: MappingProof,
}

struct ClosureCellProof {
    cell: Py<PyAny>,
    contents: Option<Py<PyAny>>,
    behavior: Option<BehaviorProof>,
}

struct DictProof {
    items: Vec<(String, Py<PyAny>)>,
    behaviors: Vec<BehaviorProof>,
}

struct MappingProof {
    mapping: Py<PyAny>,
    items: ObjectItems,
    behaviors: Vec<BehaviorProof>,
}

struct SequenceProof {
    sequence: Py<PyAny>,
    items: Vec<Py<PyAny>>,
    behaviors: Vec<BehaviorProof>,
}

struct ClassProof {
    dictionary: DictProof,
    bases: SequenceProof,
}

enum BehaviorDetails {
    Function(CallableProof),
    Class(Box<ClassProof>),
    Partial {
        function: Box<BehaviorProof>,
        arguments: SequenceProof,
        keywords: Option<MappingProof>,
    },
    Descriptor(Vec<(String, BehaviorProof)>),
    Mapping(Box<MappingProof>),
}

struct BehaviorProof {
    object: Py<PyAny>,
    details: Option<BehaviorDetails>,
}

struct ManagerProof {
    manager: Py<PyAny>,
    manager_type: Py<PyAny>,
    class_behavior: BehaviorProof,
    objects: DictProof,
    mappings: Vec<(String, MappingProof)>,
    pools_behavior: BehaviorProof,
}

struct ManagerPoolsProof {
    pools: Py<PyAny>,
    pools_dict: DictProof,
    pool_container: MappingProof,
    visible_pools: Vec<(Py<PyAny>, Py<PyAny>)>,
    visible_pool_count: usize,
}

#[derive(Clone)]
struct ManagerRecord {
    proof: Arc<ManagerProof>,
    pools: Arc<ManagerPoolsProof>,
}

struct AdapterPoolSelection {
    pool: Arc<AdapterPool>,
    identity: usize,
    lifecycle_epoch: u64,
}

static ADAPTER_POOLS: OnceLock<Mutex<HashMap<usize, SideEntry>>> = OnceLock::new();
static NEXT_ADAPTER_LIFECYCLE_EPOCH: AtomicU64 = AtomicU64::new(1);
const MANAGER_PROOF_ATTEMPTS: usize = 8;

#[pyclass(module = "requests._requests_rust", unsendable)]
struct NativeAdapterRaw {
    body: Option<AdapterResponseBody>,
    pool: Py<PyAny>,
    content_encoding: Option<String>,
    decoder: Option<Py<PyAny>>,
    decoded: Vec<u8>,
    decoded_offset: usize,
    decoder_eof: bool,
    decode_started: bool,
    decode_failed: bool,
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

fn object_identity(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<usize> {
    PyModule::import(py, "builtins")?
        .getattr("id")?
        .call1((value,))?
        .extract()
}

fn callable_proof(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    visited: &mut HashSet<usize>,
) -> PyResult<CallableProof> {
    let closure = value.getattr("__closure__")?;
    let mut closure_cells = Vec::new();
    if !closure.is_none() {
        for cell in closure.try_iter()? {
            let cell = cell?;
            let contents = cell.getattr("cell_contents").ok().map(Bound::unbind);
            let behavior = contents
                .as_ref()
                .map(|contents| behavior_proof_inner(py, contents.bind(py), visited))
                .transpose()?;
            closure_cells.push(ClosureCellProof {
                cell: cell.unbind(),
                contents,
                behavior,
            });
        }
    }
    let defaults = value.getattr("__defaults__")?;
    let default_items = if defaults.is_none() {
        None
    } else {
        Some(sequence_proof_inner(py, &defaults, visited)?)
    };
    let kwdefaults = value.getattr("__kwdefaults__")?;
    let kwdefault_items = if kwdefaults.is_none() {
        None
    } else {
        Some(mapping_proof_inner(py, &kwdefaults, visited)?)
    };
    Ok(CallableProof {
        function: value.clone().unbind(),
        code: value.getattr("__code__")?.unbind(),
        defaults: defaults.unbind(),
        default_items,
        kwdefaults: kwdefaults.unbind(),
        kwdefault_items,
        closure: closure.unbind(),
        closure_cells,
        attributes: mapping_proof_inner(py, &value.getattr("__dict__")?, visited)?,
        annotations: mapping_proof_inner(py, &value.getattr("__annotations__")?, visited)?,
    })
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
    if let Some(expected) = &proof.default_items
        && !sequence_proof_is_pristine(py, expected)?
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
    for (cell, expected) in current.iter().zip(&proof.closure_cells) {
        if !cell.is(expected.cell.bind(py)) {
            return Ok(false);
        }
        let contents = cell.getattr("cell_contents").ok();
        match (contents, &expected.contents) {
            (None, None) => {}
            (Some(contents), Some(expected)) if contents.is(expected.bind(py)) => {}
            _ => return Ok(false),
        }
        if let Some(behavior) = &expected.behavior
            && !behavior_proof_is_pristine(py, behavior.object.bind(py), behavior)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dict_proof_inner(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    visited: &mut HashSet<usize>,
) -> PyResult<DictProof> {
    let items = dict_snapshot(value)?;
    let behaviors = items
        .iter()
        .map(|(_, item)| behavior_proof_inner(py, item.bind(py), visited))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(DictProof { items, behaviors })
}

fn dict_proof(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<DictProof> {
    dict_proof_inner(py, value, &mut HashSet::new())
}

fn dict_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &DictProof,
) -> PyResult<bool> {
    if !exact_dict_snapshot(py, value, &proof.items)? {
        return Ok(false);
    }
    for behavior in &proof.behaviors {
        if !behavior_proof_is_pristine(py, behavior.object.bind(py), behavior)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sequence_proof_inner(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    visited: &mut HashSet<usize>,
) -> PyResult<SequenceProof> {
    let items = value
        .try_iter()?
        .map(|item| item.map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    let behaviors = items
        .iter()
        .map(|item| behavior_proof_inner(py, item.bind(py), visited))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(SequenceProof {
        sequence: value.clone().unbind(),
        items,
        behaviors,
    })
}

fn behavior_proof_inner(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    visited: &mut HashSet<usize>,
) -> PyResult<BehaviorProof> {
    if !visited.insert(object_identity(py, value)?) {
        return Ok(BehaviorProof {
            object: value.clone().unbind(),
            details: None,
        });
    }
    let function_type = PyModule::import(py, "types")?.getattr("FunctionType")?;
    let type_type = PyModule::import(py, "builtins")?.getattr("type")?;
    let partial_type = PyModule::import(py, "functools")?.getattr("partial")?;
    let details = if value.is_instance(&function_type)? {
        Some(BehaviorDetails::Function(callable_proof(
            py, value, visited,
        )?))
    } else if value.is_instance(&partial_type)? {
        let function = Box::new(behavior_proof_inner(py, &value.getattr("func")?, visited)?);
        let arguments = sequence_proof_inner(py, &value.getattr("args")?, visited)?;
        let keywords = value.getattr("keywords")?;
        let keywords = (!keywords.is_none())
            .then(|| mapping_proof_inner(py, &keywords, visited))
            .transpose()?;
        Some(BehaviorDetails::Partial {
            function,
            arguments,
            keywords,
        })
    } else if value.is_instance(&type_type)? {
        let bases = sequence_proof_inner(py, &value.getattr("__bases__")?, visited)?;
        let dictionary = dict_proof_inner(py, &value.getattr("__dict__")?, visited)?;
        Some(BehaviorDetails::Class(Box::new(ClassProof {
            dictionary,
            bases,
        })))
    } else {
        let mut members = Vec::new();
        for name in ["__func__", "fget", "fset", "fdel"] {
            if let Ok(member) = value.getattr(name)
                && !member.is_none()
            {
                members.push((name.to_owned(), behavior_proof_inner(py, &member, visited)?));
            }
        }
        if !members.is_empty() {
            Some(BehaviorDetails::Descriptor(members))
        } else {
            None
        }
    };
    Ok(BehaviorProof {
        object: value.clone().unbind(),
        details,
    })
}

fn behavior_proof(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<BehaviorProof> {
    behavior_proof_inner(py, value, &mut HashSet::new())
}

fn sequence_proof_is_pristine(py: Python<'_>, proof: &SequenceProof) -> PyResult<bool> {
    let sequence = proof.sequence.bind(py);
    let items = sequence.try_iter()?.collect::<PyResult<Vec<_>>>()?;
    if items.len() != proof.items.len()
        || items
            .iter()
            .zip(&proof.items)
            .any(|(item, expected)| !item.is(expected.bind(py)))
    {
        return Ok(false);
    }
    for behavior in &proof.behaviors {
        if !behavior_proof_is_pristine(py, behavior.object.bind(py), behavior)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn behavior_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &BehaviorProof,
) -> PyResult<bool> {
    if !value.is(proof.object.bind(py)) {
        return Ok(false);
    }
    match &proof.details {
        Some(BehaviorDetails::Function(callable)) => callable_proof_is_pristine(py, callable),
        Some(BehaviorDetails::Class(class_proof)) => Ok(value
            .getattr("__bases__")?
            .is(class_proof.bases.sequence.bind(py))
            && sequence_proof_is_pristine(py, &class_proof.bases)?
            && dict_proof_is_pristine(py, &value.getattr("__dict__")?, &class_proof.dictionary)?),
        Some(BehaviorDetails::Partial {
            function,
            arguments,
            keywords,
        }) => {
            if !behavior_proof_is_pristine(py, &value.getattr("func")?, function)?
                || !value.getattr("args")?.is(arguments.sequence.bind(py))
                || !sequence_proof_is_pristine(py, arguments)?
            {
                return Ok(false);
            }
            let current = value.getattr("keywords")?;
            match keywords {
                Some(keywords) => mapping_proof_is_pristine(py, &current, keywords),
                None => Ok(current.is_none()),
            }
        }
        Some(BehaviorDetails::Descriptor(members)) => {
            for (name, behavior) in members {
                if !behavior_proof_is_pristine(py, &value.getattr(name.as_str())?, behavior)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Some(BehaviorDetails::Mapping(mapping)) => mapping_proof_is_pristine(py, value, mapping),
        None => Ok(true),
    }
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
    urllib3_url: String,
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

#[derive(Clone, Copy)]
struct ProxyManagerConfiguration {
    pool_connections: usize,
    pool_maxsize: usize,
    pool_block: bool,
}

impl From<&NativeSendInput> for ProxyManagerConfiguration {
    fn from(input: &NativeSendInput) -> Self {
        Self {
            pool_connections: input.pool_connections,
            pool_maxsize: input.pool_maxsize,
            pool_block: input.pool_block,
        }
    }
}

fn initialize_adapter_state(py: Python<'_>) -> PyResult<AdapterState> {
    let adapters = PyModule::import(py, "requests.adapters")?;
    let adapter_type = adapters.getattr("HTTPAdapter")?;
    let send = adapter_type.getattr("send")?;
    let send_globals = send.getattr("__globals__")?.cast_into::<PyDict>()?;
    let send_builtins = send.getattr("__builtins__")?.cast_into::<PyDict>()?;
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
            .and_then(|value| behavior_proof(py, &value).map(|proof| (name.to_owned(), proof)))
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
            .and_then(|value| behavior_proof(py, &value).map(|proof| (name.to_owned(), proof)))
    })
    .collect::<PyResult<Vec<_>>>()?;
    let poolmanager_type = adapters.getattr("PoolManager")?;
    let poolmanager_module = PyModule::import(py, "urllib3.poolmanager")?;
    let poolmanager_behavior = behavior_proof(py, &poolmanager_type)?;
    let proxy_manager_behavior = behavior_proof(py, &poolmanager_module.getattr("ProxyManager")?)?;
    let socks_manager = adapters.getattr("SOCKSProxyManager")?;
    let socks_manager_behavior = behavior_proof(py, &socks_manager)?;
    let http_pool_classes_by_scheme =
        routing_mapping_proof(&poolmanager_module.getattr("pool_classes_by_scheme")?)?;
    let socks_pool_classes_by_scheme = socks_manager
        .getattr("pool_classes_by_scheme")
        .ok()
        .map(|mapping| routing_mapping_proof(&mapping))
        .transpose()?;
    let key_fn_by_scheme = routing_mapping_proof(&poolmanager_module.getattr("key_fn_by_scheme")?)?;
    let prepared_getattribute = prepared_request_type.getattr("__getattribute__")?.unbind();
    let exception_globals = [
        "LocationValueError",
        "InvalidURL",
        "ProtocolError",
        "OSError",
        "MaxRetryError",
        "ConnectTimeoutError",
        "NewConnectionError",
        "ConnectTimeout",
        "ResponseError",
        "RetryError",
        "_ProxyError",
        "ProxyError",
        "_SSLError",
        "SSLError",
        "ClosedPoolError",
        "_HTTPError",
        "ReadTimeoutError",
        "ReadTimeout",
        "_InvalidHeader",
        "InvalidHeader",
        "ConnectionError",
        "isinstance",
    ]
    .into_iter()
    .map(|name| {
        let (location, value) = match send_globals.get_item(name)? {
            Some(value) => (GlobalLocation::Module, value),
            None => (
                GlobalLocation::Builtins,
                send_builtins
                    .get_item(name)?
                    .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))?,
            ),
        };
        Ok(AdapterGlobalProof {
            name: name.to_owned(),
            location,
            behavior: behavior_proof(py, &value)?,
            value: value.unbind(),
        })
    })
    .collect::<PyResult<Vec<_>>>()?;
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
        http_pool_classes_by_scheme,
        socks_pool_classes_by_scheme,
        key_fn_by_scheme,
        methods,
        globals,
        send_globals: send_globals.unbind(),
        send_builtins: send_builtins.unbind(),
        exception_globals,
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
    for (name, proof) in &state.methods {
        if !behavior_proof_is_pristine(py, &adapter_type.getattr(name.as_str())?, proof)? {
            return Ok(false);
        }
    }
    let module = state.adapters_module.bind(py);
    for (name, proof) in &state.globals {
        let current = match module.getattr(name.as_str()) {
            Ok(current) => current,
            Err(_) if name == "SOCKSProxyManager" => return Ok(false),
            Err(error) => return Err(error),
        };
        if name == "SOCKSProxyManager" {
            if !matches!(behavior_proof_is_pristine(py, &current, proof), Ok(true)) {
                return Ok(false);
            }
        } else if !behavior_proof_is_pristine(py, &current, proof)? {
            return Ok(false);
        }
    }
    let send = adapter_type.getattr("send")?;
    if !send.getattr("__globals__")?.is(state.send_globals.bind(py))
        || !send
            .getattr("__builtins__")?
            .is(state.send_builtins.bind(py))
    {
        return Ok(false);
    }
    let module_dictionary = state.send_globals.bind(py);
    let builtins = state.send_builtins.bind(py);
    for proof in &state.exception_globals {
        let current = match proof.location {
            GlobalLocation::Module => module_dictionary.get_item(&proof.name)?,
            GlobalLocation::Builtins if module_dictionary.contains(&proof.name)? => {
                return Ok(false);
            }
            GlobalLocation::Builtins => builtins.get_item(&proof.name)?,
        };
        let Some(current) = current else {
            return Ok(false);
        };
        if !current.is(proof.value.bind(py))
            || !behavior_proof_is_pristine(py, &current, &proof.behavior)?
        {
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
    {
        return Ok(false);
    }
    let Ok(socks_manager) = module.getattr("SOCKSProxyManager") else {
        return Ok(false);
    };
    if !matches!(
        behavior_proof_is_pristine(py, &socks_manager, &state.socks_manager_behavior),
        Ok(true)
    ) {
        return Ok(false);
    }
    canonical_routing_sources_are_pristine(py, state, &poolmanager_module, module)
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
    let urllib3_url = match selected_proxy.as_ref() {
        Some(_) => url.clone(),
        None => {
            let path_url = request.getattr("path_url")?;
            if !path_url.is_exact_instance_of::<PyString>() {
                return Ok(Err("request path URL is unsupported".to_owned()));
            }
            path_url.extract::<String>()?
        }
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
        urllib3_url,
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

fn next_adapter_lifecycle_epoch() -> u64 {
    NEXT_ADAPTER_LIFECYCLE_EPOCH.fetch_add(1, Ordering::Relaxed)
}

fn manager_pools_proof(manager: &Bound<'_, PyAny>) -> PyResult<ManagerPoolsProof> {
    let py = manager.py();
    let pools = manager.getattr("pools")?;
    let pool_container = mapping_proof(&pools.getattr("_container")?)?;
    let visible_pools = pool_container
        .items
        .iter()
        .map(|(key, value)| (key.clone_ref(py), value.clone_ref(py)))
        .collect::<Vec<_>>();
    let visible_pool_count = visible_pools.len();
    Ok(ManagerPoolsProof {
        pools: pools.clone().unbind(),
        pools_dict: dict_proof(py, &pools.getattr("__dict__")?)?,
        pool_container,
        visible_pools,
        visible_pool_count,
    })
}

fn stable_manager_pools_proof(
    manager: &Bound<'_, PyAny>,
) -> PyResult<Option<Arc<ManagerPoolsProof>>> {
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let proof = Arc::new(manager_pools_proof(manager)?);
        if manager_pools_proof_is_pristine(manager.py(), manager, proof.as_ref())? {
            return Ok(Some(proof));
        }
    }
    Ok(None)
}

fn manager_proof(
    manager: &Bound<'_, PyAny>,
    pool_classes_by_scheme: &MappingProof,
    key_fn_by_scheme: &MappingProof,
) -> PyResult<Option<ManagerRecord>> {
    let py = manager.py();
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let mut mappings = Vec::new();
        for name in [
            "headers",
            "connection_pool_kw",
            "pool_classes_by_scheme",
            "key_fn_by_scheme",
            "proxy_headers",
        ] {
            if let Ok(mapping) = manager.getattr(name) {
                let proof = if matches!(name, "pool_classes_by_scheme" | "key_fn_by_scheme") {
                    let Some(proof) = routing_mapping_snapshot(&mapping)? else {
                        return Ok(None);
                    };
                    proof
                } else {
                    mapping_proof(&mapping)?
                };
                mappings.push((name.to_owned(), proof));
            }
        }
        let Some(candidate_pool_classes) = mappings
            .iter()
            .find(|(name, _)| name == "pool_classes_by_scheme")
            .map(|(_, proof)| proof)
        else {
            return Ok(None);
        };
        let Some(candidate_key_fns) = mappings
            .iter()
            .find(|(name, _)| name == "key_fn_by_scheme")
            .map(|(_, proof)| proof)
        else {
            return Ok(None);
        };
        if !routing_mapping_matches_canonical(py, candidate_pool_classes, pool_classes_by_scheme)?
            || !routing_mapping_matches_canonical(py, candidate_key_fns, key_fn_by_scheme)?
        {
            return Ok(None);
        }
        let pools = manager.getattr("pools")?;
        let proof = Arc::new(ManagerProof {
            manager: manager.clone().unbind(),
            manager_type: manager.get_type().into_any().unbind(),
            class_behavior: behavior_proof(py, manager.get_type().as_any())?,
            objects: dict_proof(py, &manager.getattr("__dict__")?)?,
            mappings,
            pools_behavior: behavior_proof(py, pools.get_type().as_any())?,
        });
        let Some(pools) = stable_manager_pools_proof(manager)? else {
            continue;
        };
        let record = ManagerRecord { proof, pools };
        if manager_proof_is_pristine(py, manager, &record)? {
            return Ok(Some(record));
        }
    }
    Ok(None)
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

fn mapping_proof_inner(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    visited: &mut HashSet<usize>,
) -> PyResult<MappingProof> {
    let items = object_items(value)?;
    let mut behaviors = Vec::new();
    for (_, item) in &items {
        let item = item.bind(py);
        if item.hasattr("items")? && visited.insert(object_identity(py, item)?) {
            behaviors.push(BehaviorProof {
                object: item.clone().unbind(),
                details: Some(BehaviorDetails::Mapping(Box::new(mapping_proof_inner(
                    py, item, visited,
                )?))),
            });
        } else {
            behaviors.push(behavior_proof_inner(py, item, visited)?);
        }
    }
    Ok(MappingProof {
        mapping: value.clone().unbind(),
        items,
        behaviors,
    })
}

fn mapping_proof(value: &Bound<'_, PyAny>) -> PyResult<MappingProof> {
    mapping_proof_inner(value.py(), value, &mut HashSet::new())
}

fn routing_mapping_proof(value: &Bound<'_, PyAny>) -> PyResult<MappingProof> {
    let dictionary = value.cast_exact::<PyDict>()?;
    if dictionary
        .iter()
        .any(|(key, _)| !key.is_exact_instance_of::<PyString>())
    {
        return Err(PyTypeError::new_err(
            "canonical routing mapping requires exact string keys",
        ));
    }
    mapping_proof(value)
}

fn routing_mapping_snapshot(value: &Bound<'_, PyAny>) -> PyResult<Option<MappingProof>> {
    let Ok(dictionary) = value.cast_exact::<PyDict>() else {
        return Ok(None);
    };
    let mut items = Vec::with_capacity(dictionary.len());
    for (key, value) in dictionary.iter() {
        if !key.is_exact_instance_of::<PyString>() {
            return Ok(None);
        }
        items.push((key.unbind(), value.unbind()));
    }
    Ok(Some(MappingProof {
        mapping: dictionary.clone().into_any().unbind(),
        items,
        behaviors: Vec::new(),
    }))
}

fn routing_mapping_proof_is_pristine(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    proof: &MappingProof,
) -> PyResult<bool> {
    if !value.is(proof.mapping.bind(py)) {
        return Ok(false);
    }
    let Ok(dictionary) = value.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if dictionary
        .iter()
        .any(|(key, _)| !key.is_exact_instance_of::<PyString>())
    {
        return Ok(false);
    }
    mapping_proof_is_pristine(py, value, proof)
}

fn routing_mapping_matches_canonical(
    py: Python<'_>,
    candidate: &MappingProof,
    canonical: &MappingProof,
) -> PyResult<bool> {
    if !routing_mapping_proof_is_pristine(py, canonical.mapping.bind(py), canonical)?
        || !routing_mapping_proof_is_pristine(py, candidate.mapping.bind(py), candidate)?
        || candidate.items.len() != canonical.items.len()
    {
        return Ok(false);
    }
    Ok(candidate.items.iter().zip(&canonical.items).all(
        |((key, value), (expected_key, expected_value))| {
            key.bind(py).is(expected_key.bind(py)) && value.bind(py).is(expected_value.bind(py))
        },
    ))
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
    for behavior in &proof.behaviors {
        if !behavior_proof_is_pristine(py, behavior.object.bind(py), behavior)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn manager_immutable_proof_is_pristine(
    py: Python<'_>,
    manager: &Bound<'_, PyAny>,
    proof: &ManagerProof,
) -> PyResult<bool> {
    if !manager.is(proof.manager.bind(py))
        || !manager.get_type().as_any().is(proof.manager_type.bind(py))
        || !behavior_proof_is_pristine(py, manager.get_type().as_any(), &proof.class_behavior)?
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
    behavior_proof_is_pristine(py, pools.get_type().as_any(), &proof.pools_behavior)
}

fn manager_proof_is_pristine(
    py: Python<'_>,
    manager: &Bound<'_, PyAny>,
    record: &ManagerRecord,
) -> PyResult<bool> {
    if !manager_immutable_proof_is_pristine(py, manager, record.proof.as_ref())? {
        return Ok(false);
    }
    manager_pools_proof_is_pristine(py, manager, record.pools.as_ref())
}

fn manager_pools_proof_is_pristine(
    py: Python<'_>,
    manager: &Bound<'_, PyAny>,
    proof: &ManagerPoolsProof,
) -> PyResult<bool> {
    let pools = manager.getattr("pools")?;
    if !pools.is(proof.pools.bind(py))
        || !dict_proof_is_pristine(py, &pools.getattr("__dict__")?, &proof.pools_dict)?
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
        )
        && manager_pool_count(manager)? == proof.visible_pool_count)
}

fn frozen_adapter_method<'py>(
    py: Python<'py>,
    state: &'py AdapterState,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    state
        .methods
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, proof)| proof.object.bind(py).clone())
        .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))
}

fn frozen_adapter_global<'py>(
    py: Python<'py>,
    state: &'py AdapterState,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    state
        .globals
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, proof)| proof.object.bind(py).clone())
        .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))
}

fn exact_dict_has_keys(dictionary: &Bound<'_, PyDict>, expected: &[&str]) -> PyResult<bool> {
    if dictionary.len() != expected.len() {
        return Ok(false);
    }
    for (key, _) in dictionary.iter() {
        if !key.is_exact_instance_of::<PyString>() {
            return Ok(false);
        }
        let key = key.extract::<String>()?;
        if !expected.iter().any(|expected| *expected == key) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn exact_python_value_equal(left: &Bound<'_, PyAny>, right: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(left.get_type().as_any().is(right.get_type().as_any()) && left.eq(right)?)
}

fn exact_string_dict_equal(left: &Bound<'_, PyAny>, right: &Bound<'_, PyAny>) -> PyResult<bool> {
    let Ok(left) = left.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    let Ok(right) = right.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if left.len() != right.len() {
        return Ok(false);
    }
    for (key, expected) in right.iter() {
        if !key.is_exact_instance_of::<PyString>() || !expected.is_exact_instance_of::<PyString>() {
            return Ok(false);
        }
        let Some(current) = left.get_item(&key)? else {
            return Ok(false);
        };
        if !current.is_exact_instance_of::<PyString>() || !current.eq(&expected)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn canonical_manager_pool_configuration(
    manager: &Bound<'_, PyAny>,
    configuration: ProxyManagerConfiguration,
) -> PyResult<bool> {
    let headers = manager.getattr("headers")?;
    let Ok(headers) = headers.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if !headers.is_empty() {
        return Ok(false);
    }
    let kwargs = manager.getattr("connection_pool_kw")?;
    let Ok(kwargs) = kwargs.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    let Some(maxsize) = kwargs.get_item("maxsize")? else {
        return Ok(false);
    };
    let Some(block) = kwargs.get_item("block")? else {
        return Ok(false);
    };
    if exact_usize(maxsize) != Some(configuration.pool_maxsize)
        || exact_bool(block)? != Some(configuration.pool_block)
    {
        return Ok(false);
    }
    let pools = manager.getattr("pools")?;
    Ok(
        exact_usize(pools.getattr("_maxsize")?) == Some(configuration.pool_connections)
            && pools.getattr("dispose_func")?.is_none(),
    )
}

fn canonical_routing_mapping_is_pristine(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    canonical: &MappingProof,
) -> PyResult<bool> {
    let Some(current) = routing_mapping_snapshot(current)? else {
        return Ok(false);
    };
    routing_mapping_matches_canonical(py, &current, canonical)
}

fn canonical_routing_sources_are_pristine(
    py: Python<'_>,
    state: &AdapterState,
    poolmanager_module: &Bound<'_, PyModule>,
    adapters_module: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let Ok(http_pool_classes) = poolmanager_module.getattr("pool_classes_by_scheme") else {
        return Ok(false);
    };
    if !matches!(
        routing_mapping_proof_is_pristine(
            py,
            &http_pool_classes,
            &state.http_pool_classes_by_scheme
        ),
        Ok(true)
    ) {
        return Ok(false);
    }
    let Ok(key_functions) = poolmanager_module.getattr("key_fn_by_scheme") else {
        return Ok(false);
    };
    if !matches!(
        routing_mapping_proof_is_pristine(py, &key_functions, &state.key_fn_by_scheme),
        Ok(true)
    ) {
        return Ok(false);
    }
    let Some(socks_pool_classes) = &state.socks_pool_classes_by_scheme else {
        return Ok(true);
    };
    let Ok(socks_manager) = adapters_module.getattr("SOCKSProxyManager") else {
        return Ok(false);
    };
    let Ok(socks_pool_classes_source) = socks_manager.getattr("pool_classes_by_scheme") else {
        return Ok(false);
    };
    Ok(matches!(
        routing_mapping_proof_is_pristine(py, &socks_pool_classes_source, socks_pool_classes),
        Ok(true)
    ))
}

fn canonical_http_proxy_manager_is_pristine(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    proxy_url: &Bound<'_, PyAny>,
    manager: &Bound<'_, PyAny>,
    configuration: ProxyManagerConfiguration,
) -> PyResult<bool> {
    let state = adapter_state(py)?;
    if !manager
        .get_type()
        .as_any()
        .is(state.proxy_manager_behavior.object.bind(py))
        || !canonical_routing_mapping_is_pristine(
            py,
            &manager.getattr("pool_classes_by_scheme")?,
            &state.http_pool_classes_by_scheme,
        )?
        || !canonical_routing_mapping_is_pristine(
            py,
            &manager.getattr("key_fn_by_scheme")?,
            &state.key_fn_by_scheme,
        )?
        || !canonical_manager_pool_configuration(manager, configuration)?
    {
        return Ok(false);
    }
    let proxy = manager.getattr("proxy")?;
    let expected = frozen_adapter_global(py, state, "parse_url")?.call1((proxy_url,))?;
    if !proxy.get_type().as_any().is(expected.get_type().as_any()) {
        return Ok(false);
    }
    for name in ["scheme", "auth", "host", "path", "query", "fragment"] {
        if !exact_python_value_equal(&proxy.getattr(name)?, &expected.getattr(name)?)? {
            return Ok(false);
        }
    }
    let expected_scheme = expected.getattr("scheme")?;
    if !expected_scheme.is_exact_instance_of::<PyString>() {
        return Ok(false);
    }
    let expected_port = expected.getattr("port")?;
    let expected_port = if expected_port.is_none() {
        match expected_scheme
            .extract::<String>()?
            .to_ascii_lowercase()
            .as_str()
        {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        }
    } else {
        exact_usize(expected_port)
    };
    if exact_usize(proxy.getattr("port")?) != expected_port {
        return Ok(false);
    }

    let proxy_headers = manager.getattr("proxy_headers")?;
    let expected_headers =
        frozen_adapter_method(py, state, "proxy_headers")?.call1((adapter, proxy_url))?;
    if !exact_string_dict_equal(&proxy_headers, &expected_headers)? {
        return Ok(false);
    }
    let kwargs = manager.getattr("connection_pool_kw")?;
    let Ok(kwargs) = kwargs.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if !exact_dict_has_keys(
        kwargs,
        &[
            "maxsize",
            "block",
            "_proxy",
            "_proxy_headers",
            "_proxy_config",
        ],
    )? {
        return Ok(false);
    }
    let Some(kw_proxy) = kwargs.get_item("_proxy")? else {
        return Ok(false);
    };
    let Some(kw_headers) = kwargs.get_item("_proxy_headers")? else {
        return Ok(false);
    };
    let Some(kw_config) = kwargs.get_item("_proxy_config")? else {
        return Ok(false);
    };
    let proxy_config = manager.getattr("proxy_config")?;
    if !kw_proxy.is(&proxy)
        || !kw_headers.is(&proxy_headers)
        || !kw_config.is(&proxy_config)
        || !manager.getattr("proxy_ssl_context")?.is_none()
        || !proxy_config.getattr("ssl_context")?.is_none()
        || exact_bool(proxy_config.getattr("use_forwarding_for_https")?)? != Some(false)
    {
        return Ok(false);
    }
    for name in ["assert_hostname", "assert_fingerprint"] {
        if proxy_config.hasattr(name)? && !proxy_config.getattr(name)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn canonical_socks_proxy_manager_is_pristine(
    py: Python<'_>,
    proxy_url: &Bound<'_, PyAny>,
    manager: &Bound<'_, PyAny>,
    configuration: ProxyManagerConfiguration,
) -> PyResult<bool> {
    let state = adapter_state(py)?;
    let Some(pool_classes_by_scheme) = &state.socks_pool_classes_by_scheme else {
        return Ok(false);
    };
    if !manager
        .get_type()
        .as_any()
        .is(state.socks_manager_behavior.object.bind(py))
        || !canonical_routing_mapping_is_pristine(
            py,
            &manager.getattr("pool_classes_by_scheme")?,
            pool_classes_by_scheme,
        )?
        || !canonical_routing_mapping_is_pristine(
            py,
            &manager.getattr("key_fn_by_scheme")?,
            &state.key_fn_by_scheme,
        )?
        || !manager.getattr("proxy_url")?.is(proxy_url)
        || !canonical_manager_pool_configuration(manager, configuration)?
    {
        return Ok(false);
    }
    let parsed = frozen_adapter_global(py, state, "parse_url")?.call1((proxy_url,))?;
    let scheme = parsed.getattr("scheme")?;
    if !scheme.is_exact_instance_of::<PyString>() {
        return Ok(false);
    }
    let (socks_version, rdns) = match scheme.extract::<String>()?.to_ascii_lowercase().as_str() {
        "socks4" => (1, false),
        "socks4a" => (1, true),
        "socks5" => (2, false),
        "socks5h" => (2, true),
        _ => return Ok(false),
    };
    let authentication =
        frozen_adapter_global(py, state, "get_auth_from_url")?.call1((proxy_url,))?;
    let authentication = authentication.cast::<PyTuple>()?;
    if authentication.len() != 2
        || !authentication
            .get_item(0)?
            .is_exact_instance_of::<PyString>()
        || !authentication
            .get_item(1)?
            .is_exact_instance_of::<PyString>()
    {
        return Ok(false);
    }

    let kwargs = manager.getattr("connection_pool_kw")?;
    let Ok(kwargs) = kwargs.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if !exact_dict_has_keys(kwargs, &["maxsize", "block", "_socks_options"])? {
        return Ok(false);
    }
    let Some(options) = kwargs.get_item("_socks_options")? else {
        return Ok(false);
    };
    let Ok(options) = options.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if !exact_dict_has_keys(
        options,
        &[
            "socks_version",
            "proxy_host",
            "proxy_port",
            "username",
            "password",
            "rdns",
        ],
    )? {
        return Ok(false);
    }
    let Some(version) = options.get_item("socks_version")? else {
        return Ok(false);
    };
    let Some(host) = options.get_item("proxy_host")? else {
        return Ok(false);
    };
    let Some(port) = options.get_item("proxy_port")? else {
        return Ok(false);
    };
    let Some(username) = options.get_item("username")? else {
        return Ok(false);
    };
    let Some(password) = options.get_item("password")? else {
        return Ok(false);
    };
    let Some(current_rdns) = options.get_item("rdns")? else {
        return Ok(false);
    };
    let parsed_host = parsed.getattr("host")?;
    let parsed_port = parsed.getattr("port")?;
    Ok(exact_usize(version) == Some(socks_version)
        && host.is_exact_instance_of::<PyString>()
        && exact_python_value_equal(&host, &parsed_host)?
        && ((port.is_none() && parsed_port.is_none())
            || (exact_usize(port.clone()).is_some()
                && exact_python_value_equal(&port, &parsed_port)?))
        && username.is_exact_instance_of::<PyString>()
        && password.is_exact_instance_of::<PyString>()
        && username.eq(&authentication.get_item(0)?)?
        && password.eq(&authentication.get_item(1)?)?
        && exact_bool(current_rdns)? == Some(rdns))
}

fn canonical_proxy_manager_is_pristine(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    proxy_url: &Bound<'_, PyAny>,
    manager: &Bound<'_, PyAny>,
    configuration: ProxyManagerConfiguration,
) -> PyResult<bool> {
    if !proxy_url.is_exact_instance_of::<PyString>() {
        return Ok(false);
    }
    let proxy_url_text = proxy_url.extract::<String>()?;
    if proxy_url_text.to_ascii_lowercase().starts_with("socks") {
        canonical_socks_proxy_manager_is_pristine(py, proxy_url, manager, configuration)
    } else {
        canonical_http_proxy_manager_is_pristine(py, adapter, proxy_url, manager, configuration)
    }
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

fn exact_dict_snapshot_is_current(
    snapshot: &Bound<'_, PyDict>,
    current: &Bound<'_, PyDict>,
) -> PyResult<bool> {
    if snapshot.len() != current.len() {
        return Ok(false);
    }
    for (key, value) in snapshot.iter() {
        if !key.is_exact_instance_of::<PyString>() {
            return Ok(false);
        }
        let Some(current_value) = current.get_item(&key)? else {
            return Ok(false);
        };
        if !current_value.is(&value) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn registered_adapter_pristine(py: Python<'_>, adapter: &Bound<'_, PyAny>) -> PyResult<bool> {
    let identity = adapter_id(py, adapter)?;
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let (
            lifecycle_epoch,
            proof_revision,
            weak_adapter,
            proxy_mapping,
            direct_manager,
            proxy_proofs,
        ) = {
            let table = registry
                .lock()
                .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
            let Some(entry) = table.get(&identity) else {
                return Ok(false);
            };
            (
                entry.lifecycle_epoch,
                entry.proof_revision,
                Arc::clone(&entry.weak_adapter),
                Arc::clone(&entry.proxy_mapping),
                entry.manager.clone(),
                entry.proxy_managers.clone(),
            )
        };
        let pristine = (|| -> PyResult<Option<bool>> {
            let referent = weak_adapter.bind(py).call0()?;
            let manager = adapter.getattr("poolmanager")?;
            let proxy_managers = adapter.getattr("proxy_manager")?;
            let Ok(proxy_managers) = proxy_managers.cast_exact::<PyDict>() else {
                return Ok(Some(false));
            };
            if !proxy_managers.as_any().is(proxy_mapping.bind(py)) {
                return Ok(Some(false));
            }
            let snapshot = proxy_managers.copy()?;
            if snapshot.len() != proxy_proofs.len() {
                return Ok(Some(false));
            }
            for (url, current) in snapshot.iter() {
                if !url.is_exact_instance_of::<PyString>() {
                    return Ok(Some(false));
                }
                let url_text = url.extract::<String>()?;
                let Some(expected) = proxy_proofs.get(&url_text) else {
                    return Ok(Some(false));
                };
                if !manager_proof_is_pristine(py, &current, expected)? {
                    return Ok(Some(false));
                }
            }
            if !manager_proof_is_pristine(py, &manager, &direct_manager)? {
                return Ok(Some(false));
            }
            let current_proxy_managers = adapter.getattr("proxy_manager")?;
            let Ok(current_proxy_managers) = current_proxy_managers.cast_exact::<PyDict>() else {
                return Ok(Some(false));
            };
            if !current_proxy_managers.as_any().is(proxy_mapping.bind(py)) {
                return Ok(Some(false));
            }
            let current_snapshot = current_proxy_managers.copy()?;
            if !exact_dict_snapshot_is_current(&snapshot, &current_snapshot)? {
                return Ok(None);
            }
            Ok(Some(
                referent.is(adapter)
                    && direct_manager.proof.manager.bind(py).is(&manager)
                    && manager_identity_is_pristine(py, &manager)?
                    && manager_configuration_is_pristine(adapter, &manager)?
                    && manager_pool_count(&manager)? == direct_manager.pools.visible_pool_count,
            ))
        })()?;
        let Some(pristine) = pristine else {
            continue;
        };
        let table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get(&identity) else {
            return Ok(false);
        };
        if entry.lifecycle_epoch != lifecycle_epoch {
            return Ok(false);
        }
        if entry.proof_revision != proof_revision {
            drop(table);
            continue;
        }
        if entry.proxy_mapping.as_ptr() != proxy_mapping.as_ptr() {
            return Ok(false);
        }
        drop(table);
        return Ok(pristine);
    }
    Ok(false)
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
    let Ok(proxy_managers) = proxy_managers.cast_exact::<PyDict>() else {
        return Ok(false);
    };
    if !proxy_managers.is_empty() {
        return Ok(false);
    }
    let proxy_mapping = Arc::new(proxy_managers.clone().into_any().unbind());
    let weak_adapter = PyModule::import(py, "weakref")?
        .getattr("ref")?
        .call1((adapter, callback))?
        .unbind();
    let state = adapter_state(py)?;
    let Some(manager_proof) = manager_proof(
        &manager,
        &state.http_pool_classes_by_scheme,
        &state.key_fn_by_scheme,
    )?
    else {
        return Ok(false);
    };
    let table = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let previous = table.insert(
        identity,
        SideEntry {
            lifecycle_epoch: next_adapter_lifecycle_epoch(),
            admission_revision: 0,
            proof_revision: 0,
            weak_adapter: Arc::new(weak_adapter),
            proxy_mapping,
            manager: manager_proof,
            direct_pools: PoolRealm::default(),
            proxy_pools: HashMap::new(),
            proxy_managers: HashMap::new(),
        },
    );
    drop(table);
    if let Some(previous) = previous {
        clear_realms(previous);
    }
    Ok(true)
}

fn record_visible_proxy_manager(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    proxy_url: &str,
    expected_lifecycle_epoch: u64,
    configuration: ProxyManagerConfiguration,
) -> PyResult<bool> {
    let identity = adapter_id(py, adapter)?;
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let (proof_revision, proxy_mapping, expected_managers) = {
            let table = registry
                .lock()
                .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
            let Some(entry) = table.get(&identity) else {
                return Ok(false);
            };
            if entry.lifecycle_epoch != expected_lifecycle_epoch {
                return Ok(false);
            }
            (
                entry.proof_revision,
                Arc::clone(&entry.proxy_mapping),
                entry.proxy_managers.clone(),
            )
        };
        let proxy_managers = adapter.getattr("proxy_manager")?;
        let Ok(proxy_managers) = proxy_managers.cast_exact::<PyDict>() else {
            return Ok(false);
        };
        if !proxy_managers.as_any().is(proxy_mapping.bind(py)) {
            return Ok(false);
        }
        let visible_snapshot = proxy_managers.copy()?;
        let mut target_manager_address = None;
        let mut visible_managers = HashMap::with_capacity(visible_snapshot.len());
        for (url, manager) in visible_snapshot.iter() {
            if !url.is_exact_instance_of::<PyString>() {
                return Ok(false);
            }
            let url_text = url.extract::<String>()?;
            if url_text == proxy_url {
                target_manager_address = Some(manager.as_ptr());
            }
            let record = if let Some(expected) = expected_managers.get(&url_text) {
                if expected.proof.manager.as_ptr() != manager.as_ptr()
                    || !manager_immutable_proof_is_pristine(py, &manager, expected.proof.as_ref())?
                {
                    return Ok(false);
                }
                let Some(pools) = stable_manager_pools_proof(&manager)? else {
                    return Ok(false);
                };
                ManagerRecord {
                    proof: Arc::clone(&expected.proof),
                    pools,
                }
            } else {
                if !canonical_proxy_manager_is_pristine(py, adapter, &url, &manager, configuration)?
                {
                    return Ok(false);
                }
                let state = adapter_state(py)?;
                let pool_classes_by_scheme = if url_text.to_ascii_lowercase().starts_with("socks") {
                    let Some(proof) = &state.socks_pool_classes_by_scheme else {
                        return Ok(false);
                    };
                    proof
                } else {
                    &state.http_pool_classes_by_scheme
                };
                let Some(record) =
                    manager_proof(&manager, pool_classes_by_scheme, &state.key_fn_by_scheme)?
                else {
                    return Ok(false);
                };
                record
            };
            visible_managers.insert(url_text, record);
        }
        if target_manager_address.is_none()
            || visible_managers.get(proxy_url).is_none_or(|manager| {
                Some(manager.proof.manager.as_ptr()) != target_manager_address
            })
        {
            return Ok(false);
        }
        if expected_managers.iter().any(|(url, expected)| {
            visible_managers.get(url).is_none_or(|manager| {
                manager.proof.manager.as_ptr() != expected.proof.manager.as_ptr()
            })
        }) {
            return Ok(false);
        }
        for (url, record) in &visible_managers {
            let Some(manager) = visible_snapshot.get_item(url)? else {
                return Ok(false);
            };
            if !manager_immutable_proof_is_pristine(py, &manager, record.proof.as_ref())? {
                return Ok(false);
            }
            if !manager_proof_is_pristine(py, &manager, record)? {
                return Ok(false);
            }
        }
        let current_proxy_managers = adapter.getattr("proxy_manager")?;
        let Ok(current_proxy_managers) = current_proxy_managers.cast_exact::<PyDict>() else {
            return Ok(false);
        };
        if !current_proxy_managers.as_any().is(proxy_mapping.bind(py)) {
            return Ok(false);
        }
        let current_snapshot = current_proxy_managers.copy()?;
        if !exact_dict_snapshot_is_current(&visible_snapshot, &current_snapshot)? {
            continue;
        }
        let mut table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get_mut(&identity) else {
            return Ok(false);
        };
        if entry.lifecycle_epoch != expected_lifecycle_epoch {
            return Ok(false);
        }
        if entry.proof_revision != proof_revision {
            drop(table);
            drop(visible_managers);
            continue;
        }
        if entry.proxy_mapping.as_ptr() != proxy_mapping.as_ptr() {
            return Ok(false);
        }
        if entry.proxy_managers.len() != expected_managers.len()
            || entry.proxy_managers.iter().any(|(url, current)| {
                expected_managers.get(url).is_none_or(|expected| {
                    current.proof.manager.as_ptr() != expected.proof.manager.as_ptr()
                })
            })
        {
            return Ok(false);
        }
        let previous = std::mem::replace(&mut entry.proxy_managers, visible_managers);
        entry.proof_revision = entry.proof_revision.wrapping_add(1);
        drop(table);
        drop(previous);
        return Ok(true);
    }
    Ok(false)
}

fn record_visible_direct_manager(
    identity: usize,
    expected_lifecycle_epoch: u64,
    manager: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let proof_revision = {
            let table = registry
                .lock()
                .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
            let Some(entry) = table.get(&identity) else {
                return Ok(false);
            };
            if entry.lifecycle_epoch != expected_lifecycle_epoch
                || entry.manager.proof.manager.as_ptr() != manager.as_ptr()
            {
                return Ok(false);
            }
            entry.proof_revision
        };
        let Some(pools) = stable_manager_pools_proof(manager)? else {
            return Ok(false);
        };
        let mut table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get_mut(&identity) else {
            return Ok(false);
        };
        if entry.lifecycle_epoch != expected_lifecycle_epoch
            || entry.manager.proof.manager.as_ptr() != manager.as_ptr()
        {
            return Ok(false);
        }
        if entry.proof_revision != proof_revision {
            drop(table);
            drop(pools);
            continue;
        }
        let previous = std::mem::replace(&mut entry.manager.pools, pools);
        entry.proof_revision = entry.proof_revision.wrapping_add(1);
        drop(table);
        drop(previous);
        return Ok(true);
    }
    Ok(false)
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
    drop(table);
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
    clear_realm(entry.direct_pools);
    for realm in entry.proxy_pools.into_values() {
        clear_realm(realm);
    }
}

fn clear_realm(realm: PoolRealm) {
    for pool in realm.pools.values() {
        pool.clear();
    }
}

fn reap_adapter_pools(py: Python<'_>) -> PyResult<()> {
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let candidates = {
        let table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        table
            .iter()
            .map(|(identity, entry)| {
                (
                    *identity,
                    entry.lifecycle_epoch,
                    Arc::clone(&entry.weak_adapter),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut dead = Vec::new();
    for (identity, lifecycle_epoch, weak_adapter) in candidates {
        if weak_adapter.bind(py).call0()?.is_none() {
            dead.push((identity, lifecycle_epoch));
        }
    }
    let mut removed = Vec::new();
    let mut table = registry
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    for (identity, lifecycle_epoch) in dead {
        if table
            .get(&identity)
            .is_some_and(|entry| entry.lifecycle_epoch == lifecycle_epoch)
            && let Some(entry) = table.remove(&identity)
        {
            removed.push(entry);
        }
    }
    drop(table);
    for entry in removed {
        clear_realms(entry);
    }
    Ok(())
}

fn adapter_pool(
    py: Python<'_>,
    adapter: &Bound<'_, PyAny>,
    input: &NativeSendInput,
) -> PyResult<Result<AdapterPoolSelection, String>> {
    reap_adapter_pools(py)?;
    let identity = adapter_id(py, adapter)?;
    let poolmanager = adapter.getattr("poolmanager")?;
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let (lifecycle_epoch, weak_adapter, manager_address, has_pool) = {
        let table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get(&identity) else {
            return Ok(Err(
                "adapter was not registered at initialization".to_owned()
            ));
        };
        let realm = match input.selected_proxy.as_deref() {
            Some(proxy) => entry.proxy_pools.get(proxy),
            None => Some(&entry.direct_pools),
        };
        (
            entry.lifecycle_epoch,
            Arc::clone(&entry.weak_adapter),
            entry.manager.proof.manager.as_ptr(),
            realm.is_some_and(|realm| realm.pools.contains_key(&input.pool_key)),
        )
    };
    let referent = weak_adapter.bind(py).call0()?;
    if !referent.is(adapter) || manager_address != poolmanager.as_ptr() {
        return Ok(Err("visible pool manager identity changed".to_owned()));
    }
    let candidate = if has_pool {
        None
    } else {
        Some(Arc::new(
            AdapterPool::new(
                input.pool_maxsize,
                input.pool_block,
                input.proxy.clone(),
                input.tls.clone(),
                input.timeout,
            )
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?,
        ))
    };
    let mut evicted = Vec::new();
    let mut table = registry
        .lock()
        .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
    let Some(entry) = table.get_mut(&identity) else {
        return Ok(Err(
            "adapter pool state disappeared during admission".to_owned()
        ));
    };
    if entry.lifecycle_epoch != lifecycle_epoch
        || entry.manager.proof.manager.as_ptr() != manager_address
    {
        return Ok(Err("adapter pool state changed during admission".to_owned()));
    }
    let realm = match input.selected_proxy.as_deref() {
        Some(proxy) => entry.proxy_pools.entry(proxy.to_owned()).or_default(),
        None => &mut entry.direct_pools,
    };
    let pool = if let Some(pool) = realm.pools.get(&input.pool_key) {
        Arc::clone(pool)
    } else {
        let pool = candidate.expect("missing adapter pool candidate");
        while realm.pools.len() >= input.pool_connections && input.pool_connections > 0 {
            let Some(evicted_key) = realm.order.pop_front() else {
                break;
            };
            if let Some(pool) = realm.pools.remove(&evicted_key) {
                evicted.push(pool);
            }
        }
        if input.pool_connections > 0 {
            realm
                .pools
                .insert(input.pool_key.clone(), Arc::clone(&pool));
        }
        pool
    };
    realm.order.retain(|key| key != &input.pool_key);
    if input.pool_connections > 0 {
        realm.order.push_back(input.pool_key.clone());
    }
    entry.admission_revision = entry.admission_revision.wrapping_add(1);
    drop(table);
    for pool in evicted {
        pool.clear();
    }
    Ok(Ok(AdapterPoolSelection {
        pool,
        identity,
        lifecycle_epoch,
    }))
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

fn canonical_adapter_global<'py>(
    py: Python<'py>,
    state: &AdapterState,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    state
        .exception_globals
        .iter()
        .find(|proof| proof.name == name)
        .map(|proof| proof.value.bind(py).clone())
        .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))
}

fn live_adapter_global<'py>(
    py: Python<'py>,
    state: &AdapterState,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(value) = state.send_globals.bind(py).get_item(name)? {
        return Ok(value);
    }
    state
        .send_builtins
        .bind(py)
        .get_item(name)?
        .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))
}

fn live_isinstance(
    py: Python<'_>,
    state: &AdapterState,
    value: &Bound<'_, PyAny>,
    class: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    live_adapter_global(py, state, "isinstance")?
        .call1((value, class))?
        .is_truthy()
}

fn live_reason_isinstance(
    py: Python<'_>,
    state: &AdapterState,
    original: &PyErr,
    class_name: &str,
) -> PyResult<bool> {
    let isinstance = live_adapter_global(py, state, "isinstance")?;
    let reason = original.value(py).getattr("reason")?;
    let class = live_adapter_global(py, state, class_name)?;
    isinstance.call1((reason, class))?.is_truthy()
}

fn canonical_adapter_surrogate(
    py: Python<'_>,
    state: &AdapterState,
    error: &requests::Error,
    pool: &Bound<'_, PyAny>,
    url: &str,
    urllib3_version: &str,
) -> PyResult<PyErr> {
    let kind = error.kind();
    let message = error.to_string();
    let urllib3_v2 = !urllib3_version.starts_with("1.26.");
    let direct = |name: &str, arguments: &Bound<'_, PyTuple>| -> PyResult<PyErr> {
        Ok(PyErr::from_value(
            canonical_adapter_global(py, state, name)?.call1(arguments.clone())?,
        ))
    };
    let with_source = |error: PyErr, source: &PyErr, explicit_cause: bool| {
        error.set_context(py, Some(source.clone_ref(py)));
        if explicit_cause {
            error.set_cause(py, Some(source.clone_ref(py)));
        }
        error
    };
    let connection = || -> PyResult<Bound<'_, PyAny>> {
        let class = pool.getattr("ConnectionCls")?;
        let host = pool.getattr("host")?;
        let port = pool.getattr("port")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("port", port)?;
        class.call((host,), Some(&kwargs))
    };
    let os_error = || -> PyResult<PyErr> {
        let errno = error.raw_os_error().unwrap_or(1);
        let message = PyModule::import(py, "os")?
            .getattr("strerror")?
            .call1((errno,))?;
        Ok(PyErr::from_value(
            PyModule::import(py, "builtins")?
                .getattr("OSError")?
                .call1((errno, message))?,
        ))
    };
    let max_retry = |reason: PyErr| -> PyResult<PyErr> {
        let exhausted = PyErr::from_value(
            canonical_adapter_global(py, state, "MaxRetryError")?.call1((
                pool,
                url,
                reason.value(py),
            ))?,
        );
        Ok(with_source(exhausted, &reason, urllib3_v2))
    };
    match kind {
        ErrorKind::InvalidUrl | ErrorKind::MissingSchema => {
            direct("LocationValueError", &PyTuple::new(py, [message])?)
        }
        ErrorKind::ReadTimeout => {
            let class = canonical_adapter_global(py, state, "ReadTimeoutError")?;
            Ok(PyErr::from_value(class.call1((
                pool,
                py.None(),
                "Read timed out.",
            ))?))
        }
        ErrorKind::Connect | ErrorKind::Dns => {
            let connection = connection()?;
            let source = if kind == ErrorKind::Dns {
                let errno = error.raw_os_error().unwrap_or(-2);
                let message = PyModule::import(py, "os")?
                    .getattr("strerror")?
                    .call1((errno,))?;
                PyErr::from_value(
                    PyModule::import(py, "socket")?
                        .getattr("gaierror")?
                        .call1((errno, message))?,
                )
            } else {
                os_error()?
            };
            let reason = if kind == ErrorKind::Dns && urllib3_v2 {
                let host = pool.getattr("host")?;
                let reason = PyErr::from_value(
                    PyModule::import(py, "urllib3.exceptions")?
                        .getattr("NameResolutionError")?
                        .call1((host, connection, source.value(py)))?,
                );
                with_source(reason, &source, true)
            } else {
                let detail = format!("Failed to establish a new connection: {}", source.value(py));
                let reason = PyErr::from_value(
                    canonical_adapter_global(py, state, "NewConnectionError")?
                        .call1((connection, detail))?,
                );
                with_source(reason, &source, urllib3_v2)
            };
            max_retry(reason)
        }
        ErrorKind::ConnectTimeout | ErrorKind::Proxy | ErrorKind::Tls | ErrorKind::Handshake => {
            let (reason_name, reason) = match kind {
                ErrorKind::ConnectTimeout => ("ConnectTimeoutError", message.clone()),
                ErrorKind::Proxy => ("_ProxyError", message.clone()),
                ErrorKind::Tls | ErrorKind::Handshake => ("_SSLError", message.clone()),
                _ => unreachable!(),
            };
            let reason = if reason_name == "_ProxyError" {
                let nested =
                    canonical_adapter_global(py, state, "ProtocolError")?.call1((message,))?;
                canonical_adapter_global(py, state, reason_name)?.call1((reason, nested))?
            } else {
                canonical_adapter_global(py, state, reason_name)?.call1((reason,))?
            };
            max_retry(PyErr::from_value(reason))
        }
        ErrorKind::Send | ErrorKind::Connection => {
            let source = os_error()?;
            let protocol = PyErr::from_value(
                canonical_adapter_global(py, state, "ProtocolError")?
                    .call1(("Connection aborted.", source.value(py)))?,
            );
            let protocol = with_source(protocol, &source, urllib3_v2);
            let exhausted = PyErr::from_value(
                canonical_adapter_global(py, state, "MaxRetryError")?.call1((
                    pool,
                    url,
                    protocol.value(py),
                ))?,
            );
            exhausted.set_context(py, Some(source));
            if urllib3_v2 {
                exhausted.set_cause(py, Some(protocol));
            }
            Ok(exhausted)
        }
        _ => direct("ProtocolError", &PyTuple::new(py, [message])?),
    }
}

fn canonical_retry_surrogate(
    py: Python<'_>,
    state: &AdapterState,
    message: &str,
    pool: &Bound<'_, PyAny>,
    url: &str,
) -> PyResult<PyErr> {
    let reason = canonical_adapter_global(py, state, "ResponseError")?.call1((message,))?;
    Ok(PyErr::from_value(
        canonical_adapter_global(py, state, "MaxRetryError")?.call1((pool, url, reason))?,
    ))
}

fn raised_adapter_target(
    py: Python<'_>,
    state: &AdapterState,
    target_name: &str,
    original: &PyErr,
    request: Option<&Bound<'_, PyAny>>,
) -> PyResult<PyErr> {
    let target = live_adapter_global(py, state, target_name)?;
    let value = match request {
        Some(request) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("request", request)?;
            target.call((original.value(py),), Some(&kwargs))?
        }
        None => target.call1((original.value(py),))?,
    };
    let mapped = PyErr::from_value(value);
    mapped.set_context(py, Some(original.clone_ref(py)));
    Ok(mapped)
}

fn simulate_adapter_handlers(
    py: Python<'_>,
    state: &AdapterState,
    original: &PyErr,
    request: &Bound<'_, PyAny>,
) -> PyResult<PyErr> {
    let protocol = live_adapter_global(py, state, "ProtocolError")?;
    let os_error = live_adapter_global(py, state, "OSError")?;
    let first = PyTuple::new(py, [protocol, os_error])?;
    if exception_matches(py, original, first.as_any())? {
        return raised_adapter_target(py, state, "ConnectionError", original, Some(request));
    }

    let max_retry = live_adapter_global(py, state, "MaxRetryError")?;
    if exception_matches(py, original, &max_retry)? {
        if live_reason_isinstance(py, state, original, "ConnectTimeoutError")?
            && !live_reason_isinstance(py, state, original, "NewConnectionError")?
        {
            return raised_adapter_target(py, state, "ConnectTimeout", original, Some(request));
        }
        if live_reason_isinstance(py, state, original, "ResponseError")? {
            return raised_adapter_target(py, state, "RetryError", original, Some(request));
        }
        if live_reason_isinstance(py, state, original, "_ProxyError")? {
            return raised_adapter_target(py, state, "ProxyError", original, Some(request));
        }
        if live_reason_isinstance(py, state, original, "_SSLError")? {
            return raised_adapter_target(py, state, "SSLError", original, Some(request));
        }
        return raised_adapter_target(py, state, "ConnectionError", original, Some(request));
    }

    let closed_pool = live_adapter_global(py, state, "ClosedPoolError")?;
    if exception_matches(py, original, &closed_pool)? {
        return raised_adapter_target(py, state, "ConnectionError", original, Some(request));
    }

    let proxy = live_adapter_global(py, state, "_ProxyError")?;
    if exception_matches(py, original, &proxy)? {
        return raised_adapter_target(py, state, "ProxyError", original, None);
    }

    let ssl = live_adapter_global(py, state, "_SSLError")?;
    let http = live_adapter_global(py, state, "_HTTPError")?;
    let last = PyTuple::new(py, [ssl.clone(), http])?;
    if exception_matches(py, original, last.as_any())? {
        if live_isinstance(py, state, original.value(py), &ssl)? {
            return raised_adapter_target(py, state, "SSLError", original, Some(request));
        }
        let read_timeout = live_adapter_global(py, state, "ReadTimeoutError")?;
        if live_isinstance(py, state, original.value(py), &read_timeout)? {
            return raised_adapter_target(py, state, "ReadTimeout", original, Some(request));
        }
        let invalid_header = live_adapter_global(py, state, "_InvalidHeader")?;
        if live_isinstance(py, state, original.value(py), &invalid_header)? {
            return raised_adapter_target(py, state, "InvalidHeader", original, Some(request));
        }
    }
    Ok(original.clone_ref(py))
}

fn map_adapter_surrogate(
    py: Python<'_>,
    state: &AdapterState,
    original: PyErr,
    request: &Bound<'_, PyAny>,
) -> PyErr {
    match simulate_adapter_handlers(py, state, &original, request) {
        Ok(mapped) => mapped,
        Err(error) => {
            error.set_context(py, Some(original));
            error
        }
    }
}

fn mapped_transport_error(
    py: Python<'_>,
    error: requests::Error,
    request: &Bound<'_, PyAny>,
    pool: &Bound<'_, PyAny>,
    url: &str,
    urllib3_version: &str,
) -> PyErr {
    let state = match adapter_state(py) {
        Ok(state) => state,
        Err(error) => return error,
    };
    let original = match canonical_adapter_surrogate(py, state, &error, pool, url, urllib3_version)
    {
        Ok(original) => original,
        Err(error) => return error,
    };
    map_adapter_surrogate(py, state, original, request)
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
    pool: &Bound<'_, PyAny>,
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
            pool: pool.clone().unbind(),
            content_encoding,
            decoder: None,
            decoded: Vec::new(),
            decoded_offset: 0,
            decoder_eof: false,
            decode_started: false,
            decode_failed: false,
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
    if verify.is_exact_instance_of::<PyBool>() && !verify.extract::<bool>()? {
        return Ok(py.NotImplemented());
    }
    let input = match native_send_input(py, adapter, request, timeout, verify, cert, proxies)? {
        Ok(input) => input,
        Err(_) => return Ok(py.NotImplemented()),
    };
    // The native side table and pool must be usable before the first
    // manager/cache callback. From manager entry onward the send is committed
    // and must never replay through retained Python.
    let selection = match adapter_pool(py, adapter, &input)? {
        Ok(selection) => selection,
        Err(_) => return Ok(py.NotImplemented()),
    };
    let pool = Arc::clone(&selection.pool);
    let python_pool = if let Some(proxy) = &input.selected_proxy {
        let manager = adapter.call_method1("proxy_manager_for", (proxy,))?;
        let python_pool = manager.call_method1("connection_from_url", (&input.url,))?;
        if !record_visible_proxy_manager(
            py,
            adapter,
            proxy,
            selection.lifecycle_epoch,
            ProxyManagerConfiguration::from(&input),
        )? {
            return Err(PyRuntimeError::new_err(
                "proxy manager state changed after native send commitment",
            ));
        }
        python_pool
    } else {
        let manager = adapter.getattr("poolmanager")?;
        let python_pool = manager.call_method1("connection_from_url", (&input.url,))?;
        if !record_visible_direct_manager(selection.identity, selection.lifecycle_epoch, &manager)?
        {
            return Err(PyRuntimeError::new_err(
                "adapter pool state changed after native send commitment",
            ));
        }
        python_pool
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
                    return Err(mapped_transport_error(
                        py,
                        error,
                        request,
                        &python_pool,
                        &input.urllib3_url,
                        &input.retry.version,
                    ));
                }
                match retry_state.increment(reason, &input.method_name, &input.url, None) {
                    Ok(next) => {
                        retry_state = next;
                        sleep_before_retry(py, &retry_state, 0.0)?;
                        continue;
                    }
                    Err(_) => {
                        return Err(mapped_transport_error(
                            py,
                            error,
                            request,
                            &python_pool,
                            &input.urllib3_url,
                            &input.retry.version,
                        ));
                    }
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
            return build_python_response(py, adapter, request, &python_pool, response);
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
                let state = adapter_state(py)?;
                let original = canonical_retry_surrogate(
                    py,
                    state,
                    &message,
                    &python_pool,
                    &input.urllib3_url,
                )?;
                return Err(map_adapter_surrogate(py, state, original, request));
            }
            Err(_) => {
                return build_python_response(py, adapter, request, &python_pool, response);
            }
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
    reap_adapter_pools(py)?;
    let registry = ADAPTER_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    for _ in 0..MANAGER_PROOF_ATTEMPTS {
        let (lifecycle_epoch, admission_revision, proof_revision, manager, proxy_managers) = {
            let table = registry
                .lock()
                .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
            let Some(entry) = table.get(&identity) else {
                return Ok(0);
            };
            (
                entry.lifecycle_epoch,
                entry.admission_revision,
                entry.proof_revision,
                entry.manager.clone(),
                entry.proxy_managers.clone(),
            )
        };
        let Some(manager_pools) = stable_manager_pools_proof(manager.proof.manager.bind(py))?
        else {
            return Err(PyRuntimeError::new_err(
                "adapter pool state changed while closing",
            ));
        };
        let mut proxy_pools = Vec::with_capacity(proxy_managers.len());
        for (url, manager) in &proxy_managers {
            let Some(pools) = stable_manager_pools_proof(manager.proof.manager.bind(py))? else {
                return Err(PyRuntimeError::new_err(
                    "adapter pool state changed while closing",
                ));
            };
            proxy_pools.push((url.clone(), manager.proof.manager.as_ptr(), pools));
        }
        let mut table = registry
            .lock()
            .map_err(|_| PyRuntimeError::new_err("adapter pool table lock poisoned"))?;
        let Some(entry) = table.get_mut(&identity) else {
            return Ok(0);
        };
        if entry.lifecycle_epoch != lifecycle_epoch {
            return Err(PyRuntimeError::new_err(
                "adapter pool state changed while closing",
            ));
        }
        if entry.admission_revision != admission_revision {
            return Err(PyRuntimeError::new_err(
                "adapter pool state changed while closing",
            ));
        }
        if entry.proof_revision != proof_revision {
            drop(table);
            drop(manager_pools);
            drop(proxy_pools);
            continue;
        }
        if entry.manager.proof.manager.as_ptr() != manager.proof.manager.as_ptr()
            || entry.proxy_managers.len() != proxy_pools.len()
            || proxy_pools.iter().any(|(url, address, _)| {
                entry
                    .proxy_managers
                    .get(url)
                    .is_none_or(|manager| manager.proof.manager.as_ptr() != *address)
            })
        {
            return Err(PyRuntimeError::new_err(
                "adapter pool state changed while closing",
            ));
        }
        let count = realm_pool_count(entry);
        let direct_pools = std::mem::take(&mut entry.direct_pools);
        let proxy_realms = std::mem::take(&mut entry.proxy_pools);
        let mut previous_proofs = vec![std::mem::replace(&mut entry.manager.pools, manager_pools)];
        for (url, _, pools) in proxy_pools {
            let manager = entry
                .proxy_managers
                .get_mut(&url)
                .expect("validated proxy manager disappeared");
            previous_proofs.push(std::mem::replace(&mut manager.pools, pools));
        }
        entry.lifecycle_epoch = next_adapter_lifecycle_epoch();
        entry.proof_revision = entry.proof_revision.wrapping_add(1);
        drop(table);
        drop(previous_proofs);
        clear_realm(direct_pools);
        for realm in proxy_realms.into_values() {
            clear_realm(realm);
        }
        return Ok(count);
    }
    Err(PyRuntimeError::new_err(
        "adapter pool state changed while closing",
    ))
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
    fn finish_body_failure(&mut self) {
        self.body = None;
        self.closed = true;
        self.decoder_eof = true;
        self.decoded.clear();
        self.decoded_offset = 0;
    }

    fn map_io_failure(&mut self, py: Python<'_>, error: std::io::Error) -> PyErr {
        let mapped = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<requests::Error>())
            .map_or_else(
                || PyRuntimeError::new_err(error.to_string()),
                |source| map_typed_response_error(py, source, Some(self.pool.bind(py))),
            );
        self.finish_body_failure();
        mapped
    }

    fn suppresses_urllib3_126_incomplete(&self, py: Python<'_>, error: &std::io::Error) -> bool {
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<requests::Error>())
            .is_some_and(|source| source.incomplete_body().is_some())
            && PyModule::import(py, "urllib3")
                .and_then(|module| module.getattr("__version__"))
                .and_then(|version| version.extract::<String>())
                .is_ok_and(|version| version.starts_with("1.26."))
    }

    fn map_decode_failure(&mut self, py: Python<'_>, error: PyErr) -> PyErr {
        let encoding = self
            .content_encoding
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        self.decode_failed = true;
        map_decoder_error(py, &encoding, error)
    }

    fn decode_result<T>(&mut self, py: Python<'_>, result: PyResult<T>) -> PyResult<T> {
        result.map_err(|error| self.map_decode_failure(py, error))
    }

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

    fn fill_decoded_bounded(&mut self, py: Python<'_>, wanted: Option<usize>) -> PyResult<()> {
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
                let result = match self.body.as_mut() {
                    Some(body) => py.detach(|| body.read(&mut wire)),
                    None => Ok(0),
                };
                match result {
                    Ok(read) => read,
                    Err(error) => return Err(self.map_io_failure(py, error)),
                }
            };
            wire.truncate(read);
            if read == 0 && !has_tail {
                self.body = None;
                if self.decode_failed {
                    self.decoder_eof = true;
                    break;
                }
                if let Some(decoder) = decoder {
                    let tail =
                        self.decode_result(py, Self::decompress(decoder.bind(py), py, b"", -1))?;
                    self.decoded.extend(tail);
                    let flushed = self.decode_result(
                        py,
                        decoder
                            .bind(py)
                            .call_method0("flush")
                            .and_then(|value| value.extract::<Vec<u8>>()),
                    )?;
                    self.decoded.extend(flushed);
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
                let decoded =
                    self.decode_result(py, Self::decompress(decoder.bind(py), py, &wire, maximum))?;
                self.decoded.extend(decoded);
            } else {
                self.decoded.extend(wire);
            }
        }
        Ok(())
    }

    fn read_decoded_unbounded(
        &mut self,
        py: Python<'_>,
        amount: Option<usize>,
        decoder: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<u8>> {
        let mut wire = Vec::new();
        let result = match (self.body.as_mut(), amount) {
            (Some(body), Some(amount)) => {
                wire.resize(amount, 0);
                py.detach(|| body.read(&mut wire))
            }
            (Some(body), None) => py.detach(|| body.read_to_end(&mut wire)),
            (None, _) => Ok(0),
        };
        let read = match result {
            Ok(read) => read,
            Err(error) => return Err(self.map_io_failure(py, error)),
        };
        wire.truncate(read);
        if read == 0 {
            self.body = None;
            self.decoder_eof = true;
            self.closed = true;
            return Ok(Vec::new());
        }
        let mut decoded = self.decode_result(py, Self::decompress(decoder, py, &wire, -1))?;
        if amount.is_none() {
            decoded.extend(self.decode_result(py, Self::decompress(decoder, py, b"", -1))?);
            decoded.extend(
                self.decode_result(
                    py,
                    decoder
                        .call_method0("flush")
                        .and_then(|value| value.extract::<Vec<u8>>()),
                )?,
            );
            self.body = None;
            self.decoder_eof = true;
            self.closed = true;
        }
        Ok(decoded)
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
        if decode_content && let Some(decoder) = self.decoder(py)? {
            if !Self::decoder_is_bounded(decoder.bind(py))? {
                let bytes = self.read_decoded_unbounded(py, amount, decoder.bind(py))?;
                return Ok(PyBytes::new(py, &bytes).into_any().unbind());
            }
            self.fill_decoded_bounded(py, amount)?;
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
        if let Err(error) = result {
            if self.suppresses_urllib3_126_incomplete(py, &error) {
                self.finish_body_failure();
                return Ok(PyBytes::new(py, b"").into_any().unbind());
            }
            return Err(self.map_io_failure(py, error));
        }
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
            self.closed = true;
            self.decoder_eof = true;
            if let Err(error) = py.detach(|| body.close()) {
                return Err(map_typed_response_error(
                    py,
                    &error,
                    Some(self.pool.bind(py)),
                ));
            }
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
        loop {
            let mut raw = self.raw.bind(py).borrow_mut();
            let chunk = raw.read_amount(py, self.amount, self.decode_content)?;
            if chunk.bind(py).len()? != 0 {
                return Ok(Some(chunk));
            }
            if raw.closed {
                self.done = true;
                return Ok(None);
            }
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

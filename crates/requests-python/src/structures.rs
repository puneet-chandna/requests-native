use pyo3::exceptions::{PyTypeError, PyUnicodeEncodeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBytes, PyDict, PyDictMethods, PyInt, PyModule, PyString, PyTuple,
    PyTupleMethods, PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::structures::CaseInsensitiveMap;

struct CallableProof {
    function: Py<PyAny>,
    code: Py<PyAny>,
    defaults: Py<PyAny>,
    kwdefaults: Py<PyAny>,
    closure: Py<PyAny>,
    globals: Py<PyAny>,
    builtins: Py<PyAny>,
}

struct InternalUtilsState {
    module: Py<PyModule>,
    module_dictionary: Py<PyDict>,
    to_native_string: CallableProof,
    unicode_is_ascii: CallableProof,
    builtin_str: Py<PyAny>,
    builtin_isinstance: Py<PyAny>,
    builtin_unicode_encode_error: Py<PyAny>,
}

struct ClassEntryProof {
    key: Py<PyAny>,
    value: Py<PyAny>,
    callable: Option<CallableProof>,
}

struct StatusCodesState {
    structures: Py<PyModule>,
    lookup_dict: Py<PyType>,
    lookup_dictionary: Vec<ClassEntryProof>,
    bases: Py<PyAny>,
    base_items: Vec<Py<PyAny>>,
    codes: Py<PyAny>,
}

static INTERNAL_UTILS_STATE: PyOnceLock<InternalUtilsState> = PyOnceLock::new();
static STATUS_CODES_STATE: PyOnceLock<StatusCodesState> = PyOnceLock::new();

fn callable_proof(value: &Bound<'_, PyAny>) -> PyResult<CallableProof> {
    Ok(CallableProof {
        function: value.clone().unbind(),
        code: value.getattr("__code__")?.unbind(),
        defaults: value.getattr("__defaults__")?.unbind(),
        kwdefaults: value.getattr("__kwdefaults__")?.unbind(),
        closure: value.getattr("__closure__")?.unbind(),
        globals: value.getattr("__globals__")?.unbind(),
        builtins: value.getattr("__builtins__")?.unbind(),
    })
}

fn callable_is_pristine(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    proof: &CallableProof,
) -> PyResult<bool> {
    Ok(current.is(proof.function.bind(py))
        && current.getattr("__code__")?.is(proof.code.bind(py))
        && current.getattr("__defaults__")?.is(proof.defaults.bind(py))
        && current
            .getattr("__kwdefaults__")?
            .is(proof.kwdefaults.bind(py))
        && current.getattr("__closure__")?.is(proof.closure.bind(py))
        && current.getattr("__globals__")?.is(proof.globals.bind(py))
        && current.getattr("__builtins__")?.is(proof.builtins.bind(py)))
}

fn initialize_internal_utils_state(py: Python<'_>) -> PyResult<InternalUtilsState> {
    let module = PyModule::import(py, "requests._internal_utils")?;
    let module_dictionary = module.dict();
    let builtins = PyModule::import(py, "builtins")?;
    Ok(InternalUtilsState {
        to_native_string: callable_proof(&module.getattr("to_native_string")?)?,
        unicode_is_ascii: callable_proof(&module.getattr("unicode_is_ascii")?)?,
        builtin_str: py.get_type::<PyString>().into_any().unbind(),
        builtin_isinstance: builtins.getattr("isinstance")?.unbind(),
        builtin_unicode_encode_error: builtins.getattr("UnicodeEncodeError")?.unbind(),
        module: module.unbind(),
        module_dictionary: module_dictionary.unbind(),
    })
}

fn internal_utils_state(py: Python<'_>) -> PyResult<&InternalUtilsState> {
    INTERNAL_UTILS_STATE.get_or_try_init(py, || initialize_internal_utils_state(py))
}

fn internal_callable_is_pristine(
    py: Python<'_>,
    state: &InternalUtilsState,
    name: &str,
    proof: &CallableProof,
) -> PyResult<bool> {
    let module = state.module.bind(py);
    let dictionary = module.dict();
    let builtins = proof.builtins.bind(py).cast::<PyDict>()?;
    Ok(dictionary.is(state.module_dictionary.bind(py))
        && callable_is_pristine(py, &module.getattr(name)?, proof)?
        && !dictionary.contains("isinstance")?
        && builtins
            .get_item("isinstance")?
            .is_some_and(|value| value.is(state.builtin_isinstance.bind(py))))
}

fn internal_utils_fallback(
    py: Python<'_>,
    state: &InternalUtilsState,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    Ok(state
        .module
        .bind(py)
        .getattr(operation)?
        .call1(arguments.clone())?
        .unbind())
}

#[pyfunction]
fn _internal_utils_trial(
    py: Python<'_>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    let state = internal_utils_state(py)?;
    match operation {
        "to_native_string"
            if (arguments.len() == 1 || arguments.len() == 2)
                && internal_callable_is_pristine(
                    py,
                    state,
                    operation,
                    &state.to_native_string,
                )?
                && state
                    .module
                    .bind(py)
                    .getattr("builtin_str")?
                    .is(state.builtin_str.bind(py)) =>
        {
            let value = arguments.get_item(0)?;
            if value.is_exact_instance_of::<PyString>() {
                return Ok(value.unbind());
            }
            if value.is_exact_instance_of::<PyBytes>() {
                let encoding = if arguments.len() == 2 {
                    arguments.get_item(1)?
                } else {
                    PyString::new(py, "ascii").into_any()
                };
                if encoding.is_exact_instance_of::<PyString>() {
                    return Ok(value.call_method1("decode", (encoding,))?.unbind());
                }
            }
            internal_utils_fallback(py, state, operation, arguments)
        }
        "unicode_is_ascii"
            if arguments.len() == 1
                && internal_callable_is_pristine(
                    py,
                    state,
                    operation,
                    &state.unicode_is_ascii,
                )?
                && !state
                    .module_dictionary
                    .bind(py)
                    .contains("UnicodeEncodeError")?
                && state
                    .unicode_is_ascii
                    .builtins
                    .bind(py)
                    .cast::<PyDict>()?
                    .get_item("UnicodeEncodeError")?
                    .is_some_and(|value| value.is(state.builtin_unicode_encode_error.bind(py))) =>
        {
            let value = arguments.get_item(0)?;
            if value.is_exact_instance_of::<PyString>() {
                return match value.cast::<PyString>()?.to_str() {
                    Ok(value) => Ok(requests::utils::unicode_is_ascii(value)
                        .into_pyobject(py)?
                        .to_owned()
                        .into_any()
                        .unbind()),
                    Err(error) if error.is_instance_of::<PyUnicodeEncodeError>(py) => {
                        Ok(false.into_pyobject(py)?.to_owned().into_any().unbind())
                    }
                    Err(error) => Err(error),
                };
            }
            internal_utils_fallback(py, state, operation, arguments)
        }
        "to_native_string" | "unicode_is_ascii" => {
            internal_utils_fallback(py, state, operation, arguments)
        }
        _ => Err(PyValueError::new_err(format!(
            "unknown internal utils trial operation: {operation}"
        ))),
    }
}

fn initialize_status_codes_state(py: Python<'_>) -> PyResult<StatusCodesState> {
    let status_codes = PyModule::import(py, "requests.status_codes")?;
    let structures = PyModule::import(py, "requests.structures")?;
    let lookup_dict = structures.getattr("LookupDict")?.cast_into::<PyType>()?;
    let dictionary = lookup_dict.getattr("__dict__")?;
    let mut lookup_dictionary = Vec::new();
    for key in dictionary.try_iter()? {
        let key = key?;
        let value = dictionary.get_item(&key)?;
        let callable = value
            .hasattr("__code__")?
            .then(|| callable_proof(&value))
            .transpose()?;
        lookup_dictionary.push(ClassEntryProof {
            key: key.unbind(),
            value: value.unbind(),
            callable,
        });
    }
    let bases = lookup_dict.getattr("__bases__")?;
    let base_items = bases
        .try_iter()?
        .map(|item| item.map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(StatusCodesState {
        structures: structures.unbind(),
        lookup_dict: lookup_dict.unbind(),
        lookup_dictionary,
        bases: bases.unbind(),
        base_items,
        codes: status_codes.getattr("codes")?.unbind(),
    })
}

fn status_codes_state(py: Python<'_>) -> PyResult<&StatusCodesState> {
    STATUS_CODES_STATE.get_or_try_init(py, || initialize_status_codes_state(py))
}

fn lookup_class_is_pristine(py: Python<'_>, state: &StatusCodesState) -> PyResult<bool> {
    let current = state.structures.bind(py).getattr("LookupDict")?;
    if !current.is(state.lookup_dict.bind(py)) {
        return Ok(false);
    }
    let dictionary = current.getattr("__dict__")?;
    let keys = dictionary.try_iter()?.collect::<PyResult<Vec<_>>>()?;
    if keys.len() != state.lookup_dictionary.len() {
        return Ok(false);
    }
    for (key, proof) in keys.iter().zip(&state.lookup_dictionary) {
        if !key.is(proof.key.bind(py)) {
            return Ok(false);
        }
        let value = dictionary.get_item(key)?;
        if !value.is(proof.value.bind(py)) {
            return Ok(false);
        }
        if let Some(callable) = &proof.callable
            && !callable_is_pristine(py, &value, callable)?
        {
            return Ok(false);
        }
    }
    let bases = current.getattr("__bases__")?;
    let base_items = bases.try_iter()?.collect::<PyResult<Vec<_>>>()?;
    Ok(bases.is(state.bases.bind(py))
        && base_items.len() == state.base_items.len()
        && base_items
            .iter()
            .zip(&state.base_items)
            .all(|(current, expected)| current.is(expected.bind(py))))
}

fn status_codes_fallback(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    lookup_fallback(py, subject, operation, arguments)
}

#[pyfunction]
fn _status_codes_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    let state = status_codes_state(py)?;
    if !subject.is(state.codes.bind(py))
        || !subject.get_type().is(state.lookup_dict.bind(py))
        || !lookup_class_is_pristine(py, state)?
    {
        return status_codes_fallback(py, subject, operation, arguments);
    }
    match operation {
        "getitem" if arguments.len() == 1 => {
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() {
                return status_codes_fallback(py, subject, operation, arguments);
            }
            Ok(subject
                .getattr("__dict__")?
                .cast::<PyDict>()?
                .get_item(key)?
                .map_or_else(|| py.None(), Bound::unbind))
        }
        "get" if arguments.len() == 1 || arguments.len() == 2 => {
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() {
                return status_codes_fallback(py, subject, operation, arguments);
            }
            let default = if arguments.len() == 2 {
                arguments.get_item(1)?.unbind()
            } else {
                py.None()
            };
            Ok(subject
                .getattr("__dict__")?
                .cast::<PyDict>()?
                .get_item(key)?
                .map_or(default, Bound::unbind))
        }
        "getattr" if arguments.len() == 1 => {
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() {
                return status_codes_fallback(py, subject, operation, arguments);
            }
            if let Some(value) = subject
                .getattr("__dict__")?
                .cast::<PyDict>()?
                .get_item(&key)?
            {
                Ok(value.unbind())
            } else {
                status_codes_fallback(py, subject, operation, arguments)
            }
        }
        "repr" => status_codes_fallback(py, subject, operation, arguments),
        "getitem" | "get" | "getattr" => status_codes_fallback(py, subject, operation, arguments),
        _ => Err(PyValueError::new_err(format!(
            "unknown status codes trial operation: {operation}"
        ))),
    }
}

#[pyfunction]
fn _case_insensitive_dict_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    if operation == "core_snapshot" {
        require_arguments(arguments, operation, 0)?;
        return case_insensitive_core_snapshot(py, subject);
    }

    case_insensitive_fallback(py, subject, operation, arguments)
}

fn case_insensitive_fallback(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    match operation {
        "set" => {
            require_arguments(arguments, operation, 2)?;
            subject.set_item(arguments.get_item(0)?, arguments.get_item(1)?)?;
            Ok(py.None())
        }
        "get" => {
            require_arguments(arguments, operation, 1)?;
            Ok(subject.get_item(arguments.get_item(0)?)?.unbind())
        }
        "delete" => {
            require_arguments(arguments, operation, 1)?;
            subject.del_item(arguments.get_item(0)?)?;
            Ok(py.None())
        }
        "iter" => {
            require_arguments(arguments, operation, 0)?;
            Ok(subject.try_iter()?.into_any().unbind())
        }
        "len" => {
            require_arguments(arguments, operation, 0)?;
            Ok(PyInt::new(py, subject.len()?).into_any().unbind())
        }
        "lower_items" => {
            require_arguments(arguments, operation, 0)?;
            Ok(subject.call_method0("lower_items")?.unbind())
        }
        "eq" => {
            require_arguments(arguments, operation, 1)?;
            Ok(subject
                .call_method1("__eq__", (arguments.get_item(0)?,))?
                .unbind())
        }
        "copy" => {
            require_arguments(arguments, operation, 0)?;
            Ok(subject.call_method0("copy")?.unbind())
        }
        "repr" => {
            require_arguments(arguments, operation, 0)?;
            Ok(subject.repr()?.into_any().unbind())
        }
        _ => Err(PyValueError::new_err(format!(
            "unknown CaseInsensitiveDict trial operation: {operation}"
        ))),
    }
}

fn case_insensitive_core_snapshot(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let store = subject.getattr("_store")?;
    if !is_trusted_ordered_dict(py, &store)? {
        return Err(PyTypeError::new_err(
            "core_snapshot requires an exact collections.OrderedDict _store",
        ));
    }

    let mut values = CaseInsensitiveMap::new();
    for normalized in store.try_iter()? {
        let normalized = normalized?;
        if !normalized.is_exact_instance_of::<PyString>() {
            return Err(PyTypeError::new_err(
                "core_snapshot requires exact str normalized keys",
            ));
        }
        let stored = store.get_item(&normalized)?;
        if !stored.is_exact_instance_of::<PyTuple>() {
            return Err(PyTypeError::new_err(
                "core_snapshot requires exact tuple stored entries",
            ));
        }
        let stored = stored.cast::<PyTuple>()?;
        if stored.len() != 2 {
            return Err(PyValueError::new_err(
                "core_snapshot requires two-item stored entries",
            ));
        }
        let cased = stored.get_item(0)?;
        if !cased.is_exact_instance_of::<PyString>() {
            return Err(PyTypeError::new_err(
                "core_snapshot requires exact str cased keys",
            ));
        }
        values.insert(normalized.extract()?, cased.extract()?, ());
    }

    let snapshot = PyDict::new(py);
    snapshot.set_item(
        "cased_keys",
        values
            .iter()
            .map(|(key, ())| key.to_owned())
            .collect::<Vec<_>>(),
    )?;
    snapshot.set_item(
        "normalized_keys",
        values
            .lower_items()
            .map(|(key, ())| key.to_owned())
            .collect::<Vec<_>>(),
    )?;
    snapshot.set_item("length", values.len())?;
    Ok(snapshot.into_any().unbind())
}

fn is_trusted_ordered_dict(py: Python<'_>, store: &Bound<'_, PyAny>) -> PyResult<bool> {
    let store_type = store.get_type();
    if !store_type.get_type().is(py.get_type::<PyType>()) {
        return Ok(false);
    }

    let flags = store_type.getattr("__flags__")?.extract::<u64>()?;
    const IMMUTABLE_TYPE: u64 = 1 << 8;
    const HEAP_TYPE: u64 = 1 << 9;
    if flags & HEAP_TYPE != 0
        || flags & IMMUTABLE_TYPE == 0
        || store_type.module()?.to_str()? != "collections"
        || store_type.qualname()?.to_str()? != "OrderedDict"
    {
        return Ok(false);
    }

    let bases = store_type.bases();
    Ok(bases.len() == 1 && bases.get_item(0)?.is(py.get_type::<PyDict>()))
}

#[pyfunction]
fn _lookup_dict_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    lookup_fallback(py, subject, operation, arguments)
}

fn lookup_fallback(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    match operation {
        "getitem" => {
            require_arguments(arguments, operation, 1)?;
            Ok(subject.get_item(arguments.get_item(0)?)?.unbind())
        }
        "get" => {
            if arguments.len() != 1 && arguments.len() != 2 {
                return Err(argument_count_error(
                    operation,
                    "one or two",
                    arguments.len(),
                ));
            }
            Ok(subject.call_method1("get", arguments.clone())?.unbind())
        }
        "getattr" => {
            require_arguments(arguments, operation, 1)?;
            python_getattr(py, subject, &arguments.get_item(0)?)
        }
        "repr" => {
            require_arguments(arguments, operation, 0)?;
            Ok(subject.repr()?.into_any().unbind())
        }
        _ => Err(PyValueError::new_err(format!(
            "unknown LookupDict trial operation: {operation}"
        ))),
    }
}

fn python_getattr(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    key: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(PyModule::import(py, "builtins")?
        .getattr("getattr")?
        .call1((subject, key))?
        .unbind())
}

fn require_arguments(
    arguments: &Bound<'_, PyTuple>,
    operation: &str,
    expected: usize,
) -> PyResult<()> {
    if arguments.len() == expected {
        Ok(())
    } else {
        Err(argument_count_error(
            operation,
            &expected.to_string(),
            arguments.len(),
        ))
    }
}

fn argument_count_error(operation: &str, expected: &str, actual: usize) -> PyErr {
    PyTypeError::new_err(format!(
        "{operation} expected {expected} trial arguments, got {actual}"
    ))
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    internal_utils_state(py)?;
    status_codes_state(py)?;
    module.add_function(wrap_pyfunction!(_internal_utils_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_status_codes_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_case_insensitive_dict_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_lookup_dict_trial, module)?)?;
    Ok(())
}

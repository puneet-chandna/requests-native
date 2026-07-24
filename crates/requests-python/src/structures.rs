use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyDict, PyDictMethods, PyInt, PyModule, PyString, PyTuple, PyTupleMethods,
    PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::structures::CaseInsensitiveMap;

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
    module.add_function(wrap_pyfunction!(_case_insensitive_dict_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_lookup_dict_trial, module)?)?;
    Ok(())
}

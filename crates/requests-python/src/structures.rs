use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyDict, PyInt, PyModule, PyString, PyTuple, PyTupleMethods,
};
use pyo3::wrap_pyfunction;
use requests::structures::CaseInsensitiveMap;

struct CaseInsensitiveSnapshot<'py> {
    store: Bound<'py, PyAny>,
    values: CaseInsensitiveMap<Py<PyAny>>,
}

#[pyfunction]
fn _case_insensitive_dict_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    if !is_exact_named_type(py, subject, "requests.structures", "CaseInsensitiveDict")? {
        return case_insensitive_fallback(py, subject, operation, arguments);
    }

    let Some(mut snapshot) = snapshot_case_insensitive_dict(py, subject)? else {
        return case_insensitive_fallback(py, subject, operation, arguments);
    };

    match operation {
        "set" => {
            require_arguments(arguments, operation, 2)?;
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() {
                return case_insensitive_fallback(py, subject, operation, arguments);
            }
            let Ok(cased) = key.extract::<String>() else {
                return case_insensitive_fallback(py, subject, operation, arguments);
            };
            let value = arguments.get_item(1)?;
            let lowered = python_lower(&key)?;
            let Ok(normalized) = lowered.extract::<String>() else {
                snapshot.store.set_item(&lowered, (&key, &value))?;
                return Ok(py.None());
            };
            snapshot
                .values
                .insert(normalized, cased, value.clone().unbind());
            snapshot.store.set_item(&lowered, (&key, &value))?;
            Ok(py.None())
        }
        "get" => {
            require_arguments(arguments, operation, 1)?;
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() || key.extract::<String>().is_err() {
                return case_insensitive_fallback(py, subject, operation, arguments);
            }
            let lowered = python_lower(&key)?;
            let Ok(normalized) = lowered.extract::<String>() else {
                return Ok(snapshot.store.get_item(&lowered)?.unbind());
            };
            if let Some(value) = snapshot.values.get(&normalized) {
                Ok(value.clone_ref(py))
            } else {
                Ok(snapshot.store.get_item(&lowered)?.unbind())
            }
        }
        "delete" => {
            require_arguments(arguments, operation, 1)?;
            let key = arguments.get_item(0)?;
            if !key.is_exact_instance_of::<PyString>() || key.extract::<String>().is_err() {
                return case_insensitive_fallback(py, subject, operation, arguments);
            }
            let lowered = python_lower(&key)?;
            let Ok(normalized) = lowered.extract::<String>() else {
                snapshot.store.del_item(&lowered)?;
                return Ok(py.None());
            };
            snapshot.values.remove(&normalized);
            snapshot.store.del_item(&lowered)?;
            Ok(py.None())
        }
        "iter" => {
            require_arguments(arguments, operation, 0)?;
            drop(snapshot);
            Ok(subject.try_iter()?.into_any().unbind())
        }
        "len" => {
            require_arguments(arguments, operation, 0)?;
            let length = snapshot.values.len();
            drop(snapshot);
            Ok(PyInt::new(py, length).into_any().unbind())
        }
        "lower_items" => {
            require_arguments(arguments, operation, 0)?;
            drop(snapshot);
            Ok(subject.call_method0("lower_items")?.unbind())
        }
        "eq" => {
            require_arguments(arguments, operation, 1)?;
            drop(snapshot);
            Ok(subject
                .call_method1("__eq__", (arguments.get_item(0)?,))?
                .unbind())
        }
        "copy" => {
            require_arguments(arguments, operation, 0)?;
            drop(snapshot);
            Ok(subject.call_method0("copy")?.unbind())
        }
        "repr" => {
            require_arguments(arguments, operation, 0)?;
            drop(snapshot);
            Ok(subject.repr()?.into_any().unbind())
        }
        _ => Err(PyValueError::new_err(format!(
            "unknown CaseInsensitiveDict trial operation: {operation}"
        ))),
    }
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

fn snapshot_case_insensitive_dict<'py>(
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
) -> PyResult<Option<CaseInsensitiveSnapshot<'py>>> {
    let store = match subject.getattr("_store") {
        Ok(store) => store,
        Err(_) => return Ok(None),
    };
    if !is_exact_named_type(py, &store, "collections", "OrderedDict")? {
        return Ok(None);
    }

    let mut values = CaseInsensitiveMap::new();
    for item in store.call_method0("items")?.try_iter()? {
        let item = item?;
        let Ok(item) = item.cast::<PyTuple>() else {
            return Ok(None);
        };
        if item.len() != 2 {
            return Ok(None);
        }
        let normalized = item.get_item(0)?;
        let stored = item.get_item(1)?;
        if !stored.is_exact_instance_of::<PyTuple>() {
            return Ok(None);
        }
        let Ok(stored) = stored.cast::<PyTuple>() else {
            return Ok(None);
        };
        if stored.len() != 2 {
            return Ok(None);
        }
        let cased = stored.get_item(0)?;
        if !normalized.is_exact_instance_of::<PyString>()
            || !cased.is_exact_instance_of::<PyString>()
        {
            return Ok(None);
        }
        let Ok(normalized) = normalized.extract::<String>() else {
            return Ok(None);
        };
        let Ok(cased) = cased.extract::<String>() else {
            return Ok(None);
        };
        values.insert(normalized, cased, stored.get_item(1)?.unbind());
    }

    Ok(Some(CaseInsensitiveSnapshot { store, values }))
}

fn python_lower<'py>(key: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    key.call_method0("lower")
}

#[pyfunction]
fn _lookup_dict_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    if !is_exact_named_type(py, subject, "requests.structures", "LookupDict")? {
        return lookup_fallback(py, subject, operation, arguments);
    }

    let attributes = subject.getattr("__dict__")?;
    let Ok(attributes) = attributes.cast::<PyDict>() else {
        return lookup_fallback(py, subject, operation, arguments);
    };
    if !attributes.is_exact_instance_of::<PyDict>() {
        return lookup_fallback(py, subject, operation, arguments);
    }

    match operation {
        "getitem" => {
            require_arguments(arguments, operation, 1)?;
            let key = arguments.get_item(0)?;
            Ok(attributes
                .get_item(&key)?
                .map_or_else(|| py.None(), |value| value.unbind()))
        }
        "get" => {
            if arguments.len() != 1 && arguments.len() != 2 {
                return Err(argument_count_error(
                    operation,
                    "one or two",
                    arguments.len(),
                ));
            }
            let key = arguments.get_item(0)?;
            match attributes.get_item(&key)? {
                Some(value) => Ok(value.unbind()),
                None if arguments.len() == 2 => Ok(arguments.get_item(1)?.unbind()),
                None => Ok(py.None()),
            }
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

fn is_exact_named_type(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    module_name: &str,
    type_name: &str,
) -> PyResult<bool> {
    let expected = PyModule::import(py, module_name)?.getattr(type_name)?;
    Ok(subject.is_exact_instance(&expected))
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

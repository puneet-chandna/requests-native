use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyDict, PyDictMethods, PyFunction, PyInt, PyModule, PyString, PyTuple,
    PyTupleMethods, PyType, PyTypeMethods,
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
    let result = case_insensitive_fallback(py, subject, operation, arguments)?;

    if should_exercise_case_insensitive_core(operation, arguments)
        && matches!(
            has_frozen_case_insensitive_descriptors(py, subject),
            Ok(true)
        )
    {
        let _ = exercise_case_insensitive_core(py, subject);
    }

    Ok(result)
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

fn exercise_case_insensitive_core(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<()> {
    let store = subject.getattr("_store")?;
    if !is_trusted_ordered_dict(py, &store)? {
        return Ok(());
    }
    let store = store.cast::<PyDict>()?;
    let mut values = CaseInsensitiveMap::new();
    for (normalized, stored) in store.iter() {
        if !stored.is_exact_instance_of::<PyTuple>() {
            return Ok(());
        }
        let Ok(stored) = stored.cast::<PyTuple>() else {
            return Ok(());
        };
        if stored.len() != 2 {
            return Ok(());
        }
        let cased = stored.get_item(0)?;
        if !normalized.is_exact_instance_of::<PyString>()
            || !cased.is_exact_instance_of::<PyString>()
        {
            return Ok(());
        }
        let Ok(normalized) = normalized.extract::<String>() else {
            return Ok(());
        };
        let Ok(cased) = cased.extract::<String>() else {
            return Ok(());
        };
        values.insert(normalized, cased, ());
    }

    let _ = values.len();
    Ok(())
}

fn should_exercise_case_insensitive_core(operation: &str, arguments: &Bound<'_, PyTuple>) -> bool {
    match operation {
        "len" => true,
        "set" | "get" | "delete" => {
            let Ok(key) = arguments.get_item(0) else {
                return false;
            };
            key.is_exact_instance_of::<PyString>() && key.extract::<String>().is_ok()
        }
        _ => false,
    }
}

fn has_frozen_case_insensitive_descriptors(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let subject_type = subject.get_type();
    let subject_metaclass = subject_type.get_type();
    if !is_trusted_abc_meta(py, &subject_metaclass)?
        || subject_type.module()?.to_str()? != "requests.structures"
        || subject_type.qualname()?.to_str()? != "CaseInsensitiveDict"
    {
        return Ok(false);
    }

    if !subject_type
        .getattr("__getattribute__")?
        .is(py.get_type::<PyAny>().getattr("__getattribute__")?)
    {
        return Ok(false);
    }

    for base in subject_type.mro().iter() {
        let base = base.cast::<PyType>()?;
        let base_metaclass = base.get_type();
        if !base_metaclass.is(py.get_type::<PyType>()) && !base_metaclass.is(&subject_metaclass) {
            return Ok(false);
        }
        let namespace = base.getattr("__dict__")?;
        for dynamic_name in ["__getattr__", "_store"] {
            if !namespace.call_method1("get", (dynamic_name,))?.is_none() {
                return Ok(false);
            }
        }
    }

    let namespace = subject_type.getattr("__dict__")?;
    for (name, first_line) in [
        ("__setitem__", 59),
        ("__getitem__", 64),
        ("__delitem__", 67),
        ("__len__", 73),
    ] {
        let descriptor = namespace.get_item(name)?;
        if !descriptor.is_exact_instance_of::<PyFunction>()
            || descriptor.getattr("__module__")?.extract::<String>()? != "requests.structures"
            || descriptor.getattr("__qualname__")?.extract::<String>()?
                != format!("CaseInsensitiveDict.{name}")
            || descriptor
                .getattr("__code__")?
                .getattr("co_firstlineno")?
                .extract::<usize>()?
                != first_line
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn is_trusted_abc_meta(py: Python<'_>, metaclass: &Bound<'_, PyType>) -> PyResult<bool> {
    if !metaclass.get_type().is(py.get_type::<PyType>())
        || metaclass.module()?.to_str()? != "abc"
        || metaclass.qualname()?.to_str()? != "ABCMeta"
    {
        return Ok(false);
    }

    let namespace = metaclass.getattr("__dict__")?;
    if !namespace
        .call_method1("get", ("__getattribute__",))?
        .is_none()
        || !namespace.call_method1("get", ("__getattr__",))?.is_none()
    {
        return Ok(false);
    }

    let bases = metaclass.bases();
    Ok(bases.len() == 1 && bases.get_item(0)?.is(py.get_type::<PyType>()))
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

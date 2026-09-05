#![deny(unsafe_code)]

use pyo3::exceptions::{PyAttributeError, PyNameError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};
use pyo3::wrap_pyfunction;

mod adapters;
mod auth;
mod body;
mod bridge;
mod callbacks;
mod cookies;
mod errors;
mod models;
mod response;
mod runtime;
mod sessions;
mod structures;

pub(crate) fn function_builtins<'py>(function: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    match function.getattr("__builtins__") {
        Ok(builtins) => Ok(builtins),
        Err(error) if error.is_instance_of::<PyAttributeError>(function.py()) => function
            .getattr("__globals__")?
            .cast_into::<PyDict>()?
            .get_item("__builtins__")?
            .ok_or_else(|| PyNameError::new_err("name '__builtins__' is not defined")),
        Err(error) => Err(error),
    }
}

pub(crate) fn function_builtins_dict<'py>(
    function: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let builtins = function_builtins(function)?;
    match builtins.cast::<PyDict>() {
        Ok(dictionary) => Ok(dictionary.clone()),
        Err(_) => Ok(builtins.getattr("__dict__")?.cast_into::<PyDict>()?),
    }
}

#[pyfunction]
fn backend_name() -> &'static str {
    "requests-native"
}

#[pymodule]
fn _requests_rust(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(backend_name, module)?)?;
    adapters::register(module)?;
    auth::register(module)?;
    body::register(module)?;
    callbacks::register(module)?;
    cookies::register(module)?;
    errors::register(module)?;
    models::register(module)?;
    response::register(module)?;
    runtime::register(module)?;
    sessions::register(module)?;
    structures::register(module)?;
    Ok(())
}

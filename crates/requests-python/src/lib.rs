#![deny(unsafe_code)]

use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

mod bridge;
mod models;
mod runtime;
mod structures;

#[pyfunction]
fn backend_name() -> &'static str {
    "requests-rust"
}

#[pymodule]
fn _requests_rust(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(backend_name, module)?)?;
    models::register(module)?;
    runtime::register(module)?;
    structures::register(module)?;
    Ok(())
}

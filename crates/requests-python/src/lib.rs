#![deny(unsafe_code)]

use pyo3::prelude::*;
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

#[pyfunction]
fn backend_name() -> &'static str {
    "requests-rust"
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

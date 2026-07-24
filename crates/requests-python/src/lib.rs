#![forbid(unsafe_code)]

use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

#[pyfunction]
fn backend_name() -> &'static str {
    "requests-rust"
}

#[pymodule]
fn _requests_rust(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(backend_name, module)?)?;
    Ok(())
}

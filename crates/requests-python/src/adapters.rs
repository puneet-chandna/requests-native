use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule, PyTuple};
use pyo3::wrap_pyfunction;

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

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_select_proxy_trial, module)?)?;
    Ok(())
}

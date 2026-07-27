use pyo3::exceptions::{PyNameError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyDict, PyDictMethods, PyModule, PyModuleMethods};
use pyo3::wrap_pyfunction;
use requests::{Error, ErrorKind};

struct ResponseErrorState {
    protocol_error: Py<PyAny>,
    decode_error: Py<PyAny>,
    read_timeout_error: Py<PyAny>,
    ssl_error: Py<PyAny>,
}

static RESPONSE_ERROR_STATE: PyOnceLock<ResponseErrorState> = PyOnceLock::new();

fn response_error_state(py: Python<'_>) -> PyResult<&ResponseErrorState> {
    RESPONSE_ERROR_STATE.get_or_try_init(py, || {
        let models = PyModule::import(py, "requests.models")?;
        Ok(ResponseErrorState {
            protocol_error: models.getattr("ProtocolError")?.unbind(),
            decode_error: models.getattr("DecodeError")?.unbind(),
            read_timeout_error: models.getattr("ReadTimeoutError")?.unbind(),
            ssl_error: models.getattr("SSLError")?.unbind(),
        })
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MappingSite {
    AdapterTransport,
    AdapterRetry,
    ResponseStream,
    ResponseJson,
    UrlPreparation,
    HeaderPreparation,
    Passthrough,
}

pub(crate) fn map_core_error(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    site: MappingSite,
    error: &Error,
    request: Option<&Bound<'_, PyAny>>,
    response: Option<&Bound<'_, PyAny>>,
) -> PyErr {
    map_typed_message(
        py,
        module,
        site,
        error.kind(),
        &error.to_string(),
        request,
        response,
    )
}

pub(crate) fn map_typed_message(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    site: MappingSite,
    kind: ErrorKind,
    message: &str,
    request: Option<&Bound<'_, PyAny>>,
    response: Option<&Bound<'_, PyAny>>,
) -> PyErr {
    let result = (|| -> PyResult<PyErr> {
        let target = match site {
            MappingSite::AdapterTransport => match kind {
                ErrorKind::ConnectTimeout => "ConnectTimeout",
                ErrorKind::ReadTimeout => "ReadTimeout",
                ErrorKind::Proxy => "ProxyError",
                ErrorKind::Tls | ErrorKind::Handshake => "SSLError",
                ErrorKind::InvalidUrl | ErrorKind::MissingSchema => "InvalidURL",
                _ => "ConnectionError",
            },
            MappingSite::AdapterRetry => "RetryError",
            MappingSite::UrlPreparation => match kind {
                ErrorKind::MissingSchema => "MissingSchema",
                _ => "InvalidURL",
            },
            MappingSite::HeaderPreparation => "InvalidHeader",
            _ => {
                return Err(PyValueError::new_err(
                    "typed message is unsupported at this mapping site",
                ));
            }
        };
        let class = module.getattr(target)?;
        let kwargs = PyDict::new(py);
        if let Some(request) = request {
            kwargs.set_item("request", request)?;
        }
        if let Some(response) = response {
            kwargs.set_item("response", response)?;
        }
        let value = if kwargs.is_empty() {
            class.call1((message,))?
        } else {
            class.call((message,), Some(&kwargs))?
        };
        Ok(PyErr::from_value(value))
    })();
    result.unwrap_or_else(|error| error)
}

pub(crate) fn map_original_error(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    site: MappingSite,
    original: PyErr,
    _request: Option<&Bound<'_, PyAny>>,
    _response: Option<&Bound<'_, PyAny>>,
) -> PyErr {
    match site {
        MappingSite::ResponseStream => map_stream_error(py, module, original),
        MappingSite::ResponseJson => map_json_error(py, module, original),
        MappingSite::Passthrough => original,
        _ => PyValueError::new_err("original error is unsupported at this mapping site"),
    }
}

fn name_error_with_context(py: Python<'_>, name: &str, original: &PyErr) -> PyErr {
    let error = PyNameError::new_err(format!("name '{name}' is not defined"));
    error.set_context(py, Some(original.clone_ref(py)));
    error
}

fn error_with_context(py: Python<'_>, error: PyErr, original: &PyErr) -> PyErr {
    error.set_context(py, Some(original.clone_ref(py)));
    error
}

fn map_stream_error(py: Python<'_>, module: &Bound<'_, PyModule>, original: PyErr) -> PyErr {
    let mappings = [
        ("ProtocolError", "ChunkedEncodingError"),
        ("DecodeError", "ContentDecodingError"),
        ("ReadTimeoutError", "ConnectionError"),
        ("SSLError", "RequestsSSLError"),
    ];
    for (source_name, target_name) in mappings {
        let Some(source) = module.dict().get_item(source_name).ok().flatten() else {
            return name_error_with_context(py, source_name, &original);
        };
        match original.value(py).is_instance(&source) {
            Ok(false) => continue,
            Ok(true) => {
                let Some(target) = module.dict().get_item(target_name).ok().flatten() else {
                    return name_error_with_context(py, target_name, &original);
                };
                let wrapped = match target.call1((original.value(py),)) {
                    Ok(value) => PyErr::from_value(value),
                    Err(error) => error,
                };
                wrapped.set_context(py, Some(original));
                return wrapped;
            }
            Err(error) => return error_with_context(py, error, &original),
        }
    }
    original
}

pub(crate) fn map_typed_response_error(py: Python<'_>, kind: ErrorKind, message: &str) -> PyErr {
    let state = match response_error_state(py) {
        Ok(state) => state,
        Err(error) => return error,
    };
    let original = match kind {
        ErrorKind::ContentDecoding => state.decode_error.bind(py).call1((message,)),
        ErrorKind::ReadTimeout => {
            state
                .read_timeout_error
                .bind(py)
                .call1((py.None(), py.None(), message))
        }
        ErrorKind::Tls | ErrorKind::Handshake => state.ssl_error.bind(py).call1((message,)),
        _ => state.protocol_error.bind(py).call1((message,)),
    };
    let original = match original {
        Ok(value) => PyErr::from_value(value),
        Err(error) => return error,
    };
    let models = match PyModule::import(py, "requests.models") {
        Ok(models) => models,
        Err(error) => return error,
    };
    map_stream_error(py, &models, original)
}

fn map_json_error(py: Python<'_>, module: &Bound<'_, PyModule>, original: PyErr) -> PyErr {
    let Some(source) = module.dict().get_item("JSONDecodeError").ok().flatten() else {
        return name_error_with_context(py, "JSONDecodeError", &original);
    };
    match original.value(py).is_instance(&source) {
        Ok(false) => return original,
        Ok(true) => {}
        Err(error) => return error_with_context(py, error, &original),
    }

    let Some(target) = module
        .dict()
        .get_item("RequestsJSONDecodeError")
        .ok()
        .flatten()
    else {
        return name_error_with_context(py, "RequestsJSONDecodeError", &original);
    };
    let value = original.value(py);
    let msg = match value.getattr("msg") {
        Ok(value) => value,
        Err(error) => return error_with_context(py, error, &original),
    };
    let doc = match value.getattr("doc") {
        Ok(value) => value,
        Err(error) => return error_with_context(py, error, &original),
    };
    let pos = match value.getattr("pos") {
        Ok(value) => value,
        Err(error) => return error_with_context(py, error, &original),
    };
    let wrapped = match target.call1((msg, doc, pos)) {
        Ok(value) => PyErr::from_value(value),
        Err(error) => error,
    };
    wrapped.set_context(py, Some(original));
    wrapped
}

fn trial_kind(kind: Option<&str>) -> PyResult<ErrorKind> {
    match kind {
        Some("connect_timeout") => Ok(ErrorKind::ConnectTimeout),
        Some("read_timeout") => Ok(ErrorKind::ReadTimeout),
        Some("proxy") => Ok(ErrorKind::Proxy),
        Some("tls") => Ok(ErrorKind::Tls),
        Some("handshake") => Ok(ErrorKind::Handshake),
        Some("invalid_url") => Ok(ErrorKind::InvalidUrl),
        Some("missing_schema") => Ok(ErrorKind::MissingSchema),
        Some("invalid_header") => Ok(ErrorKind::InvalidHeader),
        Some("retry_exhausted") => Ok(ErrorKind::Retry),
        Some("connection") | None => Ok(ErrorKind::Connection),
        Some(kind) => Err(PyValueError::new_err(format!(
            "unknown error mapping kind: {kind}"
        ))),
    }
}

fn trial_site(site: &str) -> PyResult<MappingSite> {
    match site {
        "adapter_transport" => Ok(MappingSite::AdapterTransport),
        "adapter_retry" => Ok(MappingSite::AdapterRetry),
        "response_stream" => Ok(MappingSite::ResponseStream),
        "response_json" => Ok(MappingSite::ResponseJson),
        "url" => Ok(MappingSite::UrlPreparation),
        "header" => Ok(MappingSite::HeaderPreparation),
        "passthrough" => Ok(MappingSite::Passthrough),
        _ => Err(PyValueError::new_err(format!(
            "unknown error mapping site: {site}"
        ))),
    }
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn _error_mapping_trial(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    site: &str,
    kind: Option<&str>,
    message: &str,
    request: Option<&Bound<'_, PyAny>>,
    response: Option<&Bound<'_, PyAny>>,
    original: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    let site = trial_site(site)?;
    let error = match original {
        Some(original) => map_original_error(
            py,
            module,
            site,
            PyErr::from_value(original.clone()),
            request,
            response,
        ),
        None => {
            let error = Error::from_binding_parts(trial_kind(kind)?, message);
            map_core_error(py, module, site, &error, request, response)
        }
    };
    Err(error)
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let _ = response_error_state(module.py())?;
    module.add_function(wrap_pyfunction!(_error_mapping_trial, module)?)?;
    Ok(())
}

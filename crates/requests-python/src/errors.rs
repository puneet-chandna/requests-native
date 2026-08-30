use pyo3::exceptions::{PyBaseException, PyNameError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyDict, PyDictMethods, PyModule, PyModuleMethods, PyTuple, PyTupleMethods, PyType,
};
use pyo3::wrap_pyfunction;
use requests::{Error, ErrorKind};

const INVALID_EXCEPT_TARGET: &str =
    "catching classes that do not inherit from BaseException is not allowed";

fn validate_exception_class(py: Python<'_>, exception: &Bound<'_, PyAny>) -> PyResult<()> {
    // CPython's CHECK_EXC_MATCH accepts one optional outer tuple. Nested tuple
    // members are invalid targets, and metaclass hooks are not consulted.
    let Ok(exception_type) = exception.cast::<PyType>() else {
        return Err(PyTypeError::new_err(INVALID_EXCEPT_TARGET));
    };
    if !exception_type.is_subclass(&py.get_type::<PyBaseException>())? {
        return Err(PyTypeError::new_err(INVALID_EXCEPT_TARGET));
    }
    Ok(())
}

pub(crate) fn exception_matches(
    py: Python<'_>,
    error: &PyErr,
    exception: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if let Ok(tuple) = exception.cast::<PyTuple>() {
        for candidate in tuple.iter() {
            validate_exception_class(py, &candidate)?;
        }
    } else {
        validate_exception_class(py, exception)?;
    }
    Ok(error.is_instance(py, exception))
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
        match exception_matches(py, &original, &source) {
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

pub(crate) fn map_typed_response_error(
    py: Python<'_>,
    error: &Error,
    pool: Option<&Bound<'_, PyAny>>,
) -> PyErr {
    let original = match canonical_response_error(py, error, pool) {
        Ok(original) => original,
        Err(error) => return error,
    };
    let models = match PyModule::import(py, "requests.models") {
        Ok(models) => models,
        Err(error) => return error,
    };
    map_stream_error(py, &models, original)
}

pub(crate) fn map_typed_raw_response_error(
    py: Python<'_>,
    error: &Error,
    pool: Option<&Bound<'_, PyAny>>,
) -> PyErr {
    canonical_response_error(py, error, pool).unwrap_or_else(|error| error)
}

fn urllib3_v2(py: Python<'_>) -> PyResult<bool> {
    Ok(!PyModule::import(py, "urllib3")?
        .getattr("__version__")?
        .extract::<String>()?
        .starts_with("1.26."))
}

fn error_with_source(py: Python<'_>, error: PyErr, source: &PyErr, explicit_cause: bool) -> PyErr {
    error.set_context(py, Some(source.clone_ref(py)));
    if explicit_cause {
        error.set_cause(py, Some(source.clone_ref(py)));
    }
    error
}

fn canonical_response_error(
    py: Python<'_>,
    error: &Error,
    pool: Option<&Bound<'_, PyAny>>,
) -> PyResult<PyErr> {
    let exceptions = PyModule::import(py, "urllib3.exceptions")?;
    let v2 = urllib3_v2(py)?;
    let pool = pool.map_or_else(|| py.None().into_bound(py), Bound::clone);
    match error.kind() {
        ErrorKind::ContentDecoding => Ok(PyErr::from_value(
            exceptions
                .getattr("DecodeError")?
                .call1((error.to_string(),))?,
        )),
        ErrorKind::ReadTimeout => {
            let timeout = PyErr::from_value(
                PyModule::import(py, "builtins")?
                    .getattr("TimeoutError")?
                    .call1(("timed out",))?,
            );
            let mapped = PyErr::from_value(exceptions.getattr("ReadTimeoutError")?.call1((
                pool,
                py.None(),
                "Read timed out.",
            ))?);
            Ok(error_with_source(py, mapped, &timeout, v2))
        }
        ErrorKind::Tls | ErrorKind::Handshake => Ok(PyErr::from_value(
            exceptions
                .getattr("SSLError")?
                .call1((error.to_string(),))?,
        )),
        _ => {
            if let Some((received, remaining)) = error.incomplete_body() {
                let incomplete = PyErr::from_value(
                    exceptions
                        .getattr("IncompleteRead")?
                        .call1((received, remaining))?,
                );
                let message = format!("Connection broken: {:?}", incomplete.value(py));
                let protocol = PyErr::from_value(
                    exceptions
                        .getattr("ProtocolError")?
                        .call1((message, incomplete.value(py)))?,
                );
                Ok(error_with_source(py, protocol, &incomplete, v2))
            } else {
                Ok(PyErr::from_value(
                    exceptions
                        .getattr("ProtocolError")?
                        .call1((error.to_string(),))?,
                ))
            }
        }
    }
}

pub(crate) fn map_decoder_error(py: Python<'_>, encoding: &str, original: PyErr) -> PyErr {
    let result = (|| -> PyResult<PyErr> {
        let response = PyModule::import(py, "urllib3.response")?;
        let classes = response
            .getattr("HTTPResponse")?
            .getattr("DECODER_ERROR_CLASSES")?;
        if !exception_matches(py, &original, &classes)? {
            return Ok(original.clone_ref(py));
        }
        let message = format!(
            "Received response with content-encoding: {encoding}, but failed to decode it."
        );
        let decode = PyErr::from_value(
            response
                .getattr("DecodeError")?
                .call1((message, original.value(py)))?,
        );
        let decode = error_with_source(py, decode, &original, urllib3_v2(py)?);
        let models = PyModule::import(py, "requests.models")?;
        Ok(map_stream_error(py, &models, decode))
    })();
    result.unwrap_or_else(|error| {
        error.set_context(py, Some(original));
        error
    })
}

fn map_json_error(py: Python<'_>, module: &Bound<'_, PyModule>, original: PyErr) -> PyErr {
    let Some(source) = module.dict().get_item("JSONDecodeError").ok().flatten() else {
        return name_error_with_context(py, "JSONDecodeError", &original);
    };
    match exception_matches(py, &original, &source) {
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
    module.add_function(wrap_pyfunction!(_error_mapping_trial, module)?)?;
    Ok(())
}

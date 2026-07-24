use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBytes, PyBytesMethods, PyDict, PyDictMethods, PyList, PyListMethods,
    PyModule, PyString, PyTuple, PyTupleMethods, PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::utils::{encode_query_pairs, trim_python_whitespace_start};
use requests::{
    HeaderInput, HeaderPart, HeaderPreparationError, InvalidHeaderPart, PreparedHeader,
    UrlPreparationError, append_url_params, is_non_http_url, prepare_headers, prepare_method,
    prepare_method_bytes, prepare_url, url_is_native_safe,
};

struct ModelsState {
    internal_utils: Py<PyModule>,
    models: Py<PyModule>,
    utils: Py<PyModule>,
    prepared_request: Py<PyType>,
    case_insensitive_dict: Py<PyType>,
    case_insensitive_dict_init: Py<PyAny>,
    case_insensitive_dict_setitem: Py<PyAny>,
    check_header_validity: Py<PyAny>,
    validate_header_part: Py<PyAny>,
    header_validators_str: Py<PyAny>,
    header_validators_byte: Py<PyAny>,
    utils_str: Py<PyAny>,
    utils_bytes: Py<PyAny>,
    to_native_string: Py<PyAny>,
    invalid_header: Py<PyAny>,
    invalid_url: Py<PyAny>,
    location_parse_error: Py<PyAny>,
    missing_schema: Py<PyAny>,
    parse_url: Py<PyAny>,
    requote_uri: Py<PyAny>,
    basestring: Py<PyAny>,
    to_key_val_list: Py<PyAny>,
    unicode_is_ascii: Py<PyAny>,
    urlencode: Py<PyAny>,
    urlunparse: Py<PyAny>,
    encode_params_descriptor: Py<PyAny>,
    builtin_str: Py<PyAny>,
    prepare_method: Py<PyAny>,
    prepare_headers: Py<PyAny>,
    prepare_url: Py<PyAny>,
    object_getattribute: Py<PyAny>,
    object_setattr: Py<PyAny>,
}

static MODELS_STATE: PyOnceLock<ModelsState> = PyOnceLock::new();

fn initialize_models_state(py: Python<'_>) -> PyResult<ModelsState> {
    let internal_utils = PyModule::import(py, "requests._internal_utils")?;
    let models = PyModule::import(py, "requests.models")?;
    let utils = PyModule::import(py, "requests.utils")?;
    let prepared_request = models.getattr("PreparedRequest")?.cast_into::<PyType>()?;
    let case_insensitive_dict = models
        .getattr("CaseInsensitiveDict")?
        .cast_into::<PyType>()?;
    let case_insensitive_dict_init = case_insensitive_dict.getattr("__init__")?;
    let case_insensitive_dict_setitem = case_insensitive_dict.getattr("__setitem__")?;
    let check_header_validity = models.getattr("check_header_validity")?;
    let validate_header_part = utils.getattr("_validate_header_part")?;
    let header_validators_str = utils.getattr("_HEADER_VALIDATORS_STR")?;
    let header_validators_byte = utils.getattr("_HEADER_VALIDATORS_BYTE")?;
    let utils_str = utils.getattr("str")?;
    let utils_bytes = utils.getattr("bytes")?;
    let to_native_string = models.getattr("to_native_string")?;
    let invalid_header = utils.getattr("InvalidHeader")?;
    let invalid_url = models.getattr("InvalidURL")?;
    let location_parse_error = models.getattr("LocationParseError")?;
    let missing_schema = models.getattr("MissingSchema")?;
    let parse_url = models.getattr("parse_url")?;
    let requote_uri = models.getattr("requote_uri")?;
    let basestring = models.getattr("basestring")?;
    let to_key_val_list = models.getattr("to_key_val_list")?;
    let unicode_is_ascii = models.getattr("unicode_is_ascii")?;
    let urlencode = models.getattr("urlencode")?;
    let urlunparse = models.getattr("urlunparse")?;
    let request_encoding_mixin = models
        .getattr("RequestEncodingMixin")?
        .cast_into::<PyType>()?;
    let encode_params_descriptor = request_encoding_mixin
        .getattr("__dict__")?
        .get_item("_encode_params")?;
    let builtin_str = internal_utils.getattr("builtin_str")?;
    let prepare_method = prepared_request.getattr("prepare_method")?;
    let prepare_headers = prepared_request.getattr("prepare_headers")?;
    let prepare_url = prepared_request.getattr("prepare_url")?;
    let object_getattribute = py.get_type::<PyAny>().getattr("__getattribute__")?;
    let object_setattr = py.get_type::<PyAny>().getattr("__setattr__")?;

    Ok(ModelsState {
        internal_utils: internal_utils.unbind(),
        models: models.unbind(),
        utils: utils.unbind(),
        prepared_request: prepared_request.unbind(),
        case_insensitive_dict: case_insensitive_dict.unbind(),
        case_insensitive_dict_init: case_insensitive_dict_init.unbind(),
        case_insensitive_dict_setitem: case_insensitive_dict_setitem.unbind(),
        check_header_validity: check_header_validity.unbind(),
        validate_header_part: validate_header_part.unbind(),
        header_validators_str: header_validators_str.unbind(),
        header_validators_byte: header_validators_byte.unbind(),
        utils_str: utils_str.unbind(),
        utils_bytes: utils_bytes.unbind(),
        to_native_string: to_native_string.unbind(),
        invalid_header: invalid_header.unbind(),
        invalid_url: invalid_url.unbind(),
        location_parse_error: location_parse_error.unbind(),
        missing_schema: missing_schema.unbind(),
        parse_url: parse_url.unbind(),
        requote_uri: requote_uri.unbind(),
        basestring: basestring.unbind(),
        to_key_val_list: to_key_val_list.unbind(),
        unicode_is_ascii: unicode_is_ascii.unbind(),
        urlencode: urlencode.unbind(),
        urlunparse: urlunparse.unbind(),
        encode_params_descriptor: encode_params_descriptor.unbind(),
        builtin_str: builtin_str.unbind(),
        prepare_method: prepare_method.unbind(),
        prepare_headers: prepare_headers.unbind(),
        prepare_url: prepare_url.unbind(),
        object_getattribute: object_getattribute.unbind(),
        object_setattr: object_setattr.unbind(),
    })
}

fn models_state(py: Python<'_>) -> PyResult<&ModelsState> {
    MODELS_STATE.get_or_try_init(py, || initialize_models_state(py))
}

fn trusted_bound_method<'py>(
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<(Bound<'py, PyAny>, bool)> {
    let callable = subject.getattr(name)?;
    let state = models_state(py)?;
    if !subject.get_type().is(state.prepared_request.bind(py))
        || !subject
            .get_type()
            .getattr("__getattribute__")?
            .is(state.object_getattribute.bind(py))
        || !subject
            .get_type()
            .getattr("__setattr__")?
            .is(state.object_setattr.bind(py))
    {
        return Ok((callable, false));
    }

    let Ok(function) = callable.getattr("__func__") else {
        return Ok((callable, false));
    };
    Ok((callable, function.is(expected.bind(py))))
}

#[pyfunction]
fn _prepare_method_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    method: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = models_state(py)?;
    let (callable, trusted) =
        trusted_bound_method(py, subject, "prepare_method", &state.prepare_method)?;
    if !trusted
        || !authoritative_field_is_none(subject, "method")?
        || !trusted_native_string_dependencies(py, state)?
    {
        return Ok(callable.call1((method,))?.unbind());
    }

    if method.is_none() {
        subject.setattr("method", method)?;
        return Ok(py.None());
    }

    let prepared = if method.is_exact_instance_of::<PyString>() {
        if let Ok(text) = method.cast::<PyString>()?.to_str()
            && text.is_ascii()
        {
            Some(prepare_method(text))
        } else {
            None
        }
    } else if method.is_exact_instance_of::<PyBytes>() {
        let bytes = method.cast::<PyBytes>()?.as_bytes();
        if bytes.is_ascii() {
            let prepared = prepare_method_bytes(bytes);
            Some(String::from_utf8(prepared).expect("ASCII stays UTF-8"))
        } else {
            None
        }
    } else {
        None
    };
    let Some(prepared) = prepared else {
        return Ok(callable.call1((method,))?.unbind());
    };

    subject.setattr("method", method)?;
    if !trusted_native_string_dependencies(py, state)? {
        return Ok(callable.call1((method,))?.unbind());
    }
    subject.setattr("method", prepared)?;
    Ok(py.None())
}

#[pyfunction]
fn _prepare_url_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    url: &Bound<'_, PyAny>,
    params: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = models_state(py)?;
    let (callable, trusted) = trusted_bound_method(py, subject, "prepare_url", &state.prepare_url)?;
    if !trusted {
        return Ok(callable.call1((url, params))?.unbind());
    }

    let Some(raw_url) = exact_url_text(url)? else {
        return Ok(callable.call1((url, params))?.unbind());
    };
    let trimmed_url = trim_python_whitespace_start(&raw_url);
    let preserve_input_identity =
        url.is_exact_instance_of::<PyString>() && trimmed_url.len() == raw_url.len();
    let raw_url = trimmed_url.to_owned();
    let url_repr = PyString::new(py, &raw_url).repr()?.to_str()?.to_owned();
    if is_non_http_url(&raw_url) {
        if preserve_input_identity {
            subject.setattr("url", url)?;
        } else {
            subject.setattr("url", &raw_url)?;
        }
        return Ok(py.None());
    }
    if !url_is_native_safe(&raw_url) {
        return Ok(callable.call1((url, params))?.unbind());
    }
    if !trusted_url_parse_dependencies(py, state)? {
        return Ok(callable.call1((url, params))?.unbind());
    }
    let base_url = match prepare_url(&raw_url, "") {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(url_preparation_error(
                py, state, error, &raw_url, &url_repr,
            )?);
        }
    };
    let encoded_params = if params.is_none() {
        String::new()
    } else {
        if !trusted_url_parameter_dependencies(py, state, subject)? {
            return Ok(callable.call1((url, params))?.unbind());
        }
        let Some(encoded_params) = exact_encoded_params(params)? else {
            return Ok(callable.call1((url, params))?.unbind());
        };
        encoded_params
    };
    subject.setattr("url", append_url_params(&base_url, &encoded_params))?;
    Ok(py.None())
}

fn trusted_native_string_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    Ok(state
        .models
        .bind(py)
        .getattr("to_native_string")?
        .is(state.to_native_string.bind(py))
        && state
            .internal_utils
            .bind(py)
            .getattr("builtin_str")?
            .is(state.builtin_str.bind(py)))
}

fn trusted_url_parse_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    let models = state.models.bind(py);
    Ok(models.getattr("parse_url")?.is(state.parse_url.bind(py))
        && models
            .getattr("requote_uri")?
            .is(state.requote_uri.bind(py))
        && models
            .getattr("unicode_is_ascii")?
            .is(state.unicode_is_ascii.bind(py))
        && models.getattr("urlunparse")?.is(state.urlunparse.bind(py))
        && models
            .getattr("LocationParseError")?
            .is(state.location_parse_error.bind(py))
        && models.getattr("InvalidURL")?.is(state.invalid_url.bind(py))
        && models
            .getattr("MissingSchema")?
            .is(state.missing_schema.bind(py)))
}

fn trusted_url_parameter_dependencies(
    py: Python<'_>,
    state: &ModelsState,
    subject: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let models = state.models.bind(py);
    Ok(trusted_native_string_dependencies(py, state)?
        && models.getattr("basestring")?.is(state.basestring.bind(py))
        && models
            .getattr("to_key_val_list")?
            .is(state.to_key_val_list.bind(py))
        && models.getattr("urlencode")?.is(state.urlencode.bind(py))
        && has_original_encode_params_descriptor(py, state, subject)?)
}

fn has_original_encode_params_descriptor(
    py: Python<'_>,
    state: &ModelsState,
    subject: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let instance_dict = subject.getattr("__dict__")?.cast_into::<PyDict>()?;
    if instance_dict.contains("_encode_params")? {
        return Ok(false);
    }

    for class in subject.get_type().mro().iter() {
        let class_dict = class.getattr("__dict__")?;
        if class_dict.contains("_encode_params")? {
            return Ok(class_dict
                .get_item("_encode_params")?
                .is(state.encode_params_descriptor.bind(py)));
        }
    }
    Ok(false)
}

fn exact_url_text(url: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    let text = if url.is_exact_instance_of::<PyString>() {
        url.cast::<PyString>()?.clone()
    } else if url.is_exact_instance_of::<PyBytes>() {
        url.call_method1("decode", ("utf8",))?
            .cast_into::<PyString>()?
    } else {
        return Ok(None);
    };
    let Ok(raw_url) = text.to_str() else {
        return Ok(None);
    };
    Ok(Some(raw_url.to_owned()))
}

fn exact_encoded_params(params: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if params.is_none() {
        return Ok(Some(String::new()));
    }
    if params.is_exact_instance_of::<PyString>() {
        return Ok(params.cast::<PyString>()?.to_str().ok().map(str::to_owned));
    }
    if params.is_exact_instance_of::<PyBytes>() {
        return Ok(Some(
            params
                .call_method1("decode", ("ascii",))?
                .cast_into::<PyString>()?
                .to_str()?
                .to_owned(),
        ));
    }
    if !params.is_exact_instance_of::<PyDict>() {
        return Ok(None);
    }

    let mut pairs = Vec::new();
    for (name, values) in params.cast::<PyDict>()?.iter() {
        let Some(name) = exact_parameter_bytes(&name)? else {
            return Ok(None);
        };
        if values.is_none() {
            continue;
        }
        if let Some(value) = exact_parameter_bytes(&values)? {
            pairs.push((name, value));
            continue;
        }

        if values.is_exact_instance_of::<PyList>() {
            for value in values.cast::<PyList>()?.iter() {
                if value.is_none() {
                    continue;
                }
                let Some(value) = exact_parameter_bytes(&value)? else {
                    return Ok(None);
                };
                pairs.push((name.clone(), value));
            }
        } else if values.is_exact_instance_of::<PyTuple>() {
            for value in values.cast::<PyTuple>()?.iter() {
                if value.is_none() {
                    continue;
                }
                let Some(value) = exact_parameter_bytes(&value)? else {
                    return Ok(None);
                };
                pairs.push((name.clone(), value));
            }
        } else {
            return Ok(None);
        }
    }
    Ok(Some(encode_query_pairs(&pairs)))
}

fn exact_parameter_bytes(value: &Bound<'_, PyAny>) -> PyResult<Option<Vec<u8>>> {
    if value.is_exact_instance_of::<PyBytes>() {
        return Ok(Some(value.cast::<PyBytes>()?.as_bytes().to_vec()));
    }
    if value.is_exact_instance_of::<PyString>() {
        return Ok(value
            .cast::<PyString>()?
            .to_str()
            .ok()
            .map(|value| value.as_bytes().to_vec()));
    }
    Ok(None)
}

fn url_preparation_error(
    py: Python<'_>,
    state: &ModelsState,
    error: UrlPreparationError,
    raw_url: &str,
    url_repr: &str,
) -> PyResult<PyErr> {
    let (exception, message) = match error {
        UrlPreparationError::InvalidLabel => {
            (&state.invalid_url, "URL has an invalid label.".to_owned())
        }
        UrlPreparationError::MissingHost => (
            &state.invalid_url,
            format!("Invalid URL {url_repr}: No host supplied"),
        ),
        UrlPreparationError::MissingScheme => (
            &state.missing_schema,
            format!(
                "Invalid URL {url_repr}: No scheme supplied. Perhaps you meant https://{raw_url}?"
            ),
        ),
        UrlPreparationError::Parse => (&state.invalid_url, format!("Failed to parse: {raw_url}")),
    };
    let instance = exception.bind(py).call1((message,))?;
    Ok(PyErr::from_value(instance))
}

struct PythonHeaderRow {
    name: Py<PyAny>,
    value: Py<PyAny>,
    name_was_bytes: bool,
}

#[pyfunction]
fn _prepare_headers_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    headers: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = models_state(py)?;
    let (callable, trusted) =
        trusted_bound_method(py, subject, "prepare_headers", &state.prepare_headers)?;
    if !trusted
        || !authoritative_field_is_none(subject, "headers")?
        || !trusted_header_dependencies(py, state)?
    {
        return Ok(callable.call1((headers,))?.unbind());
    }
    if !is_exact_header_candidate(py, headers)? {
        return Ok(callable.call1((headers,))?.unbind());
    }

    let output = state.case_insensitive_dict.bind(py).call0()?;
    subject.setattr("headers", &output)?;
    if !trusted_header_runtime_dependencies(py, state)? {
        return Ok(callable.call1((headers,))?.unbind());
    }

    let Some((native_headers, python_rows)) = exact_header_rows(py, headers)? else {
        return Ok(callable.call1((headers,))?.unbind());
    };
    let result = prepare_headers(&native_headers);
    let (prepared, error) = match result {
        Ok(prepared) => (prepared, None),
        Err(error) => (error.prepared.clone(), Some(error)),
    };
    materialize_headers(py, &output, &python_rows, &prepared)?;

    if let Some(error) = error {
        return Err(header_preparation_error(py, state, &python_rows, &error)?);
    }
    Ok(py.None())
}

fn authoritative_field_is_none(subject: &Bound<'_, PyAny>, name: &str) -> PyResult<bool> {
    let instance_dict = subject.getattr("__dict__")?.cast_into::<PyDict>()?;
    Ok(instance_dict
        .get_item(name)?
        .is_some_and(|value| value.is_none()))
}

fn trusted_header_runtime_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    let models = state.models.bind(py);
    let utils = state.utils.bind(py);
    let class = state.case_insensitive_dict.bind(py);
    Ok(trusted_native_string_dependencies(py, state)?
        && models
            .getattr("check_header_validity")?
            .is(state.check_header_validity.bind(py))
        && utils
            .getattr("_validate_header_part")?
            .is(state.validate_header_part.bind(py))
        && utils
            .getattr("_HEADER_VALIDATORS_STR")?
            .is(state.header_validators_str.bind(py))
        && utils
            .getattr("_HEADER_VALIDATORS_BYTE")?
            .is(state.header_validators_byte.bind(py))
        && utils.getattr("str")?.is(state.utils_str.bind(py))
        && utils.getattr("bytes")?.is(state.utils_bytes.bind(py))
        && utils
            .getattr("InvalidHeader")?
            .is(state.invalid_header.bind(py))
        && class
            .getattr("__setitem__")?
            .is(state.case_insensitive_dict_setitem.bind(py)))
}

fn is_exact_header_candidate(py: Python<'_>, headers: &Bound<'_, PyAny>) -> PyResult<bool> {
    if headers.is_none() || headers.is_exact_instance_of::<PyDict>() {
        return Ok(true);
    }
    if !is_trusted_ordered_dict(py, headers)? {
        return Ok(false);
    }
    let instance_dict = headers.getattr("__dict__")?.cast_into::<PyDict>()?;
    Ok(!instance_dict.contains("items")?)
}

fn trusted_header_dependencies(py: Python<'_>, state: &ModelsState) -> PyResult<bool> {
    let models = state.models.bind(py);
    let class = state.case_insensitive_dict.bind(py);
    Ok(trusted_header_runtime_dependencies(py, state)?
        && models
            .getattr("CaseInsensitiveDict")?
            .is(state.case_insensitive_dict.bind(py))
        && class
            .getattr("__init__")?
            .is(state.case_insensitive_dict_init.bind(py)))
}

fn exact_header_rows(
    py: Python<'_>,
    headers: &Bound<'_, PyAny>,
) -> PyResult<Option<(Vec<HeaderInput>, Vec<PythonHeaderRow>)>> {
    if headers.is_none() {
        return Ok(Some((Vec::new(), Vec::new())));
    }

    let mut native = Vec::new();
    let mut python = Vec::new();
    if headers.is_exact_instance_of::<PyDict>() {
        for (name, value) in headers.cast::<PyDict>()?.iter() {
            if !push_exact_header(py, &name, &value, &mut native, &mut python)? {
                return Ok(None);
            }
        }
        return Ok(Some((native, python)));
    }
    if !is_exact_header_candidate(py, headers)? {
        return Ok(None);
    }
    for (name, _) in headers.cast::<PyDict>()?.iter() {
        if !exact_header_name_is_native(&name)? {
            return Ok(None);
        }
    }
    for item in headers.call_method0("items")?.try_iter()? {
        let item = item?.cast_into::<PyTuple>()?;
        let name = item.get_item(0)?;
        let value = item.get_item(1)?;
        if !push_exact_header(py, &name, &value, &mut native, &mut python)? {
            return Ok(None);
        }
    }
    Ok(Some((native, python)))
}

fn exact_header_name_is_native(name: &Bound<'_, PyAny>) -> PyResult<bool> {
    if name.is_exact_instance_of::<PyString>() {
        return Ok(name.cast::<PyString>()?.to_str().is_ok_and(str::is_ascii));
    }
    if name.is_exact_instance_of::<PyBytes>() {
        return Ok(name.cast::<PyBytes>()?.as_bytes().is_ascii());
    }
    Ok(false)
}

fn push_exact_header(
    py: Python<'_>,
    name: &Bound<'_, PyAny>,
    value: &Bound<'_, PyAny>,
    native: &mut Vec<HeaderInput>,
    python: &mut Vec<PythonHeaderRow>,
) -> PyResult<bool> {
    let (native_name, name_was_bytes) = if name.is_exact_instance_of::<PyString>() {
        let Ok(text) = name.cast::<PyString>()?.to_str() else {
            return Ok(false);
        };
        if !text.is_ascii() {
            return Ok(false);
        }
        (HeaderPart::Text(text.to_owned()), false)
    } else if name.is_exact_instance_of::<PyBytes>() {
        let bytes = name.cast::<PyBytes>()?.as_bytes();
        if !bytes.is_ascii() {
            return Ok(false);
        }
        (HeaderPart::Bytes(bytes.to_vec()), true)
    } else {
        return Ok(false);
    };

    let native_value = if value.is_exact_instance_of::<PyString>() {
        let Ok(text) = value.cast::<PyString>()?.to_str() else {
            return Ok(false);
        };
        if !text.is_ascii() {
            return Ok(false);
        }
        HeaderPart::Text(text.to_owned())
    } else if value.is_exact_instance_of::<PyBytes>() {
        HeaderPart::Bytes(value.cast::<PyBytes>()?.as_bytes().to_vec())
    } else {
        return Ok(false);
    };

    native.push(HeaderInput {
        name: native_name,
        value: native_value,
    });
    python.push(PythonHeaderRow {
        name: name.clone().unbind(),
        value: value.clone().unbind(),
        name_was_bytes,
    });
    let _ = py;
    Ok(true)
}

fn materialize_headers(
    py: Python<'_>,
    output: &Bound<'_, PyAny>,
    rows: &[PythonHeaderRow],
    prepared: &[PreparedHeader],
) -> PyResult<()> {
    for header in prepared {
        let row = &rows[header.source_index];
        if row.name_was_bytes {
            output.set_item(PyString::new(py, &header.name), row.value.bind(py))?;
        } else {
            output.set_item(row.name.bind(py), row.value.bind(py))?;
        }
    }
    Ok(())
}

fn header_preparation_error(
    py: Python<'_>,
    state: &ModelsState,
    rows: &[PythonHeaderRow],
    error: &HeaderPreparationError,
) -> PyResult<PyErr> {
    let row = &rows[error.source_index];
    let part = match error.part {
        InvalidHeaderPart::Name => row.name.bind(py),
        InvalidHeaderPart::Value => row.value.bind(py),
    };
    let kind = match error.part {
        InvalidHeaderPart::Name => "name",
        InvalidHeaderPart::Value => "value",
    };
    let message = format!(
        "Invalid leading whitespace, reserved character(s), or return character(s) in header {kind}: {}",
        part.repr()?.to_str()?
    );
    let instance = state.invalid_header.bind(py).call1((message,))?;
    Ok(PyErr::from_value(instance))
}

fn is_trusted_ordered_dict(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<bool> {
    let value_type = value.get_type();
    if !value_type.get_type().is(py.get_type::<PyType>()) {
        return Ok(false);
    }

    let flags = value_type.getattr("__flags__")?.extract::<u64>()?;
    const IMMUTABLE_TYPE: u64 = 1 << 8;
    const HEAP_TYPE: u64 = 1 << 9;
    if flags & HEAP_TYPE != 0
        || flags & IMMUTABLE_TYPE == 0
        || value_type.module()?.to_str()? != "collections"
        || value_type.qualname()?.to_str()? != "OrderedDict"
    {
        return Ok(false);
    }
    let bases = value_type.bases();
    Ok(bases.len() == 1 && bases.get_item(0)?.is(py.get_type::<PyDict>()))
}

#[pyfunction]
fn _prepared_fields_snapshot(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let headers = subject.getattr("headers")?;
    let method = subject.getattr("method")?;
    let url = subject.getattr("url")?;
    let header_rows = if headers.is_none() {
        py.None()
    } else {
        py.get_type::<PyList>()
            .call1((headers.call_method0("items")?,))?
            .unbind()
    };

    let snapshot = PyDict::new(py);
    snapshot.set_item("method", method)?;
    snapshot.set_item("url", url)?;
    snapshot.set_item("headers", header_rows)?;
    Ok(snapshot.into_any().unbind())
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    let _ = models_state(py)?;
    module.add_function(wrap_pyfunction!(_prepare_method_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_url_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_headers_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepared_fields_snapshot, module)?)?;
    Ok(())
}

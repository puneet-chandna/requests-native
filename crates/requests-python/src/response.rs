use std::cell::Cell;
use std::collections::HashMap;
use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use pyo3::basic::CompareOp;
use pyo3::exceptions::{
    PyAssertionError, PyAttributeError, PyLookupError, PyNameError, PyRuntimeError,
    PyStopIteration, PyTypeError, PyUnicodeDecodeError,
};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBool, PyBytes, PyBytesMethods, PyDict, PyDictMethods, PyFunction, PyInt,
    PyList, PyListMethods, PyModule, PyString, PyTuple, PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::{ResponseDispositionState, ResponseEvent};

use crate::bridge::{BridgeClosed, WorkerPayload};
use crate::models::{canonical_code, intrinsic_builtin_name_is};
use crate::runtime::run_with_actions_and_signal_checker;

#[derive(Clone, Copy)]
enum DescriptorKind {
    Function,
    Property,
}

struct CanonicalDescriptor {
    kind: DescriptorKind,
    code: Py<PyAny>,
    builtins: Py<PyDict>,
}

enum ExpectedGlobal {
    Missing,
    Value(Py<PyAny>),
    Rejected,
}

struct ResponseState {
    models: Py<PyModule>,
    response_type: Py<PyType>,
    object_getattribute: Py<PyAny>,
    object_setattr: Py<PyAny>,
    descriptors: HashMap<&'static str, CanonicalDescriptor>,
    globals: HashMap<&'static str, ExpectedGlobal>,
    trusted_at_import: bool,
}

static RESPONSE_STATE: PyOnceLock<ResponseState> = PyOnceLock::new();

const DESCRIPTORS: &[(&str, DescriptorKind)] = &[
    ("content", DescriptorKind::Property),
    ("iter_content", DescriptorKind::Function),
    ("iter_lines", DescriptorKind::Function),
    ("text", DescriptorKind::Property),
    ("apparent_encoding", DescriptorKind::Property),
    ("json", DescriptorKind::Function),
    ("__repr__", DescriptorKind::Function),
    ("__bool__", DescriptorKind::Function),
    ("ok", DescriptorKind::Property),
    ("is_redirect", DescriptorKind::Property),
    ("is_permanent_redirect", DescriptorKind::Property),
    ("next", DescriptorKind::Property),
    ("raise_for_status", DescriptorKind::Function),
    ("__getstate__", DescriptorKind::Function),
    ("__setstate__", DescriptorKind::Function),
    ("close", DescriptorKind::Function),
];

const GLOBALS: &[&str] = &[
    "CONTENT_CHUNK_SIZE",
    "iter_slices",
    "stream_decode_response_unicode",
    "ProtocolError",
    "DecodeError",
    "ReadTimeoutError",
    "SSLError",
    "ChunkedEncodingError",
    "ContentDecodingError",
    "ConnectionError",
    "RequestsSSLError",
    "StreamConsumedError",
    "chardet",
    "complexjson",
    "guess_json_utf",
    "JSONDecodeError",
    "RequestsJSONDecodeError",
    "HTTPError",
    "REDIRECT_STATI",
    "codes",
    "getattr",
    "setattr",
    "bool",
    "int",
    "hasattr",
    "cast",
    "bytes",
    "UnicodeDecodeError",
    "str",
    "isinstance",
    "type",
    "len",
    "RuntimeError",
    "LookupError",
    "TypeError",
];

const CANONICALLY_MISSING_GLOBALS: &[&str] = &[
    "getattr",
    "setattr",
    "bool",
    "int",
    "hasattr",
    "bytes",
    "UnicodeDecodeError",
    "str",
    "isinstance",
    "type",
    "len",
    "RuntimeError",
    "LookupError",
    "TypeError",
];

const INSTANCE_SHADOWABLE_METHODS: &[&str] = &[
    "iter_content",
    "iter_lines",
    "json",
    "raise_for_status",
    "__getstate__",
    "__setstate__",
    "close",
];

fn raw_direct_type_entry<'py>(
    class: &Bound<'py, PyType>,
    name: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let namespace = class.getattr("__dict__")?;
    if namespace.contains(name)? {
        Ok(Some(namespace.get_item(name)?))
    } else {
        Ok(None)
    }
}

fn raw_mro_type_entry<'py>(
    class: &Bound<'py, PyType>,
    name: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    for owner in class.mro().iter() {
        let namespace = owner.getattr("__dict__")?;
        if namespace.contains(name)? {
            return Ok(Some(namespace.get_item(name)?));
        }
    }
    Ok(None)
}

fn descriptor_function<'py>(
    descriptor: &Bound<'py, PyAny>,
    kind: DescriptorKind,
) -> PyResult<Bound<'py, PyFunction>> {
    let function = match kind {
        DescriptorKind::Function => descriptor.clone(),
        DescriptorKind::Property => descriptor.getattr("fget")?,
    };
    Ok(function.cast_into::<PyFunction>()?)
}

fn function_matches_code(
    function: &Bound<'_, PyFunction>,
    code: &Bound<'_, PyAny>,
    globals: &Bound<'_, PyDict>,
) -> PyResult<bool> {
    Ok(function.getattr("__code__")?.eq(code)?
        && function.getattr("__globals__")?.is(globals)
        && function.getattr("__closure__")?.is_none()
        && function.getattr("__kwdefaults__")?.is_none())
}

fn descriptor_matches(
    class: &Bound<'_, PyType>,
    name: &str,
    expected: &CanonicalDescriptor,
    globals: &Bound<'_, PyDict>,
) -> PyResult<bool> {
    let Some(current) = raw_direct_type_entry(class, name)? else {
        return Ok(false);
    };
    let Ok(function) = descriptor_function(&current, expected.kind) else {
        return Ok(false);
    };
    Ok(
        function_matches_code(&function, expected.code.bind(function.py()), globals)?
            && function
                .getattr("__builtins__")?
                .is(expected.builtins.bind(function.py())),
    )
}

fn known_module_attr_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    module: &str,
    name: &str,
) -> PyResult<bool> {
    Ok(PyModule::import(py, module)?.getattr(name)?.is(current))
}

fn canonical_helper_is(
    py: Python<'_>,
    current: &Bound<'_, PyAny>,
    module: &str,
    name: &str,
) -> PyResult<bool> {
    let Ok(function) = current.cast::<PyFunction>() else {
        return Ok(false);
    };
    let code = canonical_code(py, module, name)?;
    let globals = PyModule::import(py, module)?.dict();
    function_matches_code(function, code.as_any(), &globals)
}

fn module_identity_is_current(py: Python<'_>, current: &Bound<'_, PyAny>) -> PyResult<bool> {
    let Ok(module) = current.cast::<PyModule>() else {
        return Ok(false);
    };
    let name = module.name()?.to_str()?.to_owned();
    Ok(py
        .import("sys")?
        .getattr("modules")?
        .get_item(&name)?
        .is(module))
}

fn global_has_canonical_provenance(
    py: Python<'_>,
    models: &Bound<'_, PyModule>,
    name: &str,
    current: Option<&Bound<'_, PyAny>>,
) -> PyResult<bool> {
    let Some(current) = current else {
        return Ok(matches!(
            name,
            "getattr"
                | "setattr"
                | "bool"
                | "int"
                | "hasattr"
                | "bytes"
                | "UnicodeDecodeError"
                | "str"
                | "isinstance"
                | "type"
                | "len"
                | "RuntimeError"
                | "LookupError"
                | "TypeError"
        ));
    };
    match name {
        "CONTENT_CHUNK_SIZE" => {
            Ok(current.is_exact_instance_of::<PyInt>() && current.extract::<usize>()? == 10 * 1024)
        }
        "iter_slices" | "stream_decode_response_unicode" | "guess_json_utf" => {
            canonical_helper_is(py, current, "requests.utils", name)
        }
        "cast" => known_module_attr_is(py, current, "typing", name),
        "ProtocolError" | "DecodeError" | "ReadTimeoutError" | "SSLError" => {
            known_module_attr_is(py, current, "urllib3.exceptions", name)
        }
        "ChunkedEncodingError"
        | "ContentDecodingError"
        | "ConnectionError"
        | "RequestsSSLError"
        | "StreamConsumedError"
        | "RequestsJSONDecodeError"
        | "HTTPError" => {
            let public_name = if name == "RequestsSSLError" {
                "SSLError"
            } else if name == "RequestsJSONDecodeError" {
                "JSONDecodeError"
            } else {
                name
            };
            known_module_attr_is(py, current, "requests.exceptions", public_name)
        }
        "chardet" => Ok(current.is_none() || module_identity_is_current(py, current)?),
        "complexjson" => module_identity_is_current(py, current),
        "JSONDecodeError" => {
            let complexjson = models.getattr("complexjson")?;
            Ok(complexjson.getattr("JSONDecodeError")?.is(current))
        }
        "REDIRECT_STATI" => Ok(current.extract::<Vec<u16>>()? == vec![301, 302, 303, 307, 308]),
        "codes" => Ok(
            current.getattr("moved_permanently")?.extract::<u16>()? == 301
                && current.getattr("permanent_redirect")?.extract::<u16>()? == 308,
        ),
        _ => Ok(false),
    }
}

fn initialize_response_state(py: Python<'_>) -> PyResult<ResponseState> {
    let models = PyModule::import(py, "requests.models")?;
    let response_type = models.getattr("Response")?.cast_into::<PyType>()?;
    let object = py.get_type::<PyAny>();
    let object_getattribute = object.getattr("__getattribute__")?.unbind();
    let object_setattr = object.getattr("__setattr__")?.unbind();

    let mut descriptors = HashMap::new();
    for &(name, kind) in DESCRIPTORS {
        let code = canonical_code(py, "requests.models", &format!("Response.{name}"))?;
        let descriptor = raw_direct_type_entry(&response_type, name)?
            .ok_or_else(|| PyRuntimeError::new_err(format!("missing Response.{name}")))?;
        let function = descriptor_function(&descriptor, kind)?;
        descriptors.insert(
            name,
            CanonicalDescriptor {
                kind,
                code: code.into_any().unbind(),
                builtins: function
                    .getattr("__builtins__")?
                    .cast_into::<PyDict>()?
                    .unbind(),
            },
        );
    }

    let mut globals = HashMap::new();
    for &name in GLOBALS {
        let current = models.dict().get_item(name)?;
        let expected = if CANONICALLY_MISSING_GLOBALS.contains(&name) {
            ExpectedGlobal::Missing
        } else if name == "cast" {
            ExpectedGlobal::Value(PyModule::import(py, "typing")?.getattr("cast")?.unbind())
        } else if !global_has_canonical_provenance(py, &models, name, current.as_ref())
            .unwrap_or(false)
        {
            ExpectedGlobal::Rejected
        } else if let Some(current) = current {
            ExpectedGlobal::Value(current.unbind())
        } else {
            ExpectedGlobal::Missing
        };
        globals.insert(name, expected);
    }

    let state = ResponseState {
        models: models.clone().unbind(),
        response_type: response_type.clone().unbind(),
        object_getattribute,
        object_setattr,
        descriptors,
        globals,
        trusted_at_import: true,
    };
    let trusted_at_import = DESCRIPTORS.iter().all(|(name, _)| {
        let expected = state
            .descriptors
            .get(name)
            .expect("canonical descriptor is present");
        descriptor_matches(&response_type, name, expected, &models.dict()).unwrap_or(false)
    }) && raw_mro_type_entry(&response_type, "__getattribute__")?
        .is_some_and(|value| value.is(state.object_getattribute.bind(py)))
        && raw_mro_type_entry(&response_type, "__setattr__")?
            .is_some_and(|value| value.is(state.object_setattr.bind(py)));
    Ok(ResponseState {
        trusted_at_import,
        ..state
    })
}

fn response_state(py: Python<'_>) -> PyResult<&ResponseState> {
    RESPONSE_STATE.get_or_try_init(py, || initialize_response_state(py))
}

fn type_is_current(
    py: Python<'_>,
    state: &ResponseState,
    current: &Bound<'_, PyType>,
) -> PyResult<bool> {
    if !state.trusted_at_import
        || !current.is(state.response_type.bind(py))
        || !state
            .models
            .bind(py)
            .dict()
            .get_item("Response")?
            .is_some_and(|value| value.is(current))
    {
        return Ok(false);
    }
    Ok(raw_mro_type_entry(current, "__getattribute__")?
        .is_some_and(|value| value.is(state.object_getattribute.bind(py)))
        && raw_mro_type_entry(current, "__setattr__")?
            .is_some_and(|value| value.is(state.object_setattr.bind(py))))
}

fn globals_are_current(
    py: Python<'_>,
    state: &ResponseState,
    descriptor: &CanonicalDescriptor,
    names: &[&str],
) -> PyResult<bool> {
    let models = state.models.bind(py);
    for name in names {
        let current = models.dict().get_item(*name)?;
        let Some(expected) = state.globals.get(name) else {
            return Ok(false);
        };
        let matches = match expected {
            ExpectedGlobal::Missing => {
                current.is_none()
                    && intrinsic_builtin_name_is(py, descriptor.builtins.bind(py), name)?
            }
            ExpectedGlobal::Value(expected) => current
                .as_ref()
                .is_some_and(|current| current.is(expected.bind(py))),
            ExpectedGlobal::Rejected => false,
        };
        if !matches {
            return Ok(false);
        }
        if let Some(current) = current.as_ref()
            && matches!(
                *name,
                "iter_slices" | "stream_decode_response_unicode" | "guess_json_utf"
            )
            && !global_has_canonical_provenance(py, models, name, Some(current))?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn exact_operation(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    globals: &[&str],
) -> PyResult<bool> {
    let state = response_state(py)?;
    if !type_is_current(py, state, &subject.get_type())? {
        return Ok(false);
    }
    if INSTANCE_SHADOWABLE_METHODS.contains(&operation)
        && subject
            .getattr("__dict__")?
            .cast::<PyDict>()?
            .contains(operation)?
    {
        return Ok(false);
    }
    let Some(expected) = state.descriptors.get(operation) else {
        return Ok(false);
    };
    Ok(descriptor_matches(
        &subject.get_type(),
        operation,
        expected,
        &state.models.bind(py).dict(),
    )? && globals_are_current(py, state, expected, globals)?)
}

fn fallback_iter_content(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
) -> PyResult<Py<PyAny>> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("decode_unicode", decode_unicode)?;
    Ok(subject
        .call_method("iter_content", (chunk_size,), Some(&kwargs))?
        .unbind())
}

fn fallback_iter_lines(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
    delimiter: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("chunk_size", chunk_size)?;
    kwargs.set_item("decode_unicode", decode_unicode)?;
    kwargs.set_item("delimiter", delimiter)?;
    Ok(subject
        .call_method("iter_lines", (), Some(&kwargs))?
        .unbind())
}

fn chunk_size_is_valid(py: Python<'_>, chunk_size: &Bound<'_, PyAny>) -> bool {
    chunk_size.is_none()
        || chunk_size
            .is_instance(&py.get_type::<PyInt>())
            .unwrap_or(false)
}

fn invalid_chunk_size(chunk_size: &Bound<'_, PyAny>) -> PyErr {
    PyTypeError::new_err(format!(
        "chunk_size must be an int, it is instead a {}.",
        chunk_size.get_type().repr().map_or_else(
            |_| "<unknown type>".to_owned(),
            |value| value.to_string_lossy().into_owned()
        )
    ))
}

enum ContentSource {
    Unstarted,
    Stream(Py<PyAny>),
    Read,
    Done,
}

#[pyclass(module = "requests._requests_rust")]
struct NativeContentIterator {
    subject: Py<PyAny>,
    chunk_size: Py<PyAny>,
    source: ContentSource,
}

impl NativeContentIterator {
    fn mark_consumed(&mut self, py: Python<'_>) -> PyResult<()> {
        self.subject.bind(py).setattr("_content_consumed", true)?;
        self.source = ContentSource::Done;
        Ok(())
    }

    fn start(&mut self, py: Python<'_>) -> PyResult<()> {
        let subject = self.subject.bind(py);
        let raw = subject.getattr("raw")?;
        if raw.hasattr("stream")? {
            let stream = subject.getattr("raw")?.getattr("stream")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("decode_content", true)?;
            let iterator = stream
                .call((self.chunk_size.bind(py),), Some(&kwargs))?
                .try_iter()?;
            self.source = ContentSource::Stream(iterator.into_any().unbind());
        } else {
            self.source = ContentSource::Read;
        }
        Ok(())
    }

    fn wrap_stream_error(&self, py: Python<'_>, error: PyErr) -> PyErr {
        match wrap_stream_error(py, error) {
            Ok(error) | Err(error) => error,
        }
    }
}

#[pymethods]
impl NativeContentIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if matches!(self.source, ContentSource::Unstarted) {
            self.start(py)?;
        }
        match &self.source {
            ContentSource::Stream(iterator) => match iterator.bind(py).call_method0("__next__") {
                Ok(chunk) => Ok(Some(chunk.unbind())),
                Err(error) if error.is_instance_of::<PyStopIteration>(py) => {
                    self.mark_consumed(py)?;
                    Ok(None)
                }
                Err(error) => Err(self.wrap_stream_error(py, error)),
            },
            ContentSource::Read => {
                let chunk = self
                    .subject
                    .bind(py)
                    .getattr("raw")?
                    .call_method1("read", (self.chunk_size.bind(py),))?;
                if !chunk.is_truthy()? {
                    self.mark_consumed(py)?;
                    Ok(None)
                } else {
                    Ok(Some(chunk.unbind()))
                }
            }
            ContentSource::Done => Ok(None),
            ContentSource::Unstarted => unreachable!(),
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        let source = std::mem::replace(&mut self.source, ContentSource::Done);
        if let ContentSource::Stream(iterator) = source {
            iterator.bind(py).call_method0("close")?;
        }
        Ok(())
    }
}

fn name_error_with_context(py: Python<'_>, name: &str, original: &PyErr) -> PyErr {
    let error = PyNameError::new_err(format!("name '{name}' is not defined"));
    error.set_context(py, Some(original.clone_ref(py)));
    error
}

fn wrap_stream_error(py: Python<'_>, original: PyErr) -> PyResult<PyErr> {
    let models = response_state(py)?.models.bind(py);
    let mappings = [
        ("ProtocolError", "ChunkedEncodingError"),
        ("DecodeError", "ContentDecodingError"),
        ("ReadTimeoutError", "ConnectionError"),
        ("SSLError", "RequestsSSLError"),
    ];
    for (source_name, target_name) in mappings {
        let Some(source) = models.dict().get_item(source_name)? else {
            return Ok(name_error_with_context(py, source_name, &original));
        };
        match original.value(py).is_instance(&source) {
            Ok(false) => continue,
            Ok(true) => {
                let Some(target) = models.dict().get_item(target_name)? else {
                    return Ok(name_error_with_context(py, target_name, &original));
                };
                let wrapped = match target.call1((original.value(py),)) {
                    Ok(value) => PyErr::from_value(value),
                    Err(error) => error,
                };
                wrapped.set_context(py, Some(original));
                return Ok(wrapped);
            }
            Err(error) => {
                error.set_context(py, Some(original));
                return Ok(error);
            }
        }
    }
    Ok(original)
}

fn native_iter_content(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
) -> PyResult<Py<PyAny>> {
    let consumed = subject.getattr("_content_consumed")?.is_truthy()?;
    let content = subject.getattr("_content")?;
    if consumed && content.is_instance(&py.get_type::<PyBool>())? {
        let exception = response_state(py)?
            .models
            .bind(py)
            .getattr("StreamConsumedError")?;
        return Err(PyErr::from_value(exception.call0()?));
    }
    if !chunk_size_is_valid(py, chunk_size) {
        return Err(invalid_chunk_size(chunk_size));
    }

    let chunks = if consumed {
        response_state(py)?
            .models
            .bind(py)
            .getattr("iter_slices")?
            .call1((content, chunk_size))?
            .unbind()
    } else {
        Py::new(
            py,
            NativeContentIterator {
                subject: subject.clone().unbind(),
                chunk_size: chunk_size.clone().unbind(),
                source: ContentSource::Unstarted,
            },
        )?
        .into_any()
    };

    if decode_unicode {
        Ok(response_state(py)?
            .models
            .bind(py)
            .getattr("stream_decode_response_unicode")?
            .call1((chunks.bind(py), subject))?
            .unbind())
    } else {
        Ok(chunks)
    }
}

fn iter_content_dispatch(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
) -> PyResult<Py<PyAny>> {
    const GLOBALS: &[&str] = &[
        "iter_slices",
        "stream_decode_response_unicode",
        "ProtocolError",
        "DecodeError",
        "ReadTimeoutError",
        "SSLError",
        "ChunkedEncodingError",
        "ContentDecodingError",
        "ConnectionError",
        "RequestsSSLError",
        "StreamConsumedError",
        "bool",
        "int",
        "hasattr",
        "cast",
        "bytes",
        "type",
        "isinstance",
        "TypeError",
    ];
    if !exact_operation(py, subject, "iter_content", GLOBALS)? {
        fallback_iter_content(py, subject, chunk_size, decode_unicode)
    } else {
        native_iter_content(py, subject, chunk_size, decode_unicode)
    }
}

#[pyfunction]
fn _response_iter_content_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
) -> PyResult<Py<PyAny>> {
    iter_content_dispatch(py, subject, chunk_size, decode_unicode)
}

fn content_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    const GLOBALS: &[&str] = &["CONTENT_CHUNK_SIZE", "RuntimeError"];
    if !exact_operation(py, subject, "content", GLOBALS)?
        || !exact_operation(py, subject, "iter_content", &[])?
    {
        return Ok(subject.getattr("content")?.unbind());
    }

    let content = subject.getattr("_content")?;
    if content.is(false.into_pyobject(py)?) {
        if subject.getattr("_content_consumed")?.is_truthy()? {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "The content for this response was already consumed",
            ));
        }
        let status_zero = subject.getattr("status_code")?.eq(0)?;
        if status_zero || subject.getattr("raw")?.is_none() {
            subject.setattr("_content", py.None())?;
        } else {
            let chunk_size = response_state(py)?
                .models
                .bind(py)
                .getattr("CONTENT_CHUNK_SIZE")?;
            let iterator = native_iter_content(py, subject, &chunk_size, false)?;
            let joined = PyBytes::new(py, b"").call_method1("join", (iterator,))?;
            subject.setattr("_content", joined)?;
        }
    }
    subject.setattr("_content_consumed", true)?;
    Ok(subject.getattr("_content")?.unbind())
}

#[pyfunction]
fn _response_content_trial(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    content_dispatch(py, subject)
}

fn apparent_encoding_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "apparent_encoding", &["chardet"])? {
        return Ok(subject.getattr("apparent_encoding")?.unbind());
    }
    let detector = response_state(py)?.models.bind(py).getattr("chardet")?;
    if detector.is_none() {
        return Ok("utf-8".into_pyobject(py)?.into_any().unbind());
    }
    let content = content_dispatch(py, subject)?;
    Ok(detector
        .call_method1("detect", (content,))?
        .get_item("encoding")?
        .unbind())
}

#[pyfunction]
fn _response_apparent_encoding_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    apparent_encoding_dispatch(py, subject)
}

fn text_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "text", &["str", "LookupError", "TypeError"])? {
        return Ok(subject.getattr("text")?.unbind());
    }

    let mut encoding = subject.getattr("encoding")?.unbind();
    if !content_dispatch(py, subject)?.bind(py).is_truthy()? {
        return Ok("".into_pyobject(py)?.into_any().unbind());
    }
    if subject.getattr("encoding")?.is_none() {
        encoding = apparent_encoding_dispatch(py, subject)?;
    }

    let string_type = py.get_type::<PyString>();
    let content = content_dispatch(py, subject)?;
    let selected_encoding = if encoding.bind(py).is_truthy()? {
        encoding.clone_ref(py)
    } else {
        "utf-8".into_pyobject(py)?.into_any().unbind()
    };
    let kwargs = PyDict::new(py);
    kwargs.set_item("errors", "replace")?;
    match string_type.call(
        (content.bind(py), selected_encoding.bind(py)),
        Some(&kwargs),
    ) {
        Ok(value) => Ok(value.unbind()),
        Err(error)
            if error.is_instance_of::<PyLookupError>(py)
                || error.is_instance_of::<PyTypeError>(py) =>
        {
            let content = content_dispatch(py, subject)?;
            Ok(string_type
                .call((content.bind(py),), Some(&kwargs))?
                .unbind())
        }
        Err(error) => Err(error),
    }
}

#[pyfunction]
fn _response_text_trial(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    text_dispatch(py, subject)
}

fn wrap_json_error(
    py: Python<'_>,
    models: &Bound<'_, PyModule>,
    original: PyErr,
) -> PyResult<PyErr> {
    if !original
        .value(py)
        .is_instance(&models.getattr("JSONDecodeError")?)?
    {
        return Ok(original);
    }
    let value = original.value(py);
    let wrapped = PyErr::from_value(models.getattr("RequestsJSONDecodeError")?.call1((
        value.getattr("msg")?,
        value.getattr("doc")?,
        value.getattr("pos")?,
    ))?);
    wrapped.set_context(py, Some(original));
    Ok(wrapped)
}

fn json_loads(
    py: Python<'_>,
    models: &Bound<'_, PyModule>,
    value: &Bound<'_, PyAny>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    match models
        .getattr("complexjson")?
        .getattr("loads")?
        .call((value,), Some(kwargs))
    {
        Ok(result) => Ok(result.unbind()),
        Err(error) => Err(wrap_json_error(py, models, error)?),
    }
}

fn json_dispatch(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    const GLOBALS: &[&str] = &[
        "complexjson",
        "guess_json_utf",
        "JSONDecodeError",
        "RequestsJSONDecodeError",
        "UnicodeDecodeError",
        "len",
    ];
    if !exact_operation(py, subject, "json", GLOBALS)?
        || !exact_operation(py, subject, "content", &[])?
        || !exact_operation(py, subject, "text", &[])?
    {
        return Ok(subject.call_method("json", (), Some(kwargs))?.unbind());
    }

    let models = response_state(py)?.models.bind(py);
    let encoding = subject.getattr("encoding")?;
    let content = content_dispatch(py, subject)?;
    if !encoding.is_truthy()? && content.bind(py).is_truthy()? && content.bind(py).len()? > 3 {
        let guessed = models
            .getattr("guess_json_utf")?
            .call1((content.bind(py),))?;
        if !guessed.is_none() {
            match content.bind(py).call_method1("decode", (&guessed,)) {
                Ok(decoded) => return json_loads(py, models, &decoded, kwargs),
                Err(error) if error.is_instance_of::<PyUnicodeDecodeError>(py) => {}
                Err(error) => return Err(error),
            }
        }
    }
    let text = text_dispatch(py, subject)?;
    json_loads(py, models, text.bind(py), kwargs)
}

#[pyfunction]
fn _response_json_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    json_dispatch(py, subject, kwargs)
}

fn in_status_range(
    py: Python<'_>,
    status: &Bound<'_, PyAny>,
    lower: i32,
    upper: i32,
) -> PyResult<bool> {
    let lower = lower.into_pyobject(py)?;
    if !lower.rich_compare(status, CompareOp::Le)?.is_truthy()? {
        return Ok(false);
    }
    let upper = upper.into_pyobject(py)?;
    status.rich_compare(upper, CompareOp::Lt)?.is_truthy()
}

fn reason_for_status<'py>(
    py: Python<'py>,
    reason: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if !reason.is_instance(&py.get_type::<PyBytes>())? {
        return Ok(reason.clone());
    }
    match reason.call_method1("decode", ("utf-8",)) {
        Ok(decoded) => Ok(decoded),
        Err(error) if error.is_instance_of::<PyUnicodeDecodeError>(py) => {
            reason.call_method1("decode", ("iso-8859-1",))
        }
        Err(error) => Err(error),
    }
}

fn raise_for_status_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if !exact_operation(
        py,
        subject,
        "raise_for_status",
        &["HTTPError", "isinstance", "bytes", "UnicodeDecodeError"],
    )? {
        return Ok(subject.call_method0("raise_for_status")?.unbind());
    }

    let status = subject.getattr("status_code")?;
    let reason_value = subject.getattr("reason")?;
    let reason = reason_for_status(py, &reason_value)?;
    let template = if in_status_range(py, &status, 400, 500)? {
        Some("{} Client Error: {} for url: {}")
    } else if in_status_range(py, &status, 500, 600)? {
        Some("{} Server Error: {} for url: {}")
    } else {
        None
    };
    let Some(template) = template else {
        return Ok(py.None());
    };
    let message = template
        .into_pyobject(py)?
        .call_method1("format", (&status, &reason, subject.getattr("url")?))?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("response", subject)?;
    let exception = response_state(py)?
        .models
        .bind(py)
        .getattr("HTTPError")?
        .call((message,), Some(&kwargs))?;
    Err(PyErr::from_value(exception))
}

fn ok_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !exact_operation(py, subject, "ok", &["HTTPError"])? {
        return subject.getattr("ok")?.extract();
    }
    match raise_for_status_dispatch(py, subject) {
        Ok(_) => Ok(true),
        Err(error)
            if error
                .value(py)
                .is_instance(&response_state(py)?.models.bind(py).getattr("HTTPError")?)? =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn fallback_metadata(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<Py<PyAny>> {
    let builtins = PyModule::import(py, "builtins")?;
    match operation {
        "repr" => Ok(builtins.getattr("repr")?.call1((subject,))?.unbind()),
        "bool" => Ok(builtins.getattr("bool")?.call1((subject,))?.unbind()),
        "ok" | "is_redirect" | "is_permanent_redirect" | "next" | "history" => {
            Ok(subject.getattr(operation)?.unbind())
        }
        "raise_for_status" => Ok(subject.call_method0("raise_for_status")?.unbind()),
        _ => Err(PyAssertionError::new_err(operation.to_owned())),
    }
}

fn metadata_dispatch(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<Py<PyAny>> {
    match operation {
        "repr" if exact_operation(py, subject, "__repr__", &[])? => Ok("<Response [{}]>"
            .into_pyobject(py)?
            .call_method1("format", (subject.getattr("status_code")?,))?
            .unbind()),
        "bool"
            if exact_operation(py, subject, "__bool__", &[])?
                && exact_operation(py, subject, "ok", &["HTTPError"])? =>
        {
            Ok(ok_dispatch(py, subject)?
                .into_pyobject(py)?
                .to_owned()
                .into_any()
                .unbind())
        }
        "ok" if exact_operation(py, subject, "ok", &["HTTPError"])? => {
            Ok(ok_dispatch(py, subject)?
                .into_pyobject(py)?
                .to_owned()
                .into_any()
                .unbind())
        }
        "is_redirect" if exact_operation(py, subject, "is_redirect", &["REDIRECT_STATI"])? => {
            let headers = subject.getattr("headers")?;
            let redirected = headers.contains("location")?
                && response_state(py)?
                    .models
                    .bind(py)
                    .getattr("REDIRECT_STATI")?
                    .contains(subject.getattr("status_code")?)?;
            Ok(redirected.into_pyobject(py)?.to_owned().into_any().unbind())
        }
        "is_permanent_redirect"
            if exact_operation(py, subject, "is_permanent_redirect", &["codes"])? =>
        {
            let headers = subject.getattr("headers")?;
            let permanent = if !headers.contains("location")? {
                false
            } else {
                let codes = response_state(py)?.models.bind(py).getattr("codes")?;
                let choices = PyTuple::new(
                    py,
                    [
                        codes.getattr("moved_permanently")?,
                        codes.getattr("permanent_redirect")?,
                    ],
                )?;
                choices.contains(subject.getattr("status_code")?)?
            };
            Ok(permanent.into_pyobject(py)?.to_owned().into_any().unbind())
        }
        "next" if exact_operation(py, subject, "next", &[])? => {
            Ok(subject.getattr("_next")?.unbind())
        }
        "history" => Ok(subject.getattr("history")?.unbind()),
        "raise_for_status"
            if exact_operation(
                py,
                subject,
                "raise_for_status",
                &["HTTPError", "isinstance", "bytes", "UnicodeDecodeError"],
            )? =>
        {
            raise_for_status_dispatch(py, subject)
        }
        _ => fallback_metadata(py, subject, operation),
    }
}

#[pyfunction]
fn _response_metadata_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
) -> PyResult<Py<PyAny>> {
    metadata_dispatch(py, subject, operation)
}

const RESPONSE_ATTRS: &[&str] = &[
    "_content",
    "status_code",
    "headers",
    "url",
    "history",
    "encoding",
    "reason",
    "cookies",
    "elapsed",
    "request",
];

fn response_attrs_are_current(subject: &Bound<'_, PyAny>) -> PyResult<bool> {
    let Some(attrs) = raw_direct_type_entry(&subject.get_type(), "__attrs__")? else {
        return Ok(false);
    };
    Ok(attrs.extract::<Vec<String>>()?
        == RESPONSE_ATTRS
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>())
}

fn pickle_get_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "__getstate__", &["getattr"])?
        || !response_attrs_are_current(subject)?
    {
        return Ok(subject.call_method0("__getstate__")?.unbind());
    }
    if !subject.getattr("_content_consumed")?.is_truthy()? {
        let _ = content_dispatch(py, subject)?;
    }
    let result = PyDict::new(py);
    let getattr = PyModule::import(py, "builtins")?.getattr("getattr")?;
    for attr in subject.getattr("__attrs__")?.try_iter()? {
        let attr = attr?;
        let value = getattr.call1((subject, &attr, py.None()))?;
        result.set_item(attr, value)?;
    }
    Ok(result.into_any().unbind())
}

fn pickle_set_dispatch(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    state: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "__setstate__", &["setattr"])? {
        return Ok(subject.call_method1("__setstate__", (state,))?.unbind());
    }
    let setattr = PyModule::import(py, "builtins")?.getattr("setattr")?;
    for item in state.call_method0("items")?.try_iter()? {
        let item = item?;
        let pair = item.cast::<PyTuple>()?;
        setattr.call1((subject, pair.get_item(0)?, pair.get_item(1)?))?;
    }
    setattr.call1((subject, "_content_consumed", true))?;
    setattr.call1((subject, "raw", py.None()))?;
    Ok(py.None())
}

#[pyfunction]
fn _response_pickle_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    state: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    match operation {
        "get" => pickle_get_dispatch(py, subject),
        "set" => pickle_set_dispatch(py, subject, state),
        _ => Err(PyAssertionError::new_err(operation.to_owned())),
    }
}

fn close_dispatch(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "close", &["getattr"])? {
        return Ok(subject.call_method0("close")?.unbind());
    }
    if !subject.getattr("_content_consumed")?.is_truthy()? {
        subject.getattr("raw")?.call_method0("close")?;
    }
    let raw = subject.getattr("raw")?;
    let release = match raw.getattr("release_conn") {
        Ok(release) => Some(release),
        Err(error) if error.is_instance_of::<PyAttributeError>(py) => None,
        Err(error) => return Err(error),
    };
    if let Some(release) = release
        && !release.is_none()
    {
        release.call0()?;
    }
    Ok(py.None())
}

#[pyfunction]
fn _response_close_trial(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    close_dispatch(py, subject)
}

#[pyfunction]
fn _response_drop_trial(
    py: Python<'_>,
    raw: &Bound<'_, PyAny>,
    operation: &str,
    chunk_size: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let subject = response_state(py)?.response_type.bind(py).call0()?;
    subject.setattr("status_code", 200)?;
    subject.setattr("raw", raw)?;

    let mut iterator = None;
    match operation {
        "untouched" => {}
        "partial" | "failed" => {
            let current = iter_content_dispatch(py, &subject, chunk_size, false)?;
            PyModule::import(py, "builtins")?
                .getattr("next")?
                .call1((current.bind(py),))?;
            iterator = Some(current);
        }
        "exhausted" => {
            let current = iter_content_dispatch(py, &subject, chunk_size, false)?;
            PyModule::import(py, "builtins")?
                .getattr("list")?
                .call1((current.bind(py),))?;
            iterator = Some(current);
        }
        "cached" => {
            let _ = content_dispatch(py, &subject)?;
        }
        _ => return Err(PyAssertionError::new_err(operation.to_owned())),
    }

    let result = PyDict::new(py);
    result.set_item("raw_is_original", subject.getattr("raw")?.is(raw))?;
    result.set_item("state", _response_fields_snapshot(py, &subject)?)?;
    drop(iterator);
    Ok(result.into_any().unbind())
}

#[pyclass(module = "requests._requests_rust")]
struct NativeLinesIterator {
    subject: Py<PyAny>,
    chunk_size: Py<PyAny>,
    decode_unicode: bool,
    delimiter: Py<PyAny>,
    chunks: Option<Py<PyAny>>,
    pending: Option<Py<PyAny>>,
    ready: Vec<Py<PyAny>>,
    ready_index: usize,
    done: bool,
}

impl NativeLinesIterator {
    fn ensure_chunks(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.chunks.is_none() {
            self.chunks = Some(iter_content_dispatch(
                py,
                self.subject.bind(py),
                self.chunk_size.bind(py),
                self.decode_unicode,
            )?);
        }
        Ok(())
    }

    fn take_ready(&mut self, py: Python<'_>) -> Option<Py<PyAny>> {
        if self.ready_index >= self.ready.len() {
            return None;
        }
        let value = self.ready[self.ready_index].clone_ref(py);
        self.ready_index += 1;
        Some(value)
    }

    fn load_lines(&mut self, py: Python<'_>, chunk: Bound<'_, PyAny>) -> PyResult<()> {
        let chunk = if let Some(pending) = self.pending.take() {
            pending.bind(py).call_method1("__add__", (chunk,))?
        } else {
            chunk
        };
        let delimiter = self.delimiter.bind(py);
        let lines = if delimiter.is_truthy()? {
            chunk.call_method1("split", (delimiter,))?
        } else {
            chunk.call_method0("splitlines")?
        };
        let lines = lines.cast::<PyList>()?;
        let mut values = lines.iter().map(Bound::unbind).collect::<Vec<Py<PyAny>>>();
        if let Some(last) = values.last()
            && last.bind(py).is_truthy()?
            && chunk.is_truthy()?
        {
            let last_tail = last.bind(py).get_item(-1)?;
            let chunk_tail = chunk.get_item(-1)?;
            if last_tail.eq(chunk_tail)? {
                self.pending = values.pop();
            }
        }
        self.ready = values;
        self.ready_index = 0;
        Ok(())
    }
}

#[pymethods]
impl NativeLinesIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        loop {
            if let Some(value) = self.take_ready(py) {
                return Ok(Some(value));
            }
            if self.done {
                return Ok(self.pending.take());
            }
            self.ensure_chunks(py)?;
            match self
                .chunks
                .as_ref()
                .expect("line source is initialized")
                .bind(py)
                .call_method0("__next__")
            {
                Ok(chunk) => self.load_lines(py, chunk)?,
                Err(error) if error.is_instance_of::<PyStopIteration>(py) => {
                    self.done = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        if let Some(chunks) = self.chunks.take()
            && chunks.bind(py).hasattr("close")?
        {
            chunks.bind(py).call_method0("close")?;
        }
        self.done = true;
        self.pending = None;
        self.ready.clear();
        Ok(())
    }
}

#[pyfunction]
fn _response_iter_lines_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    chunk_size: &Bound<'_, PyAny>,
    decode_unicode: bool,
    delimiter: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    if !exact_operation(py, subject, "iter_lines", &["cast"])? {
        return fallback_iter_lines(py, subject, chunk_size, decode_unicode, delimiter);
    }
    Ok(Py::new(
        py,
        NativeLinesIterator {
            subject: subject.clone().unbind(),
            chunk_size: chunk_size.clone().unbind(),
            decode_unicode,
            delimiter: delimiter.clone().unbind(),
            chunks: None,
            pending: None,
            ready: Vec::new(),
            ready_index: 0,
            done: false,
        },
    )?
    .into_any())
}

fn value_record(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let value_type = value.get_type();
    let type_row = PyList::new(
        py,
        [
            value_type.getattr("__module__")?,
            value_type.getattr("__qualname__")?,
        ],
    )?;
    let payload = if value.is_none() {
        py.None()
    } else if value.is_instance(&py.get_type::<PyBytes>())? {
        let bytes = py.get_type::<PyBytes>().call1((value,))?;
        let row = PyList::new(
            py,
            [
                "bytes".into_pyobject(py)?.into_any(),
                bytes.call_method0("hex")?,
            ],
        )?;
        row.into_any().unbind()
    } else if value.is_instance(&py.get_type::<PyString>())? {
        let row = PyList::new(py, ["str".into_pyobject(py)?.into_any(), value.clone()])?;
        row.into_any().unbind()
    } else if value.is_instance(&py.get_type::<PyBool>())?
        || value.is_instance(&py.get_type::<PyInt>())?
    {
        value.clone().unbind()
    } else {
        let row = PyList::new(
            py,
            [
                "opaque".into_pyobject(py)?.into_any(),
                value_type.getattr("__module__")?,
                value_type.getattr("__qualname__")?,
            ],
        )?;
        row.into_any().unbind()
    };
    let record = PyDict::new(py);
    record.set_item("type", type_row)?;
    record.set_item("payload", payload)?;
    Ok(record.into_any().unbind())
}

#[pyfunction]
fn _response_fields_snapshot(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let content = subject.getattr("_content")?;
    let snapshot = PyDict::new(py);
    snapshot.set_item("content", value_record(py, &content)?)?;
    snapshot.set_item("content_is_false", content.is(false.into_pyobject(py)?))?;
    snapshot.set_item("content_consumed", subject.getattr("_content_consumed")?)?;
    snapshot.set_item("raw_is_none", subject.getattr("raw")?.is_none())?;
    Ok(snapshot.into_any().unbind())
}

#[pyfunction]
fn _response_disposition_trial(py: Python<'_>, events: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let mut disposition = ResponseDispositionState::default();
    let mut states = vec![disposition.state().as_str()];
    for event in events.try_iter()? {
        let event = event?.extract::<String>()?;
        let event = match event.as_str() {
            "partial" => ResponseEvent::Partial,
            "clean-eof" => ResponseEvent::CleanEof,
            "close" => ResponseEvent::Close,
            "drop" => ResponseEvent::Drop,
            "read-error" => ResponseEvent::ReadError,
            "decode-error" => ResponseEvent::DecodeError,
            "protocol-error" => ResponseEvent::ProtocolError,
            "cancel" => ResponseEvent::Cancel,
            "action-disconnect" => ResponseEvent::ActionDisconnect,
            "reply-disconnect" => ResponseEvent::ReplyDisconnect,
            "python-exact" => ResponseEvent::PythonExact,
            _ => {
                return Err(PyTypeError::new_err(format!(
                    "unknown response event: {event}"
                )));
            }
        };
        states.push(disposition.apply(event).as_str());
    }
    let result = PyDict::new(py);
    result.set_item("states", states)?;
    result.set_item("final", disposition.state().as_str())?;
    result.set_item(
        "decision",
        disposition.decision().map(|decision| decision.as_str()),
    )?;
    result.set_item("decision_count", disposition.decision_count())?;
    result.set_item("native_lease", disposition.native_lease())?;
    result.set_item("implicit_python_callbacks", 0)?;
    Ok(result.into_any().unbind())
}

#[derive(Clone, Copy, Debug)]
enum ResponseAction {
    Read { size: usize },
}

#[derive(Debug)]
enum ResponseReply {
    Chunk(Vec<u8>),
    End,
    Failed,
}

impl WorkerPayload for ResponseAction {}
impl WorkerPayload for ResponseReply {}

struct OriginResponseOwner {
    subject: Py<PyAny>,
}

#[derive(Clone, Copy)]
enum LifecyclePhase {
    BeforePoll,
    QueuedBeforeDequeue,
    ReplyObserved,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseWorkerFailure {
    None = 0,
    ActionReceiver = 1,
    ReplySender = 2,
    Handler = 3,
}

impl ResponseWorkerFailure {
    fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::ActionReceiver,
            2 => Self::ReplySender,
            3 => Self::Handler,
            _ => Self::None,
        }
    }
}

const RESPONSE_WORKER_STARTING: u8 = 0;
const RESPONSE_WORKER_AWAITING_REPLY: u8 = 1;
const RESPONSE_WORKER_REPLY_OBSERVED: u8 = 2;
const RESPONSE_WORKER_DROPPED: u8 = 3;
const RESPONSE_PHASE_WAIT: Duration = Duration::from_millis(500);

struct ResponseWorkerDropGuard {
    phase: Arc<AtomicU8>,
}

impl Drop for ResponseWorkerDropGuard {
    fn drop(&mut self) {
        self.phase.store(RESPONSE_WORKER_DROPPED, Ordering::Release);
    }
}

fn wait_for_response_worker_phase(
    py: Python<'_>,
    phase: &AtomicU8,
    expected: u8,
    label: &str,
) -> PyResult<()> {
    let deadline = Instant::now() + RESPONSE_PHASE_WAIT;
    while phase.load(Ordering::Acquire) != expected {
        if Instant::now() >= deadline {
            return Err(PyRuntimeError::new_err(format!(
                "response worker did not reach {label}"
            )));
        }
        py.detach(|| std::thread::sleep(Duration::from_millis(1)));
    }
    Ok(())
}

fn require_response_worker_dropped(phase: &AtomicU8) -> PyResult<()> {
    if phase.load(Ordering::Acquire) == RESPONSE_WORKER_DROPPED {
        Ok(())
    } else {
        Err(PyRuntimeError::new_err(
            "response worker was not dropped before its origin owner",
        ))
    }
}

fn store_response_handler_error(slot: &mut Option<PyErr>, error: PyErr) -> ResponseReply {
    if slot.is_none() {
        *slot = Some(error);
    }
    ResponseReply::Failed
}

fn response_read_action(
    py: Python<'_>,
    owner: &OriginResponseOwner,
    size: usize,
) -> PyResult<ResponseReply> {
    let subject = owner.subject.bind(py);
    let raw = subject.getattr("raw")?;
    let chunk = if raw.hasattr("stream")? {
        let stream = subject.getattr("raw")?.getattr("stream")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("decode_content", true)?;
        let iterator = stream.call((size,), Some(&kwargs))?.try_iter()?;
        match iterator.into_any().call_method0("__next__") {
            Ok(chunk) => chunk,
            Err(error) if error.is_instance_of::<PyStopIteration>(py) => {
                return Ok(ResponseReply::End);
            }
            Err(error) => return Err(error),
        }
    } else {
        subject.getattr("raw")?.call_method1("read", (size,))?
    };
    if !chunk.is_truthy()? {
        Ok(ResponseReply::End)
    } else {
        Ok(ResponseReply::Chunk(
            chunk.cast::<PyBytes>()?.as_bytes().to_vec(),
        ))
    }
}

fn response_action(
    py: Python<'_>,
    action: ResponseAction,
    owner: &OriginResponseOwner,
    handler_error: &mut Option<PyErr>,
    raw_actions: &Cell<u8>,
) -> ResponseReply {
    raw_actions.set(raw_actions.get().saturating_add(1));
    let result = match action {
        ResponseAction::Read { size } => response_read_action(py, owner, size),
    };
    match result {
        Ok(reply) => reply,
        Err(error) => store_response_handler_error(handler_error, error),
    }
}

fn response_worker_failure(error: BridgeClosed) -> ResponseWorkerFailure {
    match error {
        BridgeClosed::ActionReceiver => ResponseWorkerFailure::ActionReceiver,
        BridgeClosed::ReplySender => ResponseWorkerFailure::ReplySender,
    }
}

fn response_lifecycle(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
    phase: LifecyclePhase,
    audit: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let owner = OriginResponseOwner {
        subject: subject.clone().unbind(),
    };
    let worker_phase = Arc::new(AtomicU8::new(RESPONSE_WORKER_STARTING));
    let worker_phase_guard = ResponseWorkerDropGuard {
        phase: Arc::clone(&worker_phase),
    };
    let worker_poll_phase = Arc::clone(&worker_phase);
    let worker_signal_phase = Arc::clone(&worker_phase);
    let queued = Arc::new(AtomicU8::new(0));
    let worker_queued = Arc::clone(&queued);
    let replies_observed = Arc::new(AtomicU8::new(0));
    let worker_replies_observed = Arc::clone(&replies_observed);
    let failure = Arc::new(AtomicU8::new(ResponseWorkerFailure::None as u8));
    let worker_failure = Arc::clone(&failure);
    let execute = Cell::new(0_u8);
    let raw_actions = Cell::new(0_u8);
    let mut handler_error = None;

    let result = run_with_actions_and_signal_checker(
        py,
        move |actions| async move {
            let _worker_phase_guard = worker_phase_guard;
            match phase {
                LifecyclePhase::BeforePoll => future::pending::<()>().await,
                LifecyclePhase::QueuedBeforeDequeue | LifecyclePhase::ReplyObserved => {
                    let receive_reply = match actions.enqueue(ResponseAction::Read { size: 1 }) {
                        Ok(receive_reply) => receive_reply,
                        Err(error) => {
                            worker_failure
                                .store(response_worker_failure(error) as u8, Ordering::Release);
                            return;
                        }
                    };
                    worker_queued.fetch_add(1, Ordering::AcqRel);
                    worker_poll_phase.store(RESPONSE_WORKER_AWAITING_REPLY, Ordering::Release);
                    if matches!(phase, LifecyclePhase::QueuedBeforeDequeue) {
                        future::pending::<()>().await;
                    }
                    match receive_reply.await {
                        Ok(ResponseReply::Chunk(chunk)) => {
                            let _chunk_length = chunk.len();
                        }
                        Ok(ResponseReply::End) => {}
                        Ok(ResponseReply::Failed) => {
                            worker_failure
                                .store(ResponseWorkerFailure::Handler as u8, Ordering::Release);
                        }
                        Err(_) => {
                            worker_failure
                                .store(ResponseWorkerFailure::ReplySender as u8, Ordering::Release);
                            return;
                        }
                    }
                    worker_replies_observed.fetch_add(1, Ordering::AcqRel);
                    worker_poll_phase.store(RESPONSE_WORKER_REPLY_OBSERVED, Ordering::Release);
                    future::pending::<()>().await;
                }
            }
        },
        |py, action| {
            execute.set(execute.get().saturating_add(1));
            response_action(py, action, &owner, &mut handler_error, &raw_actions)
        },
        |py| match phase {
            LifecyclePhase::BeforePoll => Err(PyErr::from_value(error.bind(py).clone())),
            LifecyclePhase::QueuedBeforeDequeue => {
                wait_for_response_worker_phase(
                    py,
                    &worker_signal_phase,
                    RESPONSE_WORKER_AWAITING_REPLY,
                    "queued-before-dequeue",
                )?;
                Err(PyErr::from_value(error.bind(py).clone()))
            }
            LifecyclePhase::ReplyObserved => {
                if execute.get() == 0 {
                    return Ok(());
                }
                wait_for_response_worker_phase(
                    py,
                    &worker_signal_phase,
                    RESPONSE_WORKER_REPLY_OBSERVED,
                    "reply-observed",
                )?;
                Err(PyErr::from_value(error.bind(py).clone()))
            }
        },
    );

    require_response_worker_dropped(&worker_phase)?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    let queued = queued.load(Ordering::Acquire);
    let replies_observed = replies_observed.load(Ordering::Acquire);
    let expected = match phase {
        LifecyclePhase::BeforePoll => (0, 0, 0, 0),
        LifecyclePhase::QueuedBeforeDequeue => (1, 0, 0, 0),
        LifecyclePhase::ReplyObserved => (1, 1, 1, 1),
    };
    if (queued, execute.get(), replies_observed, raw_actions.get()) != expected {
        return Err(PyRuntimeError::new_err(format!(
            "response lifecycle mismatch: queued={queued}, execute={}, replies={replies_observed}, raw_actions={}",
            execute.get(),
            raw_actions.get(),
        )));
    }
    let failure = ResponseWorkerFailure::from_raw(failure.load(Ordering::Acquire));
    if failure != ResponseWorkerFailure::None {
        return Err(PyRuntimeError::new_err(format!(
            "response worker failed: {failure:?}"
        )));
    }
    audit.set_item("queued", queued)?;
    audit.set_item("execute", execute.get())?;
    audit.set_item("reply_observed", replies_observed != 0)?;
    audit.set_item(
        "worker_dropped",
        worker_phase.load(Ordering::Acquire) == RESPONSE_WORKER_DROPPED,
    )?;
    audit.set_item("raw_actions", raw_actions.get())?;
    drop(owner);
    result
}

#[pyfunction]
fn _response_lifecycle_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
    phase: &str,
    audit: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let phase = match phase {
        "before-poll" => LifecyclePhase::BeforePoll,
        "queued-before-dequeue" => LifecyclePhase::QueuedBeforeDequeue,
        "reply-observed" => LifecyclePhase::ReplyObserved,
        _ => return Err(PyAssertionError::new_err(phase.to_owned())),
    };
    response_lifecycle(py, subject, error, phase, audit)
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let _ = response_state(module.py())?;
    module.add_class::<NativeContentIterator>()?;
    module.add_class::<NativeLinesIterator>()?;
    module.add_function(wrap_pyfunction!(_response_content_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_iter_content_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_iter_lines_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_text_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_apparent_encoding_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_json_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_metadata_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_pickle_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_close_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_drop_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_fields_snapshot, module)?)?;
    module.add_function(wrap_pyfunction!(_response_disposition_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_lifecycle_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use pyo3::PyAny;

    use super::{
        OriginResponseOwner, ResponseAction, ResponseReply, ResponseWorkerFailure, WorkerPayload,
    };

    fn assert_worker_payload<T: WorkerPayload>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    #[test]
    fn response_payloads_and_origin_owner_are_explicit() {
        assert_worker_payload::<ResponseAction>();
        assert_worker_payload::<ResponseReply>();

        let action = ResponseAction::Read { size: 7 };
        assert!(matches!(action, ResponseAction::Read { size: 7 }));
        let reply = ResponseReply::Chunk(vec![1, 2, 3]);
        assert!(matches!(reply, ResponseReply::Chunk(chunk) if chunk == [1, 2, 3]));
        assert_eq!(
            ResponseWorkerFailure::from_raw(ResponseWorkerFailure::ReplySender as u8),
            ResponseWorkerFailure::ReplySender
        );

        let _origin_owner_must_not_be_a_worker_payload =
            <OriginResponseOwner as AmbiguousIfWorkerPayload<_>>::marker;
        let _python_handle_must_not_be_a_worker_payload =
            <pyo3::Py<PyAny> as AmbiguousIfWorkerPayload<_>>::marker;
    }
}

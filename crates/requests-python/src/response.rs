use std::collections::HashMap;

use pyo3::exceptions::{PyNameError, PyStopIteration, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBool, PyBytes, PyDict, PyDictMethods, PyFunction, PyInt, PyList,
    PyListMethods, PyModule, PyString, PyType, PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::{ResponseDispositionState, ResponseEvent};

use crate::models::canonical_code;

#[derive(Clone, Copy)]
enum DescriptorKind {
    Function,
    Property,
}

struct CanonicalDescriptor {
    kind: DescriptorKind,
    code: Py<PyAny>,
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
    "str",
    "isinstance",
    "type",
    "len",
    "RuntimeError",
    "LookupError",
    "TypeError",
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
    function_matches_code(&function, expected.code.bind(function.py()), globals)
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
        descriptors.insert(
            name,
            CanonicalDescriptor {
                kind,
                code: code.into_any().unbind(),
            },
        );
    }

    let mut globals = HashMap::new();
    for &name in GLOBALS {
        let current = models.dict().get_item(name)?;
        let admitted =
            global_has_canonical_provenance(py, &models, name, current.as_ref()).unwrap_or(false);
        let expected = if !admitted {
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
    let trusted_at_import = type_is_current(py, &state, &response_type)?
        && DESCRIPTORS.iter().all(|(name, _)| {
            let expected = state
                .descriptors
                .get(name)
                .expect("canonical descriptor is present");
            descriptor_matches(&response_type, name, expected, &models.dict()).unwrap_or(false)
        });
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

fn globals_are_current(py: Python<'_>, state: &ResponseState, names: &[&str]) -> PyResult<bool> {
    let models = state.models.bind(py);
    for name in names {
        let current = models.dict().get_item(*name)?;
        let Some(expected) = state.globals.get(name) else {
            return Ok(false);
        };
        let matches = match expected {
            ExpectedGlobal::Missing => current.is_none(),
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
    let Some(expected) = state.descriptors.get(operation) else {
        return Ok(false);
    };
    Ok(descriptor_matches(
        &subject.get_type(),
        operation,
        expected,
        &state.models.bind(py).dict(),
    )? && globals_are_current(py, state, globals)?)
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
    if content.is(&false.into_pyobject(py)?) {
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
    if !exact_operation(py, subject, "iter_lines", &[])? {
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
    snapshot.set_item("content_is_false", content.is(&false.into_pyobject(py)?))?;
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

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let _ = response_state(module.py())?;
    module.add_class::<NativeContentIterator>()?;
    module.add_class::<NativeLinesIterator>()?;
    module.add_function(wrap_pyfunction!(_response_content_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_iter_content_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_iter_lines_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_response_fields_snapshot, module)?)?;
    module.add_function(wrap_pyfunction!(_response_disposition_trial, module)?)?;
    Ok(())
}

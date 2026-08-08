use std::cell::Cell;
use std::future::{self, Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use pyo3::exceptions::{PyBaseException, PyNameError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyBytes, PyBytesMethods, PyDict, PyDictMethods, PyFunction, PyIterator,
    PyList, PyListMethods, PyModule, PyString, PyStringMethods, PyTuple, PyTupleMethods, PyType,
    PyTypeMethods,
};
use pyo3::wrap_pyfunction;
use requests::AsyncBody;

use crate::bridge::{ActionReceiver, ActionSender, BridgeClosed, WorkerPayload, action_channel};
use crate::models::{PreparedBodyMethod, trusted_prepared_body_method, trusted_rewind_body};
use crate::runtime::{run_with_actions_and_signal_checker, run_with_owned_actions};

const BODY_BLOCK_SIZE: usize = 4;

#[derive(Clone, Copy, Debug)]
enum BodyAction {
    Read { size: usize },
    Next,
}

#[derive(Debug)]
enum BodyReply {
    Chunk(Vec<u8>),
    Skip,
    End,
    Failed,
}

impl WorkerPayload for BodyAction {}
impl WorkerPayload for BodyReply {}

#[derive(Clone, Copy)]
enum AdapterMode {
    Read,
    Next,
}

type ReplyFuture = Pin<Box<dyn Future<Output = Result<BodyReply, BridgeClosed>> + Send + 'static>>;

enum AdapterState {
    Idle,
    Waiting(ReplyFuture),
    Done,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdapterFailure {
    None = 0,
    Handler = 1,
    ActionReceiver = 2,
    ReplySender = 3,
}

impl AdapterFailure {
    fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Handler,
            2 => Self::ActionReceiver,
            3 => Self::ReplySender,
            _ => Self::None,
        }
    }
}

struct OriginBodyOwner {
    body: Py<PyAny>,
}

struct OriginBodyRunOwner {
    owner: OriginBodyOwner,
    source: OriginBodySource,
    handler_error: Option<PyErr>,
}

struct PythonBodyAdapter {
    actions: ActionSender<BodyAction, BodyReply>,
    mode: AdapterMode,
    state: AdapterState,
    size_hint: Option<u64>,
    failure: Arc<AtomicU8>,
    queued_actions: Option<Arc<AtomicU8>>,
    replies_observed: Option<Arc<AtomicU8>>,
}

impl PythonBodyAdapter {
    fn new(
        actions: ActionSender<BodyAction, BodyReply>,
        mode: AdapterMode,
        size_hint: Option<u64>,
        failure: Arc<AtomicU8>,
    ) -> Self {
        Self {
            actions,
            mode,
            state: AdapterState::Idle,
            size_hint,
            failure,
            queued_actions: None,
            replies_observed: None,
        }
    }

    fn with_lifecycle_counters(
        mut self,
        queued_actions: Arc<AtomicU8>,
        replies_observed: Arc<AtomicU8>,
    ) -> Self {
        self.queued_actions = Some(queued_actions);
        self.replies_observed = Some(replies_observed);
        self
    }

    fn mark_reply_observed(&self) {
        if let Some(replies_observed) = &self.replies_observed {
            replies_observed.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn begin_action(&mut self) {
        let action = match self.mode {
            AdapterMode::Read => BodyAction::Read {
                size: BODY_BLOCK_SIZE,
            },
            AdapterMode::Next => BodyAction::Next,
        };
        let actions = self.actions.clone();
        let queued_actions = self.queued_actions.clone();
        self.state = AdapterState::Waiting(Box::pin(async move {
            let receive_reply = actions.enqueue(action)?;
            if let Some(queued_actions) = queued_actions {
                queued_actions.fetch_add(1, Ordering::AcqRel);
            }
            receive_reply.await.map_err(|_| BridgeClosed::ReplySender)
        }));
    }

    fn fail(&mut self, failure: AdapterFailure) {
        let _ = self.failure.compare_exchange(
            AdapterFailure::None as u8,
            failure as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.state = AdapterState::Done;
    }

    fn fail_item(&mut self, failure: AdapterFailure) -> Poll<Option<requests::Result<Bytes>>> {
        self.fail(failure);
        Poll::Ready(Some(Err(requests::Error::body_stream())))
    }
}

impl AsyncBody for PythonBodyAdapter {
    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                AdapterState::Idle => this.begin_action(),
                AdapterState::Waiting(reply) => match reply.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(BodyReply::Chunk(chunk))) => {
                        this.mark_reply_observed();
                        this.state = AdapterState::Idle;
                        return Poll::Ready(Some(Ok(Bytes::from(chunk))));
                    }
                    Poll::Ready(Ok(BodyReply::Skip)) => {
                        this.mark_reply_observed();
                        this.state = AdapterState::Idle;
                        return Poll::Ready(Some(Ok(Bytes::new())));
                    }
                    Poll::Ready(Ok(BodyReply::End)) => {
                        this.mark_reply_observed();
                        this.state = AdapterState::Done;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Ok(BodyReply::Failed)) => {
                        this.mark_reply_observed();
                        return this.fail_item(AdapterFailure::Handler);
                    }
                    Poll::Ready(Err(BridgeClosed::ActionReceiver)) => {
                        return this.fail_item(AdapterFailure::ActionReceiver);
                    }
                    Poll::Ready(Err(BridgeClosed::ReplySender)) => {
                        return this.fail_item(AdapterFailure::ReplySender);
                    }
                },
                AdapterState::Done => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> Option<u64> {
        self.size_hint
    }
}

enum OriginBodySource {
    Read,
    Iterator(Py<PyIterator>),
    Once {
        pending: bool,
        cached_chunk: Option<Py<PyAny>>,
    },
}

struct BodySelection {
    mode: AdapterMode,
    source: OriginBodySource,
    size_hint: Option<u64>,
}

struct GlobalResolver<'py> {
    globals: Bound<'py, PyDict>,
    builtins: Bound<'py, PyDict>,
}

impl<'py> GlobalResolver<'py> {
    fn from_bound_method(callable: &Bound<'py, PyAny>) -> PyResult<Self> {
        Self::from_function(&callable.getattr("__func__")?)
    }

    fn from_function(callable: &Bound<'py, PyAny>) -> PyResult<Self> {
        let function = callable.cast::<PyFunction>()?;
        Ok(Self {
            globals: function.getattr("__globals__")?.cast_into::<PyDict>()?,
            builtins: function.getattr("__builtins__")?.cast_into::<PyDict>()?,
        })
    }

    fn get(&self, name: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
        if let Some(value) = self.globals.get_item(name)? {
            return Ok(Some(value));
        }
        self.builtins.get_item(name)
    }

    fn require(&self, name: &str) -> PyResult<Bound<'py, PyAny>> {
        self.get(name)?
            .ok_or_else(|| PyNameError::new_err(format!("name '{name}' is not defined")))
    }

    fn has_all(&self, names: &[&str]) -> PyResult<bool> {
        for name in names {
            if self.get(name)?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn exception_tuple(&self, py: Python<'py>, names: &[&str]) -> PyResult<Bound<'py, PyTuple>> {
        let mut exceptions = Vec::with_capacity(names.len());
        for name in names {
            exceptions.push(self.require(name)?);
        }
        PyTuple::new(py, exceptions)
    }
}

fn store_handler_error(slot: &mut Option<PyErr>, error: PyErr) -> BodyReply {
    if slot.is_none() {
        *slot = Some(error);
    }
    BodyReply::Failed
}

fn bridge_error(failure: AdapterFailure) -> PyErr {
    let message = match failure {
        AdapterFailure::ActionReceiver => BridgeClosed::ActionReceiver.to_string(),
        AdapterFailure::ReplySender => BridgeClosed::ReplySender.to_string(),
        AdapterFailure::Handler => "Python body handler failed".to_owned(),
        AdapterFailure::None => "Python body adapter ended unexpectedly".to_owned(),
    };
    PyRuntimeError::new_err(message)
}

fn body_failure(failure: &AtomicU8) -> AdapterFailure {
    AdapterFailure::from_raw(failure.load(Ordering::Acquire))
}

fn exception_matches(
    py: Python<'_>,
    error: &PyErr,
    exception: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if exception.is_instance_of::<PyTuple>() {
        for candidate in exception.cast::<PyTuple>()?.iter() {
            if exception_matches(py, error, &candidate)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let Ok(exception_type) = exception.cast::<PyType>() else {
        return Err(PyTypeError::new_err(
            "catching classes that do not inherit from BaseException is not allowed",
        ));
    };
    if !exception_type.is_subclass(&py.get_type::<PyBaseException>())? {
        return Err(PyTypeError::new_err(
            "catching classes that do not inherit from BaseException is not allowed",
        ));
    }
    Ok(error.is_instance(py, exception))
}

fn body_action(
    py: Python<'_>,
    action: BodyAction,
    owner: &OriginBodyOwner,
    source: &mut OriginBodySource,
    error: &mut Option<PyErr>,
) -> BodyReply {
    let result = match (action, source) {
        (BodyAction::Read { size }, OriginBodySource::Read) => {
            read_body_chunk(py, owner.body.bind(py), size)
        }
        (BodyAction::Next, OriginBodySource::Iterator(iterator)) => {
            next_body_chunk(py, iterator.bind(py))
        }
        (
            BodyAction::Next,
            OriginBodySource::Once {
                pending,
                cached_chunk,
            },
        ) => {
            if *pending {
                *pending = false;
                once_body_chunk(py, owner.body.bind(py), cached_chunk)
            } else {
                Ok(BodyReply::End)
            }
        }
        _ => Err(PyRuntimeError::new_err(
            "Python body adapter received an unexpected action",
        )),
    };
    match result {
        Ok(reply) => reply,
        Err(cause) => store_handler_error(error, cause),
    }
}

fn read_body_chunk(py: Python<'_>, body: &Bound<'_, PyAny>, size: usize) -> PyResult<BodyReply> {
    let chunk = body.call_method1("read", (size,))?;
    if !chunk.is_truthy()? {
        return Ok(BodyReply::End);
    }
    Ok(BodyReply::Chunk(chunk_bytes(py, &chunk)?))
}

fn next_body_chunk(py: Python<'_>, iterator: &Bound<'_, PyAny>) -> PyResult<BodyReply> {
    let mut iterator = iterator.cast::<PyIterator>()?.clone();
    let chunk = match iterator.next() {
        Some(Ok(chunk)) => chunk,
        Some(Err(error)) => return Err(error),
        None => return Ok(BodyReply::End),
    };
    if !chunk.is_truthy()? {
        return Ok(BodyReply::Skip);
    }
    Ok(BodyReply::Chunk(chunk_bytes(py, &chunk)?))
}

fn once_body_chunk(
    py: Python<'_>,
    body: &Bound<'_, PyAny>,
    cached_chunk: &mut Option<Py<PyAny>>,
) -> PyResult<BodyReply> {
    if let Some(chunk) = cached_chunk.take() {
        let chunk = chunk.bind(py);
        if !chunk.is_truthy()? {
            return Ok(BodyReply::Skip);
        }
        return Ok(BodyReply::Chunk(chunk_bytes(py, chunk)?));
    }
    if !body.is_truthy()? {
        return Ok(BodyReply::Skip);
    }
    Ok(BodyReply::Chunk(chunk_bytes(py, body)?))
}

fn chunk_bytes(py: Python<'_>, chunk: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if chunk.is_instance_of::<PyString>() {
        let encoded = chunk.call_method1("encode", ("utf-8",))?;
        return Ok(encoded.cast::<PyBytes>()?.as_bytes().to_vec());
    }
    if chunk.is_instance_of::<PyBytes>() {
        return Ok(chunk.cast::<PyBytes>()?.as_bytes().to_vec());
    }

    let builtins = PyModule::import(py, "builtins")?;
    let memoryview = PyModule::import(py, "builtins")?.getattr("memoryview")?;
    let converted = match memoryview
        .call1((chunk,))
        .and_then(|view| builtins.getattr("bytes")?.call1((view,)))
    {
        Ok(converted) => converted,
        Err(error) if error.is_instance_of::<PyTypeError>(py) => {
            match builtins.getattr("len")?.call1((chunk,)) {
                Ok(_) => return Err(error),
                Err(length_error) => return Err(length_error),
            }
        }
        Err(error) => return Err(error),
    };
    Ok(converted.cast::<PyBytes>()?.as_bytes().to_vec())
}

fn select_body_source(py: Python<'_>, body: &Bound<'_, PyAny>) -> PyResult<Option<BodySelection>> {
    if body.is_none() {
        return Ok(None);
    }
    if body.is_instance_of::<PyString>() {
        let encoded = body.call_method0("encode")?;
        let size_hint = encoded.len()? as u64;
        return Ok(Some(BodySelection {
            mode: AdapterMode::Next,
            source: OriginBodySource::Once {
                pending: true,
                cached_chunk: Some(encoded.unbind()),
            },
            size_hint: Some(size_hint),
        }));
    }
    if body.is_instance_of::<PyBytes>() {
        return Ok(Some(BodySelection {
            mode: AdapterMode::Next,
            source: OriginBodySource::Once {
                pending: true,
                cached_chunk: None,
            },
            size_hint: Some(body.len()? as u64),
        }));
    }
    if body.hasattr("read")? {
        return Ok(Some(BodySelection {
            mode: AdapterMode::Read,
            source: OriginBodySource::Read,
            size_hint: None,
        }));
    }

    let builtins = PyModule::import(py, "builtins")?;
    match builtins.getattr("memoryview")?.call1((body,)) {
        Ok(view) => Ok(Some(BodySelection {
            mode: AdapterMode::Next,
            source: OriginBodySource::Once {
                pending: true,
                cached_chunk: None,
            },
            size_hint: Some(view.getattr("nbytes")?.extract::<u64>()?),
        })),
        Err(error) if error.is_instance_of::<PyTypeError>(py) => {
            let iterator = match builtins.getattr("iter")?.call1((body,)) {
                Ok(iterator) => iterator.cast_into::<PyIterator>()?.unbind(),
                Err(error) if error.is_instance_of::<PyTypeError>(py) => {
                    let representation = body.repr()?.to_str()?.to_owned();
                    return Err(PyTypeError::new_err(format!(
                        "'body' must be a bytes-like object, file-like object, or iterable. Instead was {representation}"
                    )));
                }
                Err(error) => return Err(error),
            };
            Ok(Some(BodySelection {
                mode: AdapterMode::Next,
                source: OriginBodySource::Iterator(iterator),
                size_hint: None,
            }))
        }
        Err(error) => Err(error),
    }
}

async fn collect_adapter(mut adapter: Pin<Box<PythonBodyAdapter>>, limit: usize) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    while chunks.len() < limit {
        let next = poll_fn(|context| adapter.as_mut().poll_next(context)).await;
        match next {
            Some(Ok(chunk)) if !chunk.is_empty() => chunks.push(chunk.to_vec()),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
    chunks
}

#[pyfunction]
fn _body_stream_collect_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    limit: usize,
) -> PyResult<Vec<Vec<u8>>> {
    let body = subject.getattr("body")?;
    let Some(selection) = select_body_source(py, &body)? else {
        return Ok(Vec::new());
    };
    let owner = OriginBodyRunOwner {
        source: selection.source,
        handler_error: None,
        owner: OriginBodyOwner {
            body: body.unbind(),
        },
    };
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let worker_failure = Arc::clone(&failure);
    let (chunks, mut owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| {
            let adapter = Box::pin(PythonBodyAdapter::new(
                actions,
                selection.mode,
                selection.size_hint,
                worker_failure,
            ));
            async move { collect_adapter(adapter, limit).await }
        },
        |py, action, owner| {
            body_action(
                py,
                action,
                &owner.owner,
                &mut owner.source,
                &mut owner.handler_error,
            )
        },
    )?;
    if let Some(error) = owner.handler_error.take() {
        return Err(error);
    }
    let failure = body_failure(&failure);
    if failure != AdapterFailure::None {
        return Err(bridge_error(failure));
    }
    Ok(chunks)
}

#[pyfunction]
fn _body_stream_cancel_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
) -> PyResult<()> {
    cancel_body_at_phase(py, subject, error, CancelPhase::ReplyObserved, None)
}

#[pyfunction]
fn _body_stream_cancel_before_poll_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
) -> PyResult<()> {
    cancel_body_at_phase(py, subject, error, CancelPhase::BeforePoll, None)
}

#[pyfunction]
fn _body_stream_cancel_phase_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
    phase: &str,
    audit: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let phase = match phase {
        "before-poll" => CancelPhase::BeforePoll,
        "queued-before-dequeue" => CancelPhase::QueuedBeforeDequeue,
        "reply-observed" => CancelPhase::ReplyObserved,
        _ => {
            return Err(PyValueError::new_err(format!(
                "unknown body cancellation phase: {phase}"
            )));
        }
    };
    cancel_body_at_phase(py, subject, error, phase, Some(audit))
}

#[derive(Clone, Copy)]
enum CancelPhase {
    BeforePoll,
    QueuedBeforeDequeue,
    ReplyObserved,
}

const WORKER_STARTING: u8 = 0;
const WORKER_AWAITING_REPLY: u8 = 1;
const WORKER_REPLY_OBSERVED: u8 = 2;
const WORKER_DROPPED: u8 = 3;
const PHASE_WAIT: Duration = Duration::from_millis(500);

struct WorkerDropGuard {
    phase: Arc<AtomicU8>,
}

impl Drop for WorkerDropGuard {
    fn drop(&mut self) {
        self.phase.store(WORKER_DROPPED, Ordering::Release);
    }
}

fn wait_for_worker_phase(
    py: Python<'_>,
    phase: &AtomicU8,
    expected: u8,
    label: &str,
) -> PyResult<()> {
    let deadline = Instant::now() + PHASE_WAIT;
    while phase.load(Ordering::Acquire) != expected {
        if Instant::now() >= deadline {
            return Err(PyRuntimeError::new_err(format!(
                "body worker did not reach {label}"
            )));
        }
        py.detach(|| std::thread::sleep(Duration::from_millis(1)));
    }
    Ok(())
}

fn require_worker_dropped(phase: &AtomicU8) -> PyResult<()> {
    if phase.load(Ordering::Acquire) == WORKER_DROPPED {
        Ok(())
    } else {
        Err(PyRuntimeError::new_err(
            "body worker was not dropped before its origin owner",
        ))
    }
}

fn cancel_body_at_phase(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    error: Py<PyAny>,
    phase: CancelPhase,
    audit: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    match phase {
        CancelPhase::BeforePoll => cancel_before_poll(py, error, audit),
        CancelPhase::QueuedBeforeDequeue => cancel_queued_before_dequeue(py, error, audit),
        CancelPhase::ReplyObserved => {
            let body = subject.getattr("body")?;
            if body.is_none() {
                return Err(PyRuntimeError::new_err("cannot cancel an empty body"));
            }
            let owner = OriginBodyOwner {
                body: body.unbind(),
            };
            let result = cancel_after_reply(py, error, &owner, audit);
            drop(owner);
            result
        }
    }
}

fn cancel_before_poll(
    py: Python<'_>,
    error: Py<PyAny>,
    audit: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let worker_phase = Arc::new(AtomicU8::new(WORKER_STARTING));
    let worker_phase_guard = WorkerDropGuard {
        phase: Arc::clone(&worker_phase),
    };
    let queued_actions = Arc::new(AtomicU8::new(0));
    let worker_queued_actions = Arc::clone(&queued_actions);
    let replies_observed = Arc::new(AtomicU8::new(0));
    let worker_replies_observed = Arc::clone(&replies_observed);
    let action_count = Cell::new(0_u8);
    let result = run_with_actions_and_signal_checker(
        py,
        move |actions| {
            let adapter = PythonBodyAdapter::new(actions, AdapterMode::Next, None, failure)
                .with_lifecycle_counters(worker_queued_actions, worker_replies_observed);
            async move {
                let _worker_phase_guard = worker_phase_guard;
                let _adapter = adapter;
                future::pending::<()>().await
            }
        },
        |_py, _action| {
            action_count.set(action_count.get().saturating_add(1));
            BodyReply::Failed
        },
        |py| Err(PyErr::from_value(error.bind(py).clone())),
    );
    require_worker_dropped(&worker_phase)?;
    let queued_actions = queued_actions.load(Ordering::Acquire);
    let replies_observed = replies_observed.load(Ordering::Acquire);
    if queued_actions != 0 || action_count.get() != 0 || replies_observed != 0 {
        return Err(PyRuntimeError::new_err(
            "body action ran before the adapter was polled",
        ));
    }
    if let Some(audit) = audit {
        audit.set_item("queued", queued_actions)?;
        audit.set_item("execute", action_count.get())?;
        audit.set_item("reply_observed", replies_observed != 0)?;
        audit.set_item(
            "worker_dropped",
            worker_phase.load(Ordering::Acquire) == WORKER_DROPPED,
        )?;
    }
    result
}

fn cancel_queued_before_dequeue(
    py: Python<'_>,
    error: Py<PyAny>,
    audit: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let worker_phase = Arc::new(AtomicU8::new(WORKER_STARTING));
    let worker_poll_phase = Arc::clone(&worker_phase);
    let worker_signal_phase = Arc::clone(&worker_phase);
    let worker_phase_guard = WorkerDropGuard {
        phase: Arc::clone(&worker_phase),
    };
    let queued_actions = Arc::new(AtomicU8::new(0));
    let worker_queued_actions = Arc::clone(&queued_actions);
    let replies_observed = Arc::new(AtomicU8::new(0));
    let worker_replies_observed = Arc::clone(&replies_observed);
    let action_count = Cell::new(0_u8);
    let result = run_with_actions_and_signal_checker(
        py,
        move |actions| {
            let mut adapter = Box::pin(
                PythonBodyAdapter::new(actions, AdapterMode::Next, None, failure)
                    .with_lifecycle_counters(worker_queued_actions, worker_replies_observed),
            );
            async move {
                let _worker_phase_guard = worker_phase_guard;
                poll_fn(|context| match adapter.as_mut().poll_next(context) {
                    Poll::Pending => {
                        worker_poll_phase.store(WORKER_AWAITING_REPLY, Ordering::Release);
                        Poll::Pending
                    }
                    Poll::Ready(_) => Poll::Ready(()),
                })
                .await;
            }
        },
        |_py, _action| {
            action_count.set(action_count.get().saturating_add(1));
            BodyReply::Failed
        },
        |py| {
            wait_for_worker_phase(
                py,
                &worker_signal_phase,
                WORKER_AWAITING_REPLY,
                "queued-before-dequeue",
            )?;
            Err(PyErr::from_value(error.bind(py).clone()))
        },
    );
    require_worker_dropped(&worker_phase)?;
    let queued_actions = queued_actions.load(Ordering::Acquire);
    let replies_observed = replies_observed.load(Ordering::Acquire);
    if queued_actions != 1 || action_count.get() != 0 || replies_observed != 0 {
        return Err(PyRuntimeError::new_err(
            "queued body action executed before cancellation",
        ));
    }
    if let Some(audit) = audit {
        audit.set_item("queued", queued_actions)?;
        audit.set_item("execute", action_count.get())?;
        audit.set_item("reply_observed", replies_observed != 0)?;
        audit.set_item(
            "worker_dropped",
            worker_phase.load(Ordering::Acquire) == WORKER_DROPPED,
        )?;
    }
    result
}

fn cancel_after_reply(
    py: Python<'_>,
    error: Py<PyAny>,
    owner: &OriginBodyOwner,
    audit: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    let Some(mut selection) = select_body_source(py, owner.body.bind(py))? else {
        return Err(PyRuntimeError::new_err("cannot cancel an empty body"));
    };
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let worker_failure = Arc::clone(&failure);
    let worker_phase = Arc::new(AtomicU8::new(WORKER_STARTING));
    let worker_poll_phase = Arc::clone(&worker_phase);
    let worker_signal_phase = Arc::clone(&worker_phase);
    let worker_phase_guard = WorkerDropGuard {
        phase: Arc::clone(&worker_phase),
    };
    let queued_actions = Arc::new(AtomicU8::new(0));
    let worker_queued_actions = Arc::clone(&queued_actions);
    let replies_observed = Arc::new(AtomicU8::new(0));
    let worker_replies_observed = Arc::clone(&replies_observed);
    let action_count = Cell::new(0_u8);
    let mut handler_error = None;
    let result = run_with_actions_and_signal_checker(
        py,
        move |actions| {
            let mut adapter = Box::pin(
                PythonBodyAdapter::new(
                    actions,
                    selection.mode,
                    selection.size_hint,
                    worker_failure,
                )
                .with_lifecycle_counters(worker_queued_actions, worker_replies_observed),
            );
            async move {
                let _worker_phase_guard = worker_phase_guard;
                if matches!(
                    poll_fn(|context| adapter.as_mut().poll_next(context)).await,
                    Some(Ok(_))
                ) {
                    worker_poll_phase.store(WORKER_REPLY_OBSERVED, Ordering::Release);
                    future::pending::<()>().await;
                }
            }
        },
        |py, action| {
            let reply = body_action(py, action, owner, &mut selection.source, &mut handler_error);
            if !matches!(reply, BodyReply::Failed) {
                action_count.set(action_count.get().saturating_add(1));
            }
            reply
        },
        |py| {
            if action_count.get() == 0 {
                return Ok(());
            }
            wait_for_worker_phase(
                py,
                &worker_signal_phase,
                WORKER_REPLY_OBSERVED,
                "reply-observed",
            )?;
            Err(PyErr::from_value(error.bind(py).clone()))
        },
    );
    require_worker_dropped(&worker_phase)?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    let queued_actions = queued_actions.load(Ordering::Acquire);
    let replies_observed = replies_observed.load(Ordering::Acquire);
    if queued_actions != 1 || action_count.get() != 1 || replies_observed != 1 {
        return Err(PyRuntimeError::new_err(format!(
            "reply-observed cancellation mismatch: queued={queued_actions}, executed={}, replies={replies_observed}",
            action_count.get(),
        )));
    }
    let failure = body_failure(&failure);
    if failure != AdapterFailure::None {
        return Err(bridge_error(failure));
    }
    if let Some(audit) = audit {
        audit.set_item("total_queued", queued_actions)?;
        audit.set_item("execute", action_count.get())?;
        audit.set_item("reply_observed", replies_observed != 0)?;
        audit.set_item(
            "worker_dropped",
            worker_phase.load(Ordering::Acquire) == WORKER_DROPPED,
        )?;
    }
    result
}

fn poll_adapter(
    adapter: &mut Pin<Box<PythonBodyAdapter>>,
) -> Poll<Option<requests::Result<Bytes>>> {
    let mut context = Context::from_waker(Waker::noop());
    adapter.as_mut().poll_next(&mut context)
}

#[pyfunction]
fn _body_stream_poll_state_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let body = subject.getattr("body")?;
    let Some(selection) = select_body_source(py, &body)? else {
        return Err(PyRuntimeError::new_err("cannot poll an empty body"));
    };
    let owner = OriginBodyOwner {
        body: body.unbind(),
    };
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let (actions, mut receiver) = action_channel();
    let mut adapter = Box::pin(PythonBodyAdapter::new(
        actions,
        selection.mode,
        selection.size_hint,
        failure,
    ));
    let first_poll_pending = poll_adapter(&mut adapter).is_pending();
    let second_poll_pending = poll_adapter(&mut adapter).is_pending();
    let queued_actions = count_queued_actions(&mut receiver);
    drop(adapter);
    drop(selection.source);
    drop(owner);

    let result = PyDict::new(py);
    result.set_item("first_poll_pending", first_poll_pending)?;
    result.set_item("second_poll_pending", second_poll_pending)?;
    result.set_item("queued_actions", queued_actions)?;
    Ok(result.into_any().unbind())
}

fn count_queued_actions(receiver: &mut ActionReceiver<BodyAction, BodyReply>) -> usize {
    let mut count = 0;
    while receiver.recv_timeout(Duration::ZERO).is_ok() {
        count += 1;
    }
    count
}

#[pyfunction]
fn _body_stream_disconnect_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    mode: &str,
) -> PyResult<String> {
    let body = subject.getattr("body")?;
    let Some(selection) = select_body_source(py, &body)? else {
        return Err(PyRuntimeError::new_err("cannot disconnect an empty body"));
    };
    let owner = OriginBodyOwner {
        body: body.unbind(),
    };
    let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
    let (actions, mut receiver) = action_channel();
    let mut adapter = Box::pin(PythonBodyAdapter::new(
        actions,
        selection.mode,
        selection.size_hint,
        Arc::clone(&failure),
    ));

    let expected = match mode {
        "action-receiver" => {
            drop(receiver);
            let _ = poll_adapter(&mut adapter);
            AdapterFailure::ActionReceiver
        }
        "reply-sender" => {
            if !poll_adapter(&mut adapter).is_pending() {
                return Err(PyRuntimeError::new_err(
                    "reply disconnect did not begin a body action",
                ));
            }
            let request = receiver
                .recv_timeout(Duration::ZERO)
                .map_err(|_| PyRuntimeError::new_err("body action was not queued"))?;
            drop(request);
            let _ = poll_adapter(&mut adapter);
            AdapterFailure::ReplySender
        }
        _ => {
            return Err(PyRuntimeError::new_err(format!(
                "unknown body disconnect mode: {mode}"
            )));
        }
    };
    let observed = body_failure(&failure);
    drop(adapter);
    drop(selection.source);
    drop(owner);
    if observed != expected {
        return Err(PyRuntimeError::new_err(format!(
            "body disconnect mismatch: expected {expected:?}, observed {observed:?}"
        )));
    }
    Ok(mode.to_owned())
}

const PREPARE_BODY_GLOBALS: &[&str] = &[
    "AttributeError",
    "InvalidJSONError",
    "Iterable",
    "Mapping",
    "NotImplementedError",
    "OSError",
    "TypeError",
    "UnsupportedOperation",
    "ValueError",
    "_t",
    "basestring",
    "builtin_str",
    "bytes",
    "cast",
    "complexjson",
    "getattr",
    "hasattr",
    "isinstance",
    "list",
    "object",
    "str",
    "super_len",
    "tuple",
];

const CONTENT_LENGTH_GLOBALS: &[&str] = &["builtin_str", "super_len"];

#[pyfunction]
fn _prepare_body_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    data: &Bound<'_, PyAny>,
    files: &Bound<'_, PyAny>,
    json: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let (callable, trusted) =
        trusted_prepared_body_method(py, subject, PreparedBodyMethod::PrepareBody)?;
    if !trusted {
        return Ok(callable.call1((data, files, json))?.unbind());
    }

    let globals = GlobalResolver::from_bound_method(&callable)?;
    if !globals.has_all(PREPARE_BODY_GLOBALS)? {
        return Ok(callable.call1((data, files, json))?.unbind());
    }

    prepare_body(py, subject, data, files, json, &globals)?;
    Ok(py.None())
}

fn prepare_body(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    data: &Bound<'_, PyAny>,
    files: &Bound<'_, PyAny>,
    json: &Bound<'_, PyAny>,
    globals: &GlobalResolver<'_>,
) -> PyResult<()> {
    let mut body = py.None();
    let mut content_type: Option<Py<PyAny>> = None;

    if !data.is_truthy()? && !json.is_none() {
        content_type = Some(PyString::new(py, "application/json").into_any().unbind());
        let kwargs = PyDict::new(py);
        kwargs.set_item("allow_nan", false)?;
        let serialized =
            match globals
                .require("complexjson")?
                .call_method("dumps", (json,), Some(&kwargs))
            {
                Ok(serialized) => serialized,
                Err(error) => {
                    let value_error = globals.require("ValueError")?;
                    if exception_matches(py, &error, &value_error)? {
                        let kwargs = PyDict::new(py);
                        kwargs.set_item("request", subject)?;
                        let wrapped = globals
                            .require("InvalidJSONError")?
                            .call((error.value(py),), Some(&kwargs))?;
                        let wrapped = PyErr::from_value(wrapped);
                        wrapped.set_context(py, Some(error));
                        return Err(wrapped);
                    }
                    return Err(error);
                }
            };
        let is_bytes = globals
            .require("isinstance")?
            .call1((&serialized, globals.require("bytes")?))?
            .is_truthy()?;
        body = if is_bytes {
            serialized.unbind()
        } else {
            serialized.call_method1("encode", ("utf-8",))?.unbind()
        };
    }

    let is_iterable = globals
        .require("isinstance")?
        .call1((data, globals.require("Iterable")?))?
        .is_truthy()?
        || globals
            .require("hasattr")?
            .call1((data, "__iter__"))?
            .is_truthy()?;
    let is_stream = if is_iterable {
        let excluded = PyTuple::new(
            py,
            [
                globals.require("str")?,
                globals.require("bytes")?,
                globals.require("list")?,
                globals.require("tuple")?,
                globals.require("Mapping")?,
            ],
        )?;
        !globals
            .require("isinstance")?
            .call1((data, &excluded))?
            .is_truthy()?
    } else {
        false
    };

    if is_stream {
        let length = match globals.require("super_len")?.call1((data,)) {
            Ok(length) => Some(length.unbind()),
            Err(error) => {
                let caught = globals.exception_tuple(
                    py,
                    &["TypeError", "AttributeError", "UnsupportedOperation"],
                )?;
                if exception_matches(py, &error, &caught)? {
                    None
                } else {
                    return Err(error);
                }
            }
        };
        body = data.clone().unbind();

        let tell = globals
            .require("getattr")?
            .call1((body.bind(py), "tell", py.None()))?;
        if !tell.is_none() {
            match body.bind(py).getattr("tell")?.call0() {
                Ok(position) => subject.setattr("_body_position", position)?,
                Err(error) => {
                    if exception_matches(py, &error, &globals.require("OSError")?)? {
                        let sentinel = globals.require("object")?.call0()?;
                        subject.setattr("_body_position", sentinel)?;
                    } else {
                        return Err(error);
                    }
                }
            }
        }

        if files.is_truthy()? {
            let error = globals
                .require("NotImplementedError")?
                .call1(("Streamed bodies and files are mutually exclusive.",))?;
            return Err(PyErr::from_value(error));
        }

        let headers = subject.getattr("headers")?;
        match length {
            Some(length) if length.bind(py).is_truthy()? => {
                let length = globals.require("builtin_str")?.call1((length.bind(py),))?;
                headers.set_item("Content-Length", length)?;
            }
            _ => headers.set_item("Transfer-Encoding", "chunked")?,
        }
    } else {
        let raw_data = globals
            .require("cast")?
            .call1(("_t.RawDataType | None", data))?;
        if files.is_truthy()? {
            let encoded = subject.call_method1("_encode_files", (files, &raw_data))?;
            let (encoded_body, encoded_content_type) = unpack_pair(&encoded)?;
            body = encoded_body;
            content_type = Some(encoded_content_type);
        } else if raw_data.is_truthy()? {
            body = subject
                .call_method1("_encode_params", (&raw_data,))?
                .unbind();
            let is_base_string = globals
                .require("isinstance")?
                .call1((data, globals.require("basestring")?))?
                .is_truthy()?;
            let has_read = if is_base_string {
                false
            } else {
                globals
                    .require("_t")?
                    .getattr("has_read")?
                    .call1((data,))?
                    .is_truthy()?
            };
            if !is_base_string && !has_read {
                content_type = Some(
                    PyString::new(py, "application/x-www-form-urlencoded")
                        .into_any()
                        .unbind(),
                );
            }
        }

        prepare_content_length_stage(py, subject, body.bind(py))?;
        if let Some(content_type) = content_type {
            let headers = subject.getattr("headers")?;
            if content_type.bind(py).is_truthy()? && !headers.contains("content-type")? {
                headers.set_item("Content-Type", content_type.bind(py))?;
            }
        }
    }

    subject.setattr("body", body.bind(py))
}

fn prepare_content_length_stage(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    body: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let (callable, trusted) =
        trusted_prepared_body_method(py, subject, PreparedBodyMethod::PrepareContentLength)?;
    if trusted {
        let globals = GlobalResolver::from_bound_method(&callable)?;
        if globals.has_all(CONTENT_LENGTH_GLOBALS)? {
            return prepare_content_length(py, subject, body, &globals);
        }
    }
    callable.call1((body,))?;
    Ok(())
}

fn unpack_pair(value: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
    let mut iterator = value.try_iter()?;
    let first = match iterator.next() {
        Some(Ok(value)) => value.unbind(),
        None => {
            return Err(PyValueError::new_err(
                "not enough values to unpack (expected 2, got 0)",
            ));
        }
        Some(Err(error)) => return Err(error),
    };
    let second = match iterator.next() {
        Some(Ok(value)) => value.unbind(),
        None => {
            return Err(PyValueError::new_err(
                "not enough values to unpack (expected 2, got 1)",
            ));
        }
        Some(Err(error)) => return Err(error),
    };
    match iterator.next() {
        None => Ok((first, second)),
        Some(Err(error)) => Err(error),
        Some(Ok(_)) => Err(PyValueError::new_err(
            "too many values to unpack (expected 2)",
        )),
    }
}

#[pyfunction]
fn _prepare_content_length_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    body: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let (callable, trusted) =
        trusted_prepared_body_method(py, subject, PreparedBodyMethod::PrepareContentLength)?;
    if !trusted {
        return Ok(callable.call1((body,))?.unbind());
    }
    let globals = GlobalResolver::from_bound_method(&callable)?;
    if !globals.has_all(CONTENT_LENGTH_GLOBALS)? {
        return Ok(callable.call1((body,))?.unbind());
    }
    prepare_content_length(py, subject, body, &globals)?;
    Ok(py.None())
}

fn prepare_content_length(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    body: &Bound<'_, PyAny>,
    globals: &GlobalResolver<'_>,
) -> PyResult<()> {
    if !body.is_none() {
        let length = globals.require("super_len")?.call1((body,))?;
        if length.is_truthy()? {
            let length = globals.require("builtin_str")?.call1((&length,))?;
            let headers = subject.getattr("headers")?;
            headers.set_item("Content-Length", length)?;
        }
        return Ok(());
    }

    let method = subject.getattr("method")?;
    let no_body_methods = PyTuple::new(py, ["GET", "HEAD"])?;
    if !no_body_methods.contains(&method)? {
        let headers = subject.getattr("headers")?;
        if headers.call_method1("get", ("Content-Length",))?.is_none() {
            headers.set_item("Content-Length", "0")?;
        }
    }
    Ok(())
}

const REWIND_GLOBALS: &[&str] = &[
    "OSError",
    "UnrewindableBodyError",
    "getattr",
    "integer_types",
    "isinstance",
];

#[pyfunction]
fn _rewind_body_trial(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let (callable, trusted) = trusted_rewind_body(py)?;
    if !trusted {
        return Ok(callable.call1((subject,))?.unbind());
    }
    let globals = GlobalResolver::from_function(&callable)?;
    if !globals.has_all(REWIND_GLOBALS)? {
        return Ok(callable.call1((subject,))?.unbind());
    }

    let body = subject.getattr("body")?;
    let seek = globals
        .require("getattr")?
        .call1((&body, "seek", py.None()))?;
    if !seek.is_none() {
        let position = subject.getattr("_body_position")?;
        let can_rewind = globals
            .require("isinstance")?
            .call1((&position, globals.require("integer_types")?))?
            .is_truthy()?;
        if can_rewind {
            let seek_position = subject.getattr("_body_position")?;
            match seek.call1((&seek_position,)) {
                Ok(_) => return Ok(py.None()),
                Err(error) => {
                    if exception_matches(py, &error, &globals.require("OSError")?)? {
                        return Err(unrewindable_error_with_context(
                            py,
                            &globals,
                            "An error occurred when rewinding request body for redirect.",
                            error,
                        )?);
                    }
                    return Err(error);
                }
            }
        }
    }
    Err(unrewindable_error(
        &globals,
        "Unable to rewind request body for redirect.",
    )?)
}

fn unrewindable_error(globals: &GlobalResolver<'_>, message: &str) -> PyResult<PyErr> {
    Ok(PyErr::from_value(
        globals
            .require("UnrewindableBodyError")?
            .call1((message,))?,
    ))
}

fn unrewindable_error_with_context(
    py: Python<'_>,
    globals: &GlobalResolver<'_>,
    message: &str,
    context: PyErr,
) -> PyResult<PyErr> {
    let error = PyErr::from_value(
        globals
            .require("UnrewindableBodyError")?
            .call1((message,))?,
    );
    error.set_context(py, Some(context));
    Ok(error)
}

fn value_record(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let value_type = value.get_type();
    let type_record = PyList::empty(py);
    type_record.append(value_type.module()?)?;
    type_record.append(value_type.qualname()?)?;

    let payload: Py<PyAny> = if value.is_none() {
        py.None()
    } else if value.is_instance_of::<PyBytes>() {
        let bytes = value.cast::<PyBytes>()?.as_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        let payload = PyList::empty(py);
        payload.append("bytes")?;
        payload.append(hex)?;
        payload.into_any().unbind()
    } else if value.is_instance_of::<PyString>() {
        let payload = PyList::empty(py);
        payload.append("str")?;
        payload.append(value)?;
        payload.into_any().unbind()
    } else {
        let payload = PyList::empty(py);
        payload.append("opaque")?;
        payload.append(value_type.module()?)?;
        payload.append(value_type.qualname()?)?;
        payload.into_any().unbind()
    };

    let record = PyDict::new(py);
    record.set_item("type", type_record)?;
    record.set_item("payload", payload)?;
    Ok(record.into_any().unbind())
}

#[pyfunction]
fn _body_fields_snapshot(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let method = subject.getattr("method")?;
    let headers = subject.getattr("headers")?;
    let body = subject.getattr("body")?;
    let position = subject.getattr("_body_position")?;
    let header_rows = py
        .get_type::<PyList>()
        .call1((headers.call_method0("items")?,))?;

    let snapshot = PyDict::new(py);
    snapshot.set_item("method", method)?;
    snapshot.set_item("headers", header_rows)?;
    snapshot.set_item("body", value_record(py, &body)?)?;
    snapshot.set_item("position", value_record(py, &position)?)?;
    Ok(snapshot.into_any().unbind())
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_prepare_body_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_prepare_content_length_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_rewind_body_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_body_stream_collect_trial, module)?)?;
    module.add_function(wrap_pyfunction!(
        _body_stream_cancel_before_poll_trial,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(_body_stream_cancel_phase_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_body_stream_cancel_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_body_stream_poll_state_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_body_stream_disconnect_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_body_fields_snapshot, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use bytes::Bytes;
    use requests::{AsyncBody, ErrorKind};

    use super::{
        AdapterFailure, AdapterMode, BodyAction, BodyReply, PythonBodyAdapter, action_channel,
    };
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload>() {}

    fn poll_adapter(
        adapter: &mut Pin<Box<PythonBodyAdapter>>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
        let mut context = Context::from_waker(Waker::noop());
        adapter.as_mut().poll_next(&mut context)
    }

    fn assert_body_error_then_eof(adapter: &mut Pin<Box<PythonBodyAdapter>>) {
        let Poll::Ready(Some(Err(error))) = poll_adapter(adapter) else {
            panic!("expected one body error");
        };
        assert_eq!(error.kind(), ErrorKind::Body);
        assert!(matches!(poll_adapter(adapter), Poll::Ready(None)));
    }

    #[test]
    fn body_payloads_are_explicit_worker_payloads() {
        assert_worker_payload::<BodyAction>();
        assert_worker_payload::<BodyReply>();
    }

    #[test]
    fn adapter_handler_failure_yields_one_error_then_eof() {
        let (actions, mut receiver) = action_channel();
        let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
        let mut adapter = Box::pin(PythonBodyAdapter::new(
            actions,
            AdapterMode::Next,
            None,
            Arc::clone(&failure),
        ));

        assert!(poll_adapter(&mut adapter).is_pending());
        let request = receiver
            .recv_timeout(Duration::ZERO)
            .expect("body action should be queued");
        let (_action, reply) = request.into_parts();
        reply
            .send(BodyReply::Failed)
            .expect("adapter should still await the reply");

        assert_body_error_then_eof(&mut adapter);
        assert_eq!(
            AdapterFailure::from_raw(failure.load(Ordering::Acquire)),
            AdapterFailure::Handler
        );
    }

    #[test]
    fn adapter_action_receiver_close_yields_one_error_then_eof() {
        let (actions, receiver) = action_channel();
        let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
        let mut adapter = Box::pin(PythonBodyAdapter::new(
            actions,
            AdapterMode::Next,
            None,
            Arc::clone(&failure),
        ));
        drop(receiver);

        assert_body_error_then_eof(&mut adapter);
        assert_eq!(
            AdapterFailure::from_raw(failure.load(Ordering::Acquire)),
            AdapterFailure::ActionReceiver
        );
    }

    #[test]
    fn adapter_reply_sender_close_yields_one_error_then_eof() {
        let (actions, mut receiver) = action_channel();
        let failure = Arc::new(AtomicU8::new(AdapterFailure::None as u8));
        let mut adapter = Box::pin(PythonBodyAdapter::new(
            actions,
            AdapterMode::Next,
            None,
            Arc::clone(&failure),
        ));

        assert!(poll_adapter(&mut adapter).is_pending());
        let request = receiver
            .recv_timeout(Duration::ZERO)
            .expect("body action should be queued");
        drop(request);

        assert_body_error_then_eof(&mut adapter);
        assert_eq!(
            AdapterFailure::from_raw(failure.load(Ordering::Acquire)),
            AdapterFailure::ReplySender
        );
    }
}

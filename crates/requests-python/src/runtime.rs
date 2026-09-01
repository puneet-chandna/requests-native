use std::any::Any;
use std::cell::{Cell, RefCell};
use std::future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};
use pyo3::wrap_pyfunction;
use requests::blocking::{
    BlockingDriverError, BlockingRuntimeDriver, BlockingSubmission, BlockingTaskError,
    outstanding_submission_count,
};

use crate::bridge::{ActionReceiver, ActionSender, BridgeClosed, WorkerPayload, action_channel};

const WAKE_INTERVAL: Duration = Duration::from_millis(10);
const CANCEL_WAIT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCancellationPhase {
    CancelBeforePoll,
    CancelQueuedBeforeDequeue,
    CancelReplyObserved,
    CancelTerminalAfterTimeout,
    CancelPermanentlyNonterminal,
}

pub(crate) enum ActionReplyDisposition<R> {
    Reply(R),
    Withhold(SessionCancellationPhase),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OriginQuarantineRetentionAudit {
    phase: SessionCancellationPhase,
    observed_phase: SessionCancellationPhase,
    terminal: bool,
}

impl OriginQuarantineRetentionAudit {
    pub(crate) fn retain(phase: SessionCancellationPhase, terminal: bool) -> Self {
        Self {
            phase,
            observed_phase: phase,
            terminal,
        }
    }

    pub(crate) fn can_reap(self) -> bool {
        self.terminal
            && self.phase != SessionCancellationPhase::CancelPermanentlyNonterminal
            && self.observed_phase != SessionCancellationPhase::CancelPermanentlyNonterminal
    }

    fn with_observed_phase(mut self, observed_phase: SessionCancellationPhase) -> Self {
        self.observed_phase = observed_phase;
        self
    }
}

pub(crate) fn OriginQuarantineReapAudit(audit: OriginQuarantineRetentionAudit) -> bool {
    audit.can_reap()
}

pub(crate) fn signal_wins_ready_result(py: Python<'_>) -> PyResult<()> {
    py.check_signals()
}

thread_local! {
    static ORIGIN_QUARANTINE: RefCell<Vec<Box<dyn OriginQuarantineEntry>>> = RefCell::new(Vec::new());
    static LAST_ORIGIN_QUARANTINE_TOKEN: Cell<Option<OriginQuarantineToken>> = const { Cell::new(None) };
    static ACTIVE_ORIGIN_SUBMISSION: Cell<Option<u64>> = const { Cell::new(None) };
}

struct ActiveOriginSubmissionGuard(Option<u64>);

impl ActiveOriginSubmissionGuard {
    fn enter(id: u64) -> Self {
        Self(ACTIVE_ORIGIN_SUBMISSION.with(|active| active.replace(Some(id))))
    }
}

impl Drop for ActiveOriginSubmissionGuard {
    fn drop(&mut self) {
        ACTIVE_ORIGIN_SUBMISSION.with(|active| active.set(self.0));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OriginQuarantineToken(u64);

static NEXT_ORIGIN_QUARANTINE_TOKEN: AtomicU64 = AtomicU64::new(1);

trait OriginQuarantineEntry {
    fn retention_audit(&self) -> OriginQuarantineRetentionAudit;
    fn token(&self) -> OriginQuarantineToken;
    fn take_owner(&mut self) -> Option<Box<dyn Any>>;
    fn retains_owner(&self) -> bool;
}

struct QuarantinedOrigin<A, R, O> {
    token: OriginQuarantineToken,
    terminal: std::sync::Arc<AtomicBool>,
    phase: SessionCancellationPhase,
    actions: Option<ActionReceiver<A, R>>,
    owner: Option<O>,
}

impl<A, R, O: 'static> OriginQuarantineEntry for QuarantinedOrigin<A, R, O> {
    fn retention_audit(&self) -> OriginQuarantineRetentionAudit {
        let terminal = self.terminal.load(Ordering::Acquire);
        OriginQuarantineRetentionAudit::retain(
            if terminal {
                SessionCancellationPhase::CancelTerminalAfterTimeout
            } else {
                SessionCancellationPhase::CancelPermanentlyNonterminal
            },
            terminal,
        )
        .with_observed_phase(self.phase)
    }

    fn token(&self) -> OriginQuarantineToken {
        self.token
    }

    fn take_owner(&mut self) -> Option<Box<dyn Any>> {
        self.owner
            .take()
            .map(|owner| Box::new(owner) as Box<dyn Any>)
    }

    fn retains_owner(&self) -> bool {
        self.owner.is_some()
    }
}

impl<A, R, O> Drop for QuarantinedOrigin<A, R, O> {
    fn drop(&mut self) {
        if !self.terminal.load(Ordering::Acquire) {
            std::mem::forget(self.actions.take());
            std::mem::forget(self.owner.take());
        }
    }
}

fn reap_origin_quarantine() {
    ORIGIN_QUARANTINE.with(|entries| {
        entries
            .borrow_mut()
            .retain(|entry| !OriginQuarantineReapAudit(entry.retention_audit()));
    });
}

fn quarantine_origin_until_terminal<T, A, R, O>(
    submission: BlockingSubmission<T>,
    actions: ActionReceiver<A, R>,
    owner: O,
    phase: SessionCancellationPhase,
) where
    T: Send + 'static,
    A: Send + 'static,
    R: Send + 'static,
    O: 'static,
{
    retain_origin_after_cancellation(Some(submission), actions, owner, phase);
}

fn retain_origin_after_cancellation<T, A, R, O>(
    submission: Option<BlockingSubmission<T>>,
    actions: ActionReceiver<A, R>,
    owner: O,
    phase: SessionCancellationPhase,
) -> OriginQuarantineToken
where
    T: Send + 'static,
    A: Send + 'static,
    R: Send + 'static,
    O: 'static,
{
    let token = OriginQuarantineToken(NEXT_ORIGIN_QUARANTINE_TOKEN.fetch_add(1, Ordering::Relaxed));
    let terminal = std::sync::Arc::new(AtomicBool::new(false));
    match submission {
        Some(submission) => {
            let waiter_terminal = std::sync::Arc::clone(&terminal);
            thread::spawn(move || {
                let _ = submission.wait();
                waiter_terminal.store(true, Ordering::Release);
            });
        }
        None => terminal.store(true, Ordering::Release),
    }
    ORIGIN_QUARANTINE.with(|entries| {
        entries.borrow_mut().push(Box::new(QuarantinedOrigin {
            token,
            terminal,
            phase,
            actions: Some(actions),
            owner: Some(owner),
        }));
    });
    LAST_ORIGIN_QUARANTINE_TOKEN.with(|slot| slot.set(Some(token)));
    token
}

pub(crate) fn take_last_origin_quarantine_token() -> Option<OriginQuarantineToken> {
    LAST_ORIGIN_QUARANTINE_TOKEN.with(Cell::take)
}

pub(crate) fn last_origin_quarantine_token() -> Option<OriginQuarantineToken> {
    LAST_ORIGIN_QUARANTINE_TOKEN.with(Cell::get)
}

pub(crate) fn origin_quarantine_is_terminal(token: OriginQuarantineToken) -> Option<bool> {
    ORIGIN_QUARANTINE.with(|entries| {
        entries
            .borrow()
            .iter()
            .find(|entry| entry.token() == token)
            .map(|entry| entry.retention_audit().can_reap())
    })
}

pub(crate) fn origin_quarantine_retains_owner(token: OriginQuarantineToken) -> bool {
    ORIGIN_QUARANTINE.with(|entries| {
        entries
            .borrow()
            .iter()
            .find(|entry| entry.token() == token)
            .is_some_and(|entry| entry.retains_owner())
    })
}

pub(crate) fn reap_origin_quarantine_token(token: OriginQuarantineToken) -> bool {
    ORIGIN_QUARANTINE.with(|entries| {
        let mut entries = entries.borrow_mut();
        let Some(index) = entries.iter().position(|entry| entry.token() == token) else {
            return false;
        };
        if !entries[index].retention_audit().can_reap() {
            return false;
        }
        drop(entries.remove(index));
        LAST_ORIGIN_QUARANTINE_TOKEN.with(|slot| {
            if slot.get() == Some(token) {
                slot.set(None);
            }
        });
        true
    })
}

pub(crate) fn take_origin_quarantine_owner<O: 'static>(token: OriginQuarantineToken) -> Option<O> {
    ORIGIN_QUARANTINE.with(|entries| {
        let mut entries = entries.borrow_mut();
        let index = entries.iter().position(|entry| entry.token() == token)?;
        if !entries[index].retention_audit().can_reap() {
            return None;
        }
        let mut entry = entries.remove(index);
        let owner = entry.take_owner()?.downcast::<O>().ok().map(|owner| *owner);
        drop(entry);
        LAST_ORIGIN_QUARANTINE_TOKEN.with(|slot| {
            if slot.get() == Some(token) {
                slot.set(None);
            }
        });
        owner
    })
}

fn cancel_with_bounded_wait<T, S>(
    mut submission: BlockingSubmission<T>,
    mut wait: S,
) -> Result<Option<BlockingSubmission<T>>, BlockingTaskError>
where
    S: FnMut(),
{
    submission.cancel()?;
    let deadline = Instant::now() + CANCEL_WAIT;
    loop {
        match submission.try_wait() {
            Ok(None) if Instant::now() < deadline => wait(),
            Ok(None) => return Ok(Some(submission)),
            Ok(Some(_)) | Err(_) => return Ok(None),
        }
    }
}

static SIGNAL_FUTURE_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct RuntimeSubmissionEvent {
    id: u64,
    parent_id: Option<u64>,
    generation: u64,
}

#[derive(Default)]
struct RuntimeSubmissionObservation {
    process_id: u32,
    generation: u64,
    events: Vec<RuntimeSubmissionEvent>,
}

static RUNTIME_SUBMISSION_OBSERVATION: OnceLock<Mutex<RuntimeSubmissionObservation>> =
    OnceLock::new();
static RUNTIME_SUBMISSION_OBSERVATION_ENABLED: AtomicBool = AtomicBool::new(false);
const MAX_RUNTIME_SUBMISSION_OBSERVATIONS: usize = 256;

#[derive(Debug)]
enum ProbeAction {
    SetStarted,
    ObserveAffinity,
    RaiseOriginal,
    RunNested,
    ObserveNested,
    CancellationPoint,
}

#[derive(Debug)]
enum ProbeReply {
    Ack,
    Affinity(AffinityReport),
    Nested(NestedReport),
    HandlerFailed,
}

impl WorkerPayload for ProbeAction {}
impl WorkerPayload for ProbeReply {}

#[derive(Debug)]
enum ProbeOutcome {
    Affinity(AffinityReport),
    OriginalRaised,
    Nested(NestedReport),
    HandlerFailed,
    BridgeClosed(BridgeClosed),
    Ready,
}

#[derive(Debug)]
struct AffinityReport {
    observer_ran: bool,
    action_thread: String,
    action_interpreter: usize,
}

#[derive(Debug)]
struct NestedReport {
    value: &'static str,
    generation: u64,
    action_thread: String,
    action_interpreter: usize,
}

pub(crate) struct PythonCallContext {
    origin_thread: thread::ThreadId,
    interpreter: usize,
}

impl PythonCallContext {
    fn capture(py: Python<'_>) -> PyResult<Self> {
        Ok(Self {
            origin_thread: thread::current().id(),
            interpreter: interpreter_identity(py)?,
        })
    }

    fn drive<T, A, R, F>(
        &self,
        py: Python<'_>,
        submission: BlockingSubmission<T>,
        actions: ActionReceiver<A, R>,
        execute: F,
    ) -> PyResult<T>
    where
        T: Send + 'static,
        A: Send + 'static,
        R: Send + 'static,
        F: for<'py> FnMut(Python<'py>, A) -> R,
    {
        self.drive_with_signal_checker(py, submission, actions, execute, |py| py.check_signals())
    }

    fn drive_with_signal_checker<T, A, R, F, S>(
        &self,
        py: Python<'_>,
        mut submission: BlockingSubmission<T>,
        mut actions: ActionReceiver<A, R>,
        mut execute: F,
        mut check_signals: S,
    ) -> PyResult<T>
    where
        T: Send + 'static,
        A: Send + 'static,
        R: Send + 'static,
        F: for<'py> FnMut(Python<'py>, A) -> R,
        S: for<'py> FnMut(Python<'py>) -> PyResult<()>,
    {
        let _active_submission = ActiveOriginSubmissionGuard::enter(submission.id());
        self.ensure_affinity(py)?;
        reap_origin_quarantine();
        loop {
            let task_state =
                match signal_before_task_state(py, &mut check_signals, || submission.try_wait()) {
                    Ok(task_state) => task_state,
                    Err(signal) => {
                        cancel_and_wait(py, &mut submission).map_err(task_error)?;
                        return Err(signal);
                    }
                };
            match task_state {
                Ok(Some(output)) => return Ok(output),
                Ok(None) => {}
                Err(error) => return Err(task_error(error)),
            }

            match py.detach(|| actions.recv_timeout(WAKE_INTERVAL)) {
                Ok(request) => {
                    self.ensure_affinity(py)?;
                    let (action, reply) = request.into_parts();
                    let _ = reply.send(execute(py, action));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    py.detach(|| thread::sleep(WAKE_INTERVAL));
                }
            }
        }
    }

    fn drive_owned_with_signal_checker<T, A, R, O, F, S>(
        &self,
        py: Python<'_>,
        submission: BlockingSubmission<T>,
        actions: ActionReceiver<A, R>,
        owner: O,
        mut execute: F,
        check_signals: S,
    ) -> PyResult<(T, O)>
    where
        T: Send + 'static,
        A: Send + 'static,
        R: Send + 'static,
        O: 'static,
        F: for<'py> FnMut(Python<'py>, A, &mut O) -> R,
        S: for<'py> FnMut(Python<'py>) -> PyResult<()>,
    {
        self.drive_owned_with_reply_disposition_and_signal_checker(
            py,
            submission,
            actions,
            owner,
            move |py, action, owner| ActionReplyDisposition::Reply(execute(py, action, owner)),
            check_signals,
        )
    }

    fn drive_owned_with_reply_disposition_and_signal_checker<T, A, R, O, F, S>(
        &self,
        py: Python<'_>,
        mut submission: BlockingSubmission<T>,
        mut actions: ActionReceiver<A, R>,
        mut owner: O,
        mut execute: F,
        mut check_signals: S,
    ) -> PyResult<(T, O)>
    where
        T: Send + 'static,
        A: Send + 'static,
        R: Send + 'static,
        O: 'static,
        F: for<'py> FnMut(Python<'py>, A, &mut O) -> ActionReplyDisposition<R>,
        S: for<'py> FnMut(Python<'py>) -> PyResult<()>,
    {
        let _active_submission = ActiveOriginSubmissionGuard::enter(submission.id());
        self.ensure_affinity(py)?;
        reap_origin_quarantine();
        let mut cancellation_phase = SessionCancellationPhase::CancelBeforePoll;
        let mut withheld_replies = Vec::new();
        loop {
            let task_state =
                match signal_before_task_state(py, &mut check_signals, || submission.try_wait()) {
                    Ok(task_state) => task_state,
                    Err(signal) => {
                        let submission = cancel_with_bounded_wait(submission, || {
                            py.detach(|| thread::sleep(WAKE_INTERVAL));
                        })
                        .map_err(task_error)?;
                        retain_origin_after_cancellation(
                            submission,
                            actions,
                            owner,
                            cancellation_phase,
                        );
                        return Err(signal);
                    }
                };
            match task_state {
                Ok(Some(output)) => return Ok((output, owner)),
                Ok(None) => {
                    cancellation_phase = SessionCancellationPhase::CancelQueuedBeforeDequeue;
                }
                Err(error) => return Err(task_error(error)),
            }

            match py.detach(|| actions.recv_timeout(WAKE_INTERVAL)) {
                Ok(request) => {
                    self.ensure_affinity(py)?;
                    let (action, reply) = request.into_parts();
                    match execute(py, action, &mut owner) {
                        ActionReplyDisposition::Reply(value) => {
                            let _ = reply.send(value);
                            cancellation_phase = SessionCancellationPhase::CancelReplyObserved;
                        }
                        ActionReplyDisposition::Withhold(phase) => {
                            withheld_replies.push(reply);
                            cancellation_phase = phase;
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    py.detach(|| thread::sleep(WAKE_INTERVAL));
                }
            }
        }
    }

    fn ensure_affinity(&self, py: Python<'_>) -> PyResult<()> {
        if thread::current().id() != self.origin_thread {
            return Err(PyRuntimeError::new_err(
                "Python action moved off its entering OS thread",
            ));
        }
        if interpreter_identity(py)? != self.interpreter {
            return Err(PyRuntimeError::new_err(
                "Python action moved to a different interpreter",
            ));
        }
        Ok(())
    }

    fn thread_label(&self) -> String {
        format!("{:?}", self.origin_thread)
    }
}

fn cancel_and_wait<T>(
    py: Python<'_>,
    submission: &mut BlockingSubmission<T>,
) -> Result<(), BlockingTaskError> {
    submission.cancel()?;
    loop {
        match submission.try_wait() {
            Ok(None) => py.detach(|| thread::sleep(WAKE_INTERVAL)),
            _ => return Ok(()),
        }
    }
}

fn signal_before_task_state<T, S, W>(
    py: Python<'_>,
    check_signals: &mut S,
    task_state: W,
) -> PyResult<Result<Option<T>, BlockingTaskError>>
where
    S: for<'py> FnMut(Python<'py>) -> PyResult<()>,
    W: FnOnce() -> Result<Option<T>, BlockingTaskError>,
{
    check_signals(py)?;
    Ok(task_state())
}

fn interpreter_identity(py: Python<'_>) -> PyResult<usize> {
    Ok(py.import("sys")?.getattr("modules")?.as_ptr() as usize)
}

fn driver() -> PyResult<BlockingRuntimeDriver> {
    BlockingRuntimeDriver::process_local()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

fn submit<F>(driver: &BlockingRuntimeDriver, future: F) -> PyResult<BlockingSubmission<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let submission = driver.submit(future).map_err(driver_error)?;
    let parent_id = submission
        .parent_id()
        .or_else(|| ACTIVE_ORIGIN_SUBMISSION.with(Cell::get));
    if RUNTIME_SUBMISSION_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && let Ok(mut observation) = RUNTIME_SUBMISSION_OBSERVATION
            .get_or_init(|| Mutex::new(RuntimeSubmissionObservation::default()))
            .lock()
        && RUNTIME_SUBMISSION_OBSERVATION_ENABLED.load(Ordering::Acquire)
    {
        if observation.process_id != std::process::id() {
            observation.events.clear();
            RUNTIME_SUBMISSION_OBSERVATION_ENABLED.store(false, Ordering::Release);
        } else {
            if observation.generation != driver.generation() {
                observation.events.clear();
                observation.generation = driver.generation();
            }
            if observation.events.len() < MAX_RUNTIME_SUBMISSION_OBSERVATIONS {
                observation.events.push(RuntimeSubmissionEvent {
                    id: submission.id(),
                    parent_id,
                    generation: driver.generation(),
                });
            }
        }
    }
    crate::sessions::record_public_runtime_submission(submission.id(), parent_id);
    Ok(submission)
}

#[pyfunction]
fn _runtime_submission_trial(py: Python<'_>, operation: &str) -> PyResult<Py<PyAny>> {
    let observation = RUNTIME_SUBMISSION_OBSERVATION
        .get_or_init(|| Mutex::new(RuntimeSubmissionObservation::default()));
    if operation == "reset" {
        let generation = driver()?.generation();
        let mut observation = observation
            .lock()
            .map_err(|_| PyRuntimeError::new_err("runtime submission observation lock poisoned"))?;
        observation.process_id = std::process::id();
        observation.generation = generation;
        observation.events.clear();
        RUNTIME_SUBMISSION_OBSERVATION_ENABLED.store(true, Ordering::Release);
        return Ok(py.None());
    }
    if operation != "snapshot" {
        return Err(PyRuntimeError::new_err(
            "unknown runtime submission observation operation",
        ));
    }
    RUNTIME_SUBMISSION_OBSERVATION_ENABLED.store(false, Ordering::Release);
    let mut observation = observation
        .lock()
        .map_err(|_| PyRuntimeError::new_err("runtime submission observation lock poisoned"))?;
    let events = if observation.process_id == std::process::id() {
        std::mem::take(&mut observation.events)
    } else {
        observation.events.clear();
        Vec::new()
    };
    let result = PyDict::new(py);
    result.set_item(
        "events",
        events
            .iter()
            .map(|event| (event.id, event.parent_id, event.generation))
            .collect::<Vec<_>>(),
    )?;
    result.set_item("outstanding", outstanding_submission_count())?;
    Ok(result.into_any().unbind())
}

fn driver_error(error: BlockingDriverError) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn task_error(error: BlockingTaskError) -> PyErr {
    match error {
        BlockingTaskError::WorkerStopped => {
            PyRuntimeError::new_err("native requests worker stopped unexpectedly")
        }
        _ => PyRuntimeError::new_err(error.to_string()),
    }
}

#[pyfunction]
fn _runtime_generation_trial() -> PyResult<u64> {
    Ok(driver()?.generation())
}

#[pyfunction]
fn _panic_boundary_trial(py: Python<'_>, should_panic: bool) -> PyResult<&'static str> {
    let runtime = driver()?;
    if !should_panic {
        return submit(&runtime, async { "ok" })?.wait().map_err(task_error);
    }

    let generation = runtime.generation();
    let failure = submit(&runtime, async {
        panic!("intentional panic-boundary trial");
    })?
    .wait()
    .expect_err("a panicking runtime task must stop without a result");
    let mapped = task_error(failure);

    let recovery_runtime = driver()?;
    let recovery_generation = recovery_runtime.generation();
    let recovery = submit(&recovery_runtime, async { "ok" })?
        .wait()
        .map_err(task_error)?;
    mapped.value(py).setattr("driver_generation", generation)?;
    mapped
        .value(py)
        .setattr("recovery_generation", recovery_generation)?;
    mapped.value(py).setattr("recovery_result", recovery)?;
    Err(mapped)
}

pub(crate) fn run_with_actions_and_signal_checker<T, A, R, Fut, Build, Execute, CheckSignals>(
    py: Python<'_>,
    build: Build,
    execute: Execute,
    check_signals: CheckSignals,
) -> PyResult<T>
where
    T: Send + 'static,
    A: WorkerPayload,
    R: WorkerPayload,
    Fut: Future<Output = T> + Send + 'static,
    Build: FnOnce(ActionSender<A, R>) -> Fut,
    Execute: for<'py> FnMut(Python<'py>, A) -> R,
    CheckSignals: for<'py> FnMut(Python<'py>) -> PyResult<()>,
{
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel();
    let submission = submit(&runtime, build(actions))?;
    context.drive_with_signal_checker(py, submission, receiver, execute, check_signals)
}

pub(crate) fn run_with_owned_actions<T, A, R, O, Fut, Build, Execute>(
    py: Python<'_>,
    owner: O,
    build: Build,
    execute: Execute,
) -> PyResult<(T, O)>
where
    T: Send + 'static,
    A: WorkerPayload,
    R: WorkerPayload,
    O: 'static,
    Fut: Future<Output = T> + Send + 'static,
    Build: FnOnce(ActionSender<A, R>) -> Fut,
    Execute: for<'py> FnMut(Python<'py>, A, &mut O) -> R,
{
    run_with_owned_actions_and_signal_checker(py, owner, build, execute, |py| py.check_signals())
}

pub(crate) fn run_with_owned_actions_and_reply_disposition<T, A, R, O, Fut, Build, Execute>(
    py: Python<'_>,
    owner: O,
    build: Build,
    execute: Execute,
) -> PyResult<(T, O)>
where
    T: Send + 'static,
    A: WorkerPayload,
    R: WorkerPayload,
    O: 'static,
    Fut: Future<Output = T> + Send + 'static,
    Build: FnOnce(ActionSender<A, R>) -> Fut,
    Execute: for<'py> FnMut(Python<'py>, A, &mut O) -> ActionReplyDisposition<R>,
{
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel();
    let submission = submit(&runtime, build(actions))?;
    context.drive_owned_with_reply_disposition_and_signal_checker(
        py,
        submission,
        receiver,
        owner,
        execute,
        |py| py.check_signals(),
    )
}

pub(crate) fn run_with_owned_actions_and_signal_checker<
    T,
    A,
    R,
    O,
    Fut,
    Build,
    Execute,
    CheckSignals,
>(
    py: Python<'_>,
    owner: O,
    build: Build,
    execute: Execute,
    check_signals: CheckSignals,
) -> PyResult<(T, O)>
where
    T: Send + 'static,
    A: WorkerPayload,
    R: WorkerPayload,
    O: 'static,
    Fut: Future<Output = T> + Send + 'static,
    Build: FnOnce(ActionSender<A, R>) -> Fut,
    Execute: for<'py> FnMut(Python<'py>, A, &mut O) -> R,
    CheckSignals: for<'py> FnMut(Python<'py>) -> PyResult<()>,
{
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel();
    let submission = submit(&runtime, build(actions))?;
    context.drive_owned_with_signal_checker(py, submission, receiver, owner, execute, check_signals)
}

fn bridge_error(error: BridgeClosed) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn internal_probe_error(message: &'static str) -> PyErr {
    PyRuntimeError::new_err(message)
}

fn store_handler_error(slot: &mut Option<PyErr>, error: PyErr) -> ProbeReply {
    if slot.is_none() {
        *slot = Some(error);
    }
    ProbeReply::HandlerFailed
}

fn unexpected_action(slot: &mut Option<PyErr>) -> ProbeReply {
    store_handler_error(
        slot,
        internal_probe_error("runtime probe received an unexpected action"),
    )
}

#[pyfunction]
fn _runtime_affinity_probe(
    py: Python<'_>,
    value: Py<PyAny>,
    action_started: Py<PyAny>,
    observer_ran: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let context = PythonCallContext::capture(py)?;
    let entry_thread = context.thread_label();
    let entry_interpreter = context.interpreter;
    let runtime = driver()?;
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        match actions.request(ProbeAction::SetStarted).await {
            Ok(ProbeReply::Ack) => {}
            Ok(ProbeReply::HandlerFailed) => return ProbeOutcome::HandlerFailed,
            Ok(_) => return ProbeOutcome::HandlerFailed,
            Err(error) => return ProbeOutcome::BridgeClosed(error),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        match actions.request(ProbeAction::ObserveAffinity).await {
            Ok(ProbeReply::Affinity(report)) => ProbeOutcome::Affinity(report),
            Ok(ProbeReply::HandlerFailed) => ProbeOutcome::HandlerFailed,
            Ok(_) => ProbeOutcome::HandlerFailed,
            Err(error) => ProbeOutcome::BridgeClosed(error),
        }
    };
    let submission = submit(&runtime, future)?;
    let mut handler_error = None;
    let outcome = context.drive(py, submission, receiver, |py, action| match action {
        ProbeAction::SetStarted => match action_started.bind(py).call_method0("set") {
            Ok(_) => ProbeReply::Ack,
            Err(error) => store_handler_error(&mut handler_error, error),
        },
        ProbeAction::ObserveAffinity => {
            let observer_ran = match observer_ran.bind(py).call_method0("is_set") {
                Ok(result) => match result.extract::<bool>() {
                    Ok(result) => result,
                    Err(error) => return store_handler_error(&mut handler_error, error),
                },
                Err(error) => return store_handler_error(&mut handler_error, error),
            };
            let action_interpreter = match interpreter_identity(py) {
                Ok(identity) => identity,
                Err(error) => return store_handler_error(&mut handler_error, error),
            };
            ProbeReply::Affinity(AffinityReport {
                observer_ran,
                action_thread: format!("{:?}", thread::current().id()),
                action_interpreter,
            })
        }
        _ => unexpected_action(&mut handler_error),
    })?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    let ProbeOutcome::Affinity(report) = outcome else {
        return Err(outcome_error(outcome));
    };

    let result = PyDict::new(py);
    result.set_item("value", value.bind(py))?;
    result.set_item("observer_ran", report.observer_ran)?;
    result.set_item("entry_thread", entry_thread)?;
    result.set_item("action_thread", report.action_thread)?;
    result.set_item("entry_interpreter", entry_interpreter)?;
    result.set_item("action_interpreter", report.action_interpreter)?;
    Ok(result.into_any().unbind())
}

#[pyfunction]
fn _runtime_error_probe(py: Python<'_>, error: Py<PyAny>) -> PyResult<()> {
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        match actions.request(ProbeAction::RaiseOriginal).await {
            Ok(ProbeReply::Ack) => ProbeOutcome::OriginalRaised,
            Ok(ProbeReply::HandlerFailed) => ProbeOutcome::HandlerFailed,
            Ok(_) => ProbeOutcome::HandlerFailed,
            Err(error) => ProbeOutcome::BridgeClosed(error),
        }
    };
    let submission = submit(&runtime, future)?;
    let mut original_error = None;
    let outcome = context.drive(py, submission, receiver, |py, action| match action {
        ProbeAction::RaiseOriginal => {
            original_error = Some(PyErr::from_value(error.bind(py).clone()));
            ProbeReply::Ack
        }
        _ => unexpected_action(&mut original_error),
    })?;
    match outcome {
        ProbeOutcome::OriginalRaised => Err(original_error
            .take()
            .ok_or_else(|| internal_probe_error("runtime probe lost its original exception"))?),
        other => {
            drop(original_error);
            Err(outcome_error(other))
        }
    }
}

fn nested_probe_report(py: Python<'_>) -> PyResult<NestedReport> {
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let generation = runtime.generation();
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        match actions.request(ProbeAction::ObserveNested).await {
            Ok(ProbeReply::Nested(report)) => ProbeOutcome::Nested(report),
            Ok(ProbeReply::HandlerFailed) => ProbeOutcome::HandlerFailed,
            Ok(_) => ProbeOutcome::HandlerFailed,
            Err(error) => ProbeOutcome::BridgeClosed(error),
        }
    };
    let submission = submit(&runtime, future)?;
    let mut handler_error = None;
    let outcome = context.drive(py, submission, receiver, |py, action| match action {
        ProbeAction::ObserveNested => match interpreter_identity(py) {
            Ok(action_interpreter) => ProbeReply::Nested(NestedReport {
                value: "nested",
                generation,
                action_thread: format!("{:?}", thread::current().id()),
                action_interpreter,
            }),
            Err(error) => store_handler_error(&mut handler_error, error),
        },
        _ => unexpected_action(&mut handler_error),
    })?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    match outcome {
        ProbeOutcome::Nested(report) => Ok(report),
        other => Err(outcome_error(other)),
    }
}

#[pyfunction]
fn _runtime_nested_probe(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let context = PythonCallContext::capture(py)?;
    let entry_thread = context.thread_label();
    let entry_interpreter = context.interpreter;
    let runtime = driver()?;
    let outer_generation = runtime.generation();
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        match actions.request(ProbeAction::RunNested).await {
            Ok(ProbeReply::Nested(report)) => ProbeOutcome::Nested(report),
            Ok(ProbeReply::HandlerFailed) => ProbeOutcome::HandlerFailed,
            Ok(_) => ProbeOutcome::HandlerFailed,
            Err(error) => ProbeOutcome::BridgeClosed(error),
        }
    };
    let submission = submit(&runtime, future)?;
    let mut handler_error = None;
    let outcome = context.drive(py, submission, receiver, |py, action| match action {
        ProbeAction::RunNested => match nested_probe_report(py) {
            Ok(report) => ProbeReply::Nested(report),
            Err(error) => store_handler_error(&mut handler_error, error),
        },
        _ => unexpected_action(&mut handler_error),
    })?;
    if let Some(error) = handler_error {
        return Err(error);
    }
    let ProbeOutcome::Nested(report) = outcome else {
        return Err(outcome_error(outcome));
    };

    let result = PyDict::new(py);
    result.set_item("value", report.value)?;
    result.set_item("outer_generation", outer_generation)?;
    result.set_item("nested_generation", report.generation)?;
    result.set_item("entry_thread", entry_thread)?;
    result.set_item("nested_action_thread", report.action_thread)?;
    result.set_item("entry_interpreter", entry_interpreter)?;
    result.set_item("nested_action_interpreter", report.action_interpreter)?;
    Ok(result.into_any().unbind())
}

struct CancellationProbe;

impl Drop for CancellationProbe {
    fn drop(&mut self) {
        SIGNAL_FUTURE_CANCELLED.store(true, Ordering::Release);
    }
}

#[pyfunction]
fn _runtime_signal_probe(py: Python<'_>) -> PyResult<()> {
    SIGNAL_FUTURE_CANCELLED.store(false, Ordering::Release);
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        let _cancel_probe = CancellationProbe;
        let _keep_actions_open = actions;
        future::pending::<()>().await;
    };
    let submission = submit(&runtime, future)?;
    context.drive(py, submission, receiver, |_py, _action| {
        ProbeReply::HandlerFailed
    })
}

#[pyfunction]
fn _runtime_ready_error_probe(py: Python<'_>, error: Py<PyAny>) -> PyResult<()> {
    let mut task_state_was_polled = false;
    let mut injected_signal = |py: Python<'_>| Err(PyErr::from_value(error.bind(py).clone()));
    let result = signal_before_task_state(py, &mut injected_signal, || {
        task_state_was_polled = true;
        Ok(Some(ProbeOutcome::Ready))
    });
    match result {
        Err(error) if !task_state_was_polled => Err(error),
        Err(_) => Err(internal_probe_error(
            "runtime probe polled a ready result before its signal",
        )),
        Ok(_) => Err(internal_probe_error(
            "runtime probe accepted a ready result over its signal",
        )),
    }
}

#[pyfunction]
fn _runtime_cancel_ownership_probe(
    py: Python<'_>,
    holder: Py<PyAny>,
    error: Py<PyAny>,
) -> PyResult<()> {
    SIGNAL_FUTURE_CANCELLED.store(false, Ordering::Release);
    let origin_owned_value = holder.bind(py).call_method0("pop")?.unbind();
    let context = PythonCallContext::capture(py)?;
    let runtime = driver()?;
    let (actions, receiver) = action_channel::<ProbeAction, ProbeReply>();
    let future = async move {
        let _cancel_probe = CancellationProbe;
        match actions.request(ProbeAction::CancellationPoint).await {
            Ok(ProbeReply::Ack) => future::pending::<ProbeOutcome>().await,
            Ok(ProbeReply::HandlerFailed) => ProbeOutcome::HandlerFailed,
            Ok(_) => ProbeOutcome::HandlerFailed,
            Err(error) => ProbeOutcome::BridgeClosed(error),
        }
    };
    let submission = submit(&runtime, future)?;
    let action_seen = Cell::new(false);
    let mut injected_signal = |py: Python<'_>| {
        if action_seen.get() {
            Err(PyErr::from_value(error.bind(py).clone()))
        } else {
            Ok(())
        }
    };
    let result = context.drive_with_signal_checker(
        py,
        submission,
        receiver,
        |_py, action| match action {
            ProbeAction::CancellationPoint => {
                action_seen.set(true);
                ProbeReply::Ack
            }
            _ => ProbeReply::HandlerFailed,
        },
        &mut injected_signal,
    );
    drop(origin_owned_value);
    result.map(|_outcome| ())
}

#[pyfunction]
fn _runtime_signal_was_cancelled() -> bool {
    SIGNAL_FUTURE_CANCELLED.load(Ordering::Acquire)
}

fn outcome_error(outcome: ProbeOutcome) -> PyErr {
    match outcome {
        ProbeOutcome::BridgeClosed(error) => bridge_error(error),
        ProbeOutcome::HandlerFailed => {
            internal_probe_error("runtime probe action handler failed without a Python exception")
        }
        _ => internal_probe_error("runtime probe returned an unexpected outcome"),
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_runtime_affinity_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_error_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_nested_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_signal_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_ready_error_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_cancel_ownership_probe, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_signal_was_cancelled, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_generation_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_runtime_submission_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_panic_boundary_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        ProbeAction, ProbeReply, SessionCancellationPhase, cancel_with_bounded_wait,
        quarantine_origin_until_terminal, reap_origin_quarantine,
    };
    use crate::bridge::{WorkerPayload, action_channel};
    use requests::blocking::BlockingRuntimeDriver;

    struct TestAction;
    struct TestReply;

    impl WorkerPayload for TestAction {}
    impl WorkerPayload for TestReply {}

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct AffineDropMarker {
        dropped: Arc<AtomicBool>,
        dropped_off_origin: Arc<AtomicBool>,
        origin: thread::ThreadId,
    }

    impl Drop for AffineDropMarker {
        fn drop(&mut self) {
            if thread::current().id() != self.origin {
                self.dropped_off_origin.store(true, Ordering::Release);
            }
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn assert_worker_payload<T: WorkerPayload>() {}

    #[test]
    fn probe_payloads_are_explicit_worker_payloads() {
        assert_worker_payload::<ProbeAction>();
        assert_worker_payload::<ProbeReply>();
    }

    #[test]
    fn bounded_cancel_quarantines_origin_owner_until_terminal_origin_reap() {
        let started = Arc::new(AtomicBool::new(false));
        let worker_done = Arc::new(AtomicBool::new(false));
        let owner_dropped = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let worker_finished = Arc::clone(&worker_done);
        let owner = DropMarker(Arc::clone(&owner_dropped));
        let runtime = BlockingRuntimeDriver::process_local().expect("runtime");
        let (actions, receiver) = action_channel::<TestAction, TestReply>();
        let submission = runtime
            .submit(async move {
                let _keep_actions_open = actions;
                worker_started.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(1_200));
                worker_finished.store(true, Ordering::Release);
            })
            .expect("submission");

        let start_deadline = Instant::now() + Duration::from_secs(1);
        while !started.load(Ordering::Acquire) && Instant::now() < start_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(started.load(Ordering::Acquire));

        let before = Instant::now();
        let live_submission = cancel_with_bounded_wait(submission, || {
            thread::sleep(Duration::from_millis(10));
        })
        .expect("cancel")
        .expect("non-cooperative worker should remain live");
        assert!(before.elapsed() < Duration::from_millis(900));
        assert!(!worker_done.load(Ordering::Acquire));
        assert!(!owner_dropped.load(Ordering::Acquire));

        black_box(&owner);
        quarantine_origin_until_terminal(
            live_submission,
            receiver,
            owner,
            SessionCancellationPhase::CancelQueuedBeforeDequeue,
        );
        assert!(!owner_dropped.load(Ordering::Acquire));

        let terminal_deadline = Instant::now() + Duration::from_secs(2);
        while !worker_done.load(Ordering::Acquire) && Instant::now() < terminal_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(worker_done.load(Ordering::Acquire));
        assert!(!owner_dropped.load(Ordering::Acquire));

        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while !owner_dropped.load(Ordering::Acquire) && Instant::now() < reap_deadline {
            reap_origin_quarantine();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(owner_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn nonterminal_quarantine_does_not_block_origin_thread_teardown() {
        let worker_started = Arc::new(AtomicBool::new(false));
        let worker_done = Arc::new(AtomicBool::new(false));
        let owner_dropped = Arc::new(AtomicBool::new(false));
        let owner_dropped_off_origin = Arc::new(AtomicBool::new(false));
        let thread_worker_started = Arc::clone(&worker_started);
        let thread_worker_done = Arc::clone(&worker_done);
        let thread_owner_dropped = Arc::clone(&owner_dropped);
        let thread_owner_dropped_off_origin = Arc::clone(&owner_dropped_off_origin);
        let (quarantined_sender, quarantined_receiver) = std::sync::mpsc::channel();

        let origin = thread::spawn(move || {
            let owner = AffineDropMarker {
                dropped: thread_owner_dropped,
                dropped_off_origin: thread_owner_dropped_off_origin,
                origin: thread::current().id(),
            };
            let runtime = BlockingRuntimeDriver::process_local().expect("runtime");
            let (actions, receiver) = action_channel::<TestAction, TestReply>();
            let submission = runtime
                .submit(async move {
                    let _keep_actions_open = actions;
                    thread_worker_started.store(true, Ordering::Release);
                    thread::park();
                    thread_worker_done.store(true, Ordering::Release);
                })
                .expect("submission");

            let start_deadline = Instant::now() + Duration::from_secs(1);
            while !worker_started.load(Ordering::Acquire) && Instant::now() < start_deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(worker_started.load(Ordering::Acquire));

            let live_submission = cancel_with_bounded_wait(submission, || {
                thread::sleep(Duration::from_millis(10));
            })
            .expect("cancel")
            .expect("permanently blocked worker must remain nonterminal");
            quarantine_origin_until_terminal(
                live_submission,
                receiver,
                owner,
                SessionCancellationPhase::CancelPermanentlyNonterminal,
            );
            quarantined_sender.send(()).expect("quarantine ready");
        });

        quarantined_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("origin thread must install the quarantine");
        let (teardown_sender, teardown_receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = teardown_sender.send(origin.join().is_ok());
        });

        let teardown = teardown_receiver.recv_timeout(Duration::from_millis(300));
        assert!(!worker_done.load(Ordering::Acquire));
        assert!(!owner_dropped.load(Ordering::Acquire));
        assert!(!owner_dropped_off_origin.load(Ordering::Acquire));
        assert_eq!(
            teardown,
            Ok(true),
            "origin thread teardown must remain bounded for a permanently nonterminal quarantine",
        );
    }
}

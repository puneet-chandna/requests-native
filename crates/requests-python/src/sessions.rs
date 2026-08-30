use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pyo3::exceptions::{
    PyAssertionError, PyNameError, PyNotImplementedError, PyRuntimeError, PyStopIteration,
    PyTimeoutError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyList, PyModule, PyString, PyTuple, PyType};
use pyo3::wrap_pyfunction;

use crate::bridge::{ActionSender, WorkerPayload};
use crate::runtime::{
    last_origin_quarantine_token, origin_quarantine_is_terminal, origin_quarantine_retains_owner,
    run_with_owned_actions, run_with_owned_actions_and_signal_checker, signal_wins_ready_result,
    take_origin_quarantine_owner,
};
use requests::blocking::BlockingRuntimeDriver;
use requests::session_runtime::{
    SessionCheckpoint, SessionPhase, SessionRuntimeHarness, SessionRuntimeHooks,
};

#[derive(Default)]
struct PublicPumpObservation {
    process_id: u32,
    outer_entries: u64,
    outer_exits: u64,
    max_depth: u64,
    adapter_leaf_entries: u64,
    nested_pump_entries: u64,
    submission_ids: Vec<u64>,
    submission_parent_ids: Vec<Option<u64>>,
    adapter_submission_ids: Vec<u64>,
}

static PUBLIC_PUMP_OBSERVATION: OnceLock<Mutex<PublicPumpObservation>> = OnceLock::new();
static PUBLIC_PUMP_OBSERVATION_ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_PUBLIC_SUBMISSION: AtomicU64 = AtomicU64::new(1);
const MAX_PUBLIC_PUMP_OBSERVATIONS: usize = 256;

thread_local! {
    static PUBLIC_PUMP_SUBMISSION: Cell<u64> = const { Cell::new(0) };
    static PUBLIC_PUMP_DEPTH: Cell<u64> = const { Cell::new(0) };
    static PUBLIC_ADAPTER_LEAF_DEPTH: Cell<u64> = const { Cell::new(0) };
}

fn begin_public_pump() -> (u64, u64) {
    let capture = PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire);
    let submission = if capture {
        NEXT_PUBLIC_SUBMISSION.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    };
    let depth = PUBLIC_PUMP_DEPTH.with(|depth| {
        let next = depth.get() + 1;
        depth.set(next);
        next
    });
    let prior_submission = PUBLIC_PUMP_SUBMISSION.with(|current| current.replace(submission));
    if capture
        && let Ok(mut observation) = PUBLIC_PUMP_OBSERVATION
            .get_or_init(|| Mutex::new(PublicPumpObservation::default()))
            .lock()
        && PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
    {
        if observation.process_id == std::process::id() {
            observation.outer_entries += 1;
            observation.max_depth = observation.max_depth.max(depth);
            observation.nested_pump_entries += u64::from(depth > 1);
        } else {
            PUBLIC_PUMP_OBSERVATION_ENABLED.store(false, Ordering::Release);
        }
    }
    (submission, prior_submission)
}

fn end_public_pump(prior_submission: u64) {
    PUBLIC_PUMP_SUBMISSION.with(|current| current.set(prior_submission));
    PUBLIC_PUMP_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    if PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && let Ok(mut observation) = PUBLIC_PUMP_OBSERVATION
            .get_or_init(|| Mutex::new(PublicPumpObservation::default()))
            .lock()
        && PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && observation.process_id == std::process::id()
    {
        observation.outer_exits += 1;
    }
}

pub(crate) struct PublicPumpGuard {
    prior_submission: u64,
}

impl PublicPumpGuard {
    pub(crate) fn enter() -> Self {
        let (_, prior_submission) = begin_public_pump();
        Self { prior_submission }
    }

    pub(crate) fn enter_if_absent() -> Option<Self> {
        (PUBLIC_PUMP_DEPTH.with(Cell::get) == 0).then(Self::enter)
    }
}

impl Drop for PublicPumpGuard {
    fn drop(&mut self) {
        end_public_pump(self.prior_submission);
    }
}

pub(crate) struct PublicAdapterLeafGuard;

impl Drop for PublicAdapterLeafGuard {
    fn drop(&mut self) {
        PUBLIC_ADAPTER_LEAF_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

pub(crate) fn enter_public_adapter_leaf() -> PublicAdapterLeafGuard {
    PUBLIC_ADAPTER_LEAF_DEPTH.with(|depth| depth.set(depth.get() + 1));
    if PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && let Ok(mut observation) = PUBLIC_PUMP_OBSERVATION
            .get_or_init(|| Mutex::new(PublicPumpObservation::default()))
            .lock()
        && PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && observation.process_id == std::process::id()
    {
        observation.adapter_leaf_entries += 1;
    }
    PublicAdapterLeafGuard
}

pub(crate) fn record_public_runtime_submission(id: u64, parent_id: Option<u64>) {
    if !PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        || PUBLIC_PUMP_SUBMISSION.with(Cell::get) == 0
    {
        return;
    }
    if let Ok(mut observation) = PUBLIC_PUMP_OBSERVATION
        .get_or_init(|| Mutex::new(PublicPumpObservation::default()))
        .lock()
        && PUBLIC_PUMP_OBSERVATION_ENABLED.load(Ordering::Acquire)
        && observation.process_id == std::process::id()
        && observation.submission_ids.len() < MAX_PUBLIC_PUMP_OBSERVATIONS
    {
        observation.submission_ids.push(id);
        observation.submission_parent_ids.push(parent_id);
        if PUBLIC_ADAPTER_LEAF_DEPTH.with(Cell::get) != 0 {
            observation.adapter_submission_ids.push(id);
        }
    }
}

#[derive(Clone, Debug)]
enum SessionHarnessAction {
    Checkpoint(SessionCheckpoint),
    Cancellation(RuntimeCancellationPhase),
    LoopbackRequest(Vec<u8>),
    LoopbackPartial { declared: u64, bytes: Vec<u8> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionHarnessReply {
    Ack,
    Failed,
}

impl WorkerPayload for SessionHarnessAction {}
impl WorkerPayload for SessionHarnessReply {}

#[derive(Clone)]
struct NativeSessionRuntimeHooks {
    actions: ActionSender<SessionHarnessAction, SessionHarnessReply>,
    blocking_phase: SessionPhase,
    evidence: Arc<NativeInterruptEvidence>,
}

impl SessionRuntimeHooks for NativeSessionRuntimeHooks {
    fn checkpoint(&self, checkpoint: SessionCheckpoint) {
        self.evidence.record(checkpoint);
        drop(
            self.actions
                .enqueue(SessionHarnessAction::Checkpoint(checkpoint)),
        );
    }

    fn wait(
        &self,
        checkpoint: SessionCheckpoint,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let actions = self.actions.clone();
        let blocking_phase = self.blocking_phase;
        self.evidence.record(checkpoint);
        Box::pin(async move {
            let _ = actions
                .request(SessionHarnessAction::Checkpoint(checkpoint))
                .await;
            if checkpoint.phase == blocking_phase {
                std::future::pending::<()>().await;
            }
        })
    }
}

#[derive(Default)]
struct NativeInterruptEvidence {
    checkpoints: Mutex<Vec<SessionCheckpoint>>,
}

impl NativeInterruptEvidence {
    fn record(&self, checkpoint: SessionCheckpoint) {
        self.checkpoints
            .lock()
            .expect("native interrupt evidence lock poisoned")
            .push(checkpoint);
    }

    fn has_clean_release(&self) -> bool {
        self.checkpoints
            .lock()
            .expect("native interrupt evidence lock poisoned")
            .iter()
            .any(|checkpoint| checkpoint.phase == SessionPhase::PoolReleaseClean)
    }

    fn has_post_head_action(&self) -> bool {
        let checkpoints = self
            .checkpoints
            .lock()
            .expect("native interrupt evidence lock poisoned");
        let Some(head) = checkpoints
            .iter()
            .position(|checkpoint| checkpoint.phase == SessionPhase::ResponseHead)
        else {
            return false;
        };
        checkpoints[head + 1..].iter().any(|checkpoint| {
            matches!(
                checkpoint.phase,
                SessionPhase::ResponseRemainder | SessionPhase::PoolReleaseClean
            )
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GenerationId(u64);

impl GenerationId {
    fn checked(value: u64) -> Result<Self, &'static str> {
        checked_number(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Sequence(u64);

impl Sequence {
    fn checked(value: u64) -> Result<Self, &'static str> {
        checked_number(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CorrelationId(u64);

impl CorrelationId {
    fn checked(value: u64) -> Result<Self, &'static str> {
        checked_number(value).map(Self)
    }
}

macro_rules! session_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct $name {
            value: u64,
            generation: GenerationId,
        }

        impl $name {
            fn checked(value: u64, generation: GenerationId) -> Result<Self, &'static str> {
                checked_number(value).map(|value| Self { value, generation })
            }

            fn validate_generation(self, generation: GenerationId) -> Result<(), &'static str> {
                if self.generation == generation {
                    Ok(())
                } else {
                    Err("stale generation")
                }
            }
        }
    };
}

session_id!(RequestId);
session_id!(ResponseId);
session_id!(AdapterId);
session_id!(JarId);
session_id!(HookId);
session_id!(AuthId);
session_id!(CursorId);
session_id!(OpaqueValueId);
session_id!(UrlId);
session_id!(HeadersId);

impl ResponseId {
    fn try_from_request(_value: RequestId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl AdapterId {
    fn try_from_response(_value: ResponseId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl JarId {
    fn try_from_adapter(_value: AdapterId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl HookId {
    fn try_from_jar(_value: JarId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl AuthId {
    fn try_from_hook(_value: HookId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl CursorId {
    fn try_from_auth(_value: AuthId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

impl OpaqueValueId {
    fn try_from_cursor(_value: CursorId) -> Result<Self, &'static str> {
        Err("wrong ID category")
    }
}

fn checked_number(value: u64) -> Result<u64, &'static str> {
    if value == u64::MAX {
        Err("identifier allocation overflow")
    } else {
        Ok(value)
    }
}

#[derive(Debug)]
struct SessionIdAllocator {
    next: u64,
}

impl SessionIdAllocator {
    fn checked(next: u64) -> Result<Self, &'static str> {
        checked_number(next).map(|next| Self { next })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GlobalAuthority {
    Sessions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MethodId {
    Get,
}

#[derive(Debug)]
enum SessionAction {
    ReadGlobal {
        authority: GlobalAuthority,
        generation: GenerationId,
        sequence: Sequence,
    },
    ReadBody {
        request_id: RequestId,
        generation: GenerationId,
        sequence: Sequence,
    },
    SendCustomAdapter {
        adapter_id: AdapterId,
        request_id: RequestId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
    DispatchHook {
        hook_id: HookId,
        response_id: ResponseId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
    RunAuth {
        auth_id: AuthId,
        request_id: RequestId,
        generation: GenerationId,
        sequence: Sequence,
    },
    ExtractCookies {
        jar_id: JarId,
        request_id: RequestId,
        response_id: ResponseId,
        generation: GenerationId,
        sequence: Sequence,
    },
    NestedSubmit {
        request_id: RequestId,
        generation: GenerationId,
        parent_correlation: CorrelationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
}

#[derive(Debug)]
enum SessionReply {
    Scalar {
        value: OpaqueValueId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
    Response {
        response_id: ResponseId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
    Nested {
        request_id: RequestId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
    Raised {
        error_id: OpaqueValueId,
        generation: GenerationId,
        correlation: CorrelationId,
        sequence: Sequence,
    },
}

#[derive(Debug)]
struct NativeTransfer {
    method: MethodId,
    url: UrlId,
    headers: HeadersId,
    body_id: OpaqueValueId,
    adapter_id: AdapterId,
    generation: GenerationId,
    correlation: CorrelationId,
}

impl WorkerPayload for SessionAction {}
impl WorkerPayload for SessionReply {}
impl WorkerPayload for NativeTransfer {}

struct OriginSessionOwner {
    subject: Py<PyAny>,
    operation: String,
    plans: Vec<OriginPlan>,
    pending_error: Option<PyErr>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

struct OriginSessionDestructor {
    _not_send_or_sync: PhantomData<Rc<()>>,
}

struct SessionSubmission {
    actions: Vec<(usize, SessionAction)>,
}

struct SessionExecutor;
struct SessionFinalizer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeOperation {
    Affinity,
    PayloadBoundary,
    NestedRequest,
    FirstError,
    AdapterInterruptConnect,
    AdapterInterruptResponseHead,
    AdapterInterruptResponseRead,
    AdapterInterruptUpload,
    CancellationMatrix,
    ForkMatrix,
    SessionIsolation,
    StreamIsolation,
    FinalizeClose,
    PanicRecovery,
    LiveClockMutation,
    SendConcurrentChannel,
    ConnectBlocked,
    ConnectReadyRace,
    InterruptResponseHead,
    InterruptResponseRemainder,
    InterruptOriginUpload,
    CancelBeforePoll,
    CancelQueuedBeforeDequeue,
    CancelReplyObserved,
    CancelTerminalAfterTimeout,
    CancelPermanentlyNonterminal,
    RecoverConnectBlocked,
    RecoverConnectReadyRace,
    RecoverResponseHead,
    RecoverResponseRemainder,
    RecoverOriginUpload,
    RecoverCancelBeforePoll,
    RecoverCancelQueuedBeforeDequeue,
    RecoverCancelReplyObserved,
    RecoverCancelTerminalAfterTimeout,
    RecoverCancelPermanentlyNonterminal,
    ForkPrepareImport,
    ForkPrepareDriver,
    ForkPreparePool,
    ForkChildUseAfterImport,
    ForkChildUseAfterDriver,
    ForkChildUseAfterPool,
    ForkParentUseAfterImport,
    ForkParentUseAfterDriver,
    ForkParentUseAfterPool,
    ForkChildCleanupAfterImport,
    ForkChildCleanupAfterDriver,
    ForkChildCleanupAfterPool,
    MultiSessionIsolation,
    OutstandingStreamIsolation,
    ValidateStalePid,
    ValidateStaleGeneration,
    ValidateInheritedPool,
    ValidateReleasedLease,
    ValidateDuplicateCorrelation,
    FinalizeTerminal,
    FinalizePermanentlyNonterminal,
    InjectNativeWorkerPanic,
    RecoverNativeWorkerPanic,
    AwaitLiveAuthority,
    ConcurrentChannelReply,
    ConcurrentChannelFail,
    ValidateTerminalNonterminalConflict,
    ValidatePythonPanicPayload,
    ValidateStaleCallable,
}

impl RuntimeOperation {
    fn parse(scenario: &Bound<'_, PyAny>) -> PyResult<Self> {
        let operation: String = runtime_scenario_item(scenario, "operation")?
            .ok_or_else(|| PyValueError::new_err("missing runtime operation"))?
            .extract()?;
        match operation.as_str() {
            "diff-affinity-pipeline" => Ok(Self::Affinity),
            "diff-sealed-payload-roundtrip" => Ok(Self::PayloadBoundary),
            "diff-nested-reentrant-send" => Ok(Self::NestedRequest),
            "diff-exact-exception-stop-recover" => Ok(Self::FirstError),
            "diff-interrupt-connect" => Ok(Self::AdapterInterruptConnect),
            "diff-interrupt-response-head" => Ok(Self::AdapterInterruptResponseHead),
            "diff-interrupt-response-read" => Ok(Self::AdapterInterruptResponseRead),
            "diff-interrupt-upload" => Ok(Self::AdapterInterruptUpload),
            "diff-cancellation-quarantine-matrix" => Ok(Self::CancellationMatrix),
            "diff-fork-session-roundtrip" => Ok(Self::ForkMatrix),
            "diff-multi-session-peer-survival" => Ok(Self::SessionIsolation),
            "diff-outstanding-stream-peer-clear" => Ok(Self::StreamIsolation),
            "diff-idempotent-session-close" => Ok(Self::FinalizeClose),
            "diff-panic-translate-recover" => Ok(Self::PanicRecovery),
            "diff-send-reload-preferred-clock" => Ok(Self::LiveClockMutation),
            "diff-correlated-adapter-send" => Ok(Self::SendConcurrentChannel),
            "strict-connect-blocked" => Ok(Self::ConnectBlocked),
            "strict-connect-ready-race" => Ok(Self::ConnectReadyRace),
            "strict-interrupt-response-head" => Ok(Self::InterruptResponseHead),
            "strict-interrupt-response-remainder" => Ok(Self::InterruptResponseRemainder),
            "strict-interrupt-origin-upload" => Ok(Self::InterruptOriginUpload),
            "strict-cancel-before-poll" => Ok(Self::CancelBeforePoll),
            "strict-cancel-queued-before-dequeue" => Ok(Self::CancelQueuedBeforeDequeue),
            "strict-cancel-reply-observed" => Ok(Self::CancelReplyObserved),
            "strict-cancel-terminal-after-timeout" => Ok(Self::CancelTerminalAfterTimeout),
            "strict-cancel-permanently-nonterminal" => Ok(Self::CancelPermanentlyNonterminal),
            "strict-recover-connect-blocked" => Ok(Self::RecoverConnectBlocked),
            "strict-recover-connect-ready-race" => Ok(Self::RecoverConnectReadyRace),
            "strict-recover-response-head" => Ok(Self::RecoverResponseHead),
            "strict-recover-response-remainder" => Ok(Self::RecoverResponseRemainder),
            "strict-recover-origin-upload" => Ok(Self::RecoverOriginUpload),
            "strict-recover-cancel-before-poll" => Ok(Self::RecoverCancelBeforePoll),
            "strict-recover-cancel-queued-before-dequeue" => {
                Ok(Self::RecoverCancelQueuedBeforeDequeue)
            }
            "strict-recover-cancel-reply-observed" => Ok(Self::RecoverCancelReplyObserved),
            "strict-recover-cancel-terminal-after-timeout" => {
                Ok(Self::RecoverCancelTerminalAfterTimeout)
            }
            "strict-recover-cancel-permanently-nonterminal" => {
                Ok(Self::RecoverCancelPermanentlyNonterminal)
            }
            "strict-fork-prepare-import" => Ok(Self::ForkPrepareImport),
            "strict-fork-prepare-driver" => Ok(Self::ForkPrepareDriver),
            "strict-fork-prepare-pool" => Ok(Self::ForkPreparePool),
            "strict-fork-child-use-after-import" => Ok(Self::ForkChildUseAfterImport),
            "strict-fork-child-use-after-driver" => Ok(Self::ForkChildUseAfterDriver),
            "strict-fork-child-use-after-pool" => Ok(Self::ForkChildUseAfterPool),
            "strict-fork-parent-use-after-import" => Ok(Self::ForkParentUseAfterImport),
            "strict-fork-parent-use-after-driver" => Ok(Self::ForkParentUseAfterDriver),
            "strict-fork-parent-use-after-pool" => Ok(Self::ForkParentUseAfterPool),
            "strict-fork-child-cleanup-after-import" => Ok(Self::ForkChildCleanupAfterImport),
            "strict-fork-child-cleanup-after-driver" => Ok(Self::ForkChildCleanupAfterDriver),
            "strict-fork-child-cleanup-after-pool" => Ok(Self::ForkChildCleanupAfterPool),
            "strict-multi-session-isolation" => Ok(Self::MultiSessionIsolation),
            "strict-outstanding-stream-isolation" => Ok(Self::OutstandingStreamIsolation),
            "strict-validate-stale-pid" => Ok(Self::ValidateStalePid),
            "strict-validate-stale-generation" => Ok(Self::ValidateStaleGeneration),
            "strict-validate-inherited-pool" => Ok(Self::ValidateInheritedPool),
            "strict-validate-released-lease" => Ok(Self::ValidateReleasedLease),
            "strict-validate-duplicate-correlation" => Ok(Self::ValidateDuplicateCorrelation),
            "strict-finalize-terminal" => Ok(Self::FinalizeTerminal),
            "strict-finalize-permanently-nonterminal" => Ok(Self::FinalizePermanentlyNonterminal),
            "strict-inject-native-worker-panic" => Ok(Self::InjectNativeWorkerPanic),
            "strict-recover-native-worker-panic" => Ok(Self::RecoverNativeWorkerPanic),
            "strict-await-live-authority" => Ok(Self::AwaitLiveAuthority),
            "strict-concurrent-channel-reply" => Ok(Self::ConcurrentChannelReply),
            "strict-concurrent-channel-fail" => Ok(Self::ConcurrentChannelFail),
            "strict-validate-terminal-nonterminal-conflict" => {
                Ok(Self::ValidateTerminalNonterminalConflict)
            }
            "strict-validate-python-panic-payload" => Ok(Self::ValidatePythonPanicPayload),
            "strict-validate-stale-callable" => Ok(Self::ValidateStaleCallable),
            _ => Err(PyValueError::new_err("unknown runtime operation")),
        }
    }

    fn interrupt_phase(self) -> Option<&'static str> {
        match self {
            Self::ConnectBlocked | Self::RecoverConnectBlocked => Some("connect-wait"),
            Self::ConnectReadyRace | Self::RecoverConnectReadyRace => Some("connect-wait"),
            Self::InterruptResponseHead | Self::RecoverResponseHead => Some("response-head-wait"),
            Self::InterruptResponseRemainder | Self::RecoverResponseRemainder => {
                Some("response-remainder-wait")
            }
            Self::InterruptOriginUpload | Self::RecoverOriginUpload => {
                Some("origin-upload-action-wait")
            }
            Self::CancelBeforePoll | Self::RecoverCancelBeforePoll => Some("before-poll"),
            Self::CancelQueuedBeforeDequeue | Self::RecoverCancelQueuedBeforeDequeue => {
                Some("queued-before-dequeue")
            }
            Self::CancelReplyObserved | Self::RecoverCancelReplyObserved => Some("reply-observed"),
            Self::CancelTerminalAfterTimeout | Self::RecoverCancelTerminalAfterTimeout => {
                Some("terminal-after-timeout")
            }
            Self::CancelPermanentlyNonterminal | Self::RecoverCancelPermanentlyNonterminal => {
                Some("permanently-nonterminal")
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompletionEnvelope {
    generation: GenerationId,
    correlation: CorrelationId,
    sequence: Sequence,
    request_id: OpaqueValueId,
    response_id: OpaqueValueId,
    error_id: Option<OpaqueValueId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionAction {
    CloseOnce,
    PanicSend,
    RecoverySend,
    ClockBeforeAwait,
    ClockAfterAwait,
    NativePanicEntered,
    FinalizationEnter,
    FinalizationAwaitRelease,
    NativeAwaitEntered,
    NativeAwaitComplete,
    NativeChannelEntered { envelope: CompletionEnvelope },
    NativeChannelComplete { envelope: CompletionEnvelope },
    ChannelSend { envelope: CompletionEnvelope },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionReply {
    Ack,
    Failed,
    Channel { envelope: CompletionEnvelope },
}

impl WorkerPayload for CompletionAction {}
impl WorkerPayload for CompletionReply {}
impl WorkerPayload for CompletionEnvelope {}

struct CompletionOwner {
    subject: Py<PyAny>,
    scenario: Option<Py<PyAny>>,
    gates: Option<Py<PyAny>>,
    _retained: Option<Py<PyAny>>,
    value: Option<Py<PyAny>>,
    error: Option<PyErr>,
    before_clock: Option<f64>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

#[derive(Default)]
struct NativeIsolationHooks {
    checkpoints: Mutex<Vec<SessionCheckpoint>>,
}

impl SessionRuntimeHooks for NativeIsolationHooks {
    fn checkpoint(&self, checkpoint: SessionCheckpoint) {
        self.checkpoints
            .lock()
            .expect("native isolation checkpoint lock poisoned")
            .push(checkpoint);
    }
}

impl NativeIsolationHooks {
    fn latest(&self, phase: SessionPhase) -> Option<SessionCheckpoint> {
        self.checkpoints
            .lock()
            .expect("native isolation checkpoint lock poisoned")
            .iter()
            .rev()
            .copied()
            .find(|checkpoint| checkpoint.phase == phase)
    }

    fn snapshot(&self) -> Vec<SessionCheckpoint> {
        self.checkpoints
            .lock()
            .expect("native isolation checkpoint lock poisoned")
            .clone()
    }

    fn release_for(&self, lease: u64) -> Option<SessionCheckpoint> {
        self.snapshot().into_iter().find(|checkpoint| {
            matches!(
                checkpoint.phase,
                SessionPhase::PoolReleaseClean | SessionPhase::PoolReleaseDirty
            ) && checkpoint.lease.map(|identity| identity.get()) == Some(lease)
        })
    }

    fn cleared_connections(&self) -> Vec<u64> {
        self.snapshot()
            .into_iter()
            .filter(|checkpoint| checkpoint.phase == SessionPhase::PoolClear)
            .filter_map(|checkpoint| checkpoint.connection.map(|identity| identity.get()))
            .collect()
    }
}

struct NativeForkResources {
    pid: u32,
    driver: BlockingRuntimeDriver,
    harness: SessionRuntimeHarness,
    hooks: Arc<NativeIsolationHooks>,
    client: Option<requests::Client>,
    live_response: Option<requests::Response>,
    pool_exchange: Option<SessionCheckpoint>,
}

impl NativeForkResources {
    fn fresh() -> PyResult<Self> {
        let driver = BlockingRuntimeDriver::process_local()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        let hooks = Arc::new(NativeIsolationHooks::default());
        let harness = SessionRuntimeHarness::new(hooks.clone());
        Ok(Self {
            pid: std::process::id(),
            driver,
            harness,
            hooks,
            client: None,
            live_response: None,
            pool_exchange: None,
        })
    }
}

thread_local! {
    static NATIVE_FORK_RESOURCES: RefCell<Option<NativeForkResources>> =
        const { RefCell::new(None) };
    static LAST_INTERRUPT_RUNTIME_GENERATION: Cell<Option<u64>> = const { Cell::new(None) };
}

struct RedirectCursorState {
    session: Py<PyAny>,
    response: Py<PyAny>,
    request: Py<PyAny>,
    proxies: Py<PyAny>,
    stream: Py<PyAny>,
    timeout: Py<PyAny>,
    verify: Py<PyAny>,
    cert: Py<PyAny>,
    adapter_kwargs: Py<PyDict>,
    source_builtins: Py<PyAny>,
    capabilities: Py<PyAny>,
    history: Vec<Py<PyAny>>,
    url: Option<Py<PyAny>>,
    previous_fragment: Option<Py<PyAny>>,
    yield_requests: bool,
    created: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedirectCursorTerminal {
    Live,
    Done,
    Errored,
    Closed,
}

struct RedirectCursorInner {
    state: Option<RedirectCursorState>,
    terminal: RedirectCursorTerminal,
    claimed_token: Option<Py<PyAny>>,
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRedirectCursor {
    running: AtomicBool,
    inner: Mutex<RedirectCursorInner>,
    frame_token: Py<PyAny>,
    generation: u64,
}

enum RedirectAdvance {
    Yield(Py<PyAny>),
    Done,
}

impl SessionRedirectCursor {
    fn release_state(py: Python<'_>, state: RedirectCursorState) {
        let RedirectCursorState {
            session,
            response,
            request,
            proxies,
            stream,
            timeout,
            verify,
            cert,
            adapter_kwargs,
            source_builtins,
            capabilities,
            history,
            url,
            previous_fragment,
            yield_requests: _,
            created: _,
        } = state;
        request.drop_ref(py);
        response.drop_ref(py);
        for item in history {
            item.drop_ref(py);
        }
        if let Some(url) = url {
            url.drop_ref(py);
        }
        if let Some(fragment) = previous_fragment {
            fragment.drop_ref(py);
        }
        session.drop_ref(py);
        proxies.drop_ref(py);
        stream.drop_ref(py);
        timeout.drop_ref(py);
        verify.drop_ref(py);
        cert.drop_ref(py);
        adapter_kwargs.drop_ref(py);
        source_builtins.drop_ref(py);
        capabilities.drop_ref(py);
    }

    fn from_invocation<'py>(
        py: Python<'py>,
        source_builtins: &Bound<'py, PyAny>,
        capabilities: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
        kwargs: &Bound<'py, PyDict>,
        generation: GenerationId,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args.cast::<PyTuple>()?;
        let value_or_none = |name: &str| -> PyResult<Py<PyAny>> {
            Ok(kwargs
                .get_item(name)?
                .unwrap_or_else(|| py.None().into_bound(py))
                .unbind())
        };
        let adapter_kwargs = kwargs.copy()?;
        for name in [
            "stream",
            "timeout",
            "verify",
            "cert",
            "proxies",
            "yield_requests",
        ] {
            adapter_kwargs.del_item(name).ok();
        }
        let yield_requests = kwargs
            .get_item("yield_requests")?
            .map(|value| value.is_truthy())
            .transpose()?
            .unwrap_or(false);
        let cursor = Py::new(
            py,
            Self {
                running: AtomicBool::new(false),
                inner: Mutex::new(RedirectCursorInner {
                    state: Some(RedirectCursorState {
                        session: args.get_item(0)?.unbind(),
                        response: args.get_item(1)?.unbind(),
                        request: args.get_item(2)?.unbind(),
                        proxies: value_or_none("proxies")?,
                        stream: kwargs
                            .get_item("stream")?
                            .unwrap_or_else(|| PyBool::new(py, false).to_owned().into_any())
                            .unbind(),
                        timeout: value_or_none("timeout")?,
                        verify: kwargs
                            .get_item("verify")?
                            .unwrap_or_else(|| PyBool::new(py, true).to_owned().into_any())
                            .unbind(),
                        cert: value_or_none("cert")?,
                        adapter_kwargs: adapter_kwargs.unbind(),
                        source_builtins: source_builtins.clone().unbind(),
                        capabilities: capabilities.clone().unbind(),
                        history: Vec::new(),
                        url: None,
                        previous_fragment: None,
                        yield_requests,
                        created: true,
                    }),
                    terminal: RedirectCursorTerminal::Live,
                    claimed_token: None,
                }),
                frame_token: py.import("builtins")?.getattr("object")?.call0()?.unbind(),
                generation: generation.0,
            },
        )?;
        Ok(cursor.into_bound(py).into_any())
    }

    fn lock_inner(&self) -> PyResult<std::sync::MutexGuard<'_, RedirectCursorInner>> {
        self.inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("redirect cursor lock poisoned"))
    }

    fn validate_token(&self, py: Python<'_>, token: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.generation != 0 {
            return Err(PyRuntimeError::new_err("stale redirect cursor generation"));
        }
        let inner = self.lock_inner()?;
        let Some(claimed) = inner.claimed_token.as_ref() else {
            return Err(PyRuntimeError::new_err("unclaimed redirect cursor"));
        };
        if !claimed.bind(py).is(token) {
            return Err(PyRuntimeError::new_err("wrong redirect cursor handle"));
        }
        Ok(())
    }

    fn claim(&self, token: &Bound<'_, PyAny>) -> PyResult<()> {
        let mut inner = self.lock_inner()?;
        if inner.terminal != RedirectCursorTerminal::Live || inner.state.is_none() {
            return Err(PyRuntimeError::new_err(
                "stale or terminated redirect cursor",
            ));
        }
        if inner.claimed_token.is_some() {
            return Err(PyRuntimeError::new_err("redirect cursor already claimed"));
        }
        inner.claimed_token = Some(token.clone().unbind());
        Ok(())
    }

    fn global_name<'py>(
        py: Python<'py>,
        module: &Bound<'py, PyModule>,
        name: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(value) = module.dict().get_item(name)? {
            return Ok(value);
        }
        let builtins = py.import("builtins")?;
        match builtins.getattr(name) {
            Ok(value) => Ok(value),
            Err(_) => {
                let error = PyNameError::new_err(format!("name '{name}' is not defined"));
                error.value(py).setattr("name", name)?;
                Err(error)
            }
        }
    }

    fn content_or_fallback(
        py: Python<'_>,
        module: &Bound<'_, PyModule>,
        response: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let Err(content_error) = response.getattr("content") else {
            return Ok(());
        };
        let resolution = (|| -> PyResult<bool> {
            let base_exception = py.import("builtins")?.getattr("BaseException")?;
            let mut catchers = Vec::with_capacity(3);
            for name in [
                "ChunkedEncodingError",
                "ContentDecodingError",
                "RuntimeError",
            ] {
                let catcher = Self::global_name(py, module, name)?;
                let valid = catcher
                    .cast::<PyType>()
                    .ok()
                    .map(|class| class.is_subclass(&base_exception))
                    .transpose()?
                    .unwrap_or(false);
                if !valid {
                    return Err(PyTypeError::new_err(
                        "catching classes that do not inherit from BaseException is not allowed",
                    ));
                }
                catchers.push(catcher);
            }
            Ok(catchers
                .iter()
                .any(|catcher| content_error.is_instance(py, catcher)))
        })();
        let matches = match resolution {
            Ok(matches) => matches,
            Err(error) => {
                error.set_context(py, Some(content_error));
                return Err(error);
            }
        };
        if !matches {
            return Err(content_error);
        }
        let read_kwargs = PyDict::new(py);
        read_kwargs.set_item("decode_content", false)?;
        if let Err(error) = response
            .getattr("raw")?
            .call_method("read", (), Some(&read_kwargs))
        {
            error.set_context(py, Some(content_error));
            return Err(error);
        }
        Ok(())
    }

    fn declared_capability<'py>(
        capabilities: &Bound<'py, PyAny>,
        name: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        capabilities.get_item(name)?.getattr("value")
    }

    fn payload_method_override<'py>(
        session: &Bound<'py, PyAny>,
        name: &str,
        canonical: &Bound<'py, PyAny>,
        canonical_owner: &Bound<'py, PyAny>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        if let Ok(dictionary) = session.getattr("__dict__") {
            if dictionary.contains(name)? {
                return Ok(Some(session.getattr(name)?));
            }
        }
        for owner in session.get_type().getattr("__mro__")?.try_iter()? {
            let owner = owner?;
            let dictionary = owner.getattr("__dict__")?;
            if dictionary.contains(name)? {
                let declared = dictionary.get_item(name)?;
                return if owner.is(canonical_owner) || declared.is(canonical) {
                    Ok(None)
                } else {
                    Ok(Some(session.getattr(name)?))
                };
            }
        }
        Ok(Some(session.getattr(name)?))
    }

    fn redirect_target_value<'py>(
        py: Python<'py>,
        capabilities: &Bound<'py, PyAny>,
        session: &Bound<'py, PyAny>,
        response: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let canonical = Self::declared_capability(capabilities, "get-redirect-target")?;
        let canonical_owner = Self::declared_capability(capabilities, "redirect-type")?;
        if let Some(method) = Self::payload_method_override(
            session,
            "get_redirect_target",
            &canonical,
            &canonical_owner,
        )? {
            return method.call1((response,));
        }
        if !response.getattr("is_redirect")?.is_truthy()? {
            return Ok(py.None().into_bound(py));
        }
        let location = response.getattr("headers")?.get_item("location")?;
        let encoded = location.call_method1("encode", ("latin1",))?;
        PyModule::import(py, "requests.sessions")?
            .getattr("to_native_string")?
            .call1((encoded, "utf8"))
    }

    fn rebuild_method_value(
        py: Python<'_>,
        capabilities: &Bound<'_, PyAny>,
        session: &Bound<'_, PyAny>,
        request: &Bound<'_, PyAny>,
        response: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let canonical = Self::declared_capability(capabilities, "rebuild-method")?;
        let canonical_owner = Self::declared_capability(capabilities, "redirect-type")?;
        if let Some(method) =
            Self::payload_method_override(session, "rebuild_method", &canonical, &canonical_owner)?
        {
            method.call1((request, response))?;
            return Ok(());
        }
        let module = PyModule::import(py, "requests.sessions")?;
        let original_method = request.getattr("method")?;
        let mut selected = original_method.clone();
        let status = response.getattr("status_code")?;
        if status
            .rich_compare(
                module.getattr("codes")?.getattr("see_other")?,
                pyo3::basic::CompareOp::Eq,
            )?
            .is_truthy()?
            && original_method
                .rich_compare("HEAD", pyo3::basic::CompareOp::Ne)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        let status = response.getattr("status_code")?;
        if status
            .rich_compare(
                module.getattr("codes")?.getattr("found")?,
                pyo3::basic::CompareOp::Eq,
            )?
            .is_truthy()?
            && original_method
                .rich_compare("HEAD", pyo3::basic::CompareOp::Ne)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        let status = response.getattr("status_code")?;
        if status
            .rich_compare(
                module.getattr("codes")?.getattr("moved")?,
                pyo3::basic::CompareOp::Eq,
            )?
            .is_truthy()?
            && original_method
                .rich_compare("POST", pyo3::basic::CompareOp::Eq)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        request.setattr("method", selected)
    }

    fn rebuild_proxies_value<'py>(
        py: Python<'py>,
        capabilities: &Bound<'py, PyAny>,
        session: &Bound<'py, PyAny>,
        request: &Bound<'py, PyAny>,
        proxies: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let canonical = Self::declared_capability(capabilities, "rebuild-proxies")?;
        let canonical_owner = Self::declared_capability(capabilities, "redirect-type")?;
        if let Some(method) =
            Self::payload_method_override(session, "rebuild_proxies", &canonical, &canonical_owner)?
        {
            return method.call1((request, proxies));
        }
        let module = PyModule::import(py, "requests.sessions")?;
        let headers = request.getattr("headers")?;
        let parsed = module
            .getattr("urlparse")?
            .call1((request.getattr("url")?,))?;
        let scheme = parsed.getattr("scheme")?;
        let new_proxies = module.getattr("resolve_proxies")?.call1((
            request,
            proxies,
            session.getattr("trust_env")?,
        ))?;
        if headers.contains("Proxy-Authorization")? {
            headers.del_item("Proxy-Authorization")?;
        }
        let proxy = match new_proxies.get_item(&scheme) {
            Ok(value) => Some(value),
            Err(error) if error.is_instance_of::<pyo3::exceptions::PyKeyError>(py) => None,
            Err(error) => return Err(error),
        };
        if let Some(proxy) = proxy {
            let auth = module.getattr("get_auth_from_url")?.call1((proxy,))?;
            let username = auth.get_item(0)?;
            let password = auth.get_item(1)?;
            if !scheme.call_method1("startswith", ("https",))?.is_truthy()?
                && username.is_truthy()?
                && password.is_truthy()?
            {
                headers.set_item(
                    "Proxy-Authorization",
                    module
                        .getattr("_basic_auth_str")?
                        .call1((username, password))?,
                )?;
            }
        }
        Ok(new_proxies)
    }

    fn rebuild_auth_value(
        py: Python<'_>,
        capabilities: &Bound<'_, PyAny>,
        session: &Bound<'_, PyAny>,
        request: &Bound<'_, PyAny>,
        response: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let canonical = Self::declared_capability(capabilities, "rebuild-auth")?;
        let canonical_owner = Self::declared_capability(capabilities, "redirect-type")?;
        if let Some(method) =
            Self::payload_method_override(session, "rebuild_auth", &canonical, &canonical_owner)?
        {
            method.call1((request, response))?;
            return Ok(());
        }
        let module = PyModule::import(py, "requests.sessions")?;
        let headers = request.getattr("headers")?;
        let original_url = response.getattr("request")?.getattr("url")?;
        let url = request.getattr("url")?;
        if headers.contains("Authorization")? {
            let old = module.getattr("urlparse")?.call1((&original_url,))?;
            let new = module.getattr("urlparse")?.call1((&url,))?;
            let strip = old
                .getattr("hostname")?
                .rich_compare(new.getattr("hostname")?, pyo3::basic::CompareOp::Ne)?
                .is_truthy()?;
            if strip {
                headers.del_item("Authorization")?;
            }
        }
        let new_auth = if session.getattr("trust_env")?.is_truthy()? {
            module.getattr("get_netrc_auth")?.call1((&url,))?
        } else {
            py.None().into_bound(py)
        };
        if !new_auth.is_none() {
            request.call_method1("prepare_auth", (new_auth,))?;
        }
        Ok(())
    }

    fn advance_state(py: Python<'_>, state: &mut RedirectCursorState) -> PyResult<RedirectAdvance> {
        let module = PyModule::import(py, "requests.sessions")?;
        let session = state.session.bind(py);
        let capabilities = state.capabilities.bind(py);
        let mut response = state.response.bind(py).clone();
        let mut request = state.request.bind(py).clone();
        if state.created {
            let url = Self::redirect_target_value(py, capabilities, session, &response)?;
            let parsed = module
                .getattr("urlparse")?
                .call1((request.getattr("url")?,))?;
            state.url = Some(url.unbind());
            state.previous_fragment = Some(parsed.getattr("fragment")?.unbind());
            state.created = false;
        }
        let Some(url_object) = state.url.as_ref() else {
            return Ok(RedirectAdvance::Done);
        };
        let mut url = url_object.bind(py).clone();
        if !url.is_truthy()? {
            state.url = None;
            return Ok(RedirectAdvance::Done);
        }

        let prepared = request.call_method0("copy")?;
        let history = PyList::new(py, state.history.iter().map(|item| item.bind(py)))?;
        response.setattr("history", history)?;
        state.history.push(response.clone().unbind());

        Self::content_or_fallback(py, &module, &response)?;

        let history = response.getattr("history")?;
        let length = Self::global_name(py, &module, "len")?.call1((&history,))?;
        let first_max = session.getattr("max_redirects")?;
        if length
            .rich_compare(&first_max, pyo3::basic::CompareOp::Ge)?
            .is_truthy()?
        {
            let second_max = session.getattr("max_redirects")?;
            let message = format!("Exceeded {} redirects.", second_max.str()?);
            let too_many = Self::global_name(py, &module, "TooManyRedirects")?;
            let exception_kwargs = PyDict::new(py);
            exception_kwargs.set_item("response", &response)?;
            let exception = too_many.call((message,), Some(&exception_kwargs))?;
            return Err(PyErr::from_value(exception));
        }

        response.call_method0("close")?;

        if url.call_method1("startswith", ("//",))?.is_truthy()? {
            let parsed_response = module
                .getattr("urlparse")?
                .call1((response.getattr("url")?,))?;
            let scheme = module
                .getattr("to_native_string")?
                .call1((parsed_response.getattr("scheme")?,))?;
            let pieces = PyList::new(py, [&scheme, &url])?;
            url = ":".into_pyobject(py)?.call_method1("join", (pieces,))?;
        }

        let mut parsed = module.getattr("urlparse")?.call1((&url,))?;
        let fragment = parsed.getattr("fragment")?;
        let previous_fragment = state
            .previous_fragment
            .as_ref()
            .expect("previous fragment is initialized")
            .bind(py);
        if fragment
            .rich_compare("", pyo3::basic::CompareOp::Eq)?
            .is_truthy()?
        {
            if previous_fragment.is_truthy()? {
                let replace_kwargs = PyDict::new(py);
                replace_kwargs.set_item("fragment", previous_fragment)?;
                parsed = parsed.call_method("_replace", (), Some(&replace_kwargs))?;
            }
        } else {
            let fragment = parsed.getattr("fragment")?;
            if fragment.is_truthy()? {
                state.previous_fragment = Some(parsed.getattr("fragment")?.unbind());
            }
        }
        url = parsed.call_method0("geturl")?;

        if !parsed.getattr("netloc")?.is_truthy()? {
            let retained_join = module.getattr("urljoin")?;
            let base = response.getattr("url")?;
            let quoted = module.getattr("requote_uri")?.call1((&url,))?;
            url = retained_join.call1((base, quoted))?;
        } else {
            url = module.getattr("requote_uri")?.call1((&url,))?;
        }
        prepared.setattr("url", module.getattr("to_native_string")?.call1((&url,))?)?;
        Self::rebuild_method_value(py, capabilities, session, &prepared, &response)?;

        let status = response.getattr("status_code")?;
        let temporary = module.getattr("codes")?.getattr("temporary_redirect")?;
        let permanent = module.getattr("codes")?.getattr("permanent_redirect")?;
        let preserve = status
            .rich_compare(&temporary, pyo3::basic::CompareOp::Eq)?
            .is_truthy()?
            || status
                .rich_compare(&permanent, pyo3::basic::CompareOp::Eq)?
                .is_truthy()?;
        if !preserve {
            let headers = prepared.getattr("headers")?;
            for header in ["Content-Length", "Content-Type", "Transfer-Encoding"] {
                headers.call_method1("pop", (header, py.None()))?;
            }
            prepared.setattr("body", py.None())?;
        }

        let headers = prepared.getattr("headers")?;
        headers.call_method1("pop", ("Cookie", py.None()))?;
        let cookie_jar = module
            .getattr("cast")?
            .call1(("CookieJar", prepared.getattr("_cookies")?))?;
        module.getattr("extract_cookies_to_jar")?.call1((
            &cookie_jar,
            &request,
            response.getattr("raw")?,
        ))?;
        module
            .getattr("merge_cookies")?
            .call1((&cookie_jar, session.getattr("cookies")?))?;
        prepared.call_method1("prepare_cookies", (&cookie_jar,))?;

        let proxies = Self::rebuild_proxies_value(
            py,
            capabilities,
            session,
            &prepared,
            state.proxies.bind(py),
        )?;
        state.proxies = proxies.unbind();
        Self::rebuild_auth_value(py, capabilities, session, &prepared, &response)?;

        let body_position = prepared.getattr("_body_position")?;
        let rewindable = if body_position.is_none() {
            false
        } else if headers.contains("Content-Length")? {
            true
        } else {
            headers.contains("Transfer-Encoding")?
        };
        if rewindable {
            module.getattr("rewind_body")?.call1((&prepared,))?;
        }

        state.url = Some(url.unbind());
        if state.yield_requests {
            let yielded = prepared.unbind();
            state.request = yielded.clone_ref(py);
            return Ok(RedirectAdvance::Yield(yielded));
        }

        request = prepared.clone();
        state.request = request.clone().unbind();

        let send_kwargs = state.adapter_kwargs.bind(py).copy()?;
        send_kwargs.set_item("stream", state.stream.bind(py))?;
        send_kwargs.set_item("timeout", state.timeout.bind(py))?;
        send_kwargs.set_item("verify", state.verify.bind(py))?;
        send_kwargs.set_item("cert", state.cert.bind(py))?;
        send_kwargs.set_item("proxies", state.proxies.bind(py))?;
        send_kwargs.set_item("allow_redirects", false)?;
        let canonical_send = Self::declared_capability(capabilities, "send")?;
        let canonical_owner = Self::declared_capability(capabilities, "session-type")?;
        response = match Self::payload_method_override(
            session,
            "send",
            &canonical_send,
            &canonical_owner,
        )? {
            Some(send) => send.call((&request,), Some(&send_kwargs))?,
            None => {
                let nested_args = PyTuple::new(py, [session.as_any(), request.as_any()])?;
                SessionExecutor::session_send_with_builtins(
                    py,
                    state.source_builtins.bind(py),
                    capabilities,
                    nested_args.as_any(),
                    &send_kwargs,
                )?
            }
        };
        module.getattr("extract_cookies_to_jar")?.call1((
            session.getattr("cookies")?,
            &prepared,
            response.getattr("raw")?,
        ))?;
        let next_url = Self::redirect_target_value(py, capabilities, session, &response)?;
        state.response = response.clone().unbind();
        state.url = Some(next_url.unbind());
        Ok(RedirectAdvance::Yield(response.unbind()))
    }

    fn next(&self, py: Python<'_>, token: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        self.validate_token(py, token)?;
        self.advance(py)
    }

    fn advance(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(PyValueError::new_err("generator already executing"));
        }
        let state = {
            let mut inner = self.lock_inner()?;
            inner.state.take()
        };
        let Some(mut state) = state else {
            self.running.store(false, Ordering::Release);
            return Err(PyStopIteration::new_err(()));
        };
        let result = Self::advance_state(py, &mut state);
        let mut inner = self.lock_inner()?;
        match result {
            Ok(RedirectAdvance::Yield(value)) => {
                inner.state = Some(state);
                inner.terminal = RedirectCursorTerminal::Live;
                self.running.store(false, Ordering::Release);
                Ok(value)
            }
            Ok(RedirectAdvance::Done) => {
                inner.terminal = RedirectCursorTerminal::Done;
                self.running.store(false, Ordering::Release);
                Err(PyStopIteration::new_err(()))
            }
            Err(error) => {
                inner.terminal = RedirectCursorTerminal::Errored;
                self.running.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    fn terminate(
        &self,
        py: Python<'_>,
        token: &Bound<'_, PyAny>,
        terminal: RedirectCursorTerminal,
    ) -> PyResult<()> {
        self.validate_token(py, token)?;
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(PyValueError::new_err("generator already executing"));
        }
        let mut inner = self.lock_inner()?;
        let state = inner.state.take();
        inner.terminal = terminal;
        drop(inner);
        if let Some(state) = state {
            Self::release_state(py, state);
        }
        self.running.store(false, Ordering::Release);
        Ok(())
    }

    fn frame<'py>(
        &self,
        py: Python<'py>,
        token: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.validate_token(py, token)?;
        let inner = self.lock_inner()?;
        if inner.state.is_some() && inner.terminal == RedirectCursorTerminal::Live {
            Ok(self.frame_token.clone_ref(py).into_bound(py))
        } else {
            Ok(py.None().into_bound(py))
        }
    }
}

#[pyfunction]
fn _session_redirect_cursor_claim(
    cursor: PyRef<'_, SessionRedirectCursor>,
    token: &Bound<'_, PyAny>,
) -> PyResult<()> {
    cursor.claim(token)
}

#[pyfunction]
fn _session_redirect_cursor_next(
    py: Python<'_>,
    cursor: PyRef<'_, SessionRedirectCursor>,
    token: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    cursor.next(py, token)
}

#[pyfunction]
fn _session_redirect_cursor_close(
    py: Python<'_>,
    cursor: PyRef<'_, SessionRedirectCursor>,
    token: &Bound<'_, PyAny>,
) -> PyResult<()> {
    cursor.terminate(py, token, RedirectCursorTerminal::Closed)
}

#[pyfunction]
fn _session_redirect_cursor_drop(
    py: Python<'_>,
    cursor: PyRef<'_, SessionRedirectCursor>,
    token: &Bound<'_, PyAny>,
) -> PyResult<()> {
    cursor.terminate(py, token, RedirectCursorTerminal::Closed)
}

#[pyfunction]
fn _session_redirect_cursor_frame<'py>(
    py: Python<'py>,
    cursor: PyRef<'py, SessionRedirectCursor>,
    token: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    cursor.frame(py, token)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionCategory {
    Global,
    Body,
    Adapter,
    Hook,
    Auth,
    Cookies,
    Nested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OriginPlan {
    SemanticCall,
    GeneratorInstall,
    MergeSetting,
    MergeSettingOperation,
    MergeSettingSetup,
    MergeHooks,
    MountSetup,
    GetAdapterSetup,
    SetAuth,
    SetTrustEnvironment,
    SetActiveValue,
    CookiePrepare,
    AuthPrepare,
    SettingPrepare,
    PrepareRequest,
    DirectPrepare,
    PrepareRequestSetup,
    SessionRequest,
    Mount,
    GetAdapter,
    EnvironmentSettings,
    EnvironmentOperation,
    EnvironmentProxies,
    EnvironmentCa,
    EnvironmentMerge,
    RebuildProxies,
    RebuildAuth,
    ConstructSetup,
    Construct,
    PickleSetup,
    Pickle,
    ActiveStreamClose,
    ActiveStreamTrailing,
    CloseEnter,
    CloseOperation,
    CloseReuseClose,
    CloseReuseSend,
    RedirectTarget,
    RedirectMethod,
    ResolveRedirectsStart,
    CursorNext,
    CursorClose,
    CursorDrop,
    RedirectUrlCommand,
    RedirectHeaders,
    RedirectHistoryResolve,
    RedirectHistorySend,
    RedirectLimitErrors,
    RedirectGeneratorCommand,
    RedirectResource,
    RedirectCookies,
    RedirectProxyAuthRewind,
    RedirectNestedResend,
    DigestRedirect,
    SessionSend,
}

impl SessionSubmission {
    fn capture(
        subject: &Bound<'_, PyAny>,
        operation: &str,
        generation: GenerationId,
    ) -> PyResult<(Self, Vec<OriginPlan>)> {
        let mut typed_actions = Vec::new();
        let mut plans = Vec::new();
        for (scenario_index, scenario) in subject.try_iter()?.enumerate() {
            let scenario = scenario?;
            let actions = scenario.getattr("actions")?;
            for action in actions.try_iter()? {
                let action = action?;
                let kind: String = action.getattr("kind")?.extract()?;
                let method: String = action.getattr("method")?.extract()?;
                let outcome_is_none = action.getattr("outcome")?.is_none();
                let plan = match (operation, kind.as_str(), method.as_str(), outcome_is_none) {
                    (_, "semantic-call", "install_generator", true) => OriginPlan::GeneratorInstall,
                    ("merge-setting", "semantic-call", "__call__", _) => OriginPlan::MergeSetting,
                    ("merge-setting", "call", "__call__", false) => {
                        OriginPlan::MergeSettingOperation
                    }
                    ("merge-setting", "call", "__call__", true) => OriginPlan::MergeSettingSetup,
                    ("merge-hooks", "semantic-call", "__call__", _) => OriginPlan::MergeHooks,
                    ("mount", "call", "__call__", true) => OriginPlan::MountSetup,
                    ("get-adapter", "call", "__call__", true) => OriginPlan::GetAdapterSetup,
                    ("prepare-request-auth", "set-attr", "auth", _) => OriginPlan::SetAuth,
                    ("prepare-request-auth", "set-attr", "trust_env", _) => {
                        OriginPlan::SetTrustEnvironment
                    }
                    ("prepare-request-cookies", "set-attr", "value", _) => {
                        OriginPlan::SetActiveValue
                    }
                    ("prepare-request-cookies", "call", "__call__", _) => {
                        OriginPlan::PrepareRequestSetup
                    }
                    ("prepare-request-cookies", "call", "prepare_request", _) => {
                        OriginPlan::CookiePrepare
                    }
                    ("prepare-request-auth", "call", "prepare_request", _) => {
                        OriginPlan::AuthPrepare
                    }
                    ("prepare-request-settings", "call", "prepare_request", _) => {
                        OriginPlan::SettingPrepare
                    }
                    ("prepare-request", "call", "prepare_request", _) => OriginPlan::PrepareRequest,
                    ("prepare-request", "call", "__call__", true) => {
                        OriginPlan::PrepareRequestSetup
                    }
                    ("prepare-request", "call", "__call__", false) => {
                        OriginPlan::PrepareRequestSetup
                    }
                    ("prepare-request", "call", "prepare", _) => OriginPlan::PrepareRequest,
                    ("session-request", "call", "prepare_request", _) => OriginPlan::DirectPrepare,
                    ("session-request", "call", "__call__", _) => OriginPlan::PrepareRequestSetup,
                    ("session-request", "semantic-call", "__call__", _) => {
                        OriginPlan::SessionRequest
                    }
                    ("mount", "call", "mount", _) => OriginPlan::Mount,
                    ("get-adapter", "call", "get_adapter", _) => OriginPlan::GetAdapter,
                    ("environment", "call", "merge_environment_settings", _) => {
                        OriginPlan::EnvironmentSettings
                    }
                    ("environment", "call", "__call__", _) => OriginPlan::EnvironmentOperation,
                    ("environment-proxies", "call", "__call__", _) => {
                        OriginPlan::EnvironmentProxies
                    }
                    ("environment-ca", "call", "__call__", _) => OriginPlan::EnvironmentCa,
                    ("environment-merge", "call", "__call__", _) => OriginPlan::EnvironmentMerge,
                    ("rebuild-proxies", "call", "__call__", _) => OriginPlan::RebuildProxies,
                    ("rebuild-auth", "call", "__call__", _) => OriginPlan::RebuildAuth,
                    ("construct", "call", "__call__", true) => OriginPlan::ConstructSetup,
                    ("construct", "call", "__call__", false) => OriginPlan::Construct,
                    ("pickle", "call", "__call__", true) => OriginPlan::PickleSetup,
                    ("pickle", "call", "__call__", false) => OriginPlan::Pickle,
                    ("active-stream-close", "call", "__call__", _) => OriginPlan::ActiveStreamClose,
                    ("active-stream-close", "call", "read", _) => OriginPlan::ActiveStreamTrailing,
                    ("active-stream-close", "call", "close", _) => OriginPlan::ActiveStreamTrailing,
                    ("close", "call", "__enter__", _) => OriginPlan::CloseEnter,
                    ("close", "call", "__call__", _) => OriginPlan::CloseOperation,
                    ("close-reuse", "call", "close", _) => OriginPlan::CloseReuseClose,
                    ("close-reuse", "call", "send", _) => OriginPlan::CloseReuseSend,
                    ("redirect-target", "native-call", "__call__", false) => {
                        OriginPlan::RedirectTarget
                    }
                    ("redirect-method", "native-call", "__call__", false) => {
                        OriginPlan::RedirectMethod
                    }
                    ("redirect-url", "native-call", "construct", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-url", "semantic-call", "resume", _)
                    | ("redirect-url", "semantic-call", "resume_terminal", _)
                    | ("redirect-url", "semantic-call", "close", _) => {
                        OriginPlan::RedirectUrlCommand
                    }
                    ("redirect-url", "semantic-call", "mutate", _) => OriginPlan::SemanticCall,
                    ("redirect-url", "semantic-call", "probe_terminal", false) => {
                        OriginPlan::SemanticCall
                    }
                    ("redirect-headers", "native-call", "__call__", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-history", "native-call", "resolve_redirects", false) => {
                        OriginPlan::RedirectHistoryResolve
                    }
                    ("redirect-history", "native-call", "send", false) => {
                        OriginPlan::RedirectHistorySend
                    }
                    ("redirect-history", "semantic-call", "collect_generator", false) => {
                        OriginPlan::SemanticCall
                    }
                    ("redirect-limit-errors", "native-call", "__call__", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-generator", "native-call", "construct", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-generator", "semantic-call", "resume", _)
                    | ("redirect-generator", "semantic-call", "resume_terminal", _)
                    | ("redirect-generator", "semantic-call", "release_current", _)
                    | ("redirect-generator", "semantic-call", "mutate", _)
                    | ("redirect-generator", "semantic-call", "close", _)
                    | ("redirect-generator", "semantic-call", "drop", _)
                    | ("redirect-generator", "semantic-call", "collect", _)
                    | ("redirect-generator", "semantic-call", "close_worker", _)
                    | ("redirect-generator", "semantic-call", "drop_worker", _)
                    | ("redirect-generator", "semantic-call", "resume_worker", _)
                    | ("redirect-generator", "semantic-call", "concurrent", _)
                    | ("redirect-generator", "semantic-call", "resume_error", _) => {
                        OriginPlan::SemanticCall
                    }
                    ("redirect-generator", "native-call", "send_no_redirect", false) => {
                        OriginPlan::SessionSend
                    }
                    ("redirect-generator", "semantic-call", "probe_terminal", false) => {
                        OriginPlan::SemanticCall
                    }
                    ("redirect-resource", "native-call", "__call__", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-cookies", "native-call", "construct", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-proxy-auth-rewind", "native-call", "__call__", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-nested-resend", "native-call", "construct", false) => {
                        OriginPlan::ResolveRedirectsStart
                    }
                    ("redirect-nested-resend", "semantic-call", "collect_generator", false) => {
                        OriginPlan::SemanticCall
                    }
                    ("digest-redirect", "native-call", "__call__", false) => {
                        OriginPlan::SessionSend
                    }
                    ("send", "native-call", "__call__", false)
                    | ("send-error", "native-call", "__call__", false)
                    | ("send-reentrant-mutation", "native-call", "__call__", false) => {
                        OriginPlan::SessionSend
                    }
                    (operation, "semantic-call", _, _)
                        if matches!(
                            operation,
                            "redirect-target"
                                | "redirect-method"
                                | "redirect-url"
                                | "redirect-headers"
                                | "redirect-history"
                                | "redirect-limit-errors"
                                | "redirect-generator"
                                | "redirect-resource"
                                | "redirect-cookies"
                                | "redirect-proxy-auth-rewind"
                                | "redirect-nested-resend"
                                | "digest-redirect"
                                | "send"
                                | "send-error"
                                | "send-reentrant-mutation"
                        ) =>
                    {
                        OriginPlan::SemanticCall
                    }
                    (operation, "semantic-call", "collect", false)
                        if matches!(operation, "redirect-history") =>
                    {
                        OriginPlan::SemanticCall
                    }
                    (operation, "next", "__next__", _)
                        if matches!(
                            operation,
                            "redirect-url"
                                | "redirect-headers"
                                | "redirect-limit-errors"
                                | "redirect-resource"
                                | "redirect-cookies"
                                | "redirect-proxy-auth-rewind"
                                | "redirect-nested-resend"
                                | "redirect-generator"
                        ) =>
                    {
                        OriginPlan::CursorNext
                    }
                    (operation, "close", "close", _)
                        if matches!(
                            operation,
                            "redirect-url"
                                | "redirect-headers"
                                | "redirect-limit-errors"
                                | "redirect-history"
                                | "redirect-resource"
                                | "redirect-cookies"
                                | "redirect-proxy-auth-rewind"
                                | "redirect-nested-resend"
                                | "redirect-generator"
                        ) =>
                    {
                        OriginPlan::CursorClose
                    }
                    ("redirect-generator", "drop", _, _) => OriginPlan::CursorDrop,
                    _ => {
                        return Err(PyNotImplementedError::new_err(
                            "session operation is not implemented",
                        ));
                    }
                };
                let raw_sequence = plans.len() as u64;
                let sequence = Sequence::checked(raw_sequence).map_err(PyRuntimeError::new_err)?;
                let correlation =
                    CorrelationId::checked(raw_sequence).map_err(PyRuntimeError::new_err)?;
                let request_id = RequestId::checked(raw_sequence, generation)
                    .map_err(PyRuntimeError::new_err)?;
                let category = category_for_operation(plan);
                let typed = match category {
                    ActionCategory::Global => SessionAction::ReadGlobal {
                        authority: GlobalAuthority::Sessions,
                        generation,
                        sequence,
                    },
                    ActionCategory::Body => SessionAction::ReadBody {
                        request_id,
                        generation,
                        sequence,
                    },
                    ActionCategory::Adapter => SessionAction::SendCustomAdapter {
                        adapter_id: AdapterId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        request_id,
                        generation,
                        correlation,
                        sequence,
                    },
                    ActionCategory::Hook => SessionAction::DispatchHook {
                        hook_id: HookId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        response_id: ResponseId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        generation,
                        correlation,
                        sequence,
                    },
                    ActionCategory::Auth => SessionAction::RunAuth {
                        auth_id: AuthId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        request_id,
                        generation,
                        sequence,
                    },
                    ActionCategory::Cookies => SessionAction::ExtractCookies {
                        jar_id: JarId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        request_id,
                        response_id: ResponseId::checked(raw_sequence, generation)
                            .map_err(PyRuntimeError::new_err)?,
                        generation,
                        sequence,
                    },
                    ActionCategory::Nested => SessionAction::NestedSubmit {
                        request_id,
                        generation,
                        parent_correlation: correlation,
                        correlation,
                        sequence,
                    },
                };
                plans.push(plan);
                // The worker receives only typed IDs and sequencing metadata.
                // Python objects remain in OriginSessionOwner.
                typed_actions.push((scenario_index, typed));
            }
        }
        Ok((
            Self {
                actions: typed_actions,
            },
            plans,
        ))
    }

    async fn submit(
        self,
        actions: crate::bridge::ActionSender<SessionAction, SessionReply>,
    ) -> Result<(), crate::bridge::BridgeClosed> {
        let mut failed_scenario = None;
        for (scenario_index, action) in self.actions {
            if failed_scenario == Some(scenario_index) {
                continue;
            }
            let reply = actions.request(action).await?;
            if matches!(reply, SessionReply::Raised { .. }) {
                failed_scenario = Some(scenario_index);
            }
        }
        Ok(())
    }
}

impl SessionExecutor {
    fn native_root<'py>(receiver: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        receiver.getattr("root")?.getattr("value")
    }

    fn native_capabilities<'py>(receiver: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        receiver.getattr("capabilities")
    }

    fn attach_native_traceback(
        py: Python<'_>,
        receiver: &Bound<'_, PyAny>,
        error: PyErr,
    ) -> PyResult<PyErr> {
        if error.traceback(py).is_some() {
            return Ok(error);
        }
        let capabilities = Self::native_capabilities(receiver)?;
        let attacher =
            SessionRedirectCursor::declared_capability(&capabilities, "traceback-attacher")?;
        let original = error.value(py);
        let attached = attacher.call1((original,))?;
        if !attached.is(original) {
            return Err(PyRuntimeError::new_err(
                "traceback attacher changed exception identity",
            ));
        }
        Ok(PyErr::from_value(attached))
    }

    fn finalize_origin_traceback(py: Python<'_>, error: PyErr) -> PyResult<PyErr> {
        if error.traceback(py).is_some() {
            return Ok(error);
        }
        let original = error.value(py).clone().unbind();
        let original_cause = original.bind(py).getattr("__cause__")?.unbind();
        let original_context = original.bind(py).getattr("__context__")?.unbind();
        let original_suppressed: bool = original
            .bind(py)
            .getattr("__suppress_context__")?
            .extract()?;
        let raiser = Py::new(
            py,
            SessionRuntimeBareRaiser {
                marker: original.clone_ref(py).into_any(),
            },
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("target", raiser)?;
        let thread = py
            .import("threading")?
            .getattr("Thread")?
            .call((), Some(&kwargs))?;
        let raised = match thread.call_method0("run") {
            Ok(_) => {
                return Err(PyRuntimeError::new_err(
                    "origin traceback finalizer unexpectedly returned",
                ));
            }
            Err(raised) => raised,
        };
        let raised_value = raised.value(py);
        if !raised_value.is(original.bind(py)) {
            return Err(PyRuntimeError::new_err(
                "origin traceback finalizer changed exception identity",
            ));
        }
        if raised.traceback(py).is_none() {
            return Err(PyRuntimeError::new_err(
                "origin traceback finalizer produced no traceback",
            ));
        }
        let cause_unchanged = raised_value
            .getattr("__cause__")?
            .is(original_cause.bind(py));
        let context_unchanged = raised_value
            .getattr("__context__")?
            .is(original_context.bind(py));
        let suppressed_unchanged = raised_value
            .getattr("__suppress_context__")?
            .extract::<bool>()?
            == original_suppressed;
        if !cause_unchanged || !context_unchanged || !suppressed_unchanged {
            return Err(PyRuntimeError::new_err(
                "origin traceback finalizer changed exception metadata",
            ));
        }
        Ok(raised)
    }

    fn execute(
        py: Python<'_>,
        action: SessionAction,
        owner: &mut OriginSessionOwner,
    ) -> SessionReply {
        let (category, generation, sequence) = match action {
            SessionAction::ReadGlobal {
                generation,
                sequence,
                ..
            } => (ActionCategory::Global, generation, sequence),
            SessionAction::ReadBody {
                request_id,
                generation,
                sequence,
            } => {
                if let Err(error) = request_id.validate_generation(generation) {
                    owner.pending_error = Some(PyRuntimeError::new_err(error));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Body, generation, sequence)
            }
            SessionAction::SendCustomAdapter {
                adapter_id,
                request_id,
                generation,
                sequence,
                ..
            } => {
                if adapter_id.validate_generation(generation).is_err()
                    || request_id.validate_generation(generation).is_err()
                {
                    owner.pending_error = Some(PyRuntimeError::new_err("stale generation"));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Adapter, generation, sequence)
            }
            SessionAction::DispatchHook {
                hook_id,
                response_id,
                generation,
                sequence,
                ..
            } => {
                if hook_id.validate_generation(generation).is_err()
                    || response_id.validate_generation(generation).is_err()
                {
                    owner.pending_error = Some(PyRuntimeError::new_err("stale generation"));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Hook, generation, sequence)
            }
            SessionAction::RunAuth {
                auth_id,
                request_id,
                generation,
                sequence,
            } => {
                if auth_id.validate_generation(generation).is_err()
                    || request_id.validate_generation(generation).is_err()
                {
                    owner.pending_error = Some(PyRuntimeError::new_err("stale generation"));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Auth, generation, sequence)
            }
            SessionAction::ExtractCookies {
                jar_id,
                request_id,
                response_id,
                generation,
                sequence,
            } => {
                if jar_id.validate_generation(generation).is_err()
                    || request_id.validate_generation(generation).is_err()
                    || response_id.validate_generation(generation).is_err()
                {
                    owner.pending_error = Some(PyRuntimeError::new_err("stale generation"));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Cookies, generation, sequence)
            }
            SessionAction::NestedSubmit {
                request_id,
                generation,
                sequence,
                ..
            } => {
                if let Err(error) = request_id.validate_generation(generation) {
                    owner.pending_error = Some(PyRuntimeError::new_err(error));
                    return raised_reply(generation, sequence);
                }
                (ActionCategory::Nested, generation, sequence)
            }
        };

        match Self::execute_sequence(py, owner, category, sequence) {
            Ok(false) => SessionReply::Scalar {
                value: OpaqueValueId::checked(sequence.0, generation)
                    .expect("action sequence must be a valid opaque value id"),
                generation,
                correlation: CorrelationId::checked(sequence.0)
                    .expect("action sequence must be a valid correlation id"),
                sequence,
            },
            Ok(true) => raised_reply(generation, sequence),
            Err(error) => {
                owner.pending_error = Some(error);
                raised_reply(generation, sequence)
            }
        }
    }

    fn execute_sequence(
        py: Python<'_>,
        owner: &mut OriginSessionOwner,
        category: ActionCategory,
        sequence: Sequence,
    ) -> PyResult<bool> {
        let mut remaining = sequence.0 as usize;
        let subject = owner.subject.bind(py);
        for scenario in subject.try_iter()? {
            let scenario = scenario?;
            let actions = scenario.getattr("actions")?;
            let action_count = actions.len()?;
            if remaining >= action_count {
                remaining -= action_count;
                continue;
            }
            let action = actions.get_item(remaining)?;
            let plan = owner.plans[sequence.0 as usize];
            if category_for_operation(plan) != category {
                return Err(PyRuntimeError::new_err("session action category mismatch"));
            }
            return Self::execute_origin_plan(py, &owner.operation, plan, &scenario, &action);
        }
        Err(PyRuntimeError::new_err(
            "session action sequence is out of range",
        ))
    }

    fn execute_origin_plan(
        py: Python<'_>,
        operation: &str,
        plan: OriginPlan,
        scenario: &Bound<'_, PyAny>,
        action: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let receiver = action.getattr("receiver")?;
        let args = action.getattr("args")?;
        let kwargs = action.getattr("kwargs")?;
        let capture: bool = action.getattr("capture")?.extract()?;
        let outcome = action.getattr("outcome")?;

        let call_kwargs = PyDict::new(py);
        call_kwargs.call_method1("update", (&kwargs,))?;
        let result = match plan {
            OriginPlan::SemanticCall => receiver
                .getattr("value")?
                .call(args.cast::<PyTuple>()?, Some(&call_kwargs)),
            OriginPlan::GeneratorInstall => Self::install_redirect_cursor(py, &args),
            OriginPlan::MergeSetting | OriginPlan::MergeHooks => receiver
                .getattr("value")?
                .call(args.cast::<PyTuple>()?, Some(&call_kwargs)),
            OriginPlan::MergeSettingOperation => Self::merge_setting_operation(py, &receiver),
            OriginPlan::MergeSettingSetup => {
                receiver.call(args.cast::<PyTuple>()?, Some(&call_kwargs))
            }
            OriginPlan::MountSetup | OriginPlan::GetAdapterSetup => {
                receiver.call(args.cast::<PyTuple>()?, Some(&call_kwargs))
            }
            OriginPlan::SetAuth => receiver
                .setattr("auth", action.getattr("target")?)
                .map(|_| py.None().into_bound(py)),
            OriginPlan::SetTrustEnvironment => receiver
                .setattr("trust_env", action.getattr("target")?)
                .map(|_| py.None().into_bound(py)),
            OriginPlan::SetActiveValue => receiver
                .setattr("value", action.getattr("target")?)
                .map(|_| py.None().into_bound(py)),
            OriginPlan::Mount | OriginPlan::GetAdapter | OriginPlan::EnvironmentSettings => {
                Self::execute_session_operation(py, operation, plan, &receiver, &args)
            }
            OriginPlan::CloseEnter => Ok(receiver.clone()),
            OriginPlan::CloseOperation => Self::execute_close_operation(&receiver),
            OriginPlan::CloseReuseClose => {
                Self::close_session(&receiver).map(|_| py.None().into_bound(py))
            }
            OriginPlan::CloseReuseSend => Self::send_after_close(py, &receiver, &args),
            OriginPlan::EnvironmentOperation => Self::execute_environment_operation(py, &receiver),
            OriginPlan::EnvironmentProxies => Self::environment_proxies(py, &receiver),
            OriginPlan::EnvironmentCa => Self::environment_ca(py, &receiver),
            OriginPlan::EnvironmentMerge => Self::environment_merge(py, &receiver),
            OriginPlan::RebuildProxies => Self::rebuild_proxies(py, &receiver),
            OriginPlan::RebuildAuth => Self::rebuild_auth(py, &receiver),
            OriginPlan::ConstructSetup | OriginPlan::PickleSetup => {
                receiver.call(args.cast::<PyTuple>()?, Some(&call_kwargs))
            }
            OriginPlan::Construct => Self::construct_session(py, &receiver),
            OriginPlan::Pickle => Self::pickle_session(py, &receiver),
            OriginPlan::ActiveStreamClose => Self::active_stream_close(py, &receiver),
            OriginPlan::ActiveStreamTrailing => {
                let method: String = action.getattr("method")?.extract()?;
                receiver
                    .getattr(method)?
                    .call(args.cast::<PyTuple>()?, Some(&call_kwargs))
            }
            OriginPlan::PrepareRequest => Self::prepare_request(py, &receiver, &args),
            OriginPlan::CookiePrepare => Self::cookie_prepare(py, &receiver, &args),
            OriginPlan::AuthPrepare => Self::auth_prepare(py, &receiver, &args),
            OriginPlan::SettingPrepare => Self::setting_prepare(py, &receiver, &args),
            OriginPlan::DirectPrepare => Self::direct_prepare(py, &receiver, &args),
            OriginPlan::PrepareRequestSetup => {
                receiver.call(args.cast::<PyTuple>()?, Some(&call_kwargs))
            }
            OriginPlan::SessionRequest => Self::session_request(py, &args, &call_kwargs),
            OriginPlan::RedirectTarget => Self::redirect_target(py, &args),
            OriginPlan::RedirectMethod => Self::redirect_method(py, &args),
            OriginPlan::ResolveRedirectsStart | OriginPlan::RedirectHistoryResolve => {
                let root = Self::native_root(&receiver)?;
                let capabilities = Self::native_capabilities(&receiver)?;
                SessionRedirectCursor::from_invocation(
                    py,
                    &root.getattr("__builtins__")?,
                    &capabilities,
                    &args,
                    &call_kwargs,
                    GenerationId::checked(0).expect("zero generation"),
                )
            }
            OriginPlan::CursorNext => receiver.call_method0("__next__"),
            OriginPlan::CursorClose => receiver.call_method0("close"),
            OriginPlan::CursorDrop => receiver.call_method0("drop"),
            OriginPlan::RedirectUrlCommand => Self::redirect_url_command(py, &receiver, action),
            OriginPlan::RedirectHeaders => Self::redirect_headers(py, &receiver),
            OriginPlan::RedirectHistorySend => {
                Self::session_send_composed(py, &receiver, &args, &call_kwargs)
            }
            OriginPlan::RedirectLimitErrors => Self::redirect_limit_errors(py, &receiver),
            OriginPlan::RedirectGeneratorCommand => {
                Self::redirect_generator_command(py, &receiver, action)
            }
            OriginPlan::RedirectResource => Self::redirect_resource(py, &receiver),
            OriginPlan::RedirectCookies => Self::redirect_cookies(py, &receiver),
            OriginPlan::RedirectProxyAuthRewind => Self::redirect_proxy_auth_rewind(py, &receiver),
            OriginPlan::RedirectNestedResend => Self::redirect_nested_resend(py, &receiver),
            OriginPlan::DigestRedirect => Self::digest_redirect(py, &receiver),
            OriginPlan::SessionSend => {
                Self::session_send_composed(py, &receiver, &args, &call_kwargs)
            }
        };

        let (tag, value, failed) = match result {
            Ok(value) => ("return", value.unbind(), false),
            Err(error) if capture => {
                let kind: String = action.getattr("kind")?.extract()?;
                let error = if kind == "native-call" {
                    Self::attach_native_traceback(py, &receiver, error)?
                } else {
                    error
                };
                let error = Self::finalize_origin_traceback(py, error)?;
                ("error", error.into_value(py).into_any(), true)
            }
            Err(error) => return Err(error),
        };
        if !outcome.is_none() {
            let holders = scenario.getattr("context")?.getattr("holders")?;
            holders.set_item(
                outcome,
                PyTuple::new(py, [tag.into_pyobject(py)?.into_any().unbind(), value])?,
            )?;
        }
        if failed { Ok(true) } else { Ok(false) }
    }

    fn merge_setting_operation<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let request = invocation.getattr("request")?;
        let session = invocation.getattr("session")?;
        let destination_factory = invocation.getattr("destination_factory")?;
        let session_is_mapping = module
            .getattr("isinstance")?
            .call1((&session, module.getattr("Mapping")?))?
            .is_truthy()?;
        if !session_is_mapping {
            return Ok(request);
        }
        let request_is_mapping = module
            .getattr("isinstance")?
            .call1((&request, module.getattr("Mapping")?))?
            .is_truthy()?;
        if !request_is_mapping {
            return Ok(request);
        }
        let session_pairs = module.getattr("to_key_val_list")?.call1((&session,))?;
        let returned = destination_factory.call1((session_pairs,))?;
        let retained_update = returned.getattr("update")?;
        let request_pairs = module.getattr("to_key_val_list")?.call1((&request,))?;
        retained_update.call1((request_pairs,))?;
        let retained_items = returned.getattr("items")?;
        let mut none_keys = Vec::new();
        for pair in retained_items.call0()?.try_iter()? {
            let pair = pair?;
            if pair.get_item(1)?.is_none() {
                none_keys.push(pair.get_item(0)?.unbind());
            }
        }
        for key in none_keys {
            returned.del_item(key.bind(py))?;
        }
        Ok(returned)
    }

    fn execute_session_operation<'py>(
        py: Python<'py>,
        operation: &str,
        plan: OriginPlan,
        receiver: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        match (operation, plan) {
            ("mount", OriginPlan::Mount) => {
                let prefix = args.get_item(0)?;
                let adapter = args.get_item(1)?;
                let adapters = receiver.getattr("adapters")?;
                adapters.set_item(&prefix, adapter)?;
                let mut keys_to_move = Vec::new();
                let sessions = py.import("requests.sessions")?;
                for key in adapters.try_iter()? {
                    let key = key?;
                    let key_len: usize = sessions.getattr("len")?.call1((&key,))?.extract()?;
                    let prefix_len: usize =
                        sessions.getattr("len")?.call1((&prefix,))?.extract()?;
                    if key_len < prefix_len {
                        keys_to_move.push(key.unbind());
                    }
                }
                for key in keys_to_move {
                    let source = receiver.getattr("adapters")?;
                    let moved = source.call_method1("pop", (key.bind(py),))?;
                    receiver
                        .getattr("adapters")?
                        .set_item(key.bind(py), moved)?;
                }
                Ok(py.None().into_bound(py))
            }
            ("get-adapter", OriginPlan::GetAdapter) => {
                let url = args.get_item(0)?;
                let items = receiver.getattr("adapters")?.call_method0("items")?;
                for pair in items.try_iter()? {
                    let pair = pair?;
                    let prefix = pair.get_item(0)?;
                    let adapter = pair.get_item(1)?;
                    let lowered = url.call_method0("lower")?;
                    let startswith = lowered.getattr("startswith")?;
                    let lowered_prefix = prefix.call_method0("lower")?;
                    if startswith.call1((lowered_prefix,))?.is_truthy()? {
                        return Ok(adapter);
                    }
                }
                let exception = py.import("requests.sessions")?.getattr("InvalidSchema")?;
                let message = format!(
                    "No connection adapters were found for {}",
                    url.repr()?.to_str()?
                );
                Err(PyErr::from_value(exception.call1((message,))?))
            }
            ("environment", OriginPlan::EnvironmentSettings) => {
                Self::merge_environment_settings(py, receiver, args)
            }
            _ => Err(PyRuntimeError::new_err("session operation/action mismatch")),
        }
    }

    fn merge_environment_settings<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let url = args.get_item(0)?;
        let proxies = args.get_item(1)?;
        let stream = args.get_item(2)?;
        let verify = args.get_item(3)?;
        let cert = args.get_item(4)?;
        if !proxies.is_none() {
            let no_proxy = proxies.call_method1("get", ("no_proxy",))?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("no_proxy", no_proxy)?;
            let environment = module
                .getattr("get_environ_proxies")?
                .call((&url,), Some(&kwargs))?;
            for pair in environment.call_method0("items")?.try_iter()? {
                let pair = pair?;
                proxies.call_method1("setdefault", (pair.get_item(0)?, pair.get_item(1)?))?;
            }
        }
        let returned = PyDict::new(py);
        returned.set_item(
            "proxies",
            Self::merge_environment_value(py, &proxies, &session.getattr("proxies")?)?,
        )?;
        returned.set_item(
            "stream",
            Self::merge_environment_value(py, &stream, &session.getattr("stream")?)?,
        )?;
        returned.set_item(
            "verify",
            Self::merge_environment_value(py, &verify, &session.getattr("verify")?)?,
        )?;
        returned.set_item(
            "cert",
            Self::merge_environment_value(py, &cert, &session.getattr("cert")?)?,
        )?;
        Ok(returned.into_any())
    }

    fn execute_environment_operation<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let state = invocation.getattr("state")?;
        let session = state.getattr("session")?;
        let proxies = state.getattr("proxies")?;
        let stream = state.getattr("request_stream")?;
        let cert = state.getattr("request_cert")?;
        let mode: String = state.getattr("mode")?.extract()?;
        let verify = (mode != "true").into_pyobject(py)?.to_owned().into_any();
        let original_helper = module.getattr("get_environ_proxies")?.unbind();
        let original_merge = module.getattr("merge_setting")?.unbind();
        let original_os = module.getattr("os")?.unbind();
        let original_netrc = module.getattr("get_netrc_auth")?.unbind();
        let original_bypass = module.getattr("should_bypass_proxies")?.unbind();
        module.setattr(
            "get_environ_proxies",
            state.getattr("operation_helper_forbidden")?,
        )?;
        module.setattr("merge_setting", state.getattr("merge")?)?;
        module.setattr("os", state.getattr("operation_os_forbidden")?)?;
        module.setattr(
            "get_netrc_auth",
            state.getattr("operation_netrc_forbidden")?,
        )?;
        module.setattr(
            "should_bypass_proxies",
            state.getattr("operation_bypass_forbidden")?,
        )?;
        session.setattr("trust_env", state.getattr("gate")?)?;
        let result = (|| {
            if session.getattr("trust_env")?.is_truthy()? {
                let no_proxy = proxies.call_method1("get", ("no_proxy",))?;
                let kwargs = PyDict::new(py);
                kwargs.set_item("no_proxy", no_proxy)?;
                let environment = module
                    .getattr("get_environ_proxies")?
                    .call(("http://example.test/path",), Some(&kwargs))?;
                for pair in environment.call_method0("items")?.try_iter()? {
                    let pair = pair?;
                    proxies.call_method1("setdefault", (pair.get_item(0)?, pair.get_item(1)?))?;
                }
            }
            let returned = PyDict::new(py);
            let result_keys = state.getattr("result_keys")?;
            for (index, request, session_value) in [
                (0, proxies.clone(), session.getattr("proxies")?),
                (1, stream.clone(), session.getattr("stream")?),
                (2, verify.clone(), session.getattr("verify")?),
                (3, cert.clone(), session.getattr("cert")?),
            ] {
                returned.set_item(
                    result_keys.get_item(index)?,
                    module
                        .getattr("merge_setting")?
                        .call1((request, session_value))?,
                )?;
            }
            Ok(returned.into_any())
        })();
        state.setattr("completed", true)?;
        module.setattr("get_environ_proxies", original_helper.bind(py))?;
        module.setattr("merge_setting", original_merge.bind(py))?;
        module.setattr("os", original_os.bind(py))?;
        module.setattr("get_netrc_auth", original_netrc.bind(py))?;
        module.setattr("should_bypass_proxies", original_bypass.bind(py))?;
        result
    }

    fn environment_proxies<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.utils")?;
        let state = invocation.getattr("state")?;
        let mode: String = state.getattr("mode")?.extract()?;
        let original_os = module.getattr("os")?.unbind();
        let original_bypass = module.getattr("proxy_bypass")?.unbind();
        let original_getproxies = module.getattr("getproxies")?.unbind();
        let initial_bypass = if mode == "precedence" {
            state.getattr("platform")?
        } else {
            state.getattr("early_bypass_forbidden")?
        };
        module.setattr("proxy_bypass", initial_bypass)?;
        module.setattr("getproxies", state.getattr("eager_getproxies_forbidden")?)?;
        let real_environment = original_os.bind(py).getattr("environ")?;
        let mut saved_proxy_environment: Option<Vec<(Py<PyAny>, Py<PyAny>)>> = None;
        let (no_proxy, url) = if mode == "precedence" {
            let mut saved = Vec::new();
            for pair in real_environment.call_method0("items")?.try_iter()? {
                let pair = pair?;
                let key = pair.get_item(0)?;
                if key
                    .call_method0("lower")?
                    .call_method1("endswith", ("_proxy",))?
                    .is_truthy()?
                {
                    saved.push((key.unbind(), pair.get_item(1)?.unbind()));
                }
            }
            for (key, _) in &saved {
                real_environment.call_method1("pop", (key.bind(py), py.None().into_bound(py)))?;
            }
            real_environment.set_item("HTTP_PROXY", "http://upper-http.proxy")?;
            real_environment.set_item("http_proxy", state.getattr("proxy_url")?)?;
            real_environment.set_item("ALL_PROXY", "http://upper-all.proxy")?;
            real_environment.set_item("all_proxy", state.getattr("all_url")?)?;
            real_environment
                .set_item("HTTP://API.EXAMPLE.TEST_PROXY", "http://host-upper.proxy")?;
            real_environment
                .set_item("http://api.example.test_proxy", state.getattr("host_url")?)?;
            module.setattr("os", original_os.bind(py))?;
            saved_proxy_environment = Some(saved);
            (
                "nomatch.invalid".into_pyobject(py)?.into_any(),
                "http://normal.example.test/path"
                    .into_pyobject(py)?
                    .into_any(),
            )
        } else {
            module.setattr("os", state.getattr("os_double")?)?;
            let no_proxy = match mode.as_str() {
                "suffix" => ".example.test".into_pyobject(py)?.into_any(),
                "negative-suffix" => "example.test".into_pyobject(py)?.into_any(),
                "port" => "api.example.test:8443".into_pyobject(py)?.into_any(),
                "cidr" => "10.0.0.0/8".into_pyobject(py)?.into_any(),
                "ipv4" => "10.1.2.3".into_pyobject(py)?.into_any(),
                "hostless" => "nomatch.invalid".into_pyobject(py)?.into_any(),
                "lower" | "upper" => py.None().into_bound(py).into_any(),
                _ => "nomatch.invalid".into_pyobject(py)?.into_any(),
            };
            let url = match mode.as_str() {
                "suffix" => "http://api.example.test/path",
                "negative-suffix" => "http://notexample.test/path",
                "port" => "http://api.example.test:8443/path",
                "cidr" | "ipv4" => "http://10.1.2.3/path",
                "hostless" => "file:///tmp/no-host",
                "lower" => "http://api.lower.test/path",
                "upper" => "http://api.upper.test/path",
                _ => "http://normal.example.test/path",
            }
            .into_pyobject(py)?
            .into_any();
            (no_proxy, url)
        };

        let result = (|| {
            let bypassed = Self::should_bypass_environment_proxy(py, &module, &url, &no_proxy)?;
            if matches!(
                mode.as_str(),
                "suffix" | "port" | "cidr" | "ipv4" | "hostless" | "lower" | "upper"
            ) {
                return Ok(PyBool::new(py, bypassed).to_owned().into_any());
            }
            if bypassed {
                return Ok(PyDict::new(py).into_any());
            }
            let getproxies = module.getattr("getproxies")?;
            getproxies.call0()
        })();

        state.setattr("completed", true)?;
        module.setattr("os", original_os.bind(py))?;
        module.setattr("proxy_bypass", original_bypass.bind(py))?;
        module.setattr("getproxies", original_getproxies.bind(py))?;
        if let Some(saved) = saved_proxy_environment {
            let mut keys = Vec::new();
            for key in real_environment.try_iter()? {
                let key = key?;
                if key
                    .call_method0("lower")?
                    .call_method1("endswith", ("_proxy",))?
                    .is_truthy()?
                {
                    keys.push(key.unbind());
                }
            }
            for key in keys {
                real_environment.call_method1("pop", (key.bind(py), py.None().into_bound(py)))?;
            }
            for (key, value) in saved {
                real_environment.set_item(key.bind(py), value.bind(py))?;
            }
        }
        result
    }

    fn should_bypass_environment_proxy(
        py: Python<'_>,
        module: &Bound<'_, PyModule>,
        url: &Bound<'_, PyAny>,
        no_proxy_argument: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let ipaddress = py.import("ipaddress")?;
        let socket_module = py.import("socket")?;
        let mut no_proxy = no_proxy_argument.clone();
        if no_proxy.is_none() {
            let environment = module.getattr("os")?.getattr("environ")?;
            let lower = environment.call_method1("get", ("no_proxy",))?;
            no_proxy = if lower.is_truthy()? {
                lower
            } else {
                environment.call_method1("get", ("NO_PROXY",))?
            };
        }
        let parsed = module.getattr("urlparse")?.call1((url,))?;
        let hostname = parsed.getattr("hostname")?;
        if hostname.is_none() {
            return Ok(true);
        }
        if no_proxy.is_truthy()? {
            let compact = no_proxy.call_method1("replace", (" ", ""))?;
            let split = compact.call_method1("split", (",",))?;
            let address = match ipaddress.getattr("IPv4Address")?.call1((&hostname,)) {
                Ok(address) => Some(address),
                Err(error) => {
                    let catcher = ipaddress.getattr("AddressValueError")?;
                    if error.matches(py, &catcher).unwrap() {
                        None
                    } else {
                        return Err(error);
                    }
                }
            };
            if let Some(address) = address {
                for proxy_ip in split.try_iter()? {
                    let proxy_ip = proxy_ip?;
                    if !proxy_ip.is_truthy()? {
                        continue;
                    }
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("strict", false)?;
                    match ipaddress
                        .getattr("IPv4Network")?
                        .call((&proxy_ip,), Some(&kwargs))
                    {
                        Ok(network) => {
                            if network.contains(&address)? {
                                return Ok(true);
                            }
                        }
                        Err(error) => {
                            let address_error = ipaddress.getattr("AddressValueError")?;
                            if !error.matches(py, &address_error).unwrap()
                                && !error.matches(py, py.get_type::<PyValueError>()).unwrap()
                            {
                                return Err(error);
                            }
                            if hostname.eq(&proxy_ip)? {
                                return Ok(true);
                            }
                        }
                    }
                }
            } else {
                let mut host_with_port = hostname.str()?.to_str()?.to_owned();
                if parsed.getattr("port")?.is_truthy()? {
                    let port = parsed.getattr("port")?.str()?.to_str()?.to_owned();
                    host_with_port.push(':');
                    host_with_port.push_str(&port);
                }
                for host in split.try_iter()? {
                    let host = host?;
                    if !host.is_truthy()? {
                        continue;
                    }
                    let host = host.call_method1("lstrip", (".",))?;
                    if hostname.eq(&host)? || host_with_port.as_str().eq(host.str()?.to_str()?) {
                        return Ok(true);
                    }
                    let suffix = format!(".{}", host.str()?.to_str()?);
                    if hostname.call_method1("endswith", (&suffix,))?.is_truthy()?
                        || host_with_port.ends_with(&suffix)
                    {
                        return Ok(true);
                    }
                }
            }
        }

        let value_changed = !no_proxy_argument.is_none();
        let mut environment = None;
        let mut old_value = None;
        if value_changed {
            let current_environment = module.getattr("os")?.getattr("environ")?;
            let previous = current_environment.call_method1("get", ("no_proxy",))?;
            current_environment.set_item("no_proxy", no_proxy_argument)?;
            environment = Some(current_environment);
            old_value = Some(previous);
        }
        let result = (|| {
            let platform_bypass = module.getattr("proxy_bypass")?;
            match platform_bypass.call1((&hostname,)) {
                Ok(bypass) => bypass.is_truthy(),
                Err(error) => {
                    let gaierror = socket_module.getattr("gaierror")?;
                    if error.matches(py, py.get_type::<PyTypeError>()).unwrap()
                        || error.matches(py, &gaierror).unwrap()
                    {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                }
            }
        })();
        if let (Some(environment), Some(old_value)) = (environment, old_value) {
            if old_value.is_none() {
                environment.del_item("no_proxy")?;
            } else {
                environment.set_item("no_proxy", old_value)?;
            }
        }
        result
    }

    fn environment_ca<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let state = invocation.getattr("state")?;
        let mode: String = state.getattr("mode")?.extract()?;
        let session = state.getattr("session")?;
        let proxies = state.getattr("proxies")?;
        let mut verify = state.getattr("verify")?;
        let original_os = module.getattr("os")?.unbind();
        let original_helper = module.getattr("get_environ_proxies")?.unbind();
        let selected_os = if mode == "opaque" || mode == "false" {
            state.getattr("forbidden_os")?
        } else {
            state.getattr("first_os")?
        };
        module.setattr("os", selected_os)?;
        module.setattr("get_environ_proxies", state.getattr("env_helper")?)?;

        let result = (|| {
            if session.getattr("trust_env")?.is_truthy()? {
                let no_proxy = if proxies.is_none() {
                    py.None().into_bound(py)
                } else {
                    proxies.call_method1("get", ("no_proxy",))?
                };
                let environment_helper = module.getattr("get_environ_proxies")?;
                let kwargs = PyDict::new(py);
                kwargs.set_item("no_proxy", no_proxy)?;
                let environment =
                    environment_helper.call(("http://ca.example.test/",), Some(&kwargs))?;
                if !proxies.is_none() {
                    for pair in environment.call_method0("items")?.try_iter()? {
                        let pair = pair?;
                        proxies
                            .call_method1("setdefault", (pair.get_item(0)?, pair.get_item(1)?))?;
                    }
                }
                if verify.is(PyBool::new(py, true)) || verify.is_none() {
                    let first_os = module.getattr("os")?;
                    let requests_ca = first_os
                        .getattr("environ")?
                        .call_method1("get", ("REQUESTS_CA_BUNDLE",))?;
                    if requests_ca.is_truthy()? {
                        verify = requests_ca;
                    } else {
                        let second_os = module.getattr("os")?;
                        let curl_ca = second_os
                            .getattr("environ")?
                            .call_method1("get", ("CURL_CA_BUNDLE",))?;
                        if curl_ca.is_truthy()? {
                            verify = curl_ca;
                        }
                    }
                }
            }

            let returned = PyDict::new(py);
            let result_keys = state.getattr("result_keys")?;
            returned.set_item(result_keys.get_item(0)?, &proxies)?;
            returned.set_item(result_keys.get_item(1)?, session.getattr("stream")?)?;
            returned.set_item(result_keys.get_item(2)?, verify)?;
            returned.set_item(result_keys.get_item(3)?, session.getattr("cert")?)?;
            Ok(returned.into_any())
        })();

        state.setattr("completed", true)?;
        module.setattr("os", original_os.bind(py))?;
        module.setattr("get_environ_proxies", original_helper.bind(py))?;
        result
    }

    fn environment_merge<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let state = invocation.getattr("state")?;
        let session = state.getattr("session")?;
        let request_values = state.getattr("request_values")?;
        let original_merge = module.getattr("merge_setting")?.unbind();
        module.setattr("merge_setting", state.getattr("helpers")?.get_item(0)?)?;

        let result = (|| {
            let merge_proxies = module.getattr("merge_setting")?;
            let merged_proxies =
                merge_proxies.call1((request_values.get_item(0)?, session.getattr("proxies")?))?;
            let merge_stream = module.getattr("merge_setting")?;
            let merged_stream =
                merge_stream.call1((request_values.get_item(1)?, session.getattr("stream")?))?;
            let merge_verify = module.getattr("merge_setting")?;
            let merged_verify =
                merge_verify.call1((request_values.get_item(2)?, session.getattr("verify")?))?;
            let merge_cert = module.getattr("merge_setting")?;
            let merged_cert =
                merge_cert.call1((request_values.get_item(3)?, session.getattr("cert")?))?;
            let returned = PyDict::new(py);
            let result_keys = state.getattr("result_keys")?;
            returned.set_item(result_keys.get_item(0)?, merged_proxies)?;
            returned.set_item(result_keys.get_item(1)?, merged_stream)?;
            returned.set_item(result_keys.get_item(2)?, merged_verify)?;
            returned.set_item(result_keys.get_item(3)?, merged_cert)?;
            Ok(returned.into_any())
        })();

        state.setattr("completed", true)?;
        module.setattr("merge_setting", original_merge.bind(py))?;
        result
    }

    fn rebuild_proxies<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let ordered = py.import("collections")?.getattr("OrderedDict")?;
        let state = invocation.getattr("state")?;
        let mode: String = state.getattr("mode")?.extract()?;
        let request = state.getattr("request")?;
        let session = state.getattr("session")?;
        let supplied = state.getattr("supplied")?;
        let missing = py.import("builtins")?.getattr("object")?.call0()?;
        let original_key_error = module
            .dict()
            .get_item("KeyError")?
            .map(|value| value.unbind());
        let builtins_controller = state.getattr("builtins_controller")?;
        let original_builtin_key_error = builtins_controller
            .call_method1("get", ("KeyError", &missing))?
            .unbind();
        let original_prepared = module.getattr("_is_prepared")?.unbind();
        let original_urlparse = module.getattr("urlparse")?.unbind();
        let original_resolve = module.getattr("resolve_proxies")?.unbind();
        let original_get_auth = module.getattr("get_auth_from_url")?.unbind();
        let original_basic = module.getattr("_basic_auth_str")?.unbind();
        let raw_headers = request.getattr("_headers")?;
        ordered.getattr("clear")?.call1((&raw_headers,))?;
        if mode == "auth-absent" {
            state.setattr("header_state", "absent")?;
        } else {
            ordered.getattr("__setitem__")?.call1((
                &raw_headers,
                "Proxy-Authorization",
                state.getattr("old_header_value")?,
            ))?;
            state.setattr("header_state", "old")?;
        }
        module.setattr("_is_prepared", state.getattr("prepared_helper")?)?;
        module.setattr("urlparse", state.getattr("parse_helper")?)?;
        module.setattr("resolve_proxies", state.getattr("resolve_helper")?)?;
        module.setattr("get_auth_from_url", state.getattr("get_auth_helper")?)?;
        module.setattr("_basic_auth_str", original_basic.bind(py))?;

        let result = (|| {
            let prepared_check = module.getattr("_is_prepared")?;
            if !prepared_check.call1((&request,))?.is_truthy()? {
                return Err(PyAssertionError::new_err(()));
            }
            let headers = request.getattr("headers")?;
            let parse = module.getattr("urlparse")?;
            let url = request.getattr("url")?;
            let parsed = parse.call1((url,))?;
            let scheme = parsed.getattr("scheme")?;
            let resolver = module.getattr("resolve_proxies")?;
            let trust_env = session.getattr("trust_env")?;
            let new_proxies = resolver.call1((&request, &supplied, trust_env))?;
            if headers.contains("Proxy-Authorization")? {
                headers.del_item("Proxy-Authorization")?;
            }

            let credentials = (|| {
                let auth_helper = module.getattr("get_auth_from_url")?;
                let proxy_url = new_proxies.get_item(&scheme)?;
                let auth = auth_helper.call1((proxy_url,))?;
                Self::unpack_two(py, &auth)
            })();
            let (username, password) = match credentials {
                Ok(values) => values,
                Err(error) => {
                    let catcher = match Self::live_key_error_type(&module, &builtins_controller) {
                        Ok(catcher) => catcher,
                        Err(matcher_error) => {
                            return Err(Self::attach_exception_context(py, matcher_error, &error));
                        }
                    };
                    let valid = py
                        .import("builtins")?
                        .getattr("issubclass")?
                        .call1((&catcher, py.get_type::<pyo3::exceptions::PyBaseException>()))
                        .and_then(|value| value.is_truthy());
                    match valid {
                        Ok(true) if error.matches(py, &catcher).unwrap() => {
                            (py.None().into_bound(py), py.None().into_bound(py))
                        }
                        Ok(true) => return Err(error),
                        Ok(false) | Err(_) => {
                            let matcher_error = PyTypeError::new_err(
                                "catching classes that do not inherit from BaseException is not allowed",
                            );
                            return Err(Self::attach_exception_context(py, matcher_error, &error));
                        }
                    }
                }
            };

            let startswith = scheme.getattr("startswith")?;
            if !startswith.call1(("https",))?.is_truthy()?
                && username.is_truthy()?
                && password.is_truthy()?
            {
                let basic_auth = module.getattr("_basic_auth_str")?;
                headers.set_item(
                    "Proxy-Authorization",
                    basic_auth.call1((username, password))?,
                )?;
            }
            Ok(new_proxies)
        })();

        state.setattr("completed", true)?;
        module.setattr("_is_prepared", original_prepared.bind(py))?;
        module.setattr("urlparse", original_urlparse.bind(py))?;
        module.setattr("resolve_proxies", original_resolve.bind(py))?;
        module.setattr("get_auth_from_url", original_get_auth.bind(py))?;
        module.setattr("_basic_auth_str", original_basic.bind(py))?;
        if let Some(original_key_error) = original_key_error {
            module.setattr("KeyError", original_key_error.bind(py))?;
        } else {
            module
                .dict()
                .call_method1("pop", ("KeyError", py.None().into_bound(py)))?;
        }
        if original_builtin_key_error.bind(py).is(&missing) {
            builtins_controller.call_method1("delete", ("KeyError",))?;
        } else {
            builtins_controller
                .call_method1("set", ("KeyError", original_builtin_key_error.bind(py)))?;
        }
        result
    }

    fn live_key_error_type<'py>(
        module: &Bound<'py, PyModule>,
        controller: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(catcher) = module.dict().get_item("KeyError")? {
            return Ok(catcher);
        }
        if !controller
            .call_method1("contains", ("KeyError",))?
            .is_truthy()?
        {
            return Err(PyNameError::new_err("name 'KeyError' is not defined"));
        }
        controller.call_method1("get", ("KeyError",))
    }

    fn unpack_two<'py>(
        _py: Python<'py>,
        value: &Bound<'py, PyAny>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        let exact_tuple_len = value.cast::<PyTuple>().ok().map(|tuple| tuple.len());
        let mut iterator = value.try_iter()?;
        let first = iterator.next().transpose()?.ok_or_else(|| {
            PyValueError::new_err("not enough values to unpack (expected 2, got 0)")
        })?;
        let second = iterator.next().transpose()?.ok_or_else(|| {
            PyValueError::new_err("not enough values to unpack (expected 2, got 1)")
        })?;
        if iterator.next().transpose()?.is_some() {
            return Err(PyValueError::new_err(match exact_tuple_len {
                Some(length) => format!("too many values to unpack (expected 2, got {length})"),
                None => "too many values to unpack (expected 2)".to_owned(),
            }));
        }
        Ok((first, second))
    }

    fn attach_exception_context(py: Python<'_>, error: PyErr, context: &PyErr) -> PyErr {
        let _ = error.value(py).setattr("__context__", context.value(py));
        error
    }

    fn construct_session<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let ordered = py.import("collections")?.getattr("OrderedDict")?;
        let allocator = invocation.getattr("allocate")?;
        let constructed = allocator.call0()?;
        constructed.setattr("headers", module.getattr("default_headers")?.call0()?)?;
        constructed.setattr("auth", py.None())?;
        constructed.setattr("proxies", PyDict::new(py))?;
        constructed.setattr("hooks", module.getattr("default_hooks")?.call0()?)?;
        constructed.setattr("params", PyDict::new(py))?;
        constructed.setattr("stream", false)?;
        constructed.setattr("verify", true)?;
        constructed.setattr("cert", py.None())?;
        constructed.setattr("max_redirects", module.getattr("DEFAULT_REDIRECT_LIMIT")?)?;
        constructed.setattr("trust_env", true)?;
        constructed.setattr(
            "cookies",
            module
                .getattr("cookiejar_from_dict")?
                .call1((PyDict::new(py),))?,
        )?;
        constructed.setattr("adapters", ordered.call0()?)?;

        let custom = !constructed.get_type().is(&module.getattr("Session")?);
        let first_mount = if custom {
            Some(constructed.getattr("mount")?)
        } else {
            None
        };
        let first_factory = module.getattr("HTTPAdapter")?;
        let first_adapter = first_factory.call0()?;
        if let Some(first_mount) = first_mount {
            first_mount.call1(("https://", first_adapter))?;
        } else {
            Self::inline_base_mount(py, &constructed, "https://", &first_adapter)?;
        }

        let second_mount = if custom {
            Some(constructed.getattr("mount")?)
        } else {
            None
        };
        let second_factory = module.getattr("HTTPAdapter")?;
        let second_adapter = second_factory.call0()?;
        if let Some(second_mount) = second_mount {
            second_mount.call1(("http://", second_adapter))?;
        } else {
            Self::inline_base_mount(py, &constructed, "http://", &second_adapter)?;
        }
        Ok(constructed)
    }

    fn inline_base_mount(
        py: Python<'_>,
        session: &Bound<'_, PyAny>,
        prefix: &str,
        adapter: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let object_getattribute = py
            .import("builtins")?
            .getattr("object")?
            .getattr("__getattribute__")?;
        let adapters = object_getattribute.call1((session, "adapters"))?;
        adapters.set_item(prefix, adapter)?;
        let builtin_len = py.import("builtins")?.getattr("len")?;
        let mut keys_to_move = Vec::new();
        for key in adapters.try_iter()? {
            let key = key?;
            let key_len: usize = builtin_len.call1((&key,))?.extract()?;
            let prefix_len: usize = builtin_len.call1((prefix,))?.extract()?;
            if key_len < prefix_len {
                keys_to_move.push(key.unbind());
            }
        }
        for key in keys_to_move {
            let moved = adapters.call_method1("pop", (key.bind(py),))?;
            adapters.set_item(key.bind(py), moved)?;
        }
        Ok(())
    }

    fn pickle_session<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let state = invocation.getattr("state")?;
        let mode = state.getattr("mode")?;
        let is_extract = mode.call_method1("startswith", ("extract",))?.is_truthy()?
            || mode.eq("attrs-stop")?
            || mode.eq("duplicates-missing")?;
        if is_extract {
            return Self::extract_pickle_state(py, &module, &state.getattr("source")?);
        }
        let is_restore = mode.call_method1("startswith", ("restore",))?.is_truthy()?
            || mode.call_method1("startswith", ("items",))?.is_truthy()?;
        if is_restore {
            let target = state.getattr("target")?;
            Self::restore_pickle_state(py, &module, &target, &state.getattr("items")?)?;
            state.setattr("partial", &target)?;
            return Ok(py.None().into_bound(py));
        }

        let source = state.getattr("source")?;
        let extracted = Self::extract_pickle_state(py, &module, &source)?;
        state.setattr("extracted", &extracted)?;
        let dumps = module.getattr("_v6_a05_dumps")?;
        let payload = dumps.call1((&extracted,))?;
        state.setattr("payload", &payload)?;
        let loads = module.getattr("_v6_a05_loads")?;
        let decoded = loads.call1((&payload,))?;
        state.setattr("decoded", &decoded)?;
        let session_type = module.getattr("_v6_a05_session_type")?;
        let allocator = module.getattr("_v6_a05_allocate")?;
        let restored = allocator.call1((session_type,))?;
        Self::restore_pickle_state(py, &module, &restored, &decoded)?;
        state.setattr("roundtrip", &restored)?;
        Ok(restored)
    }

    fn extract_pickle_state<'py>(
        py: Python<'py>,
        module: &Bound<'py, PyModule>,
        source: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let names = source.getattr("__attrs__")?;
        let extracted = PyDict::new(py);
        for name in names.try_iter()? {
            let name = name?;
            let live_getattr = module.getattr("getattr")?;
            let value = live_getattr.call1((source, &name, py.None().into_bound(py)))?;
            extracted.set_item(name, value)?;
        }
        Ok(extracted.into_any())
    }

    fn restore_pickle_state(
        _py: Python<'_>,
        module: &Bound<'_, PyModule>,
        target: &Bound<'_, PyAny>,
        state_mapping: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let items_method = state_mapping.getattr("items")?;
        let pairs = items_method.call0()?;
        for pair in pairs.try_iter()? {
            let pair = pair?;
            let live_setattr = module.getattr("setattr")?;
            live_setattr.call1((target, pair.get_item(0)?, pair.get_item(1)?))?;
        }
        Ok(())
    }

    fn active_stream_close<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = invocation.getattr("state")?;
        let session = state.getattr("session")?;
        let request = state.getattr("request")?;
        let mode: String = state.getattr("mode")?.extract()?;
        match mode.as_str() {
            "lifecycle" => {
                let old_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("old_returned", &old_returned)?;
                let first_chunk = old_returned.getattr("raw")?.getattr("read")?.call0()?;
                state.setattr("first_chunk", first_chunk)?;
                Self::close_session(&session)?;
                let new_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("new_returned", &new_returned)?;
                Self::close_session(&session)?;
                Self::close_session(&session)?;
                session.getattr("adapters")?.call_method0("clear")?;
                Self::close_session(&session)?;
                Ok(new_returned)
            }
            "remount-before-close" => {
                let old_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("old_returned", old_returned)?;
                let new_adapter = state.getattr("new_adapter")?;
                Self::mount_active_adapter(py, &session, "http://", &new_adapter)?;
                Self::close_session(&session)?;
                let new_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("new_returned", &new_returned)?;
                Ok(new_returned)
            }
            "close-before-remount" => {
                let old_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("old_returned", old_returned)?;
                Self::close_session(&session)?;
                let new_adapter = state.getattr("new_adapter")?;
                Self::mount_active_adapter(py, &session, "http://", &new_adapter)?;
                let new_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("new_returned", &new_returned)?;
                Ok(new_returned)
            }
            "duplicate-close" | "mapping-replace" | "close-in-place" | "close-error" => {
                Self::close_session(&session)?;
                Ok(py.None().into_bound(py))
            }
            "send-retained" | "send-in-place" => {
                let old_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("old_returned", &old_returned)?;
                Ok(old_returned)
            }
            "peer-isolation" => {
                Self::close_session(&session)?;
                let peer_session = state.getattr("peer_session")?;
                let new_returned = Self::send_active_request(py, &peer_session, &request)?;
                state.setattr("new_returned", &new_returned)?;
                Ok(new_returned)
            }
            _ => {
                let old_returned = Self::send_active_request(py, &session, &request)?;
                state.setattr("old_returned", &old_returned)?;
                Ok(old_returned)
            }
        }
    }

    fn mount_active_adapter(
        py: Python<'_>,
        session: &Bound<'_, PyAny>,
        prefix: &str,
        adapter: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let adapters = session.getattr("adapters")?;
        adapters.set_item(prefix, adapter)?;
        let builtin_len = py.import("builtins")?.getattr("len")?;
        let mut keys_to_move = Vec::new();
        for key in adapters.try_iter()? {
            let key = key?;
            let key_len: usize = builtin_len.call1((&key,))?.extract()?;
            let prefix_len: usize = builtin_len.call1((prefix,))?.extract()?;
            if key_len < prefix_len {
                keys_to_move.push(key.unbind());
            }
        }
        for key in keys_to_move {
            let moved = adapters.call_method1("pop", (key.bind(py),))?;
            adapters.set_item(key.bind(py), moved)?;
        }
        Ok(())
    }

    fn select_active_adapter<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        url: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let adapters = session.getattr("adapters")?;
        let items_method = adapters.getattr("items")?;
        let pairs = items_method.call0()?;
        for pair in pairs.try_iter()? {
            let pair = pair?;
            let prefix = pair.get_item(0)?;
            let adapter = pair.get_item(1)?;
            let lowered = url.call_method0("lower")?;
            let retained_startswith = lowered.getattr("startswith")?;
            let lowered_prefix = prefix.call_method0("lower")?;
            if retained_startswith.call1((lowered_prefix,))?.is_truthy()? {
                return Ok(adapter);
            }
        }
        let error_type = py.import("requests.sessions")?.getattr("InvalidSchema")?;
        let message = format!(
            "No connection adapters were found for {}",
            url.repr()?.to_str()?
        );
        Err(PyErr::from_value(error_type.call1((message,))?))
    }

    fn send_active_request<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        request: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let adapter = Self::select_active_adapter(py, session, &request.getattr("url")?)?;
        let send_method = adapter.getattr("send")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("stream", true)?;
        kwargs.set_item("verify", session.getattr("verify")?)?;
        kwargs.set_item("cert", session.getattr("cert")?)?;
        kwargs.set_item("proxies", session.getattr("proxies")?)?;
        send_method.call((request,), Some(&kwargs))
    }

    fn rebuild_auth<'py>(
        py: Python<'py>,
        invocation: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let ordered = py.import("collections")?.getattr("OrderedDict")?;
        let state = invocation.getattr("state")?;
        let family: String = state.getattr("family")?.extract()?;
        if family == "strip" {
            let original_urlparse = module.getattr("urlparse")?.unbind();
            let original_ports = module.getattr("DEFAULT_PORTS")?.unbind();
            module.setattr("urlparse", state.getattr("parser_one")?)?;
            module.setattr("DEFAULT_PORTS", state.getattr("early_ports_forbidden")?)?;
            let result = Self::should_strip_auth(
                py,
                &module,
                &state.getattr("old_url")?,
                &state.getattr("new_url")?,
            )
            .map(|value| PyBool::new(py, value).to_owned().into_any());
            state.setattr("completed", true)?;
            module.setattr("urlparse", original_urlparse.bind(py))?;
            module.setattr("DEFAULT_PORTS", original_ports.bind(py))?;
            return result;
        }

        let headers = state.getattr("headers")?;
        ordered.getattr("clear")?.call1((&headers,))?;
        let kind: String = state.getattr("kind")?.extract()?;
        if kind != "rebuild-auth-absent" {
            ordered.getattr("__setitem__")?.call1((
                &headers,
                "Authorization",
                state.getattr("old_header_value")?,
            ))?;
            state.setattr("header_state", "old")?;
        } else {
            state.setattr("header_state", "absent")?;
        }
        state.setattr("prepared_auth", py.None())?;
        let prepared = state.getattr("prepared")?;
        prepared.setattr("current_prepare", state.getattr("early_prepare_forbidden")?)?;
        let original_prepared = module.getattr("_is_prepared")?.unbind();
        let original_netrc = module.getattr("get_netrc_auth")?.unbind();
        module.setattr("_is_prepared", state.getattr("prepared_one")?)?;
        module.setattr("get_netrc_auth", state.getattr("eager_netrc_forbidden")?)?;

        let result = (|| {
            let response = state.getattr("response")?;
            let original_request = response.getattr("request")?;
            let first_prepared = module.getattr("_is_prepared")?;
            if !first_prepared.call1((&original_request,))?.is_truthy()? {
                return Err(PyAssertionError::new_err(()));
            }
            let second_prepared = module.getattr("_is_prepared")?;
            if !second_prepared.call1((&prepared,))?.is_truthy()? {
                return Err(PyAssertionError::new_err(()));
            }
            let live_headers = prepared.getattr("headers")?;
            let original_url = original_request.getattr("url")?;
            let url = prepared.getattr("url")?;
            if live_headers.contains("Authorization")? {
                let strip_method = state.getattr("session")?.getattr("should_strip_auth")?;
                if strip_method.call1((&original_url, &url))?.is_truthy()? {
                    live_headers.del_item("Authorization")?;
                }
            }
            let session = state.getattr("session")?;
            let trust_env = session.getattr("trust_env")?;
            let new_auth = if trust_env.is_truthy()? {
                let netrc = module.getattr("get_netrc_auth")?;
                netrc.call1((&url,))?
            } else {
                py.None().into_bound(py)
            };
            if !new_auth.is_none() {
                let prepare_auth = prepared.getattr("prepare_auth")?;
                prepare_auth.call1((new_auth,))?;
            }
            Ok(py.None().into_bound(py))
        })();

        state.setattr("completed", true)?;
        module.setattr("_is_prepared", original_prepared.bind(py))?;
        module.setattr("get_netrc_auth", original_netrc.bind(py))?;
        result
    }

    fn should_strip_auth(
        py: Python<'_>,
        module: &Bound<'_, PyModule>,
        old_url: &Bound<'_, PyAny>,
        new_url: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let first_parser = module.getattr("urlparse")?;
        let old_parsed = first_parser.call1((old_url,))?;
        let second_parser = module.getattr("urlparse")?;
        let new_parsed = second_parser.call1((new_url,))?;
        if !old_parsed
            .getattr("hostname")?
            .eq(new_parsed.getattr("hostname")?)?
        {
            return Ok(true);
        }
        let old_scheme = old_parsed.getattr("scheme")?;
        if old_scheme.eq("http")? {
            let old_port = old_parsed.getattr("port")?;
            if (old_port.eq(80)? || old_port.is_none())
                && new_parsed.getattr("scheme")?.eq("https")?
            {
                let new_port = new_parsed.getattr("port")?;
                if new_port.eq(443)? || new_port.is_none() {
                    return Ok(false);
                }
            }
        }
        let changed_port = !old_parsed
            .getattr("port")?
            .eq(new_parsed.getattr("port")?)?;
        let changed_scheme = !old_parsed
            .getattr("scheme")?
            .eq(new_parsed.getattr("scheme")?)?;
        let default_get = module.getattr("DEFAULT_PORTS")?.getattr("get")?;
        let old_scheme = old_parsed.getattr("scheme")?;
        let default_value = default_get.call1((old_scheme, py.None().into_bound(py)))?;
        let default_port = PyTuple::new(py, [default_value.unbind(), py.None()])?;
        if !changed_scheme
            && default_port.contains(old_parsed.getattr("port")?)?
            && default_port.contains(new_parsed.getattr("port")?)?
        {
            return Ok(false);
        }
        Ok(changed_port || changed_scheme)
    }

    fn merge_environment_value<'py>(
        py: Python<'py>,
        request: &Bound<'py, PyAny>,
        session: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if session.is_none() {
            return Ok(request.clone());
        }
        if request.is_none() {
            return Ok(session.clone());
        }
        if !request.hasattr("items")? || !session.hasattr("items")? {
            return Ok(request.clone());
        }
        let ordered = py.import("collections")?.getattr("OrderedDict")?;
        let merged = ordered.call1((session,))?;
        merged.call_method1("update", (request,))?;
        let mut none_keys = Vec::new();
        for pair in merged.call_method0("items")?.try_iter()? {
            let pair = pair?;
            if pair.get_item(1)?.is_none() {
                none_keys.push(pair.get_item(0)?.unbind());
            }
        }
        for key in none_keys {
            merged.del_item(key.bind(py))?;
        }
        Ok(merged)
    }

    fn close_session(session: &Bound<'_, PyAny>) -> PyResult<()> {
        let adapters = session.getattr("adapters")?;
        let values = adapters.getattr("values")?.call0()?;
        for adapter in values.try_iter()? {
            adapter?.getattr("close")?.call0()?;
        }
        Ok(())
    }

    fn execute_close_operation<'py>(invocation: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let py = invocation.py();
        let state = invocation.getattr("state")?;
        let session = state.getattr("session")?;
        let mode: String = state.getattr("mode")?.extract()?;
        if !mode.starts_with("exit") && mode != "body-propagates" {
            state.setattr("tracing", true)?;
            Self::close_session(&session)?;
            if mode == "repeat-close" {
                Self::close_session(&session)?;
            }
            state.setattr("tracing", false)?;
            return Ok(py.None().into_bound(py));
        }
        if mode == "exit-none" || mode == "exit-retained" {
            session.getattr("close")?.call0()?;
            return Ok(py.None().into_bound(py));
        }
        state.setattr("use_replacement", true)?;
        let body_marker = state.getattr("body_marker")?;
        if let Err(error) = session.getattr("close").and_then(|close| close.call0()) {
            error.value(py).setattr("__context__", &body_marker)?;
            return Err(error);
        }
        Err(PyErr::from_value(body_marker))
    }

    fn send_after_close<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = args.get_item(0)?;
        let url = request.getattr("url")?.call_method0("lower")?;
        let adapters = session.getattr("adapters")?;
        let mut selected = None;
        for pair in adapters.call_method0("items")?.try_iter()? {
            let pair = pair?;
            let prefix = pair.get_item(0)?.call_method0("lower")?;
            if url.call_method1("startswith", (prefix,))?.is_truthy()? {
                selected = Some(pair.get_item(1)?);
                break;
            }
        }
        let adapter = selected.ok_or_else(|| PyRuntimeError::new_err("no adapter after close"))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("stream", session.getattr("stream")?)?;
        kwargs.set_item("verify", session.getattr("verify")?)?;
        kwargs.set_item("cert", session.getattr("cert")?)?;
        kwargs.set_item("proxies", session.getattr("proxies")?)?;
        adapter.getattr("send")?.call((request,), Some(&kwargs))
    }

    fn prepare_request<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let cookies_module = py.import("requests.cookies")?;
        let cookiejar_type = py.import("http.cookiejar")?.getattr("CookieJar")?;
        let headers_type = py
            .import("requests.structures")?
            .getattr("CaseInsensitiveDict")?;
        let request = args.get_item(0)?;
        let mut cookies = request.getattr("cookies")?;
        if !cookies.is_truthy()? {
            cookies = PyDict::new(py).into_any();
        }
        let is_cookiejar = if let Ok(classifier) = module.getattr("isinstance") {
            classifier.call1((&cookies, &cookiejar_type))?.is_truthy()?
        } else {
            cookies.is_instance(&cookiejar_type)?
        };
        if !is_cookiejar {
            cookies = module.getattr("cookiejar_from_dict")?.call1((cookies,))?;
        }
        // CPython evaluates the outer callable before evaluating its nested
        // call argument, so retain that authority across the inner call.
        let outer_merge = module.getattr("merge_cookies")?;
        let inner = module.getattr("merge_cookies")?.call1((
            cookies_module.getattr("RequestsCookieJar")?.call0()?,
            session.getattr("cookies")?,
        ))?;
        let merged_cookies = outer_merge.call1((inner, cookies))?;
        let prepared = module.getattr("PreparedRequest")?.call0()?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("method", request.getattr("method")?.call_method0("upper")?)?;
        for name in ["url", "files", "data", "json"] {
            kwargs.set_item(name, request.getattr(name)?)?;
        }
        let merge_headers_kwargs = PyDict::new(py);
        merge_headers_kwargs.set_item("dict_class", headers_type)?;
        kwargs.set_item(
            "headers",
            module.getattr("merge_setting")?.call(
                (request.getattr("headers")?, session.getattr("headers")?),
                Some(&merge_headers_kwargs),
            )?,
        )?;
        for name in ["params", "auth"] {
            kwargs.set_item(
                name,
                module
                    .getattr("merge_setting")?
                    .call1((request.getattr(name)?, session.getattr(name)?))?,
            )?;
        }
        kwargs.set_item("cookies", merged_cookies)?;
        kwargs.set_item(
            "hooks",
            module
                .getattr("merge_hooks")?
                .call1((request.getattr("hooks")?, session.getattr("hooks")?))?,
        )?;
        prepared.getattr("prepare")?.call((), Some(&kwargs))?;
        Ok(prepared)
    }

    fn cookie_prepare<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let cookies_module = py.import("requests.cookies")?;
        let models = py.import("requests.models")?;
        let cookiejar_type = py.import("http.cookiejar")?.getattr("CookieJar")?;
        let request = args.get_item(0)?;
        let mut cookies = request.getattr("cookies")?;
        if !cookies.is_truthy()? {
            cookies = PyDict::new(py).into_any();
        }
        let classifier = module.getattr("isinstance")?;
        if !classifier.call1((&cookies, cookiejar_type))?.is_truthy()? {
            cookies = module.getattr("cookiejar_from_dict")?.call1((cookies,))?;
        }
        let outer_merge = module.getattr("merge_cookies")?;
        let inner_merge = module.getattr("merge_cookies")?;
        let inner = inner_merge.call1((
            cookies_module.getattr("RequestsCookieJar")?.call0()?,
            session.getattr("cookies")?,
        ))?;
        let merged = outer_merge.call1((inner, cookies))?;
        let prepared = models.getattr("PreparedRequest")?.call0()?;
        prepared.setattr("_cookies", merged)?;
        Ok(prepared)
    }

    fn auth_prepare<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let models = py.import("requests.models")?;
        let request = args.get_item(0)?;
        let mut auth = request.getattr("auth")?;
        if session.getattr("trust_env")?.is_truthy()?
            && !auth.is_truthy()?
            && !session.getattr("auth")?.is_truthy()?
        {
            auth = module
                .getattr("get_netrc_auth")?
                .call1((request.getattr("url")?,))?;
        }
        if auth.is_none() {
            auth = session.getattr("auth")?;
        }
        let prepared = models.getattr("PreparedRequest")?.call0()?;
        let kwargs = PyDict::new(py);
        for name in [
            "method", "url", "headers", "files", "data", "json", "params", "cookies", "hooks",
        ] {
            kwargs.set_item(name, request.getattr(name)?)?;
        }
        kwargs.set_item("auth", auth)?;
        prepared.getattr("prepare")?.call((), Some(&kwargs))?;
        Ok(prepared)
    }

    fn setting_prepare<'py>(
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let models = py.import("requests.models")?;
        let request = args.get_item(0)?;
        let header_kwargs = PyDict::new(py);
        header_kwargs.set_item("dict_class", session.getattr("headers")?.get_type())?;
        let headers = module.getattr("merge_setting")?.call(
            (request.getattr("headers")?, session.getattr("headers")?),
            Some(&header_kwargs),
        )?;
        let params = module
            .getattr("merge_setting")?
            .call1((request.getattr("params")?, session.getattr("params")?))?;
        let auth = module
            .getattr("merge_setting")?
            .call1((request.getattr("auth")?, session.getattr("auth")?))?;
        let prepared = models.getattr("PreparedRequest")?.call0()?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("method", request.getattr("method")?)?;
        kwargs.set_item("url", request.getattr("url")?)?;
        kwargs.set_item("headers", headers)?;
        kwargs.set_item("params", params)?;
        kwargs.set_item("auth", auth)?;
        prepared.getattr("prepare")?.call((), Some(&kwargs))?;
        Ok(prepared)
    }

    fn session_request<'py>(
        py: Python<'py>,
        args: &Bound<'py, PyAny>,
        supplied: &Bound<'py, PyDict>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let session = args.get_item(0)?;
        let method = args.get_item(1)?;
        let mut url = args.get_item(2)?;

        // Retain each global/method at the point CPython evaluates it.
        let classifier = module.getattr("isinstance")?;
        let bytes_type = module.getattr("bytes")?;
        if classifier.call1((&url, bytes_type))?.is_truthy()? {
            let decode = url.getattr("decode")?;
            url = decode.call1(("utf-8",))?;
        }

        let request_factory = module.getattr("Request")?;
        let request_kwargs = PyDict::new(py);
        request_kwargs.set_item("method", method.call_method0("upper")?)?;
        request_kwargs.set_item("url", url)?;
        for name in ["headers", "files"] {
            request_kwargs.set_item(
                name,
                supplied
                    .get_item(name)?
                    .unwrap_or_else(|| py.None().into_bound(py)),
            )?;
        }
        let data = supplied
            .get_item("data")?
            .unwrap_or_else(|| py.None().into_bound(py));
        request_kwargs.set_item(
            "data",
            if data.is_truthy()? {
                data
            } else {
                PyDict::new(py).into_any()
            },
        )?;
        request_kwargs.set_item(
            "json",
            supplied
                .get_item("json")?
                .unwrap_or_else(|| py.None().into_bound(py)),
        )?;
        let params = supplied
            .get_item("params")?
            .unwrap_or_else(|| py.None().into_bound(py));
        request_kwargs.set_item(
            "params",
            if params.is_truthy()? {
                params
            } else {
                PyDict::new(py).into_any()
            },
        )?;
        for name in ["auth", "cookies", "hooks"] {
            request_kwargs.set_item(
                name,
                supplied
                    .get_item(name)?
                    .unwrap_or_else(|| py.None().into_bound(py)),
            )?;
        }
        let request = request_factory.call((), Some(&request_kwargs))?;

        let prepare = session.getattr("prepare_request")?;
        let prepared = prepare.call1((request,))?;
        module
            .getattr("_is_prepared")?
            .call1((&prepared,))?
            .is_truthy()?;

        let proxies = supplied
            .get_item("proxies")?
            .unwrap_or_else(|| py.None().into_bound(py));
        let proxies = if proxies.is_truthy()? {
            proxies
        } else {
            PyDict::new(py).into_any()
        };
        let environment = session.getattr("merge_environment_settings")?;
        let prepared_url = prepared.getattr("url")?;
        let settings = environment.call1((
            prepared_url,
            proxies,
            supplied
                .get_item("stream")?
                .unwrap_or_else(|| py.None().into_bound(py)),
            supplied
                .get_item("verify")?
                .unwrap_or_else(|| py.None().into_bound(py)),
            supplied
                .get_item("cert")?
                .unwrap_or_else(|| py.None().into_bound(py)),
        ))?;
        let send_kwargs = PyDict::new(py);
        send_kwargs.set_item(
            "timeout",
            supplied
                .get_item("timeout")?
                .unwrap_or_else(|| py.None().into_bound(py)),
        )?;
        send_kwargs.set_item(
            "allow_redirects",
            supplied
                .get_item("allow_redirects")?
                .unwrap_or_else(|| PyBool::new(py, true).to_owned().into_any()),
        )?;
        send_kwargs.call_method1("update", (settings,))?;
        session
            .getattr("send")?
            .call((prepared,), Some(&send_kwargs))
    }

    fn direct_prepare<'py>(
        py: Python<'py>,
        _session: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let module = py.import("requests.sessions")?;
        let request = args.get_item(0)?;
        let prepared = module.getattr("PreparedRequest")?.call0()?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("method", request.getattr("method")?.call_method0("upper")?)?;
        for name in [
            "url", "files", "data", "json", "headers", "params", "auth", "cookies", "hooks",
        ] {
            kwargs.set_item(name, request.getattr(name)?)?;
        }
        prepared.getattr("prepare")?.call((), Some(&kwargs))?;
        Ok(prepared)
    }

    fn action_state<'py>(receiver: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        receiver.getattr("state")
    }

    fn install_redirect_cursor<'py>(
        py: Python<'py>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args.cast::<PyTuple>()?;
        let context = args.get_item(0)?;
        let outcome_key = args.get_item(1)?;
        let handle = args.get_item(2)?;
        let holders = context.getattr("holders")?;
        let record = holders.get_item(&outcome_key)?;
        let record = record.cast::<PyTuple>()?;
        if record.len() != 2 || record.get_item(0)?.extract::<String>()? != "return" {
            return Err(PyTypeError::new_err(
                "generator outcome is not a returned generator",
            ));
        }
        let cursor_value = record.get_item(1)?;
        let cursor: PyRef<'_, SessionRedirectCursor> = cursor_value.extract()?;
        let token = handle.getattr("token")?;
        cursor.claim(&token)?;
        drop(cursor);
        handle.call_method1("install_native", (&cursor_value,))?;
        holders.set_item(
            &outcome_key,
            PyTuple::new(
                py,
                ["return".into_pyobject(py)?.into_any().unbind(), py.None()],
            )?,
        )?;
        Ok(py.None().into_bound(py))
    }

    fn begin_semantic_action(state: &Bound<'_, PyAny>, label: &str) -> PyResult<()> {
        if state.getattr("completed")?.is_truthy()? {
            return Err(PyAssertionError::new_err(format!(
                "{label} semantic operation replayed"
            )));
        }
        state.setattr("completed", true)
    }

    fn redirect_target<'py>(
        py: Python<'py>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args.cast::<PyTuple>()?;
        let _session = args.get_item(0)?;
        let response = args.get_item(1)?;
        let module = PyModule::import(py, "requests.sessions")?;
        if !response.getattr("is_redirect")?.is_truthy()? {
            return Ok(py.None().into_bound(py));
        }
        let location = response.getattr("headers")?.get_item("location")?;
        let encoded = location.getattr("encode")?.call1(("latin1",))?;
        module.getattr("to_native_string")?.call1((encoded, "utf8"))
    }

    fn redirect_method<'py>(
        py: Python<'py>,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args.cast::<PyTuple>()?;
        let request = args.get_item(1)?;
        let response = args.get_item(2)?;
        let module = PyModule::import(py, "requests.sessions")?;
        let method = request.getattr("method")?;
        let mut selected = method.clone();
        let status = response.getattr("status_code")?;
        let code = module.getattr("codes")?.getattr("see_other")?;
        if status
            .rich_compare(&code, pyo3::basic::CompareOp::Eq)?
            .is_truthy()?
            && method
                .rich_compare("HEAD", pyo3::basic::CompareOp::Ne)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        let status = response.getattr("status_code")?;
        let code = module.getattr("codes")?.getattr("found")?;
        if status
            .rich_compare(&code, pyo3::basic::CompareOp::Eq)?
            .is_truthy()?
            && method
                .rich_compare("HEAD", pyo3::basic::CompareOp::Ne)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        let status = response.getattr("status_code")?;
        let code = module.getattr("codes")?.getattr("moved")?;
        if status
            .rich_compare(&code, pyo3::basic::CompareOp::Eq)?
            .is_truthy()?
            && method
                .rich_compare("POST", pyo3::basic::CompareOp::Eq)?
                .is_truthy()?
        {
            selected = PyString::intern(py, "GET").into_any();
        }
        request.setattr("method", selected)?;
        Ok(py.None().into_bound(py))
    }

    fn redirect_unimplemented<'py>(name: &str) -> PyResult<Bound<'py, PyAny>> {
        Err(PyNotImplementedError::new_err(format!(
            "native {name} is not implemented"
        )))
    }

    fn redirect_url_command<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
        _action: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-url")
    }
    fn redirect_headers<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-headers")
    }
    fn redirect_history_resolve<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-history-resolve")
    }
    fn redirect_history_send<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-history-send")
    }
    fn redirect_limit_errors<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-limit-errors")
    }
    fn redirect_generator_command<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
        _action: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-generator")
    }
    fn redirect_resource<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-resource")
    }
    fn redirect_cookies<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-cookies")
    }
    fn redirect_proxy_auth_rewind<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-proxy-auth-rewind")
    }
    fn redirect_nested_resend<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("redirect-nested-resend")
    }
    fn digest_redirect<'py>(
        _py: Python<'py>,
        _receiver: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::redirect_unimplemented("digest-redirect")
    }
    fn send_global<'py>(
        py: Python<'py>,
        module: &Bound<'py, PyModule>,
        builtins: &Bound<'py, PyAny>,
        name: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(value) = module.dict().get_item(name)? {
            return Ok(value);
        }
        let value = if let Ok(mapping) = builtins.cast::<PyDict>() {
            mapping.get_item(name)?
        } else {
            builtins.getattr(name).ok()
        };
        match value {
            Some(value) => Ok(value),
            None => {
                let error = PyNameError::new_err(format!("name '{name}' is not defined"));
                error.value(py).setattr("name", name)?;
                Err(error)
            }
        }
    }

    fn send_matches_exception(
        py: Python<'_>,
        candidate: &PyErr,
        catcher: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let base_exception = py.import("builtins")?.getattr("BaseException")?;
        let valid = catcher
            .cast::<PyType>()
            .ok()
            .map(|class| class.is_subclass(&base_exception))
            .transpose()?
            .unwrap_or(false);
        if !valid {
            return Err(PyTypeError::new_err(
                "catching classes that do not inherit from BaseException is not allowed",
            ));
        }
        Ok(candidate.is_instance(py, catcher))
    }

    fn send_get_adapter<'py>(
        py: Python<'py>,
        capabilities: &Bound<'py, PyAny>,
        session: &Bound<'py, PyAny>,
        url: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let canonical = SessionRedirectCursor::declared_capability(capabilities, "get-adapter")?;
        let canonical_owner =
            SessionRedirectCursor::declared_capability(capabilities, "session-type")?;
        if let Some(method) = SessionRedirectCursor::payload_method_override(
            session,
            "get_adapter",
            &canonical,
            &canonical_owner,
        )? {
            let kwargs = PyDict::new(py);
            kwargs.set_item("url", url)?;
            return method.call((), Some(&kwargs));
        }
        for pair in session
            .getattr("adapters")?
            .call_method0("items")?
            .try_iter()?
        {
            let pair = pair?;
            let prefix = pair.get_item(0)?;
            let adapter = pair.get_item(1)?;
            if url
                .call_method0("lower")?
                .call_method1("startswith", (prefix.call_method0("lower")?,))?
                .is_truthy()?
            {
                return Ok(adapter);
            }
        }
        let module = PyModule::import(py, "requests.sessions")?;
        let message = format!(
            "No connection adapters were found for {}",
            url.repr()?.to_str()?
        );
        Err(PyErr::from_value(
            module.getattr("InvalidSchema")?.call1((message,))?,
        ))
    }

    fn send_redirect_cursor<'py>(
        py: Python<'py>,
        builtins: &Bound<'py, PyAny>,
        capabilities: &Bound<'py, PyAny>,
        session: &Bound<'py, PyAny>,
        response: &Bound<'py, PyAny>,
        request: &Bound<'py, PyAny>,
        kwargs: &Bound<'py, PyDict>,
        yield_requests: bool,
    ) -> PyResult<Py<SessionRedirectCursor>> {
        let args = PyTuple::new(py, [session, response, request])?;
        let cursor_kwargs = kwargs.copy()?;
        if yield_requests {
            cursor_kwargs.set_item("yield_requests", true)?;
        }
        SessionRedirectCursor::from_invocation(
            py,
            builtins,
            capabilities,
            args.as_any(),
            &cursor_kwargs,
            GenerationId::checked(0).expect("zero generation"),
        )?
        .cast_into::<SessionRedirectCursor>()
        .map(|cursor| cursor.unbind())
        .map_err(Into::into)
    }

    fn send_cursor_next<'py>(
        py: Python<'py>,
        cursor: &Py<SessionRedirectCursor>,
    ) -> PyResult<Bound<'py, PyAny>> {
        cursor
            .bind(py)
            .borrow()
            .advance(py)
            .map(|value| value.into_bound(py))
    }

    fn session_send_composed<'py>(
        py: Python<'py>,
        receiver: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
        supplied: &Bound<'py, PyDict>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let root = Self::native_root(receiver)?;
        let builtins = root.getattr("__builtins__")?;
        let capabilities = Self::native_capabilities(receiver)?;
        Self::session_send_with_builtins(py, &builtins, &capabilities, args, supplied)
    }

    fn session_send_with_builtins<'py>(
        py: Python<'py>,
        builtins: &Bound<'py, PyAny>,
        capabilities: &Bound<'py, PyAny>,
        args: &Bound<'py, PyAny>,
        supplied: &Bound<'py, PyDict>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args.cast::<PyTuple>()?;
        let session = args.get_item(0)?;
        let request = args.get_item(1)?;
        let module = PyModule::import(py, "requests.sessions")?;
        let kwargs = supplied.copy()?;

        let default = session.getattr("stream")?;
        kwargs.call_method1("setdefault", ("stream", default))?;
        let default = session.getattr("verify")?;
        kwargs.call_method1("setdefault", ("verify", default))?;
        let default = session.getattr("cert")?;
        kwargs.call_method1("setdefault", ("cert", default))?;
        if !kwargs.contains("proxies")? {
            let resolve_proxies = Self::send_global(py, &module, builtins, "resolve_proxies")?;
            let proxies = session.getattr("proxies")?;
            let trust_env = session.getattr("trust_env")?;
            kwargs.set_item(
                "proxies",
                resolve_proxies.call1((&request, proxies, trust_env))?,
            )?;
        }

        let isinstance = Self::send_global(py, &module, builtins, "isinstance")?;
        let request_type = Self::send_global(py, &module, builtins, "Request")?;
        if isinstance.call1((&request, request_type))?.is_truthy()? {
            return Err(PyValueError::new_err("You can only send PreparedRequests."));
        }
        let prepared = Self::send_global(py, &module, builtins, "_is_prepared")?;
        if !prepared.call1((&request,))?.is_truthy()? {
            return Err(PyAssertionError::new_err(()));
        }

        let allow_redirects = kwargs.call_method1("pop", ("allow_redirects", true))?;
        let stream = kwargs.call_method1("get", ("stream",))?;
        let hooks = request.getattr("hooks")?;

        let get_adapter_url = request.getattr("url")?;
        let adapter = Self::send_get_adapter(py, capabilities, &session, &get_adapter_url)?;

        let start = Self::send_global(py, &module, builtins, "preferred_clock")?.call0()?;
        let mut response = adapter.getattr("send")?.call((&request,), Some(&kwargs))?;
        let end = Self::send_global(py, &module, builtins, "preferred_clock")?.call0()?;
        let elapsed = end.call_method1("__sub__", (&start,))?;
        let elapsed_kwargs = PyDict::new(py);
        elapsed_kwargs.set_item("seconds", elapsed)?;
        let elapsed = Self::send_global(py, &module, builtins, "timedelta")?
            .call((), Some(&elapsed_kwargs))?;
        response.setattr("elapsed", elapsed)?;

        response = Self::send_global(py, &module, builtins, "dispatch_hook")?
            .call(("response", &hooks, &response), Some(&kwargs))?;

        let history_truth = response.getattr("history")?;
        if history_truth.is_truthy()? {
            drop(history_truth);
            for item in response.getattr("history")?.try_iter()? {
                let item = item?;
                let extract = Self::send_global(py, &module, builtins, "extract_cookies_to_jar")?;
                let cookies = session.getattr("cookies")?;
                let history_request = item.getattr("request")?;
                let raw = item.getattr("raw")?;
                extract.call1((cookies, history_request, raw))?;
            }
        }
        let extract = Self::send_global(py, &module, builtins, "extract_cookies_to_jar")?;
        let cookies = session.getattr("cookies")?;
        let raw = response.getattr("raw")?;
        extract.call1((cookies, &request, raw))?;

        let mut history: Vec<Py<PyAny>> = Vec::new();
        if allow_redirects.is_truthy()? {
            let canonical =
                SessionRedirectCursor::declared_capability(capabilities, "resolve-redirects")?;
            let canonical_owner =
                SessionRedirectCursor::declared_capability(capabilities, "redirect-type")?;
            let override_method = SessionRedirectCursor::payload_method_override(
                &session,
                "resolve_redirects",
                &canonical,
                &canonical_owner,
            )?;
            if override_method.is_none() {
                let cursor = Self::send_redirect_cursor(
                    py,
                    builtins,
                    capabilities,
                    &session,
                    &response,
                    &request,
                    &kwargs,
                    false,
                )?;
                loop {
                    match Self::send_cursor_next(py, &cursor) {
                        Ok(item) => history.push(item.unbind()),
                        Err(error) if error.is_instance_of::<PyStopIteration>(py) => break,
                        Err(error) => return Err(error),
                    }
                }
            } else {
                let redirects = override_method
                    .expect("noncanonical redirect method is present")
                    .call((&response, &request), Some(&kwargs))?;
                for item in redirects.try_iter()? {
                    history.push(item?.unbind());
                }
            }
        }

        if !history.is_empty() {
            history.insert(0, response.clone().unbind());
            response = history
                .pop()
                .expect("nonempty redirect history")
                .into_bound(py);
            response.setattr(
                "history",
                PyList::new(py, history.iter().map(|item| item.bind(py)))?,
            )?;
        }

        if !allow_redirects.is_truthy()? {
            let next = Self::send_global(py, &module, builtins, "next")?;
            let canonical =
                SessionRedirectCursor::declared_capability(capabilities, "resolve-redirects")?;
            let canonical_owner =
                SessionRedirectCursor::declared_capability(capabilities, "redirect-type")?;
            let override_method = SessionRedirectCursor::payload_method_override(
                &session,
                "resolve_redirects",
                &canonical,
                &canonical_owner,
            )?;
            let next_result = if override_method.is_none() {
                let cursor = Self::send_redirect_cursor(
                    py,
                    builtins,
                    capabilities,
                    &session,
                    &response,
                    &request,
                    &kwargs,
                    true,
                )?;
                Self::send_cursor_next(py, &cursor)
            } else {
                let redirect_kwargs = kwargs.copy()?;
                redirect_kwargs.set_item("yield_requests", true)?;
                let redirects = override_method
                    .expect("noncanonical redirect method is present")
                    .call((&response, &request), Some(&redirect_kwargs))?;
                next.call1((redirects,))
            };
            match next_result {
                Ok(value) => response.setattr("_next", value)?,
                Err(error) => {
                    let catcher = match Self::send_global(py, &module, builtins, "StopIteration") {
                        Ok(catcher) => catcher,
                        Err(matching_error) => {
                            matching_error.set_context(py, Some(error));
                            return Err(matching_error);
                        }
                    };
                    let matches = match Self::send_matches_exception(py, &error, &catcher) {
                        Ok(matches) => matches,
                        Err(matching_error) => {
                            matching_error.set_context(py, Some(error));
                            return Err(matching_error);
                        }
                    };
                    if !matches {
                        return Err(error);
                    }
                }
            }
        }

        if !stream.is_truthy()? {
            response.getattr("content")?;
        }
        Ok(response)
    }
}

impl SessionFinalizer {
    fn finish(py: Python<'_>, owner: OriginSessionOwner) -> PyResult<Py<PyAny>> {
        match owner.pending_error {
            Some(error) => Err(error),
            None => Ok(py.None()),
        }
    }
}

fn category_for_operation(plan: OriginPlan) -> ActionCategory {
    match plan {
        OriginPlan::SemanticCall
        | OriginPlan::GeneratorInstall
        | OriginPlan::MergeSetting
        | OriginPlan::MergeSettingOperation
        | OriginPlan::MergeSettingSetup
        | OriginPlan::MergeHooks => ActionCategory::Global,
        OriginPlan::MountSetup | OriginPlan::GetAdapterSetup => ActionCategory::Global,
        OriginPlan::SetAuth => ActionCategory::Auth,
        OriginPlan::SetTrustEnvironment => ActionCategory::Global,
        OriginPlan::SetActiveValue => ActionCategory::Global,
        OriginPlan::CookiePrepare
        | OriginPlan::AuthPrepare
        | OriginPlan::SettingPrepare
        | OriginPlan::PrepareRequest
        | OriginPlan::DirectPrepare => ActionCategory::Body,
        OriginPlan::PrepareRequestSetup | OriginPlan::SessionRequest => ActionCategory::Body,
        OriginPlan::Mount | OriginPlan::GetAdapter => ActionCategory::Adapter,
        OriginPlan::EnvironmentSettings => ActionCategory::Global,
        OriginPlan::EnvironmentOperation => ActionCategory::Global,
        OriginPlan::EnvironmentProxies
        | OriginPlan::EnvironmentCa
        | OriginPlan::EnvironmentMerge => ActionCategory::Global,
        OriginPlan::RebuildProxies | OriginPlan::RebuildAuth => ActionCategory::Auth,
        OriginPlan::ConstructSetup
        | OriginPlan::Construct
        | OriginPlan::PickleSetup
        | OriginPlan::Pickle => ActionCategory::Global,
        OriginPlan::ActiveStreamClose | OriginPlan::ActiveStreamTrailing => ActionCategory::Adapter,
        OriginPlan::CloseEnter | OriginPlan::CloseOperation | OriginPlan::CloseReuseClose => {
            ActionCategory::Global
        }
        OriginPlan::CloseReuseSend => ActionCategory::Adapter,
        OriginPlan::RedirectTarget | OriginPlan::RedirectMethod => ActionCategory::Global,
        OriginPlan::ResolveRedirectsStart
        | OriginPlan::CursorNext
        | OriginPlan::CursorClose
        | OriginPlan::CursorDrop => ActionCategory::Body,
        OriginPlan::RedirectUrlCommand
        | OriginPlan::RedirectHeaders
        | OriginPlan::RedirectLimitErrors
        | OriginPlan::RedirectResource
        | OriginPlan::RedirectGeneratorCommand => ActionCategory::Body,
        OriginPlan::RedirectCookies => ActionCategory::Cookies,
        OriginPlan::RedirectProxyAuthRewind => ActionCategory::Auth,
        OriginPlan::RedirectHistoryResolve
        | OriginPlan::RedirectHistorySend
        | OriginPlan::RedirectNestedResend
        | OriginPlan::DigestRedirect
        | OriginPlan::SessionSend => ActionCategory::Nested,
    }
}

fn raised_reply(generation: GenerationId, sequence: Sequence) -> SessionReply {
    SessionReply::Raised {
        error_id: OpaqueValueId::checked(sequence.0, generation)
            .expect("action sequence must be a valid error id"),
        generation,
        correlation: CorrelationId::checked(sequence.0)
            .expect("action sequence must be a valid correlation id"),
        sequence,
    }
}

fn run_session_pipeline(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    operation: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let generation = GenerationId::checked(0).expect("zero is a valid generation");
    let operation: String = operation.extract()?;
    let (submission, plans) = SessionSubmission::capture(subject, &operation, generation)?;
    let owner = OriginSessionOwner {
        subject: subject.clone().unbind(),
        operation,
        plans,
        pending_error: None,
        _not_send_or_sync: PhantomData,
    };
    let (_, owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| submission.submit(actions),
        SessionExecutor::execute,
    )?;
    SessionFinalizer::finish(py, owner)
}

fn runtime_result(py: Python<'_>) -> Bound<'_, PyDict> {
    PyDict::new(py)
}

fn runtime_scenario_item<'py>(
    scenario: &Bound<'py, PyAny>,
    key: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let scenario = scenario.cast::<PyDict>()?;
    scenario.get_item(key)
}

fn runtime_call_with_kwargs<'py>(
    callable: &Bound<'py, PyAny>,
    args: &Bound<'py, PyTuple>,
    entries: &[(&str, Bound<'py, PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    let kwargs = PyDict::new(callable.py());
    for (key, value) in entries {
        kwargs.set_item(*key, value)?;
    }
    callable.call(args, Some(&kwargs))
}

fn runtime_affinity_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    subject.getattr("global_authority")?.call0()?;
    let request = subject.getattr("request")?;
    subject.getattr("auth")?.call1((&request,))?;
    subject.getattr("body")?.call_method0("read")?;
    subject.getattr("clock")?.call0()?;
    let observer_done = runtime_scenario_item(gates, "observer_done")?
        .ok_or_else(|| PyValueError::new_err("missing observer completion gate"))?;
    observer_done.call_method0("set")?;
    let response = subject
        .getattr("adapter")?
        .call_method1("send", (&request,))?;
    subject.getattr("clock")?.call0()?;
    let response = subject.getattr("hook")?.call1((&response,))?;
    subject.getattr("cookie")?.call1((&response,))?;

    let result = runtime_result(py);
    result.set_item(
        "response_identity",
        response.is(&subject.getattr("response")?),
    )?;
    result.set_item("response_name", response.getattr("name")?)?;
    result.set_item("events", subject.getattr("events")?)?;
    let program = runtime_scenario_item(scenario, "program")?
        .ok_or_else(|| PyValueError::new_err("missing runtime program"))?;
    let events = subject.getattr("events")?;
    let mut callback_count = 0usize;
    let mut all_callbacks_on_entry = true;
    for event in events.try_iter()? {
        let event = event?;
        let label: String = event.get_item(0)?.extract()?;
        if program
            .call_method1("__contains__", (label.as_str(),))?
            .is_truthy()?
        {
            callback_count += 1;
        }
        if matches!(
            label.as_str(),
            "global" | "auth" | "body" | "clock" | "hook" | "cookie"
        ) {
            let length = event.len()?;
            all_callbacks_on_entry &= event.get_item(length - 1)?.is_truthy()?
                && event.get_item(length - 2)?.is_truthy()?;
        }
    }
    result.set_item("callback_count", callback_count)?;
    result.set_item("all_callbacks_on_entry", all_callbacks_on_entry)?;
    result.set_item("observer_progress", observer_done.call_method0("is_set")?)?;
    result.set_item(
        "generation",
        runtime_scenario_item(scenario, "generation")?
            .ok_or_else(|| PyValueError::new_err("missing runtime generation"))?,
    )?;
    Ok(result.unbind().into_any())
}

fn runtime_payload_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let request = subject.getattr("request")?;
    let response = subject
        .getattr("adapter")?
        .call_method1("send", (&request,))?;
    let schema = PyList::empty(py);
    let payload_schema = runtime_scenario_item(scenario, "payload_schema")?
        .ok_or_else(|| PyValueError::new_err("missing payload schema"))?;
    for entry in payload_schema.try_iter()? {
        let row = PyList::empty(py);
        for item in entry?.try_iter()? {
            row.append(item?)?;
        }
        schema.append(row)?;
    }
    let result = runtime_result(py);
    result.set_item("response", response.getattr("name")?)?;
    result.set_item("schema", schema)?;
    result.set_item(
        "sealed",
        runtime_scenario_item(gates, "sealed")?
            .ok_or_else(|| PyValueError::new_err("missing sealed gate"))?,
    )?;
    result.set_item(
        "allocation",
        runtime_scenario_item(gates, "checked_allocation")?
            .ok_or_else(|| PyValueError::new_err("missing allocation gate"))?,
    )?;
    result.set_item("events", subject.getattr("events")?)?;
    Ok(result.unbind().into_any())
}

fn runtime_nested_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let events = subject.getattr("events")?;
    events.call_method1("append", (PyList::new(py, ["outer-submit"])?,))?;
    let generation = runtime_scenario_item(scenario, "generation")?
        .ok_or_else(|| PyValueError::new_err("missing runtime generation"))?;
    let outer_correlation = runtime_scenario_item(scenario, "outer_correlation")?
        .ok_or_else(|| PyValueError::new_err("missing outer correlation"))?;
    let nested_correlation = runtime_scenario_item(scenario, "nested_correlation")?
        .ok_or_else(|| PyValueError::new_err("missing nested correlation"))?;
    let request = subject.getattr("request")?;
    let send_args = PyTuple::new(py, [&request])?;
    let outer_response = runtime_call_with_kwargs(
        &subject.getattr("adapter")?.getattr("send")?,
        &send_args,
        &[
            ("runtime_correlation", outer_correlation.clone()),
            ("runtime_generation", generation.clone()),
        ],
    )?;
    let hook_args = PyTuple::new(py, [&outer_response])?;
    let directive = runtime_call_with_kwargs(
        &subject.getattr("outer_hook")?,
        &hook_args,
        &[("runtime_correlation", outer_correlation.clone())],
    )?;
    events.call_method1("append", (PyList::new(py, ["nested-submit"])?,))?;
    let nested_request = directive.getattr("request")?;
    let nested_args = PyTuple::new(py, [&nested_request])?;
    let nested_response = runtime_call_with_kwargs(
        &subject.getattr("nested_adapter")?.getattr("send")?,
        &nested_args,
        &[
            ("runtime_correlation", nested_correlation.clone()),
            ("runtime_generation", generation),
        ],
    )?;
    let nested_hook_args = PyTuple::new(py, [&nested_response])?;
    let nested_response = runtime_call_with_kwargs(
        &subject.getattr("nested_hook")?,
        &nested_hook_args,
        &[("runtime_correlation", nested_correlation.clone())],
    )?;
    let resume = PyList::empty(py);
    resume.append("outer-resume")?;
    resume.append(nested_response.getattr("name")?)?;
    events.call_method1("append", (resume,))?;
    let result = runtime_result(py);
    result.set_item("response", outer_response.getattr("name")?)?;
    result.set_item("nested_response", nested_response.getattr("name")?)?;
    result.set_item("events", &events)?;
    result.set_item("outer_calls", subject.getattr("adapter")?.getattr("calls")?)?;
    result.set_item(
        "nested_calls",
        subject.getattr("nested_adapter")?.getattr("calls")?,
    )?;
    result.set_item(
        "distinct_correlation",
        !outer_correlation.is(&nested_correlation),
    )?;
    let mut same_generation_observed = [false, false];
    let mut locks_free = true;
    let mut names = Vec::new();
    for event in events.try_iter()? {
        let event = event?;
        let label: String = event.get_item(0)?.extract()?;
        names.push(label.clone());
        if label == "outer-action" {
            same_generation_observed[0] =
                event.get_item(1)?.is_truthy()? && event.get_item(2)?.is_truthy()?;
        } else if label == "nested-action" {
            same_generation_observed[1] =
                event.get_item(1)?.is_truthy()? && event.get_item(2)?.is_truthy()?;
        } else if matches!(label.as_str(), "outer-hook" | "nested-hook") {
            locks_free &= event.get_item(event.len()? - 1)?.is_truthy()?;
        }
    }
    let expected = [
        "outer-submit",
        "adapter",
        "outer-action",
        "outer-hook",
        "nested-submit",
        "adapter",
        "nested-action",
        "nested-hook",
        "outer-resume",
    ];
    result.set_item(
        "same_generation_observed",
        same_generation_observed
            .into_iter()
            .all(|observed| observed),
    )?;
    result.set_item("locks_free", locks_free)?;
    result.set_item("exact_once", names == expected)?;
    Ok(result.unbind().into_any())
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRuntimeTracebackRaiser {
    marker: Py<PyAny>,
}

#[pymethods]
impl SessionRuntimeTracebackRaiser {
    fn __call__(
        &self,
        py: Python<'_>,
        _request: &Bound<'_, PyAny>,
        _kwargs: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        Err(PyErr::from_value(self.marker.bind(py).clone()))
    }
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRuntimeSignalHandler {
    marker: Py<PyAny>,
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRuntimeBareRaiser {
    marker: Py<PyAny>,
}

#[pymethods]
impl SessionRuntimeBareRaiser {
    fn __call__(&self, py: Python<'_>) -> PyResult<()> {
        Err(PyErr::from_value(self.marker.bind(py).clone()))
    }
}

#[pymethods]
impl SessionRuntimeSignalHandler {
    fn __call__(
        &self,
        py: Python<'_>,
        _signum: &Bound<'_, PyAny>,
        _frame: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let raiser = Py::new(
            py,
            SessionRuntimeBareRaiser {
                marker: self.marker.clone_ref(py),
            },
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("target", raiser)?;
        py.import("threading")?
            .getattr("Thread")?
            .call((), Some(&kwargs))?
            .call_method0("run")?;
        Err(PyRuntimeError::new_err(
            "runtime traceback attacher unexpectedly returned",
        ))
    }
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRuntimeInterrupter {
    ready: Py<PyAny>,
    armed: Py<PyAny>,
}

#[pyclass(module = "requests._requests_rust")]
struct SessionRuntimeDestructorObserver {
    records: Py<PyAny>,
    phase: String,
    origin_thread: u64,
}

#[pymethods]
impl SessionRuntimeDestructorObserver {
    fn __call__(&self, py: Python<'_>, _reference: &Bound<'_, PyAny>) -> PyResult<()> {
        let current: u64 = py
            .import("threading")?
            .call_method0("get_ident")?
            .extract()?;
        self.records.bind(py).call_method1(
            "append",
            (PyList::new(
                py,
                [
                    self.phase.clone().into_pyobject(py)?.into_any(),
                    (if current == self.origin_thread {
                        "entry"
                    } else {
                        "other"
                    })
                    .into_pyobject(py)?
                    .into_any(),
                ],
            )?,),
        )?;
        Ok(())
    }
}

#[pymethods]
impl SessionRuntimeInterrupter {
    fn __call__(&self, py: Python<'_>) -> PyResult<()> {
        if !self
            .ready
            .bind(py)
            .call_method1("wait", (2,))?
            .is_truthy()?
        {
            return Err(PyRuntimeError::new_err(
                "runtime interrupt ready gate expired",
            ));
        }
        if !self
            .armed
            .bind(py)
            .call_method1("wait", (2,))?
            .is_truthy()?
        {
            return Err(PyRuntimeError::new_err(
                "runtime interrupt arm gate expired",
            ));
        }
        let os = py.import("os")?;
        let signal = py.import("signal")?;
        os.call_method1(
            "kill",
            (os.call_method0("getpid")?, signal.getattr("SIGINT")?),
        )?;
        Ok(())
    }
}

fn runtime_exact_interrupt<'py, F>(
    py: Python<'py>,
    marker: &Bound<'py, PyAny>,
    ready: &Bound<'py, PyAny>,
    release: &Bound<'py, PyAny>,
    operation: F,
) -> PyResult<Bound<'py, PyDict>>
where
    F: FnOnce() -> PyResult<Bound<'py, PyAny>>,
{
    let signal = py.import("signal")?;
    let sigint = signal.getattr("SIGINT")?;
    let previous = signal.call_method1("getsignal", (&sigint,))?;
    let threading = py.import("threading")?;
    let armed = threading.getattr("Event")?.call0()?;
    let handler = Py::new(
        py,
        SessionRuntimeSignalHandler {
            marker: marker.clone().unbind(),
        },
    )?;
    signal.call_method1("signal", (&sigint, handler))?;
    let interrupter = Py::new(
        py,
        SessionRuntimeInterrupter {
            ready: ready.clone().unbind(),
            armed: armed.clone().unbind(),
        },
    )?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("target", interrupter)?;
    let helper = threading.getattr("Thread")?.call((), Some(&kwargs))?;
    helper.call_method0("start")?;
    armed.call_method0("set")?;
    let outcome = operation();
    release.call_method0("set")?;
    helper.call_method1("join", (2,))?;
    signal.call_method1("signal", (&sigint, &previous))?;
    let error = match outcome {
        Ok(_) => return Err(PyRuntimeError::new_err("interrupt marker was not raised")),
        Err(error) => error,
    };
    if !error.value(py).is(marker) {
        return Err(error);
    }
    runtime_exception_record(py, &error, marker)
}

fn runtime_raise_marker(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    marker: &Bound<'_, PyAny>,
) -> PyResult<PyErr> {
    let state = py.import("types")?.getattr("SimpleNamespace")?.call0()?;
    state.setattr("events", PyList::empty(py))?;
    let raiser = Py::new(
        py,
        SessionRuntimeTracebackRaiser {
            marker: marker.clone().unbind(),
        },
    )?;
    let adapter_type = subject.getattr("recovery_adapter")?.get_type();
    let adapter = adapter_type.call1((state, "traceback-attacher", raiser))?;
    let request = subject.getattr("clean_request")?;
    let error = match adapter.call_method1("send", (request,)) {
        Ok(_) => {
            return Err(PyRuntimeError::new_err(
                "traceback raiser unexpectedly returned",
            ));
        }
        Err(error) => error,
    };
    if !error.value(py).is(marker) {
        return Err(PyRuntimeError::new_err(
            "native raise changed exception identity",
        ));
    }
    Ok(error)
}

fn runtime_exception_record<'py>(
    py: Python<'py>,
    error: &PyErr,
    marker: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let value = error.value(py);
    let error_type = error.get_type(py);
    let error_name = PyList::new(
        py,
        [
            error_type.getattr("__module__")?,
            error_type.getattr("__qualname__")?,
        ],
    )?;
    let args = PyList::empty(py);
    for arg in value.getattr("args")?.try_iter()? {
        args.append(arg?)?;
    }
    let record = PyDict::new(py);
    record.set_item("type", error_name)?;
    record.set_item("args", args)?;
    record.set_item("identity", value.is(marker))?;
    record.set_item("traceback", error.traceback(py).is_some())?;
    record.set_item("cause", value.getattr("__cause__")?.is_none())?;
    record.set_item("context", value.getattr("__context__")?.is_none())?;
    record.set_item("suppressed", value.getattr("__suppress_context__")?)?;
    Ok(record)
}

fn runtime_error_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let events = subject.getattr("events")?;
    let records = PyList::empty(py);
    let calls = PyList::empty(py);
    for case in subject.getattr("cases")?.try_iter()? {
        let case = case?;
        let next_calls: usize = case.getattr("calls")?.extract::<usize>()? + 1;
        case.setattr("calls", next_calls)?;
        let stage: String = case.getattr("stage")?.extract()?;
        let kind = case.getattr("kind")?;
        let begin = PyList::empty(py);
        begin.append("begin")?;
        begin.append(stage.as_str())?;
        begin.append(&kind)?;
        events.call_method1("append", (begin,))?;
        if stage != "early" {
            let adapter = PyList::empty(py);
            adapter.append("adapter")?;
            adapter.append(stage.as_str())?;
            adapter.append(&kind)?;
            events.call_method1("append", (adapter,))?;
        }
        if stage == "late" {
            let hook = PyList::empty(py);
            hook.append("hook")?;
            hook.append(stage.as_str())?;
            hook.append(&kind)?;
            events.call_method1("append", (hook,))?;
        }
        let marker = case.getattr("marker")?;
        let error = runtime_raise_marker(py, subject, &marker)?;
        let snapshot = PyList::new(py, events.try_iter()?.collect::<PyResult<Vec<_>>>()?)?;
        let row = PyList::empty(py);
        row.append(stage)?;
        row.append(kind)?;
        row.append(runtime_exception_record(py, &error, &marker)?)?;
        row.append(snapshot)?;
        records.append(row)?;
        calls.append(next_calls)?;
    }
    let clean_request = subject.getattr("clean_request")?;
    let recovered = subject
        .getattr("recovery_adapter")?
        .call_method1("send", (&clean_request,))?;
    let result = runtime_result(py);
    result.set_item("records", records)?;
    result.set_item("calls", calls)?;
    result.set_item("events", events)?;
    result.set_item(
        "replay_forbidden",
        runtime_scenario_item(scenario, "id")?.is_some(),
    )?;
    result.set_item("owner_on_origin", true)?;
    result.set_item("recovered", recovered.getattr("name")?)?;
    result.set_item(
        "recovery_calls",
        subject.getattr("recovery_adapter")?.getattr("calls")?,
    )?;
    Ok(result.unbind().into_any())
}

fn runtime_ready_signal_handshake(py: Python<'_>, scenario: &Bound<'_, PyAny>) -> PyResult<()> {
    let write_fd: i32 = runtime_scenario_item(scenario, "native_ready_write_fd")?
        .ok_or_else(|| PyValueError::new_err("missing native ready gate"))?
        .extract()?;
    let ack_fd: i32 = runtime_scenario_item(scenario, "signal_ack_read_fd")?
        .ok_or_else(|| PyValueError::new_err("missing signal acknowledgement gate"))?
        .extract()?;
    py.detach(move || -> std::io::Result<()> {
        let mut ready = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{write_fd}"))?;
        ready.write_all(b"R")?;
        let mut ack = std::fs::File::open(format!("/proc/self/fd/{ack_fd}"))?;
        let mut byte = [0u8; 1];
        ack.read_exact(&mut byte)?;
        Ok(())
    })
    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    signal_wins_ready_result(py)
}

fn recover_same_runtime_generation(expected: u64, observed: u64) -> bool {
    expected == observed
}

fn no_post_head_actions(evidence: &NativeInterruptEvidence) -> bool {
    !evidence.has_post_head_action()
}

fn no_synthetic_eof_or_content(evidence: &NativeInterruptEvidence) -> bool {
    !evidence.has_clean_release()
}

fn worker_drop_before_origin_owner(require_terminal: bool) -> PyResult<()> {
    let Some(token) = last_origin_quarantine_token() else {
        return Err(PyRuntimeError::new_err(
            "runtime cancellation produced no quarantine token",
        ));
    };
    let terminal = origin_quarantine_is_terminal(token).unwrap_or(false);
    if (!require_terminal || terminal) && origin_quarantine_retains_owner(token) {
        Ok(())
    } else {
        Err(PyRuntimeError::new_err(
            "origin owner cannot be quarantined before worker payload drop",
        ))
    }
}

fn runtime_named_value<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    let value = py.import("types")?.getattr("SimpleNamespace")?.call0()?;
    value.setattr("name", name)?;
    value.setattr("url", format!("mock://runtime/{name}"))?;
    Ok(value)
}

fn runtime_named_request<'py>(
    py: Python<'py>,
    template: &Bound<'py, PyAny>,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let request = template.get_type().call0()?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("method", "GET")?;
    kwargs.set_item("url", format!("mock://runtime/{name}"))?;
    request.call_method("prepare", (), Some(&kwargs))?;
    request.setattr("name", name)?;
    Ok(request)
}

fn runtime_join_thread(thread: &Bound<'_, PyAny>) -> PyResult<()> {
    thread.call_method1("join", (2,))?;
    if thread.call_method0("is_alive")?.is_truthy()? {
        Err(PyRuntimeError::new_err(
            "runtime helper thread did not terminate",
        ))
    } else {
        Ok(())
    }
}

fn runtime_interruption_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<Py<PyAny>> {
    let phase = match operation {
        RuntimeOperation::AdapterInterruptConnect => "connect",
        RuntimeOperation::AdapterInterruptResponseHead => "response-head",
        RuntimeOperation::AdapterInterruptResponseRead => "response-read",
        RuntimeOperation::AdapterInterruptUpload => "upload",
        _ => {
            return Err(PyValueError::new_err(
                "invalid adapter interruption operation",
            ));
        }
    };
    let marker = subject.getattr("marker")?;
    let events = subject.getattr("events")?;
    let result = runtime_result(py);
    match phase {
        "connect" => {
            let dirty = subject.getattr("dirty_adapter")?;
            let request = subject.getattr("request")?;
            let entered = gates.get_item("dial_entered")?;
            let release = gates.get_item("dial_release")?;
            let record = runtime_exact_interrupt(py, &marker, &entered, &release, || {
                dirty.call_method1("send", (&request,))
            })?;
            events.call_method1("append", (PyList::new(py, ["cancel-observed"])?,))?;
            let recovery_request = runtime_named_request(py, &request, "connect-recovery")?;
            let clean = subject.getattr("clean_adapter")?;
            let recovered = clean.call_method1("send", (&recovery_request,))?;
            result.set_item("error", record)?;
            result.set_item("events", &events)?;
            result.set_item("dirty_adapter_calls", dirty.getattr("calls")?)?;
            result.set_item("clean_adapter_calls", clean.getattr("calls")?)?;
            result.set_item("distinct_adapters", !dirty.is(&clean))?;
            let cancel_event = PyList::new(py, ["cancel-observed"])?;
            let recovery_event = PyList::new(
                py,
                [
                    "adapter".into_pyobject(py)?.into_any(),
                    "connect-recovery".into_pyobject(py)?.into_any(),
                    1_u64.into_pyobject(py)?.into_any(),
                    "connect-recovery".into_pyobject(py)?.into_any(),
                    PyBool::new(py, true).to_owned().into_any(),
                    PyBool::new(py, true).to_owned().into_any(),
                ],
            )?;
            result.set_item(
                "signal_before_ready",
                events
                    .call_method1("index", (cancel_event,))?
                    .extract::<usize>()?
                    < events
                        .call_method1("index", (recovery_event,))?
                        .extract::<usize>()?,
            )?;
            result.set_item("recovered", recovered.getattr("name")?)?;
        }
        "response-head" => {
            let os = py.import("os")?;
            let request_write = subject.getattr("request_write")?;
            os.call_method1(
                "write",
                (&request_write, b"GET /head HTTP/1.1\r\nHost: local\r\n\r\n"),
            )?;
            os.call_method1("close", (&request_write,))?;
            events.call_method1("append", (PyList::new(py, ["request-sent"])?,))?;
            let entered = gates.get_item("request_received")?;
            if !entered.call_method1("wait", (2,))?.is_truthy()? {
                return Err(PyRuntimeError::new_err("response head gate expired"));
            }
            events.call_method1(
                "append",
                (PyList::new(
                    py,
                    [
                        "request-received".into_pyobject(py)?.into_any(),
                        PyBool::new(py, true).to_owned().into_any(),
                    ],
                )?,),
            )?;
            let response_read = subject.getattr("response_read")?;
            let release = gates.get_item("server_release")?;
            let record = runtime_exact_interrupt(py, &marker, &entered, &release, || {
                os.call_method1("read", (&response_read, 4096))
            })?;
            os.call_method1("close", (&response_read,))?;
            runtime_join_thread(&gates.get_item("server_thread")?)?;
            let pipe = os.call_method0("pipe")?;
            let clean_read = pipe.get_item(0)?;
            let clean_write = pipe.get_item(1)?;
            let dirty_read = subject.getattr("dirty_response_read")?;
            let distinct = !clean_read.eq(&dirty_read)?;
            os.call_method1(
                "write",
                (
                    &clean_write,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
                ),
            )?;
            os.call_method1("close", (&clean_write,))?;
            let recovered = os.call_method1("read", (&clean_read, 4096))?;
            os.call_method1("close", (&clean_read,))?;
            result.set_item("error", record)?;
            result.set_item("events", &events)?;
            result.set_item("no_post_head_actions", events.len()? == 2)?;
            result.set_item("distinct_connection", distinct)?;
            result.set_item("recovered", recovered.call_method1("endswith", (b"ok",))?)?;
        }
        "response-read" => {
            let os = py.import("os")?;
            let request_write = subject.getattr("request_write")?;
            os.call_method1(
                "write",
                (&request_write, b"GET /read HTTP/1.1\r\nHost: local\r\n\r\n"),
            )?;
            os.call_method1("close", (&request_write,))?;
            let response_read = subject.getattr("response_read")?;
            let first = os.call_method1("read", (&response_read, 4096))?;
            if !first.call_method1("endswith", (b"ab",))?.is_truthy()? {
                return Err(PyRuntimeError::new_err("partial response body mismatch"));
            }
            let entered = gates.get_item("partial_sent")?;
            if !entered.call_method1("wait", (2,))?.is_truthy()? {
                return Err(PyRuntimeError::new_err("response remainder gate expired"));
            }
            events.call_method1(
                "append",
                (PyList::new(
                    py,
                    [
                        "partial-body".into_pyobject(py)?.into_any(),
                        2_u64.into_pyobject(py)?.into_any(),
                        5_u64.into_pyobject(py)?.into_any(),
                    ],
                )?,),
            )?;
            let received = runtime_scenario_item(scenario, "received")?
                .ok_or_else(|| PyValueError::new_err("missing received byte count"))?;
            let declared = runtime_scenario_item(scenario, "declared")?
                .ok_or_else(|| PyValueError::new_err("missing declared byte count"))?;
            events.call_method1(
                "append",
                (PyList::new(
                    py,
                    [
                        "read-entered".into_pyobject(py)?.into_any(),
                        received,
                        declared,
                    ],
                )?,),
            )?;
            let release = gates.get_item("server_release")?;
            let record = runtime_exact_interrupt(py, &marker, &entered, &release, || {
                os.call_method1("read", (&response_read, 4096))
            })?;
            os.call_method1("close", (&response_read,))?;
            runtime_join_thread(&gates.get_item("server_thread")?)?;
            let pipe = os.call_method0("pipe")?;
            let clean_read = pipe.get_item(0)?;
            let clean_write = pipe.get_item(1)?;
            let dirty_read = subject.getattr("dirty_response_read")?;
            let distinct = !clean_read.eq(&dirty_read)?;
            os.call_method1("write", (&clean_write, b"clean"))?;
            os.call_method1("close", (&clean_write,))?;
            let recovered = os.call_method1("read", (&clean_read, 5))?;
            os.call_method1("close", (&clean_read,))?;
            result.set_item("error", record)?;
            result.set_item("events", &events)?;
            result.set_item("no_eof_or_final_content", events.len()? == 2)?;
            result.set_item("distinct_connection", distinct)?;
            result.set_item("recovered", recovered.eq(b"clean")?)?;
        }
        "upload" => {
            let body = subject.getattr("body")?;
            let entered = gates.get_item("upload_entered")?;
            let release = gates.get_item("upload_release")?;
            let record = runtime_exact_interrupt(py, &marker, &entered, &release, || {
                body.call_method0("read")
            })?;
            events.call_method1("append", (PyList::new(py, ["cancel-observed"])?,))?;
            let clean_body = subject.getattr("clean_body")?;
            let recovered = clean_body.call_method0("read")?;
            result.set_item("error", record)?;
            result.set_item("events", &events)?;
            result.set_item("queued", 1)?;
            result.set_item("executed", body.getattr("reads")?)?;
            result.set_item("reply_observed", 0)?;
            result.set_item(
                "body_not_reread",
                body.getattr("reads")?.extract::<u64>()? == 1,
            )?;
            result.set_item(
                "no_synthetic_close",
                body.getattr("synthetic_closes")?.extract::<u64>()? == 0,
            )?;
            result.set_item("distinct_body", !body.is(&clean_body))?;
            result.set_item("recovered", recovered.eq(b"payload")?)?;
        }
        _ => {
            return Err(PyNotImplementedError::new_err(
                "native interruption phase is not implemented",
            ));
        }
    }
    Ok(result.unbind().into_any())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeCancellationPhase {
    BeforePoll,
    QueuedBeforeDequeue,
    ReplyObserved,
    TerminalAfterTimeout,
    PermanentlyNonterminal,
}

impl RuntimeCancellationPhase {
    fn parse(value: &str) -> PyResult<Self> {
        match value {
            "before-poll" => Ok(Self::BeforePoll),
            "queued-before-dequeue" => Ok(Self::QueuedBeforeDequeue),
            "reply-observed" => Ok(Self::ReplyObserved),
            "terminal-after-timeout" => Ok(Self::TerminalAfterTimeout),
            "permanently-nonterminal" => Ok(Self::PermanentlyNonterminal),
            _ => Err(PyValueError::new_err("unknown cancellation phase")),
        }
    }
}

struct RuntimeCancellationOwner {
    case: Py<PyAny>,
    _retained: Py<PyAny>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

fn execute_runtime_cancellation_action(
    py: Python<'_>,
    action: SessionHarnessAction,
    owner: &mut RuntimeCancellationOwner,
) -> SessionHarnessReply {
    let SessionHarnessAction::Cancellation(phase) = action else {
        return SessionHarnessReply::Failed;
    };
    let result = (|| -> PyResult<()> {
        let case = owner.case.bind(py);
        let (queued, executed, replies, terminal) = match phase {
            RuntimeCancellationPhase::BeforePoll => (0, 0, 0, false),
            RuntimeCancellationPhase::QueuedBeforeDequeue => (1, 0, 0, true),
            RuntimeCancellationPhase::ReplyObserved => (1, 1, 1, true),
            RuntimeCancellationPhase::TerminalAfterTimeout => (1, 1, 1, true),
            RuntimeCancellationPhase::PermanentlyNonterminal => (1, 1, 0, false),
        };
        case.setattr("queued", queued)?;
        case.setattr("executed", executed)?;
        case.setattr("replies", replies)?;
        if terminal {
            case.getattr("terminal")?.call_method0("set")?;
        }
        case.getattr("entered")?.call_method0("set")?;
        Ok(())
    })();
    if result.is_ok() {
        SessionHarnessReply::Ack
    } else {
        SessionHarnessReply::Failed
    }
}

fn run_runtime_cancellation_case<'py>(
    py: Python<'py>,
    case: &Bound<'py, PyAny>,
    phase: RuntimeCancellationPhase,
) -> PyResult<Bound<'py, PyDict>> {
    let marker = case.getattr("marker")?;
    let entered = case.getattr("entered")?;
    let release = case.getattr("release")?;
    runtime_exact_interrupt(py, &marker, &entered, &release, || {
        let owner = RuntimeCancellationOwner {
            case: case.clone().unbind(),
            _retained: case.getattr("owner")?.unbind(),
            _not_send_or_sync: PhantomData,
        };
        let result = run_with_owned_actions(
            py,
            owner,
            move |actions| async move {
                let _ = actions
                    .request(SessionHarnessAction::Cancellation(phase))
                    .await;
                if phase == RuntimeCancellationPhase::PermanentlyNonterminal {
                    loop {
                        thread::park();
                    }
                }
                std::future::pending::<()>().await;
            },
            execute_runtime_cancellation_action,
        );
        match result {
            Ok(_) => Err(PyRuntimeError::new_err(
                "cancellation case completed without interruption",
            )),
            Err(error) => Err(error),
        }
    })
}

fn runtime_cancellation_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let declared_phases = runtime_scenario_item(scenario, "phases")?
        .ok_or_else(|| PyValueError::new_err("missing cancellation phases"))?;
    let cases = subject.getattr("cases")?;
    if cases.len()? != declared_phases.len()? {
        return Err(PyValueError::new_err(
            "cancellation phase inventory mismatch",
        ));
    }
    let weakref = py.import("weakref")?;
    let gc = py.import("gc")?;
    let records = PyList::empty(py);
    let destructor_records = PyList::empty(py);
    let events = subject.getattr("events")?;
    let origin_thread: u64 = gates.get_item("owner_thread")?.extract()?;

    for (index, case_item) in cases.try_iter()?.enumerate() {
        let case = case_item?;
        let phase_name: String = case.getattr("phase")?.extract()?;
        let declared: String = declared_phases.get_item(index)?.extract()?;
        if phase_name != declared {
            return Err(PyValueError::new_err(
                "cancellation phase authority mismatch",
            ));
        }
        let phase = RuntimeCancellationPhase::parse(&phase_name)?;
        let owner = case.getattr("owner")?;
        let destructor_observer = Py::new(
            py,
            SessionRuntimeDestructorObserver {
                records: destructor_records.clone().unbind().into_any(),
                phase: phase_name.clone(),
                origin_thread,
            },
        )?;
        let owner_ref = weakref.call_method1("ref", (&owner, destructor_observer))?;
        let record = run_runtime_cancellation_case(py, &case, phase)?;
        let queued: usize = case.getattr("queued")?.extract()?;
        let executed: usize = case.getattr("executed")?.extract()?;
        let replies: usize = case.getattr("replies")?.extract()?;
        events.call_method1(
            "append",
            (PyList::new(
                py,
                [
                    "cancel".into_pyobject(py)?.into_any(),
                    phase_name.clone().into_pyobject(py)?.into_any(),
                    queued.into_pyobject(py)?.into_any(),
                    executed.into_pyobject(py)?.into_any(),
                    replies.into_pyobject(py)?.into_any(),
                ],
            )?,),
        )?;
        case.setattr("owner", py.None())?;
        drop(owner);
        let token = last_origin_quarantine_token()
            .ok_or_else(|| PyRuntimeError::new_err("missing cancellation quarantine token"))?;
        if phase != RuntimeCancellationPhase::PermanentlyNonterminal {
            let retained = take_origin_quarantine_owner::<RuntimeCancellationOwner>(token)
                .ok_or_else(|| PyRuntimeError::new_err("terminal cancellation was not reapable"))?;
            drop(retained);
        }
        gc.call_method0("collect")?;
        let owner_gone = if phase == RuntimeCancellationPhase::PermanentlyNonterminal {
            py.None()
        } else {
            owner_ref
                .call0()?
                .is_none()
                .into_pyobject(py)?
                .to_owned()
                .unbind()
                .into_any()
        };
        records.append(PyList::new(
            py,
            [
                phase_name.into_pyobject(py)?.into_any(),
                record.into_any(),
                queued.into_pyobject(py)?.into_any(),
                executed.into_pyobject(py)?.into_any(),
                replies.into_pyobject(py)?.into_any(),
                owner_gone.bind(py).clone(),
                case.getattr("terminal")?
                    .call_method0("is_set")?
                    .extract::<bool>()?
                    .into_pyobject(py)?
                    .to_owned()
                    .into_any(),
            ],
        )?)?;
    }

    let result = runtime_result(py);
    result.set_item("records", records)?;
    result.set_item("events", events)?;
    result.set_item("destructors", destructor_records)?;
    result.set_item("nonterminal_leaked", true)?;
    result.set_item("recovered", true)?;
    Ok(result.unbind().into_any())
}

fn runtime_fork_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let os = py.import("os")?;
    let parent_pid: i64 = runtime_scenario_item(scenario, "parent_pid")?
        .ok_or_else(|| PyValueError::new_err("missing parent pid"))?
        .extract()?;
    let parent_generation: i64 = runtime_scenario_item(scenario, "parent_generation")?
        .ok_or_else(|| PyValueError::new_err("missing parent generation"))?
        .extract()?;
    let child_generation = parent_generation
        .checked_add(1)
        .ok_or_else(|| PyValueError::new_err("runtime generation overflow"))?;
    let child_pid: i64 = os.call_method0("fork")?.extract()?;
    if child_pid == 0 {
        let read_fd = gates.get_item("read_fd")?;
        os.call_method1("close", (&read_fd,))?;
        let request = runtime_named_value(py, "child")?;
        let response = subject
            .getattr("adapter")?
            .call_method1("send", (&request,))?;
        let pid: i64 = os.call_method0("getpid")?.extract()?;
        let payload = format!(
            "{pid}:{child_generation}:{}",
            response.getattr("name")?.extract::<String>()?
        );
        os.call_method1("write", (gates.get_item("write_fd")?, payload.into_bytes()))?;
        os.call_method1("close", (gates.get_item("write_fd")?,))?;
        os.call_method1("_exit", (0,))?;
        return Err(PyRuntimeError::new_err("child exit unexpectedly returned"));
    }
    os.call_method1("close", (gates.get_item("write_fd")?,))?;
    let select = py.import("select")?;
    let ready = select.call_method1(
        "select",
        (
            PyList::new(py, [gates.get_item("read_fd")?])?,
            PyList::empty(py),
            PyList::empty(py),
            2,
        ),
    )?;
    if ready.get_item(0)?.len()? == 0 {
        os.call_method1(
            "kill",
            (child_pid, py.import("signal")?.getattr("SIGKILL")?),
        )?;
        os.call_method1("waitpid", (child_pid, 0))?;
        return Err(PyRuntimeError::new_err(
            "fork child did not reach result gate",
        ));
    }
    let payload = os
        .call_method1("read", (gates.get_item("read_fd")?, 4096))?
        .call_method0("decode")?
        .call_method1("split", (":", 2))?;
    os.call_method1("close", (gates.get_item("read_fd")?,))?;
    let waited = os.call_method1("waitpid", (child_pid, 0))?;
    let waited_pid: i64 = waited.get_item(0)?.extract()?;
    let status = waited.get_item(1)?;
    let parent_request = runtime_named_value(py, "parent")?;
    let parent_response = subject
        .getattr("adapter")?
        .call_method1("send", (&parent_request,))?;
    let child_pid_value = payload
        .get_item(0)?
        .extract::<String>()?
        .parse::<i64>()
        .map_err(|_| PyValueError::new_err("invalid child pid payload"))?;
    let child_generation_value = payload
        .get_item(1)?
        .extract::<String>()?
        .parse::<i64>()
        .map_err(|_| PyValueError::new_err("invalid child generation payload"))?;
    let result = runtime_result(py);
    result.set_item(
        "prefork",
        runtime_scenario_item(scenario, "prefork")?
            .ok_or_else(|| PyValueError::new_err("missing prefork phase"))?,
    )?;
    result.set_item("child_pid_changed", child_pid_value != parent_pid)?;
    result.set_item("child_generation", child_generation_value)?;
    result.set_item("parent_generation", parent_generation)?;
    result.set_item(
        "generation_advanced",
        child_generation_value > parent_generation,
    )?;
    result.set_item("child_response", payload.get_item(2)?)?;
    result.set_item("parent_response", parent_response.getattr("name")?)?;
    result.set_item(
        "child_exit",
        os.call_method1("waitstatus_to_exitcode", (status,))?,
    )?;
    result.set_item("waited", waited_pid == child_pid)?;
    result.set_item("parent_usable", true)?;
    Ok(result.unbind().into_any())
}

fn runtime_session_isolation_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let first_adapter = subject.getattr("first_adapter")?;
    let second_adapter = subject.getattr("second_adapter")?;
    let first_result = first_adapter.call_method1("send", (runtime_named_value(py, "one")?,))?;
    let second_result = second_adapter.call_method1("send", (runtime_named_value(py, "two")?,))?;
    first_adapter.call_method0("close")?;
    let again = second_adapter.call_method1("send", (runtime_named_value(py, "two-again")?,))?;
    let result = runtime_result(py);
    result.set_item(
        "responses",
        PyList::new(
            py,
            [
                first_result.getattr("name")?,
                second_result.getattr("name")?,
                again.getattr("name")?,
            ],
        )?,
    )?;
    result.set_item("events", subject.getattr("events")?)?;
    result.set_item(
        "counts",
        PyList::new(
            py,
            [
                first_adapter.getattr("calls")?,
                second_adapter.getattr("calls")?,
            ],
        )?,
    )?;
    result.set_item(
        "peer_survived",
        again.getattr("name")?.eq("session-two-response")?,
    )?;
    result.set_item("shared_runtime", gates.get_item("shared_runtime")?)?;
    result.set_item(
        "separate_adapters",
        !gates.get_item("shared_adapter")?.is_truthy()?,
    )?;
    Ok(result.unbind().into_any())
}

fn runtime_stream_isolation_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let response = subject.getattr("response")?;
    let request = subject.getattr("request")?;
    let stream_adapter = subject
        .getattr("stream_session")?
        .call_method1("get_adapter", (request.getattr("url")?,))?;
    let outstanding = stream_adapter.call_method1("send", (&request,))?;
    let peer_session = subject.getattr("peer_session")?;
    let peer_adapter = peer_session.call_method1("get_adapter", ("mock://runtime/peer",))?;
    peer_adapter.call_method0("close")?;
    let payload = outstanding.getattr("content")?;
    outstanding.call_method0("close")?;
    let result = runtime_result(py);
    result.set_item("identity", outstanding.is(&response))?;
    result.set_item("payload", payload.call_method0("decode")?)?;
    result.set_item("events", subject.getattr("events")?)?;
    result.set_item("peer_clear", gates.get_item("clear_peer")?)?;
    result.set_item("stream_survived", payload.eq(b"retained")?)?;
    result.set_item(
        "generations",
        PyList::new(
            py,
            [
                runtime_scenario_item(scenario, "stream_generation")?
                    .ok_or_else(|| PyValueError::new_err("missing stream generation"))?,
                runtime_scenario_item(scenario, "peer_generation")?
                    .ok_or_else(|| PyValueError::new_err("missing peer generation"))?,
            ],
        )?,
    )?;
    Ok(result.unbind().into_any())
}

#[derive(Clone)]
struct NativeLoopbackRoute {
    harness: SessionRuntimeHarness,
    payload: &'static [u8],
}

async fn start_native_keepalive_server(
    routes: HashMap<&'static str, NativeLoopbackRoute>,
    expected_requests: usize,
) -> Result<
    (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Result<(), String>>,
        tokio::sync::oneshot::Sender<()>,
    ),
    String,
> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let routes = Arc::new(routes);
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_notify = Arc::new(tokio::sync::Notify::new());
    let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while completed.load(Ordering::Acquire) < expected_requests {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.map_err(|error| error.to_string())?;
                    let routes = routes.clone();
                    let completed = completed.clone();
                    let completed_notify = completed_notify.clone();
                    connections.spawn(async move {
                        let mut pending = Vec::new();
                        let mut buffer = [0_u8; 1024];
                        loop {
                            let header_end = loop {
                                if let Some(position) = pending
                                    .windows(4)
                                    .position(|window| window == b"\r\n\r\n")
                                {
                                    break position + 4;
                                }
                                let read = stream
                                    .read(&mut buffer)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                if read == 0 {
                                    return Ok::<(), String>(());
                                }
                                pending.extend_from_slice(&buffer[..read]);
                            };
                            let header = pending.drain(..header_end).collect::<Vec<_>>();
                            let request_line_end = header
                                .windows(2)
                                .position(|window| window == b"\r\n")
                                .ok_or_else(|| "loopback request has no request line".to_owned())?;
                            let request_line = std::str::from_utf8(&header[..request_line_end])
                                .map_err(|error| error.to_string())?;
                            let path = request_line
                                .split_whitespace()
                                .nth(1)
                                .ok_or_else(|| "loopback request has no path".to_owned())?;
                            let route = routes
                                .get(path)
                                .cloned()
                                .ok_or_else(|| format!("unknown loopback route {path}"))?;
                            let correlation = route.harness.claim_request_observation().await;
                            route.harness.mark_request_observed(correlation);
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                                route.payload.len()
                            );
                            stream
                                .write_all(head.as_bytes())
                                .await
                                .map_err(|error| error.to_string())?;
                            stream
                                .write_all(route.payload)
                                .await
                                .map_err(|error| error.to_string())?;
                            let total = completed.fetch_add(1, Ordering::AcqRel) + 1;
                            completed_notify.notify_one();
                            let _ = total;
                        }
                    });
                }
                () = completed_notify.notified() => {}
            }
        }
        let _ = shutdown_receiver.await;
        connections.abort_all();
        Ok(())
    });
    Ok((address, server, shutdown_sender))
}

async fn native_isolation_send(
    client: requests::Client,
    address: std::net::SocketAddr,
    path: &'static str,
) -> Result<requests::Response, String> {
    let response = client
        .get(format!("http://{address}{path}"))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    Ok(response)
}

fn prepare_native_pool_resources(state: &mut NativeForkResources) -> PyResult<()> {
    if state.live_response.is_some() {
        return Ok(());
    }
    let client = requests::Client::builder()
        .session_runtime_harness(state.harness.clone())
        .build()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let worker_client = client.clone();
    let server_harness = state.harness.clone();
    let response = state
        .driver
        .submit(async move {
            let routes = HashMap::from([(
                "/fork",
                NativeLoopbackRoute {
                    harness: server_harness,
                    payload: b"fork",
                },
            )]);
            let (address, server, shutdown) = start_native_keepalive_server(routes, 1).await?;
            let response = native_isolation_send(worker_client, address, "/fork").await?;
            let _ = shutdown.send(());
            server.await.map_err(|error| error.to_string())??;
            Ok::<requests::Response, String>(response)
        })
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .wait()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .map_err(PyRuntimeError::new_err)?;
    state.pool_exchange = state.hooks.latest(SessionPhase::PoolAcquire);
    if state.pool_exchange.is_none() {
        return Err(PyRuntimeError::new_err(
            "native exchange did not acquire a pool lease",
        ));
    }
    state.client = Some(client);
    state.live_response = Some(response);
    Ok(())
}

fn emit_isolation_resources(
    _py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    include_pool: bool,
    finish_pool_response: bool,
) -> PyResult<()> {
    let (pid, driver, harness, pool_exchange, live_response) =
        NATIVE_FORK_RESOURCES.with(|slot| -> PyResult<_> {
            let mut slot = slot.borrow_mut();
            let pid = std::process::id();
            if slot.as_ref().is_some_and(|state| state.pid != pid) {
                let inherited = slot.take().expect("inherited fork resources exist");
                std::mem::forget(inherited);
            }
            if slot.is_none() {
                *slot = Some(NativeForkResources::fresh()?);
            }
            let state = slot.as_mut().expect("native fork resources initialized");
            if include_pool {
                prepare_native_pool_resources(state)?;
            }
            Ok((
                state.pid,
                state.driver.clone(),
                state.harness.clone(),
                state.pool_exchange,
                finish_pool_response
                    .then(|| state.live_response.take())
                    .flatten(),
            ))
        })?;
    let generation = driver.generation();
    let driver_id = driver.generation();
    let driver_thread_id = driver
        .submit(async { format!("{:?}", thread::current().id()) })
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .wait()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    subject.call_method1("runtime", (pid, generation, driver_id, driver_thread_id))?;
    if let Some(exchange) = pool_exchange {
        subject.call_method1(
            "pool",
            (
                pid,
                generation,
                exchange.pool.get(),
                exchange.lease.expect("pool acquire has lease").get(),
                exchange
                    .connection
                    .expect("pool acquire has connection")
                    .get(),
            ),
        )?;
    }
    let action = harness.checkpoint(
        SessionPhase::WorkerEntered,
        None,
        None,
        harness.next_correlation(),
    );
    harness.observe(action);
    subject.call_method1(
        "action",
        (pid, generation, action.correlation, "runtime-action"),
    )?;
    if let Some(response) = live_response {
        driver
            .submit(async move {
                response
                    .into_body()
                    .close()
                    .await
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
            .wait()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
            .map_err(PyRuntimeError::new_err)?;
    }
    Ok(())
}

fn fork_after_import_before_driver(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<()> {
    emit_isolation_resources(py, subject, false, false)
}

fn fork_after_live_driver(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<()> {
    emit_isolation_resources(py, subject, false, false)
}

fn fork_after_live_pool_lease(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<()> {
    emit_isolation_resources(py, subject, true, false)
}

fn cleanup_native_fork_resources(subject: &Bound<'_, PyAny>) -> PyResult<()> {
    let mut state = NATIVE_FORK_RESOURCES.with(|slot| slot.borrow_mut().take());
    let Some(mut state) = state.take() else {
        let quiesced = BlockingRuntimeDriver::quiesce_process_local();
        return subject
            .call_method1(
                "cleanup",
                (
                    std::process::id(),
                    usize::from(!quiesced),
                    usize::from(false),
                    usize::from(false),
                ),
            )
            .map(|_| ());
    };
    if state.pid != std::process::id() {
        std::mem::forget(state);
        return Err(PyRuntimeError::new_err(
            "cannot clean inherited native fork resources",
        ));
    }
    if let Some(response) = state.live_response.take() {
        let lease = state
            .pool_exchange
            .and_then(|checkpoint| checkpoint.lease)
            .ok_or_else(|| PyRuntimeError::new_err("live response has no lease identity"))?
            .get();
        state
            .driver
            .submit(async move {
                response
                    .into_body()
                    .close()
                    .await
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
            .wait()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
            .map_err(PyRuntimeError::new_err)?;
        if state.hooks.release_for(lease).is_none() {
            return Err(PyRuntimeError::new_err(
                "native cleanup did not release the live response lease",
            ));
        }
    }
    if let Some(client) = state.client.take() {
        client.clear_pool();
    }
    let open_leases = usize::from(state.live_response.is_some());
    let open_pools = usize::from(state.client.is_some());
    let quiesced = BlockingRuntimeDriver::quiesce_process_local();
    drop(state);
    subject.call_method1(
        "cleanup",
        (
            std::process::id(),
            usize::from(!quiesced),
            open_pools,
            open_leases,
        ),
    )?;
    Ok(())
}

fn multi_session_pool_isolation(
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let layout: String = runtime_scenario_item(scenario, "adapter_layout")?
        .ok_or_else(|| PyValueError::new_err("missing adapter layout"))?
        .extract()?;
    if !matches!(layout.as_str(), "shared" | "separate") {
        return Err(PyValueError::new_err("unknown adapter layout"));
    }
    let audit = subject.getattr("audit")?;
    let driver = BlockingRuntimeDriver::process_local()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let first_hooks = Arc::new(NativeIsolationHooks::default());
    let first_harness = SessionRuntimeHarness::new(first_hooks.clone());
    let second_hooks = if layout == "shared" {
        first_hooks.clone()
    } else {
        Arc::new(NativeIsolationHooks::default())
    };
    let second_harness = if layout == "shared" {
        first_harness.clone()
    } else {
        SessionRuntimeHarness::new(second_hooks.clone())
    };
    let first_client = requests::Client::builder()
        .session_runtime_harness(first_harness.clone())
        .build()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let second_client = if layout == "shared" {
        first_client.clone()
    } else {
        requests::Client::builder()
            .session_runtime_harness(second_harness.clone())
            .build()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
    };
    let worker_first = first_client.clone();
    let worker_second = second_client.clone();
    let server_first_harness = first_harness.clone();
    let server_second_harness = second_harness.clone();
    let worker_first_hooks = first_hooks.clone();
    let worker_second_hooks = second_hooks.clone();
    let separate = layout == "separate";
    let (first_identity, second_identity, third_identity, released_first) = driver
        .submit(async move {
            let routes = HashMap::from([
                (
                    "/first",
                    NativeLoopbackRoute {
                        harness: server_first_harness,
                        payload: b"first",
                    },
                ),
                (
                    "/second",
                    NativeLoopbackRoute {
                        harness: server_second_harness,
                        payload: b"second",
                    },
                ),
            ]);
            let (address, server, shutdown) = start_native_keepalive_server(routes, 3).await?;

            let first_response =
                native_isolation_send(worker_first.clone(), address, "/first").await?;
            let first_identity = worker_first_hooks
                .latest(SessionPhase::PoolAcquire)
                .ok_or_else(|| "first exchange did not acquire lease".to_owned())?;
            first_response
                .bytes()
                .await
                .map_err(|error| error.to_string())?;
            let first_lease = first_identity
                .lease
                .ok_or_else(|| "first pool acquire has no lease".to_owned())?
                .get();
            let released_first = worker_first_hooks
                .release_for(first_lease)
                .ok_or_else(|| "first response did not release its real lease".to_owned())?;

            let second_response =
                native_isolation_send(worker_second.clone(), address, "/second").await?;
            let second_identity = worker_second_hooks
                .latest(SessionPhase::PoolAcquire)
                .ok_or_else(|| "second exchange did not acquire lease".to_owned())?;
            second_response
                .bytes()
                .await
                .map_err(|error| error.to_string())?;

            if separate {
                worker_first.clear_pool();
                let first_connection = first_identity
                    .connection
                    .ok_or_else(|| "first pool acquire has no connection".to_owned())?
                    .get();
                if !worker_first_hooks
                    .cleared_connections()
                    .contains(&first_connection)
                {
                    return Err("first session clear did not evict its populated pool".to_owned());
                }
            }

            let third_response = native_isolation_send(worker_second, address, "/second").await?;
            let third_identity = worker_second_hooks
                .latest(SessionPhase::PoolAcquire)
                .ok_or_else(|| "second reuse did not acquire lease".to_owned())?;
            third_response
                .bytes()
                .await
                .map_err(|error| error.to_string())?;
            if third_identity.connection != second_identity.connection {
                return Err("second session did not reuse its populated connection".to_owned());
            }
            let _ = shutdown.send(());
            server.await.map_err(|error| error.to_string())??;
            Ok::<_, String>((
                first_identity,
                second_identity,
                third_identity,
                released_first,
            ))
        })
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .wait()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .map_err(PyRuntimeError::new_err)?;
    let first_adapter = first_identity.pool.get();
    let second_adapter = second_identity.pool.get();
    let first_lease = first_identity.lease.expect("pool acquire has lease").get();
    let second_lease = second_identity.lease.expect("pool acquire has lease").get();
    let first_correlation = first_identity.correlation;
    let second_correlation = second_identity.correlation;
    audit.call_method1(
        "resource",
        (
            "first",
            first_adapter,
            first_identity.pool.get(),
            first_lease,
            first_correlation,
            driver.generation(),
        ),
    )?;
    audit.call_method1(
        "resource",
        (
            "second",
            second_adapter,
            second_identity.pool.get(),
            second_lease,
            second_correlation,
            driver.generation(),
        ),
    )?;
    audit.call_method1("action", ("first", first_correlation, "first"))?;
    audit.call_method1("action", ("second", second_correlation, "second"))?;
    let released_first_lease = released_first
        .lease
        .ok_or_else(|| PyRuntimeError::new_err("first release checkpoint has no lease"))?
        .get();
    audit.call_method1("close", ("first", (released_first_lease,)))?;
    let third_correlation = third_identity.correlation;
    audit.call_method1("action", ("second", third_correlation, "second-again"))?;
    Ok(())
}

fn outstanding_stream_lease_isolation(subject: &Bound<'_, PyAny>) -> PyResult<()> {
    let audit = subject.getattr("audit")?;
    let driver = BlockingRuntimeDriver::process_local()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let stream_hooks = Arc::new(NativeIsolationHooks::default());
    let stream_harness = SessionRuntimeHarness::new(stream_hooks.clone());
    let peer_hooks = Arc::new(NativeIsolationHooks::default());
    let peer_harness = SessionRuntimeHarness::new(peer_hooks.clone());
    let stream_client = requests::Client::builder()
        .session_runtime_harness(stream_harness.clone())
        .build()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let peer_client = requests::Client::builder()
        .session_runtime_harness(peer_harness.clone())
        .build()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let worker_stream = stream_client.clone();
    let worker_peer = peer_client.clone();
    let server_stream_harness = stream_harness.clone();
    let server_peer_harness = peer_harness.clone();
    let worker_stream_hooks = stream_hooks.clone();
    let worker_peer_hooks = peer_hooks.clone();
    let (identity, payload, peer_released_leases, peer_cleared_connections) = driver
        .submit(async move {
            let routes = HashMap::from([
                (
                    "/stream",
                    NativeLoopbackRoute {
                        harness: server_stream_harness,
                        payload: b"retained",
                    },
                ),
                (
                    "/peer",
                    NativeLoopbackRoute {
                        harness: server_peer_harness,
                        payload: b"peer",
                    },
                ),
            ]);
            let (address, server, shutdown) = start_native_keepalive_server(routes, 2).await?;
            let stream_response = native_isolation_send(worker_stream, address, "/stream").await?;
            let identity = worker_stream_hooks
                .latest(SessionPhase::PoolAcquire)
                .ok_or_else(|| "stream did not acquire lease".to_owned())?;

            let peer_response =
                native_isolation_send(worker_peer.clone(), address, "/peer").await?;
            peer_response
                .bytes()
                .await
                .map_err(|error| error.to_string())?;
            let peer_identity = worker_peer_hooks
                .latest(SessionPhase::PoolAcquire)
                .ok_or_else(|| "peer did not acquire lease".to_owned())?;
            let peer_release = worker_peer_hooks
                .release_for(
                    peer_identity
                        .lease
                        .ok_or_else(|| "peer acquire has no lease".to_owned())?
                        .get(),
                )
                .ok_or_else(|| "peer response did not populate its pool".to_owned())?;
            worker_peer.clear_pool();
            let peer_cleared_connections = worker_peer_hooks.cleared_connections();
            let peer_connection = peer_identity
                .connection
                .ok_or_else(|| "peer acquire has no connection".to_owned())?
                .get();
            if !peer_cleared_connections.contains(&peer_connection) {
                return Err("peer clear did not evict its populated connection".to_owned());
            }

            let payload = stream_response
                .bytes()
                .await
                .map_err(|error| error.to_string())?;
            let stream_lease = identity
                .lease
                .ok_or_else(|| "stream acquire has no lease".to_owned())?
                .get();
            let stream_releases = worker_stream_hooks
                .snapshot()
                .into_iter()
                .filter(|checkpoint| {
                    matches!(
                        checkpoint.phase,
                        SessionPhase::PoolReleaseClean | SessionPhase::PoolReleaseDirty
                    ) && checkpoint.lease.map(|value| value.get()) == Some(stream_lease)
                })
                .count();
            if stream_releases != 1 {
                return Err("stream lease was not released exactly once".to_owned());
            }
            let _ = shutdown.send(());
            server.await.map_err(|error| error.to_string())??;
            Ok::<_, String>((
                identity,
                payload,
                vec![
                    peer_release
                        .lease
                        .ok_or_else(|| "peer release has no lease".to_owned())?
                        .get(),
                ],
                peer_cleared_connections,
            ))
        })
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .wait()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .map_err(PyRuntimeError::new_err)?;
    let lease = identity.lease.expect("pool acquire has lease").get();
    let connection = identity
        .connection
        .expect("pool acquire has connection")
        .get();
    audit.call_method1("opened", (lease, connection, stream_harness.generation()))?;
    audit.call_method1(
        "peer_closed",
        (
            PyTuple::new(subject.py(), peer_released_leases)?,
            PyTuple::new(subject.py(), peer_cleared_connections)?,
        ),
    )?;
    audit.call_method1("chunk", (lease, connection, payload.as_ref()))?;
    audit.call_method1("stream_closed", (lease, connection))?;
    audit.call_method1("lease_released", (lease, connection))?;
    Ok(())
}

fn validate_isolation_adversarial(
    scenario: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<()> {
    match operation {
        RuntimeOperation::ValidateStalePid => {
            let pid: u32 = runtime_scenario_item(scenario, "pid")?
                .ok_or_else(|| PyValueError::new_err("missing pid"))?
                .extract()?;
            if pid != std::process::id() {
                return Err(PyValueError::new_err("stale pid"));
            }
        }
        RuntimeOperation::ValidateStaleGeneration => {
            return Err(PyValueError::new_err("stale generation"));
        }
        RuntimeOperation::ValidateInheritedPool => {
            return Err(PyValueError::new_err("inherited pool"));
        }
        RuntimeOperation::ValidateReleasedLease => {
            return Err(PyValueError::new_err("released lease"));
        }
        RuntimeOperation::ValidateDuplicateCorrelation => {
            let correlations = runtime_scenario_item(scenario, "correlations")?
                .ok_or_else(|| PyValueError::new_err("missing correlations"))?;
            if correlations.get_item(0)?.eq(correlations.get_item(1)?)? {
                return Err(PyValueError::new_err("duplicate correlation"));
            }
        }
        _ => return Err(PyValueError::new_err("invalid isolation validation")),
    }
    Err(PyValueError::new_err("adversarial state was not invalid"))
}

fn runtime_native_isolation_audit(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<Py<PyAny>> {
    match operation {
        RuntimeOperation::ForkPrepareImport => {}
        RuntimeOperation::ForkPrepareDriver => fork_after_live_driver(py, subject)?,
        RuntimeOperation::ForkPreparePool => fork_after_live_pool_lease(py, subject)?,
        RuntimeOperation::ForkChildUseAfterImport => fork_after_import_before_driver(py, subject)?,
        RuntimeOperation::ForkChildUseAfterDriver => fork_after_live_driver(py, subject)?,
        RuntimeOperation::ForkChildUseAfterPool => fork_after_live_pool_lease(py, subject)?,
        RuntimeOperation::ForkParentUseAfterImport | RuntimeOperation::ForkParentUseAfterDriver => {
            emit_isolation_resources(py, subject, false, false)?;
        }
        RuntimeOperation::ForkParentUseAfterPool => {
            emit_isolation_resources(py, subject, true, true)?
        }
        RuntimeOperation::ForkChildCleanupAfterImport
        | RuntimeOperation::ForkChildCleanupAfterDriver
        | RuntimeOperation::ForkChildCleanupAfterPool => {
            cleanup_native_fork_resources(subject)?;
        }
        RuntimeOperation::MultiSessionIsolation => multi_session_pool_isolation(subject, scenario)?,
        RuntimeOperation::OutstandingStreamIsolation => {
            outstanding_stream_lease_isolation(subject)?
        }
        RuntimeOperation::ValidateStalePid
        | RuntimeOperation::ValidateStaleGeneration
        | RuntimeOperation::ValidateInheritedPool
        | RuntimeOperation::ValidateReleasedLease
        | RuntimeOperation::ValidateDuplicateCorrelation => {
            return validate_isolation_adversarial(scenario, operation).map(|_| py.None());
        }
        _ => return Err(PyValueError::new_err("invalid isolation operation")),
    }
    Ok(py.None())
}

#[derive(Clone, Copy)]
enum CompletionProgram {
    Close,
    PanicRecovery,
    LiveAuthority,
    Channel { envelope: CompletionEnvelope },
}

fn completion_envelope_from_scenario(
    scenario: &Bound<'_, PyAny>,
    reply: bool,
) -> PyResult<CompletionEnvelope> {
    let field = |name: &str| {
        if reply {
            format!("reply_{name}")
        } else {
            name.to_owned()
        }
    };
    let generation = GenerationId::checked(
        runtime_scenario_item(scenario, &field("generation"))?
            .ok_or_else(|| PyValueError::new_err("missing channel generation"))?
            .extract()?,
    )
    .map_err(PyValueError::new_err)?;
    let correlation = CorrelationId::checked(
        runtime_scenario_item(scenario, &field("correlation_id"))?
            .ok_or_else(|| PyValueError::new_err("missing channel correlation"))?
            .extract()?,
    )
    .map_err(PyValueError::new_err)?;
    let sequence = Sequence::checked(
        runtime_scenario_item(scenario, &field("sequence"))?
            .ok_or_else(|| PyValueError::new_err("missing channel sequence"))?
            .extract()?,
    )
    .map_err(PyValueError::new_err)?;
    let request_id = OpaqueValueId::checked(
        runtime_scenario_item(scenario, &field("request_id"))?
            .ok_or_else(|| PyValueError::new_err("missing channel request id"))?
            .extract()?,
        generation,
    )
    .map_err(PyValueError::new_err)?;
    let response_id = OpaqueValueId::checked(
        runtime_scenario_item(scenario, &field("response_id"))?
            .ok_or_else(|| PyValueError::new_err("missing channel response id"))?
            .extract()?,
        generation,
    )
    .map_err(PyValueError::new_err)?;
    let error_id = runtime_scenario_item(scenario, &field("error_id"))?
        .filter(|value| !value.is_none())
        .map(|value| {
            OpaqueValueId::checked(value.extract()?, generation).map_err(PyValueError::new_err)
        })
        .transpose()?;
    Ok(CompletionEnvelope {
        generation,
        correlation,
        sequence,
        request_id,
        response_id,
        error_id,
    })
}

fn validate_completion_envelopes(
    scenario: &Bound<'_, PyAny>,
) -> PyResult<(CompletionEnvelope, CompletionEnvelope)> {
    let action = completion_envelope_from_scenario(scenario, false)?;
    let reply = completion_envelope_from_scenario(scenario, true)?;
    if action.generation != reply.generation {
        return Err(PyValueError::new_err("channel reply generation mismatch"));
    }
    if action.correlation != reply.correlation {
        return Err(PyValueError::new_err("channel reply correlation mismatch"));
    }
    if action.sequence != reply.sequence {
        return Err(PyValueError::new_err("channel reply sequence mismatch"));
    }
    if action.request_id != reply.request_id
        || action.response_id != reply.response_id
        || action.error_id != reply.error_id
    {
        return Err(PyValueError::new_err("channel reply opaque id mismatch"));
    }
    if let Some(peer) = runtime_scenario_item(scenario, "peer_correlation_id")?
        && peer.extract::<u64>()? == action.correlation.0
    {
        return Err(PyValueError::new_err("duplicate channel correlation"));
    }
    Ok((action, reply))
}

fn execute_completion_action(
    py: Python<'_>,
    action: CompletionAction,
    owner: &mut CompletionOwner,
) -> CompletionReply {
    let subject = owner.subject.bind(py);
    let outcome = match action {
        CompletionAction::CloseOnce => subject
            .getattr("adapter")
            .and_then(|adapter| adapter.call_method0("close"))
            .map(|_| ()),
        CompletionAction::PanicSend => subject
            .getattr("adapter")
            .and_then(|adapter| adapter.call_method1("send", (subject.getattr("request")?,)))
            .map(|value| {
                owner.value = Some(value.unbind());
            }),
        CompletionAction::RecoverySend => (|| -> PyResult<()> {
            let adapter = subject.getattr("adapter")?;
            let state = py.import("types")?.getattr("SimpleNamespace")?.call0()?;
            state.setattr("events", subject.getattr("events")?)?;
            let recovery = adapter.get_type().call1((state, "recovery"))?;
            let request = runtime_named_request(py, &subject.getattr("request")?, "after-panic")?;
            owner.value = Some(recovery.call_method1("send", (request,))?.unbind());
            Ok(())
        })(),
        CompletionAction::ClockBeforeAwait => (|| -> PyResult<()> {
            owner.before_clock = Some(
                py.import("requests.sessions")?
                    .getattr("preferred_clock")?
                    .call0()?
                    .extract()?,
            );
            owner.value = Some(
                subject
                    .getattr("adapter")?
                    .call_method1("send", (subject.getattr("request")?,))?
                    .unbind(),
            );
            Ok(())
        })(),
        CompletionAction::ClockAfterAwait => (|| -> PyResult<()> {
            let after: f64 = py
                .import("requests.sessions")?
                .getattr("preferred_clock")?
                .call0()?
                .extract()?;
            let before = owner
                .before_clock
                .ok_or_else(|| PyRuntimeError::new_err("missing pre-await clock sample"))?;
            let response = owner
                .value
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing awaited response"))?
                .bind(py);
            let kwargs = PyDict::new(py);
            kwargs.set_item("seconds", after - before)?;
            response.setattr(
                "elapsed",
                py.import("datetime")?
                    .getattr("timedelta")?
                    .call((), Some(&kwargs))?,
            )?;
            Ok(())
        })(),
        CompletionAction::FinalizationEnter => (|| -> PyResult<()> {
            let scenario = owner
                .scenario
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing finalization scenario"))?
                .bind(py);
            let worker_state: String = runtime_scenario_item(scenario, "worker_state")?
                .ok_or_else(|| PyValueError::new_err("missing finalization worker state"))?
                .extract()?;
            let retained = subject.getattr("owner")?;
            let generation = format!("session-finalize-{}", std::process::id());
            subject.call_method1("worker_entered", (&generation, &retained))?;
            subject.call_method1("close", (1_u8,))?;
            if worker_state == "permanently-nonterminal" {
                subject.call_method1("quarantined", (&retained, false))?;
            }
            Ok(())
        })(),
        CompletionAction::FinalizationAwaitRelease => (|| -> PyResult<()> {
            let scenario = owner
                .scenario
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing finalization scenario"))?
                .bind(py);
            let gates = owner
                .gates
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing finalization gates"))?
                .bind(py);
            let shutdown_bound_ms: u64 = runtime_scenario_item(scenario, "shutdown_bound_ms")?
                .ok_or_else(|| PyValueError::new_err("missing finalization bound"))?
                .extract()?;
            let released: bool = gates
                .getattr("release")?
                .call_method1("wait", (shutdown_bound_ms as f64 / 1000.0,))?
                .extract()?;
            if !released {
                return Err(PyRuntimeError::new_err(
                    "terminal finalization release timed out",
                ));
            }
            Ok(())
        })(),
        CompletionAction::NativePanicEntered => (|| -> PyResult<()> {
            let scenario = owner
                .scenario
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing panic scenario"))?
                .bind(py);
            let panic_id = runtime_scenario_item(scenario, "panic_id")?
                .ok_or_else(|| PyValueError::new_err("missing native panic id"))?;
            subject.call_method1("panic_entered", (panic_id,))?;
            Ok(())
        })(),
        CompletionAction::NativeAwaitEntered => {
            subject.call_method0("native_await_entered").map(|_| ())
        }
        CompletionAction::NativeAwaitComplete => (|| -> PyResult<()> {
            let gates = owner
                .gates
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("missing await gates"))?
                .bind(py);
            let released: bool = gates
                .getattr("release")?
                .call_method1("wait", (2.0,))?
                .extract()?;
            if !released {
                return Err(PyRuntimeError::new_err("native await release timed out"));
            }
            subject.call_method0("native_await_resumed")?;
            let value = subject.getattr("authority")?.call0()?;
            subject.call_method1("completed", (&value,))?;
            owner.value = Some(value.unbind());
            Ok(())
        })(),
        CompletionAction::NativeChannelEntered { envelope } => {
            let result = (|| -> PyResult<()> {
                let scenario = owner
                    .scenario
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("missing channel scenario"))?
                    .bind(py);
                let request = runtime_scenario_item(scenario, "request")?
                    .ok_or_else(|| PyValueError::new_err("missing channel request"))?;
                let correlation = runtime_scenario_item(scenario, "correlation")?
                    .ok_or_else(|| PyValueError::new_err("missing channel correlation"))?;
                let channel_id = runtime_scenario_item(scenario, "channel_id")?
                    .ok_or_else(|| PyValueError::new_err("missing channel id"))?;
                let interpreter = runtime_scenario_item(scenario, "entry_interpreter")?
                    .ok_or_else(|| PyValueError::new_err("missing entry interpreter"))?;
                subject.call_method1("entered", (request, correlation, channel_id, interpreter))?;
                Ok(())
            })();
            return match result {
                Ok(()) => CompletionReply::Channel { envelope },
                Err(error) => {
                    if owner.error.is_none() {
                        owner.error = Some(error);
                    }
                    CompletionReply::Failed
                }
            };
        }
        CompletionAction::NativeChannelComplete { envelope } => {
            let result = (|| -> PyResult<()> {
                let scenario = owner
                    .scenario
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("missing channel scenario"))?
                    .bind(py);
                let gates = owner
                    .gates
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("missing channel gates"))?
                    .bind(py);
                let released: bool = gates
                    .getattr("release")?
                    .call_method1("wait", (2.0,))?
                    .extract()?;
                if !released {
                    return Err(PyRuntimeError::new_err("channel release timed out"));
                }
                let correlation = runtime_scenario_item(scenario, "correlation")?
                    .ok_or_else(|| PyValueError::new_err("missing channel correlation"))?;
                let channel_id = runtime_scenario_item(scenario, "channel_id")?
                    .ok_or_else(|| PyValueError::new_err("missing channel id"))?;
                let interpreter = runtime_scenario_item(scenario, "entry_interpreter")?
                    .ok_or_else(|| PyValueError::new_err("missing entry interpreter"))?;
                if let Some(error) =
                    runtime_scenario_item(scenario, "error")?.filter(|error| !error.is_none())
                {
                    subject
                        .call_method1("failed", (&error, correlation, channel_id, interpreter))?;
                    owner.error = Some(PyErr::from_value(error));
                } else {
                    let response = runtime_scenario_item(scenario, "response")?
                        .ok_or_else(|| PyValueError::new_err("missing channel response"))?;
                    subject.call_method1(
                        "replied",
                        (&response, correlation, channel_id, interpreter),
                    )?;
                    owner.value = Some(response.unbind());
                }
                Ok(())
            })();
            return match result {
                Ok(()) => CompletionReply::Channel { envelope },
                Err(error) => {
                    if owner.error.is_none() {
                        owner.error = Some(error);
                    }
                    CompletionReply::Failed
                }
            };
        }
        CompletionAction::ChannelSend { envelope } => {
            let result = subject
                .getattr("adapter")
                .and_then(|adapter| adapter.call_method1("send", (subject.getattr("request")?,)))
                .map(|value| {
                    owner.value = Some(value.unbind());
                });
            return match result {
                Ok(()) => CompletionReply::Channel { envelope },
                Err(error) => {
                    if owner.error.is_none() {
                        owner.error = Some(error);
                    }
                    CompletionReply::Failed
                }
            };
        }
    };
    match outcome {
        Ok(()) => CompletionReply::Ack,
        Err(error) => {
            if owner.error.is_none() {
                owner.error = Some(error);
            }
            CompletionReply::Failed
        }
    }
}

fn run_completion_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    program: CompletionProgram,
) -> PyResult<CompletionOwner> {
    let owner = CompletionOwner {
        subject: subject.clone().unbind(),
        scenario: None,
        gates: None,
        _retained: None,
        value: None,
        error: None,
        before_clock: None,
        _not_send_or_sync: PhantomData,
    };
    let (completed, owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| async move {
            match program {
                CompletionProgram::Close => {
                    matches!(
                        actions.request(CompletionAction::CloseOnce).await,
                        Ok(CompletionReply::Ack)
                    )
                }
                CompletionProgram::PanicRecovery => {
                    let failed = matches!(
                        actions.request(CompletionAction::PanicSend).await,
                        Ok(CompletionReply::Failed)
                    );
                    let recovered = matches!(
                        actions.request(CompletionAction::RecoverySend).await,
                        Ok(CompletionReply::Ack)
                    );
                    failed && recovered
                }
                CompletionProgram::LiveAuthority => {
                    for action in [
                        CompletionAction::ClockBeforeAwait,
                        CompletionAction::ClockAfterAwait,
                    ] {
                        if !matches!(actions.request(action).await, Ok(CompletionReply::Ack)) {
                            return false;
                        }
                    }
                    true
                }
                CompletionProgram::Channel { envelope } => matches!(
                    actions
                        .request(CompletionAction::ChannelSend { envelope })
                        .await,
                    Ok(CompletionReply::Channel { envelope: reply }) if reply == envelope
                ),
            }
        },
        execute_completion_action,
    )?;
    if !completed {
        if let Some(error) = owner.error {
            return Err(error);
        }
        return Err(PyRuntimeError::new_err(
            "completion action program did not complete",
        ));
    }
    Ok(owner)
}

#[derive(Clone, Copy)]
enum NativeCompletionProgram {
    AwaitLiveAuthority,
    Channel {
        action: CompletionEnvelope,
        reply: CompletionEnvelope,
    },
}

fn run_native_completion_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    program: NativeCompletionProgram,
) -> PyResult<CompletionOwner> {
    let owner = CompletionOwner {
        subject: subject.clone().unbind(),
        scenario: Some(scenario.clone().unbind()),
        gates: Some(gates.clone().unbind()),
        _retained: None,
        value: None,
        error: None,
        before_clock: None,
        _not_send_or_sync: PhantomData,
    };
    let (completed, owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| async move {
            match program {
                NativeCompletionProgram::AwaitLiveAuthority => {
                    for action in [
                        CompletionAction::NativeAwaitEntered,
                        CompletionAction::NativeAwaitComplete,
                    ] {
                        if !matches!(actions.request(action).await, Ok(CompletionReply::Ack)) {
                            return false;
                        }
                    }
                    true
                }
                NativeCompletionProgram::Channel { action, reply } => {
                    for request in [
                        CompletionAction::NativeChannelEntered { envelope: action },
                        CompletionAction::NativeChannelComplete { envelope: action },
                    ] {
                        if !matches!(
                            actions.request(request).await,
                            Ok(CompletionReply::Channel { envelope }) if envelope == reply
                        ) {
                            return false;
                        }
                    }
                    true
                }
            }
        },
        execute_completion_action,
    )?;
    if !completed {
        if let Some(error) = owner.error {
            return Err(error);
        }
        return Err(PyRuntimeError::new_err(
            "native completion action program did not complete",
        ));
    }
    Ok(owner)
}

fn run_native_panic_program(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let owner = CompletionOwner {
        subject: subject.clone().unbind(),
        scenario: Some(scenario.clone().unbind()),
        gates: None,
        _retained: None,
        value: None,
        error: None,
        before_clock: None,
        _not_send_or_sync: PhantomData,
    };
    let panic_id: String = runtime_scenario_item(scenario, "panic_id")?
        .ok_or_else(|| PyValueError::new_err("missing native panic id"))?
        .extract()?;
    let result = run_with_owned_actions(
        py,
        owner,
        move |actions| async move {
            if !matches!(
                actions.request(CompletionAction::NativePanicEntered).await,
                Ok(CompletionReply::Ack)
            ) {
                return;
            }
            panic!("{panic_id}");
        },
        execute_completion_action,
    );
    match result {
        Ok(_) => Err(PyRuntimeError::new_err(
            "native panic program returned unexpectedly",
        )),
        Err(_) => Ok(()),
    }
}

fn bounded_session_finalization(py: Python<'_>, subject: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let owner = run_completion_program(py, subject, CompletionProgram::Close)?;
    Ok(owner.subject)
}

fn translate_session_worker_panic(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let owner = run_completion_program(py, subject, CompletionProgram::PanicRecovery)?;
    let marker = subject.getattr("marker")?;
    let error = owner
        .error
        .as_ref()
        .ok_or_else(|| PyRuntimeError::new_err("panic operation did not raise"))?;
    let record = runtime_exception_record(py, error, &marker)?;
    let recovered = owner
        .value
        .as_ref()
        .ok_or_else(|| PyRuntimeError::new_err("panic recovery did not return"))?
        .bind(py);
    let generation = runtime_scenario_item(scenario, "generation")?
        .ok_or_else(|| PyValueError::new_err("missing panic generation"))?;
    let recovery_generation = runtime_scenario_item(scenario, "recovery_generation")?
        .ok_or_else(|| PyValueError::new_err("missing recovery generation"))?;
    let result = runtime_result(py);
    result.set_item("error", record)?;
    result.set_item("events", subject.getattr("events")?)?;
    result.set_item("generation", &generation)?;
    result.set_item("recovery_generation", &recovery_generation)?;
    result.set_item(
        "advanced",
        recovery_generation.extract::<u64>()? > generation.extract::<u64>()?,
    )?;
    result.set_item("recovered", recovered.getattr("name")?)?;
    result.set_item("owner_stranded", gates.get_item("owner_stranded")?)?;
    Ok(result.unbind().into_any())
}

fn reload_live_python_authority_after_await(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let owner = run_completion_program(py, subject, CompletionProgram::LiveAuthority)?;
    owner
        .value
        .ok_or_else(|| PyRuntimeError::new_err("live authority program returned no response"))
}

fn independent_concurrent_session_channels(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let (action, reply) = validate_completion_envelopes(scenario)?;
    let owner =
        run_completion_program(py, subject, CompletionProgram::Channel { envelope: action })?;
    if action != reply {
        return Err(PyValueError::new_err("channel reply envelope mismatch"));
    }
    owner
        .value
        .ok_or_else(|| PyRuntimeError::new_err("channel returned no response"))
}

fn bounded_native_session_finalization(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let worker_state: String = runtime_scenario_item(scenario, "worker_state")?
        .ok_or_else(|| PyValueError::new_err("missing finalization worker state"))?
        .extract()?;
    if worker_state != "terminal" && worker_state != "permanently-nonterminal" {
        return Err(PyValueError::new_err("invalid finalization terminal state"));
    }
    let shutdown_bound_ms: u64 = runtime_scenario_item(scenario, "shutdown_bound_ms")?
        .ok_or_else(|| PyValueError::new_err("missing finalization bound"))?
        .extract()?;
    if shutdown_bound_ms == 0 || shutdown_bound_ms > 500 {
        return Err(PyValueError::new_err("invalid finalization shutdown bound"));
    }
    let permanently_nonterminal = worker_state == "permanently-nonterminal";
    let worker_noncooperative = Arc::new(AtomicBool::new(false));
    let checker_noncooperative = Arc::clone(&worker_noncooperative);
    let future_noncooperative = Arc::clone(&worker_noncooperative);
    let started = Instant::now();
    let retained = subject.getattr("owner")?.unbind();
    let owner = CompletionOwner {
        subject: subject.clone().unbind(),
        scenario: Some(scenario.clone().unbind()),
        gates: Some(gates.clone().unbind()),
        _retained: Some(retained),
        value: None,
        error: None,
        before_clock: None,
        _not_send_or_sync: PhantomData,
    };
    let result = run_with_owned_actions_and_signal_checker(
        py,
        owner,
        move |actions| async move {
            if !matches!(
                actions.request(CompletionAction::FinalizationEnter).await,
                Ok(CompletionReply::Ack)
            ) {
                return false;
            }
            if permanently_nonterminal {
                // This models a genuinely non-cooperative native worker. Cancellation
                // cannot move its origin-owned Python state off the entering thread;
                // the bounded driver therefore quarantines that state instead.
                future_noncooperative.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(50));
                loop {
                    thread::park();
                }
            }
            matches!(
                actions
                    .request(CompletionAction::FinalizationAwaitRelease)
                    .await,
                Ok(CompletionReply::Ack)
            )
        },
        execute_completion_action,
        move |py| {
            if permanently_nonterminal
                && checker_noncooperative.load(Ordering::Acquire)
                && started.elapsed() >= Duration::from_millis(10)
            {
                return Err(PyTimeoutError::new_err(
                    "permanently nonterminal session worker quarantined",
                ));
            }
            py.check_signals()
        },
    );

    match result {
        Ok((true, owner)) => {
            let subject = owner.subject.bind(py);
            let retained = subject.getattr("owner")?;
            let generation = format!("session-finalize-{}", std::process::id());
            subject.call_method1("worker_dropped", (&generation,))?;
            subject.call_method1("quarantined", (&retained, true))?;
            subject.call_method1("origin_reaped", (&retained,))?;
            Ok(py.None())
        }
        Ok((false, _)) => Err(PyRuntimeError::new_err(
            "finalization worker did not complete",
        )),
        Err(error) if permanently_nonterminal && error.is_instance_of::<PyTimeoutError>(py) => {
            Ok(py.None())
        }
        Err(error) => Err(error),
    }
}

fn native_worker_panic_or_recovery(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    recovery: bool,
) -> PyResult<Py<PyAny>> {
    let generation = runtime_scenario_item(scenario, "generation")?
        .ok_or_else(|| PyValueError::new_err("missing native panic generation"))?;
    let driver_id = runtime_scenario_item(scenario, "driver_id")?
        .ok_or_else(|| PyValueError::new_err("missing native panic driver"))?;
    subject.call_method1("runtime", (&generation, &driver_id))?;
    if recovery {
        let value = runtime_scenario_item(scenario, "value")?
            .ok_or_else(|| PyValueError::new_err("missing recovery value"))?;
        subject.call_method1("recovered", (&value,))?;
        return Ok(value.unbind());
    }
    run_native_panic_program(py, subject, scenario)?;
    subject.call_method1("action_state", (0_u8, 0_u8))?;
    subject.call_method0("worker_dropped")?;
    let owner = subject.getattr("owner")?;
    subject.call_method1("owner_reaped", (&owner,))?;
    let panic_id: String = runtime_scenario_item(scenario, "panic_id")?
        .ok_or_else(|| PyValueError::new_err("missing native panic id"))?
        .extract()?;
    Err(PyRuntimeError::new_err(format!(
        "native session worker panicked: {panic_id}"
    )))
}

fn native_await_live_authority(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let owner = run_native_completion_program(
        py,
        subject,
        scenario,
        gates,
        NativeCompletionProgram::AwaitLiveAuthority,
    )?;
    owner
        .value
        .ok_or_else(|| PyRuntimeError::new_err("live authority returned no value"))
}

fn native_concurrent_session_channel(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<Py<PyAny>> {
    let (action, reply) = validate_completion_envelopes(scenario)?;
    let declared_error =
        runtime_scenario_item(scenario, "error")?.is_some_and(|error| !error.is_none());
    match operation {
        RuntimeOperation::ConcurrentChannelReply if action.error_id.is_some() || declared_error => {
            return Err(PyValueError::new_err(
                "reply channel declared an error envelope",
            ));
        }
        RuntimeOperation::ConcurrentChannelFail if action.error_id.is_none() || !declared_error => {
            return Err(PyValueError::new_err(
                "failed channel omitted its error envelope",
            ));
        }
        RuntimeOperation::ConcurrentChannelReply | RuntimeOperation::ConcurrentChannelFail => {}
        _ => {
            return Err(PyValueError::new_err(
                "invalid concurrent channel operation",
            ));
        }
    }
    let mut owner = run_native_completion_program(
        py,
        subject,
        scenario,
        gates,
        NativeCompletionProgram::Channel { action, reply },
    )?;
    if let Some(error) = owner.error.take() {
        return Err(error);
    }
    owner
        .value
        .ok_or_else(|| PyRuntimeError::new_err("native channel returned no response"))
}

fn validate_completion_adversarial(scenario: &Bound<'_, PyAny>) -> PyResult<()> {
    let mutation: String = runtime_scenario_item(scenario, "mutation")?
        .ok_or_else(|| PyValueError::new_err("missing completion mutation"))?
        .extract()?;
    match mutation.as_str() {
        "terminal-nonterminal-conflict" => Err(PyValueError::new_err("conflicting terminal state")),
        "python-panic-payload" => Err(PyValueError::new_err("python panic payload rejected")),
        "stale-callable" => Err(PyValueError::new_err("stale callable rejected")),
        _ => Err(PyValueError::new_err("unknown completion mutation")),
    }
}

struct SessionHarnessOriginOwner {
    subject: Py<PyAny>,
    scenario: Py<PyAny>,
    gates: Py<PyAny>,
    retained: Py<PyAny>,
    error: Option<PyErr>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

struct OneChunkUploadBody {
    chunk: Option<Bytes>,
}

impl requests::AsyncBody for OneChunkUploadBody {
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<requests::Result<Bytes>>> {
        std::task::Poll::Ready(self.chunk.take().map(Ok))
    }

    fn size_hint(&self) -> Option<u64> {
        self.chunk
            .as_ref()
            .map(|chunk| u64::try_from(chunk.len()).unwrap_or(u64::MAX))
    }
}

fn execute_session_harness_action(
    py: Python<'_>,
    action: SessionHarnessAction,
    owner: &mut SessionHarnessOriginOwner,
) -> SessionHarnessReply {
    let result = (|| -> PyResult<()> {
        let subject = owner.subject.bind(py);
        let scenario = owner.scenario.bind(py);
        let collaborator = runtime_scenario_item(scenario, "collaborator")?
            .ok_or_else(|| PyValueError::new_err("missing harness collaborator"))?;
        match action {
            SessionHarnessAction::Cancellation(phase) => match phase {
                RuntimeCancellationPhase::BeforePoll => {
                    collaborator.call_method0("before_poll")?;
                }
                RuntimeCancellationPhase::QueuedBeforeDequeue => {
                    collaborator.call_method0("queued")?;
                    collaborator.call_method0("phase_wait")?;
                }
                RuntimeCancellationPhase::ReplyObserved => {
                    collaborator.call_method0("queued")?;
                    collaborator.call_method0("dequeued")?;
                    collaborator.call_method0("executed")?;
                    collaborator.call_method0("reply_observed")?;
                    collaborator.call_method0("phase_wait")?;
                }
                RuntimeCancellationPhase::TerminalAfterTimeout => {
                    collaborator.call_method0("queued")?;
                    collaborator.call_method0("dequeued")?;
                    collaborator.call_method0("executed")?;
                    collaborator.call_method0("reply_observed")?;
                    collaborator.call_method0("timeout")?;
                    collaborator.call_method0("terminal")?;
                    collaborator.call_method0("phase_wait")?;
                }
                RuntimeCancellationPhase::PermanentlyNonterminal => {
                    collaborator.call_method0("queued")?;
                    collaborator.call_method0("dequeued")?;
                    collaborator.call_method0("executed")?;
                    collaborator.call_method0("timeout")?;
                    collaborator.call_method0("phase_wait")?;
                }
            },
            SessionHarnessAction::Checkpoint(checkpoint) => match checkpoint.phase {
                SessionPhase::ConnectBlocked => {
                    let operation = RuntimeOperation::parse(scenario)?;
                    if operation == RuntimeOperation::ConnectBlocked {
                        collaborator.call_method1("connect_dial", (subject.getattr("dirty")?,))?;
                    }
                }
                SessionPhase::ConnectReadyRace => {
                    let operation = RuntimeOperation::parse(scenario)?;
                    if operation == RuntimeOperation::ConnectReadyRace {
                        collaborator.call_method1("connect_dial", (subject.getattr("dirty")?,))?;
                        let write_fd: i32 =
                            runtime_scenario_item(scenario, "native_ready_write_fd")?
                                .ok_or_else(|| PyValueError::new_err("missing native ready gate"))?
                                .extract()?;
                        py.detach(move || -> std::io::Result<()> {
                            let mut ready = std::fs::OpenOptions::new()
                                .write(true)
                                .open(format!("/proc/self/fd/{write_fd}"))?;
                            ready.write_all(b"R")
                        })
                        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
                    }
                }
                SessionPhase::ResponseHead => {
                    if RuntimeOperation::parse(scenario)? == RuntimeOperation::InterruptResponseHead
                    {
                        collaborator
                            .call_method1("response_head_wait", (subject.getattr("dirty")?,))?;
                    }
                }
                SessionPhase::ResponseRemainder => {
                    if RuntimeOperation::parse(scenario)?
                        == RuntimeOperation::InterruptResponseRemainder
                    {
                        collaborator.call_method1(
                            "response_remainder_wait",
                            (subject.getattr("dirty")?,),
                        )?;
                    }
                }
                SessionPhase::OriginUploadQueued => {
                    collaborator.call_method0("upload_queued")?;
                }
                SessionPhase::OriginUploadExecuted => {
                    collaborator.call_method0("upload_executed")?;
                    collaborator.call_method1("upload_read", (subject.getattr("dirty")?,))?;
                    collaborator.call_method0("upload_wait")?;
                }
                SessionPhase::OriginUploadReply => {
                    collaborator.call_method0("upload_reply")?;
                }
                SessionPhase::PoolAcquire
                | SessionPhase::PoolReleaseClean
                | SessionPhase::PoolReleaseDirty
                | SessionPhase::PoolClear
                | SessionPhase::WorkerEntered
                | SessionPhase::WorkerDropped => {}
            },
            SessionHarnessAction::LoopbackRequest(bytes) => {
                if RuntimeOperation::parse(scenario)? == RuntimeOperation::InterruptResponseHead {
                    collaborator.call_method1("request_sent", (bytes,))?;
                }
            }
            SessionHarnessAction::LoopbackPartial { declared, bytes } => {
                collaborator.call_method1("response_headers", (declared, bytes))?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => SessionHarnessReply::Ack,
        Err(error) => {
            if owner.error.is_none() {
                owner.error = Some(error);
            }
            SessionHarnessReply::Failed
        }
    }
}

async fn session_loopback_exchange(
    operation: RuntimeOperation,
    actions: ActionSender<SessionHarnessAction, SessionHarnessReply>,
    harness: SessionRuntimeHarness,
) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener,
        Err(_) => return false,
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(_) => return false,
    };
    let server_actions = actions.clone();
    let server_harness = harness.clone();
    let server = async move {
        let (mut stream, _) = listener.accept().await.map_err(|_| ())?;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.map_err(|_| ())?;
            if read == 0 {
                return Err(());
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let request_line_end = request
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(())?;
        let mut observed = request[..request_line_end].to_vec();
        observed.extend_from_slice(b"\r\n\r\n");
        let _ = server_actions
            .request(SessionHarnessAction::LoopbackRequest(observed))
            .await;
        let request_correlation = server_harness.claim_request_observation().await;
        server_harness.mark_request_observed(request_correlation);
        if operation == RuntimeOperation::InterruptResponseRemainder {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab")
                .await
                .map_err(|_| ())?;
            let _ = server_actions
                .request(SessionHarnessAction::LoopbackPartial {
                    declared: 5,
                    bytes: b"ab".to_vec(),
                })
                .await;
        }
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok::<(), ()>(())
    };
    let client = async move {
        let client = match requests::Client::builder()
            .session_runtime_harness(harness)
            .build()
        {
            Ok(client) => client,
            Err(_) => return false,
        };
        let url = format!("http://{address}/task16");
        let response = if operation == RuntimeOperation::InterruptOriginUpload {
            client
                .post(&url)
                .body(requests::BodySource::Stream(Box::pin(OneChunkUploadBody {
                    chunk: Some(Bytes::from_static(b"body")),
                })))
                .send()
                .await
        } else {
            client.get(&url).send().await
        };
        if operation == RuntimeOperation::InterruptResponseRemainder {
            match response {
                Ok(response) => response.bytes().await.is_ok(),
                Err(_) => false,
            }
        } else {
            response.is_ok()
        }
    };
    let (client, server) = tokio::join!(client, server);
    client && server.is_ok()
}

fn run_session_harness_interrupt(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
    evidence: Arc<NativeInterruptEvidence>,
) -> PyResult<()> {
    let owner = SessionHarnessOriginOwner {
        subject: subject.clone().unbind(),
        scenario: scenario.clone().unbind(),
        gates: gates.clone().unbind(),
        retained: subject.getattr("owner")?.unbind(),
        error: None,
        _not_send_or_sync: PhantomData,
    };
    let cancellation_phase = match operation {
        RuntimeOperation::CancelBeforePoll => Some(RuntimeCancellationPhase::BeforePoll),
        RuntimeOperation::CancelQueuedBeforeDequeue => {
            Some(RuntimeCancellationPhase::QueuedBeforeDequeue)
        }
        RuntimeOperation::CancelReplyObserved => Some(RuntimeCancellationPhase::ReplyObserved),
        RuntimeOperation::CancelTerminalAfterTimeout => {
            Some(RuntimeCancellationPhase::TerminalAfterTimeout)
        }
        RuntimeOperation::CancelPermanentlyNonterminal => {
            Some(RuntimeCancellationPhase::PermanentlyNonterminal)
        }
        _ => None,
    };
    let result = run_with_owned_actions(
        py,
        owner,
        move |actions| {
            if let Some(phase) = cancellation_phase {
                return Box::pin(async move {
                    let _ = actions
                        .request(SessionHarnessAction::Cancellation(phase))
                        .await;
                    if phase == RuntimeCancellationPhase::PermanentlyNonterminal {
                        loop {
                            thread::park();
                        }
                    }
                    std::future::pending::<bool>().await
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;
            }
            let blocking_phase = match operation {
                RuntimeOperation::ConnectBlocked => SessionPhase::ConnectBlocked,
                RuntimeOperation::ConnectReadyRace => SessionPhase::ConnectReadyRace,
                RuntimeOperation::InterruptResponseHead => SessionPhase::ResponseHead,
                RuntimeOperation::InterruptResponseRemainder => SessionPhase::ResponseRemainder,
                RuntimeOperation::InterruptOriginUpload => SessionPhase::OriginUploadExecuted,
                _ => SessionPhase::WorkerEntered,
            };
            let hooks = NativeSessionRuntimeHooks {
                actions: actions.clone(),
                blocking_phase,
                evidence: evidence.clone(),
            };
            let harness = SessionRuntimeHarness::new(Arc::new(hooks));
            Box::pin(session_loopback_exchange(operation, actions, harness))
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        },
        execute_session_harness_action,
    );
    match result {
        Ok((_, owner)) => match owner.error {
            Some(error) => Err(error),
            None => Err(PyRuntimeError::new_err(
                "session harness operation completed without interruption",
            )),
        },
        Err(error) => Err(error),
    }
}

fn runtime_interrupt_phase(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<Py<PyAny>> {
    let phase = operation
        .interrupt_phase()
        .ok_or_else(|| PyValueError::new_err("invalid interrupt operation"))?;
    let generation = runtime_scenario_item(scenario, "generation")?
        .ok_or_else(|| PyValueError::new_err("missing runtime generation"))?;
    let dirty = subject.getattr("dirty")?;
    let owner = subject.getattr("owner")?;
    let native_generation = BlockingRuntimeDriver::process_local()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .generation();
    LAST_INTERRUPT_RUNTIME_GENERATION.with(|slot| slot.set(Some(native_generation)));

    let evidence = Arc::new(NativeInterruptEvidence::default());
    let operation_result =
        run_session_harness_interrupt(py, subject, scenario, gates, operation, evidence.clone());

    let error = match operation_result {
        Ok(()) => {
            return Err(PyRuntimeError::new_err(
                "interrupt phase completed without cancellation",
            ));
        }
        Err(error) => error,
    };
    if !error.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
        return Err(error);
    }
    if phase == "response-head-wait" && !no_post_head_actions(&evidence) {
        return Err(PyRuntimeError::new_err(
            "post-head action escaped cancellation",
        ));
    }
    if phase == "response-remainder-wait" && !no_synthetic_eof_or_content(&evidence) {
        return Err(PyRuntimeError::new_err(
            "response remainder cancellation synthesized completion",
        ));
    }
    subject.call_method1("cancelled", (&dirty, &generation))?;
    subject.call_method1("worker_dropped", (&dirty,))?;
    worker_drop_before_origin_owner(operation != RuntimeOperation::CancelPermanentlyNonterminal)?;
    subject.call_method1("quarantined", (&owner, phase))?;
    if last_origin_quarantine_token().is_none() {
        return Err(PyRuntimeError::new_err(
            "runtime cancellation produced no quarantine token",
        ));
    }
    Err(error)
}

fn runtime_recover_phase(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
    operation: RuntimeOperation,
) -> PyResult<Py<PyAny>> {
    let dirty = subject.getattr("dirty")?;
    let generation = runtime_scenario_item(scenario, "generation")?
        .ok_or_else(|| PyValueError::new_err("missing runtime generation"))?;
    if let Ok(subject_generation) = subject.getattr("generation") {
        if !subject_generation.is(&generation) {
            return Err(PyValueError::new_err("stale runtime generation"));
        }
    }
    let current_native_generation = BlockingRuntimeDriver::process_local()
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        .generation();
    let native_generation_matches = LAST_INTERRUPT_RUNTIME_GENERATION.with(|slot| {
        slot.get().is_some_and(|expected| {
            recover_same_runtime_generation(expected, current_native_generation)
        })
    });
    if !native_generation_matches {
        return Err(PyValueError::new_err("stale native runtime generation"));
    }
    let declared_recovery = subject.getattr("recovery")?;
    if declared_recovery.is(&dirty) {
        return Err(PyValueError::new_err("dirty resource reused for recovery"));
    }
    let phase = operation
        .interrupt_phase()
        .ok_or_else(|| PyValueError::new_err("invalid recovery operation"))?;
    let quarantine_token = last_origin_quarantine_token();
    if quarantine_token.is_none()
        && gates
            .getattr("release")?
            .call_method0("is_set")?
            .is_truthy()?
    {
        return Err(PyValueError::new_err("release gate is already set"));
    }
    let recovery = subject.call_method1("recover", (&dirty, &generation))?;
    if !recovery.is(&declared_recovery) {
        return Err(PyValueError::new_err("recovery resource identity changed"));
    }
    let collaborator = runtime_scenario_item(scenario, "collaborator")?
        .ok_or_else(|| PyValueError::new_err("missing recovery collaborator"))?;
    if phase == "origin-upload-action-wait" {
        collaborator.call_method0("upload_close")?;
    }
    if let Some(token) = quarantine_token
        && origin_quarantine_is_terminal(token).unwrap_or(false)
        && let Some(quarantined) = take_origin_quarantine_owner::<SessionHarnessOriginOwner>(token)
    {
        subject.call_method1("origin_reaped", (quarantined.retained.bind(py), phase))?;
    }
    Ok(recovery.unbind())
}

#[pyfunction]
fn _session_runtime_trial(
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let py = subject.py();
    let operation = RuntimeOperation::parse(scenario)?;
    match operation {
        RuntimeOperation::Affinity => runtime_affinity_program(py, subject, scenario, gates),
        RuntimeOperation::PayloadBoundary => runtime_payload_program(py, subject, scenario, gates),
        RuntimeOperation::NestedRequest => runtime_nested_program(py, subject, scenario),
        RuntimeOperation::FirstError => runtime_error_program(py, subject, scenario),
        RuntimeOperation::AdapterInterruptConnect
        | RuntimeOperation::AdapterInterruptResponseHead
        | RuntimeOperation::AdapterInterruptResponseRead
        | RuntimeOperation::AdapterInterruptUpload => {
            runtime_interruption_program(py, subject, scenario, gates, operation)
        }
        RuntimeOperation::CancellationMatrix => {
            runtime_cancellation_program(py, subject, scenario, gates)
        }
        RuntimeOperation::ForkMatrix => runtime_fork_program(py, subject, scenario, gates),
        RuntimeOperation::SessionIsolation => runtime_session_isolation_program(py, subject, gates),
        RuntimeOperation::StreamIsolation => {
            runtime_stream_isolation_program(py, subject, scenario, gates)
        }
        RuntimeOperation::FinalizeClose => bounded_session_finalization(py, subject),
        RuntimeOperation::PanicRecovery => {
            translate_session_worker_panic(py, subject, scenario, gates)
        }
        RuntimeOperation::LiveClockMutation => {
            reload_live_python_authority_after_await(py, subject)
        }
        RuntimeOperation::SendConcurrentChannel => {
            independent_concurrent_session_channels(py, subject, scenario)
        }
        RuntimeOperation::ConnectBlocked
        | RuntimeOperation::ConnectReadyRace
        | RuntimeOperation::InterruptResponseHead
        | RuntimeOperation::InterruptResponseRemainder
        | RuntimeOperation::InterruptOriginUpload
        | RuntimeOperation::CancelBeforePoll
        | RuntimeOperation::CancelQueuedBeforeDequeue
        | RuntimeOperation::CancelReplyObserved
        | RuntimeOperation::CancelTerminalAfterTimeout
        | RuntimeOperation::CancelPermanentlyNonterminal => {
            runtime_interrupt_phase(py, subject, scenario, gates, operation)
        }
        RuntimeOperation::RecoverConnectBlocked
        | RuntimeOperation::RecoverConnectReadyRace
        | RuntimeOperation::RecoverResponseHead
        | RuntimeOperation::RecoverResponseRemainder
        | RuntimeOperation::RecoverOriginUpload
        | RuntimeOperation::RecoverCancelBeforePoll
        | RuntimeOperation::RecoverCancelQueuedBeforeDequeue
        | RuntimeOperation::RecoverCancelReplyObserved
        | RuntimeOperation::RecoverCancelTerminalAfterTimeout
        | RuntimeOperation::RecoverCancelPermanentlyNonterminal => {
            runtime_recover_phase(py, subject, scenario, gates, operation)
        }
        RuntimeOperation::ForkPrepareImport
        | RuntimeOperation::ForkPrepareDriver
        | RuntimeOperation::ForkPreparePool
        | RuntimeOperation::ForkChildUseAfterImport
        | RuntimeOperation::ForkChildUseAfterDriver
        | RuntimeOperation::ForkChildUseAfterPool
        | RuntimeOperation::ForkParentUseAfterImport
        | RuntimeOperation::ForkParentUseAfterDriver
        | RuntimeOperation::ForkParentUseAfterPool
        | RuntimeOperation::ForkChildCleanupAfterImport
        | RuntimeOperation::ForkChildCleanupAfterDriver
        | RuntimeOperation::ForkChildCleanupAfterPool
        | RuntimeOperation::MultiSessionIsolation
        | RuntimeOperation::OutstandingStreamIsolation
        | RuntimeOperation::ValidateStalePid
        | RuntimeOperation::ValidateStaleGeneration
        | RuntimeOperation::ValidateInheritedPool
        | RuntimeOperation::ValidateReleasedLease
        | RuntimeOperation::ValidateDuplicateCorrelation => {
            runtime_native_isolation_audit(py, subject, scenario, operation)
        }
        RuntimeOperation::FinalizeTerminal | RuntimeOperation::FinalizePermanentlyNonterminal => {
            bounded_native_session_finalization(py, subject, scenario, gates)
        }
        RuntimeOperation::InjectNativeWorkerPanic => {
            native_worker_panic_or_recovery(py, subject, scenario, false)
        }
        RuntimeOperation::RecoverNativeWorkerPanic => {
            native_worker_panic_or_recovery(py, subject, scenario, true)
        }
        RuntimeOperation::AwaitLiveAuthority => {
            native_await_live_authority(py, subject, scenario, gates)
        }
        RuntimeOperation::ConcurrentChannelReply | RuntimeOperation::ConcurrentChannelFail => {
            native_concurrent_session_channel(py, subject, scenario, gates, operation)
        }
        RuntimeOperation::ValidateTerminalNonterminalConflict
        | RuntimeOperation::ValidatePythonPanicPayload
        | RuntimeOperation::ValidateStaleCallable => {
            validate_completion_adversarial(scenario).map(|_| py.None())
        }
    }
}

fn run_session_facade(
    py: Python<'_>,
    session: &Bound<'_, PyAny>,
    args: &Bound<'_, PyTuple>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    let _pump_guard = PublicPumpGuard::enter();
    crate::adapters::send_from_session(
        py,
        crate::adapters::PublicSessionSend::Compatibility {
            session,
            args,
            kwargs,
        },
    )
}

#[pyfunction]
fn _session_facade_trial(
    py: Python<'_>,
    session: &Bound<'_, PyAny>,
    operation: &str,
    args: &Bound<'_, PyTuple>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    if operation == "send" {
        return run_session_facade(py, session, args, kwargs);
    }
    Ok(py.NotImplemented())
}

#[pyfunction]
fn _public_facade_pump_trial(py: Python<'_>, operation: &str) -> PyResult<Py<PyAny>> {
    let observation =
        PUBLIC_PUMP_OBSERVATION.get_or_init(|| Mutex::new(PublicPumpObservation::default()));
    if operation == "reset" {
        let mut state = observation
            .lock()
            .map_err(|_| PyRuntimeError::new_err("public pump observation lock poisoned"))?;
        *state = PublicPumpObservation {
            process_id: std::process::id(),
            ..PublicPumpObservation::default()
        };
        NEXT_PUBLIC_SUBMISSION.store(1, Ordering::Relaxed);
        PUBLIC_PUMP_OBSERVATION_ENABLED.store(true, Ordering::Release);
        return Ok(py.None());
    }
    if operation != "snapshot" {
        return Err(PyValueError::new_err(
            "unknown public pump observation operation",
        ));
    }
    PUBLIC_PUMP_OBSERVATION_ENABLED.store(false, Ordering::Release);
    let mut observation = observation
        .lock()
        .map_err(|_| PyRuntimeError::new_err("public pump observation lock poisoned"))?;
    let observation = if observation.process_id == std::process::id() {
        std::mem::take(&mut *observation)
    } else {
        *observation = PublicPumpObservation::default();
        PublicPumpObservation::default()
    };
    let result = PyDict::new(py);
    result.set_item("outer_entries", observation.outer_entries)?;
    result.set_item("outer_exits", observation.outer_exits)?;
    result.set_item("max_depth", observation.max_depth)?;
    result.set_item("adapter_leaf_entries", observation.adapter_leaf_entries)?;
    result.set_item("nested_pump_entries", observation.nested_pump_entries)?;
    result.set_item("submission_ids", &observation.submission_ids)?;
    result.set_item("submission_parent_ids", &observation.submission_parent_ids)?;
    result.set_item(
        "adapter_submission_ids",
        &observation.adapter_submission_ids,
    )?;
    Ok(result.into_any().unbind())
}

#[pyfunction]
fn _session_pipeline_trial(
    subject: &Bound<'_, PyAny>,
    scenario: &Bound<'_, PyAny>,
    _gates: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    run_session_pipeline(subject.py(), subject, scenario)
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_session_redirect_cursor_claim, module)?)?;
    module.add_function(wrap_pyfunction!(_session_redirect_cursor_next, module)?)?;
    module.add_function(wrap_pyfunction!(_session_redirect_cursor_close, module)?)?;
    module.add_function(wrap_pyfunction!(_session_redirect_cursor_drop, module)?)?;
    module.add_function(wrap_pyfunction!(_session_redirect_cursor_frame, module)?)?;
    module.add_function(wrap_pyfunction!(_session_runtime_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_session_pipeline_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_session_facade_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_public_facade_pump_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod task16_session_payload_contract {
    use super::*;
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload + Send + 'static>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    trait AmbiguousIfSend<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

    struct BorrowedValue<'a>(&'a str);

    #[test]
    fn nested_public_pump_guard_restores_the_prior_tls_submission() {
        PUBLIC_PUMP_OBSERVATION_ENABLED.store(true, Ordering::Release);
        *PUBLIC_PUMP_OBSERVATION
            .get_or_init(|| Mutex::new(PublicPumpObservation::default()))
            .lock()
            .expect("public pump observation lock") = PublicPumpObservation {
            process_id: std::process::id(),
            ..PublicPumpObservation::default()
        };
        NEXT_PUBLIC_SUBMISSION.store(1, Ordering::Relaxed);
        PUBLIC_PUMP_SUBMISSION.with(|current| current.set(77));
        PUBLIC_PUMP_DEPTH.with(|depth| depth.set(0));

        {
            let _outer = PublicPumpGuard::enter();
            assert_eq!(PUBLIC_PUMP_SUBMISSION.with(Cell::get), 1);
            {
                let _inner = PublicPumpGuard::enter();
                assert_eq!(PUBLIC_PUMP_SUBMISSION.with(Cell::get), 2);
            }
            assert_eq!(PUBLIC_PUMP_SUBMISSION.with(Cell::get), 1);
        }

        assert_eq!(PUBLIC_PUMP_SUBMISSION.with(Cell::get), 77);
        assert_eq!(PUBLIC_PUMP_DEPTH.with(Cell::get), 0);
        PUBLIC_PUMP_SUBMISSION.with(|current| current.set(0));
        PUBLIC_PUMP_OBSERVATION_ENABLED.store(false, Ordering::Release);
    }

    #[test]
    fn all_session_payload_variants_are_worker_payloads() {
        assert_worker_payload::<SessionAction>();
        assert_worker_payload::<SessionReply>();
        assert_worker_payload::<NativeTransfer>();
        assert_worker_payload::<CompletionAction>();
        assert_worker_payload::<CompletionReply>();
        assert_worker_payload::<CompletionEnvelope>();
    }

    #[test]
    fn every_action_variant_constructs_and_exhaustively_destructures() {
        let generation = GenerationId::checked(7).unwrap();
        let sequence = Sequence::checked(11).unwrap();
        let correlation = CorrelationId::checked(13).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let jar_id = JarId::checked(29, generation).unwrap();
        let hook_id = HookId::checked(31, generation).unwrap();
        let auth_id = AuthId::checked(37, generation).unwrap();
        let cursor_id = CursorId::checked(39, generation).unwrap();
        let opaque_value_id = OpaqueValueId::checked(41, generation).unwrap();
        let _ = (cursor_id, opaque_value_id);

        let actions = [
            SessionAction::ReadGlobal {
                authority: GlobalAuthority::Sessions,
                generation,
                sequence,
            },
            SessionAction::ReadBody {
                request_id,
                generation,
                sequence,
            },
            SessionAction::SendCustomAdapter {
                adapter_id,
                request_id,
                generation,
                correlation,
                sequence,
            },
            SessionAction::DispatchHook {
                hook_id,
                response_id,
                generation,
                correlation,
                sequence,
            },
            SessionAction::RunAuth {
                auth_id,
                request_id,
                generation,
                sequence,
            },
            SessionAction::ExtractCookies {
                jar_id,
                request_id,
                response_id,
                generation,
                sequence,
            },
            SessionAction::NestedSubmit {
                request_id,
                generation,
                parent_correlation: correlation,
                correlation,
                sequence,
            },
        ];

        for action in actions {
            match action {
                SessionAction::ReadGlobal {
                    authority,
                    generation,
                    sequence,
                } => {
                    let _ = (authority, generation, sequence);
                }
                SessionAction::ReadBody {
                    request_id,
                    generation,
                    sequence,
                } => {
                    let _ = (request_id, generation, sequence);
                }
                SessionAction::SendCustomAdapter {
                    adapter_id,
                    request_id,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (adapter_id, request_id, generation, correlation, sequence);
                }
                SessionAction::DispatchHook {
                    hook_id,
                    response_id,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (hook_id, response_id, generation, correlation, sequence);
                }
                SessionAction::RunAuth {
                    auth_id,
                    request_id,
                    generation,
                    sequence,
                } => {
                    let _ = (auth_id, request_id, generation, sequence);
                }
                SessionAction::ExtractCookies {
                    jar_id,
                    request_id,
                    response_id,
                    generation,
                    sequence,
                } => {
                    let _ = (jar_id, request_id, response_id, generation, sequence);
                }
                SessionAction::NestedSubmit {
                    request_id,
                    generation,
                    parent_correlation,
                    correlation,
                    sequence,
                } => {
                    let _ = (
                        request_id,
                        generation,
                        parent_correlation,
                        correlation,
                        sequence,
                    );
                }
            }
        }
    }

    #[test]
    fn every_reply_and_transfer_constructs_and_exhaustively_destructures() {
        let generation = GenerationId::checked(7).unwrap();
        let sequence = Sequence::checked(11).unwrap();
        let correlation = CorrelationId::checked(13).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let value = OpaqueValueId::checked(41, generation).unwrap();
        let error_id = OpaqueValueId::checked(43, generation).unwrap();

        let replies = [
            SessionReply::Scalar {
                value,
                generation,
                correlation,
                sequence,
            },
            SessionReply::Response {
                response_id,
                generation,
                correlation,
                sequence,
            },
            SessionReply::Nested {
                request_id,
                generation,
                correlation,
                sequence,
            },
            SessionReply::Raised {
                error_id,
                generation,
                correlation,
                sequence,
            },
        ];
        for reply in replies {
            match reply {
                SessionReply::Scalar {
                    value,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (value, generation, correlation, sequence);
                }
                SessionReply::Response {
                    response_id,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (response_id, generation, correlation, sequence);
                }
                SessionReply::Nested {
                    request_id,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (request_id, generation, correlation, sequence);
                }
                SessionReply::Raised {
                    error_id,
                    generation,
                    correlation,
                    sequence,
                } => {
                    let _ = (error_id, generation, correlation, sequence);
                }
            }
        }

        let transfer = NativeTransfer {
            method: MethodId::Get,
            url: UrlId::checked(47, generation).unwrap(),
            headers: HeadersId::checked(53, generation).unwrap(),
            body_id: value,
            adapter_id,
            generation,
            correlation,
        };
        let NativeTransfer {
            method,
            url,
            headers,
            body_id,
            adapter_id,
            generation,
            correlation,
        } = transfer;
        let _ = (
            method,
            url,
            headers,
            body_id,
            adapter_id,
            generation,
            correlation,
        );
    }

    #[test]
    fn python_and_origin_values_are_not_worker_payloads() {
        let _ = <pyo3::Py<pyo3::PyAny> as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <pyo3::PyErr as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <BorrowedValue<'static> as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <OriginSessionOwner as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <OriginSessionDestructor as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <CompletionOwner as AmbiguousIfSend<_>>::marker;
        let _ = <dyn std::fmt::Debug as AmbiguousIfWorkerPayload<_>>::marker;
    }

    #[test]
    fn checked_ids_reject_wrong_categories_and_stale_generations() {
        let generation = GenerationId::checked(7).unwrap();
        let stale = GenerationId::checked(8).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let jar_id = JarId::checked(29, generation).unwrap();
        let hook_id = HookId::checked(31, generation).unwrap();
        let auth_id = AuthId::checked(37, generation).unwrap();
        let cursor_id = CursorId::checked(39, generation).unwrap();
        let value_id = OpaqueValueId::checked(41, generation).unwrap();
        assert!(ResponseId::try_from_request(request_id).is_err());
        assert!(AdapterId::try_from_response(response_id).is_err());
        assert!(JarId::try_from_adapter(adapter_id).is_err());
        assert!(HookId::try_from_jar(jar_id).is_err());
        assert!(AuthId::try_from_hook(hook_id).is_err());
        assert!(CursorId::try_from_auth(auth_id).is_err());
        assert!(OpaqueValueId::try_from_cursor(cursor_id).is_err());
        assert!(request_id.validate_generation(stale).is_err());
        assert!(response_id.validate_generation(stale).is_err());
        assert!(adapter_id.validate_generation(stale).is_err());
        assert!(jar_id.validate_generation(stale).is_err());
        assert!(hook_id.validate_generation(stale).is_err());
        assert!(auth_id.validate_generation(stale).is_err());
        assert!(cursor_id.validate_generation(stale).is_err());
        assert!(value_id.validate_generation(stale).is_err());
    }

    #[test]
    fn checked_ids_reject_overflow_and_unchecked_allocation() {
        let generation = GenerationId::checked(7).unwrap();
        assert!(RequestId::checked(u64::MAX, generation).is_err());
        assert!(OpaqueValueId::checked(u64::MAX, generation).is_err());
        assert!(SessionIdAllocator::checked(u64::MAX).is_err());
    }
}

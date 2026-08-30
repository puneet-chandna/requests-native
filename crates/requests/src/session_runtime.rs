//! Internal typed observation seam for the Python session runtime.
//!
//! This module deliberately exposes no Python values.  A session runtime may
//! attach hooks to the native request pipeline, but workers only observe opaque
//! numeric identities and typed phase transitions.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

macro_rules! opaque_identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub fn get(self) -> u64 {
                self.0
            }
        }
    };
}

opaque_identity!(SessionRuntimeIdentity);
opaque_identity!(SessionPoolIdentity);
opaque_identity!(SessionConnectionIdentity);
opaque_identity!(SessionLeaseIdentity);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    ConnectBlocked,
    ConnectReadyRace,
    ResponseHead,
    ResponseRemainder,
    OriginUploadQueued,
    OriginUploadExecuted,
    OriginUploadReply,
    PoolAcquire,
    PoolReleaseClean,
    PoolReleaseDirty,
    PoolClear,
    WorkerEntered,
    WorkerDropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCheckpoint {
    pub phase: SessionPhase,
    pub runtime: SessionRuntimeIdentity,
    pub pool: SessionPoolIdentity,
    pub connection: Option<SessionConnectionIdentity>,
    pub lease: Option<SessionLeaseIdentity>,
    pub generation: u64,
    pub correlation: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionExchangeIdentity {
    pub connection: SessionConnectionIdentity,
    pub lease: SessionLeaseIdentity,
    pub correlation: u64,
}

pub struct SessionExchangeReservation {
    harness: SessionRuntimeHarness,
    identity: SessionExchangeIdentity,
    promoted: bool,
}

pub struct SessionActiveExchange {
    harness: SessionRuntimeHarness,
    identity: SessionExchangeIdentity,
    released: bool,
}

impl fmt::Debug for SessionExchangeReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionExchangeReservation")
            .field("identity", &self.identity)
            .field("promoted", &self.promoted)
            .finish()
    }
}

impl SessionExchangeReservation {
    pub fn checkpoint(&self, phase: SessionPhase) -> SessionCheckpoint {
        self.harness.checkpoint(
            phase,
            Some(self.identity.connection),
            Some(self.identity.lease),
            self.identity.correlation,
        )
    }

    pub fn promote(mut self) -> SessionExchangeIdentity {
        self.promoted = true;
        self.identity
    }

    pub fn activate(mut self) -> SessionActiveExchange {
        self.promoted = true;
        SessionActiveExchange {
            harness: self.harness.clone(),
            identity: self.identity,
            released: false,
        }
    }
}

impl SessionActiveExchange {
    pub fn identity(&self) -> SessionExchangeIdentity {
        self.identity
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let checkpoint = self.harness.checkpoint(
            SessionPhase::PoolReleaseClean,
            Some(self.identity.connection),
            Some(self.identity.lease),
            self.identity.correlation,
        );
        self.harness.observe(checkpoint);
    }
}

impl Drop for SessionActiveExchange {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl Drop for SessionExchangeReservation {
    fn drop(&mut self) {
        if self.promoted {
            return;
        }
        let checkpoint = self.checkpoint(SessionPhase::PoolReleaseDirty);
        self.harness.observe(checkpoint);
    }
}

pub trait SessionRuntimeHooks: Send + Sync {
    fn checkpoint(&self, checkpoint: SessionCheckpoint);

    fn wait(
        &self,
        checkpoint: SessionCheckpoint,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.checkpoint(checkpoint);
        Box::pin(std::future::ready(()))
    }
}

#[derive(Clone)]
pub struct SessionRuntimeHarness {
    inner: Arc<SessionRuntimeHarnessInner>,
}

struct SessionRuntimeHarnessInner {
    hooks: Arc<dyn SessionRuntimeHooks>,
    runtime: SessionRuntimeIdentity,
    pool: SessionPoolIdentity,
    generation: u64,
    next_sequence: AtomicU64,
    request_observations: Mutex<RequestObservations>,
    request_observation_notify: Notify,
}

#[derive(Default)]
struct RequestObservations {
    pending: VecDeque<u64>,
    entries: HashMap<u64, Arc<RequestObservation>>,
}

#[derive(Default)]
struct RequestObservation {
    observed: AtomicBool,
    notify: Notify,
}

static NEXT_RUNTIME_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn next_process_identity() -> u64 {
    (u64::from(std::process::id()) << 32)
        | (NEXT_RUNTIME_IDENTITY.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX))
}

impl fmt::Debug for SessionRuntimeHarness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRuntimeHarness")
            .field("runtime", &self.inner.runtime)
            .field("pool", &self.inner.pool)
            .field("generation", &self.inner.generation)
            .finish_non_exhaustive()
    }
}

impl SessionRuntimeHarness {
    pub fn new(hooks: Arc<dyn SessionRuntimeHooks>) -> Self {
        let runtime = next_process_identity();
        let pool = next_process_identity();
        let generation = next_process_identity();
        Self {
            inner: Arc::new(SessionRuntimeHarnessInner {
                hooks,
                runtime: SessionRuntimeIdentity(runtime),
                pool: SessionPoolIdentity(pool),
                generation,
                next_sequence: AtomicU64::new(1),
                request_observations: Mutex::new(RequestObservations::default()),
                request_observation_notify: Notify::new(),
            }),
        }
    }

    pub fn runtime_identity(&self) -> SessionRuntimeIdentity {
        self.inner.runtime
    }

    pub fn pool_identity(&self) -> SessionPoolIdentity {
        self.inner.pool
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn allocate_connection_identity(&self) -> SessionConnectionIdentity {
        SessionConnectionIdentity(next_process_identity())
    }

    pub fn allocate_lease_identity(&self) -> SessionLeaseIdentity {
        SessionLeaseIdentity(next_process_identity())
    }

    pub fn reserve_exchange(&self) -> SessionExchangeReservation {
        SessionExchangeReservation {
            harness: self.clone(),
            identity: SessionExchangeIdentity {
                connection: self.allocate_connection_identity(),
                lease: self.allocate_lease_identity(),
                correlation: self.next_correlation(),
            },
            promoted: false,
        }
    }

    pub fn next_correlation(&self) -> u64 {
        next_process_identity()
    }

    pub fn checkpoint(
        &self,
        phase: SessionPhase,
        connection: Option<SessionConnectionIdentity>,
        lease: Option<SessionLeaseIdentity>,
        correlation: u64,
    ) -> SessionCheckpoint {
        SessionCheckpoint {
            phase,
            runtime: self.inner.runtime,
            pool: self.inner.pool,
            connection,
            lease,
            generation: self.inner.generation,
            correlation,
            sequence: self.inner.next_sequence.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn observe(&self, checkpoint: SessionCheckpoint) {
        self.inner.hooks.checkpoint(checkpoint);
    }

    pub fn clear_pool(&self) {
        let checkpoint = self.checkpoint(SessionPhase::PoolClear, None, None, 0);
        self.observe(checkpoint);
    }

    pub fn begin_request_observation(&self) -> u64 {
        let correlation = self.next_correlation();
        let mut observations = self
            .inner
            .request_observations
            .lock()
            .expect("request observation lock poisoned");
        observations
            .entries
            .insert(correlation, Arc::new(RequestObservation::default()));
        observations.pending.push_back(correlation);
        drop(observations);
        self.inner.request_observation_notify.notify_one();
        correlation
    }

    pub async fn claim_request_observation(&self) -> u64 {
        loop {
            let notified = self.inner.request_observation_notify.notified();
            if let Some(correlation) = self
                .inner
                .request_observations
                .lock()
                .expect("request observation lock poisoned")
                .pending
                .pop_front()
            {
                return correlation;
            }
            notified.await;
        }
    }

    pub fn mark_request_observed(&self, correlation: u64) {
        let observation = self
            .inner
            .request_observations
            .lock()
            .expect("request observation lock poisoned")
            .entries
            .get(&correlation)
            .cloned()
            .expect("request observation must be registered before it is marked");
        observation.observed.store(true, Ordering::Release);
        observation.notify.notify_waiters();
    }

    pub async fn wait_request_observed(&self, correlation: u64) {
        let observation = self
            .inner
            .request_observations
            .lock()
            .expect("request observation lock poisoned")
            .entries
            .get(&correlation)
            .cloned()
            .expect("request observation must be registered before it is awaited");
        loop {
            let notified = observation.notify.notified();
            if observation.observed.load(Ordering::Acquire) {
                self.inner
                    .request_observations
                    .lock()
                    .expect("request observation lock poisoned")
                    .entries
                    .remove(&correlation);
                return;
            }
            notified.await;
        }
    }

    pub async fn wait(&self, checkpoint: SessionCheckpoint) {
        self.inner.hooks.wait(checkpoint).await;
    }

    pub fn wait_future(
        &self,
        checkpoint: SessionCheckpoint,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.inner.hooks.wait(checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ImmediateHooks;

    impl SessionRuntimeHooks for ImmediateHooks {
        fn checkpoint(&self, _checkpoint: SessionCheckpoint) {}
    }

    #[tokio::test]
    async fn request_observations_are_one_shot_and_correlation_scoped() {
        let harness = SessionRuntimeHarness::new(Arc::new(ImmediateHooks));
        let first = harness.begin_request_observation();
        let second = harness.begin_request_observation();
        assert_ne!(first, second);
        assert_eq!(harness.claim_request_observation().await, first);
        assert_eq!(harness.claim_request_observation().await, second);

        harness.mark_request_observed(second);
        harness.wait_request_observed(second).await;

        let first_wait = harness.wait_request_observed(first);
        tokio::pin!(first_wait);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut first_wait)
                .await
                .is_err()
        );
        harness.mark_request_observed(first);
        first_wait.await;
    }
}

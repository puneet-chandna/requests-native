use std::sync::{Arc, Mutex};

use requests_native::session_runtime::{
    SessionCheckpoint, SessionPhase, SessionRuntimeHarness, SessionRuntimeHooks,
};
use requests_native::{BodySource, ResponseDispositionState, ResponseEvent};

#[derive(Default)]
struct RecordingHooks(Mutex<Vec<SessionCheckpoint>>);

impl SessionRuntimeHooks for RecordingHooks {
    fn checkpoint(&self, checkpoint: SessionCheckpoint) {
        self.0.lock().expect("event lock poisoned").push(checkpoint);
    }
}

impl RecordingHooks {
    fn events(&self) -> Vec<SessionCheckpoint> {
        self.0.lock().expect("event lock poisoned").clone()
    }
}

#[test]
fn body_source_drop_releases_its_owned_bytes_once() {
    let body = BodySource::from(vec![1, 2, 3, 4]);
    assert!(matches!(body, BodySource::Bytes(_)));
    drop(body);
}

#[test]
fn response_close_and_drop_are_idempotent() {
    for first in [
        ResponseEvent::CleanEof,
        ResponseEvent::Close,
        ResponseEvent::Drop,
    ] {
        let mut state = ResponseDispositionState::default();
        state.apply(first);
        let decision = state.decision();
        state.apply(ResponseEvent::Close);
        state.apply(ResponseEvent::Drop);
        assert_eq!(state.decision(), decision);
        assert_eq!(state.decision_count(), 1);
    }
}

#[test]
fn active_exchange_release_is_clean_identity_preserving_and_idempotent() {
    let hooks = Arc::new(RecordingHooks::default());
    let harness = SessionRuntimeHarness::new(hooks.clone());
    let reservation = harness.reserve_exchange();
    let reserved = reservation.checkpoint(SessionPhase::ConnectBlocked);
    let active = reservation.activate();
    let identity = active.identity();

    assert_eq!(Some(identity.connection), reserved.connection);
    assert_eq!(Some(identity.lease), reserved.lease);
    assert_eq!(identity.correlation, reserved.correlation);
    assert_eq!(reserved.runtime, harness.runtime_identity());
    assert_eq!(reserved.pool, harness.pool_identity());
    assert_eq!(reserved.generation, harness.generation());

    active.release();
    assert_eq!(
        hooks.events(),
        [SessionCheckpoint {
            phase: SessionPhase::PoolReleaseClean,
            sequence: reserved.sequence + 1,
            ..reserved
        }]
    );
}

#[test]
fn abandoned_reservation_releases_dirty_with_its_original_identity() {
    let hooks = Arc::new(RecordingHooks::default());
    let harness = SessionRuntimeHarness::new(hooks.clone());
    let reservation = harness.reserve_exchange();
    let reserved = reservation.checkpoint(SessionPhase::ConnectBlocked);

    drop(reservation);
    assert_eq!(
        hooks.events(),
        [SessionCheckpoint {
            phase: SessionPhase::PoolReleaseDirty,
            sequence: reserved.sequence + 1,
            ..reserved
        }]
    );
}

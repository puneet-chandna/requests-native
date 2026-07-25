//! Python-free response state used by the compatibility binding.
//!
//! Content projection and connection disposition are intentionally separate.
//! A response can, for example, have an exhausted uncached body while the
//! disposition has already become dirty.

/// The observable shape of the response body cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseContent {
    /// No body read has completed and no cache has been installed.
    Streaming,
    /// The stream reached EOF without installing a cache.
    ExhaustedUncached,
    /// A byte cache has been installed.
    Cached,
    /// The response represents the `None` empty-body sentinel.
    EmptyNone,
}

/// Python-free classification of the `_content` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseCache {
    FalseSentinel,
    EmptyNone,
    Cached,
}

impl ResponseContent {
    /// Project the two fields used by `requests.Response`.
    pub const fn from_python_fields(cache: ResponseCache, content_consumed: bool) -> Self {
        match (cache, content_consumed) {
            (ResponseCache::FalseSentinel, false) => Self::Streaming,
            (ResponseCache::FalseSentinel, true) => Self::ExhaustedUncached,
            (ResponseCache::EmptyNone, _) => Self::EmptyNone,
            (ResponseCache::Cached, _) => Self::Cached,
        }
    }
}

/// Abstract events that affect a native response's reuse decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseEvent {
    Partial,
    CleanEof,
    Close,
    Drop,
    ReadError,
    DecodeError,
    ProtocolError,
    Cancel,
    ActionDisconnect,
    ReplyDisconnect,
    /// Compatibility execution is owned by the frozen Python implementation.
    PythonExact,
}

/// Monotonic disposition state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseDisposition {
    Open,
    Partial,
    Reusable,
    CloseDirty,
    PythonExact,
}

impl ResponseDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Partial => "Partial",
            Self::Reusable => "Reusable",
            Self::CloseDirty => "CloseDirty",
            Self::PythonExact => "PythonExact",
        }
    }
}

/// The single terminal decision emitted for native ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseDecision {
    Reusable,
    CloseDirty,
}

impl ResponseDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reusable => "Reusable",
            Self::CloseDirty => "CloseDirty",
        }
    }
}

/// Small state machine which owns no transport lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponseDispositionState {
    state: ResponseDisposition,
    decision: Option<ResponseDecision>,
    decision_count: u8,
    native_lease: bool,
}

impl Default for ResponseDispositionState {
    fn default() -> Self {
        Self {
            state: ResponseDisposition::Open,
            decision: None,
            decision_count: 0,
            native_lease: true,
        }
    }
}

impl ResponseDispositionState {
    pub const fn state(self) -> ResponseDisposition {
        self.state
    }

    pub const fn decision(self) -> Option<ResponseDecision> {
        self.decision
    }

    pub const fn decision_count(self) -> u8 {
        self.decision_count
    }

    pub const fn native_lease(self) -> bool {
        self.native_lease
    }

    pub fn apply(&mut self, event: ResponseEvent) -> ResponseDisposition {
        match self.state {
            ResponseDisposition::Reusable
            | ResponseDisposition::CloseDirty
            | ResponseDisposition::PythonExact => return self.state,
            ResponseDisposition::Open | ResponseDisposition::Partial => {}
        }

        match event {
            ResponseEvent::PythonExact if self.state == ResponseDisposition::Open => {
                self.state = ResponseDisposition::PythonExact;
                self.native_lease = false;
            }
            ResponseEvent::Partial => {
                self.state = ResponseDisposition::Partial;
            }
            ResponseEvent::CleanEof => {
                self.decide(ResponseDecision::Reusable);
            }
            ResponseEvent::Close | ResponseEvent::Drop
                if self.state == ResponseDisposition::Open =>
            {
                self.decide(ResponseDecision::CloseDirty);
            }
            ResponseEvent::Close
            | ResponseEvent::Drop
            | ResponseEvent::ReadError
            | ResponseEvent::DecodeError
            | ResponseEvent::ProtocolError
            | ResponseEvent::Cancel
            | ResponseEvent::ActionDisconnect
            | ResponseEvent::ReplyDisconnect
            | ResponseEvent::PythonExact => {
                self.decide(ResponseDecision::CloseDirty);
            }
        }
        self.state
    }

    fn decide(&mut self, decision: ResponseDecision) {
        debug_assert!(self.decision.is_none());
        self.decision = Some(decision);
        self.decision_count += 1;
        self.state = match decision {
            ResponseDecision::Reusable => ResponseDisposition::Reusable,
            ResponseDecision::CloseDirty => ResponseDisposition::CloseDirty,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ResponseCache, ResponseContent, ResponseDecision, ResponseDisposition,
        ResponseDispositionState, ResponseEvent,
    };

    #[test]
    fn response_content_projection_is_independent_of_disposition() {
        assert_eq!(
            ResponseContent::from_python_fields(ResponseCache::FalseSentinel, false),
            ResponseContent::Streaming
        );
        assert_eq!(
            ResponseContent::from_python_fields(ResponseCache::FalseSentinel, true),
            ResponseContent::ExhaustedUncached
        );
        assert_eq!(
            ResponseContent::from_python_fields(ResponseCache::Cached, true),
            ResponseContent::Cached
        );
        assert_eq!(
            ResponseContent::from_python_fields(ResponseCache::EmptyNone, true),
            ResponseContent::EmptyNone
        );
    }

    #[test]
    fn response_disposition_is_monotonic_and_exactly_once() {
        let mut clean = ResponseDispositionState::default();
        assert_eq!(
            clean.apply(ResponseEvent::CleanEof),
            ResponseDisposition::Reusable
        );
        assert_eq!(
            clean.apply(ResponseEvent::Close),
            ResponseDisposition::Reusable
        );
        assert_eq!(
            clean.apply(ResponseEvent::Drop),
            ResponseDisposition::Reusable
        );
        assert_eq!(clean.decision(), Some(ResponseDecision::Reusable));
        assert_eq!(clean.decision_count(), 1);

        for event in [
            ResponseEvent::ReadError,
            ResponseEvent::DecodeError,
            ResponseEvent::ProtocolError,
            ResponseEvent::Cancel,
            ResponseEvent::ActionDisconnect,
            ResponseEvent::ReplyDisconnect,
        ] {
            let mut dirty = ResponseDispositionState::default();
            assert_eq!(dirty.apply(event), ResponseDisposition::CloseDirty);
            assert_eq!(
                dirty.apply(ResponseEvent::CleanEof),
                ResponseDisposition::CloseDirty
            );
            assert_eq!(dirty.decision(), Some(ResponseDecision::CloseDirty));
            assert_eq!(dirty.decision_count(), 1);
        }

        let mut partial = ResponseDispositionState::default();
        assert_eq!(
            partial.apply(ResponseEvent::Partial),
            ResponseDisposition::Partial
        );
        assert_eq!(partial.decision(), None);
        assert_eq!(
            partial.apply(ResponseEvent::Drop),
            ResponseDisposition::CloseDirty
        );
        assert_eq!(partial.decision_count(), 1);

        let mut python = ResponseDispositionState::default();
        assert_eq!(
            python.apply(ResponseEvent::PythonExact),
            ResponseDisposition::PythonExact
        );
        assert_eq!(
            python.apply(ResponseEvent::Drop),
            ResponseDisposition::PythonExact
        );
        assert_eq!(python.decision(), None);
        assert_eq!(python.decision_count(), 0);
        assert!(!python.native_lease());
    }
}

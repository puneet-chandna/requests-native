//! Python-free response state used by the compatibility binding.
//!
//! Content projection and connection disposition are intentionally separate.
//! A response can, for example, have an exhausted uncached body while the
//! disposition has already become dirty.

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;

use crate::transport::{ConnectionDriver, TransportResponse};
use crate::{Error, Result};

pub struct Response {
    head: http::response::Parts,
    body: Option<Incoming>,
    url: String,
    driver: Option<ConnectionDriver>,
    disposition: ResponseDispositionState,
}

impl Response {
    pub(crate) fn from_transport(response: TransportResponse) -> Self {
        Self {
            head: response.head,
            body: Some(response.body),
            url: response.url,
            driver: Some(response.driver),
            disposition: ResponseDispositionState::without_native_lease(),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.head.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.head.headers
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn bytes(mut self) -> Result<Bytes> {
        let Some(body) = self.body.take() else {
            self.disposition.apply(ResponseEvent::ReadError);
            return Err(Error::response_body("response body was already consumed"));
        };
        let Some(mut driver) = self.driver.take() else {
            self.disposition.apply(ResponseEvent::ReadError);
            return Err(Error::connection_stopped());
        };

        let collection = body.collect();
        tokio::pin!(collection);
        let mut driver_pending = true;
        let body_result = loop {
            tokio::select! {
                biased;
                driver_result = driver.task_mut(), if driver_pending => {
                    driver_pending = false;
                    match driver_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => break Err(error),
                        Err(error) => break Err(Error::connection(error)),
                    }
                }
                result = &mut collection => {
                    break result
                        .map(|collected| collected.to_bytes())
                        .map_err(Error::response_body);
                }
            }
        };

        if driver_pending {
            // Once Hyper has collected the complete framed body, this request
            // succeeded. The biased select above reports any driver failure
            // ready before body completion; abort-and-wait here only closes
            // the deliberately non-pooled connection.
            drop(driver.abort_and_wait().await);
        }
        let result = body_result;

        self.disposition.apply(if result.is_ok() {
            ResponseEvent::CleanEof
        } else {
            ResponseEvent::ReadError
        });
        result
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        drop(self.driver.take());
        self.disposition.apply(ResponseEvent::Drop);
    }
}

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
    pub(crate) const fn without_native_lease() -> Self {
        Self {
            state: ResponseDisposition::Open,
            decision: None,
            decision_count: 0,
            native_lease: false,
        }
    }

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

    #[test]
    fn unleased_native_response_never_claims_a_pool_lease() {
        let mut response = ResponseDispositionState::without_native_lease();

        assert!(!response.native_lease());
        assert_eq!(
            response.apply(ResponseEvent::CleanEof),
            ResponseDisposition::Reusable
        );
        assert!(!response.native_lease());
    }
}

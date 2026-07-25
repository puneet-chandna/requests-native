//! Python-free response state used by the compatibility binding.
//!
//! Content projection and connection disposition are intentionally separate.
//! A response can, for example, have an exhausted uncached body while the
//! disposition has already become dirty.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use http::header::TRANSFER_ENCODING;
use http::{HeaderMap, StatusCode};
use hyper::body::{Body, Incoming};

use crate::transport::{TransportLease, TransportResponse};
use crate::{Error, Result};

pub struct Response {
    head: http::response::Parts,
    body: Option<Incoming>,
    url: String,
    driver: Option<ResponseBodyDriver>,
    read_timeout: Option<Duration>,
    disposition: Option<ResponseDispositionState>,
}

impl Response {
    pub(crate) fn from_transport(response: TransportResponse) -> Self {
        Self {
            head: response.head,
            body: Some(response.body),
            url: response.url,
            driver: Some(ResponseBodyDriver::Network(response.lease)),
            read_timeout: response.read_timeout,
            disposition: Some(ResponseDispositionState::default()),
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

    pub fn into_body(mut self) -> ResponseBody {
        let chunked = has_chunked_transfer_encoding(&self.head.headers);
        ResponseBody {
            source: self
                .body
                .take()
                .map(|body| ResponseBodySource::Incoming(Box::pin(body))),
            driver: self.driver.take(),
            read_timeout: self.read_timeout.take(),
            read_deadline: None,
            chunked,
            terminal: false,
            disposition: self
                .disposition
                .take()
                .expect("response disposition transfers exactly once"),
            #[cfg(test)]
            probe: None,
        }
    }

    pub async fn bytes(self) -> Result<Bytes> {
        let mut body = self.into_body();
        let mut collected = BytesMut::new();
        while let Some(chunk) = poll_fn(|context| Pin::new(&mut body).poll_next(context)).await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(collected.freeze())
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        if let Some(disposition) = &mut self.disposition {
            disposition.apply(ResponseEvent::Drop);
        }
        if let Some(driver) = self.driver.take() {
            driver.finish_now(false);
        }
    }
}

fn has_chunked_transfer_encoding(headers: &HeaderMap) -> bool {
    headers
        .get_all(TRANSFER_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
}

enum ResponseBodySource {
    Incoming(Pin<Box<Incoming>>),
    #[cfg(test)]
    Pending,
}

enum ResponseBodyDriver {
    Network(TransportLease),
    #[cfg(test)]
    Controlled(ControlledDriver),
}

impl ResponseBodyDriver {
    fn poll_result(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        match self {
            Self::Network(lease) => lease.poll_result(context),
            #[cfg(test)]
            Self::Controlled(driver) => driver.poll(context),
        }
    }

    async fn abort_and_wait(self) -> Result<()> {
        match self {
            Self::Network(driver) => driver.abort_and_wait().await,
            #[cfg(test)]
            Self::Controlled(_) => Ok(()),
        }
    }

    fn finish_now(self, reusable: bool) {
        match self {
            Self::Network(lease) => lease.finish_now(reusable),
            #[cfg(test)]
            Self::Controlled(_) => {}
        }
    }
}

pub struct ResponseBody {
    source: Option<ResponseBodySource>,
    driver: Option<ResponseBodyDriver>,
    read_timeout: Option<Duration>,
    read_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    chunked: bool,
    terminal: bool,
    disposition: ResponseDispositionState,
    #[cfg(test)]
    probe: Option<TestResponseBodyProbe>,
}

impl ResponseBody {
    pub async fn close(mut self) -> Result<()> {
        let Some(driver) = self.begin_terminal(ResponseEvent::Close) else {
            return Ok(());
        };
        driver.abort_and_wait().await
    }

    fn begin_terminal(&mut self, event: ResponseEvent) -> Option<ResponseBodyDriver> {
        if self.terminal {
            return None;
        }
        self.terminal = true;
        drop(self.source.take());
        drop(self.read_deadline.take());
        self.disposition.apply(event);
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe.record_terminal(
                self.disposition.state(),
                usize::from(self.disposition.decision_count()),
            );
        }
        self.driver.take()
    }

    fn finish_now(&mut self, event: ResponseEvent) {
        if let Some(driver) = self.begin_terminal(event) {
            driver.finish_now(self.disposition.decision() == Some(ResponseDecision::Reusable));
        }
    }

    fn finish_error(&mut self, event: ResponseEvent, error: Error) -> Poll<Option<Result<Bytes>>> {
        self.finish_now(event);
        Poll::Ready(Some(Err(error)))
    }

    fn poll_source(&mut self, context: &mut Context<'_>) -> Poll<Option<Result<Bytes>>> {
        loop {
            let result = match self.source.as_mut() {
                Some(ResponseBodySource::Incoming(body)) => body.as_mut().poll_frame(context),
                #[cfg(test)]
                Some(ResponseBodySource::Pending) => return Poll::Pending,
                None => {
                    return Poll::Ready(Some(Err(Error::response_body(
                        "response body was already consumed",
                    ))));
                }
            };
            match result {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(bytes) => return Poll::Ready(Some(Ok(bytes))),
                    Err(_) => continue,
                },
                Poll::Ready(Some(Err(error))) => {
                    let error = if self.chunked {
                        Error::chunked_encoding(error)
                    } else {
                        Error::response_body(error)
                    };
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    #[cfg(test)]
    fn test_pending_body_and_driver() -> (Self, TestDriverFailure, TestResponseBodyProbe) {
        let shared = Arc::new(ControlledDriverShared {
            failure: Mutex::new(None),
            waker: Mutex::new(None),
        });
        let probe = TestResponseBodyProbe {
            inner: Arc::new(TestResponseBodyProbeInner {
                disposition: Mutex::new(ResponseDisposition::Open),
                decision_count: AtomicUsize::new(0),
                cleanup_count: AtomicUsize::new(0),
            }),
        };
        (
            Self {
                source: Some(ResponseBodySource::Pending),
                driver: Some(ResponseBodyDriver::Controlled(ControlledDriver {
                    shared: Arc::clone(&shared),
                })),
                read_timeout: None,
                read_deadline: None,
                chunked: false,
                terminal: false,
                disposition: ResponseDispositionState::without_native_lease(),
                probe: Some(probe.clone()),
            },
            TestDriverFailure { shared },
            probe,
        )
    }
}

impl Stream for ResponseBody {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }

        match self.poll_source(context) {
            Poll::Ready(Some(Ok(bytes))) => {
                drop(self.read_deadline.take());
                self.disposition.apply(ResponseEvent::Partial);
                return Poll::Ready(Some(Ok(bytes)));
            }
            Poll::Ready(Some(Err(error))) => {
                let event = if error.kind() == crate::ErrorKind::ChunkedEncoding {
                    ResponseEvent::ProtocolError
                } else {
                    ResponseEvent::ReadError
                };
                return self.finish_error(event, error);
            }
            Poll::Ready(None) => {
                self.finish_now(ResponseEvent::CleanEof);
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }

        if self.read_deadline.is_none()
            && let Some(timeout) = self.read_timeout
        {
            self.read_deadline = Some(Box::pin(tokio::time::sleep(timeout)));
        }

        if let Some(driver) = self.driver.as_mut() {
            match driver.poll_result(context) {
                Poll::Ready(Err(error)) => {
                    return self.finish_error(ResponseEvent::ReadError, error);
                }
                Poll::Ready(Ok(())) => {
                    context.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Pending => {}
            }
        }

        if let Some(deadline) = self.read_deadline.as_mut()
            && deadline.as_mut().poll(context).is_ready()
        {
            let timeout = self
                .read_timeout
                .expect("read deadline exists only when read timeout is configured");
            return self.finish_error(ResponseEvent::ReadError, Error::read_timeout(timeout));
        }
        Poll::Pending
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        self.finish_now(ResponseEvent::Drop);
    }
}

#[cfg(test)]
struct ControlledDriverShared {
    failure: Mutex<Option<String>>,
    waker: Mutex<Option<std::task::Waker>>,
}

#[cfg(test)]
struct ControlledDriver {
    shared: Arc<ControlledDriverShared>,
}

#[cfg(test)]
impl ControlledDriver {
    fn poll(&self, context: &mut Context<'_>) -> Poll<Result<()>> {
        if let Some(message) = self.shared.failure.lock().expect("failure lock").take() {
            Poll::Ready(Err(Error::connection(message)))
        } else {
            *self.shared.waker.lock().expect("waker lock") = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
struct TestDriverFailure {
    shared: Arc<ControlledDriverShared>,
}

#[cfg(test)]
impl TestDriverFailure {
    fn fail(self, message: &str) {
        *self.shared.failure.lock().expect("failure lock") = Some(message.to_owned());
        if let Some(waker) = self.shared.waker.lock().expect("waker lock").take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestResponseBodyProbe {
    inner: Arc<TestResponseBodyProbeInner>,
}

#[cfg(test)]
struct TestResponseBodyProbeInner {
    disposition: Mutex<ResponseDisposition>,
    decision_count: AtomicUsize,
    cleanup_count: AtomicUsize,
}

#[cfg(test)]
impl TestResponseBodyProbe {
    fn record_terminal(&self, disposition: ResponseDisposition, decision_count: usize) {
        self.inner
            .decision_count
            .store(decision_count, Ordering::Relaxed);
        self.inner.cleanup_count.fetch_add(1, Ordering::Relaxed);
        *self.inner.disposition.lock().expect("disposition lock") = disposition;
    }

    fn disposition(&self) -> ResponseDisposition {
        *self.inner.disposition.lock().expect("disposition lock")
    }

    fn decision_count(&self) -> usize {
        self.inner.decision_count.load(Ordering::Relaxed)
    }

    fn cleanup_count(&self) -> usize {
        self.inner.cleanup_count.load(Ordering::Relaxed)
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
    #[cfg(test)]
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
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::sync::mpsc::{self, Receiver, TryRecvError};
    use std::time::Duration;

    use futures_core::Stream;

    use super::{
        ResponseBody, ResponseCache, ResponseContent, ResponseDecision, ResponseDisposition,
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

    #[test]
    fn response_stream_events_make_exactly_one_terminal_decision() {
        let mut clean = ResponseDispositionState::without_native_lease();
        assert_eq!(
            clean.apply(ResponseEvent::Partial),
            ResponseDisposition::Partial
        );
        assert_eq!(clean.decision_count(), 0);
        assert_eq!(
            clean.apply(ResponseEvent::CleanEof),
            ResponseDisposition::Reusable
        );
        assert_eq!(clean.decision(), Some(ResponseDecision::Reusable));
        assert_eq!(clean.decision_count(), 1);
        assert_eq!(
            clean.apply(ResponseEvent::Drop),
            ResponseDisposition::Reusable
        );
        assert_eq!(clean.decision_count(), 1);
        assert!(!clean.native_lease());

        for partial in [false, true] {
            for event in [
                ResponseEvent::Close,
                ResponseEvent::Drop,
                ResponseEvent::ReadError,
                ResponseEvent::ProtocolError,
                ResponseEvent::Cancel,
            ] {
                let mut dirty = ResponseDispositionState::without_native_lease();
                if partial {
                    assert_eq!(
                        dirty.apply(ResponseEvent::Partial),
                        ResponseDisposition::Partial
                    );
                } else {
                    assert_eq!(dirty.state(), ResponseDisposition::Open);
                }
                assert_eq!(dirty.decision_count(), 0);
                assert_eq!(dirty.apply(event), ResponseDisposition::CloseDirty);
                assert_eq!(dirty.decision(), Some(ResponseDecision::CloseDirty));
                assert_eq!(dirty.decision_count(), 1);
                assert_eq!(
                    dirty.apply(ResponseEvent::CleanEof),
                    ResponseDisposition::CloseDirty
                );
                assert_eq!(dirty.decision_count(), 1);
                assert!(!dirty.native_lease());
            }
        }
    }

    #[test]
    fn connection_driver_failure_maps_to_connection_error_and_one_dirty_read_decision() {
        let error = crate::Error::connection("deterministic test driver failure");
        assert_eq!(error.kind(), crate::ErrorKind::Connection);

        let mut response = ResponseDispositionState::without_native_lease();
        assert_eq!(
            response.apply(ResponseEvent::Partial),
            ResponseDisposition::Partial
        );
        assert_eq!(
            response.apply(ResponseEvent::ReadError),
            ResponseDisposition::CloseDirty
        );
        assert_eq!(response.decision(), Some(ResponseDecision::CloseDirty));
        assert_eq!(response.decision_count(), 1);
        assert_eq!(
            response.apply(ResponseEvent::ReadError),
            ResponseDisposition::CloseDirty
        );
        assert_eq!(response.decision_count(), 1);
    }

    async fn wait_for_pending_poll(pending: &Receiver<()>) {
        loop {
            match pending.try_recv() {
                Ok(()) => return,
                Err(TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(TryRecvError::Disconnected) => {
                    panic!("response body task ended before reporting Pending")
                }
            }
        }
    }

    #[test]
    fn pending_response_body_driver_failure_is_typed_terminal_and_cleaned_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build deterministic body-driver runtime");
        let (body, driver_failure, probe) = ResponseBody::test_pending_body_and_driver();
        let (pending_tx, pending_rx) = mpsc::channel();

        let (mut body, item) = runtime.block_on(async {
            let task = tokio::spawn(async move {
                let mut body = body;
                let mut pending_tx = Some(pending_tx);
                let item = poll_fn(|context| {
                    let result = Pin::new(&mut body).poll_next(context);
                    if result.is_pending()
                        && let Some(pending_tx) = pending_tx.take()
                    {
                        pending_tx
                            .send(())
                            .expect("report pending response body poll");
                    }
                    result
                })
                .await;
                (body, item)
            });

            tokio::time::timeout(Duration::from_secs(1), wait_for_pending_poll(&pending_rx))
                .await
                .expect("response body did not remain pending before driver failure");
            driver_failure.fail("deterministic test driver failure");
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("driver failure did not wake response body")
                .expect("response body task failed")
        });

        let error = item
            .expect("driver failure did not yield an error")
            .expect_err("driver failure unexpectedly yielded bytes");
        assert_eq!(error.kind(), crate::ErrorKind::Connection);
        assert_eq!(
            error.to_string(),
            "HTTP/1.1 connection driver failed: deterministic test driver failure"
        );
        assert!(
            runtime
                .block_on(poll_fn(|context| Pin::new(&mut body).poll_next(context)))
                .is_none(),
            "driver failure must be terminal"
        );
        assert_eq!(probe.disposition(), ResponseDisposition::CloseDirty);
        assert_eq!(probe.decision_count(), 1);
        assert_eq!(probe.cleanup_count(), 1);
        drop(body);
        assert_eq!(probe.decision_count(), 1);
        assert_eq!(probe.cleanup_count(), 1);
    }
}

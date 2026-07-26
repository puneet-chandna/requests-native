use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Method;

use super::{Connector, DeadlineSource, Transport, select_deadline_source};
use crate::{AsyncBody, BodySource, Error, ErrorKind, RequestBuilder, Timeout};

const SHORT_DEADLINE: Duration = Duration::from_millis(200);
const LONG_DEADLINE: Duration = Duration::from_millis(800);
const SAFETY_RELEASE: Duration = Duration::from_millis(500);
const OUTER_BOUND: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct ControlledConnector {
    shared: Arc<ControlledConnectorShared>,
}

struct ControlledConnectorShared {
    started: AtomicUsize,
    dropped: AtomicUsize,
    released: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

struct ControlledConnectFuture {
    shared: Arc<ControlledConnectorShared>,
    reported_started: bool,
}

impl ControlledConnector {
    fn new() -> Self {
        Self {
            shared: Arc::new(ControlledConnectorShared {
                started: AtomicUsize::new(0),
                dropped: AtomicUsize::new(0),
                released: AtomicBool::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    async fn wait_started(&self) {
        tokio::time::timeout(OUTER_BOUND, async {
            while self.shared.started.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controlled connector was never polled");
        assert_eq!(self.shared.started.load(Ordering::Acquire), 1);
    }

    fn release(&self) {
        self.shared.released.store(true, Ordering::Release);
        if let Some(waker) = self
            .shared
            .waker
            .lock()
            .expect("connector waker lock")
            .take()
        {
            waker.wake();
        }
    }

    async fn assert_dropped_once(&self) {
        tokio::time::timeout(OUTER_BOUND, async {
            while self.shared.dropped.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controlled connector future was not dropped");
        assert_eq!(self.shared.dropped.load(Ordering::Acquire), 1);
    }
}

impl Connector for ControlledConnector {
    fn connect(
        &self,
        _host: &str,
        _port: u16,
        _target: &str,
    ) -> Pin<Box<dyn Future<Output = crate::Result<tokio::net::TcpStream>> + Send>> {
        Box::pin(ControlledConnectFuture {
            shared: Arc::clone(&self.shared),
            reported_started: false,
        })
    }
}

impl Future for ControlledConnectFuture {
    type Output = crate::Result<tokio::net::TcpStream>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.reported_started {
            self.reported_started = true;
            self.shared.started.fetch_add(1, Ordering::AcqRel);
        }
        if self.shared.released.load(Ordering::Acquire) {
            return Poll::Ready(Err(Error::connect(
                "connector.test:80",
                "controlled connector released",
            )));
        }
        *self.shared.waker.lock().expect("connector waker lock") = Some(context.waker().clone());
        if self.shared.released.load(Ordering::Acquire) {
            context.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl Drop for ControlledConnectFuture {
    fn drop(&mut self) {
        self.shared.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

struct PollProbeBody {
    polls: Arc<AtomicUsize>,
}

impl AsyncBody for PollProbeBody {
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<crate::Result<Bytes>>> {
        self.polls.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(None)
    }

    fn size_hint(&self) -> Option<u64> {
        None
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build timeout contract runtime")
}

fn request(timeout: Timeout, body: BodySource) -> crate::Request {
    RequestBuilder::new(Method::POST, "http://connector.test/pending")
        .timeout(timeout)
        .body(body)
        .build()
        .expect("build connector contract request")
}

async fn send_with_safety_release(timeout: Timeout) -> Error {
    let connector = ControlledConnector::new();
    let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
    let request = request(timeout, BodySource::Empty);
    let task_transport = Arc::clone(&transport);
    let mut send = tokio::spawn(async move { task_transport.send(request).await });

    connector.wait_started().await;
    let release_connector = connector.clone();
    let mut release = tokio::spawn(async move {
        tokio::time::sleep(SAFETY_RELEASE).await;
        release_connector.release();
    });
    let result = tokio::time::timeout(OUTER_BOUND, &mut send).await;
    release.abort();
    let _ = (&mut release).await;
    let result = match result {
        Ok(result) => result.expect("controlled send task failed"),
        Err(error) => {
            send.abort();
            let _ = send.await;
            panic!("controlled send exceeded outer bound: {error}");
        }
    };
    connector.assert_dropped_once().await;
    match result {
        Err(error) => error,
        Ok(_) => panic!("pending controlled connector unexpectedly produced a response"),
    }
}

fn assert_connect_timeout(error: &Error, source: &str) {
    assert_eq!(error.kind(), ErrorKind::ConnectTimeout);
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("connect"),
        "connect timeout must identify its phase: {message}"
    );
    match source {
        "connect" => {
            assert!(
                message.contains("connect timeout") && !message.contains("total timeout"),
                "connect-specific timeout context missing: {message}"
            );
        }
        "total" => {
            assert!(
                message.contains("total timeout"),
                "total timeout context missing during connect: {message}"
            );
        }
        _ => panic!("unsupported connect timeout source: {source}"),
    }
}

fn assert_equal_read_total_tie_prefers_read(_phase: &str) {
    let deadline = Instant::now();
    assert_eq!(
        select_deadline_source(Some(deadline), Some(deadline)),
        Some(DeadlineSource::Read)
    );
}

#[test]
fn equal_absolute_response_head_deadlines_prefer_read_source() {
    assert_equal_read_total_tie_prefers_read("response head");
}

#[test]
fn equal_absolute_response_body_deadlines_prefer_read_source() {
    assert_equal_read_total_tie_prefers_read("response body");
}

#[test]
fn connect_component_expiry_is_connect_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: None,
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn total_expiry_during_connect_is_connect_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: None,
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "total");
}

#[test]
fn shorter_connect_deadline_precedes_total_during_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: Some(LONG_DEADLINE),
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn shorter_total_deadline_precedes_connect_during_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(LONG_DEADLINE),
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "total");
}

#[test]
fn equal_connect_and_total_deadlines_prefer_connect_specific_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn read_timeout_does_not_govern_pending_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: None,
        read: Some(SHORT_DEADLINE),
        total: None,
    }));

    assert_eq!(error.kind(), ErrorKind::Connect);
    assert!(error.to_string().contains("controlled connector released"));
}

#[test]
fn both_none_release_preserves_original_connect_error() {
    runtime().block_on(async {
        let connector = ControlledConnector::new();
        let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
        let task_transport = Arc::clone(&transport);
        let mut send = tokio::spawn(async move {
            task_transport
                .send(request(Timeout::default(), BodySource::Empty))
                .await
        });

        connector.wait_started().await;
        connector.release();
        let result = tokio::time::timeout(OUTER_BOUND, &mut send)
            .await
            .expect("released connector send exceeded outer bound")
            .expect("released connector send task failed");
        connector.assert_dropped_once().await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("released controlled connector unexpectedly produced a response"),
        };
        assert_eq!(error.kind(), ErrorKind::Connect);
        assert!(error.to_string().contains("controlled connector released"));
    });
}

#[test]
fn request_body_is_not_polled_while_connect_is_pending() {
    runtime().block_on(async {
        let connector = ControlledConnector::new();
        let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
        let polls = Arc::new(AtomicUsize::new(0));
        let body = BodySource::Stream(Box::pin(PollProbeBody {
            polls: Arc::clone(&polls),
        }));
        let task_transport = Arc::clone(&transport);
        let mut send =
            tokio::spawn(
                async move { task_transport.send(request(Timeout::default(), body)).await },
            );

        connector.wait_started().await;
        assert_eq!(polls.load(Ordering::Acquire), 0);
        connector.release();
        let result = tokio::time::timeout(OUTER_BOUND, &mut send)
            .await
            .expect("body pre-poll control exceeded outer bound")
            .expect("body pre-poll send task failed");
        connector.assert_dropped_once().await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("released controlled connector unexpectedly produced a response"),
        };
        assert_eq!(error.kind(), ErrorKind::Connect);
        assert_eq!(polls.load(Ordering::Acquire), 0);
    });
}

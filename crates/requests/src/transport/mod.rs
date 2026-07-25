mod connect;
mod pool;
#[cfg(test)]
mod pool_tests;

use std::fmt;
use std::future::Future;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, HOST};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::task::JoinHandle;

use self::pool::{ConnectionLease, IdleConnection, LeaseTerminal, Pool, PoolKey, TlsPoolKey};
use crate::models::RequestParts;
use crate::{BodySource, Error, Request, Result};

const MAX_IDLE_PER_KEY: usize = 10;

pub(crate) struct Transport {
    pool: Arc<Mutex<Pool>>,
}

impl fmt::Debug for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Transport")
    }
}

pub(crate) struct TransportResponse {
    pub head: http::response::Parts,
    pub body: Incoming,
    pub url: String,
    pub lease: TransportLease,
    pub read_timeout: Option<std::time::Duration>,
}

pub(crate) struct TransportLease {
    pool: Arc<Mutex<Pool>>,
    lease: Option<ConnectionLease>,
}

pub(crate) struct ConnectionDriver {
    task: Option<JoinHandle<Result<()>>>,
    shutdown: Option<std::net::TcpStream>,
}

impl ConnectionDriver {
    fn spawn(
        task: impl Future<Output = Result<()>> + Send + 'static,
        shutdown: Option<std::net::TcpStream>,
    ) -> Self {
        Self {
            task: Some(tokio::spawn(task)),
            shutdown,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.task.is_some()
    }

    pub(crate) fn is_reusable(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub(crate) fn peer_is_open(&self) -> bool {
        let Some(stream) = &self.shutdown else {
            return false;
        };
        let mut byte = [0_u8; 1];
        match stream.peek(&mut byte) {
            Ok(0) => false,
            Ok(_) => true,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                true
            }
            Err(_) => false,
        }
    }

    pub(crate) fn task_mut(&mut self) -> Option<&mut JoinHandle<Result<()>>> {
        self.task.as_mut()
    }

    pub(crate) fn finish(
        &mut self,
        result: std::result::Result<Result<()>, tokio::task::JoinError>,
    ) -> Result<()> {
        self.task.take();
        self.shutdown.take();
        match result {
            Ok(result) => result,
            Err(error) => Err(Error::connection(error)),
        }
    }

    pub(crate) async fn abort_and_wait(&mut self) -> Result<()> {
        self.shutdown_socket_once();
        let Some(task) = self.take_and_abort_task_once() else {
            return Ok(());
        };
        let result = task.await;
        match result {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(Error::connection(error)),
        }
    }

    pub(crate) fn shutdown_now(&mut self) {
        self.shutdown_socket_once();
        drop(self.take_and_abort_task_once());
    }

    fn shutdown_socket_once(&mut self) {
        if let Some(stream) = self.shutdown.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn take_and_abort_task_once(&mut self) -> Option<JoinHandle<Result<()>>> {
        if let Some(task) = self.task.take() {
            task.abort();
            Some(task)
        } else {
            None
        }
    }
}

impl Drop for ConnectionDriver {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}

impl TransportLease {
    fn new(pool: Arc<Mutex<Pool>>, lease: ConnectionLease) -> Self {
        Self {
            pool,
            lease: Some(lease),
        }
    }

    pub(crate) fn poll_result(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        let driver = self.driver_mut();
        let Some(task) = driver.task_mut() else {
            return Poll::Pending;
        };
        match Pin::new(task).poll(context) {
            Poll::Ready(result) => Poll::Ready(driver.finish(result)),
            Poll::Pending => Poll::Pending,
        }
    }

    pub(crate) async fn abort_and_wait(mut self) -> Result<()> {
        let result = self.driver_mut().abort_and_wait().await;
        self.release(false);
        result
    }

    pub(crate) fn finish_now(mut self, reusable: bool) {
        if !reusable {
            self.driver_mut().shutdown_now();
        }
        self.release(reusable);
    }

    fn driver_mut(&mut self) -> &mut ConnectionDriver {
        let (_, driver) = self
            .lease
            .as_mut()
            .expect("transport lease owns one connection")
            .connection_mut()
            .network_parts_mut();
        driver
    }

    fn release(&mut self, reusable: bool) {
        let Some(mut lease) = self.lease.take() else {
            return;
        };
        let reusable = reusable && lease.is_live() && lease.peer_is_open();
        lease = lease.complete(if reusable {
            LeaseTerminal::CleanEof
        } else {
            LeaseTerminal::Dirty
        });
        let rejected = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .release(lease)
        };
        drop(rejected);
    }
}

impl Drop for TransportLease {
    fn drop(&mut self) {
        let Some(mut lease) = self.lease.take() else {
            return;
        };
        let (_, driver) = lease.connection_mut().network_parts_mut();
        driver.shutdown_now();
        let lease = lease.complete(LeaseTerminal::Dirty);
        let rejected = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .release(lease)
        };
        drop(rejected);
    }
}

impl Transport {
    pub(crate) fn new() -> Self {
        Self {
            pool: Arc::new(Mutex::new(Pool::new(MAX_IDLE_PER_KEY))),
        }
    }

    #[allow(dead_code)]
    fn clear_pool(&self) {
        let evicted = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .clear()
        };
        drop(evicted);
    }

    pub async fn send(&self, request: Request) -> Result<TransportResponse> {
        validate_request(&request)?;
        let host = request
            .uri()
            .host()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .to_owned();
        let port = request.uri().port_u16().unwrap_or(80);
        let scheme = request
            .uri()
            .scheme()
            .cloned()
            .ok_or_else(|| Error::invalid_url(request.url()))?;
        let authority = request
            .uri()
            .authority()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .clone();
        let target = authority.as_str().to_owned();
        let key = PoolKey::new(scheme, authority, None, TlsPoolKey::plain(), None);
        let request = request.into_parts();
        let read_timeout = request.timeout.read;
        let (outgoing, url) = outgoing_request(request)?;

        let mut lease = self.acquire_connection(key, &host, port, &target).await?;
        let response = {
            let (sender, driver) = lease.connection_mut().network_parts_mut();
            let sending = sender.send_request(outgoing);
            tokio::pin!(sending);
            loop {
                if !driver.is_running() {
                    break sending.as_mut().await.map_err(Error::send);
                }

                let Some(driver_task) = driver.task_mut() else {
                    continue;
                };
                tokio::select! {
                    biased;
                    result = &mut sending => break result.map_err(Error::send),
                    driver_result = driver_task => {
                        match driver.finish(driver_result) {
                            Ok(()) => continue,
                            Err(error) => break Err(error),
                        }
                    }
                }
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(send_error) => {
                let cleanup = TransportLease::new(Arc::clone(&self.pool), lease)
                    .abort_and_wait()
                    .await;
                return match cleanup {
                    Ok(()) => Err(send_error),
                    Err(driver_error) => Err(Error::send_with_cleanup(send_error, driver_error)),
                };
            }
        };
        let (head, body) = response.into_parts();

        Ok(TransportResponse {
            head,
            body,
            url,
            lease: TransportLease::new(Arc::clone(&self.pool), lease),
            read_timeout,
        })
    }

    async fn acquire_connection(
        &self,
        key: PoolKey,
        host: &str,
        port: u16,
        target: &str,
    ) -> Result<ConnectionLease> {
        loop {
            let candidate = {
                self.pool
                    .lock()
                    .expect("transport pool lock poisoned")
                    .acquire(&key)
            };
            let Some(mut lease) = candidate else {
                break;
            };
            if !lease.is_live() || !lease.peer_is_open() {
                TransportLease::new(Arc::clone(&self.pool), lease).finish_now(false);
                continue;
            }
            let ready = {
                let (sender, _) = lease.connection_mut().network_parts_mut();
                sender.ready().await
            };
            if ready.is_ok() && lease.is_live() && lease.peer_is_open() {
                return Ok(lease);
            }
            TransportLease::new(Arc::clone(&self.pool), lease).finish_now(false);
        }

        self.connect_connection(key, host, port, target).await
    }

    async fn connect_connection(
        &self,
        key: PoolKey,
        host: &str,
        port: u16,
        target: &str,
    ) -> Result<ConnectionLease> {
        let generation = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .generation_number(&key)
        };
        let stream = connect::connect(host, port, target).await?;
        let stream = stream
            .into_std()
            .map_err(|error| Error::connect(target, error))?;
        let shutdown = stream
            .try_clone()
            .map_err(|error| Error::connect(target, error))?;
        let stream = tokio::net::TcpStream::from_std(stream)
            .map_err(|error| Error::connect(target, error))?;
        let (sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(Error::handshake)?;
        let driver = ConnectionDriver::spawn(
            async move { connection.await.map_err(Error::connection) },
            Some(shutdown),
        );
        Ok(ConnectionLease::new(
            key,
            generation,
            IdleConnection::network(sender, driver),
        ))
    }
}

fn validate_request(request: &Request) -> Result<()> {
    if request.uri().scheme_str() != Some("http") {
        return Err(Error::unsupported_scheme(
            request.url(),
            request.uri().scheme_str(),
        ));
    }
    validate_content_lengths(request.headers())?;
    Ok(())
}

fn validate_content_lengths(headers: &http::HeaderMap) -> Result<()> {
    let mut first = None;
    for value in headers
        .get_all(CONTENT_LENGTH)
        .iter()
        .filter_map(parsed_content_length)
    {
        if first.is_some_and(|first| value != first) {
            return Err(Error::conflicting_content_length());
        }
        first = Some(value);
    }
    Ok(())
}

fn parsed_content_length(value: &http::HeaderValue) -> Option<u64> {
    let value = value.to_str().ok()?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn outgoing_request(mut request: RequestParts) -> Result<(http::Request<OutgoingBody>, String)> {
    let origin = request
        .uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str)
        .parse::<http::Uri>()
        .map_err(|_| Error::invalid_url(&request.url))?;
    if !request.headers.contains_key(HOST) {
        let authority = request
            .uri
            .authority()
            .ok_or_else(|| Error::invalid_url(&request.url))?;
        let host = authority
            .as_str()
            .parse()
            .map_err(|_| Error::invalid_url(&request.url))?;
        request.headers.insert(HOST, host);
    }

    let mut outgoing = http::Request::new(OutgoingBody::new(request.body));
    *outgoing.method_mut() = request.method;
    *outgoing.uri_mut() = origin;
    *outgoing.headers_mut() = request.headers;
    Ok((outgoing, request.url))
}

struct OutgoingBody {
    source: BodySource,
}

impl OutgoingBody {
    fn new(source: BodySource) -> Self {
        Self { source }
    }
}

impl Body for OutgoingBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>>>> {
        let body = self.get_mut();
        match std::mem::take(&mut body.source) {
            BodySource::Empty => Poll::Ready(None),
            BodySource::Bytes(bytes) if bytes.is_empty() => Poll::Ready(None),
            BodySource::Bytes(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            BodySource::Stream(mut stream) => match stream.as_mut().poll_next(context) {
                Poll::Pending => {
                    body.source = BodySource::Stream(stream);
                    Poll::Pending
                }
                Poll::Ready(Some(chunk)) => {
                    body.source = BodySource::Stream(stream);
                    Poll::Ready(Some(chunk.map(Frame::data)))
                }
                Poll::Ready(None) => Poll::Ready(None),
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.source, BodySource::Empty)
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        match &self.source {
            BodySource::Empty => hint.set_exact(0),
            BodySource::Bytes(bytes) => hint.set_exact(bytes.len() as u64),
            BodySource::Stream(stream) => {
                if let Some(length) = stream.size_hint() {
                    hint.set_exact(length);
                }
            }
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use http::{HeaderName, HeaderValue, Method};

    use super::{ConnectionDriver, outgoing_request, validate_request};
    use crate::{AsyncBody, BodySource, ErrorKind, RequestBuilder};

    struct NeverBody;

    impl AsyncBody for NeverBody {
        fn poll_next(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<crate::Result<Bytes>>> {
            Poll::Ready(None)
        }

        fn size_hint(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn request_validation_allows_bodies_and_rejects_https_before_io() {
        let bytes = RequestBuilder::new(Method::GET, "http://example.test/")
            .body(Bytes::from_static(b"body"))
            .build()
            .unwrap();
        let stream = RequestBuilder::new(Method::GET, "http://example.test/")
            .body(BodySource::Stream(Box::pin(NeverBody)))
            .build()
            .unwrap();
        let https = RequestBuilder::new(Method::GET, "https://example.test/")
            .build()
            .unwrap();

        validate_request(&bytes).unwrap();
        validate_request(&stream).unwrap();
        assert_eq!(
            validate_request(&https).unwrap_err().kind(),
            ErrorKind::InvalidUrl
        );
    }

    #[test]
    fn request_validation_allows_equal_content_lengths() {
        let request = RequestBuilder::new(Method::POST, "http://example.test/")
            .header(
                HeaderName::from_static("content-length"),
                HeaderValue::from_static("3"),
            )
            .header(
                HeaderName::from_static("content-length"),
                HeaderValue::from_static("03"),
            )
            .body(Bytes::from_static(b"abc"))
            .build()
            .unwrap();

        validate_request(&request).unwrap();
    }

    #[test]
    fn outgoing_get_uses_origin_form_and_preserves_explicit_host() {
        let request =
            RequestBuilder::new(Method::GET, "http://example.test:8080/direct?source=unit")
                .header(
                    HeaderName::from_static("host"),
                    HeaderValue::from_static("example.test:8080"),
                )
                .build()
                .unwrap();

        let (outgoing, url) = outgoing_request(request.into_parts()).unwrap();

        assert_eq!(outgoing.uri().to_string(), "/direct?source=unit");
        assert_eq!(outgoing.headers().len(), 1);
        assert_eq!(
            outgoing.headers().get("host"),
            Some(&HeaderValue::from_static("example.test:8080"))
        );
        assert_eq!(url, "http://example.test:8080/direct?source=unit");
    }

    #[test]
    fn dropping_connection_driver_aborts_its_task() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dropped = Arc::new(AtomicBool::new(false));
            let task_dropped = Arc::clone(&dropped);
            let driver = ConnectionDriver::spawn(
                async move {
                    let _drop_flag = DropFlag(task_dropped);
                    future::pending::<()>().await;
                    Ok(())
                },
                None,
            );
            tokio::task::yield_now().await;

            drop(driver);
            for _ in 0..10 {
                if dropped.load(Ordering::Acquire) {
                    break;
                }
                tokio::task::yield_now().await;
            }

            assert!(dropped.load(Ordering::Acquire));
        });
    }

    #[test]
    fn finished_connection_driver_is_not_reusable() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let driver = ConnectionDriver::spawn(async { Ok(()) }, None);
            tokio::time::timeout(Duration::from_secs(1), async {
                while !driver
                    .task
                    .as_ref()
                    .expect("driver task exists")
                    .is_finished()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("connection driver did not finish");

            assert!(driver.is_running());
            assert!(!driver.is_reusable());
        });
    }
}

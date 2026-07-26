mod connect;
pub(crate) mod decode;
#[cfg(test)]
mod establishment_tests;
mod pool;
#[cfg(test)]
mod pool_tests;
#[cfg(test)]
mod timeout_tests;

use std::fmt;
use std::future::Future;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::HeaderValue;
use http::header::{ACCEPT_ENCODING, CONTENT_LENGTH, HOST};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::task::JoinHandle;

use self::pool::{ConnectionLease, IdleConnection, LeaseTerminal, Pool, PoolKey, TlsPoolKey};
use crate::models::RequestParts;
use crate::{BodySource, ContentCodecs, Error, Proxy, Request, Result, Timeout, TlsConfig};

pub(crate) const DEFAULT_MAX_IDLE_PER_HOST: usize = 10;

pub(crate) struct Transport {
    pool: Arc<Mutex<Pool>>,
    #[cfg(test)]
    derived_pool_keys: Mutex<Vec<PoolKey>>,
    #[cfg(test)]
    establishment_control: Option<Arc<dyn EstablishmentControl>>,
    connector: Arc<dyn Connector>,
    proxy: Option<Proxy>,
    #[allow(dead_code)]
    tls: TlsConfig,
    default_timeout: Timeout,
    content_codecs: ContentCodecs,
}

pub(super) trait Connector: Send + Sync {
    fn connect(
        &self,
        host: &str,
        port: u16,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = Result<tokio::net::TcpStream>> + Send>>;
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EstablishmentStage {
    Connect,
    Tls,
    Http1,
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) trait EstablishmentControl: Send + Sync {
    fn blocking_load_started(&self);
    fn blocking_load_finished(&self, succeeded: bool);
    fn checkpoint(
        &self,
        stage: EstablishmentStage,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    fn raw_shutdown_taken(&self);
}

struct DirectConnector;

impl Connector for DirectConnector {
    fn connect(
        &self,
        host: &str,
        port: u16,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = Result<tokio::net::TcpStream>> + Send>> {
        let host = host.to_owned();
        let target = target.to_owned();
        Box::pin(async move { connect::connect(&host, port, &target).await })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeadlineSource {
    Read,
    Total,
}

pub(super) fn select_deadline_source(
    read_deadline: Option<Instant>,
    total_deadline: Option<Instant>,
) -> Option<DeadlineSource> {
    match (read_deadline, total_deadline) {
        (Some(read), Some(total)) if read <= total => Some(DeadlineSource::Read),
        (Some(_), Some(_)) => Some(DeadlineSource::Total),
        (Some(_), None) => Some(DeadlineSource::Read),
        (None, Some(_)) => Some(DeadlineSource::Total),
        (None, None) => None,
    }
}

fn deadline_for(
    source: Option<DeadlineSource>,
    read_deadline: Option<Instant>,
    total_deadline: Option<Instant>,
) -> Option<Instant> {
    match source {
        Some(DeadlineSource::Read) => read_deadline,
        Some(DeadlineSource::Total) => total_deadline,
        None => None,
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_for_body_completion(completion: &mut Option<BodyCompletion>) -> Instant {
    match completion {
        Some(completion) => completion.await,
        None => std::future::pending().await,
    }
}

enum ExchangeEvent {
    Deadline(DeadlineSource),
    Response(std::result::Result<http::Response<Incoming>, hyper::Error>),
    UploadComplete(Instant),
    Driver(std::result::Result<Result<()>, tokio::task::JoinError>),
}

#[derive(Clone, Copy)]
struct EstablishmentDeadlines {
    connect_timeout: Option<Duration>,
    connect_deadline: Option<Instant>,
    total_timeout: Option<Duration>,
    total_deadline: Option<Instant>,
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
    pub read_timeout: Option<Duration>,
    pub total_timeout: Option<Duration>,
    pub total_deadline: Option<Instant>,
    pub content_codecs: ContentCodecs,
}

pub(crate) struct TransportLease {
    pool: Arc<Mutex<Pool>>,
    lease: Option<ConnectionLease>,
}

pub(crate) struct ConnectionDriver {
    task: Option<JoinHandle<Result<()>>>,
    shutdown: Option<std::net::TcpStream>,
    #[cfg(test)]
    shutdown_observer: Option<Arc<dyn EstablishmentControl>>,
}

impl ConnectionDriver {
    fn spawn(
        task: impl Future<Output = Result<()>> + Send + 'static,
        shutdown: Option<std::net::TcpStream>,
    ) -> Self {
        Self {
            task: Some(tokio::spawn(task)),
            shutdown,
            #[cfg(test)]
            shutdown_observer: None,
        }
    }

    #[cfg(test)]
    fn with_shutdown_observer(mut self, observer: Option<Arc<dyn EstablishmentControl>>) -> Self {
        self.shutdown_observer = observer;
        self
    }

    #[cfg(test)]
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
            #[cfg(test)]
            if let Some(observer) = &self.shutdown_observer {
                observer.raw_shutdown_taken();
            }
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
    pub(crate) fn configured(
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
    ) -> Self {
        Self::with_configuration(
            Arc::new(DirectConnector),
            proxy,
            tls,
            default_timeout,
            pool_max_idle_per_host,
            content_codecs,
        )
    }

    #[cfg(test)]
    fn with_connector(connector: Arc<dyn Connector>) -> Self {
        Self::with_configuration(
            connector,
            None,
            TlsConfig::default(),
            Timeout::default(),
            DEFAULT_MAX_IDLE_PER_HOST,
            ContentCodecs::new(true, true),
        )
    }

    fn with_configuration(
        connector: Arc<dyn Connector>,
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
    ) -> Self {
        Self {
            pool: Arc::new(Mutex::new(Pool::new(pool_max_idle_per_host))),
            #[cfg(test)]
            derived_pool_keys: Mutex::new(Vec::new()),
            #[cfg(test)]
            establishment_control: None,
            connector,
            proxy,
            tls,
            default_timeout,
            content_codecs,
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

    #[cfg(test)]
    fn drain_derived_pool_keys(&self) -> Vec<PoolKey> {
        std::mem::take(
            &mut *self
                .derived_pool_keys
                .lock()
                .expect("derived pool-key observation lock poisoned"),
        )
    }

    #[cfg(test)]
    fn with_test_establishment_control(mut self, control: Arc<dyn EstablishmentControl>) -> Self {
        self.establishment_control = Some(control);
        self
    }

    #[cfg(test)]
    async fn establishment_checkpoint(&self, stage: EstablishmentStage) {
        if let Some(control) = &self.establishment_control {
            control.checkpoint(stage).await;
        }
    }

    pub async fn send(&self, request: Request) -> Result<TransportResponse> {
        if self.proxy.is_some() {
            return Err(Error::proxy_not_implemented());
        }
        let timeout = request.timeout().unwrap_or(self.default_timeout);
        let started = Instant::now();
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
        #[cfg(test)]
        self.derived_pool_keys
            .lock()
            .expect("derived pool-key observation lock poisoned")
            .push(key.clone());
        let mut request = request.into_parts();
        if !request.headers.contains_key(ACCEPT_ENCODING) {
            request.headers.insert(
                ACCEPT_ENCODING,
                HeaderValue::from_static(self.content_codecs.accept_encoding()),
            );
        }
        let connect_timeout = timeout.connect;
        let read_timeout = timeout.read;
        let total_timeout = timeout.total;
        let connect_deadline = connect_timeout.and_then(|timeout| started.checked_add(timeout));
        let total_deadline = total_timeout.and_then(|timeout| started.checked_add(timeout));
        let establishment_deadlines = EstablishmentDeadlines {
            connect_timeout,
            connect_deadline,
            total_timeout,
            total_deadline,
        };
        let track_body_completion = read_timeout.is_some() || total_timeout.is_some();
        let (outgoing, url, mut body_completion) =
            outgoing_request(request, track_body_completion)?;

        let mut lease = self
            .acquire_connection(key, &host, port, &target, establishment_deadlines)
            .await?;
        let response = {
            let exchange_started = Instant::now();
            let upload_completed_at = body_completion
                .as_ref()
                .and_then(BodyCompletion::completed_at);
            let mut completion_pending = body_completion.is_some() && upload_completed_at.is_none();
            if upload_completed_at.is_some() {
                body_completion.take();
            }
            let mut head_read_deadline = upload_completed_at.and_then(|completed_at| {
                read_timeout.and_then(|timeout| {
                    std::cmp::max(exchange_started, completed_at).checked_add(timeout)
                })
            });
            let (sender, driver) = lease.connection_mut().network_parts_mut();
            let sending = sender.send_request(outgoing);
            tokio::pin!(sending);
            loop {
                if completion_pending
                    && let Some(completed_at) = body_completion
                        .as_ref()
                        .and_then(BodyCompletion::completed_at)
                {
                    completion_pending = false;
                    body_completion.take();
                    head_read_deadline = read_timeout.and_then(|timeout| {
                        std::cmp::max(exchange_started, completed_at).checked_add(timeout)
                    });
                }
                let deadline_source = select_deadline_source(head_read_deadline, total_deadline);
                let deadline = deadline_for(deadline_source, head_read_deadline, total_deadline);
                let event = {
                    let deadline_wait = wait_for_deadline(deadline);
                    tokio::pin!(deadline_wait);
                    let driver_wait = std::future::poll_fn(|context| {
                        let Some(task) = driver.task_mut() else {
                            return Poll::Pending;
                        };
                        Pin::new(task).poll(context)
                    });
                    tokio::pin!(driver_wait);
                    tokio::select! {
                        biased;
                        () = &mut deadline_wait => ExchangeEvent::Deadline(
                            deadline_source.expect("finite deadline wait requires a source"),
                        ),
                        result = &mut sending => ExchangeEvent::Response(result),
                        completed_at = wait_for_body_completion(&mut body_completion),
                            if completion_pending => {
                            ExchangeEvent::UploadComplete(completed_at)
                        }
                        driver_result = &mut driver_wait => ExchangeEvent::Driver(driver_result),
                    }
                };
                match event {
                    ExchangeEvent::Deadline(DeadlineSource::Read) => {
                        break Err(Error::response_head_timeout(
                            read_timeout
                                .expect("read deadline exists only when timeout is configured"),
                            false,
                        ));
                    }
                    ExchangeEvent::Deadline(DeadlineSource::Total) => {
                        let timeout = total_timeout
                            .expect("total deadline exists only when timeout is configured");
                        let error = if completion_pending {
                            Error::request_exchange_total_timeout(timeout)
                        } else {
                            Error::response_head_timeout(timeout, true)
                        };
                        break Err(error);
                    }
                    ExchangeEvent::Response(result) => break result.map_err(Error::send),
                    ExchangeEvent::UploadComplete(completed_at) => {
                        completion_pending = false;
                        body_completion.take();
                        head_read_deadline = read_timeout.and_then(|timeout| {
                            std::cmp::max(exchange_started, completed_at).checked_add(timeout)
                        });
                    }
                    ExchangeEvent::Driver(driver_result) => match driver.finish(driver_result) {
                        Ok(()) => continue,
                        Err(error) => break Err(error),
                    },
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
                    Err(driver_error) => Err(Error::with_cleanup(send_error, driver_error)),
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
            total_timeout,
            total_deadline,
            content_codecs: self.content_codecs,
        })
    }

    async fn acquire_connection(
        &self,
        key: PoolKey,
        host: &str,
        port: u16,
        target: &str,
        deadlines: EstablishmentDeadlines,
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
                match deadlines.total_deadline {
                    Some(deadline) => {
                        tokio::select! {
                            biased;
                            () = tokio::time::sleep_until(
                                tokio::time::Instant::from_std(deadline)
                            ) => None,
                            result = sender.ready() => Some(result),
                        }
                    }
                    None => Some(sender.ready().await),
                }
            };
            let Some(ready) = ready else {
                TransportLease::new(Arc::clone(&self.pool), lease).finish_now(false);
                return Err(Error::request_exchange_total_timeout(
                    deadlines
                        .total_timeout
                        .expect("total deadline exists only when timeout is configured"),
                ));
            };
            if ready.is_ok() && lease.is_live() && lease.peer_is_open() {
                return Ok(lease);
            }
            TransportLease::new(Arc::clone(&self.pool), lease).finish_now(false);
        }

        self.connect_connection(key, host, port, target, deadlines)
            .await
    }

    async fn connect_connection(
        &self,
        key: PoolKey,
        host: &str,
        port: u16,
        target: &str,
        deadlines: EstablishmentDeadlines,
    ) -> Result<ConnectionLease> {
        let generation = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .generation_number(&key)
        };
        let establishing = async {
            #[cfg(test)]
            self.establishment_checkpoint(EstablishmentStage::Connect)
                .await;
            let stream = self.connector.connect(host, port, target).await?;
            let stream = stream
                .into_std()
                .map_err(|error| Error::connect(target, error))?;
            let shutdown = stream
                .try_clone()
                .map_err(|error| Error::connect(target, error))?;
            let stream = tokio::net::TcpStream::from_std(stream)
                .map_err(|error| Error::connect(target, error))?;
            #[cfg(test)]
            self.establishment_checkpoint(EstablishmentStage::Http1)
                .await;
            let (sender, connection) = http1::handshake(TokioIo::new(stream))
                .await
                .map_err(Error::handshake)?;
            let driver = ConnectionDriver::spawn(
                async move { connection.await.map_err(Error::connection) },
                Some(shutdown),
            );
            #[cfg(test)]
            let driver =
                driver.with_shutdown_observer(self.establishment_control.as_ref().map(Arc::clone));
            Ok(ConnectionLease::new(
                key,
                generation,
                IdleConnection::network(sender, driver),
            ))
        };
        tokio::pin!(establishing);
        match select_deadline_source(deadlines.connect_deadline, deadlines.total_deadline) {
            None => establishing.await,
            Some(source) => {
                let deadline = match source {
                    DeadlineSource::Read => deadlines
                        .connect_deadline
                        .expect("connect deadline selected only when configured"),
                    DeadlineSource::Total => deadlines
                        .total_deadline
                        .expect("total deadline selected only when configured"),
                };
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        let (timeout, total) = match source {
                            DeadlineSource::Read => (
                                deadlines.connect_timeout.expect(
                                    "connect deadline selected only when timeout is configured"
                                ),
                                false,
                            ),
                            DeadlineSource::Total => (
                                deadlines.total_timeout.expect(
                                    "total deadline selected only when timeout is configured"
                                ),
                                true,
                            ),
                        };
                        Err(Error::connect_timeout(target, timeout, total))
                    },
                    result = &mut establishing => result,
                }
            }
        }
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

fn outgoing_request(
    mut request: RequestParts,
    track_body_completion: bool,
) -> Result<(http::Request<OutgoingBody>, String, Option<BodyCompletion>)> {
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

    let (body, completion) = OutgoingBody::new(request.body, track_body_completion);
    let mut outgoing = http::Request::new(body);
    *outgoing.method_mut() = request.method;
    *outgoing.uri_mut() = origin;
    *outgoing.headers_mut() = request.headers;
    Ok((outgoing, request.url, completion))
}

struct OutgoingBody {
    source: BodySource,
    progress: Option<Arc<BodyProgress>>,
}

impl OutgoingBody {
    fn new(source: BodySource, track_completion: bool) -> (Self, Option<BodyCompletion>) {
        let initially_complete = match &source {
            BodySource::Empty => true,
            BodySource::Bytes(bytes) => bytes.is_empty(),
            BodySource::Stream(_) => false,
        };
        let progress = track_completion.then(|| Arc::new(BodyProgress::new(initially_complete)));
        let completion = progress.as_ref().map(|progress| BodyCompletion {
            progress: Arc::clone(progress),
        });
        (Self { source, progress }, completion)
    }

    fn mark_complete(&self) {
        if let Some(progress) = &self.progress {
            progress.mark_complete();
        }
    }
}

struct BodyProgress {
    state: Mutex<BodyProgressState>,
}

struct BodyProgressState {
    completed_at: Option<Instant>,
    waker: Option<Waker>,
}

impl BodyProgress {
    fn new(complete: bool) -> Self {
        Self {
            state: Mutex::new(BodyProgressState {
                completed_at: complete.then(Instant::now),
                waker: None,
            }),
        }
    }

    fn mark_complete(&self) {
        let waker = {
            let mut state = self
                .state
                .lock()
                .expect("request body progress lock poisoned");
            if state.completed_at.is_some() {
                return;
            }
            state.completed_at = Some(Instant::now());
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn completed_at(&self) -> Option<Instant> {
        self.state
            .lock()
            .expect("request body progress lock poisoned")
            .completed_at
    }

    fn poll_complete(&self, context: &mut Context<'_>) -> Poll<Instant> {
        let mut state = self
            .state
            .lock()
            .expect("request body progress lock poisoned");
        if let Some(completed_at) = state.completed_at {
            state.waker.take();
            Poll::Ready(completed_at)
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

struct BodyCompletion {
    progress: Arc<BodyProgress>,
}

impl BodyCompletion {
    fn completed_at(&self) -> Option<Instant> {
        self.progress.completed_at()
    }
}

impl Future for BodyCompletion {
    type Output = Instant;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.progress.poll_complete(context)
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
            BodySource::Empty => {
                body.mark_complete();
                Poll::Ready(None)
            }
            BodySource::Bytes(bytes) if bytes.is_empty() => {
                body.mark_complete();
                Poll::Ready(None)
            }
            BodySource::Bytes(bytes) => {
                body.mark_complete();
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            BodySource::Stream(mut stream) => match stream.as_mut().poll_next(context) {
                Poll::Pending => {
                    body.source = BodySource::Stream(stream);
                    Poll::Pending
                }
                Poll::Ready(Some(chunk)) => {
                    body.source = BodySource::Stream(stream);
                    Poll::Ready(Some(chunk.map(Frame::data)))
                }
                Poll::Ready(None) => {
                    body.mark_complete();
                    Poll::Ready(None)
                }
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
    fn connection_driver_shutdown_handle_is_a_raw_standard_tcp_stream() {
        let driver = ConnectionDriver {
            task: None,
            shutdown: None,
            shutdown_observer: None,
        };
        let ConnectionDriver {
            task,
            shutdown,
            shutdown_observer,
        } = &driver;
        let _: &Option<std::net::TcpStream> = shutdown;
        assert!(task.is_none());
        assert!(shutdown_observer.is_none());
    }

    #[test]
    fn production_http1_inventory_requires_one_shared_generic_seam() {
        // The future helper is intentionally absent in RED. Referencing it as
        // a Rust item would make the test target fail to compile, so this
        // guard is lexical and strictly bounded to the production prefix.
        let source = include_str!("mod.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("transport source keeps one final cfg(test) module");

        assert_eq!(
            production.matches("http1::handshake(").count(),
            1,
            "plain and TLS streams must share exactly one Hyper HTTP/1 handshake"
        );
        for forbidden in [
            "hyper_rustls",
            "MaybeHttpsStream",
            "dyn AsyncRead",
            "dyn AsyncWrite",
            "dyn tokio::io::AsyncRead",
            "dyn tokio::io::AsyncWrite",
        ] {
            assert!(
                !production.contains(forbidden),
                "production transport must not contain erased or wrapper I/O pattern {forbidden:?}"
            );
        }

        let definitions = production
            .lines()
            .filter(|line| line.contains("fn start_http1<"))
            .collect::<Vec<_>>();
        assert_eq!(
            definitions.len(),
            1,
            "production transport requires one generic start_http1 seam"
        );
        assert!(
            !definitions[0].contains("pub"),
            "start_http1 must remain private"
        );
        assert!(
            production.matches("start_http1(").count() >= 2,
            "plain and TLS establishment must both call the shared start_http1 seam"
        );
    }

    #[test]
    fn request_validation_allows_http_and_https_but_rejects_other_schemes() {
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
        let ftp = RequestBuilder::new(Method::GET, "ftp://example.test/")
            .build()
            .unwrap();

        validate_request(&bytes).unwrap();
        validate_request(&stream).unwrap();
        validate_request(&https).unwrap();
        assert_eq!(
            validate_request(&ftp).unwrap_err().kind(),
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

        let (outgoing, url, completion) = outgoing_request(request.into_parts(), false).unwrap();

        assert_eq!(outgoing.uri().to_string(), "/direct?source=unit");
        assert!(completion.is_none());
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

use std::fmt;
use std::future::{Future, poll_fn};
use std::io::{self, Read};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use bytes::Bytes;
use futures_core::Stream;
use tokio::runtime::{Handle, Runtime, RuntimeFlavor};
use tokio::task::AbortHandle;

use crate::{
    BodySource, Client as AsyncClient, ClientBuilder as AsyncClientBuilder, Error, HeaderMap,
    HeaderName, HeaderValue, Method, Proxy, Request, RequestBuilder as AsyncRequestBuilder,
    Response as AsyncResponse, ResponseBody as AsyncResponseBody, Result as RequestResult,
    StatusCode, Timeout, TlsConfig, Version,
};

static PROCESS_RUNTIME: OnceLock<DriverRegistry> = OnceLock::new();
static NEXT_SUBMISSION_ID: AtomicU64 = AtomicU64::new(1);
static OUTSTANDING_SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
static SUBMISSION_PROCESS_ID: AtomicU32 = AtomicU32::new(0);

tokio::task_local! {
    static ACTIVE_SUBMISSION_ID: u64;
}

#[derive(Clone)]
pub struct BlockingRuntimeDriver {
    inner: Arc<DriverInner>,
}

impl BlockingRuntimeDriver {
    pub fn process_local() -> io::Result<Self> {
        PROCESS_RUNTIME
            .get_or_init(DriverRegistry::default)
            .driver_for(std::process::id())
    }

    pub fn process_id(&self) -> u32 {
        self.inner.process_id
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    #[doc(hidden)]
    pub fn quiesce_process_local() -> bool {
        PROCESS_RUNTIME
            .get_or_init(DriverRegistry::default)
            .clear_for_process(std::process::id())
    }

    pub fn is_same_runtime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn submit<F>(&self, future: F) -> Result<BlockingSubmission<F::Output>, BlockingDriverError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.submit_for_process(future, std::process::id())
    }

    fn submit_for_process<F>(
        &self,
        future: F,
        current_process_id: u32,
    ) -> Result<BlockingSubmission<F::Output>, BlockingDriverError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if current_process_id != self.process_id() {
            return Err(BlockingDriverError::StaleProcess {
                recorded: self.process_id(),
                current: current_process_id,
            });
        }
        ensure_submission_process(current_process_id);
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let id = NEXT_SUBMISSION_ID.fetch_add(1, Ordering::Relaxed);
        let parent_id = ACTIVE_SUBMISSION_ID.try_with(|active| *active).ok();
        OUTSTANDING_SUBMISSIONS.fetch_add(1, Ordering::AcqRel);
        let outstanding = OutstandingSubmission;
        let task = self
            .inner
            .handle
            .spawn(ACTIVE_SUBMISSION_ID.scope(id, async move {
                let _outstanding = outstanding;
                let output = future.await;
                let _ = sender.send(output);
            }));
        Ok(BlockingSubmission {
            id,
            parent_id,
            process_id: self.process_id(),
            receiver: Some(receiver),
            abort: Some(task.abort_handle()),
            cancelled,
        })
    }
}

impl fmt::Debug for BlockingRuntimeDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlockingRuntimeDriver")
            .field("process_id", &self.process_id())
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlockingDriverError {
    StaleProcess { recorded: u32, current: u32 },
}

impl fmt::Display for BlockingDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleProcess { recorded, current } => write!(
                formatter,
                "blocking runtime belongs to process {recorded}, not current process {current}"
            ),
        }
    }
}

impl std::error::Error for BlockingDriverError {}

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlockingTaskError {
    Cancelled,
    WorkerStopped,
    AlreadyCompleted,
    StaleProcess { recorded: u32, current: u32 },
}

impl fmt::Display for BlockingTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("blocking runtime task was cancelled"),
            Self::WorkerStopped => formatter.write_str("blocking runtime task stopped"),
            Self::AlreadyCompleted => {
                formatter.write_str("blocking runtime task result was already consumed")
            }
            Self::StaleProcess { recorded, current } => write!(
                formatter,
                "blocking runtime task belongs to process {recorded}, not current process {current}"
            ),
        }
    }
}

impl std::error::Error for BlockingTaskError {}

pub struct BlockingSubmission<T> {
    id: u64,
    parent_id: Option<u64>,
    process_id: u32,
    receiver: Option<mpsc::Receiver<T>>,
    abort: Option<AbortHandle>,
    cancelled: Arc<AtomicBool>,
}

impl<T> BlockingSubmission<T> {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn parent_id(&self) -> Option<u64> {
        self.parent_id
    }

    pub fn try_wait(&mut self) -> Result<Option<T>, BlockingTaskError> {
        self.try_wait_for_process(std::process::id())
    }

    fn try_wait_for_process(
        &mut self,
        current_process_id: u32,
    ) -> Result<Option<T>, BlockingTaskError> {
        self.validate_process(current_process_id)?;
        let Some(receiver) = self.receiver.as_ref() else {
            return Err(BlockingTaskError::AlreadyCompleted);
        };
        match receiver.try_recv() {
            Ok(output) => {
                self.receiver = None;
                Ok(Some(output))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                Err(self.stopped_error())
            }
        }
    }

    pub fn wait(self) -> Result<T, BlockingTaskError> {
        self.wait_for_process(std::process::id())
    }

    fn wait_for_process(mut self, current_process_id: u32) -> Result<T, BlockingTaskError> {
        self.validate_process(current_process_id)?;
        let receiver = self
            .receiver
            .take()
            .ok_or(BlockingTaskError::AlreadyCompleted)?;
        let result = match Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| receiver.recv())
            }
            _ => receiver.recv(),
        };
        result.map_err(|_| self.stopped_error())
    }

    pub fn cancel(&self) -> Result<(), BlockingTaskError> {
        self.cancel_for_process(std::process::id())
    }

    fn cancel_for_process(&self, current_process_id: u32) -> Result<(), BlockingTaskError> {
        self.validate_process(current_process_id)?;
        if self.receiver.is_none() {
            return Err(BlockingTaskError::AlreadyCompleted);
        }
        self.cancelled.store(true, Ordering::Release);
        if let Some(abort) = self.abort.as_ref() {
            abort.abort();
        }
        Ok(())
    }

    fn validate_process(&self, current_process_id: u32) -> Result<(), BlockingTaskError> {
        if current_process_id == self.process_id {
            Ok(())
        } else {
            Err(BlockingTaskError::StaleProcess {
                recorded: self.process_id,
                current: current_process_id,
            })
        }
    }

    fn finish_drop_for_process(&mut self, current_process_id: u32) -> bool {
        let Some(abort) = self.abort.take() else {
            return false;
        };
        if current_process_id == self.process_id {
            abort.abort();
            true
        } else {
            mem::forget(abort);
            false
        }
    }

    fn stopped_error(&self) -> BlockingTaskError {
        if self.cancelled.load(Ordering::Acquire) {
            BlockingTaskError::Cancelled
        } else {
            BlockingTaskError::WorkerStopped
        }
    }
}

struct OutstandingSubmission;

impl Drop for OutstandingSubmission {
    fn drop(&mut self) {
        OUTSTANDING_SUBMISSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[doc(hidden)]
pub fn outstanding_submission_count() -> usize {
    ensure_submission_process(std::process::id());
    OUTSTANDING_SUBMISSIONS.load(Ordering::Acquire)
}

fn ensure_submission_process(process_id: u32) {
    if SUBMISSION_PROCESS_ID.load(Ordering::Acquire) == process_id {
        return;
    }
    if SUBMISSION_PROCESS_ID.swap(process_id, Ordering::AcqRel) != process_id {
        NEXT_SUBMISSION_ID.store(1, Ordering::Release);
        OUTSTANDING_SUBMISSIONS.store(0, Ordering::Release);
    }
}

impl<T> Drop for BlockingSubmission<T> {
    fn drop(&mut self) {
        self.finish_drop_for_process(std::process::id());
    }
}

struct DriverInner {
    process_id: u32,
    generation: u64,
    handle: Handle,
    _runtime: RuntimeOwner,
}

struct RuntimeOwner {
    runtime: Option<Runtime>,
    process_id: u32,
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        if std::process::id() != self.process_id
            && let Some(runtime) = self.runtime.take()
        {
            mem::forget(runtime);
        }
    }
}

struct DriverRegistry {
    current: Mutex<Option<Arc<DriverInner>>>,
    next_generation: AtomicU64,
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self {
            current: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }
}

impl DriverRegistry {
    fn clear_for_process(&self, process_id: u32) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current
            .as_ref()
            .is_none_or(|driver| driver.process_id != process_id)
        {
            return false;
        }
        drop(current.take());
        true
    }

    fn driver_for(&self, process_id: u32) -> io::Result<BlockingRuntimeDriver> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| io::Error::other("blocking runtime registry lock poisoned"))?;
        if let Some(driver) = current.as_ref()
            && driver.process_id == process_id
        {
            return Ok(BlockingRuntimeDriver {
                inner: Arc::clone(driver),
            });
        }
        if let Some(inherited) = current.take() {
            mem::forget(inherited);
        }
        ensure_submission_process(process_id);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .enable_time()
            .thread_name("requests-runtime")
            .build()?;
        let generation = (u64::from(process_id) << 32)
            | ((self.next_generation.fetch_add(1, Ordering::Relaxed) + 1) & u64::from(u32::MAX));
        let driver = Arc::new(DriverInner {
            process_id,
            generation,
            handle: runtime.handle().clone(),
            _runtime: RuntimeOwner {
                runtime: Some(runtime),
                process_id,
            },
        });
        *current = Some(Arc::clone(&driver));
        Ok(BlockingRuntimeDriver { inner: driver })
    }
}

#[derive(Clone)]
pub struct Client {
    inner: AsyncClient,
    driver: BlockingRuntimeDriver,
}

pub struct ClientBuilder {
    inner: AsyncClientBuilder,
}

pub struct RequestBuilder {
    inner: AsyncRequestBuilder,
    driver: BlockingRuntimeDriver,
}

pub struct Response {
    inner: AsyncResponse,
    driver: BlockingRuntimeDriver,
}

pub struct ResponseBody {
    inner: Option<AsyncResponseBody>,
    driver: BlockingRuntimeDriver,
    remainder: Option<Bytes>,
    pending_error: Option<io::Error>,
    terminal: bool,
}

impl Client {
    pub fn new() -> RequestResult<Self> {
        Self::builder().build()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder {
            inner: AsyncClient::builder(),
        }
    }

    pub fn request(&self, method: Method, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.request(method, url),
            driver: self.driver.clone(),
        }
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.get(url),
            driver: self.driver.clone(),
        }
    }

    pub fn head(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.head(url),
            driver: self.driver.clone(),
        }
    }

    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.post(url),
            driver: self.driver.clone(),
        }
    }

    pub fn put(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.put(url),
            driver: self.driver.clone(),
        }
    }

    pub fn patch(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.patch(url),
            driver: self.driver.clone(),
        }
    }

    pub fn delete(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            inner: self.inner.delete(url),
            driver: self.driver.clone(),
        }
    }

    pub fn execute(&self, request: Request) -> RequestResult<Response> {
        let client = self.inner.clone();
        let inner = submit_and_wait(&self.driver, async move { client.execute(request).await })?;
        Ok(Response {
            inner,
            driver: self.driver.clone(),
        })
    }

    #[doc(hidden)]
    pub fn clear_pool(&self) {
        self.inner.clear_pool();
    }
}

impl ClientBuilder {
    pub fn proxy(self, proxy: Proxy) -> Self {
        Self {
            inner: self.inner.proxy(proxy),
        }
    }

    pub fn tls(self, tls: TlsConfig) -> Self {
        Self {
            inner: self.inner.tls(tls),
        }
    }

    pub fn timeout(self, timeout: Timeout) -> Self {
        Self {
            inner: self.inner.timeout(timeout),
        }
    }

    pub fn pool_max_idle_per_host(self, maximum: usize) -> Self {
        Self {
            inner: self.inner.pool_max_idle_per_host(maximum),
        }
    }

    pub fn build(self) -> RequestResult<Client> {
        let inner = self.inner.build()?;
        let driver = BlockingRuntimeDriver::process_local().map_err(Error::blocking)?;
        Ok(Client { inner, driver })
    }
}

impl RequestBuilder {
    pub fn header(self, name: HeaderName, value: HeaderValue) -> Self {
        Self {
            inner: self.inner.header(name, value),
            driver: self.driver,
        }
    }

    pub fn headers(self, headers: HeaderMap) -> Self {
        Self {
            inner: self.inner.headers(headers),
            driver: self.driver,
        }
    }

    pub fn body(self, body: impl Into<BodySource>) -> Self {
        Self {
            inner: self.inner.body(body),
            driver: self.driver,
        }
    }

    pub fn timeout(self, timeout: Timeout) -> Self {
        Self {
            inner: self.inner.timeout(timeout),
            driver: self.driver,
        }
    }

    pub fn build(self) -> RequestResult<Request> {
        self.inner.build()
    }

    pub fn send(self) -> RequestResult<Response> {
        let Self { inner, driver } = self;
        let response = submit_and_wait(&driver, async move { inner.send().await })?;
        Ok(Response {
            inner: response,
            driver,
        })
    }

    /// Execute this request on an already-running async driver.
    ///
    /// The Python adapter uses this path so its signal-aware origin driver owns
    /// the only runtime submission for the transport future.
    #[doc(hidden)]
    pub async fn send_async(self) -> RequestResult<Response> {
        let Self { inner, driver } = self;
        let response = inner.send().await?;
        Ok(Response {
            inner: response,
            driver,
        })
    }
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    pub fn reason(&self) -> &str {
        self.inner.reason()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    pub fn url(&self) -> &str {
        self.inner.url()
    }

    pub fn version(&self) -> Version {
        self.inner.version()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.inner.content_length()
    }

    pub fn into_body(self) -> ResponseBody {
        let Self { inner, driver } = self;
        ResponseBody {
            inner: Some(inner.into_body()),
            driver,
            remainder: None,
            pending_error: None,
            terminal: false,
        }
    }

    pub fn into_raw_body(self) -> ResponseBody {
        let Self { inner, driver } = self;
        ResponseBody {
            inner: Some(inner.into_raw_body()),
            driver,
            remainder: None,
            pending_error: None,
            terminal: false,
        }
    }

    pub fn bytes(self) -> RequestResult<Bytes> {
        let Self { inner, driver } = self;
        submit_and_wait(&driver, async move { inner.bytes().await })
    }

    pub fn text(self) -> RequestResult<String> {
        let Self { inner, driver } = self;
        submit_and_wait(&driver, async move { inner.text().await })
    }
}

impl Read for ResponseBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some(remainder) = self.remainder.take() {
            let read = copy_frame(buffer, remainder, &mut self.remainder);
            self.finish_declared_length_if_buffer_empty(false);
            return Ok(read);
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.terminal {
            return Ok(0);
        }
        let Some(mut body) = self.inner.take() else {
            self.terminal = true;
            return Ok(0);
        };

        let polled = submit_and_wait(&self.driver, async move {
            let frame = loop {
                match poll_fn(|context| Pin::new(&mut body).poll_next(context)).await {
                    Some(Ok(frame)) if frame.is_empty() => {}
                    frame => break frame,
                }
            };
            Ok((body, frame))
        });
        let (mut body, frame) = match polled {
            Ok(polled) => polled,
            Err(error) => {
                self.terminal = true;
                return Err(io::Error::other(error));
            }
        };
        let frame = match frame {
            Some(Ok(frame)) => {
                let read = copy_frame(buffer, frame, &mut self.remainder);
                if self.remainder.is_none() {
                    body.finish_declared_length(false);
                }
                if body.is_terminal() {
                    self.terminal = true;
                } else {
                    self.inner = Some(body);
                }
                return Ok(read);
            }
            frame => frame,
        };
        if body.is_terminal() {
            self.terminal = true;
        } else {
            self.inner = Some(body);
        }
        match frame {
            Some(Ok(_)) => unreachable!("successful frames return after copying"),
            Some(Err(error)) => {
                self.terminal = true;
                Err(io::Error::other(error))
            }
            None => {
                self.terminal = true;
                Ok(0)
            }
        }
    }
}

impl ResponseBody {
    #[doc(hidden)]
    pub fn is_terminal(&self) -> bool {
        self.terminal && self.remainder.is_none() && self.pending_error.is_none()
    }

    fn finish_declared_length_if_buffer_empty(&mut self, allow_encoded_completion: bool) {
        if self.remainder.is_some() {
            return;
        }
        let terminal = self.inner.as_mut().is_some_and(|body| {
            body.finish_declared_length(allow_encoded_completion);
            body.is_terminal()
        });
        if terminal {
            self.terminal = true;
            self.inner = None;
        }
    }

    /// Read from the async response body without creating another runtime
    /// submission. Ownership makes cancellation drop the body and its lease.
    #[doc(hidden)]
    pub async fn read_async(
        self,
        amount: Option<usize>,
        allow_encoded_completion: bool,
    ) -> io::Result<(Self, Vec<u8>)> {
        self.read_async_inner(amount, allow_encoded_completion, true)
            .await
    }

    /// Read at most one transport frame. Decoders use this so reaching their
    /// requested decoded output does not consume a later EOF implicitly.
    #[doc(hidden)]
    pub async fn read_frame_async(
        self,
        maximum: usize,
        allow_encoded_completion: bool,
    ) -> io::Result<(Self, Vec<u8>)> {
        self.read_async_inner(Some(maximum), allow_encoded_completion, false)
            .await
    }

    async fn read_async_inner(
        mut self,
        amount: Option<usize>,
        allow_encoded_completion: bool,
        fill_requested: bool,
    ) -> io::Result<(Self, Vec<u8>)> {
        if amount == Some(0) {
            return Ok((self, Vec::new()));
        }
        let mut output = Vec::new();
        if let Some(remainder) = self.remainder.take() {
            match amount {
                Some(amount) => {
                    let read = amount.min(remainder.len());
                    output.extend_from_slice(&remainder[..read]);
                    if read < remainder.len() {
                        self.remainder = Some(remainder.slice(read..));
                    }
                    if output.len() == amount {
                        self.finish_declared_length_if_buffer_empty(allow_encoded_completion);
                        return Ok((self, output));
                    }
                }
                None => output.extend_from_slice(&remainder),
            }
            self.finish_declared_length_if_buffer_empty(allow_encoded_completion);
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.terminal {
            return Ok((self, output));
        }
        let Some(mut body) = self.inner.take() else {
            self.terminal = true;
            return Ok((self, output));
        };

        loop {
            let frame = loop {
                match poll_fn(|context| Pin::new(&mut body).poll_next(context)).await {
                    Some(Ok(frame)) if frame.is_empty() => {}
                    frame => break frame,
                }
            };
            match frame {
                Some(Ok(frame)) => match amount {
                    Some(amount) => {
                        let wanted = amount.saturating_sub(output.len());
                        let read = wanted.min(frame.len());
                        output.extend_from_slice(&frame[..read]);
                        if read < frame.len() {
                            self.remainder = Some(frame.slice(read..));
                        }
                        if self.remainder.is_none() {
                            body.finish_declared_length(allow_encoded_completion);
                        }
                        if body.is_terminal() {
                            self.terminal = true;
                            return Ok((self, output));
                        }
                        if output.len() == amount || !fill_requested {
                            self.inner = Some(body);
                            return Ok((self, output));
                        }
                    }
                    None => {
                        output.extend_from_slice(&frame);
                        body.finish_declared_length(allow_encoded_completion);
                        if body.is_terminal() {
                            self.terminal = true;
                            return Ok((self, output));
                        }
                    }
                },
                Some(Err(error)) => {
                    self.terminal = true;
                    let error = io::Error::other(error);
                    if output.is_empty() {
                        return Err(error);
                    }
                    self.pending_error = Some(error);
                    return Ok((self, output));
                }
                None => {
                    self.terminal = true;
                    return Ok((self, output));
                }
            }
        }
    }

    /// Mark an encoded wire body clean only after its decoder has proved that
    /// the complete representation was consumed successfully.
    #[doc(hidden)]
    pub fn finish_encoded_declared_length(&mut self) {
        self.finish_declared_length_if_buffer_empty(true);
    }

    #[doc(hidden)]
    pub async fn close_async(mut self) -> RequestResult<()> {
        let Some(body) = self.inner.take() else {
            return Ok(());
        };
        body.close().await
    }

    pub fn close(mut self) -> RequestResult<()> {
        let Some(body) = self.inner.take() else {
            return Ok(());
        };
        submit_and_wait(&self.driver, async move { body.close().await })
    }
}

pub fn get(url: impl AsRef<str>) -> RequestResult<Response> {
    Client::new()?.get(url).send()
}

pub fn head(url: impl AsRef<str>) -> RequestResult<Response> {
    Client::new()?.head(url).send()
}

pub fn post(url: impl AsRef<str>, body: impl Into<BodySource>) -> RequestResult<Response> {
    Client::new()?.post(url).body(body).send()
}

pub fn put(url: impl AsRef<str>, body: impl Into<BodySource>) -> RequestResult<Response> {
    Client::new()?.put(url).body(body).send()
}

pub fn patch(url: impl AsRef<str>, body: impl Into<BodySource>) -> RequestResult<Response> {
    Client::new()?.patch(url).body(body).send()
}

pub fn delete(url: impl AsRef<str>) -> RequestResult<Response> {
    Client::new()?.delete(url).send()
}

fn submit_and_wait<T, F>(driver: &BlockingRuntimeDriver, future: F) -> RequestResult<T>
where
    T: Send + 'static,
    F: Future<Output = RequestResult<T>> + Send + 'static,
{
    let submission = driver.submit(future).map_err(Error::blocking)?;
    submission.wait().map_err(Error::blocking)?
}

fn copy_frame(buffer: &mut [u8], frame: Bytes, remainder: &mut Option<Bytes>) -> usize {
    let read = buffer.len().min(frame.len());
    buffer[..read].copy_from_slice(&frame[..read]);
    if read < frame.len() {
        *remainder = Some(frame.slice(read..));
    }
    read
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::io::{self, Read};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncWriteExt;

    use super::{
        BlockingDriverError, BlockingRuntimeDriver, BlockingSubmission, BlockingTaskError,
        DriverRegistry,
    };

    #[derive(Debug, Eq, PartialEq)]
    struct FutureOutput {
        value: String,
        worker: thread::ThreadId,
    }

    #[test]
    fn process_local_driver_is_reused_and_runs_generic_futures_off_caller_thread() {
        let caller = thread::current().id();
        let first = BlockingRuntimeDriver::process_local().unwrap();
        let second = BlockingRuntimeDriver::process_local().unwrap();

        assert_eq!(first.process_id(), std::process::id());
        assert_eq!(first.generation(), second.generation());
        assert!(first.is_same_runtime(&second));

        let output = first
            .submit(async move {
                FutureOutput {
                    value: String::from("complete"),
                    worker: thread::current().id(),
                }
            })
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(output.value, "complete");
        assert_ne!(output.worker, caller);
    }

    #[test]
    fn process_local_runtime_drives_tokio_tcp_io() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> io::Result<[u8; 4]> {
            let deadline = Instant::now() + Duration::from_secs(1);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "timed out accepting loopback connection",
                            ));
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut request = [0; 4];
            stream.read_exact(&mut request)?;
            Ok(request)
        });

        let client_result = BlockingRuntimeDriver::process_local()
            .unwrap()
            .submit(async move {
                let mut stream = tokio::net::TcpStream::connect(address).await?;
                stream.write_all(b"ping").await
            })
            .unwrap()
            .wait();
        let server_result = server.join().expect("loopback server thread panicked");

        client_result
            .expect("process runtime worker stopped")
            .expect("Tokio TCP client failed");
        assert_eq!(server_result.expect("loopback server failed"), *b"ping");
    }

    #[test]
    fn submission_wait_yields_a_multi_thread_runtime_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let watchdog_sender = sender.clone();
        let watchdog = thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            let _ = watchdog_sender.send("watchdog");
        });

        let result = runtime.block_on(async move {
            tokio::spawn(async move {
                let _runtime_task = tokio::spawn(async move {
                    let _ = sender.send("runtime");
                });
                BlockingRuntimeDriver::process_local()
                    .unwrap()
                    .submit(async move { receiver.recv().unwrap() })
                    .unwrap()
                    .wait()
                    .unwrap()
            })
            .await
            .unwrap()
        });
        watchdog.join().unwrap();

        assert_eq!(result, "runtime");
    }

    #[test]
    fn submission_supports_try_wait_wait_and_cancel() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let mut ready = driver.submit(async { 42_u16 }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let value = loop {
            if let Some(value) = ready.try_wait().unwrap() {
                break value;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        };
        assert_eq!(value, 42);

        let mut pending = driver.submit(future::pending::<()>()).unwrap();
        assert_eq!(pending.try_wait().unwrap(), None);
        pending.cancel().unwrap();
        assert_eq!(pending.wait(), Err(BlockingTaskError::Cancelled));
    }

    #[test]
    fn cancel_rejects_a_submission_after_its_output_was_consumed() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let mut submission = driver.submit(async { 42_u16 }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if submission.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }

        assert_eq!(
            submission.cancel(),
            Err(BlockingTaskError::AlreadyCompleted)
        );
    }

    #[test]
    fn blocking_wait_does_not_enter_the_callers_tokio_runtime() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let caller_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let value = caller_runtime.block_on(async move {
            driver
                .submit(async { String::from("no nested block_on") })
                .unwrap()
                .wait()
                .unwrap()
        });

        assert_eq!(value, "no nested block_on");
    }

    #[test]
    fn registry_releases_its_lock_before_submitted_work_runs() {
        let registry = Arc::new(DriverRegistry::default());
        let process_id = std::process::id();
        let driver = registry.driver_for(process_id).unwrap();
        let generation = driver.generation();
        let work_registry = Arc::clone(&registry);

        let observed_generation = driver
            .submit(async move { work_registry.driver_for(process_id).unwrap().generation() })
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(observed_generation, generation);
    }

    #[test]
    fn stale_driver_refuses_to_submit_to_an_inherited_runtime() {
        let registry = DriverRegistry::default();
        let parent_pid = std::process::id();
        let parent = registry.driver_for(parent_pid).unwrap();

        let result = parent.submit_for_process(async {}, parent_pid.wrapping_add(1));

        assert!(matches!(
            result,
            Err(BlockingDriverError::StaleProcess {
                recorded,
                current,
            }) if recorded == parent_pid && current == parent_pid.wrapping_add(1)
        ));
    }

    #[test]
    fn inherited_submission_try_wait_rejects_the_child_process() {
        let parent_pid = std::process::id();
        let mut submission = pending_submission();

        let result = submission.try_wait_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_wait_rejects_the_child_process_without_blocking() {
        let parent_pid = std::process::id();
        let submission = pending_submission();

        let result = submission.wait_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_cancel_rejects_the_child_process() {
        let parent_pid = std::process::id();
        let submission = pending_submission();

        let result = submission.cancel_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_drop_skips_the_tokio_abort_handle() {
        let parent_pid = std::process::id();
        let mut local_submission = pending_submission();
        let mut inherited_submission = pending_submission();

        assert!(local_submission.finish_drop_for_process(parent_pid));
        assert!(!inherited_submission.finish_drop_for_process(parent_pid.wrapping_add(1)));
    }

    fn pending_submission() -> BlockingSubmission<()> {
        BlockingRuntimeDriver::process_local()
            .unwrap()
            .submit(future::pending())
            .unwrap()
    }

    fn assert_stale_process(error: BlockingTaskError, recorded: u32) {
        assert!(matches!(
            error,
            BlockingTaskError::StaleProcess {
                recorded: error_recorded,
                current,
            } if error_recorded == recorded && current == recorded.wrapping_add(1)
        ));
    }

    #[test]
    fn process_identity_change_leaks_inherited_runtime_and_advances_generation() {
        let registry = DriverRegistry::default();
        let parent_pid = std::process::id();
        let parent = registry.driver_for(parent_pid).unwrap();
        let parent_generation = parent.generation();
        let inherited_runtime = Arc::downgrade(&parent.inner);
        drop(parent);

        let child = registry.driver_for(parent_pid.wrapping_add(1)).unwrap();

        assert_ne!(child.process_id(), parent_pid);
        assert!(child.generation() > parent_generation);
        assert!(
            inherited_runtime.upgrade().is_some(),
            "PID replacement must leak rather than drop/join inherited threads"
        );
    }
}

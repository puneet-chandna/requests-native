mod connect;
pub(crate) mod decode;
#[cfg(test)]
mod establishment_tests;
mod pool;
#[cfg(test)]
mod pool_tests;
mod proxy;
#[cfg(test)]
mod timeout_tests;
mod tls;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::HeaderValue;
use http::header::{ACCEPT_ENCODING, CONTENT_LENGTH, HOST, PROXY_AUTHORIZATION};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::JoinHandle;

use self::pool::{
    ConnectionLease, DirtyLeaseCause, IdentityKey, IdleConnection, Pool, PoolKey, ProxyKey,
    TlsPoolKey,
};
use crate::client::{OriginUploadActionEntered, UploadQueuedExecutedReplyCounts};
use crate::models::RequestParts;
use crate::response::ResponseHeadWaitEntered;
use crate::session_runtime::{
    SessionCheckpoint, SessionConnectionIdentity, SessionExchangeIdentity, SessionLeaseIdentity,
    SessionPhase, SessionRuntimeHarness,
};
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
    session_runtime: Option<SessionRuntimeHarness>,
}

pub(super) trait Connector: Send + Sync {
    fn connect(
        &self,
        host: &str,
        port: u16,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = Result<tokio::net::TcpStream>> + Send>>;

    fn connect_session(
        &self,
        host: &str,
        port: u16,
        target: &str,
        _checkpoint: Option<SessionCheckpoint>,
    ) -> Pin<Box<dyn Future<Output = Result<tokio::net::TcpStream>> + Send>> {
        self.connect(host, port, target)
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EstablishmentStage {
    Connect,
    Tls,
    Http1,
}

pub(super) type NativeRootLoader =
    Arc<dyn Fn() -> rustls_native_certs::CertificateResult + Send + Sync>;

#[cfg(test)]
#[allow(dead_code)]
pub(super) trait EstablishmentControl: Send + Sync {
    fn blocking_load_started(&self);
    fn blocking_load_finished(&self, succeeded: bool);
    fn native_root_loader(&self) -> Option<NativeRootLoader> {
        None
    }
    fn checkpoint(
        &self,
        stage: EstablishmentStage,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    fn raw_shutdown_taken(&self);
}

struct DirectConnector {
    session_runtime: Option<SessionRuntimeHarness>,
}

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

    fn connect_session(
        &self,
        host: &str,
        port: u16,
        target: &str,
        checkpoint: Option<SessionCheckpoint>,
    ) -> Pin<Box<dyn Future<Output = Result<tokio::net::TcpStream>> + Send>> {
        let host = host.to_owned();
        let target = target.to_owned();
        let session_runtime = self.session_runtime.clone();
        Box::pin(async move {
            let gate = session_runtime
                .clone()
                .zip(checkpoint)
                .map(|(harness, checkpoint)| {
                    connect::SessionInjectedConnectorGate::new(harness, checkpoint)
                });
            let result = connect::connect_with_gate(&host, port, &target, gate).await;
            if let (Some(harness), Some(mut checkpoint)) = (&session_runtime, checkpoint) {
                checkpoint.phase = SessionPhase::ConnectReadyRace;
                harness.wait(checkpoint).await;
            }
            result
        })
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
    ResponseHeadGate,
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
    pub raw_headers: Vec<(String, Vec<u8>)>,
}

pub(crate) struct TransportLease {
    pool: Arc<Mutex<Pool>>,
    lease: Option<ConnectionLease>,
    active_exchange: ActiveExchangeGuard,
}

struct ActiveExchangeGuard {
    session_runtime: Option<SessionRuntimeHarness>,
    connection: Option<SessionConnectionIdentity>,
    lease: Option<SessionLeaseIdentity>,
    correlation: u64,
    released: bool,
    response_remainder_gate: ResponseRemainderGate,
}

enum ResponseRemainderGate {
    Uninitialized,
    Waiting(Box<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>),
    Completed,
}

impl ActiveExchangeGuard {
    fn new(session_runtime: Option<SessionRuntimeHarness>, lease: &ConnectionLease) -> Self {
        let correlation = session_runtime
            .as_ref()
            .map(SessionRuntimeHarness::next_correlation)
            .unwrap_or(0);
        Self {
            session_runtime,
            connection: lease.session_connection_identity(),
            lease: lease.session_lease_identity(),
            correlation,
            released: false,
            response_remainder_gate: ResponseRemainderGate::Uninitialized,
        }
    }

    fn release(&mut self, reusable: bool) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(harness) = &self.session_runtime {
            let phase = if reusable {
                SessionPhase::PoolReleaseClean
            } else {
                SessionPhase::PoolReleaseDirty
            };
            let checkpoint =
                harness.checkpoint(phase, self.connection, self.lease, self.correlation);
            harness.observe(checkpoint);
        }
    }

    fn poll_response_remainder(&mut self, context: &mut Context<'_>) -> Poll<()> {
        let Some(harness) = &self.session_runtime else {
            return Poll::Ready(());
        };
        if matches!(
            self.response_remainder_gate,
            ResponseRemainderGate::Uninitialized
        ) {
            let checkpoint = harness.checkpoint(
                SessionPhase::ResponseRemainder,
                self.connection,
                self.lease,
                self.correlation,
            );
            self.response_remainder_gate =
                ResponseRemainderGate::Waiting(Box::new(harness.wait_future(checkpoint)));
        }
        match &mut self.response_remainder_gate {
            ResponseRemainderGate::Waiting(wait) => match wait.as_mut().as_mut().poll(context) {
                Poll::Ready(()) => {
                    self.response_remainder_gate = ResponseRemainderGate::Completed;
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            },
            ResponseRemainderGate::Completed => Poll::Ready(()),
            ResponseRemainderGate::Uninitialized => unreachable!(),
        }
    }
}

const MAX_RESPONSE_HEAD_BYTES: usize = 8192 + 4096 * 100;
const MAX_RESPONSE_HEADERS: usize = 100;

#[derive(Clone, Default)]
struct ResponseHeadObservation(Arc<Mutex<ResponseHeadState>>);

#[derive(Default)]
struct ResponseHeadState {
    generation: u64,
    active: bool,
    captured: Vec<u8>,
    final_head_start: usize,
    final_head_consumed: usize,
    content_length_normalization: ContentLengthNormalization,
    conflicting_content_length: Option<String>,
    raw_headers: Vec<(String, Vec<u8>)>,
    python_header_names: Option<HashMap<String, String>>,
}

#[derive(Clone, Copy, Default)]
enum ContentLengthNormalization {
    #[default]
    None,
    Ignore,
    Overflow,
}

impl ResponseHeadObservation {
    fn begin(&self, python_header_names: Option<Vec<String>>) {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        let generation = state.generation.wrapping_add(1);
        *state = ResponseHeadState {
            generation,
            active: true,
            captured: Vec::new(),
            final_head_start: 0,
            final_head_consumed: 0,
            content_length_normalization: ContentLengthNormalization::None,
            conflicting_content_length: None,
            raw_headers: Vec::new(),
            python_header_names: python_header_names.map(|names| {
                names
                    .into_iter()
                    .map(|name| (name.to_ascii_lowercase(), name))
                    .collect()
            }),
        };
    }

    fn observe(&self, bytes: &[u8]) {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        if !state.active {
            return;
        }
        let remaining = MAX_RESPONSE_HEAD_BYTES.saturating_sub(state.captured.len());
        state
            .captured
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        inspect_complete_response_head(&mut state);
    }

    fn buffers_python_response_head(&self) -> bool {
        let state = self.0.lock().expect("response-head observer lock poisoned");
        state.active && state.python_header_names.is_some()
    }

    fn buffer_python_response_bytes(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        let remaining = MAX_RESPONSE_HEAD_BYTES.saturating_sub(state.captured.len());
        if bytes.len() > remaining {
            let mut passthrough = std::mem::take(&mut state.captured);
            passthrough.extend_from_slice(bytes);
            state.active = false;
            state.python_header_names = None;
            return Some(passthrough);
        }
        state.captured.extend_from_slice(bytes);
        inspect_complete_response_head(&mut state);
        take_normalized_python_response(&mut state)
    }

    fn complete_python_response_on_eof(&self) -> Option<Vec<u8>> {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        if !state.active || state.captured.is_empty() {
            return None;
        }
        let suffix = if state.captured.ends_with(b"\r\n") {
            b"\r\n".as_slice()
        } else {
            b"\r\n\r\n".as_slice()
        };
        if state.captured.len() + suffix.len() > MAX_RESPONSE_HEAD_BYTES {
            return None;
        }
        state.captured.extend_from_slice(suffix);
        inspect_complete_response_head(&mut state);
        take_normalized_python_response(&mut state)
    }

    fn complete_on_eof(&self) -> Option<Vec<u8>> {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        if !state.active || state.captured.is_empty() {
            return None;
        }
        let suffix = if state.captured.ends_with(b"\r\n") {
            b"\r\n".as_slice()
        } else {
            b"\r\n\r\n".as_slice()
        };
        if state.captured.len() + suffix.len() > MAX_RESPONSE_HEAD_BYTES {
            return None;
        }
        let mut completed = state.captured.clone();
        completed.extend_from_slice(suffix);
        let parsed = parsed_response_head(&completed)?;
        if (100..200).contains(&parsed.status) && parsed.status != 101 {
            return None;
        }
        state.conflicting_content_length = parsed.conflicting_content_length;
        state.raw_headers = parsed.raw_headers;
        state.active = false;
        Some(suffix.to_vec())
    }

    fn conflicting_content_length(&self) -> Option<String> {
        self.0
            .lock()
            .expect("response-head observer lock poisoned")
            .conflicting_content_length
            .clone()
    }

    fn take_raw_headers(&self) -> Vec<(String, Vec<u8>)> {
        let mut state = self.0.lock().expect("response-head observer lock poisoned");
        state.captured = Vec::new();
        state.python_header_names = None;
        std::mem::take(&mut state.raw_headers)
    }

    fn generation(&self) -> u64 {
        self.0
            .lock()
            .expect("response-head observer lock poisoned")
            .generation
    }

    fn rewrite_python_request_head(&self, bytes: &mut Vec<u8>) {
        let state = self.0.lock().expect("response-head observer lock poisoned");
        let Some(names) = &state.python_header_names else {
            return;
        };
        let Some(head_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            return;
        };
        let Some(first_line_end) = bytes[..head_end]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            return;
        };
        let request_line = bytes[..first_line_end].to_vec();
        let tail = bytes[head_end + 4..].to_vec();
        let mut headers = Vec::new();
        let mut start = first_line_end + 2;
        while start < head_end {
            let Some(relative_end) = bytes[start..head_end + 2]
                .windows(2)
                .position(|window| window == b"\r\n")
            else {
                break;
            };
            let end = start + relative_end;
            let mut line = bytes[start..end].to_vec();
            let mut host = false;
            if let Some(colon) = line.iter().position(|byte| *byte == b':') {
                let lower = String::from_utf8_lossy(&line[..colon]).to_ascii_lowercase();
                host = lower == "host" && !names.contains_key("host");
                let spelling = names
                    .get(&lower)
                    .cloned()
                    .unwrap_or_else(|| python_default_header_spelling(&lower));
                if spelling.len() == colon {
                    line[..colon].copy_from_slice(spelling.as_bytes());
                }
            }
            headers.push((host, line));
            start = end + 2;
        }
        let mut rewritten = Vec::with_capacity(bytes.len());
        rewritten.extend_from_slice(&request_line);
        rewritten.extend_from_slice(b"\r\n");
        for (_, line) in headers.iter().filter(|(host, _)| *host) {
            rewritten.extend_from_slice(line);
            rewritten.extend_from_slice(b"\r\n");
        }
        for (_, line) in headers.iter().filter(|(host, _)| !*host) {
            rewritten.extend_from_slice(line);
            rewritten.extend_from_slice(b"\r\n");
        }
        rewritten.extend_from_slice(b"\r\n");
        rewritten.extend_from_slice(&tail);
        *bytes = rewritten;
    }
}

struct ParsedResponseHead {
    consumed: usize,
    status: u16,
    content_length_normalization: ContentLengthNormalization,
    conflicting_content_length: Option<String>,
    raw_headers: Vec<(String, Vec<u8>)>,
}

fn parsed_response_head(bytes: &[u8]) -> Option<ParsedResponseHead> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_RESPONSE_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    let httparse::Status::Complete(consumed) = response.parse(bytes).ok()? else {
        return None;
    };
    let status = response.code?;
    let mut content_lengths = Vec::new();
    let mut content_length_values = Vec::new();
    let mut invalid_content_length = false;
    let mut overflowing_content_length = false;
    let mut chunked = false;
    let raw_headers = response
        .headers
        .iter()
        .map(|header| (header.name.to_owned(), header.value.to_vec()))
        .collect();
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {
            content_length_values.push(String::from_utf8_lossy(header.value).into_owned());
            for token in header.value.split(|byte| *byte == b',') {
                let token = trim_ascii(token);
                if token.is_empty() || !token.iter().all(u8::is_ascii_digit) {
                    invalid_content_length = true;
                    continue;
                }
                let canonical = trim_leading_ascii_zeroes(token);
                overflowing_content_length |= std::str::from_utf8(canonical)
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_none();
                content_lengths.push(canonical.to_vec());
            }
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked |= header
                .value
                .split(|byte| *byte == b',')
                .any(|coding| trim_ascii(coding).eq_ignore_ascii_case(b"chunked"));
        }
    }
    let conflicting = !invalid_content_length
        && !chunked
        && content_lengths
            .first()
            .is_some_and(|first| content_lengths.iter().any(|value| value != first));
    let conflicting_content_length = conflicting.then(|| {
        format!(
            "Content-Length contained multiple unmatching values ({})",
            content_length_values.join(", ")
        )
    });
    let content_length_normalization = if invalid_content_length {
        ContentLengthNormalization::Ignore
    } else if overflowing_content_length && !conflicting {
        if chunked {
            ContentLengthNormalization::Ignore
        } else {
            ContentLengthNormalization::Overflow
        }
    } else {
        ContentLengthNormalization::None
    };
    Some(ParsedResponseHead {
        consumed,
        status,
        content_length_normalization,
        conflicting_content_length,
        raw_headers,
    })
}

fn inspect_complete_response_head(state: &mut ResponseHeadState) {
    let mut start = 0;
    while let Some(parsed) = parsed_response_head(&state.captured[start..]) {
        if (100..200).contains(&parsed.status) && parsed.status != 101 {
            start += parsed.consumed;
            continue;
        }
        state.final_head_start = start;
        state.final_head_consumed = parsed.consumed;
        state.content_length_normalization = parsed.content_length_normalization;
        state.conflicting_content_length = parsed.conflicting_content_length;
        state.raw_headers = parsed.raw_headers;
        state.active = false;
        return;
    }
}

fn take_normalized_python_response(state: &mut ResponseHeadState) -> Option<Vec<u8>> {
    if state.active {
        return None;
    }
    let captured = std::mem::take(&mut state.captured);
    match state.content_length_normalization {
        ContentLengthNormalization::None => Some(captured),
        normalization => {
            let start = state.final_head_start;
            let end = start + state.final_head_consumed;
            let final_head = &captured[start..end];
            let first_line_end = final_head.windows(2).position(|window| window == b"\r\n")?;
            let mut normalized = Vec::with_capacity(captured.len());
            normalized.extend_from_slice(&captured[..start]);
            normalized.extend_from_slice(&final_head[..first_line_end + 2]);
            let mut wrote_overflow = false;
            for (name, value) in &state.raw_headers {
                if name.eq_ignore_ascii_case("content-length") {
                    if matches!(normalization, ContentLengthNormalization::Overflow)
                        && !wrote_overflow
                    {
                        // Hyper rejects larger lengths at the response-head boundary. This
                        // framing-only sentinel still guarantees an incomplete body; the Python
                        // adapter reports the original, arbitrarily large decimal value.
                        normalized.extend_from_slice(b"Content-Length: 9223372036854775807\r\n");
                        wrote_overflow = true;
                    }
                    continue;
                }
                normalized.extend_from_slice(name.as_bytes());
                normalized.extend_from_slice(b": ");
                normalized.extend_from_slice(value);
                normalized.extend_from_slice(b"\r\n");
            }
            normalized.extend_from_slice(b"\r\n");
            normalized.extend_from_slice(&captured[end..]);
            Some(normalized)
        }
    }
}

fn python_default_header_spelling(lower: &str) -> String {
    lower
        .split('-')
        .map(|part| {
            let mut bytes = part.as_bytes().to_vec();
            if let Some(first) = bytes.first_mut() {
                first.make_ascii_uppercase();
            }
            String::from_utf8(bytes).expect("HTTP header names are ASCII")
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn trim_leading_ascii_zeroes(bytes: &[u8]) -> &[u8] {
    let first_nonzero = bytes
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(bytes.len().saturating_sub(1));
    &bytes[first_nonzero..]
}

struct ResponseHeadIo<IO> {
    inner: IO,
    observation: ResponseHeadObservation,
    injected: VecDeque<u8>,
    outgoing: Vec<u8>,
    outgoing_offset: usize,
    outgoing_head_complete: bool,
    generation: u64,
}

impl<IO> ResponseHeadIo<IO> {
    fn new(inner: IO, observation: ResponseHeadObservation) -> Self {
        Self {
            inner,
            observation,
            injected: VecDeque::new(),
            outgoing: Vec::new(),
            outgoing_offset: 0,
            outgoing_head_complete: false,
            generation: 0,
        }
    }

    fn fill_injected(&mut self, buffer: &mut ReadBuf<'_>) {
        let amount = buffer.remaining().min(self.injected.len());
        let bytes = self.injected.drain(..amount).collect::<Vec<_>>();
        buffer.put_slice(&bytes);
    }

    fn poll_drain_outgoing(&mut self, context: &mut Context<'_>) -> Poll<io::Result<()>>
    where
        IO: AsyncWrite + Unpin,
    {
        while self.outgoing_offset < self.outgoing.len() {
            match Pin::new(&mut self.inner)
                .poll_write(context, &self.outgoing[self.outgoing_offset..])
            {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(written)) => self.outgoing_offset += written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        if self.outgoing_head_complete {
            self.outgoing = Vec::new();
        } else {
            self.outgoing.clear();
        }
        self.outgoing_offset = 0;
        Poll::Ready(Ok(()))
    }
}

impl<IO: AsyncRead + Unpin> AsyncRead for ResponseHeadIo<IO> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let generation = this.observation.generation();
        if generation != this.generation {
            debug_assert_eq!(this.outgoing_offset, this.outgoing.len());
            this.outgoing = Vec::new();
            this.outgoing_offset = 0;
            this.outgoing_head_complete = false;
            this.generation = generation;
        }
        if !this.injected.is_empty() {
            this.fill_injected(buffer);
            return Poll::Ready(Ok(()));
        }
        if this.observation.buffers_python_response_head() {
            let mut bytes = [0_u8; 8192];
            let mut incoming = ReadBuf::new(&mut bytes);
            return match Pin::new(&mut this.inner).poll_read(context, &mut incoming) {
                Poll::Ready(Ok(())) if incoming.filled().is_empty() => {
                    if let Some(injected) = this.observation.complete_python_response_on_eof() {
                        this.injected.extend(injected);
                        this.fill_injected(buffer);
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Ok(())) => {
                    if let Some(injected) = this
                        .observation
                        .buffer_python_response_bytes(incoming.filled())
                    {
                        this.injected.extend(injected);
                        this.fill_injected(buffer);
                        Poll::Ready(Ok(()))
                    } else {
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            };
        }
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                let read = &buffer.filled()[before..];
                if read.is_empty() {
                    if let Some(injected) = this.observation.complete_on_eof() {
                        this.injected.extend(injected);
                        this.fill_injected(buffer);
                    }
                } else {
                    this.observation.observe(read);
                }
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for ResponseHeadIo<IO> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let generation = this.observation.generation();
        if generation != this.generation {
            debug_assert_eq!(this.outgoing_offset, this.outgoing.len());
            this.outgoing = Vec::new();
            this.outgoing_offset = 0;
            this.outgoing_head_complete = false;
            this.generation = generation;
        }
        if this.outgoing_offset < this.outgoing.len() {
            match this.poll_drain_outgoing(context) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        if this.outgoing_head_complete {
            return Pin::new(&mut this.inner).poll_write(context, buffer);
        }
        this.outgoing.extend_from_slice(buffer);
        if this.outgoing.windows(4).any(|window| window == b"\r\n\r\n") {
            this.observation
                .rewrite_python_request_head(&mut this.outgoing);
            this.outgoing_head_complete = true;
        }
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain_outgoing(context) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(context),
            result => result,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain_outgoing(context) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(context),
            result => result,
        }
    }
}

pub(crate) struct ConnectionDriver {
    task: Option<JoinHandle<Result<()>>>,
    shutdown: Option<std::net::TcpStream>,
    response_head: Option<ResponseHeadObservation>,
    #[cfg(test)]
    shutdown_observer: Option<Arc<dyn EstablishmentControl>>,
}

impl ConnectionDriver {
    #[cfg(test)]
    fn spawn(
        task: impl Future<Output = Result<()>> + Send + 'static,
        shutdown: Option<std::net::TcpStream>,
    ) -> Self {
        let mut driver = Self::with_shutdown(shutdown);
        driver.start(task);
        driver
    }

    fn with_shutdown(shutdown: Option<std::net::TcpStream>) -> Self {
        Self {
            task: None,
            shutdown,
            response_head: None,
            #[cfg(test)]
            shutdown_observer: None,
        }
    }

    fn start(&mut self, task: impl Future<Output = Result<()>> + Send + 'static) {
        debug_assert!(self.task.is_none());
        self.task = Some(tokio::spawn(task));
    }

    fn observe_response_head(&mut self, observation: ResponseHeadObservation) {
        self.response_head = Some(observation);
    }

    fn begin_response_head(&self, python_header_names: Option<Vec<String>>) {
        if let Some(observation) = &self.response_head {
            observation.begin(python_header_names);
        }
    }

    fn conflicting_content_length(&self) -> Option<String> {
        self.response_head
            .as_ref()
            .and_then(ResponseHeadObservation::conflicting_content_length)
    }

    fn take_raw_response_headers(&self) -> Vec<(String, Vec<u8>)> {
        self.response_head
            .as_ref()
            .map_or_else(Vec::new, ResponseHeadObservation::take_raw_headers)
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
    fn new(
        pool: Arc<Mutex<Pool>>,
        lease: ConnectionLease,
        session_runtime: Option<SessionRuntimeHarness>,
    ) -> Self {
        let active_exchange = ActiveExchangeGuard::new(session_runtime, &lease);
        Self {
            pool,
            lease: Some(lease),
            active_exchange,
        }
    }

    pub(crate) fn poll_response_remainder(&mut self, context: &mut Context<'_>) -> Poll<()> {
        self.active_exchange.poll_response_remainder(context)
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
        self.release(false, DirtyLeaseCause::Cancellation);
        result
    }

    pub(crate) async fn abort_upload_and_wait(mut self) -> Result<()> {
        let result = self.driver_mut().abort_and_wait().await;
        self.release(false, DirtyLeaseCause::Upload);
        result
    }

    pub(crate) fn finish_now(mut self, reusable: bool) {
        if !reusable {
            self.driver_mut().shutdown_now();
        }
        self.release(reusable, DirtyLeaseCause::Cancellation);
    }

    pub(crate) fn finish_incomplete_body_now(mut self) {
        self.driver_mut().shutdown_now();
        self.release(false, DirtyLeaseCause::IncompleteBody);
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

    fn release(&mut self, reusable: bool, cause: DirtyLeaseCause) {
        let Some(mut lease) = self.lease.take() else {
            return;
        };
        let reusable = reusable && lease.is_live() && lease.peer_is_open();
        self.active_exchange.release(reusable);
        lease = if reusable {
            lease.complete(pool::LeaseTerminal::CleanEof)
        } else {
            lease.complete_dirty(cause)
        };
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
        self.active_exchange.release(false);
        let lease = lease.complete_dirty(DirtyLeaseCause::Cancellation);
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
    #[cfg(test)]
    pub(crate) fn configured(
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
    ) -> Self {
        Self::configured_with_session_runtime(
            proxy,
            tls,
            default_timeout,
            pool_max_idle_per_host,
            content_codecs,
            None,
        )
    }

    pub(crate) fn configured_with_session_runtime(
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
        session_runtime: Option<SessionRuntimeHarness>,
    ) -> Self {
        Self::with_configuration_and_session_runtime(
            Arc::new(DirectConnector {
                session_runtime: session_runtime.clone(),
            }),
            proxy,
            tls,
            default_timeout,
            pool_max_idle_per_host,
            content_codecs,
            session_runtime,
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

    #[cfg(test)]
    fn with_configuration(
        connector: Arc<dyn Connector>,
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
    ) -> Self {
        Self::with_configuration_and_session_runtime(
            connector,
            proxy,
            tls,
            default_timeout,
            pool_max_idle_per_host,
            content_codecs,
            None,
        )
    }

    fn with_configuration_and_session_runtime(
        connector: Arc<dyn Connector>,
        proxy: Option<Proxy>,
        tls: TlsConfig,
        default_timeout: Timeout,
        pool_max_idle_per_host: usize,
        content_codecs: ContentCodecs,
        session_runtime: Option<SessionRuntimeHarness>,
    ) -> Self {
        Self {
            pool: Arc::new(Mutex::new(Pool::new_with_session_runtime(
                pool_max_idle_per_host,
                session_runtime.clone(),
            ))),
            #[cfg(test)]
            derived_pool_keys: Mutex::new(Vec::new()),
            #[cfg(test)]
            establishment_control: None,
            connector,
            proxy,
            tls,
            default_timeout,
            content_codecs,
            session_runtime,
        }
    }

    pub(crate) fn clear_pool(&self) {
        let evicted = {
            self.pool
                .lock()
                .expect("transport pool lock poisoned")
                .clear()
        };
        if let Some(harness) = &self.session_runtime {
            if evicted.is_empty() {
                let checkpoint = harness.checkpoint(
                    SessionPhase::PoolClear,
                    None,
                    None,
                    harness.next_correlation(),
                );
                harness.observe(checkpoint);
            } else {
                for connection in &evicted {
                    let checkpoint = harness.checkpoint(
                        SessionPhase::PoolClear,
                        connection.session_identity(),
                        None,
                        harness.next_correlation(),
                    );
                    harness.observe(checkpoint);
                }
            }
        }
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
        let timeout = request.timeout().unwrap_or(self.default_timeout);
        let started = Instant::now();
        validate_request(&request)?;
        let host = request
            .uri()
            .host()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .to_owned();
        let scheme = request
            .uri()
            .scheme()
            .cloned()
            .ok_or_else(|| Error::invalid_url(request.url()))?;
        let port = request
            .uri()
            .port_u16()
            .unwrap_or(if scheme == http::uri::Scheme::HTTPS {
                443
            } else {
                80
            });
        let authority = request
            .uri()
            .authority()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .clone();
        let target = authority.as_str().to_owned();
        let (tls, identity) = if scheme == http::uri::Scheme::HTTPS {
            (
                TlsPoolKey::from_config(&self.tls),
                self.tls.identity.as_ref().map(IdentityKey::from_identity),
            )
        } else {
            (TlsPoolKey::plain(), None)
        };
        let proxy_key = self.proxy.as_ref().map(ProxyKey::from_proxy);
        let target_is_https = scheme == http::uri::Scheme::HTTPS;
        let absolute_form = proxy::uses_absolute_form(self.proxy.as_ref(), target_is_https);
        let key = PoolKey::new(scheme, authority, proxy_key, tls, identity);
        #[cfg(test)]
        self.derived_pool_keys
            .lock()
            .expect("derived pool-key observation lock poisoned")
            .push(key.clone());
        let mut request = request.into_parts();
        let python_header_names = request.python_header_names.clone();
        if absolute_form
            && let Some(proxy) = &self.proxy
            && let Some(authorization) = proxy::authorization(proxy)?
        {
            request.headers.insert(PROXY_AUTHORIZATION, authorization);
        }
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
        let mut lease = self
            .acquire_connection(key, &host, port, &target, establishment_deadlines)
            .await?;
        let track_body_completion = read_timeout.is_some() || total_timeout.is_some();
        let upload_correlation = self
            .session_runtime
            .as_ref()
            .map(SessionRuntimeHarness::next_correlation)
            .unwrap_or(0);
        let upload_identity = lease.session_exchange_identity(upload_correlation);
        let (outgoing, url, mut body_completion) = outgoing_request_with_session_runtime(
            request,
            track_body_completion,
            absolute_form,
            self.session_runtime.clone(),
            upload_identity,
        )?;
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
            let session_connection_identity = lease.session_connection_identity();
            let session_lease_identity = lease.session_lease_identity();
            let (sender, driver) = lease.connection_mut().network_parts_mut();
            driver.begin_response_head(python_header_names);
            let response_head_correlation = self
                .session_runtime
                .as_ref()
                .map(SessionRuntimeHarness::begin_request_observation);
            let sending = sender.send_request(outgoing);
            tokio::pin!(sending);
            let mut head_wait = ResponseHeadWaitEntered::default();
            let response_head_gate = async {
                if let Some(harness) = &self.session_runtime {
                    let correlation = response_head_correlation
                        .expect("session request observation registered before send");
                    harness.wait_request_observed(correlation).await;
                    let checkpoint = harness.checkpoint(
                        SessionPhase::ResponseHead,
                        session_connection_identity,
                        session_lease_identity,
                        correlation,
                    );
                    harness.wait(checkpoint).await;
                }
            };
            tokio::pin!(response_head_gate);
            let result = loop {
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
                        () = &mut response_head_gate, if !head_wait.permits_post_head() => {
                            ExchangeEvent::ResponseHeadGate
                        }
                        result = &mut sending => ExchangeEvent::Response(result),
                        completed_at = wait_for_body_completion(&mut body_completion),
                            if completion_pending => {
                            ExchangeEvent::UploadComplete(completed_at)
                        }
                        driver_result = &mut driver_wait => ExchangeEvent::Driver(driver_result),
                    }
                };
                match event {
                    ExchangeEvent::ResponseHeadGate => {
                        head_wait.enter();
                    }
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
                    ExchangeEvent::Response(result) => {
                        debug_assert!(head_wait.permits_post_head());
                        break match result {
                            Ok(response) => Ok((response, driver.take_raw_response_headers())),
                            Err(error) => {
                                if error.is_parse() {
                                    if let Some(message) = driver.conflicting_content_length() {
                                        Err(Error::invalid_response_header(message))
                                    } else {
                                        Err(Error::send_hyper(error))
                                    }
                                } else {
                                    Err(Error::send_hyper(error))
                                }
                            }
                        };
                    }
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
            };
            result.map_err(|error| (error, completion_pending))
        };
        let response = match response {
            Ok(response) => response,
            Err((send_error, upload_pending)) => {
                let cleanup_lease = TransportLease::new(
                    Arc::clone(&self.pool),
                    lease,
                    self.session_runtime.clone(),
                );
                let cleanup = if upload_pending {
                    cleanup_lease.abort_upload_and_wait().await
                } else {
                    cleanup_lease.abort_and_wait().await
                };
                return match cleanup {
                    Ok(()) => Err(send_error),
                    Err(driver_error) => Err(Error::with_cleanup(send_error, driver_error)),
                };
            }
        };
        let (response, raw_headers) = response;
        let (head, body) = response.into_parts();

        Ok(TransportResponse {
            head,
            body,
            url,
            lease: TransportLease::new(Arc::clone(&self.pool), lease, self.session_runtime.clone()),
            read_timeout,
            total_timeout,
            total_deadline,
            content_codecs: self.content_codecs,
            raw_headers,
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
                TransportLease::new(Arc::clone(&self.pool), lease, self.session_runtime.clone())
                    .finish_now(false);
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
                TransportLease::new(Arc::clone(&self.pool), lease, self.session_runtime.clone())
                    .finish_now(false);
                return Err(Error::request_exchange_total_timeout(
                    deadlines
                        .total_timeout
                        .expect("total deadline exists only when timeout is configured"),
                ));
            };
            if ready.is_ok() && lease.is_live() && lease.peer_is_open() {
                self.observe_pool_acquire(&lease);
                return Ok(lease);
            }
            TransportLease::new(Arc::clone(&self.pool), lease, self.session_runtime.clone())
                .finish_now(false);
        }

        let lease = self
            .connect_connection(key, host, port, target, deadlines)
            .await?;
        self.observe_pool_acquire(&lease);
        Ok(lease)
    }

    fn observe_pool_acquire(&self, lease: &ConnectionLease) {
        if let Some(harness) = &self.session_runtime {
            let checkpoint = harness.checkpoint(
                SessionPhase::PoolAcquire,
                lease.session_connection_identity(),
                lease.session_lease_identity(),
                harness.next_correlation(),
            );
            harness.observe(checkpoint);
        }
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
        let reservation = self
            .session_runtime
            .as_ref()
            .map(SessionRuntimeHarness::reserve_exchange);
        let establishing = async {
            let tls = self.tls.clone();
            let proxy = self.proxy.clone();
            #[cfg(test)]
            let establishment_control = self.establishment_control.as_ref().map(Arc::clone);
            #[cfg(test)]
            let native_root_loader = self
                .establishment_control
                .as_ref()
                .and_then(|control| control.native_root_loader());
            #[cfg(not(test))]
            let native_root_loader = None;
            let target_is_https = key.scheme == http::uri::Scheme::HTTPS;
            let proxy_needs_tls = proxy.as_ref().is_some_and(proxy::needs_tls);
            let (loaded_tls, proxy_tls) = if target_is_https || proxy_needs_tls {
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(control) = &establishment_control {
                        control.blocking_load_started();
                    }
                    let target = target_is_https
                        .then(|| tls::load(&tls, native_root_loader.clone()))
                        .transpose();
                    let proxy = proxy_needs_tls
                        .then(|| {
                            tls::load(
                                &TlsConfig {
                                    roots: tls.roots.clone(),
                                    identity: None,
                                },
                                native_root_loader,
                            )
                        })
                        .transpose();
                    let result = target.and_then(|target| proxy.map(|proxy| (target, proxy)));
                    #[cfg(test)]
                    if let Some(control) = &establishment_control {
                        control.blocking_load_finished(result.is_ok());
                    }
                    result
                })
                .await
                .map_err(Error::tls)??
            } else {
                (None, None)
            };
            #[cfg(test)]
            self.establishment_checkpoint(EstablishmentStage::Connect)
                .await;
            let endpoint = match &proxy {
                Some(proxy) => proxy::endpoint(proxy)?,
                None => proxy::Endpoint {
                    host: host.to_owned(),
                    port,
                },
            };
            let stream = self
                .connector
                .connect_session(
                    &endpoint.host,
                    endpoint.port,
                    target,
                    reservation
                        .as_ref()
                        .map(|reservation| reservation.checkpoint(SessionPhase::ConnectBlocked)),
                )
                .await?;
            let stream = stream
                .into_std()
                .map_err(|error| Error::connect(target, error))?;
            let shutdown = stream
                .try_clone()
                .map_err(|error| Error::connect(target, error))?;
            let stream = tokio::net::TcpStream::from_std(stream)
                .map_err(|error| Error::connect(target, error))?;
            let driver = ConnectionDriver::with_shutdown(Some(shutdown));
            #[cfg(test)]
            let driver =
                driver.with_shutdown_observer(self.establishment_control.as_ref().map(Arc::clone));
            let stream = match &proxy {
                Some(proxy) => {
                    proxy::establish(proxy, stream, proxy_tls, host, port, target_is_https).await?
                }
                None => proxy::ProxyStream::Plain(stream),
            };
            let connection_identity = reservation
                .as_ref()
                .map(|reservation| reservation.checkpoint(SessionPhase::ConnectBlocked))
                .and_then(|checkpoint| checkpoint.connection);
            let connection = match loaded_tls {
                Some(loaded_tls) => {
                    #[cfg(test)]
                    self.establishment_checkpoint(EstablishmentStage::Tls).await;
                    let stream = tls::handshake(loaded_tls, host, stream).await?;
                    #[cfg(test)]
                    self.establishment_checkpoint(EstablishmentStage::Http1)
                        .await;
                    start_http1(stream, driver, connection_identity).await?
                }
                None => {
                    #[cfg(test)]
                    self.establishment_checkpoint(EstablishmentStage::Http1)
                        .await;
                    start_http1(stream, driver, connection_identity).await?
                }
            };
            match reservation {
                Some(reservation) => Ok(ConnectionLease::new_with_exchange_identity(
                    key,
                    generation,
                    connection,
                    reservation.promote(),
                )),
                None => Ok(ConnectionLease::new(key, generation, connection)),
            }
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

async fn start_http1<IO>(
    stream: IO,
    mut driver: ConnectionDriver,
    session_identity: Option<SessionConnectionIdentity>,
) -> Result<IdleConnection>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let observation = ResponseHeadObservation::default();
    let stream = ResponseHeadIo::new(stream, observation.clone());
    let (sender, connection) = http1::Builder::new()
        .handshake(TokioIo::new(stream))
        .await
        .map_err(Error::handshake)?;
    driver.observe_response_head(observation);
    driver.start(async move { connection.await.map_err(Error::connection_hyper) });
    Ok(IdleConnection::network(sender, driver, session_identity))
}

fn validate_request(request: &Request) -> Result<()> {
    if !matches!(request.uri().scheme_str(), Some("http" | "https")) {
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

#[cfg(test)]
fn outgoing_request(
    request: RequestParts,
    track_body_completion: bool,
    absolute_form: bool,
) -> Result<(http::Request<OutgoingBody>, String, Option<BodyCompletion>)> {
    outgoing_request_with_session_runtime(request, track_body_completion, absolute_form, None, None)
}

fn outgoing_request_with_session_runtime(
    mut request: RequestParts,
    track_body_completion: bool,
    absolute_form: bool,
    session_runtime: Option<SessionRuntimeHarness>,
    session_identity: Option<SessionExchangeIdentity>,
) -> Result<(http::Request<OutgoingBody>, String, Option<BodyCompletion>)> {
    let request_target = if absolute_form {
        let mut url =
            url::Url::parse(&request.url).map_err(|_| Error::invalid_url(&request.url))?;
        url.set_fragment(None);
        url.set_username("")
            .map_err(|_| Error::invalid_url(&request.url))?;
        url.set_password(None)
            .map_err(|_| Error::invalid_url(&request.url))?;
        url.as_str()
            .parse::<http::Uri>()
            .map_err(|_| Error::invalid_url(&request.url))?
    } else {
        request
            .uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str)
            .parse::<http::Uri>()
            .map_err(|_| Error::invalid_url(&request.url))?
    };
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

    let (body, completion) = OutgoingBody::new(
        request.body,
        track_body_completion,
        session_runtime,
        session_identity,
    );
    let mut outgoing = http::Request::new(body);
    *outgoing.method_mut() = request.method;
    *outgoing.uri_mut() = request_target;
    *outgoing.headers_mut() = request.headers;
    Ok((outgoing, request.url, completion))
}

struct OutgoingBody {
    source: BodySource,
    progress: Option<Arc<BodyProgress>>,
    upload_entered: OriginUploadActionEntered,
    upload_counts: UploadQueuedExecutedReplyCounts,
    session_runtime: Option<SessionRuntimeHarness>,
    session_identity: Option<SessionExchangeIdentity>,
    upload_wait: Option<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    upload_wait_complete: bool,
}

impl OutgoingBody {
    fn new(
        source: BodySource,
        track_completion: bool,
        session_runtime: Option<SessionRuntimeHarness>,
        session_identity: Option<SessionExchangeIdentity>,
    ) -> (Self, Option<BodyCompletion>) {
        let initially_complete = match &source {
            BodySource::Empty => true,
            BodySource::Bytes(bytes) => bytes.is_empty(),
            BodySource::Stream(_) => false,
        };
        let progress = track_completion.then(|| Arc::new(BodyProgress::new(initially_complete)));
        let completion = progress.as_ref().map(|progress| BodyCompletion {
            progress: Arc::clone(progress),
        });
        let upload_counts = UploadQueuedExecutedReplyCounts::default();
        if matches!(&source, BodySource::Stream(_))
            && let (Some(harness), Some(identity)) = (&session_runtime, session_identity)
        {
            let checkpoint = harness.checkpoint(
                SessionPhase::OriginUploadQueued,
                Some(identity.connection),
                Some(identity.lease),
                identity.correlation,
            );
            harness.observe(checkpoint);
        }
        (
            Self {
                source,
                progress,
                upload_entered: OriginUploadActionEntered::default(),
                upload_counts,
                session_runtime,
                session_identity,
                upload_wait: None,
                upload_wait_complete: false,
            },
            completion,
        )
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
            BodySource::Stream(mut stream) => {
                if !body.upload_wait_complete && body.upload_wait.is_none() {
                    body.upload_counts.queued();
                    body.upload_entered.enter();
                    body.upload_counts.executed();
                }
                if !body.upload_wait_complete
                    && let (Some(harness), Some(identity)) =
                        (&body.session_runtime, body.session_identity)
                {
                    let wait = body.upload_wait.get_or_insert_with(|| {
                        let checkpoint = harness.checkpoint(
                            SessionPhase::OriginUploadExecuted,
                            Some(identity.connection),
                            Some(identity.lease),
                            identity.correlation,
                        );
                        harness.wait_future(checkpoint)
                    });
                    if wait.as_mut().poll(context).is_pending() {
                        body.source = BodySource::Stream(stream);
                        return Poll::Pending;
                    }
                    body.upload_wait_complete = true;
                    body.upload_wait.take();
                }
                debug_assert!(body.upload_entered.is_entered());
                let result = match stream.as_mut().poll_next(context) {
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
                };
                if result.is_ready() {
                    body.upload_counts.reply_observed();
                    if let (Some(harness), Some(identity)) =
                        (&body.session_runtime, body.session_identity)
                    {
                        let checkpoint = harness.checkpoint(
                            SessionPhase::OriginUploadReply,
                            Some(identity.connection),
                            Some(identity.lease),
                            identity.correlation,
                        );
                        harness.observe(checkpoint);
                    }
                }
                debug_assert!(body.upload_counts.is_consistent());
                result
            }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use bytes::Bytes;
    use http::{HeaderName, HeaderValue, Method};

    use super::{
        ActiveExchangeGuard, ConnectionDriver, MAX_RESPONSE_HEAD_BYTES, ResponseHeadIo,
        ResponseHeadObservation, ResponseRemainderGate, outgoing_request, validate_request,
    };
    use crate::session_runtime::{SessionCheckpoint, SessionRuntimeHarness, SessionRuntimeHooks};
    use crate::{AsyncBody, BodySource, ErrorKind, RequestBuilder};

    struct NeverBody;

    struct ImmediateRemainderHooks(Arc<AtomicUsize>);

    impl SessionRuntimeHooks for ImmediateRemainderHooks {
        fn checkpoint(&self, _checkpoint: SessionCheckpoint) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

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
    fn completed_response_remainder_gate_is_not_repolled() {
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let runtime =
            SessionRuntimeHarness::new(Arc::new(ImmediateRemainderHooks(Arc::clone(&checkpoints))));
        let mut guard = ActiveExchangeGuard {
            session_runtime: Some(runtime),
            connection: None,
            lease: None,
            correlation: 1,
            released: false,
            response_remainder_gate: ResponseRemainderGate::Uninitialized,
        };
        let mut context = Context::from_waker(Waker::noop());

        assert_eq!(guard.poll_response_remainder(&mut context), Poll::Ready(()));
        assert_eq!(guard.poll_response_remainder(&mut context), Poll::Ready(()));
        assert_eq!(checkpoints.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn connection_driver_shutdown_handle_is_a_raw_standard_tcp_stream() {
        let driver = ConnectionDriver {
            task: None,
            shutdown: None,
            response_head: None,
            shutdown_observer: None,
        };
        let ConnectionDriver {
            task,
            shutdown,
            response_head,
            shutdown_observer,
        } = &driver;
        let _: &Option<std::net::TcpStream> = shutdown;
        assert!(task.is_none());
        assert!(shutdown_observer.is_none());
        assert!(response_head.is_none());
    }

    #[test]
    fn eof_completes_only_a_valid_partial_response_head() {
        let observation = ResponseHeadObservation::default();
        observation.begin(None);
        observation.observe(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n");

        assert_eq!(observation.complete_on_eof(), Some(b"\r\n".to_vec()));
        assert_eq!(observation.conflicting_content_length(), None);

        let empty = ResponseHeadObservation::default();
        empty.begin(None);
        assert_eq!(empty.complete_on_eof(), None);
    }

    #[test]
    fn conflicting_numeric_content_lengths_are_observed_unless_chunked_wins() {
        let conflict = ResponseHeadObservation::default();
        conflict.begin(None);
        conflict.observe(b"HTTP/1.1 200 OK\r\nContent-Length: 016\r\nContent-Length: 32\r\n\r\n");
        assert_eq!(
            conflict.conflicting_content_length().as_deref(),
            Some("Content-Length contained multiple unmatching values (016, 32)")
        );

        let chunked = ResponseHeadObservation::default();
        chunked.begin(None);
        chunked.observe(
            b"HTTP/1.1 200 OK\r\nContent-Length: 16, 32\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        assert_eq!(chunked.conflicting_content_length(), None);
    }

    #[test]
    fn completed_observer_transfers_headers_and_discards_large_buffers() {
        let observation = ResponseHeadObservation::default();
        observation.begin(Some(Vec::new()));
        let mut response = Vec::with_capacity(MAX_RESPONSE_HEAD_BYTES / 2);
        response.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\nx");

        assert_eq!(
            observation.buffer_python_response_bytes(&response),
            Some(b"HTTP/1.1 200 OK\r\n\r\nx".to_vec())
        );
        assert_eq!(
            observation.take_raw_headers(),
            vec![("Content-Length".to_owned(), b"nope".to_vec())]
        );
        {
            let state = observation
                .0
                .lock()
                .expect("response-head observer lock poisoned");
            assert_eq!(state.captured.capacity(), 0);
            assert!(state.raw_headers.is_empty());
            assert!(state.python_header_names.is_none());
        }
        observation.begin(Some(Vec::new()));
        assert!(observation.take_raw_headers().is_empty());

        let mut io = ResponseHeadIo::new(tokio::io::sink(), ResponseHeadObservation::default());
        io.outgoing = Vec::with_capacity(MAX_RESPONSE_HEAD_BYTES);
        io.outgoing_head_complete = true;
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            io.poll_drain_outgoing(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(io.outgoing.capacity(), 0);
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
            production
                .matches(".handshake(TokioIo::new(stream))")
                .count(),
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

        let (outgoing, url, completion) =
            outgoing_request(request.into_parts(), false, false).unwrap();

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

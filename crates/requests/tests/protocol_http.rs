use std::collections::VecDeque;
use std::io::{Read, Write};
use std::marker::PhantomPinned;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use requests::{
    AsyncBody, BodySource, Client, ErrorKind, HeaderName, HeaderValue, Method, RequestBuilder,
    StatusCode,
};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const POST_EXCHANGE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const SCRIPTED_RESPONSE: &[u8] =
    b"HTTP/1.1 201 Created\r\nx-fixture: direct\r\nContent-Length: 7\r\n\r\ndirect\n";
const CONNECTION_CLOSE_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const FRAMING_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nx-fixture: framing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const HEAD_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nx-fixture: framing\r\nContent-Length: 7\r\nConnection: close\r\n\r\n";

#[derive(Debug)]
struct Observation {
    accepted_connections: usize,
    request_bytes: Vec<u8>,
}

struct ScriptedServer {
    address: SocketAddr,
    shutdown: Option<Sender<()>>,
    worker: Option<JoinHandle<Result<Observation, String>>>,
}

impl ScriptedServer {
    fn spawn() -> Self {
        Self::spawn_with_response(SCRIPTED_RESPONSE)
    }

    fn spawn_with_response(response: &'static [u8]) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make fixture listener nonblocking");
        let address = listener.local_addr().expect("read fixture address");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let worker = thread::spawn(move || serve(listener, &shutdown_rx, response));

        Self {
            address,
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/direct?source=task10", self.address)
    }

    fn authority(&self) -> String {
        self.address.to_string()
    }

    fn finish(mut self) -> Result<Observation, String> {
        self.signal_shutdown();
        join_worker(self.worker.take())
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        let _ = join_worker(self.worker.take());
    }
}

fn join_worker(
    worker: Option<JoinHandle<Result<Observation, String>>>,
) -> Result<Observation, String> {
    let worker = worker.ok_or_else(|| "fixture worker already joined".to_owned())?;
    worker
        .join()
        .map_err(|_| "fixture worker panicked".to_owned())?
}

fn serve(
    listener: TcpListener,
    shutdown: &Receiver<()>,
    response: &[u8],
) -> Result<Observation, String> {
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("fixture accept failed: {error}")),
        }

        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                return Ok(Observation {
                    accepted_connections: 0,
                    request_bytes: Vec::new(),
                });
            }
            Err(TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("fixture timed out accepting a connection".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    };

    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("set fixture read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("set fixture write timeout: {error}"))?;

    let mut request_bytes = read_complete_request(&mut stream)?;
    stream
        .write_all(response)
        .map_err(|error| format!("write scripted response: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush scripted response: {error}"))?;
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("make accepted stream nonblocking: {error}"))?;

    let mut accepted_connections = 1;
    let mut retained_connections = Vec::new();
    let keep_alive_deadline = Instant::now() + EXCHANGE_TIMEOUT;
    loop {
        drain_request_bytes(&mut stream, &mut request_bytes)?;
        accept_retries(
            &listener,
            &mut retained_connections,
            &mut accepted_connections,
        )?;

        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                drain_request_bytes_to_quiescence(&mut stream, &mut request_bytes)?;
                accept_retries(
                    &listener,
                    &mut retained_connections,
                    &mut accepted_connections,
                )?;
                break;
            }
            Err(TryRecvError::Empty) => {}
        }
        if Instant::now() >= keep_alive_deadline {
            return Err("fixture timed out waiting for shutdown".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }

    drop(stream);
    drop(retained_connections);
    Ok(Observation {
        accepted_connections,
        request_bytes,
    })
}

fn accept_retries(
    listener: &TcpListener,
    retained_connections: &mut Vec<TcpStream>,
    accepted_connections: &mut usize,
) -> Result<(), String> {
    loop {
        match listener.accept() {
            Ok((extra, _)) => {
                *accepted_connections += 1;
                retained_connections.push(extra);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(format!("fixture retry accept failed: {error}")),
        }
    }
}

fn read_complete_request(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];

    loop {
        if complete_request_len(&request)?.is_some() {
            return Ok(request);
        }
        if request.len() >= MAX_REQUEST_BYTES {
            return Err(format!("request exceeded {MAX_REQUEST_BYTES} bytes"));
        }

        let remaining = MAX_REQUEST_BYTES - request.len();
        let read_limit = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..read_limit])
            .map_err(|error| format!("read request: {error}"))?;
        if read == 0 {
            return Err("client closed before completing request".to_owned());
        }
        request.extend_from_slice(&buffer[..read]);
    }
}

fn complete_request_len(request: &[u8]) -> Result<Option<usize>, String> {
    let Some(head_offset) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        if request.len() >= MAX_REQUEST_HEAD_BYTES {
            return Err(format!(
                "request head exceeded {MAX_REQUEST_HEAD_BYTES} bytes"
            ));
        }
        return Ok(None);
    };
    let body_offset = head_offset + 4;
    let head = std::str::from_utf8(&request[..head_offset])
        .map_err(|error| format!("request head was not UTF-8: {error}"))?;
    let mut content_length = None;
    let mut chunked = false;

    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("malformed request header: {line:?}"));
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| format!("invalid Content-Length {value:?}: {error}"))?,
            );
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        {
            chunked = true;
        }
    }

    if chunked {
        return chunked_body_len(&request[body_offset..])
            .map(|length| length.map(|length| body_offset + length));
    }
    let request_len = body_offset + content_length.unwrap_or(0);
    Ok((request.len() >= request_len).then_some(request_len))
}

fn chunked_body_len(body: &[u8]) -> Result<Option<usize>, String> {
    let mut cursor = 0;
    loop {
        let Some(relative_line_end) = body[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            return Ok(None);
        };
        let line_end = cursor + relative_line_end;
        let size_text = std::str::from_utf8(&body[cursor..line_end])
            .map_err(|error| format!("chunk size was not UTF-8: {error}"))?;
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|error| format!("invalid chunk size {size_text:?}: {error}"))?;
        cursor = line_end + 2;

        if size == 0 {
            loop {
                let Some(relative_trailer_end) = body[cursor..]
                    .windows(2)
                    .position(|window| window == b"\r\n")
                else {
                    return Ok(None);
                };
                let trailer_end = cursor + relative_trailer_end;
                cursor = trailer_end + 2;
                if relative_trailer_end == 0 {
                    return Ok(Some(cursor));
                }
            }
        }

        let chunk_end = cursor
            .checked_add(size)
            .ok_or_else(|| "chunk size overflowed usize".to_owned())?;
        if body.len() < chunk_end + 2 {
            return Ok(None);
        }
        if &body[chunk_end..chunk_end + 2] != b"\r\n" {
            return Err("chunk payload was not followed by CRLF".to_owned());
        }
        cursor = chunk_end + 2;
    }
}

fn drain_request_bytes_to_quiescence(
    stream: &mut TcpStream,
    request: &mut Vec<u8>,
) -> Result<(), String> {
    stream
        .set_nonblocking(false)
        .map_err(|error| format!("restore accepted stream blocking mode: {error}"))?;
    stream
        .set_read_timeout(Some(POST_EXCHANGE_READ_TIMEOUT))
        .map_err(|error| format!("set post-exchange read timeout: {error}"))?;

    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => append_request_bytes(request, &buffer[..read])?,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("drain post-exchange request bytes: {error}")),
        }
    }
}

fn drain_request_bytes(stream: &mut TcpStream, request: &mut Vec<u8>) -> Result<(), String> {
    let mut buffer = [0_u8; 1024];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => append_request_bytes(request, &buffer[..read])?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("drain request bytes: {error}")),
        }
    }
}

fn append_request_bytes(request: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    if request.len() + bytes.len() > MAX_REQUEST_BYTES {
        return Err(format!("request exceeded {MAX_REQUEST_BYTES} bytes"));
    }
    request.extend_from_slice(bytes);
    Ok(())
}

#[derive(Debug)]
struct CapturedRequest {
    request_line: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn parse(observation: &Observation) -> Self {
        assert_eq!(observation.accepted_connections, 1);
        let head_offset = observation
            .request_bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("captured request has a complete head");
        let head = std::str::from_utf8(&observation.request_bytes[..head_offset])
            .expect("captured request head is UTF-8");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("captured request line").to_owned();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(':').expect("well-formed captured header");
                (name.to_ascii_lowercase(), value.trim().as_bytes().to_vec())
            })
            .collect();
        let body = observation.request_bytes[head_offset + 4..].to_vec();
        Self {
            request_line,
            headers,
            body,
        }
    }

    fn header_values(&self, name: &str) -> Vec<&[u8]> {
        self.headers
            .iter()
            .filter_map(|(candidate, value)| {
                candidate
                    .eq_ignore_ascii_case(name)
                    .then_some(value.as_slice())
            })
            .collect()
    }
}

#[derive(Clone)]
struct BodyProbe {
    polls: Arc<Mutex<Vec<Option<Bytes>>>>,
}

struct TrackedBody {
    chunks: Mutex<VecDeque<Bytes>>,
    size_hint: Option<u64>,
    probe: BodyProbe,
    _pinned: PhantomPinned,
}

impl TrackedBody {
    fn source(
        chunks: impl IntoIterator<Item = Bytes>,
        size_hint: Option<u64>,
    ) -> (BodySource, BodyProbe) {
        let probe = BodyProbe {
            polls: Arc::new(Mutex::new(Vec::new())),
        };
        let body = Self {
            chunks: Mutex::new(chunks.into_iter().collect()),
            size_hint,
            probe: probe.clone(),
            _pinned: PhantomPinned,
        };
        (BodySource::Stream(Box::pin(body)), probe)
    }
}

impl AsyncBody for TrackedBody {
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
        let body = self.as_ref().get_ref();
        let next = body.chunks.lock().expect("body chunks lock").pop_front();
        body.probe
            .polls
            .lock()
            .expect("body poll log lock")
            .push(next.clone());
        Poll::Ready(next.map(Ok))
    }

    fn size_hint(&self) -> Option<u64> {
        self.size_hint
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build caller-owned Tokio runtime")
}

fn complete_exchange(
    runtime: &tokio::runtime::Runtime,
    request: RequestBuilder,
) -> (StatusCode, String, Bytes) {
    let exchange = runtime.block_on(async {
        tokio::time::timeout(EXCHANGE_TIMEOUT, async {
            let response = request.send().await?;
            let status = response.status();
            let url = response.url().to_owned();
            let body = response.bytes().await?;
            Ok::<_, requests::Error>((status, url, body))
        })
        .await
    });
    match exchange {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => panic!("HTTP exchange failed: {error}"),
        Err(error) => panic!("HTTP exchange timed out: {error}"),
    }
}

#[test]
fn request_framing_get_preserves_explicit_host_and_same_name_value_order() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let client = Client::new().expect("build client");

    let (status, _, body) = complete_exchange(
        &runtime,
        client
            .get(server.url())
            .header(
                HeaderName::from_static("host"),
                HeaderValue::from_static("virtual.example.test"),
            )
            .header(
                HeaderName::from_static("x-sequence"),
                HeaderValue::from_static("first"),
            )
            .header(
                HeaderName::from_static("x-unrelated"),
                HeaderValue::from_static("between"),
            )
            .header(
                HeaderName::from_static("x-sequence"),
                HeaderValue::from_static("second"),
            ),
    );
    let observation = server.finish().expect("loopback fixture completed");
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(request.request_line, "GET /direct?source=task10 HTTP/1.1");
    assert_eq!(
        request.header_values("host"),
        vec![&b"virtual.example.test"[..]]
    );
    assert_eq!(
        request.header_values("x-sequence"),
        vec![&b"first"[..], &b"second"[..]]
    );
    assert_eq!(request.header_values("x-unrelated"), vec![&b"between"[..]]);
    assert_eq!(request.headers.len(), 4);
    assert!(request.header_values("content-length").is_empty());
    assert!(request.header_values("transfer-encoding").is_empty());
    assert!(request.body.is_empty());
}

#[test]
fn request_framing_get_inserts_missing_host_from_authority() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let authority = server.authority();
    let client = Client::new().expect("build client");

    let (status, _, body) = complete_exchange(&runtime, client.get(server.url()));
    let observation = server.finish().expect("loopback fixture completed");
    assert_eq!(
        observation.request_bytes,
        format!("GET /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\n\r\n").into_bytes()
    );
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(request.request_line, "GET /direct?source=task10 HTTP/1.1");
    assert_eq!(request.header_values("host"), vec![authority.as_bytes()]);
    assert!(request.header_values("content-length").is_empty());
    assert!(request.header_values("transfer-encoding").is_empty());
    assert!(request.body.is_empty());
}

#[test]
fn request_framing_head_has_no_request_body_framing() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(HEAD_RESPONSE);
    let authority = server.authority();
    let client = Client::new().expect("build client");

    let (status, _, body) = complete_exchange(&runtime, client.request(Method::HEAD, server.url()));
    let observation = server.finish().expect("loopback fixture completed");
    assert_eq!(
        observation.request_bytes,
        format!("HEAD /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\n\r\n").into_bytes()
    );
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(request.request_line, "HEAD /direct?source=task10 HTTP/1.1");
    assert_eq!(request.header_values("host"), vec![authority.as_bytes()]);
    assert!(request.header_values("content-length").is_empty());
    assert!(request.header_values("transfer-encoding").is_empty());
    assert!(request.body.is_empty());
}

#[test]
fn request_framing_fixed_post_uses_content_length() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let authority = server.authority();
    let client = Client::new().expect("build client");

    let (status, _, response_body) = complete_exchange(
        &runtime,
        client
            .request(Method::POST, server.url())
            .body(Bytes::from_static(b"fixed")),
    );
    let observation = server.finish().expect("loopback fixture completed");
    assert_eq!(
        observation.request_bytes,
        format!(
            "POST /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\ncontent-length: 5\r\n\r\nfixed"
        )
        .into_bytes()
    );
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(response_body.is_empty());
    assert_eq!(request.request_line, "POST /direct?source=task10 HTTP/1.1");
    assert_eq!(request.header_values("host"), vec![authority.as_bytes()]);
    assert_eq!(request.header_values("content-length"), vec![&b"5"[..]]);
    assert!(request.header_values("transfer-encoding").is_empty());
    assert_eq!(request.headers.len(), 2);
    assert_eq!(request.body, b"fixed");
}

#[test]
fn request_framing_unknown_length_non_unpin_body_is_polled_after_connect_and_chunked() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let authority = server.authority();
    let (body, probe) = TrackedBody::source(
        [Bytes::from_static(b"abc"), Bytes::from_static(b"de")],
        None,
    );
    let client = Client::new().expect("build client");
    assert!(probe.polls.lock().expect("body poll log lock").is_empty());

    let (status, _, response_body) = complete_exchange(
        &runtime,
        client.request(Method::POST, server.url()).body(body),
    );
    let observation = server.finish().expect("loopback fixture completed");
    assert_eq!(
        observation.request_bytes,
        format!(
            "POST /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"
        )
        .into_bytes()
    );
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(response_body.is_empty());
    assert_eq!(
        *probe.polls.lock().expect("body poll log lock"),
        vec![
            Some(Bytes::from_static(b"abc")),
            Some(Bytes::from_static(b"de")),
            None,
        ]
    );
    assert_eq!(request.request_line, "POST /direct?source=task10 HTTP/1.1");
    assert_eq!(request.header_values("host"), vec![authority.as_bytes()]);
    assert!(request.header_values("content-length").is_empty());
    assert_eq!(
        request.header_values("transfer-encoding"),
        vec![&b"chunked"[..]]
    );
    assert_eq!(request.headers.len(), 2);
    assert_eq!(request.body, b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
}

#[test]
fn request_framing_body_is_not_polled_when_connect_fails() {
    let runtime = runtime();
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .expect("reserve refused-connect address");
    let address = listener.local_addr().expect("read refused-connect address");
    drop(listener);
    let (body, probe) = TrackedBody::source([Bytes::from_static(b"never")], None);
    let client = Client::new().expect("build client");
    assert!(probe.polls.lock().expect("body poll log lock").is_empty());

    let result = runtime.block_on(async {
        tokio::time::timeout(
            EXCHANGE_TIMEOUT,
            client
                .request(Method::POST, format!("http://{address}/unreachable"))
                .body(body)
                .send(),
        )
        .await
    });
    let error = match result {
        Ok(Err(error)) => error,
        Ok(Ok(_)) => panic!("request to closed loopback listener unexpectedly succeeded"),
        Err(error) => panic!("refused-connect request timed out: {error}"),
    };

    assert_eq!(error.kind(), ErrorKind::Connect);
    assert!(probe.polls.lock().expect("body poll log lock").is_empty());
}

#[test]
fn request_framing_conflicting_content_lengths_fail_before_connect() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let client = Client::new().expect("build client");
    let request = client
        .request(Method::POST, server.url())
        .header(
            HeaderName::from_static("content-length"),
            HeaderValue::from_static("3"),
        )
        .header(
            HeaderName::from_static("content-length"),
            HeaderValue::from_static("4"),
        )
        .body(Bytes::from_static(b"abc"));

    let result =
        runtime.block_on(async { tokio::time::timeout(EXCHANGE_TIMEOUT, request.send()).await });
    let error = match result {
        Ok(Err(error)) => error,
        Ok(Ok(_)) => panic!("conflicting Content-Length request unexpectedly succeeded"),
        Err(error) => panic!("conflicting Content-Length request timed out: {error}"),
    };
    let observation = server.finish().expect("zero-connect fixture completed");

    assert_eq!(error.kind(), ErrorKind::Builder);
    assert_eq!(error.to_string(), "conflicting Content-Length headers");
    assert_eq!(observation.accepted_connections, 0);
    assert!(observation.request_bytes.is_empty());
}

#[test]
fn request_framing_equal_content_lengths_are_preserved() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let authority = server.authority();
    let client = Client::new().expect("build client");

    let (status, _, response_body) = complete_exchange(
        &runtime,
        client
            .request(Method::POST, server.url())
            .header(
                HeaderName::from_static("content-length"),
                HeaderValue::from_static("3"),
            )
            .header(
                HeaderName::from_static("content-length"),
                HeaderValue::from_static("03"),
            )
            .body(Bytes::from_static(b"abc")),
    );
    let observation = server.finish().expect("loopback fixture completed");
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(response_body.is_empty());
    assert_eq!(request.header_values("host"), vec![authority.as_bytes()]);
    assert_eq!(
        request.header_values("content-length"),
        vec![&b"3"[..], &b"03"[..]]
    );
    assert!(request.header_values("transfer-encoding").is_empty());
    assert_eq!(request.body, b"abc");
}

#[test]
fn request_framing_strips_fragment_from_wire_but_retains_response_url() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let authority = server.authority();
    let url = format!("{}#client-only", server.url());
    let client = Client::new().expect("build client");

    let (status, response_url, body) = complete_exchange(&runtime, client.get(&url));
    let observation = server.finish().expect("loopback fixture completed");
    assert_eq!(
        observation.request_bytes,
        format!("GET /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\n\r\n").into_bytes()
    );
    let request = CapturedRequest::parse(&observation);

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(response_url, url);
    assert_eq!(request.request_line, "GET /direct?source=task10 HTTP/1.1");
    assert!(!request.request_line.contains('#'));
}

#[test]
fn get_over_new_plain_connection() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build caller-owned Tokio runtime");
    let server = ScriptedServer::spawn();
    let url = server.url();
    let authority = server.authority();
    let host = HeaderValue::from_bytes(authority.as_bytes()).expect("valid loopback Host");

    let exchange = runtime.block_on(async {
        tokio::time::timeout(EXCHANGE_TIMEOUT, async {
            let client = Client::new()?;
            let response = client
                .get(&url)
                .header(HeaderName::from_static("host"), host)
                .send()
                .await?;
            let status = response.status();
            let fixture_header = response.headers().get("x-fixture").cloned();
            let response_url = response.url().to_owned();
            let body = response.bytes().await?;

            Ok::<_, requests::Error>((status, fixture_header, response_url, body))
        })
        .await
    });

    let observation = server.finish();
    let (status, fixture_header, response_url, body) = match exchange {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => panic!("plain HTTP exchange failed: {error}"),
        Err(error) => panic!("plain HTTP exchange timed out: {error}"),
    };
    let observation = observation.expect("loopback fixture completed");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        fixture_header.as_ref().map(HeaderValue::as_bytes),
        Some(&b"direct"[..])
    );
    assert_eq!(response_url, url);
    assert_eq!(body.as_ref(), b"direct\n");
    assert_eq!(observation.accepted_connections, 1);
    assert_eq!(
        observation.request_bytes,
        format!("GET /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\n\r\n").into_bytes()
    );
}

#[test]
fn complete_connection_close_response_is_successful() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build caller-owned Tokio runtime");

    for iteration in 0..8 {
        let server = ScriptedServer::spawn_with_response(CONNECTION_CLOSE_RESPONSE);
        let url = server.url();
        let authority = server.authority();
        let host = HeaderValue::from_bytes(authority.as_bytes()).expect("valid loopback Host");

        let exchange = runtime.block_on(async {
            tokio::time::timeout(EXCHANGE_TIMEOUT, async {
                let client = Client::new()?;
                let response = client
                    .get(&url)
                    .header(HeaderName::from_static("host"), host)
                    .send()
                    .await?;
                let status = response.status();
                let connection = response.headers().get("connection").cloned();
                let body = response.bytes().await?;

                Ok::<_, requests::Error>((status, connection, body))
            })
            .await
        });

        let observation = server.finish();
        let (status, connection, body) = match exchange {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                panic!("connection-close exchange {iteration} failed: {error}")
            }
            Err(error) => {
                panic!("connection-close exchange {iteration} timed out: {error}")
            }
        };
        let observation = observation.expect("loopback fixture completed");

        assert_eq!(status, StatusCode::OK, "iteration {iteration}");
        assert_eq!(
            connection.as_ref().map(HeaderValue::as_bytes),
            Some(&b"close"[..]),
            "iteration {iteration}"
        );
        assert_eq!(body.as_ref(), b"ok", "iteration {iteration}");
        assert_eq!(observation.accepted_connections, 1, "iteration {iteration}");
        assert_eq!(
            observation.request_bytes,
            format!("GET /direct?source=task10 HTTP/1.1\r\nhost: {authority}\r\n\r\n").into_bytes(),
            "iteration {iteration}"
        );
    }
}

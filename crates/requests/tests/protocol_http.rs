use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::io::{Read, Write};
use std::marker::PhantomPinned;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
#[cfg(feature = "blocking")]
use requests::blocking;
use requests::{
    AsyncBody, BodySource, Client, ErrorKind, HeaderName, HeaderValue, Method, Proxy,
    RequestBuilder, ResponseBody, StatusCode, Timeout, Uri, Version,
};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const POST_EXCHANGE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const PHASE_TIMEOUT: Duration = Duration::from_secs(3);
const POOL_TIMEOUT: Duration = Duration::from_secs(5);
const DEADLINE_FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);
const DEADLINE_OUTER_TIMEOUT: Duration = Duration::from_secs(4);
const DEADLINE_SHORT: Duration = Duration::from_millis(200);
const DEADLINE_LONG: Duration = Duration::from_millis(900);
const DEADLINE_PHASE_DELAY: Duration = Duration::from_millis(600);
const UPLOAD_TOTAL_LONG: Duration = Duration::from_millis(900);
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
const NO_LENGTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nraw";
const CHUNKED_METADATA_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n";
const DEADLINE_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nx-fixture: deadline\r\nContent-Length: 2\r\n\r\nok";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptedConnectionMode {
    KeepOpen,
    CloseAfterWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhaseCommand {
    ReleaseNext,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhaseEvent {
    FirstSent,
    TailSent(usize),
    PeerEof,
}

#[derive(Debug)]
struct PhasedObservation {
    request_bytes: Vec<u8>,
    peer_eof_count: usize,
}

struct PhasedServer {
    address: SocketAddr,
    commands: Sender<PhaseCommand>,
    events: Receiver<PhaseEvent>,
    worker: Option<JoinHandle<Result<PhasedObservation, String>>>,
}

#[derive(Debug)]
struct DeadlineObservation {
    request_bytes: Vec<u8>,
    peer_eof_count: usize,
    expected_write_failures: usize,
}

struct DeadlinePhase {
    delay: Option<Duration>,
    bytes: Vec<u8>,
}

impl DeadlinePhase {
    fn after(delay: Duration, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delay: Some(delay),
            bytes: bytes.into(),
        }
    }

    fn gated(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delay: None,
            bytes: bytes.into(),
        }
    }
}

enum DeadlineCommand {
    ReleaseNext,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeadlineEvent {
    RequestCaptured,
    PhaseSent(usize),
}

struct DeadlineServer {
    address: SocketAddr,
    commands: Sender<DeadlineCommand>,
    events: Receiver<DeadlineEvent>,
    result: Receiver<Result<DeadlineObservation, String>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct PendingUploadObservation {
    request_bytes: Vec<u8>,
    peer_eof_count: usize,
    expected_close_errors: usize,
}

struct PendingUploadServer {
    address: SocketAddr,
    shutdown: Sender<()>,
    result: Receiver<Result<PendingUploadObservation, String>>,
    worker: Option<JoinHandle<()>>,
}

struct CallerTaskBody {
    chunk: Option<Bytes>,
    length: u64,
    caller_thread: thread::ThreadId,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    all_polls_on_caller_runtime_thread: Arc<AtomicBool>,
}

impl AsyncBody for CallerTaskBody {
    fn poll_next(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
        self.polls.fetch_add(1, Ordering::AcqRel);
        if thread::current().id() != self.caller_thread
            || tokio::runtime::Handle::try_current().is_err()
        {
            self.all_polls_on_caller_runtime_thread
                .store(false, Ordering::Release);
        }
        Poll::Ready(self.chunk.take().map(Ok))
    }

    fn size_hint(&self) -> Option<u64> {
        Some(self.length)
    }
}

impl Drop for CallerTaskBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PoolScript {
    KeepAlive,
    HoldFirstBody,
    MalformedFirstBody,
}

enum PoolCommand {
    CloseConnection {
        id: usize,
        acknowledgement: Sender<Result<(), String>>,
    },
}

#[derive(Debug)]
struct PoolRequestObservation {
    connection_id: usize,
    request_bytes: Vec<u8>,
}

#[derive(Debug)]
struct PoolObservation {
    accepted_connections: usize,
    requests: Vec<PoolRequestObservation>,
    peer_closed_connections: Vec<usize>,
}

impl PoolObservation {
    fn connection_ids(&self) -> Vec<usize> {
        self.requests
            .iter()
            .map(|request| request.connection_id)
            .collect()
    }
}

struct PoolConnection {
    id: usize,
    stream: TcpStream,
    request_buffer: Vec<u8>,
    requests_served: usize,
    closed: bool,
}

struct PoolServer {
    address: SocketAddr,
    commands: Sender<PoolCommand>,
    shutdown: Option<Sender<()>>,
    worker: Option<JoinHandle<Result<PoolObservation, String>>>,
}

impl PoolServer {
    fn spawn(script: PoolScript, expected_requests: usize, expected_peer_closes: usize) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind pool loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make pool fixture listener nonblocking");
        let address = listener.local_addr().expect("read pool fixture address");
        let (command_tx, command_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            serve_pool(
                listener,
                command_rx,
                shutdown_rx,
                script,
                expected_requests,
                expected_peer_closes,
            )
        });
        Self {
            address,
            commands: command_tx,
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn close_connection(&self, id: usize) {
        let (acknowledgement, acknowledged) = mpsc::channel();
        self.commands
            .send(PoolCommand::CloseConnection {
                id,
                acknowledgement,
            })
            .expect("send pooled connection close command");
        acknowledged
            .recv_timeout(POOL_TIMEOUT)
            .expect("timed out waiting for pooled connection close acknowledgement")
            .expect("pooled connection close failed");
    }

    fn finish(mut self) -> Result<PoolObservation, String> {
        self.signal_shutdown();
        join_pool_worker(self.worker.take())
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for PoolServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        let _ = join_pool_worker(self.worker.take());
    }
}

fn join_pool_worker(
    worker: Option<JoinHandle<Result<PoolObservation, String>>>,
) -> Result<PoolObservation, String> {
    let worker = worker.ok_or_else(|| "pool fixture worker already joined".to_owned())?;
    worker
        .join()
        .map_err(|_| "pool fixture worker panicked".to_owned())?
}

fn serve_pool(
    listener: TcpListener,
    commands: Receiver<PoolCommand>,
    shutdown: Receiver<()>,
    script: PoolScript,
    expected_requests: usize,
    expected_peer_closes: usize,
) -> Result<PoolObservation, String> {
    let deadline = Instant::now() + POOL_TIMEOUT;
    let mut connections = Vec::new();
    let mut accepted_connections = 0;
    let mut requests = Vec::new();
    let mut peer_closed_connections = Vec::new();
    let mut shutting_down = false;

    loop {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(true)
                        .map_err(|error| format!("make pooled stream nonblocking: {error}"))?;
                    stream
                        .set_write_timeout(Some(SOCKET_TIMEOUT))
                        .map_err(|error| format!("set pooled stream write timeout: {error}"))?;
                    let id = accepted_connections;
                    accepted_connections += 1;
                    connections.push(PoolConnection {
                        id,
                        stream,
                        request_buffer: Vec::new(),
                        requests_served: 0,
                        closed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(format!("pool fixture accept failed: {error}")),
            }
        }

        for connection in &mut connections {
            read_pool_requests(
                connection,
                script,
                &mut requests,
                &mut peer_closed_connections,
            )?;
        }
        connections.retain(|connection| !connection.closed);

        while let Ok(PoolCommand::CloseConnection {
            id,
            acknowledgement,
        }) = commands.try_recv()
        {
            let result = connections
                .iter_mut()
                .find(|connection| connection.id == id)
                .ok_or_else(|| format!("pooled connection {id} is not open"))
                .and_then(|connection| {
                    connection
                        .stream
                        .shutdown(Shutdown::Both)
                        .map_err(|error| format!("shutdown pooled connection {id}: {error}"))?;
                    connection.closed = true;
                    Ok(())
                });
            let _ = acknowledgement.send(result);
        }
        connections.retain(|connection| !connection.closed);

        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => shutting_down = true,
            Err(TryRecvError::Empty) => {}
        }
        if shutting_down
            && requests.len() >= expected_requests
            && peer_closed_connections.len() >= expected_peer_closes
        {
            for connection in &connections {
                let _ = connection.stream.shutdown(Shutdown::Both);
            }
            return Ok(PoolObservation {
                accepted_connections,
                requests,
                peer_closed_connections,
            });
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "pool fixture timed out: accepted={accepted_connections}, requests={}, peer_closes={}",
                requests.len(),
                peer_closed_connections.len()
            ));
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn read_pool_requests(
    connection: &mut PoolConnection,
    script: PoolScript,
    requests: &mut Vec<PoolRequestObservation>,
    peer_closed_connections: &mut Vec<usize>,
) -> Result<(), String> {
    let mut buffer = [0_u8; 1024];
    loop {
        match connection.stream.read(&mut buffer) {
            Ok(0) => {
                connection.closed = true;
                if !peer_closed_connections.contains(&connection.id) {
                    peer_closed_connections.push(connection.id);
                }
                break;
            }
            Ok(read) => {
                if connection.request_buffer.len() + read > MAX_REQUEST_BYTES {
                    return Err(format!(
                        "pooled request on connection {} exceeded {MAX_REQUEST_BYTES} bytes",
                        connection.id
                    ));
                }
                connection.request_buffer.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!(
                    "read pooled request on connection {}: {error}",
                    connection.id
                ));
            }
        }
    }

    while let Some(request_len) = complete_request_len(&connection.request_buffer)? {
        let remainder = connection.request_buffer.split_off(request_len);
        let request_bytes = std::mem::replace(&mut connection.request_buffer, remainder);
        let is_head = request_bytes.starts_with(b"HEAD ");
        requests.push(PoolRequestObservation {
            connection_id: connection.id,
            request_bytes,
        });
        write_pool_response(connection, script, is_head)?;
        connection.requests_served += 1;
    }
    Ok(())
}

fn write_pool_response(
    connection: &mut PoolConnection,
    script: PoolScript,
    is_head: bool,
) -> Result<(), String> {
    const COMPLETE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    const COMPLETE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
    const PARTIAL: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\npartial\r\n";
    const MALFORMED: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZ\r\n";

    let response = if is_head {
        COMPLETE_HEAD
    } else {
        match (script, connection.id, connection.requests_served) {
            (PoolScript::HoldFirstBody, 0, 0) => PARTIAL,
            (PoolScript::MalformedFirstBody, 0, 0) => MALFORMED,
            _ => COMPLETE,
        }
    };
    connection
        .stream
        .set_nonblocking(false)
        .map_err(|error| format!("make pooled stream blocking for response: {error}"))?;
    connection
        .stream
        .write_all(response)
        .map_err(|error| format!("write pooled response: {error}"))?;
    connection
        .stream
        .flush()
        .map_err(|error| format!("flush pooled response: {error}"))?;
    connection
        .stream
        .set_nonblocking(true)
        .map_err(|error| format!("restore pooled stream nonblocking mode: {error}"))
}

impl PhasedServer {
    fn spawn(first: Vec<u8>, tails: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind phased loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make phased fixture listener nonblocking");
        let address = listener.local_addr().expect("read phased fixture address");
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker =
            thread::spawn(move || serve_phases(listener, command_rx, event_tx, first, tails));
        Self {
            address,
            commands: command_tx,
            events: event_rx,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/phased", self.address)
    }

    fn wait_first(&self) {
        assert_eq!(
            self.events
                .recv_timeout(PHASE_TIMEOUT)
                .expect("wait for first response phase"),
            PhaseEvent::FirstSent
        );
    }

    fn release_next(&self, index: usize) {
        self.commands
            .send(PhaseCommand::ReleaseNext)
            .expect("release next response phase");
        assert_eq!(
            self.events
                .recv_timeout(PHASE_TIMEOUT)
                .expect("wait for released response phase"),
            PhaseEvent::TailSent(index)
        );
    }

    fn release_after(&self, delay: Duration) -> JoinHandle<()> {
        let commands = self.commands.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            commands
                .send(PhaseCommand::ReleaseNext)
                .expect("release delayed response phase");
        })
    }

    fn wait_tail(&self, index: usize) {
        assert_eq!(
            self.events
                .recv_timeout(PHASE_TIMEOUT)
                .expect("wait for delayed response phase"),
            PhaseEvent::TailSent(index)
        );
    }

    fn wait_peer_eof(&self) {
        assert_eq!(
            self.events
                .recv_timeout(PHASE_TIMEOUT)
                .expect("wait for peer EOF"),
            PhaseEvent::PeerEof
        );
    }

    fn finish(mut self) -> Result<PhasedObservation, String> {
        let _ = self.commands.send(PhaseCommand::Close);
        join_phased_worker(self.worker.take())
    }
}

impl Drop for PhasedServer {
    fn drop(&mut self) {
        let _ = self.commands.send(PhaseCommand::Close);
        let _ = join_phased_worker(self.worker.take());
    }
}

fn join_phased_worker(
    worker: Option<JoinHandle<Result<PhasedObservation, String>>>,
) -> Result<PhasedObservation, String> {
    let worker = worker.ok_or_else(|| "phased fixture worker already joined".to_owned())?;
    worker
        .join()
        .map_err(|_| "phased fixture worker panicked".to_owned())?
}

fn serve_phases(
    listener: TcpListener,
    commands: Receiver<PhaseCommand>,
    events: Sender<PhaseEvent>,
    first: Vec<u8>,
    tails: Vec<Vec<u8>>,
) -> Result<PhasedObservation, String> {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("phased fixture accept failed: {error}")),
        }
        if matches!(commands.try_recv(), Ok(PhaseCommand::Close)) {
            return Ok(PhasedObservation {
                request_bytes: Vec::new(),
                peer_eof_count: 0,
            });
        }
        if Instant::now() >= deadline {
            return Err("phased fixture timed out accepting a connection".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    };

    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("set phased fixture read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("set phased fixture write timeout: {error}"))?;
    let request_bytes = read_complete_request(&mut stream)?;
    write_phase(&mut stream, &first)?;
    events
        .send(PhaseEvent::FirstSent)
        .map_err(|_| "phased fixture first-phase receiver closed".to_owned())?;
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("make phased fixture stream nonblocking: {error}"))?;

    let mut tails = tails.into_iter().enumerate();
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        match commands.try_recv() {
            Ok(PhaseCommand::ReleaseNext) => {
                let Some((index, phase)) = tails.next() else {
                    return Err("phased fixture has no remaining response phase".to_owned());
                };
                stream
                    .set_nonblocking(false)
                    .map_err(|error| format!("make phased fixture stream blocking: {error}"))?;
                write_phase(&mut stream, &phase)?;
                events
                    .send(PhaseEvent::TailSent(index))
                    .map_err(|_| "phased fixture tail receiver closed".to_owned())?;
                stream.set_nonblocking(true).map_err(|error| {
                    format!("restore phased fixture stream nonblocking mode: {error}")
                })?;
            }
            Ok(PhaseCommand::Close) | Err(TryRecvError::Disconnected) => {
                let _ = stream.shutdown(Shutdown::Both);
                return Ok(PhasedObservation {
                    request_bytes,
                    peer_eof_count: 0,
                });
            }
            Err(TryRecvError::Empty) => {}
        }

        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => {
                events
                    .send(PhaseEvent::PeerEof)
                    .map_err(|_| "phased fixture peer-EOF receiver closed".to_owned())?;
                return Ok(PhasedObservation {
                    request_bytes,
                    peer_eof_count: 1,
                });
            }
            Ok(_) => return Err("phased fixture received bytes after request".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("observe phased fixture peer: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("phased fixture timed out waiting for command or peer EOF".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn write_phase(stream: &mut TcpStream, phase: &[u8]) -> Result<(), String> {
    stream
        .write_all(phase)
        .map_err(|error| format!("write phased response: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush phased response: {error}"))
}

impl DeadlineServer {
    fn spawn(phases: Vec<DeadlinePhase>) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind deadline loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make deadline fixture listener nonblocking");
        let address = listener
            .local_addr()
            .expect("read deadline fixture address");
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = result_tx.send(serve_deadline(listener, command_rx, event_tx, phases));
        });

        Self {
            address,
            commands: command_tx,
            events: event_rx,
            result: result_rx,
            worker: Some(worker),
        }
    }

    fn delayed_head(delay: Duration) -> Self {
        Self::spawn(vec![DeadlinePhase::after(delay, DEADLINE_RESPONSE)])
    }

    fn url(&self) -> String {
        format!("http://{}/deadline", self.address)
    }

    fn finish(mut self) -> Result<DeadlineObservation, String> {
        let result = self
            .result
            .recv_timeout(DEADLINE_FIXTURE_TIMEOUT)
            .map_err(|error| format!("deadline fixture result timed out: {error}"))?;
        self.join_worker()?;
        result
    }

    async fn wait_request_captured(&self) {
        loop {
            match self.events.try_recv() {
                Ok(DeadlineEvent::RequestCaptured) => return,
                Ok(event) => panic!("unexpected deadline event before request capture: {event:?}"),
                Err(TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(TryRecvError::Disconnected) => {
                    panic!("deadline fixture ended before capturing the request")
                }
            }
        }
    }

    fn release_next(&self, index: usize) {
        self.commands
            .send(DeadlineCommand::ReleaseNext)
            .expect("release deadline response phase");
        assert_eq!(
            self.events
                .recv_timeout(DEADLINE_FIXTURE_TIMEOUT)
                .expect("wait for released deadline response phase"),
            DeadlineEvent::PhaseSent(index)
        );
    }

    fn signal_shutdown(&mut self) {
        let _ = self.commands.send(DeadlineCommand::Shutdown);
    }

    fn join_worker(&mut self) -> Result<(), String> {
        let worker = self
            .worker
            .take()
            .ok_or_else(|| "deadline fixture worker already joined".to_owned())?;
        worker
            .join()
            .map_err(|_| "deadline fixture worker panicked".to_owned())
    }
}

impl Drop for DeadlineServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        let _ = self.result.recv_timeout(DEADLINE_FIXTURE_TIMEOUT);
        let _ = self.join_worker();
    }
}

fn serve_deadline(
    listener: TcpListener,
    commands: Receiver<DeadlineCommand>,
    events: Sender<DeadlineEvent>,
    phases: Vec<DeadlinePhase>,
) -> Result<DeadlineObservation, String> {
    let deadline = Instant::now() + DEADLINE_FIXTURE_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("deadline fixture accept failed: {error}")),
        }
        if deadline_shutdown_requested(&commands)? {
            return Ok(DeadlineObservation {
                request_bytes: Vec::new(),
                peer_eof_count: 0,
                expected_write_failures: 0,
            });
        }
        if Instant::now() >= deadline {
            return Err("deadline fixture timed out accepting a connection".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    };

    stream
        .set_read_timeout(Some(DEADLINE_FIXTURE_TIMEOUT))
        .map_err(|error| format!("set deadline fixture read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(DEADLINE_FIXTURE_TIMEOUT))
        .map_err(|error| format!("set deadline fixture write timeout: {error}"))?;
    let request_bytes = read_complete_request(&mut stream)?;
    events
        .send(DeadlineEvent::RequestCaptured)
        .map_err(|_| "deadline fixture request event receiver closed".to_owned())?;
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("make deadline fixture stream nonblocking: {error}"))?;
    let mut observation = DeadlineObservation {
        request_bytes,
        peer_eof_count: 0,
        expected_write_failures: 0,
    };

    for (index, phase) in phases.into_iter().enumerate() {
        if !wait_for_deadline_phase(
            &mut stream,
            &commands,
            deadline,
            phase.delay,
            &mut observation,
        )? {
            return Ok(observation);
        }
        if let Err(error) = write_deadline_phase(&mut stream, &phase.bytes) {
            if expected_deadline_write_failure(&error) {
                observation.expected_write_failures += 1;
                return Ok(observation);
            }
            return Err(format!("write deadline response phase: {error}"));
        }
        events
            .send(DeadlineEvent::PhaseSent(index))
            .map_err(|_| "deadline fixture phase event receiver closed".to_owned())?;
    }

    loop {
        if deadline_shutdown_requested(&commands)? {
            return Ok(observation);
        }
        if observe_deadline_peer_eof(&mut stream)? {
            observation.peer_eof_count = 1;
            return Ok(observation);
        }
        if Instant::now() >= deadline {
            return Err("deadline fixture timed out waiting for peer EOF".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn wait_for_deadline_phase(
    stream: &mut TcpStream,
    commands: &Receiver<DeadlineCommand>,
    deadline: Instant,
    delay: Option<Duration>,
    observation: &mut DeadlineObservation,
) -> Result<bool, String> {
    let release_at = delay.map(|delay| Instant::now() + delay);
    loop {
        if release_at.is_some_and(|release_at| Instant::now() >= release_at) {
            return Ok(true);
        }
        match commands.try_recv() {
            Ok(DeadlineCommand::ReleaseNext) if release_at.is_none() => return Ok(true),
            Ok(DeadlineCommand::ReleaseNext) => {
                return Err("release command targeted a delayed deadline phase".to_owned());
            }
            Ok(DeadlineCommand::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(false),
            Err(TryRecvError::Empty) => {}
        }
        if observe_deadline_peer_eof(stream)? {
            observation.peer_eof_count = 1;
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Err("deadline fixture timed out before a response phase".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn deadline_shutdown_requested(commands: &Receiver<DeadlineCommand>) -> Result<bool, String> {
    match commands.try_recv() {
        Ok(DeadlineCommand::Shutdown) | Err(TryRecvError::Disconnected) => Ok(true),
        Ok(DeadlineCommand::ReleaseNext) => {
            Err("release command arrived outside a gated deadline phase".to_owned())
        }
        Err(TryRecvError::Empty) => Ok(false),
    }
}

fn observe_deadline_peer_eof(stream: &mut TcpStream) -> Result<bool, String> {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => Ok(true),
        Ok(_) => Err("deadline fixture received bytes after the complete request".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Ok(false),
        Err(error) if expected_deadline_write_failure(&error) => Ok(true),
        Err(error) => Err(format!("observe deadline fixture peer: {error}")),
    }
}

fn write_deadline_phase(stream: &mut TcpStream, phase: &[u8]) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    let write = stream.write_all(phase).and_then(|()| stream.flush());
    let restore = stream.set_nonblocking(true);
    write.and(restore)
}

fn expected_deadline_write_failure(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

impl PendingUploadServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind pending-upload loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make pending-upload listener nonblocking");
        let address = listener
            .local_addr()
            .expect("read pending-upload fixture address");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = result_tx.send(serve_pending_upload(listener, shutdown_rx));
        });
        Self {
            address,
            shutdown: shutdown_tx,
            result: result_rx,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/upload", self.address)
    }

    fn finish(mut self) -> Result<PendingUploadObservation, String> {
        let result = self
            .result
            .recv_timeout(DEADLINE_FIXTURE_TIMEOUT)
            .map_err(|error| format!("pending-upload fixture result timed out: {error}"))?;
        self.join_worker()?;
        result
    }

    fn join_worker(&mut self) -> Result<(), String> {
        let worker = self
            .worker
            .take()
            .ok_or_else(|| "pending-upload fixture worker already joined".to_owned())?;
        worker
            .join()
            .map_err(|_| "pending-upload fixture worker panicked".to_owned())
    }
}

impl Drop for PendingUploadServer {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.shutdown.send(());
            let _ = self.result.recv_timeout(DEADLINE_FIXTURE_TIMEOUT);
            let _ = self.join_worker();
        }
    }
}

fn serve_pending_upload(
    listener: TcpListener,
    shutdown: Receiver<()>,
) -> Result<PendingUploadObservation, String> {
    let deadline = Instant::now() + DEADLINE_FIXTURE_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("pending-upload accept failed: {error}")),
        }
        if matches!(
            shutdown.try_recv(),
            Ok(()) | Err(TryRecvError::Disconnected)
        ) {
            return Ok(PendingUploadObservation {
                request_bytes: Vec::new(),
                peer_eof_count: 0,
                expected_close_errors: 0,
            });
        }
        if Instant::now() >= deadline {
            return Err("pending-upload fixture timed out accepting a connection".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    };
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("make pending-upload stream nonblocking: {error}"))?;

    let mut observation = PendingUploadObservation {
        request_bytes: Vec::new(),
        peer_eof_count: 0,
        expected_close_errors: 0,
    };
    let mut head_captured = false;
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                if !head_captured {
                    return Err("pending upload closed before request head capture".to_owned());
                }
                observation.peer_eof_count = 1;
                return Ok(observation);
            }
            Ok(read) => {
                if observation.request_bytes.len() + read > MAX_REQUEST_BYTES {
                    return Err(format!(
                        "pending upload exceeded {MAX_REQUEST_BYTES} captured bytes"
                    ));
                }
                observation.request_bytes.extend_from_slice(&buffer[..read]);
                head_captured = observation
                    .request_bytes
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n");
                if !head_captured && observation.request_bytes.len() >= MAX_REQUEST_HEAD_BYTES {
                    return Err(format!(
                        "pending upload head exceeded {MAX_REQUEST_HEAD_BYTES} bytes"
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if expected_deadline_write_failure(&error) && head_captured => {
                observation.expected_close_errors = 1;
                return Ok(observation);
            }
            Err(error) => return Err(format!("read pending upload: {error}")),
        }
        if matches!(
            shutdown.try_recv(),
            Ok(()) | Err(TryRecvError::Disconnected)
        ) {
            return Ok(observation);
        }
        if Instant::now() >= deadline {
            return Err("pending-upload fixture timed out waiting for peer close".to_owned());
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

impl ScriptedServer {
    fn spawn() -> Self {
        Self::spawn_with_response(SCRIPTED_RESPONSE)
    }

    fn spawn_with_response(response: &'static [u8]) -> Self {
        Self::spawn_with_mode(response, ScriptedConnectionMode::KeepOpen)
    }

    fn spawn_close_after_response(response: &'static [u8]) -> Self {
        Self::spawn_with_mode(response, ScriptedConnectionMode::CloseAfterWrite)
    }

    fn spawn_with_mode(response: &'static [u8], mode: ScriptedConnectionMode) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make fixture listener nonblocking");
        let address = listener.local_addr().expect("read fixture address");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let worker = thread::spawn(move || serve(listener, &shutdown_rx, response, mode));

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
    mode: ScriptedConnectionMode,
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
    if mode == ScriptedConnectionMode::CloseAfterWrite {
        drop(stream);
        return Ok(Observation {
            accepted_connections: 1,
            request_bytes,
        });
    }
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
        Self::parse_bytes(&observation.request_bytes)
    }

    fn parse_bytes(request_bytes: &[u8]) -> Self {
        let head_offset = request_bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("captured request has a complete head");
        let head = std::str::from_utf8(&request_bytes[..head_offset])
            .expect("captured request head is UTF-8");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("captured request line").to_owned();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(':').expect("well-formed captured header");
                (name.to_ascii_lowercase(), value.trim().as_bytes().to_vec())
            })
            .collect();
        let body = request_bytes[head_offset + 4..].to_vec();
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

#[derive(Clone)]
struct PendingUploadProbe {
    inner: Arc<PendingUploadProbeInner>,
}

struct PendingUploadProbeInner {
    polls: AtomicUsize,
    drops: AtomicUsize,
}

struct PendingUploadBody {
    probe: PendingUploadProbe,
}

impl PendingUploadBody {
    fn source() -> (BodySource, PendingUploadProbe) {
        let probe = PendingUploadProbe {
            inner: Arc::new(PendingUploadProbeInner {
                polls: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
            }),
        };
        (
            BodySource::Stream(Box::pin(Self {
                probe: probe.clone(),
            })),
            probe,
        )
    }
}

impl AsyncBody for PendingUploadBody {
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
        self.probe.inner.polls.fetch_add(1, Ordering::AcqRel);
        Poll::Pending
    }

    fn size_hint(&self) -> Option<u64> {
        None
    }
}

impl Drop for PendingUploadBody {
    fn drop(&mut self) {
        self.probe.inner.drops.fetch_add(1, Ordering::AcqRel);
    }
}

impl PendingUploadProbe {
    async fn wait_pending(&self) {
        tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, async {
            while self.inner.polls.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending upload body was never polled");
        assert!(self.inner.polls.load(Ordering::Acquire) >= 1);
    }

    async fn assert_dropped_once(&self) {
        tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, async {
            while self.inner.drops.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending upload body was not dropped");
        assert_eq!(self.inner.drops.load(Ordering::Acquire), 1);
    }
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

#[cfg(feature = "blocking")]
fn bounded_blocking<T, F, C>(work: F, cancel: C) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    C: FnOnce(),
{
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(work());
    });
    match receiver.recv_timeout(EXCHANGE_TIMEOUT) {
        Ok(result) => {
            worker.join().expect("bounded blocking worker panicked");
            result
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Ok(()) => panic!("bounded blocking worker stopped without a result"),
            Err(payload) => std::panic::resume_unwind(payload),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancel();
            let _ = worker.join();
            panic!("blocking operation exceeded {EXCHANGE_TIMEOUT:?}")
        }
    }
}

#[cfg(feature = "blocking")]
fn bounded_body_read<C>(
    mut body: blocking::ResponseBody,
    capacity: usize,
    cancel: C,
) -> (blocking::ResponseBody, std::io::Result<Vec<u8>>)
where
    C: FnOnce(),
{
    bounded_blocking(
        move || {
            let mut buffer = vec![0; capacity];
            let result = body.read(&mut buffer).map(|read| {
                buffer.truncate(read);
                buffer
            });
            (body, result)
        },
        cancel,
    )
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

fn complete_top_level_exchange(
    runtime: &tokio::runtime::Runtime,
    future: impl Future<Output = requests::Result<requests::Response>>,
) -> Bytes {
    runtime
        .block_on(async {
            tokio::time::timeout(EXCHANGE_TIMEOUT, async {
                let response = future.await?;
                response.bytes().await
            })
            .await
        })
        .expect("top-level exchange exceeded outer bound")
        .expect("top-level exchange failed")
}

fn send_response(runtime: &tokio::runtime::Runtime, request: RequestBuilder) -> requests::Response {
    match runtime.block_on(async { tokio::time::timeout(EXCHANGE_TIMEOUT, request.send()).await }) {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => panic!("response-head exchange failed: {error}"),
        Err(error) => panic!("response-head exchange timed out: {error}"),
    }
}

fn send_deadline_request(
    runtime: &tokio::runtime::Runtime,
    request: RequestBuilder,
) -> requests::Result<requests::Response> {
    runtime
        .block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, request.send()).await })
        .unwrap_or_else(|error| panic!("deadline-contract request exceeded outer bound: {error}"))
}

fn assert_timeout_error(error: &requests::Error, source: &str, phase: &str) {
    assert_eq!(error.kind(), ErrorKind::ReadTimeout);
    let message = error.to_string().to_ascii_lowercase();
    match source {
        "read" => {
            assert!(
                message.contains("read") && !message.contains("total"),
                "timeout error must identify a read-specific timeout: {message}"
            );
        }
        "total" => {
            assert!(
                message.contains("total"),
                "timeout error must identify a total timeout: {message}"
            );
        }
        _ => panic!("unsupported timeout source assertion: {source}"),
    }
    assert!(
        message.contains(phase),
        "timeout error must identify {phase:?}: {message}"
    );
}

fn expect_deadline_error(
    result: requests::Result<requests::Response>,
    missing_timeout: &str,
) -> requests::Error {
    match result {
        Err(error) => error,
        Ok(response) => {
            drop(response);
            panic!("{missing_timeout}");
        }
    }
}

fn collect_deadline_body(
    runtime: &tokio::runtime::Runtime,
    response: requests::Response,
) -> requests::Result<Bytes> {
    runtime
        .block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, response.bytes()).await })
        .unwrap_or_else(|error| panic!("deadline-contract body exceeded outer bound: {error}"))
}

fn pending_upload_error(
    runtime: &tokio::runtime::Runtime,
    client: &Client,
    server: &PendingUploadServer,
    timeout: Timeout,
) -> requests::Error {
    let (body, probe) = PendingUploadBody::source();
    let mut send = runtime.spawn(
        client
            .request(Method::POST, server.url())
            .body(body)
            .timeout(timeout)
            .send(),
    );
    runtime.block_on(probe.wait_pending());
    let result =
        runtime.block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, &mut send).await });
    let result = match result {
        Ok(result) => result.expect("pending-upload request task failed"),
        Err(error) => {
            send.abort();
            let _ = runtime.block_on(send);
            runtime.block_on(probe.assert_dropped_once());
            panic!("pending-upload request exceeded outer bound: {error}");
        }
    };
    runtime.block_on(probe.assert_dropped_once());
    match result {
        Err(error) => error,
        Ok(response) => {
            drop(response);
            panic!("permanently pending upload unexpectedly produced a response");
        }
    }
}

fn assert_pending_upload_timeout(error: &requests::Error) {
    assert_eq!(error.kind(), ErrorKind::ReadTimeout);
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("total"),
        "pending-upload timeout must identify the total source: {message}"
    );
    assert!(
        message.contains("request exchange")
            || (message.contains("request send") && message.contains("response head")),
        "pending-upload timeout must identify the combined send/head exchange: {message}"
    );
}

fn assert_pending_upload_observation(observation: &PendingUploadObservation) {
    let head_end = observation
        .request_bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("pending upload captured a complete request head");
    let head = std::str::from_utf8(&observation.request_bytes[..head_end])
        .expect("pending-upload request head is UTF-8")
        .to_ascii_lowercase();
    assert!(head.starts_with("post /upload http/1.1\r\n"));
    assert!(head.contains("\r\ntransfer-encoding: chunked"));
    assert!(!head.contains("\r\ncontent-length:"));
    assert!(
        observation.request_bytes[head_end + 4..].is_empty(),
        "permanently pending body must not produce upload bytes"
    );
    assert_eq!(
        observation.peer_eof_count + observation.expected_close_errors,
        1,
        "pending-upload timeout must close its one connection"
    );
}

fn assert_deadline_observation(observation: &DeadlineObservation) {
    assert_eq!(
        complete_request_len(&observation.request_bytes).expect("parse deadline request"),
        Some(observation.request_bytes.len()),
        "deadline fixture must capture one complete request"
    );
    assert!(
        observation
            .request_bytes
            .starts_with(b"GET /deadline HTTP/1.1\r\n")
    );
    assert_eq!(
        observation.peer_eof_count + observation.expected_write_failures,
        1,
        "deadline path must close its one connection"
    );
}

async fn next_response_frame<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    poll_fn(|context| Pin::new(&mut *stream).poll_next(context)).await
}

async fn next_response_frame_after_pending(
    mut body: ResponseBody,
    pending: Sender<()>,
) -> (ResponseBody, Option<requests::Result<Bytes>>) {
    let mut pending = Some(pending);
    let item = poll_fn(|context| {
        let result = Pin::new(&mut body).poll_next(context);
        if result.is_pending()
            && let Some(pending) = pending.take()
        {
            pending.send(()).expect("report pending response body poll");
        }
        result
    })
    .await;
    (body, item)
}

async fn wait_for_pending_poll(pending: &Receiver<()>) {
    loop {
        match pending.try_recv() {
            Ok(()) => return,
            Err(TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(TryRecvError::Disconnected) => {
                panic!("response body task ended before reporting a pending poll")
            }
        }
    }
}

#[test]
fn timeout_contract_total_expires_during_pending_request_upload() {
    let runtime = runtime();
    let server = PendingUploadServer::spawn();
    let client = Client::new().expect("build pending-upload client");
    let error = pending_upload_error(
        &runtime,
        &client,
        &server,
        Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        },
    );
    assert_pending_upload_timeout(&error);

    drop(client);
    let observation = server.finish().expect("pending-upload fixture completed");
    assert_pending_upload_observation(&observation);
}

#[test]
fn timeout_contract_connect_and_read_do_not_govern_pending_request_upload() {
    let runtime = runtime();
    let server = PendingUploadServer::spawn();
    let client = Client::new().expect("build pending-upload client");
    let error = pending_upload_error(
        &runtime,
        &client,
        &server,
        Timeout {
            connect: Some(DEADLINE_SHORT),
            read: Some(DEADLINE_SHORT),
            total: Some(UPLOAD_TOTAL_LONG),
        },
    );
    assert_pending_upload_timeout(&error);

    drop(client);
    let observation = server.finish().expect("pending-upload fixture completed");
    assert_pending_upload_observation(&observation);
}

#[test]
fn timeout_contract_response_head_uses_read_timeout() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_SHORT),
        total: None,
    };

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url()).timeout(timeout)),
        "delayed response head ignored the read timeout",
    );
    assert_timeout_error(&error, "read", "response head");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_read_none_and_total_none_allow_delayed_response_head() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(Duration::from_millis(300));
    let client = Client::new().expect("build deadline client");

    let response = send_deadline_request(
        &runtime,
        client.get(server.url()).timeout(Timeout::default()),
    )
    .expect("None deadlines rejected a delayed response head");
    let body = runtime
        .block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, response.bytes()).await })
        .expect("deadline control body exceeded outer bound")
        .expect("deadline control body failed");
    assert_eq!(body, Bytes::from_static(b"ok"));

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_into_body_outside_runtime_preserves_total_deadline() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(Duration::ZERO);
    let client = Client::new().expect("build deadline client");
    let response = send_deadline_request(
        &runtime,
        client.get(server.url()).timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_LONG),
        }),
    )
    .expect("response head should arrive before the total deadline");

    let mut body = response.into_body();
    let bytes = runtime
        .block_on(async {
            tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, async {
                let mut collected = Vec::new();
                while let Some(frame) = next_response_frame(&mut body).await {
                    collected.extend_from_slice(&frame?);
                }
                Ok::<_, requests::Error>(Bytes::from(collected))
            })
            .await
        })
        .expect("body collection exceeded outer bound")
        .expect("body collection failed");
    assert_eq!(bytes, Bytes::from_static(b"ok"));

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_total_precedes_read_while_waiting_for_response_head() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_LONG),
        total: Some(DEADLINE_SHORT),
    };

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url()).timeout(timeout)),
        "delayed response head ignored the shorter total timeout",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_read_precedes_total_while_waiting_for_response_head() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_SHORT),
        total: Some(DEADLINE_LONG),
    };

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url()).timeout(timeout)),
        "delayed response head ignored the shorter read timeout",
    );
    assert_timeout_error(&error, "read", "response head");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_total_timeout_expires_while_waiting_for_response_head() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: None,
        total: Some(DEADLINE_SHORT),
    };

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url()).timeout(timeout)),
        "delayed response head ignored the total timeout",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_equal_head_durations_prefer_earlier_total_deadline() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_SHORT),
        total: Some(DEADLINE_SHORT),
    };

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url()).timeout(timeout)),
        "equal response-head deadlines were ignored",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_total_spans_response_head_and_multiple_body_reads() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::from_millis(150),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n",
        ),
        DeadlinePhase::after(Duration::from_millis(250), b"3\r\ntwo\r\n"),
        DeadlinePhase::after(Duration::from_millis(400), b"0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(Duration::from_millis(500)),
        total: Some(Duration::from_millis(650)),
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("response head should arrive within both deadlines");
    let error = collect_deadline_body(&runtime, response)
        .expect_err("multiple short body gaps ignored the full-request total timeout");
    assert_timeout_error(&error, "total", "response body");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_read_precedes_total_while_waiting_for_response_body() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::ZERO,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ),
        DeadlinePhase::after(DEADLINE_PHASE_DELAY, b"2\r\nok\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_SHORT),
        total: Some(DEADLINE_LONG),
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("response head should arrive before either body deadline");
    let error = collect_deadline_body(&runtime, response)
        .expect_err("delayed body ignored the shorter read timeout");
    assert_timeout_error(&error, "read", "response body");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_total_precedes_read_while_waiting_for_response_body() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::ZERO,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ),
        DeadlinePhase::after(DEADLINE_PHASE_DELAY, b"2\r\nok\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_LONG),
        total: Some(DEADLINE_SHORT),
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("response head should arrive before either body deadline");
    let error = collect_deadline_body(&runtime, response)
        .expect_err("delayed body ignored the shorter total timeout");
    assert_timeout_error(&error, "total", "response body");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_total_only_expires_while_waiting_for_response_body() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::ZERO,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ),
        DeadlinePhase::after(DEADLINE_PHASE_DELAY, b"2\r\nok\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: None,
        total: Some(DEADLINE_SHORT),
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("response head should arrive before the total deadline");
    let error = collect_deadline_body(&runtime, response)
        .expect_err("delayed body ignored the total-only deadline");
    assert_timeout_error(&error, "total", "response body");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_equal_body_durations_prefer_earlier_total_deadline() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::ZERO,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ),
        DeadlinePhase::after(DEADLINE_PHASE_DELAY, b"2\r\nok\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: None,
        read: Some(DEADLINE_SHORT),
        total: Some(DEADLINE_SHORT),
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("response head should arrive before equal body deadlines");
    let error = collect_deadline_body(&runtime, response)
        .expect_err("delayed body ignored equal read and total timeouts");
    assert_timeout_error(&error, "total", "response body");

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_connect_deadline_ends_before_delayed_head_and_body() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::after(
            Duration::from_millis(300),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ),
        DeadlinePhase::after(Duration::from_millis(300), b"2\r\nok\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let timeout = Timeout {
        connect: Some(DEADLINE_SHORT),
        read: None,
        total: None,
    };

    let response = send_deadline_request(&runtime, client.get(server.url()).timeout(timeout))
        .expect("completed connect deadline leaked into delayed response head");
    let body = collect_deadline_body(&runtime, response)
        .expect("completed connect deadline leaked into delayed response body");
    assert_eq!(body, Bytes::from_static(b"ok"));

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
    assert_deadline_observation(&observation);
}

#[test]
fn timeout_contract_all_none_allows_gated_response_head_and_body() {
    let runtime = runtime();
    let server = DeadlineServer::spawn(vec![
        DeadlinePhase::gated(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"),
        DeadlinePhase::gated(b"3\r\none\r\n"),
        DeadlinePhase::gated(b"3\r\ntwo\r\n0\r\n\r\n"),
    ]);
    let client = Client::new().expect("build deadline client");
    let request = client.get(server.url()).timeout(Timeout::default());
    let response_task = runtime.spawn(request.send());

    runtime
        .block_on(async {
            tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, server.wait_request_captured()).await
        })
        .expect("deadline fixture did not capture the gated request");
    assert!(
        !response_task.is_finished(),
        "response head completed before its gate was released"
    );
    server.release_next(0);
    let response = runtime
        .block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, response_task).await })
        .expect("gated response head exceeded outer bound")
        .expect("gated response-head task failed")
        .expect("None deadlines rejected a gated response head");

    let body_task = runtime.spawn(response.bytes());
    runtime.block_on(tokio::task::yield_now());
    assert!(
        !body_task.is_finished(),
        "response body completed before its first gate was released"
    );
    server.release_next(1);
    runtime.block_on(tokio::task::yield_now());
    assert!(
        !body_task.is_finished(),
        "response body completed before its EOF gate was released"
    );
    server.release_next(2);
    let body = runtime
        .block_on(async { tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, body_task).await })
        .expect("gated response body exceeded outer bound")
        .expect("gated response-body task failed")
        .expect("None deadlines rejected a gated response body");
    assert_eq!(body, Bytes::from_static(b"onetwo"));

    drop(client);
    let observation = server.finish().expect("deadline fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
    assert_deadline_observation(&observation);
}

#[test]
fn response_body_is_unpin_for_by_value_close_after_polling() {
    fn assert_unpin<T: Unpin>() {}
    assert_unpin::<ResponseBody>();
}

#[test]
fn response_stream_yields_first_frame_before_released_tail_then_clean_eof() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nfirst\r\n"
            .to_vec(),
        vec![b"4\r\ntail\r\n0\r\n\r\n".to_vec()],
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let mut body = response.into_body();
    let first = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("first response frame timed out")
        .expect("first response frame missing")
        .expect("first response frame failed");
    assert_eq!(first, Bytes::from_static(b"first"));

    server.release_next(0);
    let tail = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("tail response frame timed out")
        .expect("tail response frame missing")
        .expect("tail response frame failed");
    assert_eq!(tail, Bytes::from_static(b"tail"));
    assert!(
        runtime
            .block_on(async {
                tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
            })
            .expect("clean response EOF timed out")
            .is_none()
    );

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
    assert!(
        observation
            .request_bytes
            .starts_with(b"GET /phased HTTP/1.1\r\n")
    );
}

#[test]
fn response_stream_malformed_chunk_is_typed_terminal_error_with_one_cleanup() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        vec![b"not-hex\r\n".to_vec()],
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let mut body = response.into_body();
    server.release_next(0);
    let error = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("malformed response poll timed out")
        .expect("malformed response did not yield an error")
        .expect_err("malformed response unexpectedly yielded bytes");
    assert_eq!(error.kind(), ErrorKind::ChunkedEncoding);
    assert!(
        runtime
            .block_on(async {
                tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
            })
            .expect("terminal malformed response EOF timed out")
            .is_none()
    );

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_bytes_uses_stream_path_for_malformed_chunk_and_cleanup() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        vec![b"also-not-hex\r\n".to_vec()],
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    server.release_next(0);
    let error = runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, response.bytes()).await })
        .expect("malformed response bytes collection timed out")
        .expect_err("malformed response bytes unexpectedly succeeded");
    assert_eq!(error.kind(), ErrorKind::ChunkedEncoding);

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_close_before_read_closes_one_shot_connection_once() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let body = response.into_body();
    runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, body.close()).await })
        .expect("response close timed out")
        .expect("response close failed");

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_close_after_partial_read_closes_one_shot_connection_once() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\npartial\r\n"
            .to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let mut body = response.into_body();
    let first = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("partial response frame timed out")
        .expect("partial response frame missing")
        .expect("partial response frame failed");
    assert_eq!(first, Bytes::from_static(b"partial"));
    runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, body.close()).await })
        .expect("partial response close timed out")
        .expect("partial response close failed");

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_partial_drop_closes_promptly_once() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\npartial\r\n"
            .to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let mut body = response.into_body();
    let first = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("partial response frame timed out")
        .expect("partial response frame missing")
        .expect("partial response frame failed");
    assert_eq!(first, Bytes::from_static(b"partial"));
    drop(body);

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_pending_read_cancellation_closes_promptly_once() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");

    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    runtime.block_on(async {
        let mut body = response.into_body();
        let polled = Arc::new(AtomicBool::new(false));
        let task_polled = Arc::clone(&polled);
        let task = tokio::spawn(async move {
            poll_fn(|context| {
                let result = Pin::new(&mut body).poll_next(context);
                if result.is_pending() {
                    task_polled.store(true, Ordering::Release);
                }
                result
            })
            .await
        });
        tokio::time::timeout(PHASE_TIMEOUT, async {
            while !polled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending body task was never polled");
        task.abort();
        let error = task.await.expect_err("cancelled body task completed");
        assert!(error.is_cancelled());
    });

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_read_timeout_is_typed_terminal_error() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");
    let timeout = Timeout {
        connect: None,
        read: Some(Duration::from_millis(100)),
        total: None,
    };

    let response = send_response(&runtime, client.get(server.url()).timeout(timeout));
    server.wait_first();
    let mut body = response.into_body();
    let error = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("read-timeout response poll hung")
        .expect("read-timeout response did not yield an error")
        .expect_err("read-timeout response unexpectedly yielded bytes");
    assert_eq!(error.kind(), ErrorKind::ReadTimeout);
    assert!(
        runtime
            .block_on(async {
                tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
            })
            .expect("terminal read-timeout EOF timed out")
            .is_none()
    );

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_bytes_uses_stream_path_for_read_timeout_and_cleanup() {
    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        Vec::new(),
    );
    let client = Client::new().expect("build client");
    let timeout = Timeout {
        connect: None,
        read: Some(Duration::from_millis(100)),
        total: None,
    };

    let response = send_response(&runtime, client.get(server.url()).timeout(timeout));
    server.wait_first();
    let error = runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, response.bytes()).await })
        .expect("read-timeout response bytes collection hung")
        .expect_err("read-timeout response bytes unexpectedly succeeded");
    assert_eq!(error.kind(), ErrorKind::ReadTimeout);

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

#[test]
fn response_stream_read_timeout_resets_per_frame_and_is_not_total_duration() {
    const READ_TIMEOUT: Duration = Duration::from_millis(600);
    const RELEASE_DELAY: Duration = Duration::from_millis(400);

    let runtime = runtime();
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\none\r\n"
            .to_vec(),
        vec![b"3\r\ntwo\r\n".to_vec(), b"0\r\n\r\n".to_vec()],
    );
    let client = Client::new().expect("build client");
    let timeout = Timeout {
        connect: None,
        read: Some(READ_TIMEOUT),
        total: None,
    };

    let response = send_response(&runtime, client.get(server.url()).timeout(timeout));
    server.wait_first();
    let mut body = response.into_body();
    let first = runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("first timed response frame timed out")
        .expect("first timed response frame missing")
        .expect("first timed response frame failed");
    assert_eq!(first, Bytes::from_static(b"one"));

    let started = Instant::now();
    let (pending_tx, pending_rx) = mpsc::channel();
    let second = runtime.spawn(next_response_frame_after_pending(body, pending_tx));
    runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, wait_for_pending_poll(&pending_rx)).await
        })
        .expect("second response frame never reached a pending poll");
    let release = server.release_after(RELEASE_DELAY);
    let second = runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, second).await })
        .expect("second timed response frame task hung")
        .expect("second timed response frame task failed");
    release.join().expect("join first delayed release");
    server.wait_tail(0);
    let (returned_body, second) = second;
    body = returned_body;
    let second = second
        .expect("second timed response frame missing")
        .expect("second timed response frame failed");
    assert_eq!(second, Bytes::from_static(b"two"));

    let (pending_tx, pending_rx) = mpsc::channel();
    let eof = runtime.spawn(next_response_frame_after_pending(body, pending_tx));
    runtime
        .block_on(async {
            tokio::time::timeout(PHASE_TIMEOUT, wait_for_pending_poll(&pending_rx)).await
        })
        .expect("terminal response poll never reached Pending");
    let release = server.release_after(RELEASE_DELAY);
    let eof = runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, eof).await })
        .expect("timed response EOF task hung")
        .expect("timed response EOF task failed");
    release.join().expect("join second delayed release");
    server.wait_tail(1);
    let (body, eof) = eof;
    assert!(eof.is_none(), "timed response did not reach clean EOF");
    drop(body);
    assert!(
        started.elapsed() > READ_TIMEOUT,
        "two successful pending reads must outlive one read deadline"
    );

    server.wait_peer_eof();
    let observation = server.finish().expect("phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
}

fn assert_pool_requests(
    observation: &PoolObservation,
    expected_connection_ids: &[usize],
    expected_paths: &[&str],
) {
    assert_eq!(observation.connection_ids(), expected_connection_ids);
    assert_eq!(observation.requests.len(), expected_paths.len());
    for (request, path) in observation.requests.iter().zip(expected_paths) {
        assert_eq!(
            complete_request_len(&request.request_bytes).expect("parse pooled request"),
            Some(request.request_bytes.len()),
            "pooled request must be captured exactly once and without truncation"
        );
        assert!(
            request
                .request_bytes
                .starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()),
            "unexpected pooled request: {:?}",
            String::from_utf8_lossy(&request.request_bytes)
        );
    }
}

#[test]
fn pool_clean_eof_reuses_one_connection_for_two_requests() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 2, 0);
    let client = Client::new().expect("build pooled client");

    let (_, _, first) = complete_exchange(&runtime, client.get(server.url("/pool/first")));
    let (_, _, second) = complete_exchange(&runtime, client.get(server.url("/pool/second")));
    assert_eq!(first, Bytes::from_static(b"ok"));
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 1);
    assert_pool_requests(&observation, &[0, 0], &["/pool/first", "/pool/second"]);
}

#[test]
fn pool_dead_idle_connection_is_discarded_before_the_next_request() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 2, 0);
    let client = Client::new().expect("build pooled client");

    let (_, _, first) = complete_exchange(&runtime, client.get(server.url("/pool/live")));
    assert_eq!(first, Bytes::from_static(b"ok"));
    server.close_connection(0);

    let (_, _, second) = complete_exchange(&runtime, client.get(server.url("/pool/after-dead")));
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert_pool_requests(&observation, &[0, 1], &["/pool/live", "/pool/after-dead"]);
}

#[test]
fn pool_response_drop_before_body_uses_a_second_connection() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 2, 1);
    let client = Client::new().expect("build pooled client");

    let response = send_response(&runtime, client.get(server.url("/pool/drop-response")));
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    let (_, _, second) = complete_exchange(
        &runtime,
        client.get(server.url("/pool/after-response-drop")),
    );
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(
        &observation,
        &[0, 1],
        &["/pool/drop-response", "/pool/after-response-drop"],
    );
}

#[test]
fn pool_last_client_drop_closes_its_idle_connection() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 1, 1);
    let client = Client::new().expect("build pooled client");

    let (_, _, body) = complete_exchange(&runtime, client.get(server.url("/pool/last-client")));
    assert_eq!(body, Bytes::from_static(b"ok"));
    drop(client);

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 1);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(&observation, &[0], &["/pool/last-client"]);
}

#[test]
fn pool_partial_body_drop_forces_a_second_connection() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::HoldFirstBody, 2, 1);
    let client = Client::new().expect("build pooled client");

    let response = send_response(&runtime, client.get(server.url("/pool/drop")));
    let mut body = response.into_body();
    let partial = runtime
        .block_on(async {
            tokio::time::timeout(POOL_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("timed out waiting for partial pooled frame")
        .expect("partial pooled frame missing")
        .expect("partial pooled frame failed");
    assert_eq!(partial, Bytes::from_static(b"partial"));
    drop(body);

    let (_, _, second) = complete_exchange(&runtime, client.get(server.url("/pool/after-drop")));
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(&observation, &[0, 1], &["/pool/drop", "/pool/after-drop"]);
}

#[test]
fn pool_partial_body_close_forces_a_second_connection() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::HoldFirstBody, 2, 1);
    let client = Client::new().expect("build pooled client");

    let response = send_response(&runtime, client.get(server.url("/pool/close")));
    let mut body = response.into_body();
    let partial = runtime
        .block_on(async {
            tokio::time::timeout(POOL_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("timed out waiting for partial pooled frame")
        .expect("partial pooled frame missing")
        .expect("partial pooled frame failed");
    assert_eq!(partial, Bytes::from_static(b"partial"));
    runtime
        .block_on(async { tokio::time::timeout(POOL_TIMEOUT, body.close()).await })
        .expect("timed out closing partial pooled response")
        .expect("close partial pooled response");

    let (_, _, second) = complete_exchange(&runtime, client.get(server.url("/pool/after-close")));
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(&observation, &[0, 1], &["/pool/close", "/pool/after-close"]);
}

#[test]
fn pool_malformed_body_error_forces_a_second_connection() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::MalformedFirstBody, 2, 1);
    let client = Client::new().expect("build pooled client");

    let response = send_response(&runtime, client.get(server.url("/pool/malformed")));
    let mut body = response.into_body();
    let error = runtime
        .block_on(async {
            tokio::time::timeout(POOL_TIMEOUT, next_response_frame(&mut body)).await
        })
        .expect("timed out waiting for malformed pooled body error")
        .expect("malformed pooled body ended without an error")
        .expect_err("malformed pooled body unexpectedly produced a frame");
    assert_eq!(error.kind(), ErrorKind::ChunkedEncoding);
    drop(body);

    let (_, _, second) =
        complete_exchange(&runtime, client.get(server.url("/pool/after-malformed")));
    assert_eq!(second, Bytes::from_static(b"ok"));

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(
        &observation,
        &[0, 1],
        &["/pool/malformed", "/pool/after-malformed"],
    );
}

#[test]
fn pool_instances_isolate_two_clients_for_the_same_authority() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 4, 0);
    let first_client = Client::new().expect("build first pooled client");
    let second_client = Client::new().expect("build second pooled client");

    complete_exchange(
        &runtime,
        first_client.get(server.url("/pool/client-a/first")),
    );
    complete_exchange(
        &runtime,
        second_client.get(server.url("/pool/client-b/first")),
    );
    complete_exchange(
        &runtime,
        first_client.get(server.url("/pool/client-a/second")),
    );
    complete_exchange(
        &runtime,
        second_client.get(server.url("/pool/client-b/second")),
    );

    let observation = server.finish().expect("pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert_pool_requests(
        &observation,
        &[0, 1, 0, 1],
        &[
            "/pool/client-a/first",
            "/pool/client-b/first",
            "/pool/client-a/second",
            "/pool/client-b/second",
        ],
    );
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

#[test]
fn client_execute_uses_the_existing_request_framing_pipeline() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let client = Client::new().expect("build execute client");
    let request = RequestBuilder::new(Method::POST, server.url())
        .header(
            HeaderName::from_static("x-execute"),
            HeaderValue::from_static("same-pipeline"),
        )
        .body(Bytes::from_static(b"execute"))
        .build()
        .expect("build standalone execute request");

    let response = runtime
        .block_on(async { tokio::time::timeout(EXCHANGE_TIMEOUT, client.execute(request)).await })
        .expect("execute exceeded outer bound")
        .expect("execute failed");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        runtime
            .block_on(response.bytes())
            .expect("collect execute body")
            .is_empty()
    );

    drop(client);
    let observation = server.finish().expect("execute fixture completed");
    let request = CapturedRequest::parse(&observation);
    assert_eq!(request.request_line, "POST /direct?source=task10 HTTP/1.1");
    assert_eq!(
        request.header_values("x-execute"),
        vec![&b"same-pipeline"[..]]
    );
    assert_eq!(request.body, b"execute");
}

#[test]
fn response_version_and_head_content_length_come_from_response_head() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(HEAD_RESPONSE);
    let client = Client::new().expect("build response metadata client");
    let response = send_response(&runtime, client.head(server.url()));

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_11);
    assert_eq!(response.content_length(), Some(7));
    assert!(
        runtime
            .block_on(response.bytes())
            .expect("collect HEAD body")
            .is_empty()
    );

    drop(client);
    let observation = server.finish().expect("HEAD metadata fixture completed");
    let request = CapturedRequest::parse(&observation);
    assert_eq!(request.request_line, "HEAD /direct?source=task10 HTTP/1.1");
}

#[test]
fn response_content_length_is_none_for_absent_and_chunked_metadata() {
    let runtime = runtime();

    for (wire_response, expected_body, close_after_write) in [
        (NO_LENGTH_RESPONSE, b"raw".as_slice(), true),
        (CHUNKED_METADATA_RESPONSE, b"ok".as_slice(), false),
    ] {
        let server = if close_after_write {
            ScriptedServer::spawn_close_after_response(wire_response)
        } else {
            ScriptedServer::spawn_with_response(wire_response)
        };
        let client = Client::new().expect("build response metadata client");
        let response = send_response(&runtime, client.get(server.url()));

        assert_eq!(response.content_length(), None);
        assert_eq!(
            runtime
                .block_on(async { tokio::time::timeout(EXCHANGE_TIMEOUT, response.bytes()).await })
                .expect("response without fixed length exceeded outer bound")
                .expect("collect response without fixed length"),
            expected_body
        );

        drop(client);
        server.finish().expect("metadata fixture completed");
    }
}

#[test]
fn response_text_decodes_the_complete_body_as_utf8_lossy() {
    let runtime = runtime();
    let mut first = b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\nsplit \xF0\x9F".to_vec();
    let tail = b"\x92\x96 bad \xFF".to_vec();
    let server = PhasedServer::spawn(std::mem::take(&mut first), vec![tail]);
    let client = Client::new().expect("build response text client");
    let response = send_response(&runtime, client.get(server.url()));
    server.wait_first();
    let release = server.release_after(Duration::from_millis(75));

    let text = runtime
        .block_on(async { tokio::time::timeout(PHASE_TIMEOUT, response.text()).await })
        .expect("response text exceeded outer bound")
        .expect("response text failed");
    release.join().expect("join text tail release");
    server.wait_tail(0);
    assert_eq!(text, "split 💖 bad \u{fffd}");

    drop(client);
    server.wait_peer_eof();
    let observation = server.finish().expect("response text fixture completed");
    assert!(
        observation
            .request_bytes
            .starts_with(b"GET /phased HTTP/1.1\r\n")
    );
}

#[test]
fn client_builder_default_timeout_applies_to_bound_send_and_last_setter_wins() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_LONG),
        })
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        })
        .build()
        .expect("build default-timeout client");

    let error = expect_deadline_error(
        send_deadline_request(&runtime, client.get(server.url())),
        "bound send ignored its client default timeout",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(client);
    let observation = server.finish().expect("default timeout fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn client_execute_uses_the_executing_clients_default_timeout() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let source_client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_LONG),
        })
        .build()
        .expect("build source client");
    let executing_client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        })
        .build()
        .expect("build executing client");
    let request = source_client
        .get(server.url())
        .build()
        .expect("build request without explicit timeout");

    let result = runtime
        .block_on(async {
            tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, executing_client.execute(request)).await
        })
        .expect("cross-client execute exceeded outer bound");
    let error = expect_deadline_error(
        result,
        "executing client default did not replace the source client default",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(source_client);
    drop(executing_client);
    let observation = server
        .finish()
        .expect("cross-client default timeout fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn client_execute_applies_its_default_to_a_standalone_request() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(DEADLINE_PHASE_DELAY);
    let executing_client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        })
        .build()
        .expect("build executing client");
    let request = RequestBuilder::new(Method::GET, server.url())
        .build()
        .expect("build standalone request without explicit timeout");

    let result = runtime
        .block_on(async {
            tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, executing_client.execute(request)).await
        })
        .expect("standalone execute exceeded outer bound");
    let error = expect_deadline_error(
        result,
        "standalone request did not inherit the executing client default",
    );
    assert_timeout_error(&error, "total", "response head");

    drop(executing_client);
    let observation = server
        .finish()
        .expect("standalone default timeout fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn partial_request_timeout_wholly_replaces_the_bound_client_default() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(Duration::from_millis(350));
    let client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        })
        .build()
        .expect("build default-timeout client");
    let request = client.get(server.url()).timeout(Timeout {
        connect: None,
        read: Some(DEADLINE_LONG),
        total: None,
    });

    let response = send_deadline_request(&runtime, request)
        .expect("partial request timeout incorrectly merged the client total timeout");
    assert_eq!(
        collect_deadline_body(&runtime, response).expect("collect partial-override response body"),
        Bytes::from_static(b"ok")
    );

    drop(client);
    let observation = server
        .finish()
        .expect("partial request timeout fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn explicit_request_timeout_wholly_replaces_the_executing_client_default() {
    let runtime = runtime();
    let server = DeadlineServer::delayed_head(Duration::from_millis(350));
    let source_client = Client::new().expect("build source client");
    let executing_client = Client::builder()
        .timeout(Timeout {
            connect: None,
            read: None,
            total: Some(DEADLINE_SHORT),
        })
        .build()
        .expect("build executing client");
    let request = source_client
        .get(server.url())
        .timeout(Timeout::default())
        .build()
        .expect("build request with explicit all-None timeout");

    let response = runtime
        .block_on(async {
            tokio::time::timeout(DEADLINE_OUTER_TIMEOUT, executing_client.execute(request)).await
        })
        .expect("explicit timeout execute exceeded outer bound")
        .expect("explicit all-None timeout did not disable the client default");
    assert_eq!(
        collect_deadline_body(&runtime, response).expect("collect override response body"),
        Bytes::from_static(b"ok")
    );

    drop(source_client);
    drop(executing_client);
    let observation = server
        .finish()
        .expect("explicit timeout override fixture completed");
    assert_deadline_observation(&observation);
}

#[test]
fn zero_pool_idle_limit_disables_reuse_and_last_setter_wins() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 2, 0);
    let client = Client::builder()
        .pool_max_idle_per_host(4)
        .pool_max_idle_per_host(0)
        .build()
        .expect("build zero-idle client");

    complete_exchange(&runtime, client.get(server.url("/builder-pool/first")));
    complete_exchange(&runtime, client.get(server.url("/builder-pool/second")));

    drop(client);
    let observation = server.finish().expect("zero-idle pool fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert_pool_requests(
        &observation,
        &[0, 1],
        &["/builder-pool/first", "/builder-pool/second"],
    );
}

#[test]
fn configured_proxy_fails_closed_after_the_client_is_dropped() {
    let runtime = runtime();
    let origin = ScriptedServer::spawn();
    let proxy = ScriptedServer::spawn();
    let proxy_uri = format!("http://{}", proxy.authority())
        .parse::<Uri>()
        .expect("valid local proxy URI");
    let client = Client::builder()
        .proxy(Proxy::Http(proxy_uri))
        .build()
        .expect("build configured-proxy client");
    let (body, probe) = TrackedBody::source(
        [Bytes::from_static(b"proxy-body")],
        Some(b"proxy-body".len() as u64),
    );
    let request = client.request(Method::POST, origin.url()).body(body);
    drop(client);

    let result = runtime
        .block_on(async { tokio::time::timeout(EXCHANGE_TIMEOUT, request.send()).await })
        .expect("configured proxy request exceeded outer bound");
    let error = match result {
        Err(error) => error,
        Ok(response) => {
            drop(response);
            panic!("configured proxy silently used the direct transport")
        }
    };
    assert_eq!(error.kind(), ErrorKind::Proxy);
    assert!(
        probe
            .polls
            .lock()
            .expect("proxy body poll log lock")
            .is_empty(),
        "configured proxy must fail before polling the request body"
    );

    let proxy_observation = proxy.finish().expect("proxy fail-closed fixture completed");
    assert_eq!(proxy_observation.accepted_connections, 0);
    assert!(proxy_observation.request_bytes.is_empty());
    let origin_observation = origin
        .finish()
        .expect("origin fail-closed fixture completed");
    assert_eq!(origin_observation.accepted_connections, 0);
    assert!(origin_observation.request_bytes.is_empty());
}

#[test]
fn top_level_helpers_use_wire_methods_and_fresh_clients_without_a_shared_pool() {
    let runtime = runtime();
    let server = PoolServer::spawn(PoolScript::KeepAlive, 6, 0);

    assert_eq!(
        complete_top_level_exchange(&runtime, requests::get(server.url("/top-level/get"))),
        Bytes::from_static(b"ok")
    );
    assert!(
        complete_top_level_exchange(&runtime, requests::head(server.url("/top-level/head")))
            .is_empty()
    );
    assert_eq!(
        complete_top_level_exchange(
            &runtime,
            requests::post(server.url("/top-level/post"), Vec::from(&b"post"[..])),
        ),
        Bytes::from_static(b"ok")
    );
    assert_eq!(
        complete_top_level_exchange(
            &runtime,
            requests::put(
                server.url("/top-level/put"),
                BodySource::Bytes(Bytes::from_static(b"put")),
            ),
        ),
        Bytes::from_static(b"ok")
    );
    assert_eq!(
        complete_top_level_exchange(
            &runtime,
            requests::patch(server.url("/top-level/patch"), Bytes::from_static(b"patch"),),
        ),
        Bytes::from_static(b"ok")
    );
    assert_eq!(
        complete_top_level_exchange(&runtime, requests::delete(server.url("/top-level/delete")),),
        Bytes::from_static(b"ok")
    );

    let observation = server.finish().expect("top-level helper fixture completed");
    assert_eq!(observation.accepted_connections, 6);
    assert_eq!(observation.connection_ids(), [0, 1, 2, 3, 4, 5]);
    let expected = [
        ("GET /top-level/get HTTP/1.1", b"".as_slice()),
        ("HEAD /top-level/head HTTP/1.1", b"".as_slice()),
        ("POST /top-level/post HTTP/1.1", b"post".as_slice()),
        ("PUT /top-level/put HTTP/1.1", b"put".as_slice()),
        ("PATCH /top-level/patch HTTP/1.1", b"patch".as_slice()),
        ("DELETE /top-level/delete HTTP/1.1", b"".as_slice()),
    ];
    for (observed, (request_line, body)) in observation.requests.iter().zip(expected) {
        let request = CapturedRequest::parse_bytes(&observed.request_bytes);
        assert_eq!(request.request_line, request_line);
        assert_eq!(request.body, body);
    }
}

#[test]
fn top_level_streamed_post_is_polled_on_the_callers_tokio_runtime_and_moved_once() {
    let runtime = runtime();
    let server = ScriptedServer::spawn_with_response(FRAMING_RESPONSE);
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let all_polls_on_caller_runtime_thread = Arc::new(AtomicBool::new(true));
    let body = BodySource::Stream(Box::pin(CallerTaskBody {
        chunk: Some(Bytes::from_static(b"caller")),
        length: 6,
        caller_thread: thread::current().id(),
        polls: Arc::clone(&polls),
        drops: Arc::clone(&drops),
        all_polls_on_caller_runtime_thread: Arc::clone(&all_polls_on_caller_runtime_thread),
    }));

    let response = runtime
        .block_on(async {
            tokio::time::timeout(EXCHANGE_TIMEOUT, requests::post(server.url(), body)).await
        })
        .expect("top-level POST exceeded outer bound")
        .expect("top-level POST failed");
    assert!(
        runtime
            .block_on(response.bytes())
            .expect("collect top-level POST body")
            .is_empty()
    );

    let observation = server.finish().expect("top-level POST fixture completed");
    let request = CapturedRequest::parse(&observation);
    assert_eq!(request.request_line, "POST /direct?source=task10 HTTP/1.1");
    assert_eq!(request.body, b"caller");
    assert!(polls.load(Ordering::Acquire) >= 1);
    assert!(all_polls_on_caller_runtime_thread.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_request_runs_inside_a_current_thread_tokio_runtime() {
    let mut server = ScriptedServer::spawn();
    let url = server.url();
    let result = bounded_blocking(
        move || {
            runtime().block_on(async move {
                let response = blocking::get(url)?;
                response.bytes()
            })
        },
        || server.signal_shutdown(),
    );
    let body = result.expect("blocking request inside Tokio runtime failed");

    let observation = server.finish().expect("blocking runtime fixture completed");
    let request = CapturedRequest::parse(&observation);
    assert_eq!(request.request_line, "GET /direct?source=task10 HTTP/1.1");
    assert_eq!(body, Bytes::from_static(b"direct\n"));
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_send_returns_after_head_and_response_body_read_streams() {
    let server = PhasedServer::spawn(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        vec![
            Vec::new(),
            b"6\r\nabcdef\r\n".to_vec(),
            b"4\r\nghij\r\n0\r\n\r\n".to_vec(),
        ],
    );
    let url = server.url();
    let response = bounded_blocking(
        move || {
            let client = blocking::Client::new()?;
            client.get(url).send()
        },
        || {
            let _ = server.commands.send(PhaseCommand::Close);
        },
    )
    .expect("blocking send failed before response body release");
    server.wait_first();

    let body = response.into_body();
    let (body, empty) = bounded_body_read(body, 0, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert!(empty.expect("empty blocking read failed").is_empty());

    server.release_next(0);
    server.release_next(1);
    let (body, first) = bounded_body_read(body, 2, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert_eq!(first.expect("first blocking read failed"), b"ab");
    let (body, middle) = bounded_body_read(body, 3, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert_eq!(middle.expect("middle blocking read failed"), b"cde");
    let (body, remainder) = bounded_body_read(body, 4, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert_eq!(remainder.expect("remainder blocking read failed"), b"f");

    server.release_next(2);
    let (body, tail) = bounded_body_read(body, 3, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert_eq!(tail.expect("tail blocking read failed"), b"ghi");
    let (body, tail_remainder) = bounded_body_read(body, 3, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert_eq!(
        tail_remainder.expect("tail remainder blocking read failed"),
        b"j"
    );
    let (body, eof) = bounded_body_read(body, 8, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert!(eof.expect("blocking EOF read failed").is_empty());
    let (body, repeated_eof) = bounded_body_read(body, 8, || {
        let _ = server.commands.send(PhaseCommand::Close);
    });
    assert!(
        repeated_eof
            .expect("repeated blocking EOF read failed")
            .is_empty()
    );
    drop(body);

    server.wait_peer_eof();
    let observation = server.finish().expect("blocking phased fixture completed");
    assert_eq!(observation.peer_eof_count, 1);
    assert!(
        observation
            .request_bytes
            .starts_with(b"GET /phased HTTP/1.1\r\n")
    );
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_response_metadata_bytes_and_text_match_wire() {
    const METADATA_RESPONSE: &[u8] = b"HTTP/1.1 206 Partial Content\r\n\
        x-blocking: metadata\r\nContent-Length: 5\r\nConnection: close\r\n\r\nbytes";
    const TEXT_RESPONSE: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntext \xFF";

    let mut metadata_server = ScriptedServer::spawn_with_response(METADATA_RESPONSE);
    let mut text_server = ScriptedServer::spawn_with_response(TEXT_RESPONSE);
    let metadata_url = metadata_server.url();
    let expected_url = metadata_url.clone();
    let text_url = text_server.url();
    let result = bounded_blocking(
        move || {
            let response = blocking::get(metadata_url)?;
            let status = response.status();
            let header = response.headers().get("x-blocking").cloned();
            let url = response.url().to_owned();
            let version = response.version();
            let content_length = response.content_length();
            let bytes = response.bytes()?;
            let text = blocking::get(text_url)?.text()?;
            Ok::<_, requests::Error>((status, header, url, version, content_length, bytes, text))
        },
        || {
            metadata_server.signal_shutdown();
            text_server.signal_shutdown();
        },
    )
    .expect("blocking response collection failed");

    metadata_server
        .finish()
        .expect("blocking metadata fixture completed");
    text_server
        .finish()
        .expect("blocking text fixture completed");
    let (status, header, url, version, content_length, bytes, text) = result;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header.as_ref().map(HeaderValue::as_bytes),
        Some(&b"metadata"[..])
    );
    assert_eq!(url, expected_url);
    assert_eq!(version, Version::HTTP_11);
    assert_eq!(content_length, Some(5));
    assert_eq!(bytes, Bytes::from_static(b"bytes"));
    assert_eq!(text, "text \u{fffd}");
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_full_body_read_reuses_one_connection() {
    let mut server = PoolServer::spawn(PoolScript::KeepAlive, 2, 0);
    let first_url = server.url("/blocking-pool/full");
    let second_url = server.url("/blocking-pool/after-full");
    let result = bounded_blocking(
        move || {
            let client = blocking::Client::new()?;
            let mut body = client.get(first_url).send()?.into_body();
            let mut first = Vec::new();
            body.read_to_end(&mut first)
                .expect("read complete blocking response body");
            let second = client.get(second_url).send()?.bytes()?;
            Ok::<_, requests::Error>((first, second))
        },
        || server.signal_shutdown(),
    )
    .expect("blocking pooled exchange failed");

    let observation = server
        .finish()
        .expect("blocking clean pool fixture completed");
    assert_eq!(result.0, b"ok");
    assert_eq!(result.1, Bytes::from_static(b"ok"));
    assert_eq!(observation.accepted_connections, 1);
    assert_pool_requests(
        &observation,
        &[0, 0],
        &["/blocking-pool/full", "/blocking-pool/after-full"],
    );
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_partial_body_drop_forces_a_second_connection() {
    let mut server = PoolServer::spawn(PoolScript::HoldFirstBody, 2, 1);
    let first_url = server.url("/blocking-pool/drop");
    let second_url = server.url("/blocking-pool/after-drop");
    let result = bounded_blocking(
        move || {
            let client = blocking::Client::new()?;
            let mut body = client.get(first_url).send()?.into_body();
            let mut prefix = [0; 3];
            body.read_exact(&mut prefix)
                .expect("read partial blocking response body");
            drop(body);
            let second = client.get(second_url).send()?.bytes()?;
            Ok::<_, requests::Error>((prefix, second))
        },
        || server.signal_shutdown(),
    )
    .expect("blocking partial-drop exchange failed");

    let observation = server
        .finish()
        .expect("blocking partial-drop pool fixture completed");
    assert_eq!(result.0, *b"par");
    assert_eq!(result.1, Bytes::from_static(b"ok"));
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(
        &observation,
        &[0, 1],
        &["/blocking-pool/drop", "/blocking-pool/after-drop"],
    );
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_partial_body_close_forces_a_second_connection() {
    let mut server = PoolServer::spawn(PoolScript::HoldFirstBody, 2, 1);
    let first_url = server.url("/blocking-pool/close");
    let second_url = server.url("/blocking-pool/after-close");
    let result = bounded_blocking(
        move || {
            let client = blocking::Client::new()?;
            let mut body = client.get(first_url).send()?.into_body();
            let mut prefix = [0; 3];
            body.read_exact(&mut prefix)
                .expect("read partial blocking response body");
            body.close()?;
            let second = client.get(second_url).send()?.bytes()?;
            Ok::<_, requests::Error>((prefix, second))
        },
        || server.signal_shutdown(),
    )
    .expect("blocking partial-close exchange failed");

    let observation = server
        .finish()
        .expect("blocking partial-close pool fixture completed");
    assert_eq!(result.0, *b"par");
    assert_eq!(result.1, Bytes::from_static(b"ok"));
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.peer_closed_connections.contains(&0));
    assert_pool_requests(
        &observation,
        &[0, 1],
        &["/blocking-pool/close", "/blocking-pool/after-close"],
    );
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_execute_client_and_top_level_helpers_preserve_wire_contracts() {
    let mut server = PoolServer::spawn(PoolScript::KeepAlive, 13, 0);
    let address = server.address;
    let result = bounded_blocking(
        move || {
            let url = |path: &str| format!("http://{address}{path}");
            let collect = |response: requests::Result<blocking::Response>| {
                response.and_then(blocking::Response::bytes)
            };
            let client = blocking::Client::new()?;
            let execute_request = RequestBuilder::new(Method::POST, url("/blocking/execute"))
                .body(Vec::from(&b"execute"[..]))
                .build()?;
            let bodies = vec![
                client.execute(execute_request)?.bytes()?,
                collect(client.get(url("/blocking/client/get")).send())?,
                collect(client.head(url("/blocking/client/head")).send())?,
                collect(
                    client
                        .post(url("/blocking/client/post"))
                        .body(Vec::from(&b"client-post"[..]))
                        .send(),
                )?,
                collect(
                    client
                        .put(url("/blocking/client/put"))
                        .body(BodySource::Bytes(Bytes::from_static(b"client-put")))
                        .send(),
                )?,
                collect(
                    client
                        .patch(url("/blocking/client/patch"))
                        .body(Vec::from(&b"client-patch"[..]))
                        .send(),
                )?,
                collect(client.delete(url("/blocking/client/delete")).send())?,
                blocking::get(url("/blocking/top/get"))?.bytes()?,
                blocking::head(url("/blocking/top/head"))?.bytes()?,
                blocking::post(url("/blocking/top/post"), Vec::from(&b"top-post"[..]))?.bytes()?,
                blocking::put(
                    url("/blocking/top/put"),
                    BodySource::Bytes(Bytes::from_static(b"top-put")),
                )?
                .bytes()?,
                blocking::patch(url("/blocking/top/patch"), Vec::from(&b"top-patch"[..]))?
                    .bytes()?,
                blocking::delete(url("/blocking/top/delete"))?.bytes()?,
            ];

            let client_error = match client.get("/relative-client").send() {
                Err(error) => error,
                Ok(response) => {
                    drop(response);
                    panic!("blocking client accepted a relative URL")
                }
            };
            let top_level_error = match blocking::get("/relative-top-level") {
                Err(error) => error,
                Ok(response) => {
                    drop(response);
                    panic!("blocking top-level helper accepted a relative URL")
                }
            };
            Ok::<_, requests::Error>((bodies, client_error.kind(), top_level_error.kind()))
        },
        || server.signal_shutdown(),
    )
    .expect("blocking convenience exchanges failed");

    let observation = server
        .finish()
        .expect("blocking convenience pool fixture completed");
    assert_eq!(result.1, ErrorKind::InvalidUrl);
    assert_eq!(result.2, ErrorKind::InvalidUrl);
    let expected_response_bodies = [
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
        b"ok".as_slice(),
    ];
    for (body, expected) in result.0.iter().zip(expected_response_bodies) {
        assert_eq!(body.as_ref(), expected);
    }

    assert_eq!(observation.accepted_connections, 7);
    assert_eq!(
        observation.connection_ids(),
        [0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6]
    );
    let expected_requests = [
        ("POST /blocking/execute HTTP/1.1", b"execute".as_slice()),
        ("GET /blocking/client/get HTTP/1.1", b"".as_slice()),
        ("HEAD /blocking/client/head HTTP/1.1", b"".as_slice()),
        (
            "POST /blocking/client/post HTTP/1.1",
            b"client-post".as_slice(),
        ),
        (
            "PUT /blocking/client/put HTTP/1.1",
            b"client-put".as_slice(),
        ),
        (
            "PATCH /blocking/client/patch HTTP/1.1",
            b"client-patch".as_slice(),
        ),
        ("DELETE /blocking/client/delete HTTP/1.1", b"".as_slice()),
        ("GET /blocking/top/get HTTP/1.1", b"".as_slice()),
        ("HEAD /blocking/top/head HTTP/1.1", b"".as_slice()),
        ("POST /blocking/top/post HTTP/1.1", b"top-post".as_slice()),
        ("PUT /blocking/top/put HTTP/1.1", b"top-put".as_slice()),
        (
            "PATCH /blocking/top/patch HTTP/1.1",
            b"top-patch".as_slice(),
        ),
        ("DELETE /blocking/top/delete HTTP/1.1", b"".as_slice()),
    ];
    assert_eq!(observation.requests.len(), expected_requests.len());
    for (observed, (request_line, body)) in observation.requests.iter().zip(expected_requests) {
        let request = CapturedRequest::parse_bytes(&observed.request_bytes);
        assert_eq!(request.request_line, request_line);
        assert_eq!(request.body, body);
    }
}

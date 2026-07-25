use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use requests::{Client, HeaderName, HeaderValue, StatusCode};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const SCRIPTED_RESPONSE: &[u8] =
    b"HTTP/1.1 201 Created\r\nx-fixture: direct\r\nContent-Length: 7\r\n\r\ndirect\n";

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
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make fixture listener nonblocking");
        let address = listener.local_addr().expect("read fixture address");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let worker = thread::spawn(move || serve(listener, &shutdown_rx));

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

fn serve(listener: TcpListener, shutdown: &Receiver<()>) -> Result<Observation, String> {
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("fixture accept failed: {error}")),
        }

        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                return Err("fixture shut down before accepting a connection".to_owned());
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

    let mut request_bytes = read_through_request_head(&mut stream)?;
    stream
        .write_all(SCRIPTED_RESPONSE)
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
                drain_request_bytes(&mut stream, &mut request_bytes)?;
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

fn read_through_request_head(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];

    loop {
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
        if request.len() >= MAX_REQUEST_HEAD_BYTES {
            return Err(format!(
                "request head exceeded {MAX_REQUEST_HEAD_BYTES} bytes"
            ));
        }

        let remaining = MAX_REQUEST_HEAD_BYTES - request.len();
        let read_limit = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..read_limit])
            .map_err(|error| format!("read request head: {error}"))?;
        if read == 0 {
            return Err("client closed before completing request head".to_owned());
        }
        request.extend_from_slice(&buffer[..read]);
    }
}

fn drain_request_bytes(stream: &mut TcpStream, request: &mut Vec<u8>) -> Result<(), String> {
    let mut buffer = [0_u8; 1024];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if request.len() + read > MAX_REQUEST_BYTES {
                    return Err(format!(
                        "request exceeded {MAX_REQUEST_BYTES} bytes while checking for a GET body"
                    ));
                }
                request.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("drain request bytes: {error}")),
        }
    }
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

    let exchange = runtime.block_on(tokio::time::timeout(EXCHANGE_TIMEOUT, async {
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
    }));

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

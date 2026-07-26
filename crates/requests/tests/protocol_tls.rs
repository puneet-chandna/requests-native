use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use requests::{CertificateSource, Client, StatusCode, TlsConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST_HEAD: usize = 16 * 1024;
const VALID_SERVER: ServerIdentity = ServerIdentity {
    certificate_chain: include_bytes!("../../../tests/certs/valid/server/server.pem"),
    private_key: include_bytes!("../../../tests/certs/valid/server/server.key"),
};
const EXPIRED_SERVER: ServerIdentity = ServerIdentity {
    certificate_chain: include_bytes!("../../../tests/certs/expired/server/server.pem"),
    private_key: include_bytes!("../../../tests/certs/expired/server/server.key"),
};
const WRONG_HOST_SERVER: ServerIdentity = ServerIdentity {
    certificate_chain: include_bytes!("../../../tests/fixtures/tls/wrong-host/wrong-host.pem"),
    private_key: include_bytes!("../../../tests/fixtures/tls/wrong-host/wrong-host.key"),
};
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntls-ok";

#[derive(Clone, Copy)]
struct ServerIdentity {
    certificate_chain: &'static [u8],
    private_key: &'static [u8],
}

#[derive(Debug, Eq, PartialEq)]
enum ServerEvent {
    TlsCompleted,
    HttpRequestRead,
}

#[derive(Debug)]
struct TlsObservation {
    tls_completed: bool,
    alpn: Option<Vec<u8>>,
    request_bytes: Vec<u8>,
    handshake_error: Option<String>,
    events: Vec<ServerEvent>,
}

#[derive(Clone, Copy)]
enum ExpectedClientResult {
    Success,
    TlsFailure,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build TLS contract runtime")
}

fn repository_fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests")
        .join(path)
}

fn frozen_ca_bundle() -> PathBuf {
    repository_fixture("certs/expired/ca/ca.crt")
}

fn tls_acceptor(identity: ServerIdentity) -> TlsAcceptor {
    let mut certificates = Cursor::new(identity.certificate_chain);
    let certificates = rustls_pemfile::certs(&mut certificates)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse frozen server certificate chain");
    let mut private_key = Cursor::new(identity.private_key);
    let private_key = rustls_pemfile::private_key(&mut private_key)
        .expect("parse frozen server private key")
        .expect("frozen server private key exists");
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .expect("frozen server certificate matches private key");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsAcceptor::from(Arc::new(config))
}

async fn read_request_head<S>(stream: &mut S) -> Result<Vec<u8>, String>
where
    S: AsyncRead + Unpin,
{
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() == MAX_REQUEST_HEAD {
            return Err("TLS request head exceeded fixture bound".to_owned());
        }
        let read = stream
            .read(&mut byte)
            .await
            .map_err(|error| format!("read TLS request: {error}"))?;
        if read == 0 {
            return Err("TLS peer closed before complete HTTP request head".to_owned());
        }
        request.push(byte[0]);
    }
    Ok(request)
}

async fn serve_one_tls(
    listener: TcpListener,
    identity: ServerIdentity,
) -> Result<TlsObservation, String> {
    let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "TLS fixture accept timed out".to_owned())?
        .map_err(|error| format!("accept TLS fixture connection: {error}"))?;
    let accepted = tokio::time::timeout(IO_TIMEOUT, tls_acceptor(identity).accept(stream))
        .await
        .map_err(|_| "TLS fixture handshake timed out".to_owned())?;
    let mut stream = match accepted {
        Ok(stream) => stream,
        Err(error) => {
            return Ok(TlsObservation {
                tls_completed: false,
                alpn: None,
                request_bytes: Vec::new(),
                handshake_error: Some(error.to_string()),
                events: Vec::new(),
            });
        }
    };

    let mut events = vec![ServerEvent::TlsCompleted];
    let alpn = stream.get_ref().1.alpn_protocol().map(ToOwned::to_owned);
    let request_bytes = tokio::time::timeout(IO_TIMEOUT, read_request_head(&mut stream))
        .await
        .map_err(|_| "TLS fixture request read timed out".to_owned())??;
    events.push(ServerEvent::HttpRequestRead);
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(RESPONSE))
        .await
        .map_err(|_| "TLS fixture response write timed out".to_owned())?
        .map_err(|error| format!("write TLS fixture response: {error}"))?;
    tokio::time::timeout(IO_TIMEOUT, stream.shutdown())
        .await
        .map_err(|_| "TLS fixture response shutdown timed out".to_owned())?
        .map_err(|error| format!("shutdown TLS fixture response: {error}"))?;
    Ok(TlsObservation {
        tls_completed: true,
        alpn,
        request_bytes,
        handshake_error: None,
        events,
    })
}

fn assert_pre_socket_bundle_failure(bundle: PathBuf) {
    let client = Client::builder()
        .tls(TlsConfig {
            roots: CertificateSource::PemBundle(bundle),
            identity: None,
        })
        .build()
        .expect("client construction remains path-blind");

    let error = runtime().block_on(async {
        match tokio::time::timeout(
            IO_TIMEOUT,
            client.get("https://no-socket.invalid/check").send(),
        )
        .await
        .expect("pre-socket TLS bundle failure timed out")
        {
            Ok(_) => panic!("invalid CA bundle unexpectedly returned HTTP"),
            Err(error) => error,
        }
    });
    let kind = format!("{:?}", error.kind());
    assert_ne!(
        kind, "Dns",
        "invalid CA bundle must not fall through to DNS"
    );
    assert_ne!(
        kind, "Connect",
        "invalid CA bundle must not fall through to connect"
    );
    assert_eq!(
        kind, "Tls",
        "invalid CA bundle must be a TLS error: {error}"
    );
}

#[test]
fn pre_socket_missing_ca_bundle_is_tls_error() {
    let bundle = repository_fixture("fixtures/tls/missing-ca.pem");
    assert!(!bundle.exists(), "missing-bundle fixture must stay absent");
    assert_pre_socket_bundle_failure(bundle);
}

#[test]
fn pre_socket_empty_ca_bundle_is_tls_error() {
    let bundle = repository_fixture("fixtures/tls/empty-ca.pem");
    assert!(
        std::fs::read_to_string(&bundle)
            .expect("read tracked empty-bundle fixture")
            .trim()
            .is_empty(),
        "tracked empty-bundle fixture must contain only whitespace"
    );
    assert_pre_socket_bundle_failure(bundle);
}

#[test]
fn pre_socket_garbage_ca_bundle_is_tls_error() {
    assert_pre_socket_bundle_failure(repository_fixture("fixtures/tls/garbage-ca.pem"));
}

fn run_tls_case(
    identity: ServerIdentity,
    roots: CertificateSource,
    expected: ExpectedClientResult,
) {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind TLS loopback fixture");
        let address = listener.local_addr().expect("read TLS fixture address");
        let server = tokio::spawn(serve_one_tls(listener, identity));
        let client = Client::builder()
            .tls(TlsConfig {
                roots,
                identity: None,
            })
            .build()
            .expect("build TLS client without I/O");
        let url = format!("https://localhost:{}/encrypted", address.port());
        let sent = tokio::time::timeout(IO_TIMEOUT, client.get(&url).send()).await;

        match expected {
            ExpectedClientResult::Success => {
                let response = match sent {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        server.abort();
                        let _ = server.await;
                        panic!(
                            "HTTPS failed before encrypted HTTP/1 exchange: {:?}: {error}",
                            error.kind()
                        );
                    }
                    Err(_) => {
                        server.abort();
                        let _ = server.await;
                        panic!("HTTPS request timed out");
                    }
                };
                assert_eq!(response.status(), StatusCode::OK);
                let body = tokio::time::timeout(IO_TIMEOUT, response.bytes())
                    .await
                    .expect("TLS response body timed out")
                    .expect("read TLS response body");
                assert_eq!(body.as_ref(), b"tls-ok");

                let observation = await_server(server).await;
                assert!(observation.tls_completed);
                assert_eq!(observation.handshake_error, None);
                assert_eq!(
                    observation.events,
                    [ServerEvent::TlsCompleted, ServerEvent::HttpRequestRead]
                );
                assert_eq!(observation.alpn.as_deref(), Some(b"http/1.1".as_slice()));
                assert!(
                    observation
                        .request_bytes
                        .starts_with(b"GET /encrypted HTTP/1.1\r\n"),
                    "unexpected encrypted HTTP request: {:?}",
                    String::from_utf8_lossy(&observation.request_bytes)
                );
                let expected_host = format!("host: localhost:{}\r\n", address.port());
                assert!(
                    String::from_utf8_lossy(&observation.request_bytes)
                        .to_ascii_lowercase()
                        .contains(&expected_host),
                    "encrypted request must carry the explicit loopback authority"
                );
            }
            ExpectedClientResult::TlsFailure => {
                let error = match sent {
                    Ok(Err(error)) => error,
                    Ok(Ok(_)) => {
                        server.abort();
                        let _ = server.await;
                        panic!("TLS verification failure unexpectedly returned HTTP");
                    }
                    Err(_) => {
                        server.abort();
                        let _ = server.await;
                        panic!("TLS verification failure timed out");
                    }
                };
                let kind = format!("{:?}", error.kind());
                assert_ne!(kind, "Dns", "TLS rejection must not be a DNS error");
                assert_ne!(kind, "Connect", "TLS rejection must not be a connect error");
                if kind != "Tls" {
                    server.abort();
                    let _ = server.await;
                    panic!("verification failure must be TLS: {error}");
                }
                assert_eq!(kind, "Tls", "verification failure must be TLS: {error}");

                let observation = await_server(server).await;
                assert!(!observation.tls_completed);
                assert_eq!(observation.alpn, None);
                assert!(observation.request_bytes.is_empty());
                assert!(observation.events.is_empty());
                assert!(
                    observation.handshake_error.is_some(),
                    "server must observe the client rejecting the TLS handshake"
                );
            }
        }
    });
}

async fn await_server(
    server: tokio::task::JoinHandle<Result<TlsObservation, String>>,
) -> TlsObservation {
    tokio::time::timeout(IO_TIMEOUT, server)
        .await
        .expect("TLS fixture task timed out")
        .expect("TLS fixture task panicked")
        .expect("TLS fixture failed")
}

#[test]
fn pem_bundle_https_uses_encrypted_http1_transport() {
    run_tls_case(
        VALID_SERVER,
        CertificateSource::PemBundle(frozen_ca_bundle()),
        ExpectedClientResult::Success,
    );
}

#[test]
fn platform_roots_reject_local_untrusted_server_as_tls() {
    run_tls_case(
        VALID_SERVER,
        CertificateSource::Platform,
        ExpectedClientResult::TlsFailure,
    );
}

#[test]
fn pem_bundle_rejects_expired_server_as_tls() {
    run_tls_case(
        EXPIRED_SERVER,
        CertificateSource::PemBundle(frozen_ca_bundle()),
        ExpectedClientResult::TlsFailure,
    );
}

#[test]
fn pem_bundle_rejects_wrong_hostname_as_tls() {
    run_tls_case(
        WRONG_HOST_SERVER,
        CertificateSource::PemBundle(frozen_ca_bundle()),
        ExpectedClientResult::TlsFailure,
    );
}

#[test]
fn disabled_verification_keeps_untrusted_server_encrypted() {
    run_tls_case(
        VALID_SERVER,
        CertificateSource::Disabled,
        ExpectedClientResult::Success,
    );
}

#[test]
fn disabled_verification_accepts_expired_server_encrypted() {
    run_tls_case(
        EXPIRED_SERVER,
        CertificateSource::Disabled,
        ExpectedClientResult::Success,
    );
}

#[test]
fn disabled_verification_accepts_wrong_hostname_encrypted() {
    run_tls_case(
        WRONG_HOST_SERVER,
        CertificateSource::Disabled,
        ExpectedClientResult::Success,
    );
}

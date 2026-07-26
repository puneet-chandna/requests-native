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
const SERVER_CERTIFICATE_CHAIN: &[u8] =
    include_bytes!("../../../tests/certs/valid/server/server.pem");
const SERVER_PRIVATE_KEY: &[u8] = include_bytes!("../../../tests/certs/valid/server/server.key");
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntls-ok";

#[derive(Debug)]
struct TlsObservation {
    tls_completed: bool,
    alpn: Option<Vec<u8>>,
    request_bytes: Vec<u8>,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build TLS contract runtime")
}

fn frozen_ca_bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/certs/expired/ca/ca.crt")
}

fn tls_acceptor() -> TlsAcceptor {
    let mut certificates = Cursor::new(SERVER_CERTIFICATE_CHAIN);
    let certificates = rustls_pemfile::certs(&mut certificates)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse frozen server certificate chain");
    let mut private_key = Cursor::new(SERVER_PRIVATE_KEY);
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

async fn serve_one_tls(listener: TcpListener) -> Result<TlsObservation, String> {
    let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "TLS fixture accept timed out".to_owned())?
        .map_err(|error| format!("accept TLS fixture connection: {error}"))?;
    let mut stream = tokio::time::timeout(IO_TIMEOUT, tls_acceptor().accept(stream))
        .await
        .map_err(|_| "TLS fixture handshake timed out".to_owned())?
        .map_err(|error| format!("complete TLS fixture handshake: {error}"))?;
    let alpn = stream.get_ref().1.alpn_protocol().map(ToOwned::to_owned);
    let request_bytes = tokio::time::timeout(IO_TIMEOUT, read_request_head(&mut stream))
        .await
        .map_err(|_| "TLS fixture request read timed out".to_owned())??;
    stream
        .write_all(RESPONSE)
        .await
        .map_err(|error| format!("write TLS fixture response: {error}"))?;
    stream
        .shutdown()
        .await
        .map_err(|error| format!("shutdown TLS fixture response: {error}"))?;
    Ok(TlsObservation {
        tls_completed: true,
        alpn,
        request_bytes,
    })
}

#[test]
fn pem_bundle_https_uses_encrypted_http1_transport() {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind TLS loopback fixture");
        let address = listener.local_addr().expect("read TLS fixture address");
        let server = tokio::spawn(serve_one_tls(listener));
        let client = Client::builder()
            .tls(TlsConfig {
                roots: CertificateSource::PemBundle(frozen_ca_bundle()),
                identity: None,
            })
            .build()
            .expect("build PEM-bundle client without I/O");
        let url = format!("https://localhost:{}/encrypted", address.port());

        let sent = tokio::time::timeout(IO_TIMEOUT, client.get(&url).send()).await;
        let response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                server.abort();
                let _ = server.await;
                panic!(
                    "PEM-bundle HTTPS failed before encrypted HTTP/1 exchange: {:?}: {error}",
                    error.kind()
                );
            }
            Err(_) => {
                server.abort();
                let _ = server.await;
                panic!("PEM-bundle HTTPS request timed out");
            }
        };
        assert_eq!(response.status(), StatusCode::OK);
        let body = tokio::time::timeout(IO_TIMEOUT, response.bytes())
            .await
            .expect("TLS response body timed out")
            .expect("read TLS response body");
        assert_eq!(body.as_ref(), b"tls-ok");

        let observation = tokio::time::timeout(IO_TIMEOUT, server)
            .await
            .expect("TLS fixture task timed out")
            .expect("TLS fixture task panicked")
            .expect("TLS fixture failed");
        assert!(observation.tls_completed);
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
    });
}

use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use requests::{CertificateSource, Client, Identity, StatusCode, TlsConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST_HEAD: usize = 16 * 1024;
const FROZEN_CA_CERTIFICATE: &[u8] = include_bytes!("../../../tests/certs/expired/ca/ca.crt");
const MTLS_CLIENT_CERTIFICATE: &[u8] =
    include_bytes!("../../../tests/fixtures/tls/mtls-client/client.pem");
const MTLS_CLIENT_CHAIN: &[u8] =
    include_bytes!("../../../tests/fixtures/tls/mtls-client/client-chain.pem");
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
const POOLED_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: keep-alive\r\n\r\ntls-pool";
static NEXT_CAPATH_DIRECTORY: AtomicU64 = AtomicU64::new(0);

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
    protocol_version: Option<rustls::ProtocolVersion>,
    peer_certificates: Vec<Vec<u8>>,
    request_bytes: Vec<u8>,
    handshake_error: Option<String>,
    events: Vec<ServerEvent>,
}

#[derive(Debug)]
struct PooledTlsObservation {
    accepts: usize,
    request_bytes: Vec<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum ExpectedClientResult {
    Success,
    TlsFailure,
    RequiredIdentityFailure,
}

#[derive(Clone, Copy)]
enum ClientAuthentication {
    None,
    Required,
}

struct CapathDirectory {
    path: PathBuf,
}

impl CapathDirectory {
    fn new() -> Self {
        let sequence = NEXT_CAPATH_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock precedes Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "requests-protocol-tls-capath-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create unique capath fixture directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, basename: &str, contents: &[u8]) -> PathBuf {
        assert_eq!(
            Path::new(basename).components().count(),
            1,
            "capath fixture writes only direct entries"
        );
        let path = self.path.join(basename);
        std::fs::write(&path, contents).expect("write capath fixture entry");
        path
    }

    fn directory(&self, basename: &str) -> PathBuf {
        assert_eq!(
            Path::new(basename).components().count(),
            1,
            "capath fixture creates only direct entries"
        );
        let path = self.path.join(basename);
        std::fs::create_dir(&path).expect("create portable directory-as-file read failure");
        path
    }
}

impl Drop for CapathDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

fn mtls_client_fixture(filename: &str) -> PathBuf {
    repository_fixture(&format!("fixtures/tls/mtls-client/{filename}"))
}

fn separate_mtls_identity() -> Identity {
    Identity {
        certificate_chain: mtls_client_fixture("client-chain.pem"),
        private_key: Some(mtls_client_fixture("client.key")),
    }
}

fn combined_mtls_identity() -> Identity {
    Identity {
        certificate_chain: mtls_client_fixture("client-combined.pem"),
        private_key: None,
    }
}

fn pem_certificate_der(bytes: &[u8], label: &str) -> Vec<Vec<u8>> {
    let mut cursor = Cursor::new(bytes);
    rustls_pemfile::certs(&mut cursor)
        .map(|certificate| {
            certificate
                .unwrap_or_else(|error| panic!("parse {label} certificate: {error}"))
                .as_ref()
                .to_vec()
        })
        .collect()
}

fn tls_acceptor(
    identity: ServerIdentity,
    client_authentication: ClientAuthentication,
) -> TlsAcceptor {
    let mut certificates = Cursor::new(identity.certificate_chain);
    let certificates = rustls_pemfile::certs(&mut certificates)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse frozen server certificate chain");
    let mut private_key = Cursor::new(identity.private_key);
    let private_key = rustls_pemfile::private_key(&mut private_key)
        .expect("parse frozen server private key")
        .expect("frozen server private key exists");
    let builder = rustls::ServerConfig::builder();
    let builder = match client_authentication {
        ClientAuthentication::None => builder.with_no_client_auth(),
        ClientAuthentication::Required => {
            let mut roots = rustls::RootCertStore::empty();
            let mut root = Cursor::new(FROZEN_CA_CERTIFICATE);
            for certificate in rustls_pemfile::certs(&mut root) {
                roots
                    .add(certificate.expect("parse frozen client-authentication root"))
                    .expect("add frozen client-authentication root");
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .expect("build required client-certificate verifier");
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder
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
    client_authentication: ClientAuthentication,
) -> Result<TlsObservation, String> {
    let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "TLS fixture accept timed out".to_owned())?
        .map_err(|error| format!("accept TLS fixture connection: {error}"))?;
    let accepted = tokio::time::timeout(
        IO_TIMEOUT,
        tls_acceptor(identity, client_authentication).accept(stream),
    )
    .await
    .map_err(|_| "TLS fixture handshake timed out".to_owned())?;
    let mut stream = match accepted {
        Ok(stream) => stream,
        Err(error) => {
            return Ok(TlsObservation {
                tls_completed: false,
                alpn: None,
                protocol_version: None,
                peer_certificates: Vec::new(),
                request_bytes: Vec::new(),
                handshake_error: Some(error.to_string()),
                events: Vec::new(),
            });
        }
    };

    let mut events = vec![ServerEvent::TlsCompleted];
    let alpn = stream.get_ref().1.alpn_protocol().map(ToOwned::to_owned);
    let protocol_version = stream.get_ref().1.protocol_version();
    let peer_certificates = stream
        .get_ref()
        .1
        .peer_certificates()
        .unwrap_or_default()
        .iter()
        .map(|certificate| certificate.as_ref().to_vec())
        .collect();
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
        protocol_version,
        peer_certificates,
        request_bytes,
        handshake_error: None,
        events,
    })
}

async fn serve_two_tls_requests_on_one_connection(
    listener: TcpListener,
) -> Result<PooledTlsObservation, String> {
    let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "pooled TLS fixture first accept timed out".to_owned())?
        .map_err(|error| format!("accept pooled TLS fixture connection: {error}"))?;
    let mut stream = tokio::time::timeout(
        IO_TIMEOUT,
        tls_acceptor(VALID_SERVER, ClientAuthentication::None).accept(stream),
    )
    .await
    .map_err(|_| "pooled TLS fixture handshake timed out".to_owned())?
    .map_err(|error| format!("pooled TLS fixture handshake failed: {error}"))?;

    let serving = async move {
        let mut request_bytes = Vec::with_capacity(2);
        for request_number in 1..=2 {
            let request = tokio::time::timeout(IO_TIMEOUT, read_request_head(&mut stream))
                .await
                .map_err(|_| {
                    format!("pooled TLS fixture request {request_number} read timed out")
                })??;
            request_bytes.push(request);
            tokio::time::timeout(IO_TIMEOUT, stream.write_all(POOLED_RESPONSE))
                .await
                .map_err(|_| {
                    format!("pooled TLS fixture response {request_number} write timed out")
                })?
                .map_err(|error| {
                    format!("write pooled TLS fixture response {request_number}: {error}")
                })?;
        }
        tokio::time::timeout(IO_TIMEOUT, stream.shutdown())
            .await
            .map_err(|_| "pooled TLS fixture shutdown timed out".to_owned())?
            .map_err(|error| format!("shutdown pooled TLS fixture: {error}"))?;
        Ok(PooledTlsObservation {
            accepts: 1,
            request_bytes,
        })
    };
    tokio::pin!(serving);
    let second_accept = tokio::time::timeout(IO_TIMEOUT, listener.accept());
    tokio::pin!(second_accept);

    tokio::select! {
        result = &mut serving => result,
        accepted = &mut second_accept => match accepted {
            Ok(Ok((_, peer))) => Err(format!(
                "pooled TLS fixture rejected unexpected second TCP connection from {peer}"
            )),
            Ok(Err(error)) => Err(format!(
                "pooled TLS fixture second-accept monitor failed: {error}"
            )),
            Err(_) => Err(
                "pooled TLS fixture did not serve two requests within its accept bound".to_owned()
            ),
        },
    }
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

#[test]
fn pre_socket_root_bundle_read_failure_is_tls_error() {
    let directory = CapathDirectory::new();
    let bundle = directory.directory("unreadable-root.pem");
    assert_pre_socket_bundle_failure(bundle);
}

fn assert_mtls_pre_socket_identity_failure(identity: Identity, filename_tokens: &[&str]) {
    let client = Client::builder()
        .tls(TlsConfig {
            roots: CertificateSource::PemBundle(frozen_ca_bundle()),
            identity: Some(identity),
        })
        .build()
        .expect("client construction remains identity-path-blind");
    let error = runtime().block_on(async {
        match tokio::time::timeout(
            IO_TIMEOUT,
            client.get("https://no-socket.invalid/check").send(),
        )
        .await
        .expect("pre-socket mTLS identity failure timed out")
        {
            Ok(_) => panic!("invalid mTLS identity unexpectedly returned HTTP"),
            Err(error) => error,
        }
    });
    let kind = format!("{:?}", error.kind());
    assert_ne!(kind, "Dns", "invalid identity must not fall through to DNS");
    assert_ne!(
        kind, "Connect",
        "invalid identity must not fall through to connect"
    );
    assert_eq!(kind, "Tls", "invalid identity must be a TLS error: {error}");
    let message = error.to_string();
    for token in filename_tokens {
        assert!(
            message.contains(token),
            "TLS identity error must name {token}: {message}"
        );
    }
}

#[test]
fn mtls_pre_socket_missing_certificate_chain_is_tls_error() {
    let directory = CapathDirectory::new();
    let certificate_chain = directory.path().join("missing-client-chain.pem");
    assert!(
        !certificate_chain.exists(),
        "missing client certificate fixture must stay absent"
    );
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain,
            private_key: Some(mtls_client_fixture("client.key")),
        },
        &["missing-client-chain.pem"],
    );
}

#[test]
fn mtls_pre_socket_empty_certificate_chain_is_tls_error() {
    let directory = CapathDirectory::new();
    let certificate_chain = directory.write("empty-client-chain.pem", b"");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain,
            private_key: Some(mtls_client_fixture("client.key")),
        },
        &["empty-client-chain.pem"],
    );
}

#[test]
fn mtls_pre_socket_malformed_certificate_chain_is_tls_error() {
    let directory = CapathDirectory::new();
    let certificate_chain = directory.write("malformed-client-chain.pem", b"not a PEM certificate");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain,
            private_key: Some(mtls_client_fixture("client.key")),
        },
        &["malformed-client-chain.pem"],
    );
}

#[test]
fn mtls_pre_socket_certificate_chain_read_failure_is_tls_error() {
    let directory = CapathDirectory::new();
    let certificate_chain = directory.directory("unreadable-client-chain.pem");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain,
            private_key: Some(mtls_client_fixture("client.key")),
        },
        &["unreadable-client-chain.pem"],
    );
}

#[test]
fn mtls_pre_socket_certificate_only_combined_file_is_tls_error() {
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: None,
        },
        &["client-chain.pem"],
    );
}

#[test]
fn mtls_pre_socket_missing_separate_key_is_tls_error() {
    let directory = CapathDirectory::new();
    let private_key = directory.path().join("missing-client.key");
    assert!(
        !private_key.exists(),
        "missing client-key fixture must stay absent"
    );
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: Some(private_key),
        },
        &["missing-client.key"],
    );
}

#[test]
fn mtls_pre_socket_empty_separate_key_is_tls_error() {
    let directory = CapathDirectory::new();
    let private_key = directory.write("empty-client.key", b"");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: Some(private_key),
        },
        &["empty-client.key"],
    );
}

#[test]
fn mtls_pre_socket_malformed_separate_key_is_tls_error() {
    let directory = CapathDirectory::new();
    let private_key = directory.write("malformed-client.key", b"not a PEM private key");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: Some(private_key),
        },
        &["malformed-client.key"],
    );
}

#[test]
fn mtls_pre_socket_separate_key_read_failure_is_tls_error() {
    let directory = CapathDirectory::new();
    let private_key = directory.directory("unreadable-client.key");
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: Some(private_key),
        },
        &["unreadable-client.key"],
    );
}

#[test]
fn mtls_pre_socket_mismatched_certificate_and_key_is_tls_error() {
    assert_mtls_pre_socket_identity_failure(
        Identity {
            certificate_chain: mtls_client_fixture("client-chain.pem"),
            private_key: Some(repository_fixture("certs/valid/server/server.key")),
        },
        &[],
    );
}

fn capath_request_failure(directory: &CapathDirectory) -> (String, String) {
    let client = Client::builder()
        .tls(TlsConfig {
            roots: CertificateSource::PemDirectory(directory.path().to_path_buf()),
            identity: None,
        })
        .build()
        .expect("client construction remains path-blind");
    let error = runtime().block_on(async {
        match tokio::time::timeout(IO_TIMEOUT, client.get("https://127.0.0.1:0/check").send())
            .await
            .expect("capath policy request timed out")
        {
            Ok(_) => panic!("capath policy request unexpectedly returned HTTP"),
            Err(error) => error,
        }
    });
    (format!("{:?}", error.kind()), error.to_string())
}

fn assert_capath_accepted(directory: &CapathDirectory) {
    let (kind, error) = capath_request_failure(directory);
    assert_eq!(
        kind, "Connect",
        "accepted capath must finish loading and reach the local connector: {error}"
    );
}

fn assert_capath_rejected(directory: &CapathDirectory) -> String {
    let (kind, error) = capath_request_failure(directory);
    assert_ne!(kind, "Connect", "rejected capath must fail before connect");
    assert_eq!(kind, "Tls", "rejected capath must be a TLS error: {error}");
    error
}

#[test]
fn capath_accepts_lowercase_hash_shaped_direct_entry() {
    let directory = CapathDirectory::new();
    directory.write("117adfc4.0", FROZEN_CA_CERTIFICATE);
    assert_capath_accepted(&directory);
}

#[test]
fn capath_accepts_uppercase_hex_hash_shape() {
    let directory = CapathDirectory::new();
    directory.write("ABCDEF12.0", FROZEN_CA_CERTIFICATE);
    assert_capath_accepted(&directory);
}

#[test]
fn capath_accepts_multi_digit_nonnegative_suffix() {
    let directory = CapathDirectory::new();
    directory.write("117adfc4.123", FROZEN_CA_CERTIFICATE);
    assert_capath_accepted(&directory);
}

#[test]
fn capath_accepts_mixed_case_hex_and_leading_zero_multi_digit_suffix() {
    let directory = CapathDirectory::new();
    directory.write("0aBcDeF1.007", FROZEN_CA_CERTIFICATE);
    assert_capath_accepted(&directory);
}

#[cfg(unix)]
#[test]
fn capath_accepts_file_symlink_to_valid_root() {
    let directory = CapathDirectory::new();
    let target = directory.write("root.pem", FROZEN_CA_CERTIFICATE);
    std::os::unix::fs::symlink(target, directory.path().join("117adfc4.0"))
        .expect("create eligible capath file symlink");
    assert_capath_accepted(&directory);
}

#[test]
fn capath_ignores_unrelated_valid_pem_name() {
    let directory = CapathDirectory::new();
    directory.write("root.pem", FROZEN_CA_CERTIFICATE);
    assert_capath_rejected(&directory);
}

#[test]
fn capath_accepts_eligible_root_alongside_unrelated_valid_pem() {
    let directory = CapathDirectory::new();
    directory.write("root.pem", FROZEN_CA_CERTIFICATE);
    directory.write("117adfc4.0", FROZEN_CA_CERTIFICATE);
    assert_capath_accepted(&directory);
}

#[test]
fn capath_does_not_recurse_into_child_directory() {
    let directory = CapathDirectory::new();
    let child = directory.path().join("child");
    std::fs::create_dir(&child).expect("create capath child directory");
    std::fs::write(child.join("117adfc4.0"), FROZEN_CA_CERTIFICATE)
        .expect("write nested eligible certificate");
    assert_capath_rejected(&directory);
}

#[test]
fn capath_rejects_directory_without_entries() {
    let directory = CapathDirectory::new();
    assert_capath_rejected(&directory);
}

#[test]
fn capath_rejects_empty_eligible_entry() {
    let directory = CapathDirectory::new();
    directory.write("117adfc4.0", b"");
    assert_capath_rejected(&directory);
}

#[test]
fn capath_rejects_malformed_eligible_entry() {
    let directory = CapathDirectory::new();
    directory.write("117adfc4.0", b"not a PEM certificate");
    assert_capath_rejected(&directory);
}

#[test]
fn capath_rejects_eligible_entry_read_failure() {
    let directory = CapathDirectory::new();
    directory.directory("117adfc4.0");
    assert_capath_rejected(&directory);
}

#[cfg(unix)]
#[test]
fn capath_rejects_broken_eligible_symlink() {
    let directory = CapathDirectory::new();
    std::os::unix::fs::symlink(
        directory.path().join("missing-root.pem"),
        directory.path().join("117adfc4.0"),
    )
    .expect("create broken eligible capath symlink");
    assert_capath_rejected(&directory);
}

#[test]
fn capath_ignores_invalid_basenames() {
    let directory = CapathDirectory::new();
    for basename in [
        "117adfc.0",
        "117adfc40.0",
        "117adfcg.0",
        "117adfc4.-1",
        "117adfc4.+1",
        "117adfc4.",
        "117adfc4.1x",
        "117adfc4.nope",
    ] {
        directory.write(basename, FROZEN_CA_CERTIFICATE);
    }
    assert_capath_rejected(&directory);
}

#[test]
fn capath_ignores_non_ascii_numeric_hash_and_suffix_fields_even_with_valid_pem() {
    let directory = CapathDirectory::new();
    directory.write("١17adfc4.0", FROZEN_CA_CERTIFICATE);
    directory.write("１17adfc4.0", FROZEN_CA_CERTIFICATE);
    directory.write("117adfc4.١", FROZEN_CA_CERTIFICATE);
    directory.write("117adfc4.１２", FROZEN_CA_CERTIFICATE);
    assert_capath_rejected(&directory);
}

#[test]
fn capath_eagerly_rejects_malformed_entry_after_valid_root() {
    let directory = CapathDirectory::new();
    directory.write("00000000.0", FROZEN_CA_CERTIFICATE);
    directory.write("ffffffff.0", b"malformed later entry");

    let error = assert_capath_rejected(&directory);
    assert!(
        error.contains("ffffffff.0"),
        "TLS error must name the later malformed eligible entry: {error}"
    );
    assert!(
        !error.contains("00000000.0"),
        "valid first entry must not mask or cause the TLS error: {error}"
    );
}

#[test]
fn capath_reports_lexically_first_malformed_eligible_entry() {
    let directory = CapathDirectory::new();
    directory.write("ffffffff.2", b"malformed second");
    directory.write("00000000.10", b"malformed first");

    let error = assert_capath_rejected(&directory);
    assert!(
        error.contains("00000000.10"),
        "TLS error must name the lexically first eligible entry: {error}"
    );
    assert!(
        !error.contains("ffffffff.2"),
        "TLS error must stop at the lexically first malformed entry: {error}"
    );
}

fn run_tls_case(
    server_identity: ServerIdentity,
    roots: CertificateSource,
    expected: ExpectedClientResult,
) {
    run_tls_case_with_client_authentication(
        server_identity,
        ClientAuthentication::None,
        roots,
        None,
        expected,
        None,
    );
}

fn run_mtls_case(
    roots: CertificateSource,
    client_identity: Option<Identity>,
    expected: ExpectedClientResult,
) {
    let expected_peer_certificates = match expected {
        ExpectedClientResult::Success => Some(pem_certificate_der(
            MTLS_CLIENT_CHAIN,
            "ordered mTLS client chain",
        )),
        ExpectedClientResult::TlsFailure | ExpectedClientResult::RequiredIdentityFailure => None,
    };
    run_tls_case_with_client_authentication(
        VALID_SERVER,
        ClientAuthentication::Required,
        roots,
        client_identity,
        expected,
        expected_peer_certificates,
    );
}

fn run_tls_case_with_client_authentication(
    server_identity: ServerIdentity,
    client_authentication: ClientAuthentication,
    roots: CertificateSource,
    client_identity: Option<Identity>,
    expected: ExpectedClientResult,
    expected_peer_certificates: Option<Vec<Vec<u8>>>,
) {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind TLS loopback fixture");
        let address = listener.local_addr().expect("read TLS fixture address");
        let server = tokio::spawn(serve_one_tls(
            listener,
            server_identity,
            client_authentication,
        ));
        let client = Client::builder()
            .tls(TlsConfig {
                roots,
                identity: client_identity,
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
                match expected_peer_certificates {
                    Some(expected_peer_certificates) => {
                        let expected_leaf =
                            pem_certificate_der(MTLS_CLIENT_CERTIFICATE, "mTLS client leaf");
                        assert_eq!(expected_leaf.len(), 1);
                        assert_eq!(
                            observation.peer_certificates.first(),
                            expected_leaf.first(),
                            "peer leaf DER must equal the checked-in CN=requests certificate"
                        );
                        assert_eq!(
                            observation.peer_certificates, expected_peer_certificates,
                            "peer certificate DER sequence must preserve configured chain order"
                        );
                        assert_eq!(
                            observation.protocol_version,
                            Some(rustls::ProtocolVersion::TLSv1_3),
                            "mTLS success must keep TLS 1.3 enabled"
                        );
                    }
                    None => assert!(
                        observation.peer_certificates.is_empty(),
                        "server without client authentication must not record a peer identity"
                    ),
                }
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
            ExpectedClientResult::TlsFailure | ExpectedClientResult::RequiredIdentityFailure => {
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
                match expected {
                    ExpectedClientResult::TlsFailure => {
                        if kind != "Tls" {
                            server.abort();
                            let _ = server.await;
                            panic!("server verification failure must be TLS, got {kind}: {error}");
                        }
                    }
                    ExpectedClientResult::RequiredIdentityFailure => {
                        let allowed =
                            matches!(kind.as_str(), "Tls" | "Handshake" | "Send" | "Connection");
                        if !allowed {
                            server.abort();
                            let _ = server.await;
                            panic!(
                                "required client identity must fail after connect as \
                                 Tls, Handshake, Send, or Connection; got {kind}: {error}"
                            );
                        }
                    }
                    ExpectedClientResult::Success => unreachable!(),
                }

                let observation = await_server(server).await;
                assert!(!observation.tls_completed);
                assert_eq!(observation.alpn, None);
                assert_eq!(observation.protocol_version, None);
                assert!(observation.peer_certificates.is_empty());
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

async fn await_pooled_server(
    server: tokio::task::JoinHandle<Result<PooledTlsObservation, String>>,
) -> Result<PooledTlsObservation, String> {
    tokio::time::timeout(IO_TIMEOUT, server)
        .await
        .map_err(|_| "pooled TLS fixture task timed out".to_owned())?
        .map_err(|error| format!("pooled TLS fixture task panicked: {error}"))?
}

#[test]
fn pooled_tls_connection_does_not_reload_overwritten_ca_bundle() {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind pooled TLS loopback fixture");
        let address = listener
            .local_addr()
            .expect("read pooled TLS fixture address");
        let server = tokio::spawn(serve_two_tls_requests_on_one_connection(listener));
        let directory = CapathDirectory::new();
        let bundle = directory.write("pooled-ca.pem", FROZEN_CA_CERTIFICATE);
        let client = Client::builder()
            .tls(TlsConfig {
                roots: CertificateSource::PemBundle(bundle.clone()),
                identity: None,
            })
            .build()
            .expect("build pooled TLS client without I/O");

        let first_url = format!("https://localhost:{}/pool-first", address.port());
        let first = match tokio::time::timeout(IO_TIMEOUT, client.get(&first_url).send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                server.abort();
                let _ = server.await;
                panic!(
                    "first pooled TLS request failed: {:?}: {error}",
                    error.kind()
                );
            }
            Err(_) => {
                server.abort();
                let _ = server.await;
                panic!("first pooled TLS request timed out");
            }
        };
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = tokio::time::timeout(IO_TIMEOUT, first.bytes())
            .await
            .expect("first pooled TLS response body timed out")
            .expect("read first pooled TLS response body");
        assert_eq!(first_body.as_ref(), b"tls-pool");

        std::fs::write(&bundle, b"garbage after first TLS connection")
            .expect("overwrite the configured CA bundle at the same lexical path");

        let second_url = format!("https://localhost:{}/pool-second", address.port());
        let second = match tokio::time::timeout(IO_TIMEOUT, client.get(&second_url).send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let server_result = await_pooled_server(server).await;
                panic!(
                    "second pooled TLS request failed after CA overwrite: {:?}: {error}; \
                     server: {server_result:?}",
                    error.kind()
                );
            }
            Err(_) => {
                server.abort();
                let _ = server.await;
                panic!("second pooled TLS request timed out");
            }
        };
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = tokio::time::timeout(IO_TIMEOUT, second.bytes())
            .await
            .expect("second pooled TLS response body timed out")
            .expect("read second pooled TLS response body");
        assert_eq!(second_body.as_ref(), b"tls-pool");

        let observation = await_pooled_server(server)
            .await
            .expect("pooled TLS fixture failed");
        assert_eq!(observation.accepts, 1);
        assert_eq!(observation.request_bytes.len(), 2);
        assert!(
            observation.request_bytes[0].starts_with(b"GET /pool-first HTTP/1.1\r\n"),
            "unexpected first pooled request: {:?}",
            String::from_utf8_lossy(&observation.request_bytes[0]),
        );
        assert!(
            observation.request_bytes[1].starts_with(b"GET /pool-second HTTP/1.1\r\n"),
            "unexpected second pooled request: {:?}",
            String::from_utf8_lossy(&observation.request_bytes[1]),
        );
    });
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
fn platform_roots_build_without_tls_io() {
    Client::builder()
        .tls(TlsConfig {
            roots: CertificateSource::Platform,
            identity: None,
        })
        .build()
        .expect("build platform-roots client without I/O");
}

#[test]
fn unrelated_pem_bundle_rejects_untrusted_server_as_tls() {
    run_tls_case(
        VALID_SERVER,
        CertificateSource::PemBundle(repository_fixture("fixtures/tls/wrong-host/wrong-host.pem")),
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

#[test]
fn mtls_loopback_requires_client_identity_before_http() {
    run_mtls_case(
        CertificateSource::PemBundle(frozen_ca_bundle()),
        None,
        ExpectedClientResult::RequiredIdentityFailure,
    );
}

#[test]
fn mtls_loopback_separate_chain_and_key_succeeds() {
    run_mtls_case(
        CertificateSource::PemBundle(frozen_ca_bundle()),
        Some(separate_mtls_identity()),
        ExpectedClientResult::Success,
    );
}

#[test]
fn mtls_loopback_combined_chain_and_key_succeeds() {
    run_mtls_case(
        CertificateSource::PemBundle(frozen_ca_bundle()),
        Some(combined_mtls_identity()),
        ExpectedClientResult::Success,
    );
}

#[test]
fn mtls_loopback_disabled_server_verification_still_sends_identity() {
    run_mtls_case(
        CertificateSource::Disabled,
        Some(separate_mtls_identity()),
        ExpectedClientResult::Success,
    );
}

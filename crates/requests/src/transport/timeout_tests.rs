use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Method;

use super::pool::{IdentityKey, PoolKey, TlsPoolKey};
use super::{
    Connector, DEFAULT_MAX_IDLE_PER_HOST, DeadlineSource, Transport, select_deadline_source,
};
use crate::{
    AsyncBody, BodySource, CertificateSource, ContentCodecs, Error, ErrorKind, Identity,
    RequestBuilder, Timeout, TlsConfig,
};

const SHORT_DEADLINE: Duration = Duration::from_millis(200);
const LONG_DEADLINE: Duration = Duration::from_millis(800);
const SAFETY_RELEASE: Duration = Duration::from_millis(500);
const OUTER_BOUND: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct ControlledConnector {
    shared: Arc<ControlledConnectorShared>,
}

struct ControlledConnectorShared {
    started: AtomicUsize,
    dropped: AtomicUsize,
    released: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

struct ControlledConnectFuture {
    shared: Arc<ControlledConnectorShared>,
    reported_started: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConnectCall {
    host: String,
    port: u16,
    target: String,
}

#[derive(Clone, Default)]
struct RecordingConnector {
    calls: Arc<Mutex<Vec<ConnectCall>>>,
}

impl RecordingConnector {
    fn calls(&self) -> Vec<ConnectCall> {
        self.calls.lock().expect("connector calls lock").clone()
    }
}

impl Connector for RecordingConnector {
    fn connect(
        &self,
        host: &str,
        port: u16,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = crate::Result<tokio::net::TcpStream>> + Send>> {
        self.calls
            .lock()
            .expect("connector calls lock")
            .push(ConnectCall {
                host: host.to_owned(),
                port,
                target: target.to_owned(),
            });
        let target = target.to_owned();
        Box::pin(async move { Err(Error::connect(&target, "controlled connector error")) })
    }
}

impl ControlledConnector {
    fn new() -> Self {
        Self {
            shared: Arc::new(ControlledConnectorShared {
                started: AtomicUsize::new(0),
                dropped: AtomicUsize::new(0),
                released: AtomicBool::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    async fn wait_started(&self) {
        tokio::time::timeout(OUTER_BOUND, async {
            while self.shared.started.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controlled connector was never polled");
        assert_eq!(self.shared.started.load(Ordering::Acquire), 1);
    }

    fn release(&self) {
        self.shared.released.store(true, Ordering::Release);
        if let Some(waker) = self
            .shared
            .waker
            .lock()
            .expect("connector waker lock")
            .take()
        {
            waker.wake();
        }
    }

    async fn assert_dropped_once(&self) {
        tokio::time::timeout(OUTER_BOUND, async {
            while self.shared.dropped.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controlled connector future was not dropped");
        assert_eq!(self.shared.dropped.load(Ordering::Acquire), 1);
    }
}

impl Connector for ControlledConnector {
    fn connect(
        &self,
        _host: &str,
        _port: u16,
        _target: &str,
    ) -> Pin<Box<dyn Future<Output = crate::Result<tokio::net::TcpStream>> + Send>> {
        Box::pin(ControlledConnectFuture {
            shared: Arc::clone(&self.shared),
            reported_started: false,
        })
    }
}

impl Future for ControlledConnectFuture {
    type Output = crate::Result<tokio::net::TcpStream>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.reported_started {
            self.reported_started = true;
            self.shared.started.fetch_add(1, Ordering::AcqRel);
        }
        if self.shared.released.load(Ordering::Acquire) {
            return Poll::Ready(Err(Error::connect(
                "connector.test:80",
                "controlled connector released",
            )));
        }
        *self.shared.waker.lock().expect("connector waker lock") = Some(context.waker().clone());
        if self.shared.released.load(Ordering::Acquire) {
            context.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl Drop for ControlledConnectFuture {
    fn drop(&mut self) {
        self.shared.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

struct PollProbeBody {
    polls: Arc<AtomicUsize>,
}

impl AsyncBody for PollProbeBody {
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<crate::Result<Bytes>>> {
        self.polls.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(None)
    }

    fn size_hint(&self) -> Option<u64> {
        None
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build timeout contract runtime")
}

fn request(timeout: Timeout, body: BodySource) -> crate::Request {
    RequestBuilder::new(Method::POST, "http://connector.test/pending")
        .timeout(timeout)
        .body(body)
        .build()
        .expect("build connector contract request")
}

fn assert_implicit_connector_call(url: &str, expected: ConnectCall) {
    runtime().block_on(async {
        let connector = RecordingConnector::default();
        let transport = Transport::with_connector(Arc::new(connector.clone()));
        let request = RequestBuilder::new(Method::GET, url)
            .build()
            .expect("build implicit-port request");
        let error = match transport.send(request).await {
            Err(error) => error,
            Ok(_) => panic!("controlled connector unexpectedly produced a response"),
        };
        assert_eq!(
            error.kind(),
            ErrorKind::Connect,
            "request must reach the typed connector before its controlled error"
        );
        assert_eq!(connector.calls(), vec![expected]);
    });
}

fn send_with_recording_tls(url: &str, tls: TlsConfig) -> (Error, Vec<ConnectCall>, Vec<PoolKey>) {
    runtime().block_on(async {
        let connector = RecordingConnector::default();
        let transport = Transport::with_configuration(
            Arc::new(connector.clone()),
            None,
            tls,
            Timeout::default(),
            DEFAULT_MAX_IDLE_PER_HOST,
            ContentCodecs::new(true, true),
        );
        let request = RequestBuilder::new(Method::GET, url)
            .build()
            .expect("build deferred TLS-loading request");
        let result = tokio::time::timeout(OUTER_BOUND, transport.send(request))
            .await
            .expect("deferred TLS-loading request exceeded outer bound");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("controlled connector unexpectedly produced a response"),
        };
        (
            error,
            connector.calls(),
            transport.drain_derived_pool_keys(),
        )
    })
}

fn recorded_pool_key(url: &str, tls: TlsConfig) -> (PoolKey, Error, Vec<ConnectCall>) {
    let (error, calls, keys) = send_with_recording_tls(url, tls);
    assert_eq!(
        keys.len(),
        1,
        "Transport::send must record its one production-derived PoolKey before loading or connect",
    );
    (
        keys.into_iter().next().expect("one observed pool key"),
        error,
        calls,
    )
}

fn missing_tls_material_key(tls: TlsConfig) -> PoolKey {
    let (key, error, calls) = recorded_pool_key("https://secure.test/path", tls);
    assert!(
        calls.is_empty(),
        "missing TLS material must fail before connector"
    );
    assert_eq!(
        format!("{:?}", error.kind()),
        "Tls",
        "missing TLS material must be an exact TLS error: {error}",
    );
    key
}

fn assert_https_pool_key(
    key: &PoolKey,
    expected_tls: TlsPoolKey,
    expected_identity: Option<IdentityKey>,
) {
    assert_eq!(key.scheme, http::uri::Scheme::HTTPS);
    assert_eq!(key.authority.host(), "secure.test");
    assert_eq!(key.proxy, None);
    assert_eq!(key.tls, expected_tls);
    assert_eq!(key.identity, expected_identity);
}

fn frozen_root_bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/certs/expired/ca/ca.crt")
}

#[test]
fn tls_loading_plain_http_never_reads_missing_root_or_identity_paths() {
    let (error, calls, keys) = send_with_recording_tls(
        "http://plain.test/path",
        TlsConfig {
            roots: CertificateSource::PemBundle(PathBuf::from("red-e-missing-root.pem")),
            identity: Some(Identity {
                certificate_chain: PathBuf::from("red-e-missing-client-chain.pem"),
                private_key: Some(PathBuf::from("red-e-missing-client.key")),
            }),
        },
    );

    assert_eq!(error.kind(), ErrorKind::Connect);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].tls, TlsPoolKey::plain());
    assert_eq!(keys[0].identity, None);
    assert_eq!(
        calls,
        [ConnectCall {
            host: "plain.test".to_owned(),
            port: 80,
            target: "plain.test".to_owned(),
        }],
    );
}

#[test]
fn tls_loading_https_missing_root_fails_before_connector() {
    let (error, calls, keys) = send_with_recording_tls(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::PemBundle(PathBuf::from("red-e-missing-root.pem")),
            identity: None,
        },
    );

    assert_eq!(
        keys.len(),
        1,
        "HTTPS key derivation must precede missing-root loading",
    );
    assert!(calls.is_empty(), "TLS load failure must precede connect");
    assert_eq!(
        format!("{:?}", error.kind()),
        "Tls",
        "missing TLS root must be an exact TLS error: {error}",
    );
}

#[test]
fn tls_loading_https_valid_root_reaches_connector_once() {
    let root = frozen_root_bundle();
    assert!(root.is_file(), "frozen root fixture must exist: {root:?}");
    let root_directory = root.parent().expect("frozen root has a parent");
    let alias = root_directory
        .join("..")
        .join(
            root_directory
                .file_name()
                .expect("frozen root parent has a filename"),
        )
        .join(root.file_name().expect("frozen root has a filename"));
    assert!(
        alias.is_file(),
        "lexical alias of frozen root must exist: {alias:?}"
    );
    assert_ne!(
        root, alias,
        "existing frozen root spellings must remain lexically distinct",
    );
    let (error, calls, keys) = send_with_recording_tls(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::PemBundle(root.clone()),
            identity: None,
        },
    );

    assert_eq!(keys.len(), 1);
    assert_https_pool_key(&keys[0], TlsPoolKey::pem_bundle(&root), None);
    assert_eq!(
        error.kind(),
        ErrorKind::Connect,
        "valid root must load before the controlled connector error: {error}",
    );
    assert_eq!(
        calls,
        [ConnectCall {
            host: "secure.test".to_owned(),
            port: 443,
            target: "secure.test".to_owned(),
        }],
    );

    let (alias_error, alias_calls, alias_keys) = send_with_recording_tls(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::PemBundle(alias.clone()),
            identity: None,
        },
    );
    assert_eq!(alias_keys.len(), 1);
    assert_https_pool_key(&alias_keys[0], TlsPoolKey::pem_bundle(&alias), None);
    assert_ne!(
        keys[0], alias_keys[0],
        "existing-file lexical aliases must remain distinct pool identities",
    );
    assert_eq!(alias_error.kind(), ErrorKind::Connect);
    assert_eq!(alias_calls, calls);
}

#[test]
fn actual_plain_http_pool_key_is_invariant_across_tls_configuration() {
    let configurations = [
        TlsConfig::default(),
        TlsConfig {
            roots: CertificateSource::Disabled,
            identity: None,
        },
        TlsConfig {
            roots: CertificateSource::PemBundle(PathBuf::from("missing-ca.pem")),
            identity: None,
        },
        TlsConfig {
            roots: CertificateSource::PemDirectory(PathBuf::from("missing-capath")),
            identity: None,
        },
        TlsConfig {
            roots: CertificateSource::Platform,
            identity: Some(Identity {
                certificate_chain: PathBuf::from("missing-combined-client.pem"),
                private_key: None,
            }),
        },
        TlsConfig {
            roots: CertificateSource::Platform,
            identity: Some(Identity {
                certificate_chain: PathBuf::from("missing-client-chain.pem"),
                private_key: Some(PathBuf::from("missing-client.key")),
            }),
        },
    ];
    let keys = configurations
        .into_iter()
        .map(|tls| {
            let (key, error, calls) = recorded_pool_key("http://plain.test/path", tls);
            assert_eq!(error.kind(), ErrorKind::Connect);
            assert_eq!(
                calls,
                [ConnectCall {
                    host: "plain.test".to_owned(),
                    port: 80,
                    target: "plain.test".to_owned(),
                }],
            );
            key
        })
        .collect::<Vec<_>>();

    for key in &keys[1..] {
        assert_eq!(key, &keys[0]);
    }
    let PoolKey {
        scheme,
        authority,
        proxy,
        tls,
        identity,
    } = &keys[0];
    assert_eq!(scheme, &http::uri::Scheme::HTTP);
    assert_eq!(authority.host(), "plain.test");
    assert_eq!(proxy, &None);
    assert_eq!(tls, &TlsPoolKey::plain());
    assert_eq!(identity, &None);
}

#[test]
fn actual_https_pool_keys_preserve_root_mode_and_lexical_paths() {
    let (platform, _, _) = recorded_pool_key(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::Platform,
            identity: None,
        },
    );
    assert_https_pool_key(&platform, TlsPoolKey::platform(), None);
    let (disabled, disabled_error, disabled_calls) = recorded_pool_key(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::Disabled,
            identity: None,
        },
    );
    assert_https_pool_key(&disabled, TlsPoolKey::disabled(), None);
    assert_eq!(disabled_error.kind(), ErrorKind::Connect);
    assert_eq!(
        disabled_calls,
        [ConnectCall {
            host: "secure.test".to_owned(),
            port: 443,
            target: "secure.test".to_owned(),
        }],
    );
    let bundle_path = PathBuf::from("ca.pem");
    let directory_path = PathBuf::from("capath");
    let bundle = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::PemBundle(bundle_path.clone()),
        identity: None,
    });
    let directory = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::PemDirectory(directory_path.clone()),
        identity: None,
    });
    assert_https_pool_key(&bundle, TlsPoolKey::pem_bundle(&bundle_path), None);
    assert_https_pool_key(&directory, TlsPoolKey::pem_directory(&directory_path), None);

    let modes = [&platform, &disabled, &bundle, &directory];
    for (index, left) in modes.iter().enumerate() {
        for right in &modes[index + 1..] {
            assert_ne!(left, right);
        }
    }
    assert_eq!(
        bundle,
        missing_tls_material_key(TlsConfig {
            roots: CertificateSource::PemBundle(PathBuf::from("ca.pem")),
            identity: None,
        }),
    );
    let alias_path = PathBuf::from("./ca.pem");
    let alias = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::PemBundle(alias_path.clone()),
        identity: None,
    });
    assert_https_pool_key(&alias, TlsPoolKey::pem_bundle(&alias_path), None);
    assert_ne!(bundle, alias);

    let first_path = PathBuf::from("uninspected-a/ca.pem");
    let second_path = PathBuf::from("uninspected-b/ca.pem");
    let first = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::PemBundle(first_path.clone()),
        identity: None,
    });
    let second = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::PemBundle(second_path.clone()),
        identity: None,
    });
    assert_https_pool_key(&first, TlsPoolKey::pem_bundle(&first_path), None);
    assert_https_pool_key(&second, TlsPoolKey::pem_bundle(&second_path), None);
    assert_ne!(first, second);
}

#[test]
fn actual_https_pool_keys_preserve_identity_shape_and_lexical_paths() {
    let (without_identity, error, calls) = recorded_pool_key(
        "https://secure.test/path",
        TlsConfig {
            roots: CertificateSource::Disabled,
            identity: None,
        },
    );
    assert_eq!(error.kind(), ErrorKind::Connect);
    assert_eq!(calls.len(), 1);
    assert_https_pool_key(&without_identity, TlsPoolKey::disabled(), None);
    let certificate_chain = PathBuf::from("client.pem");
    let private_key = PathBuf::from("client.key");
    let combined = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::Disabled,
        identity: Some(Identity {
            certificate_chain: certificate_chain.clone(),
            private_key: None,
        }),
    });
    let separate = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::Disabled,
        identity: Some(Identity {
            certificate_chain: certificate_chain.clone(),
            private_key: Some(private_key.clone()),
        }),
    });
    assert_https_pool_key(
        &combined,
        TlsPoolKey::disabled(),
        Some(IdentityKey::from_paths(&certificate_chain, None)),
    );
    assert_https_pool_key(
        &separate,
        TlsPoolKey::disabled(),
        Some(IdentityKey::from_paths(
            &certificate_chain,
            Some(&private_key),
        )),
    );

    assert_ne!(without_identity, combined);
    assert_ne!(combined, separate);
    assert_ne!(without_identity, separate);
    assert_eq!(
        combined,
        missing_tls_material_key(TlsConfig {
            roots: CertificateSource::Disabled,
            identity: Some(Identity {
                certificate_chain: PathBuf::from("client.pem"),
                private_key: None,
            }),
        }),
    );
    let certificate_alias = PathBuf::from("./client.pem");
    let certificate_alias_key = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::Disabled,
        identity: Some(Identity {
            certificate_chain: certificate_alias.clone(),
            private_key: Some(private_key.clone()),
        }),
    });
    assert_https_pool_key(
        &certificate_alias_key,
        TlsPoolKey::disabled(),
        Some(IdentityKey::from_paths(
            &certificate_alias,
            Some(&private_key),
        )),
    );
    assert_ne!(separate, certificate_alias_key);

    let private_key_alias = PathBuf::from("./client.key");
    let private_alias_key = missing_tls_material_key(TlsConfig {
        roots: CertificateSource::Disabled,
        identity: Some(Identity {
            certificate_chain: certificate_chain.clone(),
            private_key: Some(private_key_alias.clone()),
        }),
    });
    assert_https_pool_key(
        &private_alias_key,
        TlsPoolKey::disabled(),
        Some(IdentityKey::from_paths(
            &certificate_chain,
            Some(&private_key_alias),
        )),
    );
    assert_ne!(separate, private_alias_key);
}

async fn send_with_safety_release(timeout: Timeout) -> Error {
    let connector = ControlledConnector::new();
    let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
    let request = request(timeout, BodySource::Empty);
    let task_transport = Arc::clone(&transport);
    let mut send = tokio::spawn(async move { task_transport.send(request).await });

    connector.wait_started().await;
    let release_connector = connector.clone();
    let mut release = tokio::spawn(async move {
        tokio::time::sleep(SAFETY_RELEASE).await;
        release_connector.release();
    });
    let result = tokio::time::timeout(OUTER_BOUND, &mut send).await;
    release.abort();
    let _ = (&mut release).await;
    let result = match result {
        Ok(result) => result.expect("controlled send task failed"),
        Err(error) => {
            send.abort();
            let _ = send.await;
            panic!("controlled send exceeded outer bound: {error}");
        }
    };
    connector.assert_dropped_once().await;
    match result {
        Err(error) => error,
        Ok(_) => panic!("pending controlled connector unexpectedly produced a response"),
    }
}

fn assert_connect_timeout(error: &Error, source: &str) {
    assert_eq!(error.kind(), ErrorKind::ConnectTimeout);
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("connect"),
        "connect timeout must identify its phase: {message}"
    );
    match source {
        "connect" => {
            assert!(
                message.contains("connect timeout") && !message.contains("total timeout"),
                "connect-specific timeout context missing: {message}"
            );
        }
        "total" => {
            assert!(
                message.contains("total timeout"),
                "total timeout context missing during connect: {message}"
            );
        }
        _ => panic!("unsupported connect timeout source: {source}"),
    }
}

fn assert_equal_read_total_tie_prefers_read(_phase: &str) {
    let deadline = Instant::now();
    assert_eq!(
        select_deadline_source(Some(deadline), Some(deadline)),
        Some(DeadlineSource::Read)
    );
}

#[test]
fn implicit_http_uses_port_80_and_exact_connector_target() {
    assert_implicit_connector_call(
        "http://plain.test/path",
        ConnectCall {
            host: "plain.test".to_owned(),
            port: 80,
            target: "plain.test".to_owned(),
        },
    );
}

#[test]
fn implicit_https_uses_port_443_and_exact_connector_target() {
    assert_implicit_connector_call(
        "https://secure.test/path",
        ConnectCall {
            host: "secure.test".to_owned(),
            port: 443,
            target: "secure.test".to_owned(),
        },
    );
}

#[test]
fn equal_absolute_response_head_deadlines_prefer_read_source() {
    assert_equal_read_total_tie_prefers_read("response head");
}

#[test]
fn equal_absolute_response_body_deadlines_prefer_read_source() {
    assert_equal_read_total_tie_prefers_read("response body");
}

#[test]
fn connect_component_expiry_is_connect_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: None,
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn total_expiry_during_connect_is_connect_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: None,
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "total");
}

#[test]
fn shorter_connect_deadline_precedes_total_during_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: Some(LONG_DEADLINE),
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn shorter_total_deadline_precedes_connect_during_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(LONG_DEADLINE),
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "total");
}

#[test]
fn equal_connect_and_total_deadlines_prefer_connect_specific_timeout() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: Some(SHORT_DEADLINE),
        read: None,
        total: Some(SHORT_DEADLINE),
    }));

    assert_connect_timeout(&error, "connect");
}

#[test]
fn read_timeout_does_not_govern_pending_connect() {
    let error = runtime().block_on(send_with_safety_release(Timeout {
        connect: None,
        read: Some(SHORT_DEADLINE),
        total: None,
    }));

    assert_eq!(error.kind(), ErrorKind::Connect);
    assert!(error.to_string().contains("controlled connector released"));
}

#[test]
fn both_none_release_preserves_original_connect_error() {
    runtime().block_on(async {
        let connector = ControlledConnector::new();
        let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
        let task_transport = Arc::clone(&transport);
        let mut send = tokio::spawn(async move {
            task_transport
                .send(request(Timeout::default(), BodySource::Empty))
                .await
        });

        connector.wait_started().await;
        connector.release();
        let result = tokio::time::timeout(OUTER_BOUND, &mut send)
            .await
            .expect("released connector send exceeded outer bound")
            .expect("released connector send task failed");
        connector.assert_dropped_once().await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("released controlled connector unexpectedly produced a response"),
        };
        assert_eq!(error.kind(), ErrorKind::Connect);
        assert!(error.to_string().contains("controlled connector released"));
    });
}

#[test]
fn request_body_is_not_polled_while_connect_is_pending() {
    runtime().block_on(async {
        let connector = ControlledConnector::new();
        let transport = Arc::new(Transport::with_connector(Arc::new(connector.clone())));
        let polls = Arc::new(AtomicUsize::new(0));
        let body = BodySource::Stream(Box::pin(PollProbeBody {
            polls: Arc::clone(&polls),
        }));
        let task_transport = Arc::clone(&transport);
        let mut send =
            tokio::spawn(
                async move { task_transport.send(request(Timeout::default(), body)).await },
            );

        connector.wait_started().await;
        assert_eq!(polls.load(Ordering::Acquire), 0);
        connector.release();
        let result = tokio::time::timeout(OUTER_BOUND, &mut send)
            .await
            .expect("body pre-poll control exceeded outer bound")
            .expect("body pre-poll send task failed");
        connector.assert_dropped_once().await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("released controlled connector unexpectedly produced a response"),
        };
        assert_eq!(error.kind(), ErrorKind::Connect);
        assert_eq!(polls.load(Ordering::Acquire), 0);
    });
}

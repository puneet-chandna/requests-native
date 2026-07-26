use std::future::{Future, pending, ready};
use std::io::Cursor;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http::Method;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use super::{
    Connector, DEFAULT_MAX_IDLE_PER_HOST, EstablishmentControl, EstablishmentStage, Transport,
};
use crate::{
    BodySource, CertificateSource, ContentCodecs, Error, ErrorKind, RequestBuilder, Timeout,
    TlsConfig,
};

const SHORT_DEADLINE: Duration = Duration::from_millis(200);
const LONG_DEADLINE: Duration = Duration::from_millis(800);
const OUTER_BOUND: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedStage {
    LoadStarted,
    LoadFinished(bool),
    Connect,
    Tls,
    Http1,
    RawShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockedStage {
    Load,
    Tls,
    Http1,
}

struct TestEstablishmentControl {
    blocked: Option<BlockedStage>,
    stages: Mutex<Vec<ObservedStage>>,
    load_released: Mutex<bool>,
    load_release: Condvar,
    load_finished: AtomicBool,
    raw_shutdowns: AtomicUsize,
}

impl TestEstablishmentControl {
    fn new(blocked: Option<BlockedStage>) -> Arc<Self> {
        Arc::new(Self {
            blocked,
            stages: Mutex::new(Vec::new()),
            load_released: Mutex::new(false),
            load_release: Condvar::new(),
            load_finished: AtomicBool::new(false),
            raw_shutdowns: AtomicUsize::new(0),
        })
    }

    fn record(&self, stage: ObservedStage) {
        self.stages
            .lock()
            .expect("establishment stage lock poisoned")
            .push(stage);
    }

    fn stages(&self) -> Vec<ObservedStage> {
        self.stages
            .lock()
            .expect("establishment stage lock poisoned")
            .clone()
    }

    fn release_load(&self) {
        *self
            .load_released
            .lock()
            .expect("load release lock poisoned") = true;
        self.load_release.notify_all();
    }

    async fn await_released_load_completion(&self) {
        if !self
            .stages()
            .iter()
            .any(|stage| matches!(stage, ObservedStage::LoadStarted))
        {
            return;
        }
        tokio::time::timeout(OUTER_BOUND, async {
            while !self.load_finished.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released blocking TLS loader did not finish");
    }
}

impl EstablishmentControl for TestEstablishmentControl {
    fn blocking_load_started(&self) {
        self.record(ObservedStage::LoadStarted);
        if self.blocked == Some(BlockedStage::Load) {
            let mut released = self
                .load_released
                .lock()
                .expect("load release lock poisoned");
            while !*released {
                released = self
                    .load_release
                    .wait(released)
                    .expect("load release lock poisoned while waiting");
            }
        }
    }

    fn blocking_load_finished(&self, succeeded: bool) {
        self.record(ObservedStage::LoadFinished(succeeded));
        self.load_finished.store(true, Ordering::Release);
    }

    fn checkpoint(
        &self,
        stage: EstablishmentStage,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let observed = match stage {
            EstablishmentStage::Connect => ObservedStage::Connect,
            EstablishmentStage::Tls => ObservedStage::Tls,
            EstablishmentStage::Http1 => ObservedStage::Http1,
        };
        self.record(observed);
        let blocked = matches!(
            (self.blocked, stage),
            (Some(BlockedStage::Tls), EstablishmentStage::Tls)
                | (Some(BlockedStage::Http1), EstablishmentStage::Http1)
        );
        if blocked {
            Box::pin(pending())
        } else {
            Box::pin(ready(()))
        }
    }

    fn raw_shutdown_taken(&self) {
        self.raw_shutdowns.fetch_add(1, Ordering::AcqRel);
        self.record(ObservedStage::RawShutdown);
    }
}

#[derive(Clone, Default)]
struct FailingConnector {
    calls: Arc<AtomicUsize>,
}

impl FailingConnector {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl Connector for FailingConnector {
    fn connect(
        &self,
        _host: &str,
        _port: u16,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = crate::Result<tokio::net::TcpStream>> + Send>> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let target = target.to_owned();
        Box::pin(async move { Err(Error::connect(&target, "controlled connector failure")) })
    }
}

#[derive(Clone, Default)]
struct PendingConnector {
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

struct PendingConnectFuture {
    drops: Arc<AtomicUsize>,
}

impl Future for PendingConnectFuture {
    type Output = crate::Result<tokio::net::TcpStream>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for PendingConnectFuture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

impl Connector for PendingConnector {
    fn connect(
        &self,
        _host: &str,
        _port: u16,
        _target: &str,
    ) -> Pin<Box<dyn Future<Output = crate::Result<tokio::net::TcpStream>> + Send>> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Box::pin(PendingConnectFuture {
            drops: Arc::clone(&self.drops),
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build establishment contract runtime")
}

fn frozen_ca_bundle() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/certs/expired/ca/ca.crt")
}

fn request(url: &str, timeout: Timeout) -> crate::Request {
    RequestBuilder::new(Method::GET, url)
        .timeout(timeout)
        .body(BodySource::Empty)
        .build()
        .expect("build establishment contract request")
}

fn transport(
    connector: Arc<dyn Connector>,
    tls: TlsConfig,
    control: Arc<TestEstablishmentControl>,
) -> Transport {
    Transport::with_configuration(
        connector,
        None,
        tls,
        Timeout::default(),
        DEFAULT_MAX_IDLE_PER_HOST,
        ContentCodecs::new(true, true),
    )
    .with_test_establishment_control(control)
}

fn assert_no_pool_entry(transport: &Transport) {
    let keys = transport.drain_derived_pool_keys();
    assert_eq!(
        keys.len(),
        1,
        "one production pool key must be derived before establishment",
    );
    let pool = transport.pool.lock().expect("transport pool lock poisoned");
    assert_eq!(pool.generation_count(), 0);
    assert!(
        pool.generation(&keys[0]).is_none(),
        "failed establishment must not create a pool generation",
    );
}

fn assert_establishment_timeout(error: &Error, source: &str) {
    assert_eq!(error.kind(), ErrorKind::ConnectTimeout);
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("connection establishment"),
        "timeout must identify the complete connection-establishment phase: {message}",
    );
    assert!(
        !message.contains("tcp connect"),
        "timeout must not describe the complete establishment future as TCP-only: {message}",
    );
    match source {
        "connect" => {
            assert!(message.contains("connect timeout"));
            assert!(!message.contains("total timeout"));
        }
        "total" => assert!(message.contains("total timeout")),
        _ => panic!("unsupported establishment timeout source: {source}"),
    }
}

fn expect_error(result: crate::Result<super::TransportResponse>, context: &str) -> Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("{context}"),
    }
}

async fn run_blocked_load(
    connect: Duration,
    total: Duration,
) -> (Error, Vec<ObservedStage>, Transport) {
    let control = TestEstablishmentControl::new(Some(BlockedStage::Load));
    let connector = FailingConnector::default();
    let transport = transport(
        Arc::new(connector.clone()),
        TlsConfig {
            roots: CertificateSource::PemBundle(frozen_ca_bundle()),
            identity: None,
        },
        Arc::clone(&control),
    );
    let result = tokio::time::timeout(
        OUTER_BOUND,
        transport.send(request(
            "https://load-stage.test/path",
            Timeout {
                connect: Some(connect),
                read: None,
                total: Some(total),
            },
        )),
    )
    .await
    .expect("blocked TLS load exceeded its outer bound");
    control.release_load();
    control.await_released_load_completion().await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("blocked TLS load unexpectedly returned a response"),
    };
    assert_eq!(connector.calls(), 0);
    assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 0);
    (error, control.stages(), transport)
}

#[test]
fn production_https_establishment_inventory_is_ordered_and_typed() {
    let transport = include_str!("mod.rs");
    let error = include_str!("../error.rs");
    let (production, _) = transport
        .split_once("#[cfg(test)]\nmod tests {")
        .expect("transport source keeps one final cfg(test) module");
    let (_, connection_and_after) = production
        .split_once("async fn connect_connection(")
        .expect("transport keeps one pool-miss establishment function");
    let (connection, _) = connection_and_after
        .split_once("\n}\n\nfn validate_request")
        .expect("establishment function remains bounded before validation");
    let compact = connection.split_whitespace().collect::<String>();
    let mut violations = Vec::new();

    let raw_shutdown_calls = production.matches(".raw_shutdown_taken()").count();
    if raw_shutdown_calls != 1 {
        violations.push("production must contain exactly one raw-shutdown observation call");
    }
    let shutdown_helper = production
        .split_once("fn shutdown_socket_once(&mut self) {")
        .and_then(|(_, helper_and_after)| {
            helper_and_after
                .split_once("\n    }\n\n    fn take_and_abort_task_once")
                .map(|(helper, _)| helper)
        });
    match shutdown_helper {
        Some(helper) => {
            let helper = helper.split_whitespace().collect::<String>();
            if helper.matches(".raw_shutdown_taken()").count() != raw_shutdown_calls {
                violations
                    .push("raw-shutdown observation must occur only inside shutdown_socket_once");
            }
            if helper.matches("self.shutdown.take()").count() != 1
                || helper.matches("stream.shutdown(Shutdown::Both)").count() != 1
            {
                violations.push(
                    "shutdown_socket_once must retain one raw Option::take and one socket shutdown",
                );
            }
            let ordered = [
                helper.find("self.shutdown.take()"),
                helper.find(".raw_shutdown_taken()"),
                helper.find("stream.shutdown(Shutdown::Both)"),
            ];
            if !matches!(
                ordered,
                [Some(take), Some(observe), Some(shutdown)]
                    if take < observe && observe < shutdown
            ) {
                violations.push(
                    "shutdown_socket_once must take the raw socket, observe that take, then shut it down",
                );
            }
        }
        None => violations.push("production must retain one bounded shutdown_socket_once helper"),
    }

    if production.matches("tokio::task::spawn_blocking").count() != 1 {
        violations.push("HTTPS establishment needs exactly one blocking load/parse task");
    }
    if !compact.contains("spawn_blocking") || !compact.contains(".await") {
        violations.push("the one blocking loader must be awaited inside establishment");
    }
    for required in [
        "blocking_load_started",
        "tls::load",
        "blocking_load_finished",
        "self.connector.connect",
        "tls::handshake",
        "start_http1",
        "EstablishmentStage::Tls",
        "EstablishmentStage::Http1",
    ] {
        if !compact.contains(required) {
            violations.push(required);
        }
    }
    let ordered = [
        compact.find("blocking_load_started"),
        compact.find("tls::load"),
        compact.find("blocking_load_finished"),
        compact.find("self.connector.connect"),
        compact.find("EstablishmentStage::Tls"),
        compact.find("tls::handshake"),
        compact.find("EstablishmentStage::Http1"),
        compact.find("start_http1"),
    ];
    if !matches!(
        ordered,
        [
            Some(load_started),
            Some(load),
            Some(load_finished),
            Some(connect),
            Some(tls_stage),
            Some(tls),
            Some(http1_stage),
            Some(http1),
        ] if load_started < load
            && load < load_finished
            && load_finished < connect
            && connect < tls_stage
            && tls_stage < tls
            && tls < http1_stage
            && http1_stage < http1
    ) {
        violations.push("HTTPS stages must remain load -> Connector -> TLS -> HTTP/1");
    }
    if !compact.contains("letestablishing=async{")
        || !compact.contains("result=&mutestablishing=>result")
    {
        violations.push("one deadline select must wrap the complete establishment future");
    }
    if compact.contains("read_deadline") || compact.contains("read_timeout") {
        violations.push("read timeout must not govern connection establishment");
    }
    if !production.contains("fn start_http1<")
        || production.matches("http1::handshake(").count() != 1
        || !production.contains(".map_err(Error::handshake)")
    {
        violations.push("one shared private Hyper HTTP/1 seam must retain Handshake mapping");
    }
    if !error.contains("    Tls,") || !error.contains("ErrorKind::Tls") {
        violations.push("native TLS failures need stable ErrorKind::Tls construction");
    }
    if !error.contains("connection establishment") || error.contains("TCP connect phase") {
        violations.push("connect timeout text must name connection establishment, not TCP");
    }

    assert!(
        violations.is_empty(),
        "production establishment contract violations:\n{}",
        violations.join("\n"),
    );
}

#[test]
fn plain_http_skips_load_and_tls_before_connector_failure() {
    runtime().block_on(async {
        let control = TestEstablishmentControl::new(None);
        let connector = FailingConnector::default();
        let transport = transport(
            Arc::new(connector.clone()),
            TlsConfig {
                roots: CertificateSource::PemBundle(std::path::PathBuf::from(
                    "plain-http-must-not-read.pem",
                )),
                identity: None,
            },
            Arc::clone(&control),
        );
        let error = expect_error(
            transport
                .send(request("http://plain.test/path", Timeout::default()))
                .await,
            "controlled connector unexpectedly returned a response",
        );

        assert_eq!(error.kind(), ErrorKind::Connect);
        assert_eq!(connector.calls(), 1);
        assert_eq!(control.stages(), [ObservedStage::Connect]);
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 0);
        assert_no_pool_entry(&transport);
    });
}

#[test]
fn native_load_failure_is_pre_socket_tls_and_never_enters_pool() {
    runtime().block_on(async {
        let control = TestEstablishmentControl::new(None);
        let connector = FailingConnector::default();
        let transport = transport(
            Arc::new(connector.clone()),
            TlsConfig {
                roots: CertificateSource::PemBundle(std::path::PathBuf::from(
                    "red-f-missing-root.pem",
                )),
                identity: None,
            },
            Arc::clone(&control),
        );
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                "https://load-failure.test/path",
                Timeout::default(),
            )),
        )
        .await
        .expect("native load failure exceeded outer bound");
        let error = expect_error(
            result,
            "missing native root unexpectedly returned a response",
        );

        assert_eq!(format!("{:?}", error.kind()), "Tls");
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(false),
            ],
        );
        assert_eq!(connector.calls(), 0);
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 0);
        assert_no_pool_entry(&transport);
    });
}

#[test]
fn shorter_connect_deadline_expires_during_blocking_load() {
    let (error, stages, transport) =
        runtime().block_on(run_blocked_load(SHORT_DEADLINE, LONG_DEADLINE));
    assert_establishment_timeout(&error, "connect");
    assert_eq!(
        stages,
        [
            ObservedStage::LoadStarted,
            ObservedStage::LoadFinished(true),
        ],
    );
    assert_no_pool_entry(&transport);
}

#[test]
fn shorter_total_deadline_expires_during_blocking_load() {
    let (error, stages, transport) =
        runtime().block_on(run_blocked_load(LONG_DEADLINE, SHORT_DEADLINE));
    assert_establishment_timeout(&error, "total");
    assert_eq!(
        stages,
        [
            ObservedStage::LoadStarted,
            ObservedStage::LoadFinished(true),
        ],
    );
    assert_no_pool_entry(&transport);
}

#[test]
fn equal_deadlines_during_blocking_load_prefer_connect_timeout() {
    let (error, stages, transport) =
        runtime().block_on(run_blocked_load(SHORT_DEADLINE, SHORT_DEADLINE));
    assert_establishment_timeout(&error, "connect");
    assert_eq!(
        stages,
        [
            ObservedStage::LoadStarted,
            ObservedStage::LoadFinished(true),
        ],
    );
    assert_no_pool_entry(&transport);
}

#[test]
fn connector_failure_follows_one_successful_load_and_never_enters_pool() {
    runtime().block_on(async {
        let control = TestEstablishmentControl::new(None);
        let connector = FailingConnector::default();
        let transport = transport(
            Arc::new(connector.clone()),
            TlsConfig {
                roots: CertificateSource::PemBundle(frozen_ca_bundle()),
                identity: None,
            },
            Arc::clone(&control),
        );
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                "https://connector-failure.test/path",
                Timeout::default(),
            )),
        )
        .await
        .expect("connector failure exceeded outer bound");
        let error = expect_error(
            result,
            "controlled connector unexpectedly returned a response",
        );

        assert_eq!(error.kind(), ErrorKind::Connect);
        assert_eq!(connector.calls(), 1);
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(true),
                ObservedStage::Connect,
            ],
        );
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 0);
        assert_no_pool_entry(&transport);
    });
}

#[test]
fn connector_timeout_follows_one_successful_load_and_is_cancellation_safe() {
    runtime().block_on(async {
        let control = TestEstablishmentControl::new(None);
        let connector = PendingConnector::default();
        let transport = transport(
            Arc::new(connector.clone()),
            TlsConfig {
                roots: CertificateSource::PemBundle(frozen_ca_bundle()),
                identity: None,
            },
            Arc::clone(&control),
        );
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                "https://connector-timeout.test/path",
                Timeout {
                    connect: Some(SHORT_DEADLINE),
                    read: None,
                    total: None,
                },
            )),
        )
        .await
        .expect("connector timeout exceeded outer bound");
        let error = expect_error(result, "pending connector unexpectedly returned a response");

        assert_establishment_timeout(&error, "connect");
        assert_eq!(connector.calls.load(Ordering::Acquire), 1);
        assert_eq!(connector.drops.load(Ordering::Acquire), 1);
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(true),
                ObservedStage::Connect,
            ],
        );
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 0);
        assert_no_pool_entry(&transport);
    });
}

fn tls_acceptor(certificate_chain: &'static [u8], key: &'static [u8]) -> TlsAcceptor {
    let mut certificates = Cursor::new(certificate_chain);
    let certificates = rustls_pemfile::certs(&mut certificates)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse frozen server certificate chain");
    let mut private_key = Cursor::new(key);
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

async fn await_one_closed_byte(mut stream: impl tokio::io::AsyncRead + Unpin) {
    let mut byte = [0_u8; 1];
    let result = tokio::time::timeout(OUTER_BOUND, stream.read(&mut byte))
        .await
        .expect("post-connect fixture did not observe bounded cleanup");
    match result {
        Ok(0) | Err(_) => {}
        Ok(_) => panic!("HTTP application bytes reached a gated establishment stage"),
    }
}

#[test]
fn red_f_tls_failure_closes_raw_socket_once_without_http_or_pool_entry() {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind TLS-failure loopback fixture");
        let address = listener.local_addr().expect("read TLS-failure address");
        let server = tokio::spawn(async move {
            let (stream, _) = tokio::time::timeout(OUTER_BOUND, listener.accept())
                .await
                .expect("TLS-failure accept timed out")
                .expect("accept TLS-failure connection");
            let acceptor = tls_acceptor(
                include_bytes!("../../../../tests/fixtures/tls/wrong-host/wrong-host.pem"),
                include_bytes!("../../../../tests/fixtures/tls/wrong-host/wrong-host.key"),
            );
            if let Ok(stream) = tokio::time::timeout(OUTER_BOUND, acceptor.accept(stream))
                .await
                .expect("TLS-failure server handshake timed out")
            {
                await_one_closed_byte(stream).await;
            }
        });
        let control = TestEstablishmentControl::new(None);
        let transport = Transport::configured(
            None,
            TlsConfig {
                roots: CertificateSource::PemBundle(frozen_ca_bundle()),
                identity: None,
            },
            Timeout::default(),
            DEFAULT_MAX_IDLE_PER_HOST,
            ContentCodecs::new(true, true),
        )
        .with_test_establishment_control(control.clone());
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                &format!("https://127.0.0.1:{}/tls-failure", address.port()),
                Timeout::default(),
            )),
        )
        .await
        .expect("TLS failure exceeded outer bound");
        let error = expect_error(result, "invalid TLS peer unexpectedly returned a response");

        assert_eq!(format!("{:?}", error.kind()), "Tls");
        tokio::time::timeout(OUTER_BOUND, server)
            .await
            .expect("TLS-failure fixture join timed out")
            .expect("TLS-failure fixture panicked");
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(true),
                ObservedStage::Connect,
                ObservedStage::Tls,
                ObservedStage::RawShutdown,
            ],
        );
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 1);
        assert_no_pool_entry(&transport);
    });
}

#[test]
fn red_f_tls_gate_timeout_closes_raw_socket_once_without_http_or_pool_entry() {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind TLS-stage loopback fixture");
        let address = listener.local_addr().expect("read TLS-stage address");
        let server = tokio::spawn(async move {
            let (stream, _) = tokio::time::timeout(OUTER_BOUND, listener.accept())
                .await
                .expect("TLS-stage accept timed out")
                .expect("accept TLS-stage connection");
            await_one_closed_byte(stream).await;
        });
        let control = TestEstablishmentControl::new(Some(BlockedStage::Tls));
        let transport = Transport::configured(
            None,
            TlsConfig {
                roots: CertificateSource::Disabled,
                identity: None,
            },
            Timeout::default(),
            DEFAULT_MAX_IDLE_PER_HOST,
            ContentCodecs::new(true, true),
        )
        .with_test_establishment_control(control.clone());
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                &format!("https://127.0.0.1:{}/tls-gate", address.port()),
                Timeout {
                    connect: Some(LONG_DEADLINE),
                    read: None,
                    total: Some(SHORT_DEADLINE),
                },
            )),
        )
        .await
        .expect("TLS-stage timeout exceeded outer bound");
        let error = expect_error(result, "blocked TLS stage unexpectedly returned a response");

        assert_establishment_timeout(&error, "total");
        tokio::time::timeout(OUTER_BOUND, server)
            .await
            .expect("TLS-stage fixture join timed out")
            .expect("TLS-stage fixture panicked");
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(true),
                ObservedStage::Connect,
                ObservedStage::Tls,
                ObservedStage::RawShutdown,
            ],
        );
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 1);
        assert_no_pool_entry(&transport);
    });
}

#[test]
fn red_f_http1_gate_timeout_closes_raw_socket_once_after_completed_tls() {
    runtime().block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP/1-stage loopback fixture");
        let address = listener.local_addr().expect("read HTTP/1-stage address");
        let server = tokio::spawn(async move {
            let (stream, _) = tokio::time::timeout(OUTER_BOUND, listener.accept())
                .await
                .expect("HTTP/1-stage accept timed out")
                .expect("accept HTTP/1-stage connection");
            let acceptor = tls_acceptor(
                include_bytes!("../../../../tests/certs/valid/server/server.pem"),
                include_bytes!("../../../../tests/certs/valid/server/server.key"),
            );
            let stream = tokio::time::timeout(OUTER_BOUND, acceptor.accept(stream))
                .await
                .expect("HTTP/1-stage TLS handshake timed out")
                .expect("complete HTTP/1-stage TLS handshake");
            await_one_closed_byte(stream).await;
        });
        let control = TestEstablishmentControl::new(Some(BlockedStage::Http1));
        let transport = Transport::configured(
            None,
            TlsConfig {
                roots: CertificateSource::Disabled,
                identity: None,
            },
            Timeout::default(),
            DEFAULT_MAX_IDLE_PER_HOST,
            ContentCodecs::new(true, true),
        )
        .with_test_establishment_control(control.clone());
        let result = tokio::time::timeout(
            OUTER_BOUND,
            transport.send(request(
                &format!("https://127.0.0.1:{}/http1-gate", address.port()),
                Timeout {
                    connect: Some(SHORT_DEADLINE),
                    read: None,
                    total: Some(SHORT_DEADLINE),
                },
            )),
        )
        .await
        .expect("HTTP/1-stage timeout exceeded outer bound");
        let error = expect_error(
            result,
            "blocked HTTP/1 stage unexpectedly returned a response",
        );

        assert_establishment_timeout(&error, "connect");
        tokio::time::timeout(OUTER_BOUND, server)
            .await
            .expect("HTTP/1-stage fixture join timed out")
            .expect("HTTP/1-stage fixture panicked");
        assert_eq!(
            control.stages(),
            [
                ObservedStage::LoadStarted,
                ObservedStage::LoadFinished(true),
                ObservedStage::Connect,
                ObservedStage::Tls,
                ObservedStage::Http1,
                ObservedStage::RawShutdown,
            ],
        );
        assert_eq!(control.raw_shutdowns.load(Ordering::Acquire), 1);
        assert_no_pool_entry(&transport);
    });
}

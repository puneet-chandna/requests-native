use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use requests::{CertificateSource, Client, ErrorKind, Proxy, TlsConfig, Uri};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEAD: usize = 16 * 1024;
const RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nproxy-ok";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build proxy test runtime")
}

fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set proxy read timeout");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < MAX_HEAD, "proxy request head exceeded bound");
        stream
            .read_exact(&mut byte)
            .expect("read proxy request head");
        head.push(byte[0]);
    }
    head
}

fn spawn_http_proxy() -> (std::net::SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP proxy");
    let address = listener.local_addr().expect("HTTP proxy address");
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept HTTP proxy connection");
        let request = read_head(&mut stream);
        stream.write_all(RESPONSE).expect("write proxy response");
        request
    });
    (address, task)
}

#[test]
fn http_proxy_receives_absolute_form_and_basic_credentials_only_as_a_header() {
    let (proxy_address, proxy_task) = spawn_http_proxy();
    let proxy_uri = format!("http://proxy-user:proxy-pass@{proxy_address}")
        .parse::<Uri>()
        .expect("valid authenticated proxy URI");
    let client = Client::builder()
        .proxy(Proxy::Http(proxy_uri))
        .build()
        .expect("build HTTP proxy client");

    let body = runtime()
        .block_on(
            client
                .get("http://origin.invalid:8123/path?q=one#fragment")
                .send(),
        )
        .expect("HTTP proxy request")
        .bytes();
    assert_eq!(
        runtime().block_on(body).expect("collect proxy response"),
        "proxy-ok"
    );

    let request = String::from_utf8(proxy_task.join().expect("join HTTP proxy"))
        .expect("proxy request is ASCII");
    assert!(
        request.starts_with("GET http://origin.invalid:8123/path?q=one HTTP/1.1\r\n"),
        "{request:?}"
    );
    assert!(
        request.contains("\r\nproxy-authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz\r\n")
            || request.contains("\r\nProxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz\r\n"),
        "{request:?}"
    );
    assert!(!request.contains("proxy-user:proxy-pass@"), "{request:?}");
    assert!(!request.contains("#fragment"), "{request:?}");
}

#[derive(Clone, Copy, Debug)]
enum SocksKind {
    Four,
    FourA,
    Five,
    FiveH,
}

#[derive(Debug)]
struct SocksObservation {
    destination: String,
    request_head: Vec<u8>,
}

fn read_c_string(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).expect("read SOCKS string");
        if byte[0] == 0 {
            return bytes;
        }
        bytes.push(byte[0]);
    }
}

fn serve_socks4(stream: &mut TcpStream) -> String {
    let mut request = [0_u8; 8];
    stream
        .read_exact(&mut request)
        .expect("read SOCKS4 request");
    assert_eq!(request[0..2], [4, 1]);
    let port = u16::from_be_bytes([request[2], request[3]]);
    let ip = [request[4], request[5], request[6], request[7]];
    let _user = read_c_string(stream);
    let host = if ip == [0, 0, 0, 1] {
        String::from_utf8(read_c_string(stream)).expect("SOCKS4a host is UTF-8")
    } else {
        std::net::Ipv4Addr::from(ip).to_string()
    };
    stream
        .write_all(&[0, 90, request[2], request[3], 0, 0, 0, 0])
        .expect("write SOCKS4 success");
    format!("{host}:{port}")
}

fn serve_socks5(stream: &mut TcpStream) -> String {
    let mut greeting = [0_u8; 2];
    stream
        .read_exact(&mut greeting)
        .expect("read SOCKS5 greeting");
    assert_eq!(greeting[0], 5);
    let mut methods = vec![0_u8; greeting[1] as usize];
    stream
        .read_exact(&mut methods)
        .expect("read SOCKS5 methods");
    assert!(methods.contains(&0));
    stream.write_all(&[5, 0]).expect("select no-auth method");

    let mut request = [0_u8; 4];
    stream
        .read_exact(&mut request)
        .expect("read SOCKS5 request");
    assert_eq!(request[0..3], [5, 1, 0]);
    let host = match request[3] {
        1 => {
            let mut address = [0_u8; 4];
            stream.read_exact(&mut address).expect("read SOCKS5 IPv4");
            std::net::Ipv4Addr::from(address).to_string()
        }
        3 => {
            let mut length = [0_u8; 1];
            stream
                .read_exact(&mut length)
                .expect("read SOCKS5 domain length");
            let mut domain = vec![0_u8; length[0] as usize];
            stream.read_exact(&mut domain).expect("read SOCKS5 domain");
            String::from_utf8(domain).expect("SOCKS5 domain is UTF-8")
        }
        other => panic!("unexpected SOCKS5 address type {other}"),
    };
    let mut port = [0_u8; 2];
    stream.read_exact(&mut port).expect("read SOCKS5 port");
    stream
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
        .expect("write SOCKS5 success");
    format!("{host}:{}", u16::from_be_bytes(port))
}

fn spawn_socks_proxy(
    kind: SocksKind,
) -> (std::net::SocketAddr, thread::JoinHandle<SocksObservation>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SOCKS proxy");
    let address = listener.local_addr().expect("SOCKS proxy address");
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept SOCKS connection");
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .expect("set SOCKS read timeout");
        let destination = match kind {
            SocksKind::Four | SocksKind::FourA => serve_socks4(&mut stream),
            SocksKind::Five | SocksKind::FiveH => serve_socks5(&mut stream),
        };
        let request_head = read_head(&mut stream);
        stream.write_all(RESPONSE).expect("write SOCKS response");
        SocksObservation {
            destination,
            request_head,
        }
    });
    (address, task)
}

fn exercise_socks(kind: SocksKind, target_host: &str) -> SocksObservation {
    let (proxy_address, proxy_task) = spawn_socks_proxy(kind);
    let proxy = match kind {
        SocksKind::Four => Proxy::Socks4(
            format!("socks4://{proxy_address}")
                .parse()
                .expect("valid SOCKS4 URI"),
        ),
        SocksKind::FourA => Proxy::Socks4(
            format!("socks4a://{proxy_address}")
                .parse()
                .expect("valid SOCKS4a URI"),
        ),
        SocksKind::Five => Proxy::Socks5 {
            uri: format!("socks5://{proxy_address}")
                .parse()
                .expect("valid SOCKS5 URI"),
            remote_dns: false,
        },
        SocksKind::FiveH => Proxy::Socks5 {
            uri: format!("socks5h://{proxy_address}")
                .parse()
                .expect("valid SOCKS5h URI"),
            remote_dns: true,
        },
    };
    let client = Client::builder()
        .proxy(proxy)
        .build()
        .expect("build SOCKS proxy client");
    let response = runtime()
        .block_on(
            client
                .get(format!("http://{target_host}:8124/socks"))
                .send(),
        )
        .expect("SOCKS request");
    assert_eq!(
        runtime().block_on(response.bytes()).expect("SOCKS body"),
        "proxy-ok"
    );
    proxy_task.join().expect("join SOCKS proxy")
}

#[test]
fn socks4_and_socks4a_preserve_local_versus_remote_dns_semantics() {
    let local = exercise_socks(SocksKind::Four, "127.0.0.1");
    assert_eq!(local.destination, "127.0.0.1:8124");
    assert!(local.request_head.starts_with(b"GET /socks HTTP/1.1\r\n"));

    let remote = exercise_socks(SocksKind::FourA, "unresolved.invalid");
    assert_eq!(remote.destination, "unresolved.invalid:8124");
    assert!(remote.request_head.starts_with(b"GET /socks HTTP/1.1\r\n"));
}

#[test]
fn socks5_and_socks5h_preserve_local_versus_remote_dns_semantics() {
    let local = exercise_socks(SocksKind::Five, "127.0.0.1");
    assert_eq!(local.destination, "127.0.0.1:8124");
    assert!(local.request_head.starts_with(b"GET /socks HTTP/1.1\r\n"));

    let remote = exercise_socks(SocksKind::FiveH, "unresolved.invalid");
    assert_eq!(remote.destination, "unresolved.invalid:8124");
    assert!(remote.request_head.starts_with(b"GET /socks HTTP/1.1\r\n"));
}

fn tls_acceptor() -> TlsAcceptor {
    let certificate = include_bytes!("../../../tests/certs/valid/server/server.pem");
    let private_key = include_bytes!("../../../tests/certs/valid/server/server.key");
    let certificates = rustls_pemfile::certs(&mut std::io::Cursor::new(certificate))
        .collect::<Result<Vec<_>, _>>()
        .expect("parse server certificate");
    let private_key = rustls_pemfile::private_key(&mut std::io::Cursor::new(private_key))
        .expect("parse server key")
        .expect("server key exists");
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .expect("certificate matches key");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsAcceptor::from(std::sync::Arc::new(config))
}

fn spawn_tls_origin() -> (std::net::SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let (address_sender, address_receiver) = mpsc::channel();
    let task = thread::spawn(move || {
        let runtime = runtime();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind TLS origin");
            address_sender
                .send(listener.local_addr().expect("TLS origin address"))
                .expect("publish TLS origin address");
            let (stream, _) = listener.accept().await.expect("accept TLS origin");
            let mut stream = tls_acceptor()
                .accept(stream)
                .await
                .expect("accept tunneled TLS");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < MAX_HEAD);
                let mut byte = [0_u8; 1];
                stream
                    .read_exact(&mut byte)
                    .await
                    .expect("read tunneled request");
                request.push(byte[0]);
            }
            stream
                .write_all(RESPONSE)
                .await
                .expect("write tunneled response");
            request
        })
    });
    (
        address_receiver.recv().expect("receive TLS origin address"),
        task,
    )
}

fn spawn_connect_proxy(
    origin: std::net::SocketAddr,
) -> (std::net::SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind CONNECT proxy");
    let address = listener.local_addr().expect("CONNECT proxy address");
    let task = thread::spawn(move || {
        let (mut client, _) = listener.accept().expect("accept CONNECT client");
        let connect_head = read_head(&mut client);
        let mut upstream = TcpStream::connect(origin).expect("connect TLS origin");
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("write CONNECT response");
        let mut client_read = client.try_clone().expect("clone CONNECT client");
        let mut upstream_write = upstream.try_clone().expect("clone upstream");
        let forward = thread::spawn(move || {
            std::io::copy(&mut client_read, &mut upstream_write).expect("forward client to origin");
        });
        std::io::copy(&mut upstream, &mut client).expect("forward origin to client");
        forward.join().expect("join client forwarding");
        connect_head
    });
    (address, task)
}

fn frozen_ca_bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/certs/expired/ca/ca.crt")
}

#[test]
fn https_uses_authenticated_connect_then_tls_and_origin_form() {
    let (origin_address, origin_task) = spawn_tls_origin();
    let (proxy_address, proxy_task) = spawn_connect_proxy(origin_address);
    let proxy_uri = format!("http://connect-user:connect-pass@{proxy_address}")
        .parse::<Uri>()
        .expect("valid CONNECT proxy URI");
    let client = Client::builder()
        .proxy(Proxy::Http(proxy_uri))
        .tls(TlsConfig {
            roots: CertificateSource::PemBundle(frozen_ca_bundle()),
            identity: None,
        })
        .build()
        .expect("build CONNECT client");
    let url = format!("https://localhost:{}/secure?q=one", origin_address.port());

    let response = runtime()
        .block_on(client.get(url).send())
        .expect("HTTPS through CONNECT");
    assert_eq!(
        runtime().block_on(response.bytes()).expect("tunneled body"),
        "proxy-ok"
    );

    let connect = String::from_utf8(proxy_task.join().expect("join CONNECT proxy"))
        .expect("CONNECT request is ASCII");
    assert!(
        connect.starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1\r\n",
            origin_address.port()
        )),
        "{connect:?}"
    );
    assert!(
        connect.contains("Basic Y29ubmVjdC11c2VyOmNvbm5lY3QtcGFzcw=="),
        "{connect:?}"
    );
    assert!(!connect.contains("connect-user:connect-pass@"));

    let tunneled = String::from_utf8(origin_task.join().expect("join TLS origin"))
        .expect("origin request is ASCII");
    assert!(
        tunneled.starts_with("GET /secure?q=one HTTP/1.1\r\n"),
        "{tunneled:?}"
    );
    assert!(
        !tunneled
            .to_ascii_lowercase()
            .contains("proxy-authorization")
    );
}

#[test]
fn proxy_rejection_is_typed_and_does_not_reach_the_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rejecting proxy");
    let address = listener.local_addr().expect("rejecting proxy address");
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept rejecting proxy");
        let request = read_head(&mut stream);
        stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .expect("write proxy rejection");
        request
    });
    let client = Client::builder()
        .proxy(Proxy::Http(
            format!("http://{address}")
                .parse()
                .expect("valid rejecting proxy URI"),
        ))
        .build()
        .expect("build rejecting proxy client");

    let error = match runtime().block_on(client.get("https://origin.invalid/fail").send()) {
        Err(error) => error,
        Ok(response) => {
            drop(response);
            panic!("CONNECT rejection must fail")
        }
    };
    assert_eq!(error.kind(), ErrorKind::Proxy);
    assert!(error.to_string().contains("407"), "{error}");
    assert!(
        task.join()
            .expect("join rejecting proxy")
            .starts_with(b"CONNECT origin.invalid:443 HTTP/1.1\r\n")
    );
}

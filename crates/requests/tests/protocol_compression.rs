use std::future::poll_fn;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use requests::{Client, ContentCodecs, ErrorKind, HeaderName, HeaderValue, Response, ResponseBody};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ASYNC_TIMEOUT: Duration = Duration::from_secs(3);
const PENDING_BOUND: Duration = Duration::from_millis(100);
const PAYLOAD: &[u8] = b"alpha-beta-gamma-delta|alpha-beta-gamma-delta|\
alpha-beta-gamma-delta|alpha-beta-gamma-delta|\
\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\
\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f";

const GZIP: &str = "1f8b08000000000002ff4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00fe41f4157c000000";
const ZLIB_DEFLATE: &str = "789c4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00baaa24b9";
const RAW_DEFLATE: &str = "4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00";
const BROTLI: &str = "1b7b00e80572714853f8ae5dd22c2c0d19e4aa541a32e9e7b8ca878604b5f77842830f484801";
const ZSTANDARD: &str = "28b52ffd207cfd01007403616c7068612d626574612d67616d6d612d64656c74617c000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f010045d1d904";

const EMPTY_GZIP: &str = "1f8b08000000000002ff03000000000000000000";
const EMPTY_ZLIB: &str = "789c030000000001";
const EMPTY_RAW_DEFLATE: &str = "0300";
const EMPTY_BROTLI: &str = "3b";
const EMPTY_ZSTANDARD: &str = "28b52ffd2000010000";

const FIRST_MEMBER: &[u8] = b"first-member:";
const SECOND_MEMBER: &[u8] = b"second-member!";
const FIRST_GZIP: &str = "1f8b08000000000002ff4bcb2c2a2ed1cd4dcd4d4a2db20200dad159b70d000000";
const SECOND_GZIP: &str = "1f8b08000000000002ff2b4e4dcecf4bd1cd4dcd4d4a2d520400a7f4f2390e000000";
const FIRST_ZSTANDARD: &str = "28b52ffd200d69000066697273742d6d656d6265723a";
const SECOND_ZSTANDARD: &str = "28b52ffd200e7100007365636f6e642d6d656d62657221";
const FIRST_ZLIB: &str = "789c4bcb2c2a2ed1cd4dcd4d4a2db2020024560508";
const SECOND_ZLIB: &str = "789c2b4e4dcecf4bd1cd4dcd4d4a2d52040029500543";
const FIRST_RAW_DEFLATE: &str = "4bcb2c2a2ed1cd4dcd4d4a2db20200";
const SECOND_RAW_DEFLATE: &str = "2b4e4dcecf4bd1cd4dcd4d4a2d520400";

const CORRUPT_GZIP: &str = "1f8b08000000000002ff4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00f1b1f4157c000000";
const CORRUPT_ZLIB: &str = "789c4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00baaa2446";

#[derive(Clone, Copy)]
struct CodecCase {
    name: &'static str,
    encoding: &'static str,
    wire: &'static str,
}

fn hex_bytes(input: &str) -> Vec<u8> {
    assert_eq!(input.len() % 2, 0, "fixture hex must contain byte pairs");
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("fixture hex is ASCII");
            u8::from_str_radix(pair, 16).expect("fixture hex contains valid digits")
        })
        .collect()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build caller-owned runtime")
}

fn send_response(runtime: &tokio::runtime::Runtime, client: &Client, url: &str) -> Response {
    runtime
        .block_on(async { tokio::time::timeout(ASYNC_TIMEOUT, client.get(url).send()).await })
        .expect("response head timed out")
        .expect("request failed")
}

fn response_bytes(
    runtime: &tokio::runtime::Runtime,
    response: Response,
) -> requests::Result<Bytes> {
    runtime
        .block_on(async { tokio::time::timeout(ASYNC_TIMEOUT, response.bytes()).await })
        .expect("response body timed out")
}

fn response_body_bytes(
    runtime: &tokio::runtime::Runtime,
    mut body: ResponseBody,
) -> requests::Result<Bytes> {
    runtime.block_on(async {
        let mut collected = Vec::new();
        while let Some(frame) = tokio::time::timeout(ASYNC_TIMEOUT, next_frame(&mut body))
            .await
            .expect("response body frame timed out")
        {
            collected.extend_from_slice(&frame?);
        }
        Ok(Bytes::from(collected))
    })
}

async fn next_frame(body: &mut ResponseBody) -> Option<requests::Result<Bytes>> {
    poll_fn(|context| Pin::new(&mut *body).poll_next(context)).await
}

fn captured_headers(request: &[u8], expected_name: &str) -> Vec<String> {
    let head = std::str::from_utf8(request).expect("fixture request head is UTF-8");
    head.split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(expected_name)
                .then(|| value.trim().to_owned())
        })
        .collect()
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("set request read timeout: {error}"))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("read request: {error}"))?;
        if read == 0 {
            return Err("peer closed before request head completed".to_owned());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

fn write_fixed_response(
    stream: &mut TcpStream,
    encoding: &str,
    body: &[u8],
    connection: &str,
) -> Result<(), String> {
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("set response write timeout: {error}"))?;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\n\
         Content-Length: {}\r\nX-Wire-Fixture: preserved\r\n\
         Connection: {connection}\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|error| format!("write response head: {error}"))?;
    for part in body.chunks((body.len() / 3).max(1)) {
        stream
            .write_all(part)
            .map_err(|error| format!("write response body part: {error}"))?;
        stream
            .flush()
            .map_err(|error| format!("flush response body part: {error}"))?;
        thread::yield_now();
    }
    Ok(())
}

fn write_chunked_response(
    stream: &mut TcpStream,
    encoding: &str,
    body: &[u8],
    connection: &str,
) -> Result<(), String> {
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("set response write timeout: {error}"))?;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\n\
         Transfer-Encoding: chunked\r\nX-Wire-Fixture: preserved\r\n\
         Connection: {connection}\r\n\r\n"
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|error| format!("write response head: {error}"))?;
    for part in body.chunks((body.len() / 3).max(1)) {
        write_http_chunk(stream, part)?;
        thread::yield_now();
    }
    stream
        .write_all(b"0\r\n\r\n")
        .map_err(|error| format!("finish chunked response: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush chunked response: {error}"))
}

struct FixedServer {
    address: SocketAddr,
    worker: Option<JoinHandle<Result<Vec<u8>, String>>>,
}

impl FixedServer {
    fn spawn(encoding: &'static str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind fixed response fixture");
        let address = listener.local_addr().expect("read fixture address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept request: {error}"))?;
            let request = read_request(&mut stream)?;
            write_fixed_response(&mut stream, encoding, &body, "close")?;
            Ok(request)
        });
        Self {
            address,
            worker: Some(worker),
        }
    }

    fn spawn_chunked(encoding: &'static str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind chunked response fixture");
        let address = listener.local_addr().expect("read fixture address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept request: {error}"))?;
            let request = read_request(&mut stream)?;
            write_chunked_response(&mut stream, encoding, &body, "close")?;
            Ok(request)
        });
        Self {
            address,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn finish(mut self) -> Result<Vec<u8>, String> {
        self.worker
            .take()
            .expect("fixture worker exists")
            .join()
            .map_err(|_| "fixed fixture worker panicked".to_owned())?
    }
}

impl Drop for FixedServer {
    fn drop(&mut self) {
        // A failed assertion must not make fixture cleanup wait on socket I/O.
        drop(self.worker.take());
    }
}

fn assert_decoded_bytes_case(case: CodecCase) {
    let runtime = runtime();
    let client = Client::new().expect("build client");
    let wire = hex_bytes(case.wire);
    let server = FixedServer::spawn(case.encoding, wire.clone());
    let response = send_response(
        &runtime,
        &client,
        &server.url(&format!("/decode/{}", case.name)),
    );

    assert_eq!(
        response.headers().get("content-encoding").unwrap(),
        case.encoding,
        "{} Content-Encoding must describe the raw representation",
        case.name
    );
    assert_eq!(
        response.headers().get("content-length").unwrap(),
        wire.len().to_string().as_str(),
        "{} Content-Length must remain the compressed wire length",
        case.name
    );
    assert_eq!(response.content_length(), Some(wire.len() as u64));
    assert_eq!(
        response.headers().get("x-wire-fixture").unwrap(),
        "preserved"
    );
    assert_eq!(
        response_bytes(&runtime, response).expect("decode response bytes"),
        PAYLOAD,
        "{} bytes were not decoded",
        case.name
    );

    let request = server.finish().expect("fixed fixture completed");
    assert!(request.starts_with(b"GET /decode/"));
}

macro_rules! decoded_codec_test {
    ($test:ident, $name:literal, $encoding:literal, $wire:expr) => {
        #[test]
        fn $test() {
            assert_decoded_bytes_case(CodecCase {
                name: $name,
                encoding: $encoding,
                wire: $wire,
            });
        }
    };
}

decoded_codec_test!(decoded_bytes_support_gzip, "gzip", "gzip", GZIP);
decoded_codec_test!(
    decoded_bytes_support_zlib_wrapped_deflate,
    "zlib-deflate",
    "deflate",
    ZLIB_DEFLATE
);
decoded_codec_test!(
    decoded_bytes_support_raw_deflate_fallback,
    "raw-deflate",
    "deflate",
    RAW_DEFLATE
);
decoded_codec_test!(decoded_bytes_support_brotli, "brotli", "br", BROTLI);
decoded_codec_test!(
    decoded_bytes_support_zstandard,
    "zstandard",
    "zstd",
    ZSTANDARD
);

#[test]
fn empty_raw_wire_with_content_encoding_remains_empty() {
    let runtime = runtime();
    let client = Client::new().expect("build client");

    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let server = FixedServer::spawn(encoding, Vec::new());
        let response = send_response(
            &runtime,
            &client,
            &server.url(&format!("/empty-wire/{encoding}")),
        );
        assert!(
            response_bytes(&runtime, response)
                .expect("empty encoded response succeeds")
                .is_empty(),
            "empty wire body with Content-Encoding {encoding} must stay empty"
        );
        server.finish().expect("empty wire fixture completed");
    }
}

fn assert_compressed_empty_frame(encoding: &'static str, frame: &'static str) {
    let runtime = runtime();
    let client = Client::new().expect("build client");
    let server = FixedServer::spawn(encoding, hex_bytes(frame));
    let response = send_response(
        &runtime,
        &client,
        &server.url(&format!("/empty-frame/{encoding}")),
    );
    assert!(
        response_bytes(&runtime, response)
            .expect("compressed empty frame succeeds")
            .is_empty(),
        "valid compressed-empty {encoding} frame must decode empty"
    );
    server.finish().expect("empty frame fixture completed");
}

macro_rules! compressed_empty_test {
    ($test:ident, $encoding:literal, $frame:expr) => {
        #[test]
        fn $test() {
            assert_compressed_empty_frame($encoding, $frame);
        }
    };
}

compressed_empty_test!(compressed_empty_gzip_decodes_empty, "gzip", EMPTY_GZIP);
compressed_empty_test!(compressed_empty_zlib_decodes_empty, "deflate", EMPTY_ZLIB);
compressed_empty_test!(
    compressed_empty_raw_deflate_decodes_empty,
    "deflate",
    EMPTY_RAW_DEFLATE
);
compressed_empty_test!(compressed_empty_brotli_decodes_empty, "br", EMPTY_BROTLI);
compressed_empty_test!(
    compressed_empty_zstandard_decodes_empty,
    "zstd",
    EMPTY_ZSTANDARD
);

fn assert_concatenated_streams(
    encoding: &'static str,
    first: &'static str,
    second: &'static str,
    expected: &[u8],
) {
    let runtime = runtime();
    let client = Client::new().expect("build client");
    let mut wire = hex_bytes(first);
    wire.extend(hex_bytes(second));
    let server = FixedServer::spawn(encoding, wire);
    let response = send_response(&runtime, &client, &server.url("/concatenated"));
    assert_eq!(
        response_bytes(&runtime, response).expect("decode concatenated response"),
        expected,
        "unexpected concatenated {encoding} behavior"
    );
    server.finish().expect("concatenated fixture completed");
}

#[test]
fn concatenated_gzip_decodes_every_member() {
    assert_concatenated_streams(
        "gzip",
        FIRST_GZIP,
        SECOND_GZIP,
        &[FIRST_MEMBER, SECOND_MEMBER].concat(),
    );
}

#[test]
fn concatenated_zstandard_decodes_every_frame() {
    assert_concatenated_streams(
        "zstd",
        FIRST_ZSTANDARD,
        SECOND_ZSTANDARD,
        &[FIRST_MEMBER, SECOND_MEMBER].concat(),
    );
}

#[test]
fn concatenated_zlib_deflate_decodes_only_the_first_stream() {
    assert_concatenated_streams("deflate", FIRST_ZLIB, SECOND_ZLIB, FIRST_MEMBER);
}

#[test]
fn concatenated_raw_deflate_decodes_only_the_first_stream() {
    assert_concatenated_streams(
        "deflate",
        FIRST_RAW_DEFLATE,
        SECOND_RAW_DEFLATE,
        FIRST_MEMBER,
    );
}

fn configured_client(brotli: bool, zstandard: bool) -> Client {
    Client::builder()
        .content_codecs(ContentCodecs::new(brotli, zstandard))
        .build()
        .expect("build codec-configured client")
}

fn assert_inventory_header(brotli: bool, zstandard: bool, expected: &str) {
    let runtime = runtime();
    let client = configured_client(brotli, zstandard);
    let server = FixedServer::spawn("gzip", Vec::new());
    let response = send_response(&runtime, &client, &server.url("/inventory"));
    assert!(
        response_bytes(&runtime, response)
            .expect("read empty inventory response")
            .is_empty()
    );
    let request = server.finish().expect("inventory fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec![expected.to_owned()]
    );
}

#[test]
fn content_inventory_default_client_advertises_all_compiled_codecs() {
    let runtime = runtime();
    let client = Client::new().expect("build default client");
    let server = FixedServer::spawn("gzip", Vec::new());
    let response = send_response(&runtime, &client, &server.url("/inventory/default"));
    assert!(
        response_bytes(&runtime, response)
            .expect("read empty default response")
            .is_empty()
    );
    let request = server
        .finish()
        .expect("default inventory fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec!["gzip, deflate, br, zstd"]
    );
}

#[test]
fn content_inventory_configured_client_advertises_brotli_and_zstandard() {
    assert_inventory_header(true, true, "gzip, deflate, br, zstd");
}

#[test]
fn content_inventory_configured_client_omits_brotli() {
    assert_inventory_header(false, true, "gzip, deflate, zstd");
}

#[test]
fn content_inventory_configured_client_omits_zstandard() {
    assert_inventory_header(true, false, "gzip, deflate, br");
}

#[test]
fn content_inventory_configured_client_omits_both_optional_codecs() {
    assert_inventory_header(false, false, "gzip, deflate");
}

fn assert_optional_codec_projection(
    encoding: &'static str,
    wire: &'static str,
    client: &Client,
    expected: &[u8],
    path: &str,
) {
    let runtime = runtime();
    let wire = hex_bytes(wire);
    let server = FixedServer::spawn(encoding, wire.clone());
    let response = send_response(&runtime, client, &server.url(path));
    assert_eq!(
        response.headers().get("content-encoding").unwrap(),
        encoding
    );
    assert_eq!(
        response.headers().get("content-length").unwrap(),
        wire.len().to_string().as_str()
    );
    assert_eq!(response.content_length(), Some(wire.len() as u64));
    assert_eq!(
        response.headers().get("x-wire-fixture").unwrap(),
        "preserved"
    );
    assert_eq!(
        response_bytes(&runtime, response).expect("read optional codec response"),
        expected
    );
    server.finish().expect("optional codec fixture completed");
}

#[test]
fn content_inventory_enabled_brotli_decodes_the_public_body() {
    let client = configured_client(true, false);
    assert_optional_codec_projection("br", BROTLI, &client, PAYLOAD, "/inventory/br/enabled");
}

#[test]
fn content_inventory_disabled_brotli_passes_the_raw_representation_through() {
    let client = configured_client(false, true);
    let wire = hex_bytes(BROTLI);
    assert_optional_codec_projection("br", BROTLI, &client, &wire, "/inventory/br/disabled");
}

#[test]
fn content_inventory_enabled_zstandard_decodes_the_public_body() {
    let client = configured_client(false, true);
    assert_optional_codec_projection(
        "zstd",
        ZSTANDARD,
        &client,
        PAYLOAD,
        "/inventory/zstd/enabled",
    );
}

#[test]
fn content_inventory_disabled_zstandard_passes_the_raw_representation_through() {
    let client = configured_client(true, false);
    let wire = hex_bytes(ZSTANDARD);
    assert_optional_codec_projection(
        "zstd",
        ZSTANDARD,
        &client,
        &wire,
        "/inventory/zstd/disabled",
    );
}

#[test]
fn content_inventory_gzip_remains_enabled_when_optional_codecs_are_disabled() {
    let client = configured_client(false, false);
    assert_optional_codec_projection("gzip", GZIP, &client, PAYLOAD, "/inventory/gzip/required");
}

#[test]
fn content_inventory_deflate_remains_enabled_when_optional_codecs_are_disabled() {
    let client = configured_client(false, false);
    assert_optional_codec_projection(
        "deflate",
        ZLIB_DEFLATE,
        &client,
        PAYLOAD,
        "/inventory/deflate/required",
    );
}

fn send_with_accept_encoding(
    runtime: &tokio::runtime::Runtime,
    client: &Client,
    url: String,
    value: &'static str,
) -> Response {
    runtime
        .block_on(async {
            tokio::time::timeout(
                ASYNC_TIMEOUT,
                client
                    .get(url)
                    .header(
                        HeaderName::from_static("accept-encoding"),
                        HeaderValue::from_static(value),
                    )
                    .send(),
            )
            .await
        })
        .expect("explicit Accept-Encoding response head timed out")
        .expect("explicit Accept-Encoding request failed")
}

#[test]
fn content_inventory_explicit_accept_encoding_is_the_only_value_sent() {
    let runtime = runtime();
    let client = configured_client(true, true);
    let server = FixedServer::spawn("gzip", Vec::new());
    let response = send_with_accept_encoding(
        &runtime,
        &client,
        server.url("/explicit/preserved"),
        "identity",
    );
    assert!(
        response_bytes(&runtime, response)
            .expect("read empty explicit response")
            .is_empty()
    );
    let request = server.finish().expect("explicit fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec!["identity"]
    );
}

#[test]
fn content_inventory_explicit_header_does_not_disable_configured_brotli_decoder() {
    let runtime = runtime();
    let client = configured_client(true, false);

    let server = FixedServer::spawn("br", hex_bytes(BROTLI));
    let response =
        send_with_accept_encoding(&runtime, &client, server.url("/explicit/br"), "identity");
    let request = server.finish().expect("explicit Brotli fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec!["identity"]
    );
    assert_eq!(
        response_bytes(&runtime, response).expect("decode enabled Brotli"),
        PAYLOAD
    );
}

#[test]
fn content_inventory_explicit_header_does_not_enable_disabled_zstandard_decoder() {
    let runtime = runtime();
    let client = configured_client(true, false);
    let server = FixedServer::spawn("zstd", hex_bytes(ZSTANDARD));
    let response =
        send_with_accept_encoding(&runtime, &client, server.url("/explicit/zstd"), "br, zstd");
    let request = server
        .finish()
        .expect("explicit Zstandard fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec!["br, zstd"]
    );
    assert_eq!(
        response_bytes(&runtime, response).expect("read disabled Zstandard representation"),
        hex_bytes(ZSTANDARD)
    );
}

#[test]
fn content_inventory_x_gzip_is_decoded_but_never_advertised() {
    let runtime = runtime();
    let client = configured_client(false, false);
    let server = FixedServer::spawn("x-gzip", hex_bytes(GZIP));
    let response = send_response(&runtime, &client, &server.url("/inventory/x-gzip"));
    assert_eq!(
        response_bytes(&runtime, response).expect("decode x-gzip response"),
        PAYLOAD
    );
    let request = server.finish().expect("x-gzip fixture completed");
    assert_eq!(
        captured_headers(&request, "accept-encoding"),
        vec!["gzip, deflate"]
    );
}

#[test]
fn content_inventory_body_view_selectors_consume_the_response() {
    let _: fn(Response) -> ResponseBody = Response::into_body;
    let _: fn(Response) -> ResponseBody = Response::into_raw_body;
}

#[test]
fn content_inventory_raw_body_view_is_transfer_deframed_and_content_encoded() {
    let runtime = runtime();
    let client = configured_client(true, true);
    let wire = hex_bytes(GZIP);

    let raw_server = FixedServer::spawn_chunked("gzip", wire.clone());
    let raw_response = send_response(&runtime, &client, &raw_server.url("/view/raw"));
    assert_eq!(
        raw_response.headers().get("content-encoding").unwrap(),
        "gzip"
    );
    assert_eq!(
        raw_response.headers().get("transfer-encoding").unwrap(),
        "chunked"
    );
    assert!(raw_response.headers().get("content-length").is_none());
    assert_eq!(raw_response.content_length(), None);
    assert_eq!(
        raw_response.headers().get("x-wire-fixture").unwrap(),
        "preserved"
    );
    let raw_body = raw_response.into_raw_body();
    assert_eq!(
        response_body_bytes(&runtime, raw_body).expect("read raw body view"),
        wire
    );
    raw_server.finish().expect("raw view fixture completed");
}

#[test]
fn content_inventory_decoded_body_view_preserves_wire_headers() {
    let runtime = runtime();
    let client = configured_client(true, true);
    let wire = hex_bytes(GZIP);
    let decoded_server = FixedServer::spawn("gzip", hex_bytes(GZIP));
    let decoded_response = send_response(&runtime, &client, &decoded_server.url("/view/decoded"));
    assert_eq!(
        decoded_response.headers().get("content-encoding").unwrap(),
        "gzip"
    );
    assert_eq!(decoded_response.content_length(), Some(wire.len() as u64));
    assert_eq!(
        decoded_response.headers().get("x-wire-fixture").unwrap(),
        "preserved"
    );
    let decoded_body = decoded_response.into_body();
    assert_eq!(
        response_body_bytes(&runtime, decoded_body).expect("read decoded body view"),
        PAYLOAD
    );
    decoded_server
        .finish()
        .expect("decoded view fixture completed");
}

enum GateCommand {
    ReleaseTail,
    FinishHttpBody,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateEvent {
    PrefixSent,
    TailSent,
    SecondConnectionServed,
    HttpBodyFinished,
}

#[derive(Debug)]
struct GatedObservation {
    accepted_connections: usize,
    first_request: Vec<u8>,
    second_request: Vec<u8>,
    reused_request: Vec<u8>,
}

struct GatedServer {
    address: SocketAddr,
    commands: Sender<GateCommand>,
    events: Receiver<GateEvent>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<GatedObservation, String>>>,
}

impl GatedServer {
    fn spawn(wire: Vec<u8>, split: usize) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind gated fixture");
        listener
            .set_nonblocking(true)
            .expect("make gated listener nonblocking");
        let address = listener.local_addr().expect("read gated fixture address");
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            let mut first = accept_gated(&listener, &worker_shutdown)?;
            let first_request = read_gated_request(&mut first, &worker_shutdown)?;
            first
                .set_nonblocking(false)
                .map_err(|error| format!("make first connection blocking: {error}"))?;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\
                      Transfer-Encoding: chunked\r\n\r\n",
                )
                .map_err(|error| format!("write gated response head: {error}"))?;
            write_http_chunk(&mut first, &wire[..split])?;
            events_tx
                .send(GateEvent::PrefixSent)
                .map_err(|_| "report gated prefix".to_owned())?;

            match recv_gated_command(&commands_rx, &worker_shutdown)? {
                GateCommand::ReleaseTail => {}
                GateCommand::FinishHttpBody => {
                    return Err("finish command arrived before tail release".to_owned());
                }
            }
            write_http_chunk(&mut first, &wire[split..])?;
            events_tx
                .send(GateEvent::TailSent)
                .map_err(|_| "report gated tail".to_owned())?;

            let mut second = accept_gated(&listener, &worker_shutdown)?;
            let second_request = read_gated_request(&mut second, &worker_shutdown)?;
            second
                .set_nonblocking(false)
                .map_err(|error| format!("make second connection blocking: {error}"))?;
            second
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\
                      Connection: close\r\n\r\nsecond",
                )
                .map_err(|error| format!("write second response: {error}"))?;
            second
                .flush()
                .map_err(|error| format!("flush second response: {error}"))?;
            drop(second);
            events_tx
                .send(GateEvent::SecondConnectionServed)
                .map_err(|_| "report second connection".to_owned())?;

            match recv_gated_command(&commands_rx, &worker_shutdown)? {
                GateCommand::FinishHttpBody => {}
                GateCommand::ReleaseTail => {
                    return Err("tail release command repeated".to_owned());
                }
            }
            first
                .write_all(b"0\r\n\r\n")
                .map_err(|error| format!("finish chunked response: {error}"))?;
            first
                .flush()
                .map_err(|error| format!("flush chunked EOF: {error}"))?;
            events_tx
                .send(GateEvent::HttpBodyFinished)
                .map_err(|_| "report HTTP EOF".to_owned())?;

            let reused_request = read_gated_request(&mut first, &worker_shutdown)?;
            first
                .set_nonblocking(false)
                .map_err(|error| format!("make reused connection blocking: {error}"))?;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\
                      Connection: close\r\n\r\nreused",
                )
                .map_err(|error| format!("write reused response: {error}"))?;
            first
                .flush()
                .map_err(|error| format!("flush reused response: {error}"))?;

            Ok(GatedObservation {
                accepted_connections: 2,
                first_request,
                second_request,
                reused_request,
            })
        });
        Self {
            address,
            commands: commands_tx,
            events: events_rx,
            shutdown,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn wait(&self, expected: GateEvent) {
        assert_eq!(
            self.events
                .recv_timeout(IO_TIMEOUT)
                .expect("gated fixture event timed out"),
            expected
        );
    }

    fn send(&self, command: GateCommand) {
        self.commands
            .send(command)
            .expect("send gated fixture command");
    }

    fn finish(mut self) -> Result<GatedObservation, String> {
        self.worker
            .take()
            .expect("gated worker exists")
            .join()
            .map_err(|_| "gated fixture worker panicked".to_owned())?
    }
}

impl Drop for GatedServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn accept_gated(listener: &TcpListener, shutdown: &AtomicBool) -> Result<TcpStream, String> {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err("gated fixture shut down".to_owned());
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(format!("accept gated connection: {error}")),
        }
    }
}

fn read_gated_request(stream: &mut TcpStream, shutdown: &AtomicBool) -> Result<Vec<u8>, String> {
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("make gated connection nonblocking: {error}"))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        if shutdown.load(Ordering::Acquire) {
            return Err("gated fixture shut down".to_owned());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("peer closed before gated request completed".to_owned()),
            Ok(read) => request.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(format!("read gated request: {error}")),
        }
    }
    Ok(request)
}

fn recv_gated_command(
    commands: &Receiver<GateCommand>,
    shutdown: &AtomicBool,
) -> Result<GateCommand, String> {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err("gated fixture shut down".to_owned());
        }
        match commands.recv_timeout(Duration::from_millis(2)) {
            Ok(command) => return Ok(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("gated command channel closed".to_owned());
            }
        }
    }
}

fn write_http_chunk(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    write!(stream, "{:x}\r\n", bytes.len())
        .map_err(|error| format!("write HTTP chunk length: {error}"))?;
    stream
        .write_all(bytes)
        .map_err(|error| format!("write HTTP chunk data: {error}"))?;
    stream
        .write_all(b"\r\n")
        .map_err(|error| format!("write HTTP chunk trailer: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush HTTP chunk: {error}"))
}

#[test]
fn decoded_body_streams_before_raw_tail_and_waits_for_http_eof_before_reuse() {
    let runtime = runtime();
    let client = Client::new().expect("build pooled client");
    let server = GatedServer::spawn(hex_bytes(GZIP), 16);

    let response = send_response(&runtime, &client, &server.url("/gated/first"));
    let mut decoded_body = response.into_body();
    server.wait(GateEvent::PrefixSent);
    let first_decoded = runtime
        .block_on(async {
            tokio::time::timeout(ASYNC_TIMEOUT, next_frame(&mut decoded_body)).await
        })
        .expect("decoded prefix timed out")
        .expect("decoded prefix frame missing")
        .expect("decoded prefix failed");
    assert!(
        !first_decoded.is_empty() && PAYLOAD.starts_with(&first_decoded),
        "the first public body frame must be a decoded payload prefix"
    );

    server.send(GateCommand::ReleaseTail);
    server.wait(GateEvent::TailSent);
    let mut decoded = first_decoded.to_vec();
    while decoded.len() < PAYLOAD.len() {
        let frame = runtime
            .block_on(async {
                tokio::time::timeout(ASYNC_TIMEOUT, next_frame(&mut decoded_body)).await
            })
            .expect("decoded tail timed out")
            .expect("decoded stream ended before the payload")
            .expect("decoded tail failed");
        decoded.extend_from_slice(&frame);
    }
    assert_eq!(decoded, PAYLOAD);

    let second = send_response(&runtime, &client, &server.url("/gated/second"));
    assert_eq!(
        response_bytes(&runtime, second).expect("read second response"),
        b"second".as_slice()
    );
    server.wait(GateEvent::SecondConnectionServed);

    let premature_eof = runtime.block_on(async {
        tokio::time::timeout(PENDING_BOUND, next_frame(&mut decoded_body)).await
    });
    assert!(
        premature_eof.is_err(),
        "decoder EOF must wait for the underlying HTTP body EOF"
    );

    server.send(GateCommand::FinishHttpBody);
    server.wait(GateEvent::HttpBodyFinished);
    assert!(
        runtime
            .block_on(async {
                tokio::time::timeout(ASYNC_TIMEOUT, next_frame(&mut decoded_body)).await
            })
            .expect("decoded EOF timed out")
            .is_none(),
        "decoded stream must end after the underlying HTTP EOF"
    );
    drop(decoded_body);

    let reused = send_response(&runtime, &client, &server.url("/gated/reused"));
    assert_eq!(
        response_bytes(&runtime, reused).expect("read reused response"),
        b"reused".as_slice()
    );

    let observation = server.finish().expect("gated fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.first_request.starts_with(b"GET /gated/first "));
    assert!(
        observation
            .second_request
            .starts_with(b"GET /gated/second ")
    );
    assert!(
        observation
            .reused_request
            .starts_with(b"GET /gated/reused ")
    );
}

#[derive(Debug)]
struct RecoveryObservation {
    accepted_connections: usize,
    corrupt_connection_closed: bool,
}

struct CorruptRecoveryServer {
    address: SocketAddr,
    worker: Option<JoinHandle<Result<RecoveryObservation, String>>>,
}

impl CorruptRecoveryServer {
    fn spawn(encoding: &'static str, corrupt: Vec<u8>) -> Self {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind corrupt response fixture");
        let address = listener.local_addr().expect("read corrupt fixture address");
        let worker = thread::spawn(move || {
            let (mut first, _) = listener
                .accept()
                .map_err(|error| format!("accept corrupt connection: {error}"))?;
            read_request(&mut first)?;
            write_fixed_response(&mut first, encoding, &corrupt, "keep-alive")?;

            let mut probe = [0_u8; 4096];
            let read = first
                .read(&mut probe)
                .map_err(|error| format!("observe corrupt connection disposition: {error}"))?;
            if read == 0 {
                let (mut second, _) = listener
                    .accept()
                    .map_err(|error| format!("accept recovery connection: {error}"))?;
                read_request(&mut second)?;
                second
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\
                          Connection: close\r\n\r\nok",
                    )
                    .map_err(|error| format!("write recovery response: {error}"))?;
                return Ok(RecoveryObservation {
                    accepted_connections: 2,
                    corrupt_connection_closed: true,
                });
            }

            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\
                      Connection: close\r\n\r\nok",
                )
                .map_err(|error| format!("write wrongly reused recovery response: {error}"))?;
            Ok(RecoveryObservation {
                accepted_connections: 1,
                corrupt_connection_closed: false,
            })
        });
        Self {
            address,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn finish(mut self) -> Result<RecoveryObservation, String> {
        self.worker
            .take()
            .expect("corrupt worker exists")
            .join()
            .map_err(|_| "corrupt fixture worker panicked".to_owned())?
    }
}

impl Drop for CorruptRecoveryServer {
    fn drop(&mut self) {
        // A failed assertion must not make fixture cleanup wait on peer EOF.
        drop(self.worker.take());
    }
}

fn assert_content_decoding(error: &requests::Error) {
    assert_eq!(error.kind(), ErrorKind::ContentDecoding);
}

#[test]
fn corrupt_raw_gzip_errors_once_during_decoded_streaming_and_dirties_the_lease() {
    let runtime = runtime();
    let client = Client::new().expect("build pooled client");
    let server = CorruptRecoveryServer::spawn("gzip", hex_bytes(CORRUPT_GZIP));
    let response = send_response(&runtime, &client, &server.url("/corrupt/stream"));
    let mut decoded_body = response.into_body();
    let error = loop {
        match runtime
            .block_on(async {
                tokio::time::timeout(ASYNC_TIMEOUT, next_frame(&mut decoded_body)).await
            })
            .expect("corrupt stream poll timed out")
        {
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("corrupt gzip reached clean decoded EOF"),
        }
    };
    assert_content_decoding(&error);
    assert!(
        runtime
            .block_on(async { next_frame(&mut decoded_body).await })
            .is_none(),
        "a decode error must terminate later public body polls"
    );
    drop(decoded_body);

    let recovery = send_response(&runtime, &client, &server.url("/corrupt/recovery"));
    assert_eq!(
        response_bytes(&runtime, recovery).expect("read recovery response"),
        b"ok".as_slice()
    );
    let observation = server.finish().expect("corrupt fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.corrupt_connection_closed);
}

#[test]
fn corrupt_raw_zlib_errors_from_decoded_bytes_and_forces_a_fresh_connection() {
    let runtime = runtime();
    let client = Client::new().expect("build pooled client");
    let server = CorruptRecoveryServer::spawn("deflate", hex_bytes(CORRUPT_ZLIB));
    let response = send_response(&runtime, &client, &server.url("/corrupt/bytes"));
    let error = response_bytes(&runtime, response).expect_err("corrupt zlib bytes must fail");
    assert_content_decoding(&error);

    let recovery = send_response(&runtime, &client, &server.url("/corrupt/recovery"));
    assert_eq!(
        response_bytes(&runtime, recovery).expect("read recovery response"),
        b"ok".as_slice()
    );
    let observation = server.finish().expect("corrupt fixture completed");
    assert_eq!(observation.accepted_connections, 2);
    assert!(observation.corrupt_connection_closed);
}

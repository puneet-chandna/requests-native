use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use requests_native::{
    AsyncBody, BodySource, CertificateSource, Client, ClientBuilder, ErrorKind, HeaderMap,
    HeaderName, HeaderValue, Identity, Method, Proxy, RequestBuilder, StatusCode, Timeout,
    TlsConfig, Uri, Version,
};

struct PublicStream {
    chunks: VecDeque<Bytes>,
    size_hint: Option<u64>,
}

impl AsyncBody for PublicStream {
    fn poll_next(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<requests_native::Result<Bytes>>> {
        Poll::Ready(self.chunks.pop_front().map(Ok))
    }

    fn size_hint(&self) -> Option<u64> {
        self.size_hint
    }
}

#[test]
fn request_preserves_method_url_headers_and_an_empty_body() {
    let url = "https://example.test/a%2Fb?first=one&second=two";
    let mut headers = HeaderMap::new();
    headers.append(
        HeaderName::from_static("x-example"),
        HeaderValue::from_static("first"),
    );
    headers.append(
        HeaderName::from_static("x-example"),
        HeaderValue::from_static("second"),
    );

    let request = RequestBuilder::new(Method::PATCH, url)
        .headers(headers.clone())
        .build()
        .expect("valid request");

    assert_eq!(request.method(), Method::PATCH);
    assert_eq!(request.url(), url);
    assert_eq!(request.uri(), &url.parse::<Uri>().expect("valid URI"));
    assert_eq!(request.headers(), &headers);
    assert!(matches!(request.body(), BodySource::Empty));
}

#[test]
fn builder_keeps_the_first_error_through_later_stages() {
    let invalid_url = "https://exa mple.test/path";
    let error = RequestBuilder::new(Method::GET, invalid_url)
        .header(
            HeaderName::from_static("x-later-stage"),
            HeaderValue::from_static("still-runs"),
        )
        .body(Bytes::from_static(b"still invalid"))
        .build()
        .expect_err("constructor-stage URL error must survive");

    assert_eq!(error.kind(), ErrorKind::InvalidUrl);
    assert!(error.to_string().contains(invalid_url));
    let _: &dyn std::error::Error = &error;
}

fn assert_invalid_absolute_url(url: &str) {
    let error = RequestBuilder::new(Method::GET, url)
        .build()
        .expect_err("relative URL must be rejected");

    assert_eq!(error.kind(), ErrorKind::InvalidUrl);
    assert!(error.to_string().contains(url));
}

#[test]
fn builder_rejects_a_relative_path() {
    assert_invalid_absolute_url("/relative");
}

#[test]
fn builder_rejects_a_bare_host() {
    assert_invalid_absolute_url("example.test");
}

#[test]
fn builder_rejects_a_scheme_relative_url() {
    assert_invalid_absolute_url("//example.test/path");
}

#[test]
fn bytes_bodies_accept_shared_and_owned_bytes() {
    let shared = Bytes::from_static(b"shared");
    let shared_request = RequestBuilder::new(Method::POST, "https://example.test/shared")
        .body(shared.clone())
        .build()
        .expect("valid shared body");
    match shared_request.body() {
        BodySource::Bytes(body) => assert_eq!(body, &shared),
        BodySource::Empty => panic!("expected bytes body"),
        _ => panic!("expected bytes body"),
    }

    let owned_request = RequestBuilder::new(Method::PUT, "https://example.test/owned")
        .body(Vec::from(&b"owned"[..]))
        .build()
        .expect("valid owned body");
    match owned_request.body() {
        BodySource::Bytes(body) => assert_eq!(body.as_ref(), b"owned"),
        BodySource::Empty => panic!("expected bytes body"),
        _ => panic!("expected bytes body"),
    }
}

#[test]
fn public_stream_body_survives_request_builder_type_erasure() {
    let request = RequestBuilder::new(Method::POST, "https://example.test/stream")
        .body(BodySource::Stream(Box::pin(PublicStream {
            chunks: VecDeque::from([Bytes::from_static(b"stream")]),
            size_hint: Some(6),
        })))
        .build()
        .expect("valid stream request");

    let BodySource::Stream(stream) = request.body() else {
        panic!("expected stream body");
    };
    assert_eq!(stream.size_hint(), Some(6));
}

#[test]
fn error_kind_requires_forward_compatible_matching() {
    let kind = RequestBuilder::new(Method::GET, "https://bad host")
        .build()
        .expect_err("invalid URL")
        .kind();

    let label = match kind {
        ErrorKind::InvalidUrl => "invalid-url",
        _ => "future-kind",
    };
    assert_eq!(label, "invalid-url");
}

#[test]
fn approved_http_types_are_reexported() {
    let _: HeaderMap = HeaderMap::new();
    let _: HeaderName = HeaderName::from_static("x-test");
    let _: HeaderValue = HeaderValue::from_static("value");
    let _: Method = Method::GET;
    let _: StatusCode = StatusCode::OK;
    let _: Uri = Uri::from_static("https://example.test/");
    let _: Version = Version::HTTP_11;
}

#[test]
fn core_manifest_has_no_python_dependency() {
    let manifest = include_str!("../Cargo.toml").to_ascii_lowercase();

    assert!(!manifest.contains("pyo3"));
    assert!(!manifest.contains("python"));
}

fn assert_method(builder: RequestBuilder, expected: Method) {
    let request = builder.build().expect("build convenience request");
    assert_eq!(request.method(), expected);
}

fn assert_client_url_generics<U>(client: &Client, url: U)
where
    U: AsRef<str> + Clone,
{
    assert_method(client.get(url.clone()), Method::GET);
    assert_method(client.head(url.clone()), Method::HEAD);
    assert_method(client.post(url.clone()), Method::POST);
    assert_method(client.put(url.clone()), Method::PUT);
    assert_method(client.patch(url.clone()), Method::PATCH);
    assert_method(client.delete(url), Method::DELETE);
}

#[test]
fn client_convenience_builders_set_their_http_methods() {
    let client = Client::new().expect("build native client");
    assert_client_url_generics(&client, "http://example.test/native-method".to_owned());
}

#[test]
fn native_configuration_types_and_repeated_setters_build_without_tls_io() {
    let nonexistent = PathBuf::from("step12-intentionally-nonexistent.pem");
    let all_proxy_shapes = [
        Proxy::Http(Uri::from_static("http://proxy.example.test:8080")),
        Proxy::Https(Uri::from_static("https://proxy.example.test:8443")),
        Proxy::Socks4(Uri::from_static("socks4://proxy.example.test:1080")),
        Proxy::Socks5 {
            uri: Uri::from_static("socks5://proxy.example.test:1080"),
            remote_dns: true,
        },
        Proxy::Socks5 {
            uri: Uri::from_static("socks5://proxy.example.test:1081"),
            remote_dns: false,
        },
    ];
    for proxy in all_proxy_shapes {
        Client::builder()
            .proxy(proxy)
            .build()
            .expect("valid proxy shape");
    }

    let deferred_tls_shapes = [
        TlsConfig {
            roots: CertificateSource::Platform,
            identity: None,
        },
        TlsConfig {
            roots: CertificateSource::PemBundle(nonexistent.clone()),
            identity: Some(Identity {
                certificate_chain: nonexistent.clone(),
                private_key: Some(nonexistent.clone()),
            }),
        },
        TlsConfig {
            roots: CertificateSource::PemDirectory(PathBuf::from("step12-missing-ca-directory")),
            identity: Some(Identity {
                certificate_chain: nonexistent.clone(),
                private_key: None,
            }),
        },
        TlsConfig {
            roots: CertificateSource::Disabled,
            identity: None,
        },
    ];
    for tls in deferred_tls_shapes {
        Client::builder()
            .tls(tls)
            .build()
            .expect("TLS paths are retained without eager filesystem access");
    }

    let builder: ClientBuilder = Client::builder();
    let client = builder
        .proxy(Proxy::Http(Uri::from_static(
            "https://replaced-invalid.example.test",
        )))
        .proxy(Proxy::Https(Uri::from_static(
            "https://proxy.example.test:8443",
        )))
        .tls(TlsConfig {
            roots: CertificateSource::Platform,
            identity: None,
        })
        .tls(TlsConfig {
            roots: CertificateSource::PemBundle(nonexistent.clone()),
            identity: Some(Identity {
                certificate_chain: nonexistent.clone(),
                private_key: Some(nonexistent),
            }),
        })
        .timeout(Timeout {
            connect: Some(Duration::from_secs(1)),
            read: Some(Duration::from_secs(2)),
            total: Some(Duration::from_secs(3)),
        })
        .timeout(Timeout::default())
        .pool_max_idle_per_host(7)
        .pool_max_idle_per_host(0)
        .build()
        .expect("last valid settings build without filesystem or network I/O");

    drop(client);
}

#[test]
fn proxy_validation_rejects_relative_and_variant_mismatched_uris_at_build() {
    let invalid = [
        Proxy::Http(Uri::from_static("/relative")),
        Proxy::Http(Uri::from_static("https://proxy.example.test")),
        Proxy::Https(Uri::from_static("http://proxy.example.test")),
        Proxy::Socks4(Uri::from_static("socks5://proxy.example.test")),
        Proxy::Socks5 {
            uri: Uri::from_static("socks4://proxy.example.test"),
            remote_dns: false,
        },
    ];

    for proxy in invalid {
        let error = Client::builder()
            .proxy(proxy)
            .build()
            .expect_err("invalid proxy URI must fail at build");
        assert_eq!(error.kind(), ErrorKind::Proxy);
    }
}

fn assert_response_future(
    future: impl Future<Output = requests_native::Result<requests_native::Response>>,
) {
    drop(future);
}

fn assert_string_future(future: impl Future<Output = requests_native::Result<String>>) {
    drop(future);
}

fn assert_response_surface(response: requests_native::Response) {
    let _: Version = response.version();
    let _: Option<u64> = response.content_length();
    assert_string_future(response.text());
}

fn assert_top_level_url_generics<U>(url: U)
where
    U: AsRef<str> + Clone,
{
    assert_response_future(requests_native::get(url.clone()));
    assert_response_future(requests_native::head(url.clone()));
    assert_response_future(requests_native::delete(url));
}

fn assert_top_level_post_generics<U, B>(url: U, body: B)
where
    U: AsRef<str>,
    B: Into<BodySource>,
{
    assert_response_future(requests_native::post(url, body));
}

fn assert_top_level_put_generics<U, B>(url: U, body: B)
where
    U: AsRef<str>,
    B: Into<BodySource>,
{
    assert_response_future(requests_native::put(url, body));
}

fn assert_top_level_patch_generics<U, B>(url: U, body: B)
where
    U: AsRef<str>,
    B: Into<BodySource>,
{
    assert_response_future(requests_native::patch(url, body));
}

#[test]
fn execute_and_top_level_async_helpers_have_the_approved_signatures() {
    let client = Client::new().expect("build native client");
    let request = RequestBuilder::new(Method::GET, "http://example.test/execute")
        .build()
        .expect("build standalone request");
    assert_response_future(client.execute(request));

    let _response_surface: fn(requests_native::Response) = assert_response_surface;

    assert_top_level_url_generics("http://example.test/generic".to_owned());
    assert_top_level_post_generics("http://example.test/post", Vec::from(&b"post"[..]));
    assert_top_level_put_generics(
        "http://example.test/put",
        BodySource::Bytes(Bytes::from_static(b"put")),
    );
    assert_top_level_patch_generics("http://example.test/patch", Vec::from(&b"patch"[..]));
}

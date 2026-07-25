use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use requests::{
    AsyncBody, BodySource, ErrorKind, HeaderMap, HeaderName, HeaderValue, Method, RequestBuilder,
    StatusCode, Uri, Version,
};

struct PublicStream {
    chunks: VecDeque<Bytes>,
    size_hint: Option<u64>,
}

impl AsyncBody for PublicStream {
    fn poll_next(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<requests::Result<Bytes>>> {
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

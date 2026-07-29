use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    Body,
    Blocking,
    Builder,
    ChunkedEncoding,
    Connect,
    ConnectTimeout,
    Connection,
    ContentDecoding,
    Dns,
    Handshake,
    InvalidHeader,
    InvalidUrl,
    MissingSchema,
    Proxy,
    ReadTimeout,
    ResponseBody,
    Retry,
    Send,
    Tls,
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    raw_os_error: Option<i32>,
    incomplete_body: Option<(u64, u64)>,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[doc(hidden)]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }

    #[doc(hidden)]
    pub fn incomplete_body(&self) -> Option<(u64, u64)> {
        self.incomplete_body
    }

    #[doc(hidden)]
    pub fn from_binding_parts(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::plain(kind, message.into())
    }

    #[doc(hidden)]
    pub fn body_stream() -> Self {
        Self::plain(ErrorKind::Body, "request body stream failed".to_owned())
    }

    pub(crate) fn invalid_url(url: &str) -> Self {
        Self::plain(ErrorKind::InvalidUrl, format!("invalid URL: {url}"))
    }

    #[cfg(feature = "blocking")]
    pub(crate) fn blocking(error: impl fmt::Display) -> Self {
        Self::plain(
            ErrorKind::Blocking,
            format!("blocking runtime failed: {error}"),
        )
    }

    pub(crate) fn unsupported_scheme(url: &str, scheme: Option<&str>) -> Self {
        let scheme = scheme.unwrap_or("<missing>");
        Self::plain(
            ErrorKind::InvalidUrl,
            format!("unsupported URL scheme {scheme:?} for direct HTTP request: {url}"),
        )
    }

    pub(crate) fn unbound_builder() -> Self {
        Self::plain(
            ErrorKind::Builder,
            "request builder is not bound to a Client".to_owned(),
        )
    }

    pub(crate) fn conflicting_content_length() -> Self {
        Self::plain(
            ErrorKind::Builder,
            "conflicting Content-Length headers".to_owned(),
        )
    }

    pub(crate) fn invalid_proxy(message: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Proxy,
            format!("invalid proxy configuration: {message}"),
        )
    }

    pub(crate) fn proxy(error: impl fmt::Display) -> Self {
        Self::transport(ErrorKind::Proxy, format!("proxy transport failed: {error}"))
    }

    #[cfg(test)]
    pub(crate) fn dns(target: &str, error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Dns,
            format!("DNS resolution failed for {target}: {error}"),
        )
    }

    pub(crate) fn dns_io(target: &str, error: std::io::Error) -> Self {
        Self::transport_io(
            ErrorKind::Dns,
            format!("DNS resolution failed for {target}: {error}"),
            &error,
        )
    }

    pub(crate) fn no_addresses(target: &str) -> Self {
        Self::transport(
            ErrorKind::Dns,
            format!("DNS resolution returned no addresses for {target}"),
        )
    }

    pub(crate) fn connect(target: &str, error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Connect,
            format!("TCP connection failed for {target}: {error}"),
        )
    }

    pub(crate) fn connect_io(target: &str, error: std::io::Error) -> Self {
        Self::transport_io(
            ErrorKind::Connect,
            format!("TCP connection failed for {target}: {error}"),
            &error,
        )
    }

    pub(crate) fn connect_timeout(target: &str, timeout: std::time::Duration, total: bool) -> Self {
        let source = if total {
            "total timeout"
        } else {
            "connect timeout"
        };
        Self::transport(
            ErrorKind::ConnectTimeout,
            format!("connection establishment {source} after {timeout:?} for {target}"),
        )
    }

    pub(crate) fn tls(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Tls,
            format!("TLS configuration or handshake failed: {error}"),
        )
    }

    pub(crate) fn handshake(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Handshake,
            format!("HTTP/1.1 client handshake failed: {error}"),
        )
    }

    #[cfg(test)]
    pub(crate) fn send(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Send,
            format!("HTTP/1.1 request send failed: {error}"),
        )
    }

    pub(crate) fn send_hyper(error: hyper::Error) -> Self {
        Self::transport_source(
            ErrorKind::Send,
            format!("HTTP/1.1 request send failed: {error}"),
            &error,
        )
    }

    #[cfg(test)]
    pub(crate) fn send_with_cleanup(primary: Self, cleanup: Self) -> Self {
        Self::with_cleanup(primary, cleanup)
    }

    pub(crate) fn with_cleanup(primary: Self, cleanup: Self) -> Self {
        let kind = primary.kind;
        let message = format!("{primary}; connection cleanup also failed: {cleanup}");
        Self {
            kind,
            message,
            raw_os_error: primary.raw_os_error,
            incomplete_body: primary.incomplete_body,
        }
    }

    pub(crate) fn connection(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Connection,
            format!("HTTP/1.1 connection driver failed: {error}"),
        )
    }

    pub(crate) fn connection_hyper(error: hyper::Error) -> Self {
        Self::transport_source(
            ErrorKind::Connection,
            format!("HTTP/1.1 connection driver failed: {error}"),
            &error,
        )
    }

    pub(crate) fn response_body(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::ResponseBody,
            format!("HTTP/1.1 response body failed: {error}"),
        )
    }

    pub(crate) fn response_body_hyper(
        error: hyper::Error,
        incomplete_body: Option<(u64, u64)>,
    ) -> Self {
        let mut mapped = Self::transport_source(
            ErrorKind::ResponseBody,
            format!("HTTP/1.1 response body failed: {error}"),
            &error,
        );
        mapped.incomplete_body = incomplete_body;
        mapped
    }

    pub(crate) fn chunked_encoding(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::ChunkedEncoding,
            format!("HTTP/1.1 chunked response body failed: {error}"),
        )
    }

    pub(crate) fn content_decoding() -> Self {
        Self::transport(
            ErrorKind::ContentDecoding,
            "response content decoding failed".to_owned(),
        )
    }

    pub(crate) fn read_timeout(timeout: std::time::Duration) -> Self {
        Self::transport(
            ErrorKind::ReadTimeout,
            format!("HTTP/1.1 response body read timed out after {timeout:?}"),
        )
    }

    pub(crate) fn response_body_total_timeout(timeout: std::time::Duration) -> Self {
        Self::transport(
            ErrorKind::ReadTimeout,
            format!("HTTP/1.1 response body total timeout after {timeout:?}"),
        )
    }

    pub(crate) fn response_head_timeout(timeout: std::time::Duration, total: bool) -> Self {
        let source = if total {
            "total timeout"
        } else {
            "read timeout"
        };
        Self::transport(
            ErrorKind::ReadTimeout,
            format!("HTTP/1.1 response head {source} after {timeout:?}"),
        )
    }

    pub(crate) fn request_exchange_total_timeout(timeout: std::time::Duration) -> Self {
        Self::transport(
            ErrorKind::ReadTimeout,
            format!("HTTP/1.1 request exchange total timeout after {timeout:?}"),
        )
    }

    fn transport(kind: ErrorKind, message: String) -> Self {
        Self::plain(kind, message)
    }

    fn transport_io(kind: ErrorKind, message: String, source: &std::io::Error) -> Self {
        Self {
            kind,
            message,
            raw_os_error: source.raw_os_error(),
            incomplete_body: None,
        }
    }

    fn transport_source(
        kind: ErrorKind,
        message: String,
        source: &(dyn std::error::Error + 'static),
    ) -> Self {
        let mut current = Some(source);
        while let Some(error) = current {
            if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
                return Self::transport_io(kind, message, io_error);
            }
            current = error.source();
        }
        Self::plain(kind, message)
    }

    fn plain(kind: ErrorKind, message: String) -> Self {
        Self {
            kind,
            message,
            raw_os_error: None,
            incomplete_body: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};

    fn assert_send_static<T: Send + 'static>() {}

    #[test]
    fn body_stream_error_is_fixed_and_python_independent() {
        let error = Error::body_stream();

        assert_eq!(error.kind(), ErrorKind::Body);
        assert_eq!(error.to_string(), "request body stream failed");
        assert_send_static::<Error>();
    }

    #[test]
    fn binding_parts_keep_python_free_kind_and_message() {
        let cases = [
            (ErrorKind::InvalidHeader, "invalid header"),
            (ErrorKind::MissingSchema, "missing schema"),
            (ErrorKind::Retry, "retry exhausted"),
        ];

        for (kind, message) in cases {
            let error = Error::from_binding_parts(kind, message);
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), message);
        }
        assert_send_static::<Error>();
    }

    #[test]
    fn transport_failures_keep_specific_error_kinds() {
        let cases = [
            (Error::dns("example.test:80", "resolver"), ErrorKind::Dns),
            (
                Error::connect("127.0.0.1:80", "refused"),
                ErrorKind::Connect,
            ),
            (Error::handshake("protocol"), ErrorKind::Handshake),
            (Error::send("closed"), ErrorKind::Send),
            (Error::connection("driver"), ErrorKind::Connection),
            (Error::response_body("short body"), ErrorKind::ResponseBody),
        ];

        for (error, kind) in cases {
            assert_eq!(error.kind(), kind);
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn send_cleanup_failure_preserves_primary_kind_and_both_contexts() {
        let error = Error::send_with_cleanup(
            Error::send("primary send failure"),
            Error::connection("secondary driver failure"),
        );

        assert_eq!(error.kind(), ErrorKind::Send);
        assert!(
            error
                .to_string()
                .contains("HTTP/1.1 request send failed: primary send failure")
        );
        assert!(
            error
                .to_string()
                .contains("HTTP/1.1 connection driver failed: secondary driver failure")
        );
    }
}

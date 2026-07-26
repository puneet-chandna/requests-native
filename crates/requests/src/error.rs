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
    InvalidUrl,
    Proxy,
    ReadTimeout,
    ResponseBody,
    Send,
    Tls,
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[doc(hidden)]
    pub fn body_stream() -> Self {
        Self {
            kind: ErrorKind::Body,
            message: "request body stream failed".to_owned(),
        }
    }

    pub(crate) fn invalid_url(url: &str) -> Self {
        Self {
            kind: ErrorKind::InvalidUrl,
            message: format!("invalid URL: {url}"),
        }
    }

    #[cfg(feature = "blocking")]
    pub(crate) fn blocking(error: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::Blocking,
            message: format!("blocking runtime failed: {error}"),
        }
    }

    pub(crate) fn unsupported_scheme(url: &str, scheme: Option<&str>) -> Self {
        let scheme = scheme.unwrap_or("<missing>");
        Self {
            kind: ErrorKind::InvalidUrl,
            message: format!("unsupported URL scheme {scheme:?} for direct HTTP request: {url}"),
        }
    }

    pub(crate) fn unbound_builder() -> Self {
        Self {
            kind: ErrorKind::Builder,
            message: "request builder is not bound to a Client".to_owned(),
        }
    }

    pub(crate) fn conflicting_content_length() -> Self {
        Self {
            kind: ErrorKind::Builder,
            message: "conflicting Content-Length headers".to_owned(),
        }
    }

    pub(crate) fn invalid_proxy(message: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Proxy,
            format!("invalid proxy configuration: {message}"),
        )
    }

    pub(crate) fn proxy_not_implemented() -> Self {
        Self::transport(
            ErrorKind::Proxy,
            "configured proxy transport is not implemented".to_owned(),
        )
    }

    pub(crate) fn dns(target: &str, error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Dns,
            format!("DNS resolution failed for {target}: {error}"),
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

    pub(crate) fn send(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Send,
            format!("HTTP/1.1 request send failed: {error}"),
        )
    }

    #[cfg(test)]
    pub(crate) fn send_with_cleanup(primary: Self, cleanup: Self) -> Self {
        Self::with_cleanup(primary, cleanup)
    }

    pub(crate) fn with_cleanup(primary: Self, cleanup: Self) -> Self {
        let kind = primary.kind;
        Self::transport(
            kind,
            format!("{primary}; connection cleanup also failed: {cleanup}"),
        )
    }

    pub(crate) fn connection(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Connection,
            format!("HTTP/1.1 connection driver failed: {error}"),
        )
    }

    pub(crate) fn response_body(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::ResponseBody,
            format!("HTTP/1.1 response body failed: {error}"),
        )
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
        Self { kind, message }
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

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    Body,
    Builder,
    Connect,
    Connection,
    Dns,
    Handshake,
    InvalidUrl,
    ResponseBody,
    Send,
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

    pub(crate) fn send_with_cleanup(primary: Self, cleanup: Self) -> Self {
        Self::transport(
            ErrorKind::Send,
            format!("{primary}; connection cleanup also failed: {cleanup}"),
        )
    }

    pub(crate) fn connection(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::Connection,
            format!("HTTP/1.1 connection driver failed: {error}"),
        )
    }

    pub(crate) fn connection_stopped() -> Self {
        Self::transport(
            ErrorKind::Connection,
            "HTTP/1.1 connection driver stopped without a result".to_owned(),
        )
    }

    pub(crate) fn response_body(error: impl fmt::Display) -> Self {
        Self::transport(
            ErrorKind::ResponseBody,
            format!("HTTP/1.1 response body failed: {error}"),
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

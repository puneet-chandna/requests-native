#![forbid(unsafe_code)]

#[cfg(feature = "blocking")]
#[doc(hidden)]
pub mod adapters;
#[cfg(feature = "blocking")]
pub mod blocking;
mod body;
mod client;
mod error;
mod models;
mod response;
#[doc(hidden)]
pub mod retry;
pub mod structures;
mod transport;
pub mod utils;

pub use body::{AsyncBody, BodySource};
pub use client::{
    CertificateSource, Client, ClientBuilder, ContentCodecs, Identity, Proxy, TlsConfig,
};
pub use error::{Error, ErrorKind, Result};
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version};
pub use models::{
    HeaderInput, HeaderPart, HeaderPreparationError, InvalidHeaderPart, PreparedHeader, Request,
    RequestBuilder, Timeout, UrlPreparationError, append_url_params, is_non_http_url,
    prepare_headers, prepare_method, prepare_method_bytes, prepare_url, url_is_native_safe,
};
pub use response::{
    Response, ResponseBody, ResponseCache, ResponseContent, ResponseDecision, ResponseDisposition,
    ResponseDispositionState, ResponseEvent,
};

pub async fn get(url: impl AsRef<str>) -> Result<Response> {
    Client::new()?.get(url).send().await
}

pub async fn head(url: impl AsRef<str>) -> Result<Response> {
    Client::new()?.head(url).send().await
}

pub async fn post(url: impl AsRef<str>, body: impl Into<BodySource>) -> Result<Response> {
    Client::new()?.post(url).body(body).send().await
}

pub async fn put(url: impl AsRef<str>, body: impl Into<BodySource>) -> Result<Response> {
    Client::new()?.put(url).body(body).send().await
}

pub async fn patch(url: impl AsRef<str>, body: impl Into<BodySource>) -> Result<Response> {
    Client::new()?.patch(url).body(body).send().await
}

pub async fn delete(url: impl AsRef<str>) -> Result<Response> {
    Client::new()?.delete(url).send().await
}

#[cfg(feature = "platform-smoke")]
#[allow(dead_code)] // These probes are compiled by the matrix before transport implementation.
mod platform_smoke {
    use std::io;

    use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
    use hyper_util::client::legacy::connect::HttpConnector;
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

    pub(crate) fn connector() -> io::Result<HttpsConnector<HttpConnector>> {
        Ok(HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::ring::default_provider())?
            .https_or_http()
            .enable_http1()
            .build())
    }

    pub(crate) async fn http_connect_signature<S>(stream: &mut S) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        stream.flush().await
    }

    pub(crate) async fn socks4_signature<S>(stream: &mut S) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        stream.flush().await
    }

    pub(crate) async fn socks5_signature<S>(stream: &mut S) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        stream.flush().await
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "platform-smoke")]
    #[test]
    fn platform_connector_accepts_http_and_https_schemes() {
        let _connector_factory = super::platform_smoke::connector;
    }

    #[cfg(feature = "platform-smoke")]
    #[test]
    fn proxy_handshake_signatures_accept_tokio_io() {
        let _http_connect =
            super::platform_smoke::http_connect_signature::<tokio::io::DuplexStream>;
        let _socks4 = super::platform_smoke::socks4_signature::<tokio::io::DuplexStream>;
        let _socks5 = super::platform_smoke::socks5_signature::<tokio::io::DuplexStream>;
    }
}

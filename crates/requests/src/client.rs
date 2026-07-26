use std::path::PathBuf;
use std::sync::Arc;

use http::{Method, Uri};

use crate::transport::{DEFAULT_MAX_IDLE_PER_HOST, Transport};
use crate::{Error, Request, RequestBuilder, Response, Result, Timeout};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentCodecs {
    brotli: bool,
    zstandard: bool,
}

impl ContentCodecs {
    pub fn new(brotli: bool, zstandard: bool) -> Self {
        Self { brotli, zstandard }
    }

    pub(crate) const fn accept_encoding(self) -> &'static str {
        match (self.brotli, self.zstandard) {
            (true, true) => "gzip, deflate, br, zstd",
            (false, true) => "gzip, deflate, zstd",
            (true, false) => "gzip, deflate, br",
            (false, false) => "gzip, deflate",
        }
    }

    pub(crate) fn decodes(self, encoding: &str) -> bool {
        encoding.eq_ignore_ascii_case("gzip")
            || encoding.eq_ignore_ascii_case("x-gzip")
            || encoding.eq_ignore_ascii_case("deflate")
            || (self.brotli && encoding.eq_ignore_ascii_case("br"))
            || (self.zstandard && encoding.eq_ignore_ascii_case("zstd"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Proxy {
    Http(Uri),
    Https(Uri),
    Socks4(Uri),
    Socks5 { uri: Uri, remote_dns: bool },
}

impl Proxy {
    pub(crate) fn uri(&self) -> &Uri {
        match self {
            Self::Http(uri) | Self::Https(uri) | Self::Socks4(uri) => uri,
            Self::Socks5 { uri, .. } => uri,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CertificateSource {
    #[default]
    Platform,
    PemBundle(PathBuf),
    PemDirectory(PathBuf),
    Disabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub certificate_chain: PathBuf,
    pub private_key: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TlsConfig {
    pub roots: CertificateSource,
    pub identity: Option<Identity>,
}

#[derive(Clone, Debug)]
pub struct ClientBuilder {
    proxy: Option<Proxy>,
    tls: TlsConfig,
    timeout: Timeout,
    pool_max_idle_per_host: usize,
    content_codecs: ContentCodecs,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            proxy: None,
            tls: TlsConfig::default(),
            timeout: Timeout::default(),
            pool_max_idle_per_host: DEFAULT_MAX_IDLE_PER_HOST,
            content_codecs: ContentCodecs::new(true, true),
        }
    }
}

impl ClientBuilder {
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxy = Some(proxy);
        self
    }

    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = tls;
        self
    }

    pub fn timeout(mut self, timeout: Timeout) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn pool_max_idle_per_host(mut self, maximum: usize) -> Self {
        self.pool_max_idle_per_host = maximum;
        self
    }

    pub fn content_codecs(mut self, content_codecs: ContentCodecs) -> Self {
        self.content_codecs = content_codecs;
        self
    }

    pub fn build(self) -> Result<Client> {
        if let Some(proxy) = &self.proxy {
            validate_proxy(proxy)?;
        }
        Ok(Client {
            transport: Arc::new(Transport::configured(
                self.proxy,
                self.tls,
                self.timeout,
                self.pool_max_idle_per_host,
                self.content_codecs,
            )),
        })
    }
}

fn validate_proxy(proxy: &Proxy) -> Result<()> {
    let uri = proxy.uri();
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| Error::invalid_proxy("URI must be absolute and include a scheme"))?;
    let valid_scheme = match proxy {
        Proxy::Http(_) => scheme.eq_ignore_ascii_case("http"),
        Proxy::Https(_) => scheme.eq_ignore_ascii_case("https"),
        Proxy::Socks4(_) => matches!(scheme, "socks4" | "socks4a"),
        Proxy::Socks5 { remote_dns, .. } => {
            scheme.eq_ignore_ascii_case("socks5")
                || (*remote_dns && scheme.eq_ignore_ascii_case("socks5h"))
        }
    };
    if !valid_scheme {
        return Err(Error::invalid_proxy(format!(
            "proxy variant does not accept URI scheme {scheme:?}"
        )));
    }
    if uri.authority().is_none() || uri.host().is_none_or(str::is_empty) {
        return Err(Error::invalid_proxy(
            "URI must include an authority with a nonempty host",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Client {
    transport: Arc<Transport>,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn head(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn put(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn patch(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    pub fn delete(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    pub fn request(&self, method: Method, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder::for_client(method, url, Arc::clone(&self.transport))
    }

    pub async fn execute(&self, request: Request) -> Result<Response> {
        execute_request(&self.transport, request).await
    }
}

pub(crate) async fn execute_request(transport: &Transport, request: Request) -> Result<Response> {
    transport.send(request).await.map(Response::from_transport)
}

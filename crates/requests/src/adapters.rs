use crate::blocking;
use crate::{BodySource, HeaderMap, Method, Proxy, Result, Timeout, TlsConfig};

/// One configuration-specific connection pool owned by a Python adapter shadow.
///
/// The Python binding owns the weak identity table. Keeping this type Python-free
/// makes pool generation and request execution independently testable.
#[derive(Clone)]
pub struct AdapterPool {
    client: blocking::Client,
}

impl AdapterPool {
    pub fn new(
        maximum_idle_per_host: usize,
        proxy: Option<Proxy>,
        tls: TlsConfig,
        timeout: Timeout,
    ) -> Result<Self> {
        let mut builder = blocking::Client::builder()
            .pool_max_idle_per_host(maximum_idle_per_host)
            .tls(tls)
            .timeout(timeout);
        if let Some(proxy) = proxy {
            builder = builder.proxy(proxy);
        }
        Ok(Self {
            client: builder.build()?,
        })
    }

    pub fn send(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: BodySource,
        timeout: Timeout,
    ) -> Result<blocking::Response> {
        self.client
            .request(method, url)
            .headers(headers)
            .body(body)
            .timeout(timeout)
            .send()
    }

    pub fn clear(&self) {
        self.client.clear_pool();
    }
}

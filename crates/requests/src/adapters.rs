use std::io::{self, Read};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::blocking;
use crate::{BodySource, HeaderMap, Method, Proxy, Result, Timeout, TlsConfig};

/// One configuration-specific connection pool owned by a Python adapter shadow.
///
/// The Python binding owns the weak identity table. Keeping this type Python-free
/// makes pool generation and request execution independently testable.
#[derive(Clone)]
pub struct AdapterPool {
    client: blocking::Client,
    capacity: Arc<PoolCapacity>,
}

struct PoolCapacity {
    semaphore: Option<Arc<Semaphore>>,
}

struct PoolPermit {
    _permit: Option<OwnedSemaphorePermit>,
}

pub struct AdapterResponse {
    inner: blocking::Response,
    permit: PoolPermit,
}

pub struct AdapterResponseBody {
    inner: blocking::ResponseBody,
    permit: Option<PoolPermit>,
}

impl AdapterPool {
    pub fn new(
        maximum_idle_per_host: usize,
        block: bool,
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
            capacity: Arc::new(PoolCapacity {
                semaphore: (block && maximum_idle_per_host > 0)
                    .then(|| Arc::new(Semaphore::new(maximum_idle_per_host))),
            }),
        })
    }

    pub async fn send_async(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        header_names: Vec<String>,
        body: BodySource,
        timeout: Timeout,
    ) -> Result<AdapterResponse> {
        let permit = self.capacity.acquire().await;
        let inner = self
            .client
            .request(method, url)
            .headers(headers)
            .python_header_names(header_names)
            .body(body)
            .timeout(timeout)
            .send_async()
            .await?;
        Ok(AdapterResponse { inner, permit })
    }

    pub fn clear(&self) {
        self.client.clear_pool();
    }
}

impl PoolCapacity {
    async fn acquire(self: &Arc<Self>) -> PoolPermit {
        let permit = match &self.semaphore {
            Some(semaphore) => Some(
                Arc::clone(semaphore)
                    .acquire_owned()
                    .await
                    .expect("adapter capacity semaphore is never closed"),
            ),
            None => None,
        };
        PoolPermit { _permit: permit }
    }
}

impl AdapterResponse {
    pub fn status(&self) -> http::StatusCode {
        self.inner.status()
    }

    pub fn reason(&self) -> &str {
        self.inner.reason()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    #[doc(hidden)]
    pub fn raw_headers(&self) -> &[(String, Vec<u8>)] {
        self.inner.raw_headers()
    }

    pub fn into_raw_body(self) -> AdapterResponseBody {
        AdapterResponseBody {
            inner: self.inner.into_raw_body(),
            permit: Some(self.permit),
        }
    }
}

impl Read for AdapterResponseBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 && !buffer.is_empty() {
            self.permit = None;
        }
        Ok(read)
    }
}

impl AdapterResponseBody {
    pub async fn read_async(
        self,
        amount: Option<usize>,
        allow_encoded_completion: bool,
    ) -> io::Result<(Self, Vec<u8>)> {
        let Self { inner, mut permit } = self;
        let (inner, bytes) = inner.read_async(amount, allow_encoded_completion).await?;
        if inner.is_terminal() {
            permit = None;
        }
        Ok((Self { inner, permit }, bytes))
    }

    pub async fn read_frame_async(
        self,
        maximum: usize,
        allow_encoded_completion: bool,
    ) -> io::Result<(Self, Vec<u8>)> {
        let Self { inner, mut permit } = self;
        let (inner, bytes) = inner
            .read_frame_async(maximum, allow_encoded_completion)
            .await?;
        if inner.is_terminal() {
            permit = None;
        }
        Ok((Self { inner, permit }, bytes))
    }

    pub fn finish_encoded_declared_length(&mut self) {
        self.inner.finish_encoded_declared_length();
        if self.inner.is_terminal() {
            self.permit = None;
        }
    }

    pub async fn close_async(self) -> Result<()> {
        let Self { inner, permit } = self;
        let result = inner.close_async().await;
        drop(permit);
        result
    }

    pub fn close(mut self) -> Result<()> {
        let result = self.inner.close();
        self.permit = None;
        result
    }
}

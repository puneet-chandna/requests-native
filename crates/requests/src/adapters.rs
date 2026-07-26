use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex};

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
    maximum: usize,
    block: bool,
    in_use: Mutex<usize>,
    available: Condvar,
}

struct PoolPermit {
    capacity: Arc<PoolCapacity>,
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
                maximum: maximum_idle_per_host,
                block,
                in_use: Mutex::new(0),
                available: Condvar::new(),
            }),
        })
    }

    pub fn send(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: BodySource,
        timeout: Timeout,
    ) -> Result<AdapterResponse> {
        let permit = self.capacity.acquire();
        let inner = self
            .client
            .request(method, url)
            .headers(headers)
            .body(body)
            .timeout(timeout)
            .send()?;
        Ok(AdapterResponse { inner, permit })
    }

    pub fn clear(&self) {
        self.client.clear_pool();
    }
}

impl PoolCapacity {
    fn acquire(self: &Arc<Self>) -> PoolPermit {
        let mut in_use = self.in_use.lock().expect("adapter capacity lock poisoned");
        while self.block && self.maximum > 0 && *in_use >= self.maximum {
            in_use = self
                .available
                .wait(in_use)
                .expect("adapter capacity lock poisoned");
        }
        *in_use += 1;
        PoolPermit {
            capacity: Arc::clone(self),
        }
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        let mut in_use = self
            .capacity
            .in_use
            .lock()
            .expect("adapter capacity lock poisoned");
        *in_use = in_use.saturating_sub(1);
        self.capacity.available.notify_one();
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
    pub fn close(mut self) -> Result<()> {
        let result = self.inner.close();
        self.permit = None;
        result
    }
}

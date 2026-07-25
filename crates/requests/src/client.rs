use std::sync::Arc;

use http::Method;

use crate::transport::Transport;
use crate::{RequestBuilder, Result};

#[derive(Clone, Debug)]
pub struct Client {
    transport: Arc<Transport>,
}

impl Client {
    pub fn new() -> Result<Self> {
        Ok(Self {
            transport: Arc::new(Transport::new()),
        })
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn request(&self, method: Method, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder::for_client(method, url, Arc::clone(&self.transport))
    }
}

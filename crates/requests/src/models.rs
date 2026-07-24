use crate::{BodySource, Error, Result};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};

#[derive(Debug)]
pub struct Request {
    method: Method,
    url: String,
    uri: Uri,
    headers: HeaderMap,
    body: BodySource,
}

impl Request {
    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &BodySource {
        &self.body
    }
}

#[derive(Debug)]
pub struct RequestBuilder {
    request: Result<Request>,
}

impl RequestBuilder {
    pub fn new(method: Method, url: impl AsRef<str>) -> Self {
        let url = url.as_ref().to_owned();
        let request = match url.parse::<Uri>() {
            Ok(uri) if uri.scheme().is_some() && uri.authority().is_some() => Ok(Request {
                method,
                url,
                uri,
                headers: HeaderMap::new(),
                body: BodySource::Empty,
            }),
            _ => Err(Error::invalid_url(&url)),
        };

        Self { request }
    }

    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        if let Ok(request) = &mut self.request {
            request.headers.append(name, value);
        }
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        if let Ok(request) = &mut self.request {
            request.headers = headers;
        }
        self
    }

    pub fn body(mut self, body: impl Into<BodySource>) -> Self {
        if let Ok(request) = &mut self.request {
            request.body = body.into();
        }
        self
    }

    pub fn build(self) -> Result<Request> {
        self.request
    }
}

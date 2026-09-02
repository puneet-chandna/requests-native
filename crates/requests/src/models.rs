use std::sync::Arc;
use std::time::Duration;

use crate::client::execute_request;
use crate::structures::CaseInsensitiveMap;
use crate::transport::Transport;
use crate::utils::{
    HeaderValidationError, normalize_percent_escape_hex, requote_uri, trim_python_whitespace_start,
    validate_header_name, validate_header_name_bytes, validate_header_value,
    validate_header_value_bytes,
};
use crate::{BodySource, Error, Response, Result};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use url::{ParseError, Url};

pub fn prepare_method(method: &str) -> String {
    method.to_uppercase()
}

pub fn prepare_method_bytes(method: &[u8]) -> Vec<u8> {
    method.iter().map(u8::to_ascii_uppercase).collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderPart {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderInput {
    pub name: HeaderPart,
    pub value: HeaderPart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedHeader {
    pub source_index: usize,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidHeaderPart {
    Name,
    Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderPreparationError {
    pub prepared: Vec<PreparedHeader>,
    pub source_index: usize,
    pub part: InvalidHeaderPart,
}

pub fn prepare_headers(
    headers: &[HeaderInput],
) -> std::result::Result<Vec<PreparedHeader>, HeaderPreparationError> {
    let mut prepared = CaseInsensitiveMap::new();
    for (source_index, header) in headers.iter().enumerate() {
        if validate_name(&header.name).is_err() {
            return Err(header_error(
                &prepared,
                source_index,
                InvalidHeaderPart::Name,
            ));
        }
        if validate_value(&header.value).is_err() {
            return Err(header_error(
                &prepared,
                source_index,
                InvalidHeaderPart::Value,
            ));
        }

        let name = match &header.name {
            HeaderPart::Text(name) => name.clone(),
            HeaderPart::Bytes(name) => String::from_utf8_lossy(name).into_owned(),
        };
        prepared.insert(name.to_ascii_lowercase(), name, source_index);
    }
    Ok(prepared_headers(&prepared))
}

fn validate_name(name: &HeaderPart) -> std::result::Result<(), HeaderValidationError> {
    match name {
        HeaderPart::Text(name) => validate_header_name(name),
        HeaderPart::Bytes(name) => validate_header_name_bytes(name),
    }
}

fn validate_value(value: &HeaderPart) -> std::result::Result<(), HeaderValidationError> {
    match value {
        HeaderPart::Text(value) => validate_header_value(value),
        HeaderPart::Bytes(value) => validate_header_value_bytes(value),
    }
}

fn header_error(
    prepared: &CaseInsensitiveMap<usize>,
    source_index: usize,
    part: InvalidHeaderPart,
) -> HeaderPreparationError {
    HeaderPreparationError {
        prepared: prepared_headers(prepared),
        source_index,
        part,
    }
}

fn prepared_headers(prepared: &CaseInsensitiveMap<usize>) -> Vec<PreparedHeader> {
    prepared
        .iter()
        .map(|(name, source_index)| PreparedHeader {
            source_index: *source_index,
            name: name.to_owned(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrlPreparationError {
    InvalidLabel,
    MissingHost,
    MissingScheme,
    Parse,
}

pub fn prepare_url(
    raw_url: &str,
    encoded_params: &str,
) -> std::result::Result<String, UrlPreparationError> {
    let raw_url = trim_python_whitespace_start(raw_url);
    if is_non_http_url(raw_url) {
        return Ok(raw_url.to_owned());
    }

    let parsed = match Url::parse(raw_url) {
        Ok(parsed) => parsed,
        Err(ParseError::RelativeUrlWithoutBase) => {
            return Err(UrlPreparationError::MissingScheme);
        }
        Err(ParseError::EmptyHost) => return Err(UrlPreparationError::MissingHost),
        Err(_) => return Err(UrlPreparationError::Parse),
    };

    let Some(host) = parsed.host_str() else {
        return Err(UrlPreparationError::MissingHost);
    };
    if host.starts_with(['*', '.']) {
        return Err(UrlPreparationError::InvalidLabel);
    }
    if raw_host_has_disallowed_unicode(raw_url) {
        return Err(UrlPreparationError::Parse);
    }

    let (_, remainder) = raw_url
        .split_once("://")
        .ok_or(UrlPreparationError::Parse)?;
    let raw_authority = remainder
        .split_once(['/', '?', '#'])
        .map_or(remainder, |(authority, _)| authority);
    let raw_suffix = &remainder[raw_authority.len()..];
    let raw_suffix = if raw_suffix.starts_with('/') {
        raw_suffix.to_owned()
    } else {
        format!("/{raw_suffix}")
    };
    let (authority, suffix) = if parsed.scheme() == "http+unix" {
        (raw_authority, raw_suffix)
    } else {
        (host, normalize_percent_escape_hex(&raw_suffix))
    };
    let prepared = format!("{}://{authority}{suffix}", parsed.scheme());
    Ok(append_url_params(&prepared, encoded_params))
}

pub fn is_non_http_url(raw_url: &str) -> bool {
    let raw_url = trim_python_whitespace_start(raw_url);
    raw_url.contains(':') && !starts_with_http(raw_url)
}

pub fn url_is_native_safe(raw_url: &str) -> bool {
    let raw_url = trim_python_whitespace_start(raw_url);
    if is_non_http_url(raw_url) {
        return true;
    }
    if !raw_url.is_ascii()
        || raw_url.contains(['\\', '\r', '\n', '\t'])
        || raw_url.ends_with(char::is_whitespace)
        || raw_url.ends_with(['?', '#'])
        || raw_url.contains("?#")
        || raw_url.contains(['[', ']', '\''])
        || raw_url.matches('#').count() > 1
    {
        return false;
    }

    let lower = raw_url.to_ascii_lowercase();
    if lower.contains("%2e")
        || lower.contains("/../")
        || lower.contains("/./")
        || lower.ends_with("/..")
        || lower.ends_with("/.")
        || has_incomplete_percent_escape(raw_url.as_bytes())
    {
        return false;
    }

    let Some((scheme, remainder)) = raw_url.split_once("://") else {
        return !starts_with_http(raw_url);
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "http+unix") {
        return false;
    }
    let authority = remainder
        .split_once(['/', '?', '#'])
        .map_or(remainder, |(authority, _)| authority);
    if authority.is_empty()
        || authority.contains('@')
        || authority.starts_with('[')
        || authority.chars().any(char::is_whitespace)
    {
        return false;
    }
    if scheme == "http+unix" {
        if !authority.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'%')
        }) {
            return false;
        }
        let suffix = &remainder[authority.len()..];
        if !suffix.starts_with('/') || suffix.len() == 1 {
            return false;
        }
    } else if authority.contains('%')
        || !authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'*'))
    {
        return false;
    }
    if authority.contains(':') {
        return false;
    }

    let path = remainder[authority.len()..]
        .split_once(['?', '#'])
        .map_or(&remainder[authority.len()..], |(path, _)| path);
    if path
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        return false;
    }

    let numeric_final_label = scheme != "http+unix"
        && authority
            .rsplit('.')
            .next()
            .is_some_and(|label| label.starts_with(|ch: char| ch.is_ascii_digit()));
    !numeric_final_label && authority.is_ascii()
}

fn has_incomplete_percent_escape(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if index + 2 >= value.len()
                || !value[index + 1].is_ascii_hexdigit()
                || !value[index + 2].is_ascii_hexdigit()
            {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

pub fn append_url_params(prepared_url: &str, encoded_params: &str) -> String {
    let mut prepared = prepared_url.to_owned();
    if !encoded_params.is_empty() {
        let fragment = prepared.find('#').unwrap_or(prepared.len());
        let separator = if prepared[..fragment].contains('?') {
            '&'
        } else {
            '?'
        };
        prepared.insert(fragment, separator);
        prepared.insert_str(fragment + 1, encoded_params);
    }
    requote_uri(&prepared)
}

fn starts_with_http(value: &str) -> bool {
    value
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http"))
}

fn raw_host_has_disallowed_unicode(raw_url: &str) -> bool {
    let Some((_, remainder)) = raw_url.split_once("://") else {
        return false;
    };
    let authority = remainder
        .split_once(['/', '?', '#'])
        .map_or(remainder, |(authority, _)| authority);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host_port.starts_with('[') {
        host_port
    } else {
        host_port
            .rsplit_once(':')
            .filter(|(_, port)| port.chars().all(|ch| ch.is_ascii_digit()))
            .map_or(host_port, |(host, _)| host)
    };

    host.chars()
        .any(|ch| !ch.is_ascii() && !ch.is_alphanumeric() && ch != '-')
}

#[derive(Debug)]
pub struct Request {
    method: Method,
    url: String,
    uri: Uri,
    headers: HeaderMap,
    python_header_names: Option<Vec<String>>,
    body: BodySource,
    timeout: Option<Timeout>,
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

    pub(crate) fn timeout(&self) -> Option<Timeout> {
        self.timeout
    }

    pub(crate) fn into_parts(self) -> RequestParts {
        RequestParts {
            method: self.method,
            url: self.url,
            uri: self.uri,
            headers: self.headers,
            python_header_names: self.python_header_names,
            body: self.body,
        }
    }
}

pub(crate) struct RequestParts {
    pub method: Method,
    pub url: String,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub python_header_names: Option<Vec<String>>,
    pub body: BodySource,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timeout {
    pub connect: Option<Duration>,
    pub read: Option<Duration>,
    pub total: Option<Duration>,
}

#[derive(Debug)]
pub struct RequestBuilder {
    request: Result<Request>,
    transport: Option<Arc<Transport>>,
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
                python_header_names: None,
                body: BodySource::Empty,
                timeout: None,
            }),
            _ => Err(Error::invalid_url(&url)),
        };

        Self {
            request,
            transport: None,
        }
    }

    pub(crate) fn for_client(
        method: Method,
        url: impl AsRef<str>,
        transport: Arc<Transport>,
    ) -> Self {
        Self {
            transport: Some(transport),
            ..Self::new(method, url)
        }
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

    #[doc(hidden)]
    pub fn python_header_names(mut self, names: Vec<String>) -> Self {
        if let Ok(request) = &mut self.request {
            request.python_header_names = Some(names);
        }
        self
    }

    pub fn body(mut self, body: impl Into<BodySource>) -> Self {
        if let Ok(request) = &mut self.request {
            request.body = body.into();
        }
        self
    }

    pub fn timeout(mut self, timeout: Timeout) -> Self {
        if let Ok(request) = &mut self.request {
            request.timeout = Some(timeout);
        }
        self
    }

    pub fn build(self) -> Result<Request> {
        self.request
    }

    pub async fn send(self) -> Result<Response> {
        let request = self.request?;
        let transport = self.transport.ok_or_else(Error::unbound_builder)?;
        execute_request(&transport, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HeaderInput, HeaderPart, InvalidHeaderPart, UrlPreparationError, prepare_headers,
        prepare_method, prepare_method_bytes, prepare_url,
    };

    #[test]
    fn method_preparation_matches_python_upper_for_safe_native_inputs() {
        assert_eq!(prepare_method("pAtCh"), "PATCH");
        assert_eq!(prepare_method_bytes(b"delete"), b"DELETE");
    }

    #[test]
    fn headers_preserve_first_position_and_last_casing() {
        let headers = [
            HeaderInput {
                name: HeaderPart::Text("First".into()),
                value: HeaderPart::Text("one".into()),
            },
            HeaderInput {
                name: HeaderPart::Bytes(b"second".to_vec()),
                value: HeaderPart::Bytes(b"two".to_vec()),
            },
            HeaderInput {
                name: HeaderPart::Text("FIRST".into()),
                value: HeaderPart::Text("three".into()),
            },
        ];
        let prepared = prepare_headers(&headers).unwrap();
        assert_eq!(
            prepared
                .iter()
                .map(|header| (header.name.as_str(), header.source_index))
                .collect::<Vec<_>>(),
            vec![("FIRST", 2), ("second", 1)]
        );
    }

    #[test]
    fn invalid_headers_return_the_prepared_prefix() {
        let headers = [
            HeaderInput {
                name: HeaderPart::Text("Good".into()),
                value: HeaderPart::Text("one".into()),
            },
            HeaderInput {
                name: HeaderPart::Text("Bad".into()),
                value: HeaderPart::Text(" leading".into()),
            },
        ];
        let error = prepare_headers(&headers).unwrap_err();
        assert_eq!(error.part, InvalidHeaderPart::Value);
        assert_eq!(error.source_index, 1);
        assert_eq!(error.prepared[0].name, "Good");
    }

    #[test]
    fn url_preparation_uses_native_parser_and_places_params_before_fragment() {
        assert_eq!(
            prepare_url(" \tHtTp://Example.COM/a path?escaped=%7e&reserved=%2f", ""),
            Ok("http://example.com/a%20path?escaped=~&reserved=%2F".into())
        );
        assert_eq!(
            prepare_url("http://example.com/path?first=1#frag", "next=two"),
            Ok("http://example.com/path?first=1&next=two#frag".into())
        );
        assert_eq!(
            prepare_url("mailto:user@example.org", "ignored=value"),
            Ok("mailto:user@example.org".into())
        );
    }

    #[test]
    fn url_preparation_maps_parser_and_label_failures() {
        assert_eq!(
            prepare_url("example.com/path", ""),
            Err(UrlPreparationError::MissingScheme)
        );
        assert_eq!(
            prepare_url("http://", ""),
            Err(UrlPreparationError::MissingHost)
        );
        assert_eq!(
            prepare_url("http://*.example.com/", ""),
            Err(UrlPreparationError::InvalidLabel)
        );
        assert_eq!(
            prepare_url("http://☃.net/", ""),
            Err(UrlPreparationError::Parse)
        );
    }

    #[test]
    fn native_url_gate_rejects_lossy_parser_serializations() {
        for url in [
            "http://example.com/a/%2e%2e/b",
            "http://example.com:080/path",
            "http://example.com/\\path",
            "http://127.000.000.001/path",
            "http://example.com/path?",
            "http://user:@example.com/path",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://example.com/a\tb",
            "http://example.com/%0",
            "http://example.com/a%0/b",
            "http://example.com/path  ",
            "http://example.com/a[b]/",
            "http://example.com/path#one#two",
            "httpx://EXAMPLE.com",
            "http+unix://%2Fsocket",
            "http:///path",
            "http:////example.com",
            "http:/example.com",
            "http:example.com",
            "http://example .com/path",
            "http://example%20.com/path",
            "http://exam|ple.com/path",
        ] {
            assert!(!super::url_is_native_safe(url), "{url}");
        }
        assert!(super::url_is_native_safe(
            "http+unix://%2Fvar%2Frun%2Fsocket/path%7E"
        ));
    }

    #[test]
    fn standalone_request_builder_cannot_send_without_a_client() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = runtime
            .block_on(super::RequestBuilder::new(http::Method::GET, "http://example.test/").send());
        let Err(error) = result else {
            panic!("standalone builder unexpectedly sent a request");
        };

        assert_eq!(error.kind(), crate::ErrorKind::Builder);
        assert_eq!(
            error.to_string(),
            "request builder is not bound to a Client"
        );
    }
}

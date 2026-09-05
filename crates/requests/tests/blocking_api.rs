#![cfg(feature = "blocking")]

use std::future::{Future, poll_fn};
use std::io::{self, Read};
use std::pin::Pin;
use std::rc::Rc;

use bytes::Bytes;
use futures_core::Stream;
use requests_native::blocking::{
    BlockingDriverError, BlockingRuntimeDriver, BlockingSubmission, BlockingTaskError, Client,
    ClientBuilder, RequestBuilder, Response, ResponseBody,
};
use requests_native::{
    BodySource, Client as CoreClient, ClientBuilder as CoreClientBuilder, ErrorKind, HeaderMap,
    HeaderName, HeaderValue, Method, Proxy, Request, RequestBuilder as CoreRequestBuilder,
    Response as CoreResponse, ResponseBody as CoreResponseBody, Result, StatusCode, Timeout,
    TlsConfig, Version,
};

struct CustomUrl<'a> {
    value: &'a str,
    _not_send: Rc<()>,
}

impl AsRef<str> for CustomUrl<'_> {
    fn as_ref(&self) -> &str {
        self.value
    }
}

struct CustomBody<'a> {
    value: &'a [u8],
    _not_send: Rc<()>,
}

impl<'a> From<CustomBody<'a>> for BodySource {
    fn from(body: CustomBody<'a>) -> Self {
        body.value.to_vec().into()
    }
}

fn custom_url(value: &str) -> CustomUrl<'_> {
    CustomUrl {
        value,
        _not_send: Rc::new(()),
    }
}

fn custom_body(value: &[u8]) -> CustomBody<'_> {
    CustomBody {
        value,
        _not_send: Rc::new(()),
    }
}

fn assert_client_constructors() {
    let new: fn() -> Result<Client> = Client::new;
    let builder: fn() -> ClientBuilder = Client::builder;

    let _: Result<Client> = new();
    let _: ClientBuilder = builder();
}

fn assert_client_surface<'a>(client: &Client, url: &'a str, request: Request) {
    let request_method: fn(&Client, Method, CustomUrl<'a>) -> RequestBuilder = Client::request;
    let get: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::get;
    let head: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::head;
    let post: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::post;
    let put: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::put;
    let patch: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::patch;
    let delete: fn(&Client, CustomUrl<'a>) -> RequestBuilder = Client::delete;
    let execute: fn(&Client, Request) -> Result<Response> = Client::execute;

    let _: RequestBuilder = request_method(client, Method::GET, custom_url(url));
    let _: RequestBuilder = get(client, custom_url(url));
    let _: RequestBuilder = head(client, custom_url(url));
    let _: RequestBuilder = post(client, custom_url(url));
    let _: RequestBuilder = put(client, custom_url(url));
    let _: RequestBuilder = patch(client, custom_url(url));
    let _: RequestBuilder = delete(client, custom_url(url));
    let _: Result<Response> = execute(client, request);
}

fn assert_client_builder_surface(
    builder: ClientBuilder,
    proxy: Proxy,
    tls: TlsConfig,
    timeout: Timeout,
) {
    let proxy_method: fn(ClientBuilder, Proxy) -> ClientBuilder = ClientBuilder::proxy;
    let tls_method: fn(ClientBuilder, TlsConfig) -> ClientBuilder = ClientBuilder::tls;
    let timeout_method: fn(ClientBuilder, Timeout) -> ClientBuilder = ClientBuilder::timeout;
    let pool_method: fn(ClientBuilder, usize) -> ClientBuilder =
        ClientBuilder::pool_max_idle_per_host;
    let build: fn(ClientBuilder) -> Result<Client> = ClientBuilder::build;

    let builder = proxy_method(builder, proxy);
    let builder = tls_method(builder, tls);
    let builder = timeout_method(builder, timeout);
    let builder = pool_method(builder, 1);
    let _: Result<Client> = build(builder);
}

fn assert_request_builder_surface<'a>(
    build_builder: RequestBuilder,
    send_builder: RequestBuilder,
    name: HeaderName,
    value: HeaderValue,
    headers: HeaderMap,
    body: &'a [u8],
    timeout: Timeout,
) {
    let header: fn(RequestBuilder, HeaderName, HeaderValue) -> RequestBuilder =
        RequestBuilder::header;
    let headers_method: fn(RequestBuilder, HeaderMap) -> RequestBuilder = RequestBuilder::headers;
    let body_method: fn(RequestBuilder, CustomBody<'a>) -> RequestBuilder = RequestBuilder::body;
    let timeout_method: fn(RequestBuilder, Timeout) -> RequestBuilder = RequestBuilder::timeout;
    let build: fn(RequestBuilder) -> Result<Request> = RequestBuilder::build;
    let send: fn(RequestBuilder) -> Result<Response> = RequestBuilder::send;

    let builder = header(build_builder, name, value);
    let builder = headers_method(builder, headers);
    let builder = body_method(builder, custom_body(body));
    let builder = timeout_method(builder, timeout);
    let _: Result<Request> = build(builder);
    let _: Result<Response> = send(send_builder);
}

fn assert_response_surface(response: &Response) {
    let status: fn(&Response) -> StatusCode = Response::status;
    let headers: fn(&Response) -> &HeaderMap = Response::headers;
    let url: fn(&Response) -> &str = Response::url;
    let version: fn(&Response) -> Version = Response::version;
    let content_length: fn(&Response) -> Option<u64> = Response::content_length;

    let _: StatusCode = status(response);
    let _: &HeaderMap = headers(response);
    let _: &str = url(response);
    let _: Version = version(response);
    let _: Option<u64> = content_length(response);
}

fn assert_top_level_surface<'a, 'b>(url: &'a str, body: &'b [u8]) {
    let get: fn(CustomUrl<'a>) -> Result<Response> = requests_native::blocking::get;
    let head: fn(CustomUrl<'a>) -> Result<Response> = requests_native::blocking::head;
    let post: fn(CustomUrl<'a>, CustomBody<'b>) -> Result<Response> =
        requests_native::blocking::post;
    let put: fn(CustomUrl<'a>, CustomBody<'b>) -> Result<Response> = requests_native::blocking::put;
    let patch: fn(CustomUrl<'a>, CustomBody<'b>) -> Result<Response> =
        requests_native::blocking::patch;
    let delete: fn(CustomUrl<'a>) -> Result<Response> = requests_native::blocking::delete;

    let _: Result<Response> = get(custom_url(url));
    let _: Result<Response> = head(custom_url(url));
    let _: Result<Response> = post(custom_url(url), custom_body(body));
    let _: Result<Response> = put(custom_url(url), custom_body(body));
    let _: Result<Response> = patch(custom_url(url), custom_body(body));
    let _: Result<Response> = delete(custom_url(url));
}

fn assert_send_static<T: Send + 'static>() {}

fn assert_send_static_future<F: Future + Send + 'static>(future: F) {
    drop(future);
}

#[allow(clippy::too_many_arguments)]
fn assert_owned_core_futures(
    client: CoreClient,
    request: Request,
    builder: CoreRequestBuilder,
    bytes_response: CoreResponse,
    text_response: CoreResponse,
    close_body: CoreResponseBody,
    next_body: CoreResponseBody,
) {
    assert_send_static_future(async move {
        let result: Result<CoreResponse> = client.execute(request).await;
        result
    });
    assert_send_static_future(async move {
        let result: Result<CoreResponse> = builder.send().await;
        result
    });
    assert_send_static_future(async move {
        let result: Result<Bytes> = bytes_response.bytes().await;
        result
    });
    assert_send_static_future(async move {
        let result: Result<String> = text_response.text().await;
        result
    });
    assert_send_static_future(async move {
        let result: Result<()> = close_body.close().await;
        result
    });
    assert_send_static_future(async move {
        let mut body = next_body;
        let frame: Option<Result<Bytes>> =
            poll_fn(|context| Pin::new(&mut body).poll_next(context)).await;
        (body, frame)
    });
}

#[test]
fn blocking_api_compile_contract() {
    assert_send_static::<BodySource>();
    assert_send_static::<CoreClient>();
    assert_send_static::<CoreClientBuilder>();
    assert_send_static::<Request>();
    assert_send_static::<CoreRequestBuilder>();
    assert_send_static::<CoreResponse>();
    assert_send_static::<CoreResponseBody>();
    assert_send_static::<Client>();
    assert_send_static::<ClientBuilder>();
    assert_send_static::<RequestBuilder>();
    assert_send_static::<Response>();
    assert_send_static::<ResponseBody>();
    assert_send_static::<BlockingRuntimeDriver>();
    assert_send_static::<BlockingSubmission<()>>();
    assert_send_static::<BlockingDriverError>();
    assert_send_static::<BlockingTaskError>();

    let _constructors: fn() = assert_client_constructors;
    let _client_surface: for<'a> fn(&Client, &'a str, Request) = assert_client_surface;
    let _client_builder_surface: fn(ClientBuilder, Proxy, TlsConfig, Timeout) =
        assert_client_builder_surface;
    let _request_builder_surface: for<'a> fn(
        RequestBuilder,
        RequestBuilder,
        HeaderName,
        HeaderValue,
        HeaderMap,
        &'a [u8],
        Timeout,
    ) = assert_request_builder_surface;
    let _response_surface: fn(&Response) = assert_response_surface;
    let _response_into_body: fn(Response) -> ResponseBody = Response::into_body;
    let _response_bytes: fn(Response) -> Result<Bytes> = Response::bytes;
    let _response_text: fn(Response) -> Result<String> = Response::text;
    let _response_body_read: fn(&mut ResponseBody, &mut [u8]) -> io::Result<usize> = Read::read;
    let _response_body_close: fn(ResponseBody) -> Result<()> = ResponseBody::close;
    let _top_level_surface: for<'a, 'b> fn(&'a str, &'b [u8]) = assert_top_level_surface;
    let _owned_core_futures: fn(
        CoreClient,
        Request,
        CoreRequestBuilder,
        CoreResponse,
        CoreResponse,
        CoreResponseBody,
        CoreResponseBody,
    ) = assert_owned_core_futures;

    let kind = ErrorKind::Blocking;
    assert!(matches!(kind, ErrorKind::Blocking));
}

mod connect;

use std::future::Future;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::task::JoinHandle;

use crate::{BodySource, Error, Request, Result};

#[derive(Debug)]
pub(crate) struct Transport;

pub(crate) struct TransportResponse {
    pub head: http::response::Parts,
    pub body: Incoming,
    pub url: String,
    pub driver: ConnectionDriver,
}

pub(crate) struct ConnectionDriver {
    task: JoinHandle<Result<()>>,
}

impl ConnectionDriver {
    fn spawn(task: impl Future<Output = Result<()>> + Send + 'static) -> Self {
        Self {
            task: tokio::spawn(task),
        }
    }

    pub(crate) fn task_mut(&mut self) -> &mut JoinHandle<Result<()>> {
        &mut self.task
    }

    pub(crate) async fn abort_and_wait(&mut self) -> Result<()> {
        self.task.abort();
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(Error::connection(error)),
        }
    }
}

impl Drop for ConnectionDriver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Transport {
    pub async fn send(&self, request: Request) -> Result<TransportResponse> {
        validate_request(&request)?;
        let host = request
            .uri()
            .host()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .to_owned();
        let port = request.uri().port_u16().unwrap_or(80);
        let target = request
            .uri()
            .authority()
            .ok_or_else(|| Error::invalid_url(request.url()))?
            .as_str()
            .to_owned();
        let outgoing = outgoing_request(&request)?;
        let url = request.url().to_owned();

        let stream = connect::connect(&host, port, &target).await?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(Error::handshake)?;
        let mut driver =
            ConnectionDriver::spawn(async move { connection.await.map_err(Error::connection) });

        let sending = sender.send_request(outgoing);
        tokio::pin!(sending);
        let (response, driver_pending) = tokio::select! {
            biased;
            driver_result = driver.task_mut() => {
                let error = match driver_result {
                    Ok(Ok(())) => Err(Error::connection_stopped()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(Error::connection(error)),
                };
                (error, false)
            }
            result = &mut sending => (result.map_err(Error::send), true),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if driver_pending {
                    drop(driver.abort_and_wait().await);
                }
                return Err(error);
            }
        };
        let (head, body) = response.into_parts();

        Ok(TransportResponse {
            head,
            body,
            url,
            driver,
        })
    }
}

fn validate_request(request: &Request) -> Result<()> {
    if !matches!(request.body(), BodySource::Empty) {
        return Err(Error::unsupported_request_body());
    }
    if request.uri().scheme_str() != Some("http") {
        return Err(Error::unsupported_scheme(
            request.url(),
            request.uri().scheme_str(),
        ));
    }
    Ok(())
}

fn outgoing_request(request: &Request) -> Result<http::Request<Empty<Bytes>>> {
    let origin = request
        .uri()
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str)
        .parse::<http::Uri>()
        .map_err(|_| Error::invalid_url(request.url()))?;
    let mut outgoing = http::Request::new(Empty::new());
    *outgoing.method_mut() = request.method().clone();
    *outgoing.uri_mut() = origin;
    *outgoing.headers_mut() = request.headers().clone();
    Ok(outgoing)
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http::{HeaderName, HeaderValue, Method};

    use super::{ConnectionDriver, outgoing_request, validate_request};
    use crate::{AsyncBody, BodySource, ErrorKind, RequestBuilder};

    struct NeverBody;

    impl AsyncBody for NeverBody {
        fn poll_next(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<crate::Result<Bytes>>> {
            Poll::Ready(None)
        }

        fn size_hint(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn request_validation_rejects_nonempty_bodies_and_https_before_io() {
        let bytes = RequestBuilder::new(Method::GET, "http://example.test/")
            .body(Bytes::from_static(b"body"))
            .build()
            .unwrap();
        let stream = RequestBuilder::new(Method::GET, "http://example.test/")
            .body(BodySource::Stream(Box::pin(NeverBody)))
            .build()
            .unwrap();
        let https = RequestBuilder::new(Method::GET, "https://example.test/")
            .build()
            .unwrap();

        assert_eq!(
            validate_request(&bytes).unwrap_err().kind(),
            ErrorKind::Body
        );
        assert_eq!(
            validate_request(&stream).unwrap_err().kind(),
            ErrorKind::Body
        );
        assert_eq!(
            validate_request(&https).unwrap_err().kind(),
            ErrorKind::InvalidUrl
        );
    }

    #[test]
    fn outgoing_get_uses_origin_form_and_preserves_explicit_host() {
        let request =
            RequestBuilder::new(Method::GET, "http://example.test:8080/direct?source=unit")
                .header(
                    HeaderName::from_static("host"),
                    HeaderValue::from_static("example.test:8080"),
                )
                .build()
                .unwrap();

        let outgoing = outgoing_request(&request).unwrap();

        assert_eq!(outgoing.uri().to_string(), "/direct?source=unit");
        assert_eq!(outgoing.headers().len(), 1);
        assert_eq!(
            outgoing.headers().get("host"),
            Some(&HeaderValue::from_static("example.test:8080"))
        );
    }

    #[test]
    fn dropping_connection_driver_aborts_its_task() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dropped = Arc::new(AtomicBool::new(false));
            let task_dropped = Arc::clone(&dropped);
            let driver = ConnectionDriver::spawn(async move {
                let _drop_flag = DropFlag(task_dropped);
                future::pending::<()>().await;
                Ok(())
            });
            tokio::task::yield_now().await;

            drop(driver);
            for _ in 0..10 {
                if dropped.load(Ordering::Acquire) {
                    break;
                }
                tokio::task::yield_now().await;
            }

            assert!(dropped.load(Ordering::Acquire));
        });
    }
}

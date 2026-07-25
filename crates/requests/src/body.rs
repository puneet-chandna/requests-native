use bytes::Bytes;

#[derive(Debug, Default)]
#[non_exhaustive]
pub enum BodySource {
    #[default]
    Empty,
    Bytes(Bytes),
}

impl From<Bytes> for BodySource {
    fn from(body: Bytes) -> Self {
        Self::Bytes(body)
    }
}

impl From<Vec<u8>> for BodySource {
    fn from(body: Vec<u8>) -> Self {
        Self::Bytes(body.into())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use bytes::Bytes;

    use super::{AsyncBody, BodySource};

    struct ChunkBody {
        chunks: VecDeque<Bytes>,
        size_hint: Option<u64>,
    }

    impl AsyncBody for ChunkBody {
        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<crate::Result<Bytes>>> {
            Poll::Ready(self.chunks.pop_front().map(Ok))
        }

        fn size_hint(&self) -> Option<u64> {
            self.size_hint
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn body_stream_preserves_size_hint_and_poll_order_after_type_erasure() {
        let mut body = BodySource::Stream(Box::pin(ChunkBody {
            chunks: VecDeque::from([Bytes::from_static(b"first"), Bytes::from_static(b"second")]),
            size_hint: Some(11),
        }));
        let BodySource::Stream(stream) = &mut body else {
            panic!("expected stream body");
        };
        assert_eq!(stream.size_hint(), Some(11));

        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        assert_eq!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(Bytes::from_static(b"first"))))
        );
        assert_eq!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(Bytes::from_static(b"second"))))
        );
        assert_eq!(stream.as_mut().poll_next(&mut context), Poll::Ready(None));
    }

    #[test]
    fn body_stream_can_report_unknown_length() {
        let body = BodySource::Stream(Box::pin(ChunkBody {
            chunks: VecDeque::new(),
            size_hint: None,
        }));
        let BodySource::Stream(stream) = body else {
            panic!("expected stream body");
        };

        assert_eq!(stream.size_hint(), None);
    }
}

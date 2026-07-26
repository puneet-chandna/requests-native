//! Incremental response content-decoder adapters.

use std::cmp;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use async_compression::tokio::bufread::{
    BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder,
};
use bytes::Bytes;
use hyper::body::{Body, Incoming};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

use crate::{ContentCodecs, Error, Result};

const DECODE_CHUNK_SIZE: usize = 8 * 1024;
const INITIAL_DECODE_OUTPUT_LIMIT: usize = 16;
// Minimize prefix loss when Brotli discards same-call offsets on failure.
const BROTLI_INITIAL_INPUT_LIMIT: usize = 1;

#[derive(Clone, Copy)]
pub(crate) enum ContentEncoding {
    Gzip,
    Deflate,
    Brotli,
    Zstandard,
}

impl ContentEncoding {
    pub(crate) fn enabled(value: &str, codecs: ContentCodecs) -> Option<Self> {
        let value = value.trim();
        if !codecs.decodes(value) {
            return None;
        }
        if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
            Some(Self::Gzip)
        } else if value.eq_ignore_ascii_case("deflate") {
            Some(Self::Deflate)
        } else if value.eq_ignore_ascii_case("br") {
            Some(Self::Brotli)
        } else if value.eq_ignore_ascii_case("zstd") {
            Some(Self::Zstandard)
        } else {
            None
        }
    }
}

type GzipBody = GzipDecoder<IncomingReader>;
type ZlibBody = ZlibDecoder<IncomingReader>;
type RawDeflateBody = DeflateDecoder<IncomingReader>;
type BrotliBody = BrotliDecoder<IncomingReader>;
type ZstandardBody = ZstdDecoder<IncomingReader>;

enum DecoderState {
    Gzip(GzipBody),
    Zlib(ZlibBody),
    Deflate(RawDeflateBody),
    Brotli(BrotliBody),
    Zstandard(ZstandardBody),
    Drain(IncomingReader),
    RejectTrailing(IncomingReader),
}

impl DecoderState {
    fn reader_mut(&mut self) -> &mut IncomingReader {
        match self {
            Self::Gzip(decoder) => decoder.get_mut(),
            Self::Zlib(decoder) => decoder.get_mut(),
            Self::Deflate(decoder) => decoder.get_mut(),
            Self::Brotli(decoder) => decoder.get_mut(),
            Self::Zstandard(decoder) => decoder.get_mut(),
            Self::Drain(reader) | Self::RejectTrailing(reader) => reader,
        }
    }
}

pub(crate) struct DecodedBody {
    state: Option<DecoderState>,
    zlib_output: bool,
    output_seen: bool,
    pending_decoder_error: Option<io::Error>,
}

impl DecodedBody {
    pub(crate) fn new(body: Incoming, encoding: ContentEncoding, chunked: bool) -> Self {
        let mut reader = IncomingReader::new(body, chunked);
        if matches!(encoding, ContentEncoding::Brotli) {
            reader.limit_input_until_output(BROTLI_INITIAL_INPUT_LIMIT);
        }
        let state = match encoding {
            ContentEncoding::Gzip => {
                let mut decoder = GzipDecoder::new(reader);
                decoder.multiple_members(true);
                DecoderState::Gzip(decoder)
            }
            ContentEncoding::Deflate => {
                reader.start_tentative_recording();
                DecoderState::Zlib(ZlibDecoder::new(reader))
            }
            ContentEncoding::Brotli => DecoderState::Brotli(BrotliDecoder::new(reader)),
            ContentEncoding::Zstandard => {
                let mut decoder = ZstdDecoder::new(reader);
                decoder.multiple_members(true);
                DecoderState::Zstandard(decoder)
            }
        };
        Self {
            state: Some(state),
            zlib_output: false,
            output_seen: false,
            pending_decoder_error: None,
        }
    }

    pub(crate) fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<Result<Bytes>>> {
        macro_rules! poll_multi_member {
            ($decoder:ident, $variant:ident) => {{
                if let Some(error) = self.pending_decoder_error.take() {
                    let mut reader = $decoder.into_inner();
                    if let Some(error) = reader.take_transport_error() {
                        self.state = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                    if is_accepted_truncation(&error, &reader) {
                        self.state = Some(DecoderState::Drain(reader));
                        continue;
                    }
                    self.state = None;
                    return Poll::Ready(Some(Err(Error::content_decoding())));
                }
                if let Some(error) = $decoder.get_mut().take_transport_error() {
                    self.state = None;
                    return Poll::Ready(Some(Err(error)));
                }
                match poll_decoder(&mut $decoder, context, self.output_seen) {
                    Poll::Pending => {
                        self.state = Some(DecoderState::$variant($decoder));
                        return Poll::Pending;
                    }
                    Poll::Ready(read) if !read.bytes.is_empty() => {
                        self.output_seen = true;
                        self.pending_decoder_error = read.error;
                        self.state = Some(DecoderState::$variant($decoder));
                        return Poll::Ready(Some(Ok(read.bytes)));
                    }
                    Poll::Ready(DecoderRead {
                        bytes: _,
                        error: Some(error),
                    }) => {
                        let mut reader = $decoder.into_inner();
                        if let Some(error) = reader.take_transport_error() {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                        if is_accepted_truncation(&error, &reader) {
                            self.state = Some(DecoderState::Drain(reader));
                            continue;
                        }
                        self.state = None;
                        return Poll::Ready(Some(Err(Error::content_decoding())));
                    }
                    Poll::Ready(DecoderRead {
                        bytes: _,
                        error: None,
                    }) => {
                        let mut reader = $decoder.into_inner();
                        if let Some(error) = reader.take_transport_error() {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                        self.state = Some(DecoderState::Drain(reader));
                    }
                }
            }};
        }

        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(None);
            };
            match state {
                DecoderState::Gzip(mut decoder) => {
                    poll_multi_member!(decoder, Gzip);
                }
                DecoderState::Zlib(mut decoder) => {
                    if let Some(error) = self.pending_decoder_error.take() {
                        let mut reader = decoder.into_inner();
                        reader.stop_tentative_recording();
                        if let Some(error) = reader.take_transport_error() {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                        if is_accepted_flate_truncation(&error, &reader) {
                            self.state = Some(DecoderState::Drain(reader));
                            continue;
                        }
                        self.state = None;
                        return Poll::Ready(Some(Err(Error::content_decoding())));
                    }
                    if let Some(error) = decoder.get_mut().take_transport_error() {
                        self.state = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                    match poll_decoder(&mut decoder, context, self.output_seen) {
                        Poll::Pending => {
                            self.state = Some(DecoderState::Zlib(decoder));
                            return Poll::Pending;
                        }
                        Poll::Ready(read) if !read.bytes.is_empty() => {
                            self.zlib_output = true;
                            self.output_seen = true;
                            decoder.get_mut().stop_tentative_recording();
                            self.pending_decoder_error = read.error;
                            self.state = Some(DecoderState::Zlib(decoder));
                            return Poll::Ready(Some(Ok(read.bytes)));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: None,
                        }) => {
                            let mut reader = decoder.into_inner();
                            reader.stop_tentative_recording();
                            if let Some(error) = reader.take_transport_error() {
                                self.state = None;
                                return Poll::Ready(Some(Err(error)));
                            }
                            self.state = Some(DecoderState::Drain(reader));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: Some(error),
                        }) => {
                            let mut reader = decoder.into_inner();
                            if let Some(error) = reader.take_transport_error() {
                                self.state = None;
                                return Poll::Ready(Some(Err(error)));
                            }
                            if is_accepted_flate_truncation(&error, &reader) {
                                reader.stop_tentative_recording();
                                self.state = Some(DecoderState::Drain(reader));
                                continue;
                            }
                            if !self.zlib_output {
                                reader.install_tentative_replay();
                                self.state =
                                    Some(DecoderState::Deflate(DeflateDecoder::new(reader)));
                            } else {
                                self.state = None;
                                return Poll::Ready(Some(Err(Error::content_decoding())));
                            }
                        }
                    }
                }
                DecoderState::Deflate(mut decoder) => {
                    if let Some(error) = self.pending_decoder_error.take() {
                        let mut reader = decoder.into_inner();
                        if let Some(error) = reader.take_transport_error() {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                        if is_accepted_flate_truncation(&error, &reader) {
                            self.state = Some(DecoderState::Drain(reader));
                            continue;
                        }
                        self.state = None;
                        return Poll::Ready(Some(Err(Error::content_decoding())));
                    }
                    if let Some(error) = decoder.get_mut().take_transport_error() {
                        self.state = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                    match poll_decoder(&mut decoder, context, self.output_seen) {
                        Poll::Pending => {
                            self.state = Some(DecoderState::Deflate(decoder));
                            return Poll::Pending;
                        }
                        Poll::Ready(read) if !read.bytes.is_empty() => {
                            self.output_seen = true;
                            self.pending_decoder_error = read.error;
                            self.state = Some(DecoderState::Deflate(decoder));
                            return Poll::Ready(Some(Ok(read.bytes)));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: None,
                        }) => {
                            let mut reader = decoder.into_inner();
                            if let Some(error) = reader.take_transport_error() {
                                self.state = None;
                                return Poll::Ready(Some(Err(error)));
                            }
                            self.state = Some(DecoderState::Drain(reader));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: Some(error),
                        }) => {
                            let mut reader = decoder.into_inner();
                            if let Some(error) = reader.take_transport_error() {
                                self.state = None;
                                return Poll::Ready(Some(Err(error)));
                            }
                            if is_accepted_flate_truncation(&error, &reader) {
                                self.state = Some(DecoderState::Drain(reader));
                                continue;
                            }
                            self.state = None;
                            return Poll::Ready(Some(Err(Error::content_decoding())));
                        }
                    }
                }
                DecoderState::Brotli(mut decoder) => {
                    if let Some(error) = self.pending_decoder_error.take() {
                        let mut reader = decoder.into_inner();
                        if let Some(error) = reader.take_transport_error() {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                        if is_accepted_truncation(&error, &reader) {
                            self.state = Some(DecoderState::Drain(reader));
                            continue;
                        }
                        self.state = None;
                        return Poll::Ready(Some(Err(Error::content_decoding())));
                    }
                    if let Some(error) = decoder.get_mut().take_transport_error() {
                        self.state = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                    match poll_decoder(&mut decoder, context, self.output_seen) {
                        Poll::Pending => {
                            self.state = Some(DecoderState::Brotli(decoder));
                            return Poll::Pending;
                        }
                        Poll::Ready(read) if !read.bytes.is_empty() => {
                            self.output_seen = true;
                            decoder.get_mut().remove_input_limit();
                            self.pending_decoder_error = read.error;
                            self.state = Some(DecoderState::Brotli(decoder));
                            return Poll::Ready(Some(Ok(read.bytes)));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: Some(error),
                        }) => {
                            let mut reader = decoder.into_inner();
                            if let Some(error) = reader.take_transport_error() {
                                self.state = None;
                                return Poll::Ready(Some(Err(error)));
                            }
                            if is_accepted_truncation(&error, &reader) {
                                self.state = Some(DecoderState::Drain(reader));
                                continue;
                            }
                            self.state = None;
                            return Poll::Ready(Some(Err(Error::content_decoding())));
                        }
                        Poll::Ready(DecoderRead {
                            bytes: _,
                            error: None,
                        }) => {
                            self.state = Some(DecoderState::RejectTrailing(decoder.into_inner()));
                        }
                    }
                }
                DecoderState::Zstandard(mut decoder) => {
                    poll_multi_member!(decoder, Zstandard);
                }
                DecoderState::Drain(mut reader) => match reader.poll_drain(context) {
                    Poll::Pending => {
                        self.state = Some(DecoderState::Drain(reader));
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => {
                        self.state = None;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Err(error)) => {
                        self.state = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                },
                DecoderState::RejectTrailing(mut reader) => {
                    match reader.poll_reject_trailing(context) {
                        Poll::Pending => {
                            self.state = Some(DecoderState::RejectTrailing(reader));
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(())) => {
                            self.state = None;
                            return Poll::Ready(None);
                        }
                        Poll::Ready(Err(error)) => {
                            self.state = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn take_wire_progress(&mut self) -> bool {
        self.state
            .as_mut()
            .is_some_and(|state| state.reader_mut().take_wire_progress())
    }
}

struct DecoderRead {
    bytes: Bytes,
    error: Option<io::Error>,
}

fn poll_decoder(
    decoder: &mut (impl AsyncRead + Unpin),
    context: &mut Context<'_>,
    output_seen: bool,
) -> Poll<DecoderRead> {
    let mut output = [0_u8; DECODE_CHUNK_SIZE];
    let output_limit = if output_seen {
        DECODE_CHUNK_SIZE
    } else {
        INITIAL_DECODE_OUTPUT_LIMIT
    };
    let mut read = ReadBuf::new(&mut output[..output_limit]);
    match Pin::new(decoder).poll_read(context, &mut read) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(result) => Poll::Ready(DecoderRead {
            bytes: Bytes::copy_from_slice(read.filled()),
            error: result.err(),
        }),
    }
}

fn is_accepted_truncation(error: &io::Error, reader: &IncomingReader) -> bool {
    error.kind() == io::ErrorKind::UnexpectedEof
        && reader.clean_eof
        && reader.transport_error.is_none()
}

fn is_accepted_flate_truncation(error: &io::Error, reader: &IncomingReader) -> bool {
    reader.clean_eof && reader.transport_error.is_none() && is_flate_incomplete(error)
}

fn is_flate_incomplete(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::UnexpectedEof
        || (error.kind() == io::ErrorKind::Other && error.to_string() == "unexpected BufError")
}

struct IncomingReader {
    body: Pin<Box<Incoming>>,
    buffered: Bytes,
    buffered_offset: usize,
    replay: Bytes,
    replay_offset: usize,
    tentative: Vec<u8>,
    record_tentative: bool,
    transport_error: Option<Error>,
    clean_eof: bool,
    chunked: bool,
    input_limit: Option<usize>,
    wire_progress: bool,
}

impl IncomingReader {
    fn new(body: Incoming, chunked: bool) -> Self {
        Self {
            body: Box::pin(body),
            buffered: Bytes::new(),
            buffered_offset: 0,
            replay: Bytes::new(),
            replay_offset: 0,
            tentative: Vec::new(),
            record_tentative: false,
            transport_error: None,
            clean_eof: false,
            chunked,
            input_limit: None,
            wire_progress: false,
        }
    }

    fn take_wire_progress(&mut self) -> bool {
        mem::take(&mut self.wire_progress)
    }

    fn limit_input_until_output(&mut self, limit: usize) {
        self.input_limit = Some(limit);
    }

    fn remove_input_limit(&mut self) {
        self.input_limit = None;
    }

    fn start_tentative_recording(&mut self) {
        self.record_tentative = true;
    }

    fn stop_tentative_recording(&mut self) {
        self.record_tentative = false;
        self.tentative.clear();
    }

    fn install_tentative_replay(&mut self) {
        self.record_tentative = false;
        debug_assert_eq!(self.replay_offset, self.replay.len());
        self.replay = Bytes::from(mem::take(&mut self.tentative));
        self.replay_offset = 0;
    }

    fn take_transport_error(&mut self) -> Option<Error> {
        self.transport_error.take()
    }

    fn poll_drain(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            if let Some(error) = self.take_transport_error() {
                return Poll::Ready(Err(error));
            }
            let available = match Pin::new(&mut *self).poll_fill_buf(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(available)) => available,
                Poll::Ready(Err(_)) => {
                    return Poll::Ready(Err(Error::response_body("response body adapter failed")));
                }
            };
            if available.is_empty() {
                if let Some(error) = self.take_transport_error() {
                    return Poll::Ready(Err(error));
                }
                debug_assert!(self.clean_eof);
                return Poll::Ready(Ok(()));
            }
            let consumed = available.len();
            Pin::new(&mut *self).consume(consumed);
        }
    }

    fn poll_reject_trailing(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        if let Some(error) = self.take_transport_error() {
            return Poll::Ready(Err(error));
        }
        let available = match Pin::new(&mut *self).poll_fill_buf(context) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(available)) => available,
            Poll::Ready(Err(_)) => {
                return Poll::Ready(Err(Error::response_body("response body adapter failed")));
            }
        };
        if !available.is_empty() {
            return Poll::Ready(Err(Error::content_decoding()));
        }
        if let Some(error) = self.take_transport_error() {
            return Poll::Ready(Err(error));
        }
        debug_assert!(self.clean_eof);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for IncomingReader {
    fn poll_fill_buf(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        loop {
            if this.replay_offset < this.replay.len() {
                let end = this.input_limit.map_or(this.replay.len(), |limit| {
                    (this.replay_offset + limit).min(this.replay.len())
                });
                return Poll::Ready(Ok(&this.replay[this.replay_offset..end]));
            }
            if this.buffered_offset < this.buffered.len() {
                let end = this.input_limit.map_or(this.buffered.len(), |limit| {
                    (this.buffered_offset + limit).min(this.buffered.len())
                });
                return Poll::Ready(Ok(&this.buffered[this.buffered_offset..end]));
            }
            if this.transport_error.is_some() || this.clean_eof {
                return Poll::Ready(Ok(&[]));
            }
            match ready!(this.body.as_mut().poll_frame(context)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(bytes) if !bytes.is_empty() => {
                        this.wire_progress = true;
                        this.buffered = bytes;
                        this.buffered_offset = 0;
                    }
                    Ok(_) | Err(_) => {}
                },
                Some(Err(error)) => {
                    this.transport_error = Some(if this.chunked {
                        Error::chunked_encoding(error)
                    } else {
                        Error::response_body(error)
                    });
                }
                None => this.clean_eof = true,
            }
        }
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        if this.replay_offset < this.replay.len() {
            assert!(amount <= this.replay.len() - this.replay_offset);
            if this.record_tentative {
                let end = this.replay_offset + amount;
                this.tentative
                    .extend_from_slice(&this.replay[this.replay_offset..end]);
            }
            this.replay_offset += amount;
        } else {
            assert!(amount <= this.buffered.len() - this.buffered_offset);
            if this.record_tentative {
                let end = this.buffered_offset + amount;
                this.tentative
                    .extend_from_slice(&this.buffered[this.buffered_offset..end]);
            }
            this.buffered_offset += amount;
        }
    }
}

impl AsyncRead for IncomingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let available = ready!(self.as_mut().poll_fill_buf(context))?;
        let amount = cmp::min(available.len(), output.remaining());
        output.put_slice(&available[..amount]);
        self.consume(amount);
        Poll::Ready(Ok(()))
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The `aws-chunked` request-body decoder.

use crate::decoder::{Decoder, Signing};
use crate::error::Error;
use crate::limits::Limits;
use crate::sign::{SignContext, SignState};
use crate::trailer::TrailerHandle;
use crate::utils::{Sha256Sum, StdError};

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use pin_project_lite::pin_project;

pin_project! {
    /// Decoder for `aws-chunked` request bodies.
    ///
    /// The signing behaviour is fixed when the decoder is constructed:
    /// [`ChunkedStream::unsigned`] rejects signatures it cannot verify, while
    /// [`ChunkedStream::signed`] requires and verifies them. Whether the body
    /// carries a trailer block is discovered while decoding, unless
    /// [`ChunkedStream::with_required_trailers`] says the request declared one.
    pub struct ChunkedStream<S> {
        #[pin]
        decoder: Decoder<S>,
    }
}

impl<S> ChunkedStream<S> {
    /// Requires the body to end with a trailer block.
    ///
    /// A request that announced trailing headers but ends without them is a format
    /// error instead of succeeding with nothing to expose. The default stays
    /// permissive, because a body without trailers is valid for requests that did
    /// not announce any.
    #[must_use]
    pub fn with_required_trailers(mut self, required: bool) -> Self {
        self.decoder.require_trailers(required);
        self
    }

    /// Creates an unsigned decoder without signature verification.
    ///
    /// A chunk signature or trailer signature in the body is rejected as a
    /// format error, because a decoder without a signing context cannot verify
    /// it.
    #[must_use]
    pub fn unsigned(inner: S, decoded_content_length: usize, limits: Limits) -> Self {
        Self {
            decoder: Decoder::new(inner, decoded_content_length, limits, Signing::None),
        }
    }

    /// Creates a signed decoder that requires and verifies chunk and trailer signatures.
    #[must_use]
    pub fn signed(inner: S, ctx: SignContext, seed_signature: Sha256Sum, decoded_content_length: usize, limits: Limits) -> Self {
        let sign = SignState::new(ctx, seed_signature);
        Self {
            decoder: Decoder::new(inner, decoded_content_length, limits, Signing::Required(sign)),
        }
    }

    /// Returns a handle to the trailing headers of this stream.
    #[must_use]
    pub fn trailer_handle(&self) -> TrailerHandle {
        self.decoder.trailer_handle()
    }

    /// Returns the declared decoded length minus the bytes produced so far.
    #[must_use]
    pub fn exact_remaining_length(&self) -> usize {
        self.decoder.exact_remaining_length()
    }

    /// Returns the wrapped input stream.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.decoder.into_inner()
    }
}

impl<S> Stream for ChunkedStream<S>
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().decoder.poll_next(cx)
    }
}

#[cfg(test)]
mod unsigned_tests {
    use super::*;
    use crate::limits::Limits;
    use crate::test_utils::{Item, chunk_signature, drain, fragmented, seed_signature, signed_chunk_line, single};

    fn decode(items: Vec<Item>, decoded_content_length: usize) -> (Vec<u8>, Option<Error>) {
        drain(ChunkedStream::unsigned(
            futures::stream::iter(items),
            decoded_content_length,
            Limits::default(),
        ))
    }

    #[test]
    fn decodes_a_simple_body() {
        let (out, err) = decode(single(b"5\r\nhello\r\n0\r\n\r\n"), 5);
        assert_eq!(out, b"hello");
        assert!(err.is_none());
    }

    #[test]
    fn decodes_with_arbitrary_fragmentation() {
        let body = b"5\r\nhello\r\n3\r\n123\r\n0\r\n\r\n";
        for size in 1..=body.len() {
            let (out, err) = decode(fragmented(body, size), 8);
            assert_eq!(out, b"hello123", "fragment size {size}");
            assert!(err.is_none(), "fragment size {size}: {err:?}");
        }
    }

    #[test]
    fn accepts_an_empty_trailer_block() {
        for body in [b"5\r\nhello\r\n0\r\n\r\n".as_slice(), b"5\r\nhello\r\n0\r\n".as_slice()] {
            let (out, err) = decode(single(body), 5);
            assert_eq!(out, b"hello");
            assert!(err.is_none());
        }
    }

    #[test]
    fn exposes_trailing_headers_one_shot() {
        let body = b"0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n";
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, Limits::default());
        let handle = stream.trailer_handle();
        assert!(!handle.is_ready());
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(err.is_none());
        assert!(handle.is_ready());
        let headers = handle.take().expect("trailers are ready");
        assert_eq!(headers.get("x-amz-checksum-crc32").expect("header"), "AAAAAA==");
        assert!(handle.take().is_none());
    }

    #[test]
    fn preserves_duplicate_trailer_names() {
        let body = b"0\r\nx-amz-meta-a:1\r\nx-amz-meta-a:2\r\n\r\n";
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, Limits::default());
        let handle = stream.trailer_handle();
        let (_, err) = drain(stream);
        assert!(err.is_none());
        let headers = handle.take().expect("trailers are ready");
        let values: Vec<_> = headers.get_all("x-amz-meta-a").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(values, ["1", "2"]);
    }

    #[test]
    fn rejects_a_trailer_signature_in_unsigned_mode() {
        let signature = "0".repeat(64);
        let body = format!("0\r\nx-amz-a:1\r\nx-amz-trailer-signature:{signature}\r\n\r\n");
        let (out, err) = decode(single(body.as_bytes()), 0);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::FormatError)), "{err:?}");
    }

    #[test]
    fn reports_malformed_framing() {
        let cases: &[&[u8]] = &[
            b"zz\r\n",
            b"5\r\nhelloXX",
            b"5\r\nhello\r\r\n0\r\n\r\n",
            b"5;chunk-signature=short\r\nhello\r\n0\r\n\r\n",
        ];
        for body in cases {
            let (_, err) = decode(single(body), 5);
            assert!(matches!(err, Some(Error::FormatError)), "{body:?}: {err:?}");
        }

        // A signature must be exactly 64 hex characters followed by CR.
        let mut malformed = Vec::new();
        malformed.extend_from_slice(b"5;chunk-signature=");
        malformed.extend_from_slice(&[b'0'; 64]);
        malformed.push(b'X');
        malformed.extend_from_slice(b"\r\nhello\r\n0\r\n\r\n");
        let (_, err) = decode(single(&malformed), 5);
        assert!(matches!(err, Some(Error::FormatError)), "{err:?}");
    }

    #[test]
    fn reports_eof_inside_the_trailing_crlf() {
        let (out, err) = decode(single(b"5\r\nhello\r"), 5);
        assert_eq!(out, b"hello");
        assert!(matches!(err, Some(Error::Incomplete)), "{err:?}");
    }

    #[test]
    fn reports_incomplete_chunk_data() {
        let (out, err) = decode(single(b"5\r\nhel"), 5);
        assert_eq!(out, b"hel");
        assert!(matches!(err, Some(Error::Incomplete)));
    }

    #[test]
    fn reports_decoded_length_overrun() {
        let (out, err) = decode(single(b"5\r\nhello\r\n0\r\n\r\n"), 3);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::LengthMismatch)));
    }

    #[test]
    fn reports_a_short_decoded_length() {
        let (out, err) = decode(single(b"5\r\nhello\r\n0\r\n\r\n"), 10);
        assert_eq!(out, b"hello");
        assert!(matches!(err, Some(Error::Incomplete)));
    }

    #[test]
    fn accepts_eof_at_a_chunk_boundary() {
        let (out, err) = decode(single(b""), 0);
        assert_eq!(out, b"");
        assert!(err.is_none());
    }

    #[test]
    fn enforces_the_metadata_limit() {
        let limits = Limits {
            max_chunk_meta_size: 4,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"0000005\r\nhello\r\n0\r\n\r\n")), 5, limits);
        let (_, err) = drain(stream);
        assert!(matches!(err, Some(Error::ChunkMetaTooLarge(9, 4))), "{err:?}");
    }

    #[test]
    fn accepts_the_metadata_limit_at_the_boundary() {
        let body = b"0000005\r\nhello\r\n0\r\n\r\n";
        assert_eq!(body.len(), 21);

        // The metadata line is "0000005\r\n" (9 bytes), so 9 is the largest
        // accepted limit.
        let limits = Limits {
            max_chunk_meta_size: 9,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 5, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"hello");
        assert!(err.is_none(), "{err:?}");

        let limits = Limits {
            max_chunk_meta_size: 8,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 5, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::ChunkMetaTooLarge(9, 8))), "{err:?}");
    }

    #[test]
    fn enforces_the_trailer_limits() {
        let limits = Limits {
            max_trailers_size: 8,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"0\r\nx-amz-a:1\r\n\r\n")), 0, limits);
        let (_, err) = drain(stream);
        assert!(matches!(err, Some(Error::TrailersTooLarge(_, 8))), "{err:?}");

        let limits = Limits {
            max_trailer_headers: 1,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"0\r\na:1\r\nb:2\r\n\r\n")), 0, limits);
        let (_, err) = drain(stream);
        assert!(matches!(err, Some(Error::TooManyTrailerHeaders(2, 1))), "{err:?}");
    }

    #[test]
    fn accepts_the_trailer_limit_at_the_boundary() {
        let body = b"0\r\nx-amz-a:1\r\n\r\n";

        // The trailer block is "x-amz-a:1\r\n\r\n" (13 bytes), so 13 is the
        // largest accepted limit.
        let limits = Limits {
            max_trailers_size: 13,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, limits);
        let handle = stream.trailer_handle();
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(err.is_none(), "{err:?}");
        let headers = handle.take().expect("trailers are ready");
        assert_eq!(headers.get("x-amz-a").expect("header"), "1");

        let limits = Limits {
            max_trailers_size: 12,
            ..Limits::default()
        };
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::TrailersTooLarge(13, 12))), "{err:?}");
    }

    /// A stream that reports `Pending` before every fragment.
    fn pending_between_fragments(items: Vec<Item>) -> impl Stream<Item = Item> + Unpin {
        let mut inner = futures::stream::iter(items);
        let mut pending = false;
        futures::stream::poll_fn(move |cx| {
            pending = !pending;
            if pending {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Pin::new(&mut inner).poll_next(cx)
        })
    }

    #[test]
    fn propagates_pending_between_fragments() {
        let bodies: &[(&[u8], usize)] = &[(b"5\r\nhello\r\n0\r\n\r\n", 5), (b"5\r\nhello\r\n0\r\nx-amz-a:1\r\n\r\n", 5)];
        for &(body, declared) in bodies {
            let items = fragmented(body, 3);
            let stream = ChunkedStream::unsigned(pending_between_fragments(items), declared, Limits::default());
            let (out, err) = drain(stream);
            assert_eq!(out, b"hello", "{body:?}");
            assert!(err.is_none(), "{body:?}: {err:?}");
        }
    }

    #[test]
    fn reports_underlying_stream_errors_in_every_phase() {
        // The error arrives while chunk data is being read.
        let items: Vec<Item> = vec![Ok(Bytes::from_static(b"5\r\nhel")), Err("boom".into())];
        let (out, err) = decode(items, 5);
        assert_eq!(out, b"hel");
        assert!(matches!(err, Some(Error::Underlying(_))), "{err:?}");

        // The error arrives while the trailing CRLF is consumed.
        let items: Vec<Item> = vec![Ok(Bytes::from_static(b"5\r\nhello")), Err("boom".into())];
        let (out, err) = decode(items, 5);
        assert_eq!(out, b"hello");
        assert!(matches!(err, Some(Error::Underlying(_))), "{err:?}");

        // The error arrives while the trailer block is read.
        let items: Vec<Item> = vec![Ok(Bytes::from_static(b"0\r\nx-amz-a:1")), Err("boom".into())];
        let (out, err) = decode(items, 0);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::Underlying(_))), "{err:?}");
    }

    #[test]
    fn trims_whitespace_around_trailer_values() {
        let body = b"0\r\nx-amz-meta-a: 1 \t\r\nx-amz-meta-b:\t2\r\n\r\n";
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, Limits::default());
        let handle = stream.trailer_handle();
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(err.is_none(), "{err:?}");
        let headers = handle.take().expect("trailers are ready");
        assert_eq!(headers.get("x-amz-meta-a").expect("header"), "1");
        assert_eq!(headers.get("x-amz-meta-b").expect("header"), "2");
    }

    #[test]
    fn enforces_the_metadata_limit_while_flushing_a_partial_line() {
        let limits = Limits {
            max_chunk_meta_size: 4,
            ..Limits::default()
        };
        // The first fragment carries no newline, so the limit is enforced when
        // the carry is flushed into the metadata buffer.
        let items: Vec<Item> = vec![
            Ok(Bytes::from_static(b"00000")),
            Ok(Bytes::from_static(b"5\r\nhello\r\n0\r\n\r\n")),
        ];
        let stream = ChunkedStream::unsigned(futures::stream::iter(items), 5, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::ChunkMetaTooLarge(5, 4))), "{err:?}");
    }

    #[test]
    fn reports_eof_before_the_terminal_chunk() {
        let (out, err) = decode(single(b""), 5);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::Incomplete)), "{err:?}");
    }

    #[test]
    fn accepts_a_trailer_block_without_a_trailing_newline() {
        let body = b"0\r\nx-amz-a:1";
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(body)), 0, Limits::default());
        let handle = stream.trailer_handle();
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(err.is_none(), "{err:?}");
        let headers = handle.take().expect("trailers are ready");
        assert_eq!(headers.get("x-amz-a").expect("header"), "1");
    }

    #[test]
    fn rejects_a_chunk_signature_in_unsigned_mode() {
        let seed = seed_signature();
        let signature = chunk_signature(&seed, b"hello");
        let mut body = signed_chunk_line(5, &signature);
        body.extend_from_slice(b"hello\r\n");
        body.extend_from_slice(b"0\r\n\r\n");

        let (out, err) = decode(single(&body), 5);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::FormatError)));
    }

    #[test]
    fn reports_underlying_stream_errors() {
        let items: Vec<Item> = vec![Err("boom".into())];
        let (out, err) = decode(items, 0);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::Underlying(_))));
    }

    #[test]
    fn exposes_length_and_size_hint() {
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"5\r\nhello\r\n0\r\n\r\n")), 5, Limits::default());
        assert_eq!(stream.exact_remaining_length(), 5);
        assert_eq!(Stream::size_hint(&stream), (0, None));
        let (out, err) = drain(stream);
        assert_eq!(out, b"hello");
        assert!(err.is_none());
    }

    #[test]
    fn into_inner_returns_the_input_stream() {
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"")), 0, Limits::default());
        let _inner = stream.into_inner();
    }
}

#[cfg(test)]
mod signed_tests {
    use super::*;
    use crate::test_utils::{
        Item, chunk_signature, drain, fragmented, seed_signature, sign_context, signed_chunk_line, single, trailer_signature,
    };

    /// Builds a signed body with the chunks `hello` and `123`, plus the final
    /// zero chunk, and returns the last chunk signature.
    fn signed_body() -> (Vec<u8>, Sha256Sum) {
        let seed = seed_signature();
        let first = chunk_signature(&seed, b"hello");
        let second = chunk_signature(&first, b"123");
        let last = chunk_signature(&second, b"");

        let mut body = signed_chunk_line(5, &first);
        body.extend_from_slice(b"hello\r\n");
        body.extend_from_slice(&signed_chunk_line(3, &second));
        body.extend_from_slice(b"123\r\n");
        body.extend_from_slice(&signed_chunk_line(0, &last));
        (body, last)
    }

    fn decode(items: Vec<Item>, decoded_content_length: usize) -> (Vec<u8>, Option<Error>) {
        let stream = ChunkedStream::signed(
            futures::stream::iter(items),
            sign_context(),
            seed_signature(),
            decoded_content_length,
            Limits::default(),
        );
        drain(stream)
    }

    #[test]
    fn decodes_a_signed_body_with_arbitrary_fragmentation() {
        let (body, _) = signed_body();
        for size in [body.len(), 1, 3, 7, 16] {
            let (out, err) = decode(fragmented(&body, size), 8);
            assert_eq!(out, b"hello123", "fragment size {size}");
            assert!(err.is_none(), "fragment size {size}: {err:?}");
        }
        let (out, err) = decode(single(&body), 8);
        assert_eq!(out, b"hello123");
        assert!(err.is_none());
    }

    #[test]
    fn requires_chunk_signatures() {
        let (out, err) = decode(single(b"5\r\nhello\r\n0\r\n\r\n"), 5);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::FormatError)));
    }

    #[test]
    fn verifies_before_yielding() {
        let seed = seed_signature();
        let wrong = chunk_signature(&seed, b"HELLO");
        let mut body = signed_chunk_line(5, &wrong);
        body.extend_from_slice(b"hello\r\n");
        body.extend_from_slice(b"0\r\n\r\n");

        let (out, err) = decode(single(&body), 5);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::SignatureMismatch)));
    }

    #[test]
    fn requires_the_trailer_signature() {
        let (mut body, _) = signed_body();
        body.extend_from_slice(b"x-amz-checksum-crc32:AAAAAA==\r\n\r\n");
        let (out, err) = decode(single(&body), 8);
        assert_eq!(out, b"hello123");
        assert!(matches!(err, Some(Error::FormatError)));
    }

    #[test]
    fn trims_whitespace_before_verifying_the_trailer_signature() {
        let (mut body, last) = signed_body();

        // The canonical form trims the value, so the signature covers "1".
        let canonical = b"x-amz-meta-a:1\n";
        let signature = trailer_signature(&last, canonical);
        let mut buf = [0_u8; 64];
        let hex = signature.to_hex(&mut buf);
        body.extend_from_slice(format!("x-amz-meta-a: 1 \r\nx-amz-trailer-signature:{hex}\r\n\r\n").as_bytes());

        let (out, err) = decode(single(&body), 8);
        assert_eq!(out, b"hello123");
        assert!(err.is_none(), "{err:?}");
    }

    #[test]
    fn verifies_the_trailer_signature() {
        let (mut body, last) = signed_body();
        let canonical = b"x-amz-checksum-crc32:AAAAAA==\n";
        let signature = trailer_signature(&last, canonical);
        let mut buf = [0_u8; 64];
        let hex = signature.to_hex(&mut buf);
        body.extend_from_slice(format!("x-amz-checksum-crc32:AAAAAA==\r\nx-amz-trailer-signature:{hex}\r\n\r\n").as_bytes());

        let stream = ChunkedStream::signed(
            futures::stream::iter(single(&body)),
            sign_context(),
            seed_signature(),
            8,
            Limits::default(),
        );
        let handle = stream.trailer_handle();
        let (out, err) = drain(stream);
        assert_eq!(out, b"hello123");
        assert!(err.is_none(), "{err:?}");
        assert!(handle.is_ready());
        let headers = handle.take().expect("trailers are ready");
        assert_eq!(headers.get("x-amz-checksum-crc32").expect("header"), "AAAAAA==");
        assert!(headers.get("x-amz-trailer-signature").is_none());
    }

    #[test]
    fn rejects_a_tampered_trailer_signature() {
        let (mut body, last) = signed_body();
        let canonical = b"x-amz-checksum-crc32:AAAAAA==\n";
        let wrong = trailer_signature(&chunk_signature(&last, b"x"), canonical);
        let mut buf = [0_u8; 64];
        let hex = wrong.to_hex(&mut buf);
        body.extend_from_slice(format!("x-amz-checksum-crc32:AAAAAA==\r\nx-amz-trailer-signature:{hex}\r\n\r\n").as_bytes());

        let (out, err) = decode(single(&body), 8);
        assert_eq!(out, b"hello123");
        assert!(matches!(err, Some(Error::SignatureMismatch)));
    }

    #[test]
    fn enforces_the_signed_chunk_limit() {
        let limits = Limits {
            max_signed_chunk_size: 4,
            ..Limits::default()
        };
        let (body, _) = signed_body();
        let stream = ChunkedStream::signed(futures::stream::iter(single(&body)), sign_context(), seed_signature(), 8, limits);
        let (_, err) = drain(stream);
        assert!(matches!(err, Some(Error::ChunkDataTooLarge(5, 4))), "{err:?}");
    }

    #[test]
    fn accepts_the_signed_chunk_limit_at_the_boundary() {
        // The first chunk is 5 bytes, so 5 is the largest accepted limit.
        let limits = Limits {
            max_signed_chunk_size: 5,
            ..Limits::default()
        };
        let (body, _) = signed_body();
        let stream = ChunkedStream::signed(futures::stream::iter(single(&body)), sign_context(), seed_signature(), 8, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"hello123");
        assert!(err.is_none(), "{err:?}");

        let limits = Limits {
            max_signed_chunk_size: 4,
            ..Limits::default()
        };
        let stream = ChunkedStream::signed(futures::stream::iter(single(&body)), sign_context(), seed_signature(), 8, limits);
        let (out, err) = drain(stream);
        assert_eq!(out, b"");
        assert!(matches!(err, Some(Error::ChunkDataTooLarge(5, 4))), "{err:?}");
    }

    #[test]
    fn reports_a_short_decoded_length() {
        let (body, _) = signed_body();
        let (out, err) = decode(single(&body), 9);
        assert_eq!(out, b"hello123");
        assert!(matches!(err, Some(Error::Incomplete)));
    }

    #[test]
    fn reports_an_overrun_decoded_length() {
        let (body, _) = signed_body();
        let (out, err) = decode(single(&body), 7);
        // The overrun is detected when the offending fragment is emitted.
        assert_eq!(out, b"hello");
        assert!(matches!(err, Some(Error::LengthMismatch)));
    }

    #[test]
    fn exposes_length_and_size_hint() {
        let (body, _) = signed_body();
        let stream = ChunkedStream::signed(
            futures::stream::iter(single(&body)),
            sign_context(),
            seed_signature(),
            8,
            Limits::default(),
        );
        assert_eq!(stream.exact_remaining_length(), 8);
        assert_eq!(Stream::size_hint(&stream), (0, None));
        let (out, err) = drain(stream);
        assert_eq!(out, b"hello123");
        assert!(err.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{chunk_signature, drain, seed_signature, sign_context, signed_chunk_line, single};

    #[test]
    fn dispatches_unsigned_and_signed_modes() {
        let unsigned_body = b"5\r\nhello\r\n0\r\n\r\n".to_vec();
        let unsigned = ChunkedStream::unsigned(futures::stream::iter(single(&unsigned_body)), 5, Limits::default());
        assert_eq!(unsigned.exact_remaining_length(), 5);
        assert_eq!(Stream::size_hint(&unsigned), (0, None));
        let (out, err) = drain(unsigned);
        assert_eq!(out, b"hello");
        assert!(err.is_none());

        let seed = seed_signature();
        let signature = chunk_signature(&seed, b"hello");
        let last = chunk_signature(&signature, b"");
        let mut signed_body = signed_chunk_line(5, &signature);
        signed_body.extend_from_slice(b"hello\r\n");
        signed_body.extend_from_slice(&signed_chunk_line(0, &last));
        let signed =
            ChunkedStream::signed(futures::stream::iter(single(&signed_body)), sign_context(), seed, 5, Limits::default());
        assert_eq!(signed.exact_remaining_length(), 5);
        let (out, err) = drain(signed);
        assert_eq!(out, b"hello");
        assert!(err.is_none());
    }

    #[test]
    fn into_inner_returns_the_input_stream() {
        let stream = ChunkedStream::unsigned(futures::stream::iter(single(b"")), 0, Limits::default());
        let _inner = stream.into_inner();
    }
}

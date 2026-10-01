// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! aws-chunked stream
//!
//! Thin adapter over [`s3s_chunked::ChunkedStream`]: the historical public shape
//! of `AwsChunkedStream` (constructor, error type, trailing-headers handle) is
//! preserved while decoding and signature verification live in the
//! `s3s-chunked` crate.
//!
//! # Checksum trailers
//!
//! `x-amz-checksum-*` trailer values are exposed via [`TrailingHeaders`]
//! without verification: comparing them against a digest computed while
//! consuming the body is the responsibility of the [`S3`](crate::S3)
//! implementation (the reference implementation `s3s-fs` rejects mismatches
//! with `BadDigest`). The unsigned mode
//! (`STREAMING-UNSIGNED-PAYLOAD-TRAILER`) relies on those values for data
//! integrity, so implementations that skip checksum verification accept
//! unauthenticated data — a trade-off `s3s` leaves to the implementation.

use crate::auth::SecretKey;
use crate::error::{S3ErrorCode, StdError};
use crate::protocol::TrailingHeaders;
use crate::stream::{ByteStream, DynByteStream, RemainingLength};
use crate::utils::crypto::Sha256Sum;

use bytes::Bytes;
use futures::Stream;
use s3s_chunked::{ChunkedStream, Limits, SignContext, TrailerHandle};
use s3s_sigv4::AmzDate;
use std::fmt::{self, Debug};
use std::pin::Pin;
use std::task::{Context, Poll};

/// The input stream type-erased for the non-generic public struct.
///
/// The body is boxed and pinned so that any `S: Stream + Send + Sync + 'static`
/// can be stored without an `Unpin` bound on the public constructor.
type BoxedBody = Pin<Box<dyn Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static>>;

/// # Compatibility
///
/// The decoder behind this stream replaces the previous implementation, and three
/// behaviours changed with it:
///
/// - A chunk or trailer signature in a request declared unsigned is a format error:
///   an unsigned declaration carries no signing context, so such a signature cannot be
///   verified. The previous implementation verified a signature whenever one was present.
/// - A chunk metadata line above the limit is [`AwsChunkedStreamError::ChunkMetaTooLarge`],
///   which maps to `EntityTooLarge` (400); it used to surface as an internal error
///   (500).
/// - The stream fails fast: after an error item it yields `None` for ever instead of
///   resuming.
///
/// Aws chunked stream
pub struct AwsChunkedStream {
    /// The decoding state machine.
    inner: ChunkedStream<BoxedBody>,

    /// Verified trailing headers.
    trailers: TrailerHandle,
}

impl Debug for AwsChunkedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsChunkedStream").finish_non_exhaustive()
    }
}

/// [`AwsChunkedStream`]
#[derive(Debug, thiserror::Error)]
pub enum AwsChunkedStreamError {
    /// Underlying error
    #[error("AwsChunkedStreamError: Underlying: {}",.0)]
    Underlying(#[source] StdError),
    /// Signature mismatch
    #[error("AwsChunkedStreamError: SignatureMismatch")]
    SignatureMismatch,
    /// Format error
    #[error("AwsChunkedStreamError: FormatError")]
    FormatError,
    /// Incomplete stream
    #[error("AwsChunkedStreamError: Incomplete")]
    Incomplete,
    /// More bytes produced than the declared decoded length
    #[error("AwsChunkedStreamError: LengthMismatch")]
    LengthMismatch,
    /// Chunk metadata too large
    #[error("AwsChunkedStreamError: ChunkMetaTooLarge: size {0} exceeds limit {1}")]
    ChunkMetaTooLarge(usize, usize),
    /// Chunk data too large
    #[error("AwsChunkedStreamError: ChunkDataTooLarge: size {0} exceeds limit {1}")]
    ChunkDataTooLarge(usize, usize),
    /// Trailers too large
    #[error("AwsChunkedStreamError: TrailersTooLarge: size {0} exceeds limit {1}")]
    TrailersTooLarge(usize, usize),
    /// Too many trailer headers
    #[error("AwsChunkedStreamError: TooManyTrailerHeaders: count {0} exceeds limit {1}")]
    TooManyTrailerHeaders(usize, usize),
}

impl AwsChunkedStreamError {
    /// Maps a stream-verification error to the `S3` error code a conforming
    /// implementation should report.
    #[must_use]
    pub fn to_s3_error_code(&self) -> S3ErrorCode {
        match self {
            Self::Underlying(_) => S3ErrorCode::InternalError,
            Self::SignatureMismatch => S3ErrorCode::SignatureDoesNotMatch,
            Self::FormatError
            | Self::Incomplete
            | Self::LengthMismatch
            | Self::TrailersTooLarge(..)
            | Self::TooManyTrailerHeaders(..) => S3ErrorCode::IncompleteBody,
            Self::ChunkMetaTooLarge(..) | Self::ChunkDataTooLarge(..) => S3ErrorCode::EntityTooLarge,
        }
    }
}

impl From<s3s_chunked::Error> for AwsChunkedStreamError {
    fn from(error: s3s_chunked::Error) -> Self {
        use s3s_chunked::Error;
        match error {
            Error::Underlying(error) => Self::Underlying(error),
            Error::SignatureMismatch => Self::SignatureMismatch,
            Error::FormatError => Self::FormatError,
            Error::Incomplete => Self::Incomplete,
            Error::LengthMismatch => Self::LengthMismatch,
            Error::ChunkMetaTooLarge(size, limit) => Self::ChunkMetaTooLarge(size, limit),
            Error::ChunkDataTooLarge(size, limit) => Self::ChunkDataTooLarge(size, limit),
            Error::TrailersTooLarge(size, limit) => Self::TrailersTooLarge(size, limit),
            Error::TooManyTrailerHeaders(count, limit) => Self::TooManyTrailerHeaders(count, limit),
        }
    }
}

impl AwsChunkedStream {
    /// Constructs a `ChunkedStream`
    #[allow(clippy::too_many_arguments)]
    pub fn new<S>(
        body: S,
        seed_signature: Sha256Sum,
        amz_date: AmzDate,
        region: Box<str>,
        service: Box<str>,
        secret_key: SecretKey,
        decoded_content_length: usize,
        unsigned: bool,
        max_chunk_size: usize,
    ) -> Self
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        let limits = Limits {
            max_signed_chunk_size: max_chunk_size,
            ..Limits::default()
        };
        let body: BoxedBody = Box::pin(body);

        let inner = if unsigned {
            // An unsigned declaration carries no signing context, so any
            // signature in the body is rejected as a format error instead of
            // being verified.
            // Nothing here derives a signing key the decoder will not use.
            drop(secret_key);
            ChunkedStream::unsigned(body, decoded_content_length, limits)
        } else {
            // The signing key is derived and zeroized inside `SignContext`; the
            // secret itself is no longer needed.
            let sign = SignContext::new(amz_date, region, service, secret_key.expose().as_bytes());
            drop(secret_key);
            let seed = s3s_chunked::Sha256Sum::from_bytes(*seed_signature.as_bytes());
            ChunkedStream::signed(body, sign, seed, decoded_content_length, limits)
        };
        let trailers = inner.trailer_handle();

        Self { inner, trailers }
    }

    /// Returns the declared decoded length minus the bytes produced so far.
    #[must_use]
    pub fn exact_remaining_length(&self) -> usize {
        self.inner.exact_remaining_length()
    }

    /// Converts this stream into a dynamic byte stream.
    #[must_use]
    pub fn into_byte_stream(self) -> DynByteStream {
        crate::stream::into_dyn(self)
    }

    // Note: Trailing headers should be accessed via trailing_headers_handle().

    /// Get a handle to access verified trailing headers later.
    ///
    /// This can be cloned and stored outside to retrieve trailers after the
    /// stream has been fully read.
    pub(crate) fn trailing_headers_handle(&self) -> TrailingHeaders {
        TrailingHeaders::new(self.trailers.clone())
    }
}

impl Stream for AwsChunkedStream {
    type Item = Result<Bytes, AwsChunkedStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(AwsChunkedStreamError::from(error)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl ByteStream for AwsChunkedStream {
    fn remaining_length(&self) -> RemainingLength {
        RemainingLength::new_exact(self.inner.exact_remaining_length())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SEED: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
    const TIMESTAMP: &str = "20130524T000000Z";
    const REGION: &str = "us-east-1";
    const SERVICE: &str = "s3";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    use futures::StreamExt as _;
    use hyper::http::HeaderValue;
    const MAX_CHUNK_META_SIZE: usize = s3s_chunked::Limits::DEFAULT_MAX_CHUNK_META_SIZE;
    const MAX_TRAILER_HEADERS: usize = s3s_chunked::Limits::DEFAULT_MAX_TRAILER_HEADERS;
    const MAX_TRAILERS_SIZE: usize = s3s_chunked::Limits::DEFAULT_MAX_TRAILERS_SIZE;
    use s3s_sigv4::create_trailer_string_to_sign;

    #[test]
    fn to_s3_error_code_maps_all_variants() {
        let cases = [
            (
                AwsChunkedStreamError::Underlying(Box::new(std::io::Error::other("boom"))),
                S3ErrorCode::InternalError,
            ),
            (AwsChunkedStreamError::SignatureMismatch, S3ErrorCode::SignatureDoesNotMatch),
            (AwsChunkedStreamError::FormatError, S3ErrorCode::IncompleteBody),
            (AwsChunkedStreamError::Incomplete, S3ErrorCode::IncompleteBody),
            (AwsChunkedStreamError::LengthMismatch, S3ErrorCode::IncompleteBody),
            (AwsChunkedStreamError::ChunkMetaTooLarge(1, 2), S3ErrorCode::EntityTooLarge),
            (AwsChunkedStreamError::ChunkDataTooLarge(1, 2), S3ErrorCode::EntityTooLarge),
            (AwsChunkedStreamError::TrailersTooLarge(1, 2), S3ErrorCode::IncompleteBody),
            (AwsChunkedStreamError::TooManyTrailerHeaders(1, 2), S3ErrorCode::IncompleteBody),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_s3_error_code(), expected, "{err:?}");
        }
    }

    #[tokio::test]
    async fn unsigned_chunk_streams_fragments_incrementally() {
        use futures::SinkExt as _;

        // The chunk meta declares 6 bytes but the data arrives in two fragments.
        // The first fragment must be yielded before the rest is even sent.
        let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, StdError>>(4);

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let date = AmzDate::parse("20130524T000000Z").unwrap();

        let mut chunked_stream = AwsChunkedStream::new(
            rx,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            6,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        tx.send(Ok(Bytes::from_static(b"6\r\nabc"))).await.unwrap();

        let first = chunked_stream.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"abc");

        tx.send(Ok(Bytes::from_static(b"def\r\n0\r\n\r\n"))).await.unwrap();
        drop(tx);

        let second = chunked_stream.next().await.unwrap().unwrap();
        assert_eq!(second.as_ref(), b"def");

        assert!(chunked_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn unsigned_chunk_streams_crlf_split_across_fragments() {
        use futures::SinkExt as _;

        // The chunk data exactly fills the first fragment, so the trailing CRLF
        // arrives later and must be consumed byte-by-byte.
        let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, StdError>>(4);

        let mut chunked_stream = AwsChunkedStream::new(
            rx,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            6,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        tx.send(Ok(Bytes::from_static(b"6\r\nabcdef"))).await.unwrap();

        let data = chunked_stream.next().await.unwrap().unwrap();
        assert_eq!(data.as_ref(), b"abcdef");

        tx.send(Ok(Bytes::from_static(b"\r\n0\r\n\r\n"))).await.unwrap();
        drop(tx);

        assert!(chunked_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn unsigned_chunk_streaming_error_paths() {
        use futures::SinkExt as _;

        let build = |rx| {
            AwsChunkedStream::new(
                rx,
                Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                AmzDate::parse("20130524T000000Z").unwrap(),
                "us-east-1".into(),
                "s3".into(),
                "test-key".into(),
                6,
                true, // unsigned
                crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
            )
        };

        {
            // Underlying error while waiting for the rest of the chunk data:
            // the received fragment is yielded first, then the error surfaces.
            let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, StdError>>(4);
            let mut stream = build(rx);
            tx.send(Ok(Bytes::from_static(b"6\r\nabc"))).await.unwrap();
            tx.send(Err(Box::new(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom"))))
                .await
                .unwrap();

            let first = stream.next().await.unwrap().unwrap();
            assert_eq!(first.as_ref(), b"abc");
            let err = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(err, AwsChunkedStreamError::Underlying(_)));
        }

        {
            // Malformed trailing CRLF: the chunk data is yielded, then FormatError.
            let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, StdError>>(4);
            let mut stream = build(rx);
            tx.send(Ok(Bytes::from_static(b"6\r\nabcdefXX"))).await.unwrap();

            let data = stream.next().await.unwrap().unwrap();
            assert_eq!(data.as_ref(), b"abcdef");
            let err = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(err, AwsChunkedStreamError::FormatError));
        }

        {
            // Underlying error while waiting for the trailing CRLF.
            let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, StdError>>(4);
            let mut stream = build(rx);
            tx.send(Ok(Bytes::from_static(b"6\r\nabcdef"))).await.unwrap();
            tx.send(Err(Box::new(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom"))))
                .await
                .unwrap();

            let data = stream.next().await.unwrap().unwrap();
            assert_eq!(data.as_ref(), b"abcdef");
            let err = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(err, AwsChunkedStreamError::Underlying(_)));
        }
    }

    #[tokio::test]
    async fn signed_chunk_propagates_underlying_error() {
        // In signed mode the whole chunk is collected before verification,
        // so an underlying error surfaces before any data is yielded.
        let chunk_meta = b"100;chunk-signature=0000000000000000000000000000000000000000000000000000000000000000\r\n";
        let chunk_data = b"short";
        let chunk1 = join(&[chunk_meta, chunk_data]);

        let cause = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom");
        let err: Result<Bytes, StdError> = Err(Box::new(cause));
        let chunk_results = vec![Ok(chunk1), err];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            256,
            false, // signed
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::Underlying(_)))));
    }

    #[tokio::test]
    async fn signed_chunk_rejects_oversized_declaration() {
        // Declares 16 bytes but the limit is 8; the signed path buffers whole
        // chunks, so the declaration is rejected before any data is read.
        let chunk_meta = b"10;chunk-signature=0000000000000000000000000000000000000000000000000000000000000000\r\n";
        let chunk_data = b"short";
        let chunk1 = join(&[chunk_meta, chunk_data]);
        let chunk_results = vec![Ok(chunk1)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            16,
            false,
            8,
        );

        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::ChunkDataTooLarge(16, 8)))));
    }

    #[tokio::test]
    async fn unsigned_chunk_accepts_oversized_declaration() {
        // Unsigned chunks are streamed without buffering and are not subject to the limit.
        let chunk1 = join(&[b"10\r\n", b"0123456789abcdef\r\n", b"0\r\n\r\n"]);
        let chunk_results = vec![Ok(chunk1)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            16,
            true, // unsigned
            8,
        );

        let data = chunked_stream.next().await.unwrap().unwrap();
        assert_eq!(data.as_ref(), b"0123456789abcdef");
        assert!(chunked_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn unsigned_chunk_overrun_rejected_when_declaration_exceeded() {
        // The decoded-length declaration says 4 bytes but the chunk carries 6:
        // structurally valid, yet the produced bytes exceed the declaration.
        let chunk1 = join(&[b"6\r\n", b"abcdef\r\n", b"0\r\n\r\n"]);
        let chunk_results = vec![Ok(chunk1)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            4, // decoded_content_length declaration
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        // The whole chunk is streamed in one poll, so the overrun is reported
        // on the first read.
        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::LengthMismatch))));
    }

    #[tokio::test]
    async fn unsigned_chunk_shortfall_rejected_when_declaration_not_met() {
        // The decoded-length declaration says 8 bytes but the chunk carries 4
        // and the stream ends cleanly with a 0-size chunk.
        let chunk1 = join(&[b"4\r\n", b"abcd\r\n", b"0\r\n\r\n"]);
        let chunk_results = vec![Ok(chunk1)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            8, // decoded_content_length declaration
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let first = chunked_stream.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"abcd");

        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::Incomplete))));
    }

    fn join(bytes: &[&[u8]]) -> Bytes {
        let mut buf = Vec::new();
        for b in bytes {
            buf.extend_from_slice(b);
        }
        buf.into()
    }

    #[tokio::test]
    async fn example_put_object_chunked_stream() {
        let chunk1_meta = b"10000;chunk-signature=ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648\r\n";
        let chunk2_meta = b"400;chunk-signature=0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497\r\n";
        let chunk3_meta = b"0;chunk-signature=b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9\r\n";

        let chunk1_data = vec![b'a'; 0x10000]; // 65536
        let chunk2_data = vec![b'a'; 1024];
        let chunk3_data = [];
        let decoded_content_length = chunk1_data.len() + chunk2_data.len() + chunk3_data.len();

        let chunk1 = join(&[chunk1_meta, &chunk1_data, b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, &chunk2_data, b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, &chunk3_data, b"\r\n"]);

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3)];

        let seed_signature = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            false,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        {
            assert!(chunked_stream.next().await.is_none());
            assert!(chunked_stream.next().await.is_none());
            assert!(chunked_stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn example_put_object_chunked_stream_with_trailers() {
        // Example from AWS docs: https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-streaming-trailers.html
        // Seed signature corresponds to STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER canonical request
        let chunk1_meta = b"10000;chunk-signature=b474d8862b1487a5145d686f57f013e54db672cee1c953b3010fb58501ef5aa2\r\n";
        let chunk2_meta = b"400;chunk-signature=1c1344b170168f8e65b41376b44b20fe354e373826ccbbe2c1d40a8cae51e5c7\r\n";
        let chunk3_meta = b"0;chunk-signature=2ca2aba2005185cf7159c6277faf83795951dd77a3a99e6e65d5c9f85863f992\r\n";

        let chunk1_data = vec![b'a'; 0x10000]; // 65536
        let chunk2_data = vec![b'a'; 1024];
        let chunk3_data = [];
        let decoded_content_length = chunk1_data.len() + chunk2_data.len() + chunk3_data.len();

        let chunk1 = join(&[chunk1_meta, &chunk1_data, b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, &chunk2_data, b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, &chunk3_data, b"\r\n"]);

        let trailers_block = Bytes::from_static(b"x-amz-checksum-crc32c:sOO8/Q==\r\nx-amz-trailer-signature:d81f82fc3505edab99d459891051a732e8730629a2e4a59689829ca17fe2e435");

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3), Ok(trailers_block)];

        let seed_signature = "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            false,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        // No more data after verifying trailers
        assert!(chunked_stream.next().await.is_none());

        // Export trailers via handle
        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 1);
        let v = trailers.get("x-amz-checksum-crc32c").unwrap();
        assert_eq!(v, &HeaderValue::from_static("sOO8/Q=="));
    }

    #[tokio::test]
    async fn unsigned_payload_with_trailer_minimal() {
        // Construct a minimal unsigned aws-chunked stream: two data chunks and a 0 chunk, then trailers block.
        // Here we compute signatures using existing test vectors in s3s_sigv4::methods.rs indirectly by using
        // a known seed and the create_trailer_string_to_sign path inside AwsChunkedStream.

        // For unsigned per-chunk mode, meta lines are plain sizes without extensions.
        let chunk1_meta = b"3\r\n"; // size 3
        let chunk2_meta = b"4\r\n"; // size 4
        let chunk3_meta = b"0\r\n"; // last chunk

        let chunk1_data = b"abc";
        let chunk2_data = b"defg";
        let decoded_content_length = chunk1_data.len() + chunk2_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, chunk2_data.as_ref(), b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, b"\r\n"]); // 0-size chunk is followed by CRLF before trailers

        // Build trailers. We'll compute the expected trailer signature using the same algorithm as the stream.
        let seed_signature = "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e"; // from example seed for trailer flows
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        // Canonical trailers: one additional header besides x-amz-trailer-signature
        let canonical = b"x-amz-meta-foo:bar\n".to_vec();
        let string_to_sign = create_trailer_string_to_sign(&date, region, service, seed_signature, &canonical);
        let sig = s3s_sigv4::calculate_signature(&string_to_sign, secret_access_key, &date, region, service);
        let trailers_block = Bytes::from(format!("x-amz-meta-foo: bar\r\nx-amz-trailer-signature:{}", sig.as_str()));

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3), Ok(trailers_block)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        // No more data after verifying trailers
        // A signature in an unsigned declaration is rejected, not verified:
        // the decoder has no signing context to check it with.
        let next = chunked_stream.next().await;
        assert!(matches!(next, Some(Err(AwsChunkedStreamError::FormatError))), "unexpected: {next:?}");
        assert!(chunked_stream.trailing_headers_handle().take().is_none(), "trailers are not published");

        // The rejected trailer block publishes nothing, so there is no handle
        // to read here any more (the historical behaviour verified the
        // signature and exposed x-amz-meta-foo).
    }

    #[tokio::test]
    async fn unsigned_payload_with_trailer_no_signature() {
        // unsigned mode with trailers present but no x-amz-trailer-signature
        let chunk1_meta = b"3\r\n"; // size 3
        let chunk2_meta = b"0\r\n"; // last chunk

        let chunk1_data = b"xyz";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Trailers without signature
        let trailers_block = Bytes::from_static(b"x-amz-meta-a: 1\r\nx-amz-meta-b: 2");

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(trailers_block)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000"; // not used for unsigned per-chunk, but used to build ctx
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // No more data after verifying trailers (which contain no signature in unsigned mode)
        assert!(chunked_stream.next().await.is_none());

        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 2);
        assert_eq!(trailers.get("x-amz-meta-a").unwrap(), &HeaderValue::from_static("1"));
        assert_eq!(trailers.get("x-amz-meta-b").unwrap(), &HeaderValue::from_static("2"));
    }

    #[tokio::test]
    async fn test_format_error_invalid_chunk_size() {
        // Test FormatError when chunk size is not valid hex
        let chunk_meta = b"ZZZZ\r\n"; // Invalid hex
        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(Bytes::from_static(chunk_meta))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            0,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await.unwrap();
        assert!(matches!(result, Err(AwsChunkedStreamError::FormatError)));
    }

    #[tokio::test]
    async fn test_format_error_missing_crlf() {
        // Test FormatError when chunk metadata doesn't end with CRLF
        let chunk_meta = b"10"; // Missing \r\n
        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(Bytes::from_static(chunk_meta))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            16,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        // Stream ends without proper chunk metadata; the declared decoded
        // length was never delivered, so the stream reports an incomplete
        // upload instead of ending silently.
        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::Incomplete))));
    }

    #[tokio::test]
    async fn test_signature_mismatch() {
        // Test SignatureMismatch when chunk signature is wrong
        let chunk_meta = b"5;chunk-signature=0000000000000000000000000000000000000000000000000000000000000000\r\n";
        let chunk_data = b"hello";
        let chunk1 = join(&[chunk_meta, chunk_data, b"\r\n"]);
        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            5,
            false, // signed mode
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await.unwrap();
        assert!(matches!(result, Err(AwsChunkedStreamError::SignatureMismatch)));
    }

    #[tokio::test]
    async fn test_incomplete_stream() {
        // Test Incomplete when stream ends before all data is received
        let chunk_meta = b"100\r\n"; // Expects 256 bytes
        let chunk_data = b"short"; // But only sends 5 bytes
        let chunk1 = join(&[chunk_meta, chunk_data]); // No closing \r\n, stream will end

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            256,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await;
        // In unsigned mode, received fragments are yielded before the truncation is detected.
        assert!(matches!(result, Some(Ok(_))));
        let result = chunked_stream.next().await;
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::Incomplete))));
    }

    #[tokio::test]
    async fn test_incomplete_stream_signed() {
        // Test Incomplete when stream ends before all data is received.
        // In signed mode, the whole chunk is collected before verification,
        // so the error surfaces before any data is yielded.
        let chunk_meta = b"100;chunk-signature=0000000000000000000000000000000000000000000000000000000000000000\r\n";
        let chunk_data = b"short"; // But only sends 5 bytes
        let chunk1 = join(&[chunk_meta, chunk_data]); // No closing \r\n, stream will end

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            256,
            false,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await;
        // Should fail because stream ends unexpectedly
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::Incomplete))));
    }

    #[tokio::test]
    async fn test_underlying_error() {
        // Test Underlying error propagation
        use std::io;
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "network error");
        let err: Result<Bytes, StdError> = Err(Box::new(cause));
        let chunk_results: Vec<Result<Bytes, _>> = vec![err];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            0,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await.unwrap();
        assert!(matches!(result, Err(AwsChunkedStreamError::Underlying(_))));

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "AwsChunkedStreamError: Underlying: network error");

        // The cause chain must stay intact so that callers can classify the failure,
        // for example to tell a client disconnect apart from a server-side fault.
        let err: &dyn std::error::Error = &err;
        let source = err.source().expect("the underlying error should be exposed as the source");
        let kind = source.downcast_ref::<io::Error>().map(io::Error::kind);
        assert_eq!(kind, Some(io::ErrorKind::ConnectionReset));
    }

    #[tokio::test]
    async fn test_signed_mode_requires_signature() {
        // Test that signed mode (unsigned=false) requires chunk signatures
        let chunk_meta = b"5\r\n"; // No signature extension
        let chunk_data = b"hello";
        let chunk1 = join(&[chunk_meta, chunk_data, b"\r\n"]);
        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            5,
            false, // signed mode - should require signatures
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let result = chunked_stream.next().await.unwrap();
        assert!(matches!(result, Err(AwsChunkedStreamError::FormatError)));
    }

    #[tokio::test]
    async fn test_trailer_signature_in_unsigned_mode_is_rejected() {
        // Test trailer signature verification failure
        let chunk_meta = b"3\r\n";
        let chunk_data = b"abc";
        let final_chunk = b"0\r\n\r\n";
        let trailers_block = b"x-amz-checksum-crc32c:test\r\nx-amz-trailer-signature:0000000000000000000000000000000000000000000000000000000000000000";

        let chunk1 = join(&[chunk_meta, chunk_data, b"\r\n"]);
        let chunk2 = join(&[final_chunk, trailers_block]);

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            3,
            true, // unsigned chunks but signed trailer
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap().as_ref(), chunk_data);

        let result = chunked_stream.next().await;
        // Should fail with signature mismatch in trailer
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::FormatError))));
    }

    #[tokio::test]
    async fn test_trailer_format_error_invalid_header() {
        // Test FormatError when trailer headers are malformed
        let chunk_meta = b"3\r\n";
        let chunk_data = b"abc";
        let final_chunk = b"0\r\n\r\n";
        let trailers_block = b"invalid-header-without-colon\r\n"; // Malformed header

        let chunk1 = join(&[chunk_meta, chunk_data, b"\r\n"]);
        let chunk2 = join(&[final_chunk, trailers_block]);

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2)];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            "us-east-1".into(),
            "s3".into(),
            "test-key".into(),
            3,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap().as_ref(), chunk_data);

        let result = chunked_stream.next().await;
        // Should fail with format error
        assert!(matches!(result, Some(Err(AwsChunkedStreamError::FormatError))));
    }

    #[tokio::test]
    #[allow(clippy::assertions_on_constants)]
    async fn test_limits_constants_exist() {
        // This test verifies that the limit constants are defined and have reasonable values
        assert!(MAX_CHUNK_META_SIZE > 0);
        assert!(MAX_CHUNK_META_SIZE < 10 * 1024); // Should be reasonable, less than 10KB
        assert!(MAX_TRAILERS_SIZE > 0);
        assert!(MAX_TRAILERS_SIZE < 100 * 1024); // Should be reasonable, less than 100KB
        assert!(MAX_TRAILER_HEADERS > 0);
        assert!(MAX_TRAILER_HEADERS < 1000); // Should be reasonable
    }

    #[tokio::test]
    async fn test_normal_sized_trailers_work() {
        // Verify that normal-sized trailers work fine (well within limits)
        let chunk1_meta = b"3\r\n";
        let chunk2_meta = b"0\r\n";

        let chunk1_data = b"abc";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Create trailers with reasonable number of headers (50, well under limit of 100)
        let mut trailers = Vec::new();
        for i in 0..50 {
            trailers.extend_from_slice(format!("x-amz-meta-{i}: value{i}\r\n").as_bytes());
        }

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(Bytes::from(trailers))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // Should complete successfully
        assert!(chunked_stream.next().await.is_none());

        // Verify trailers were parsed
        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 50);
    }

    #[tokio::test]
    async fn test_chunk_meta_too_large() {
        // Test that chunk metadata exceeding MAX_CHUNK_META_SIZE (1KB) triggers an error
        // We create a chunk with an extremely long hex size that doesn't contain a newline
        // Split across multiple stream chunks to trigger the accumulation logic

        let meta_part1 = vec![b'f'; 600]; // First part of oversized hex number
        let meta_part2 = vec![b'f'; 600]; // Second part - together they exceed 1KB

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(Bytes::from(meta_part1)), Ok(Bytes::from(meta_part2))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            0,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        // Should get an error due to meta size limit
        let result = chunked_stream.next().await;
        assert!(result.is_some());
        // The size limit is classified directly: the historical wrapper
        // (`Underlying(ChunkMetaTooLarge)`) is gone, so the S3 error code is
        // `EntityTooLarge` (400) instead of `InternalError` (500).
        let error = result.unwrap().unwrap_err();
        assert_eq!(error.to_s3_error_code(), S3ErrorCode::EntityTooLarge);
        assert!(matches!(error, AwsChunkedStreamError::ChunkMetaTooLarge(_, _)));
    }

    #[tokio::test]
    async fn test_trailers_too_large() {
        // Test that the limit for MAX_TRAILERS_SIZE (16KB) prevents unbounded allocation
        // This test creates trailers that would exceed the limit and verifies the code
        // handles them safely without crashing or allocating unbounded memory.
        let chunk1_meta = b"3\r\n";
        let chunk2_meta = b"0\r\n";

        let chunk1_data = b"abc";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Create trailers that exceed MAX_TRAILERS_SIZE (16KB)
        // Each header is about 53 bytes, so 400 headers = ~21KB > 16KB limit
        let mut large_trailers = Vec::new();
        for i in 0..400 {
            large_trailers.extend_from_slice(format!("x-amz-meta-header-{i}: {}\r\n", "x".repeat(30)).as_bytes());
        }

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(Bytes::from(large_trailers))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        // Read the chunk data
        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // The limit prevents unbounded memory allocation during trailer parsing
        // Stream should return an error when limit is exceeded
        let mut error_found = false;
        while let Some(result) = chunked_stream.next().await {
            match result {
                Err(AwsChunkedStreamError::TrailersTooLarge(size, limit)) => {
                    assert_eq!(limit, MAX_TRAILERS_SIZE);
                    assert!(size > MAX_TRAILERS_SIZE);
                    error_found = true;
                    break;
                }
                Err(AwsChunkedStreamError::Underlying(e)) => {
                    // Error might be wrapped in Underlying
                    if let Some(AwsChunkedStreamError::TrailersTooLarge(size, limit)) = e.downcast_ref::<AwsChunkedStreamError>()
                    {
                        assert_eq!(*limit, MAX_TRAILERS_SIZE);
                        assert!(*size > MAX_TRAILERS_SIZE);
                        error_found = true;
                        break;
                    }
                    // If not the expected error, continue
                }
                Ok(_) | Err(_) => {
                    // Continue consuming stream or skip other errors
                }
            }
        }

        // Either we found the error, or trailers weren't stored (both indicate limit was enforced)
        if !error_found {
            // Verify no trailers were stored (parsing failed due to size limit)
            let handle = chunked_stream.trailing_headers_handle();
            let trailers = handle.take();
            assert!(
                trailers.is_none() || trailers.unwrap().is_empty(),
                "Trailers should not be stored when they exceed limits"
            );
        }
    }

    #[tokio::test]
    async fn test_too_many_trailer_headers() {
        // Test that the limit for MAX_TRAILER_HEADERS (100) prevents unbounded allocation
        // This test creates more headers than allowed and verifies the code
        // handles them safely without crashing or allocating unbounded memory.
        let chunk1_meta = b"3\r\n";
        let chunk2_meta = b"0\r\n";

        let chunk1_data = b"abc";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Create more than MAX_TRAILER_HEADERS (100) headers - use 150
        let mut many_trailers = Vec::new();
        for i in 0..150 {
            many_trailers.extend_from_slice(format!("x-amz-meta-{i}: value\r\n").as_bytes());
        }

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(Bytes::from(many_trailers))];

        let seed_signature = "0000000000000000000000000000000000000000000000000000000000000000";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(seed_signature).unwrap(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        // Read the chunk data
        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // The limit prevents unbounded memory allocation during trailer parsing
        // Stream should return an error when limit is exceeded
        let mut error_found = false;
        while let Some(result) = chunked_stream.next().await {
            match result {
                Err(AwsChunkedStreamError::TooManyTrailerHeaders(count, limit)) => {
                    assert_eq!(limit, MAX_TRAILER_HEADERS);
                    assert!(count > MAX_TRAILER_HEADERS);
                    error_found = true;
                    break;
                }
                Err(AwsChunkedStreamError::Underlying(e)) => {
                    // Error might be wrapped in Underlying
                    if let Some(AwsChunkedStreamError::TooManyTrailerHeaders(count, limit)) =
                        e.downcast_ref::<AwsChunkedStreamError>()
                    {
                        assert_eq!(*limit, MAX_TRAILER_HEADERS);
                        assert!(*count > MAX_TRAILER_HEADERS);
                        error_found = true;
                        break;
                    }
                    // If not the expected error, continue
                }
                Ok(_) | Err(_) => {
                    // Continue consuming stream or skip other errors
                }
            }
        }

        // Either we found the error, or trailers weren't stored (both indicate limit was enforced)
        if !error_found {
            // Verify no trailers were stored (parsing failed due to header count limit)
            let handle = chunked_stream.trailing_headers_handle();
            let trailers = handle.take();
            assert!(
                trailers.is_none() || trailers.unwrap().is_empty(),
                "Trailers should not be stored when header count exceeds limits"
            );
        }
    }

    #[test]
    fn propagates_pending_from_the_body() {
        let mut inner = Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(b"3\r\nabc\r\n0\r\n\r\n"))]));
        let mut pending_once = true;
        let body = futures::stream::poll_fn(move |cx| {
            if pending_once {
                pending_once = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            inner.as_mut().poll_next(cx)
        });

        let mut stream = AwsChunkedStream::new(
            body,
            Sha256Sum::from_hex(SEED).unwrap(),
            AmzDate::parse(TIMESTAMP).unwrap(),
            REGION.into(),
            SERVICE.into(),
            SECRET_KEY.into(),
            3,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let payload = futures::executor::block_on(async {
            let mut payload = Vec::new();
            while let Some(item) = stream.next().await {
                payload.extend_from_slice(&item.unwrap());
            }
            payload
        });
        assert_eq!(payload, b"abc");
    }

    #[test]
    fn forwards_the_exact_remaining_length() {
        let body = futures::stream::iter(vec![Ok(Bytes::from_static(b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n"))]);
        let mut stream = AwsChunkedStream::new(
            body,
            Sha256Sum::from_hex(SEED).unwrap(),
            AmzDate::parse(TIMESTAMP).unwrap(),
            REGION.into(),
            SERVICE.into(),
            SECRET_KEY.into(),
            6,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        assert_eq!(stream.exact_remaining_length(), 6);
        let first = futures::executor::block_on(stream.next()).unwrap().unwrap();
        assert_eq!(first.as_ref(), b"abc");
        assert_eq!(stream.exact_remaining_length(), 3);
    }

    #[test]
    fn debug_reports_the_adapter_and_the_handle() {
        let body = futures::stream::iter(Vec::<Result<Bytes, crate::error::StdError>>::new());
        let stream = AwsChunkedStream::new(
            body,
            Sha256Sum::from_hex(SEED).unwrap(),
            AmzDate::parse(TIMESTAMP).unwrap(),
            REGION.into(),
            SERVICE.into(),
            SECRET_KEY.into(),
            0,
            true,
            crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
        );

        let text = format!("{stream:?}");
        assert!(text.starts_with("AwsChunkedStream"), "{text}");
        let handle = stream.trailing_headers_handle();
        let text = format!("{handle:?}");
        assert!(text.starts_with("TrailingHeaders"), "{text}");
    }
}

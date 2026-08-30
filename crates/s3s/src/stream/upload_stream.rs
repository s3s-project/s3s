// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
//! `UploadStream`: a body wrapper that verifies a payload SHA-256 while
//! forwarding bytes, used to enforce the signed body hash of single-chunk
//! `SigV4` uploads.

use crate::crypto::Checksum as _;
use crate::crypto::Sha256;
use crate::error::{S3ErrorCode, StdError};
use crate::stream::{ByteStream, DynByteStream, RemainingLength};
use crate::utils::crypto::Sha256Sum;

use bytes::Bytes;
use futures::Stream;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use thiserror::Error;

/// Stream wrapper that verifies payload SHA-256 while forwarding bytes.
pub struct UploadStream<S> {
    inner: S,
    hasher: Option<Sha256>,
    expected_sha256: Sha256Sum,
    remaining_length: usize,
    state: State,
}

impl<S> fmt::Debug for UploadStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadStream")
            .field("remaining_length", &self.remaining_length)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Reading,
    AwaitingEof,
    Finished,
    Failed,
}

/// Errors produced by [`UploadStream`].
#[derive(Debug, Error)]
pub enum UploadStreamError {
    /// Underlying stream error.
    #[error("UploadStreamError: Underlying: {0}")]
    Underlying(#[source] StdError),
    /// Payload SHA-256 mismatch.
    #[error("UploadStreamError: Sha256Mismatch")]
    Sha256Mismatch,
    /// More bytes received than declared.
    #[error("UploadStreamError: LengthMismatch")]
    LengthMismatch,
    /// Stream ended before reading declared length.
    #[error("UploadStreamError: Incomplete")]
    Incomplete,
}

impl UploadStreamError {
    /// Maps a stream-verification error to the `S3` error code a conforming
    /// implementation should report.
    #[must_use]
    pub fn to_s3_error_code(&self) -> S3ErrorCode {
        match self {
            Self::Underlying(_) => S3ErrorCode::InternalError,
            Self::Sha256Mismatch => S3ErrorCode::BadDigest,
            Self::LengthMismatch | Self::Incomplete => S3ErrorCode::IncompleteBody,
        }
    }
}

impl<S> UploadStream<S> {
    /// Creates a new [`UploadStream`] with the provided expected checksum.
    pub fn new(inner: S, length: usize, expected_sha256: Sha256Sum) -> Self {
        Self {
            inner,
            hasher: Some(Sha256::new()),
            expected_sha256,
            remaining_length: length,
            state: State::Reading,
        }
    }

    /// Converts this stream into a dynamic byte stream.
    pub fn into_byte_stream(self) -> DynByteStream
    where
        Self: Sized + ByteStream + Stream<Item = Result<Bytes, UploadStreamError>> + Send + Sync + Unpin + 'static,
    {
        crate::stream::into_dyn(self)
    }

    /// Finalizes the hash and transitions to [`State::AwaitingEof`] when all declared bytes are read.
    ///
    /// Returns `Ok(true)` if the state was updated to `AwaitingEof`, `Ok(false)` if there are still
    /// bytes remaining, and returns an error while moving to `Failed` if hash finalization fails.
    fn finalize_if_complete(&mut self) -> Result<bool, UploadStreamError> {
        if self.remaining_length != 0 {
            return Ok(false);
        }

        match self.finalize_hash() {
            Ok(()) => {
                self.state = State::AwaitingEof;
                Ok(true)
            }
            Err(err) => {
                self.state = State::Failed;
                Err(err)
            }
        }
    }

    fn finalize_hash(&mut self) -> Result<(), UploadStreamError> {
        let Some(hasher) = self.hasher.take() else {
            return Ok(());
        };

        let digest = Sha256Sum::from_bytes(hasher.finalize());
        if digest == self.expected_sha256 {
            Ok(())
        } else {
            Err(UploadStreamError::Sha256Mismatch)
        }
    }
}

impl<S> Stream for UploadStream<S>
where
    S: Stream<Item = Result<Bytes, StdError>> + Unpin,
{
    type Item = Result<Bytes, UploadStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.state {
                // The normal data path: consume incoming chunks while we still expect bytes.
                // Transitions:
                //   Reading --(remaining_length == 0 && hash ok)--> AwaitingEof
                //   Reading --(remaining_length == 0 && hash mismatch)--> Failed
                //   Reading --(len > remaining_length)--> Failed
                //   Reading --(inner error/EOF too early)--> Failed
                //   Reading --(emit bytes)--> Reading (with smaller remaining_length)
                State::Reading => {
                    match self.finalize_if_complete() {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(err) => return Poll::Ready(Some(Err(err))),
                    }

                    match Pin::new(&mut self.inner).poll_next(cx) {
                        Poll::Ready(Some(Ok(bytes))) => {
                            if bytes.is_empty() {
                                continue;
                            }

                            let len = bytes.len();
                            if len > self.remaining_length {
                                self.state = State::Failed;
                                return Poll::Ready(Some(Err(UploadStreamError::LengthMismatch)));
                            }

                            if let Some(hasher) = &mut self.hasher {
                                hasher.update(bytes.as_ref());
                            }

                            self.remaining_length -= len;

                            match self.finalize_if_complete() {
                                Ok(true | false) => {}
                                Err(err) => return Poll::Ready(Some(Err(err))),
                            }

                            return Poll::Ready(Some(Ok(bytes)));
                        }
                        Poll::Ready(Some(Err(err))) => {
                            self.state = State::Failed;
                            return Poll::Ready(Some(Err(UploadStreamError::Underlying(err))));
                        }
                        Poll::Ready(None) => {
                            self.state = State::Failed;
                            return Poll::Ready(Some(Err(UploadStreamError::Incomplete)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                // All declared bytes were read and the digest matched; now any extra payload
                // is an error. We only accept EOF (transition to Finished).
                State::AwaitingEof => match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Ready(Some(Ok(bytes))) => {
                        if bytes.is_empty() {
                            continue;
                        }
                        self.state = State::Failed;
                        return Poll::Ready(Some(Err(UploadStreamError::LengthMismatch)));
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = State::Failed;
                        return Poll::Ready(Some(Err(UploadStreamError::Underlying(err))));
                    }
                    Poll::Ready(None) => {
                        self.state = State::Finished;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => return Poll::Pending,
                },
                // Terminal states: once Finished or Failed we stop polling the inner stream
                // and always return `None` to the caller.
                State::Finished | State::Failed => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl<S> ByteStream for UploadStream<S>
where
    S: Stream<Item = Result<Bytes, StdError>> + Unpin,
{
    fn remaining_length(&self) -> RemainingLength {
        RemainingLength::new_exact(self.remaining_length)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    use futures::StreamExt as _;
    use std::io;

    #[test]
    fn to_s3_error_code_maps_all_variants() {
        let cases = [
            (UploadStreamError::Sha256Mismatch, S3ErrorCode::BadDigest),
            (UploadStreamError::LengthMismatch, S3ErrorCode::IncompleteBody),
            (UploadStreamError::Incomplete, S3ErrorCode::IncompleteBody),
            (
                UploadStreamError::Underlying(Box::new(io::Error::other("boom"))),
                S3ErrorCode::InternalError,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_s3_error_code(), expected, "{err:?}");
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn ok_bytes(data: &'static [u8]) -> Result<Bytes, StdError> {
        Ok(Bytes::from_static(data))
    }

    #[tokio::test]
    async fn single_chunk_success() {
        let data = b"hello world";
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(data));
        let stream = futures::stream::iter(vec![ok_bytes(data)]);

        let mut upload = UploadStream::new(stream, data.len(), checksum);

        let chunk = upload.next().await.unwrap().unwrap();
        assert_eq!(chunk.as_ref(), data);
        assert!(upload.next().await.is_none());
    }

    #[tokio::test]
    async fn sha256_mismatch() {
        let data = b"hello";
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(b"world"));
        let stream = futures::stream::iter(vec![ok_bytes(data)]);

        let mut upload = UploadStream::new(stream, data.len(), checksum);

        let err = upload.next().await.unwrap().unwrap_err();
        assert!(matches!(err, UploadStreamError::Sha256Mismatch));
    }

    #[tokio::test]
    async fn length_mismatch_extra_bytes() {
        let data = b"abcdef";
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(data));
        let chunks = vec![ok_bytes(b"abc"), ok_bytes(b"def")];
        let stream = futures::stream::iter(chunks);

        let mut upload = UploadStream::new(stream, 5, checksum);

        let first = upload.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"abc");

        let err = upload.next().await.unwrap().unwrap_err();
        assert!(matches!(err, UploadStreamError::LengthMismatch));
    }

    #[tokio::test]
    async fn incomplete_stream() {
        let data = b"abcdef";
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(data));
        let chunks = vec![ok_bytes(b"abc")];
        let stream = futures::stream::iter(chunks);

        let mut upload = UploadStream::new(stream, data.len(), checksum);

        let first = upload.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"abc");

        let err = upload.next().await.unwrap().unwrap_err();
        assert!(matches!(err, UploadStreamError::Incomplete));
    }

    #[tokio::test]
    async fn zero_length_success() {
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(b""));
        let stream = futures::stream::iter(Vec::<Result<Bytes, StdError>>::new());

        let mut upload = UploadStream::new(stream, 0, checksum);

        assert!(upload.next().await.is_none());
    }

    #[tokio::test]
    async fn extra_payload_after_completion() {
        let data = b"abc";
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(data));
        let chunks = vec![ok_bytes(data), ok_bytes(b"extra")];
        let stream = futures::stream::iter(chunks);

        let mut upload = UploadStream::new(stream, data.len(), checksum);

        let first = upload.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), data);

        let err = upload.next().await.unwrap().unwrap_err();
        assert!(matches!(err, UploadStreamError::LengthMismatch));
    }

    #[tokio::test]
    async fn propagate_underlying_error() {
        let checksum = Sha256Sum::from_bytes(Sha256::checksum(b""));
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "boom");
        let err: Result<Bytes, StdError> = Err(Box::new(cause));
        let stream = futures::stream::iter(vec![err]);

        let mut upload = UploadStream::new(stream, 0, checksum);

        let err = upload.next().await.unwrap().unwrap_err();
        assert!(matches!(err, UploadStreamError::Underlying(_)));
        assert_eq!(err.to_string(), "UploadStreamError: Underlying: boom");

        // The cause chain must stay intact so that callers can classify the failure,
        // for example to tell a client disconnect apart from a server-side fault.
        let err: &dyn std::error::Error = &err;
        let source = err.source().expect("the underlying error should be exposed as the source");
        let kind = source.downcast_ref::<io::Error>().map(io::Error::kind);
        assert_eq!(kind, Some(io::ErrorKind::ConnectionReset));
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// Errors produced while parsing a multipart body.
///
/// The enum is `#[non_exhaustive]`: more variants may be added in a minor
/// release, so downstream `match` expressions need a wildcard arm. Existing
/// variants stay constructible.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The underlying stream returned an error.
    #[error("stream read failed: {0}")]
    StreamReadFailed(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The supplied boundary is invalid.
    #[error("invalid boundary")]
    InvalidBoundary,
    /// The body is not valid multipart data.
    #[error("invalid multipart format")]
    InvalidFormat,
    /// The stream ended before the expected multipart structure.
    #[error("incomplete multipart stream")]
    IncompleteStream,
    /// The accumulated part headers exceed the configured buffer limit.
    #[error("part headers exceed the buffer limit of {limit} bytes")]
    HeaderSizeExceeded {
        /// The configured buffer limit that was exceeded.
        limit: usize,
    },
    /// A [`FinalPartDataStream`](crate::FinalPartDataStream) found content
    /// after the closing delimiter that is not the strict closing trailer
    /// (an epilogue or another part).
    #[error("content follows the closing delimiter of the taken part")]
    StreamPartNotLast,
    /// The multipart parser was already terminated by `take_data_stream`.
    #[error("multipart stream has already been taken")]
    StreamAlreadyTaken,
    /// A taken part data stream ended before the closing delimiter.
    #[error("incomplete streaming part")]
    IncompleteStreamPart,
}

impl Error {
    /// Wraps an error returned by the underlying stream.
    ///
    /// Equivalent to constructing [`Error::StreamReadFailed`] directly, with
    /// the source boxed for you; convenient as a `map_err` function.
    #[must_use]
    pub fn stream_read_failed(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::StreamReadFailed(source.into())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn display_messages_are_stable() {
        assert_eq!(Error::InvalidBoundary.to_string(), "invalid boundary");
        assert_eq!(Error::InvalidFormat.to_string(), "invalid multipart format");
        assert_eq!(Error::IncompleteStream.to_string(), "incomplete multipart stream");
        assert_eq!(Error::IncompleteStreamPart.to_string(), "incomplete streaming part");
        assert_eq!(
            Error::StreamPartNotLast.to_string(),
            "content follows the closing delimiter of the taken part"
        );
        assert_eq!(Error::StreamAlreadyTaken.to_string(), "multipart stream has already been taken");
        assert_eq!(
            Error::HeaderSizeExceeded { limit: 8192 }.to_string(),
            "part headers exceed the buffer limit of 8192 bytes"
        );
        assert_eq!(
            Error::StreamReadFailed(Box::new(std::io::Error::other("boom"))).to_string(),
            "stream read failed: boom"
        );
    }

    #[test]
    fn only_stream_failures_expose_a_source() {
        let err = Error::StreamReadFailed(Box::new(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")));
        let kind = err
            .source()
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(std::io::ErrorKind::UnexpectedEof));

        for err in [
            Error::InvalidBoundary,
            Error::InvalidFormat,
            Error::IncompleteStream,
            Error::IncompleteStreamPart,
            Error::StreamPartNotLast,
            Error::StreamAlreadyTaken,
            Error::HeaderSizeExceeded { limit: 1 },
        ] {
            assert!(err.source().is_none());
        }
    }

    #[test]
    fn error_is_send_sync_static() {
        fn assert_bounds<T: std::error::Error + Send + Sync + 'static>() {}
        assert_bounds::<Error>();
    }
}

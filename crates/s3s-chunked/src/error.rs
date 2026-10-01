// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::utils::StdError;

/// An `aws-chunked` decoding error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying byte stream failed.
    #[error("underlying stream error: {0}")]
    Underlying(#[source] StdError),

    /// A chunk signature does not match the expected signature.
    #[error("chunk signature mismatch")]
    SignatureMismatch,

    /// The stream is not valid `aws-chunked` framing.
    #[error("malformed aws-chunked stream")]
    FormatError,

    /// The stream ended before the payload was complete.
    #[error("incomplete aws-chunked stream")]
    Incomplete,

    /// More decoded bytes were produced than the declared decoded length.
    #[error("decoded length mismatch")]
    LengthMismatch,

    /// The chunk metadata line exceeds the configured limit.
    #[error("chunk metadata size {0} exceeds limit {1}")]
    ChunkMetaTooLarge(usize, usize),

    /// A signed chunk exceeds the configured limit.
    #[error("chunk data size {0} exceeds limit {1}")]
    ChunkDataTooLarge(usize, usize),

    /// The trailer block exceeds the configured limit.
    #[error("trailers size {0} exceeds limit {1}")]
    TrailersTooLarge(usize, usize),

    /// The trailer block contains too many headers.
    #[error("trailer header count {0} exceeds limit {1}")]
    TooManyTrailerHeaders(usize, usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_stable() {
        assert_eq!(Error::SignatureMismatch.to_string(), "chunk signature mismatch");
        assert_eq!(Error::FormatError.to_string(), "malformed aws-chunked stream");
        assert_eq!(Error::Incomplete.to_string(), "incomplete aws-chunked stream");
        assert_eq!(Error::LengthMismatch.to_string(), "decoded length mismatch");
        assert_eq!(
            Error::ChunkMetaTooLarge(1025, 1024).to_string(),
            "chunk metadata size 1025 exceeds limit 1024"
        );
        assert_eq!(Error::ChunkDataTooLarge(9, 8).to_string(), "chunk data size 9 exceeds limit 8");
        assert_eq!(Error::TrailersTooLarge(9, 8).to_string(), "trailers size 9 exceeds limit 8");
        assert_eq!(
            Error::TooManyTrailerHeaders(101, 100).to_string(),
            "trailer header count 101 exceeds limit 100"
        );
    }

    #[test]
    fn underlying_error_keeps_its_source() {
        let err = Error::Underlying("boom".into());
        assert!(std::error::Error::source(&err).is_some());
        assert_eq!(err.to_string(), "underlying stream error: boom");
    }
}

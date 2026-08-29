// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use s3s::S3Error;
use s3s::S3ErrorCode;
use s3s::StdError;

use std::panic::Location;

use tracing::error;

#[derive(Debug)]
pub struct Error {
    source: StdError,
}

pub type Result<T = (), E = Error> = std::result::Result<T, E>;

impl Error {
    #[must_use]
    #[track_caller]
    pub fn new(source: StdError) -> Self {
        log(&*source);
        Self { source }
    }

    #[must_use]
    #[track_caller]
    pub fn from_string(s: impl Into<String>) -> Self {
        Self::new(s.into().into())
    }
}

impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[track_caller]
    fn from(source: E) -> Self {
        Self::new(Box::new(source))
    }
}

impl From<Error> for S3Error {
    fn from(e: Error) -> Self {
        let source = e.source;

        // Stream-verification errors from `s3s` (payload checksum, chunk
        // signature, decoded length) carry a precise `S3` error code; map them
        // so clients get `BadDigest` / `IncompleteBody` / `SignatureDoesNotMatch`
        // instead of a generic 500.
        if let Some(err) = source.downcast_ref::<s3s::stream::upload_stream::UploadStreamError>() {
            return S3Error::with_source(err.to_s3_error_code(), source);
        }
        if let Some(err) = source.downcast_ref::<s3s::stream::aws_chunked_stream::AwsChunkedStreamError>() {
            return S3Error::with_source(err.to_s3_error_code(), source);
        }

        S3Error::with_source(S3ErrorCode::InternalError, source)
    }
}

#[inline]
#[track_caller]
pub(crate) fn log(source: &dyn std::error::Error) {
    if cfg!(feature = "binary") {
        let location = Location::caller();
        let span_trace = tracing_error::SpanTrace::capture();

        error!(
            target: "s3s_fs_internal_error",
            %location,
            error=%source,
            "span trace:\n{span_trace}"
        );
    }
}

macro_rules! try_ {
    ($result:expr) => {
        match $result {
            Ok(val) => val,
            Err(err) => {
                $crate::error::log(&err);
                return Err(::s3s::S3Error::internal_error(err));
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_upload_stream_errors_to_s3_error_codes() {
        let e = Error::new(Box::new(s3s::stream::upload_stream::UploadStreamError::Sha256Mismatch));
        let s3err: S3Error = e.into();
        assert_eq!(s3err.code(), &S3ErrorCode::BadDigest);
    }

    #[test]
    fn maps_aws_chunked_stream_errors_to_s3_error_codes() {
        let e = Error::new(Box::new(s3s::stream::aws_chunked_stream::AwsChunkedStreamError::SignatureMismatch));
        let s3err: S3Error = e.into();
        assert_eq!(s3err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    }

    #[test]
    fn keeps_internal_error_for_unrecognized_sources() {
        let e = Error::new(Box::new(std::io::Error::other("boom")));
        let s3err: S3Error = e.into();
        assert_eq!(s3err.code(), &S3ErrorCode::InternalError);
    }
}

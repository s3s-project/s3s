// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Error mapping shared by the `MinIO` SDK delegates.

use minio::s3::error::Error as MinioError;
use minio::s3::error::S3ServerError;

use s3s::S3Error;
use s3s::S3ErrorCode;
use s3s::s3_error;

/// Maps an error returned by the `minio` crate to an `S3Error`.
///
/// The crate wraps non-2xx responses into `S3Server` errors: an error the
/// backend reported keeps its S3 error code, message and request id, so the s3s
/// protocol derives the correct HTTP status; a transport-level failure keeps the
/// upstream HTTP status. The remaining cases are internal errors described by
/// `context`.
pub(super) fn map_error(context: &str, e: MinioError) -> S3Error {
    match e {
        MinioError::S3Server(S3ServerError::S3Error(e)) => {
            let mut err = S3Error::new(S3ErrorCode::InternalError);
            if let Some(code) = S3ErrorCode::from_bytes(e.code().to_string().as_bytes()) {
                err.set_code(code);
            }
            if let Some(message) = e.message() {
                err.set_message(message.clone());
            }
            err.set_request_id(e.request_id().to_owned());
            err
        }
        MinioError::S3Server(S3ServerError::InvalidServerResponse { http_status_code, .. }) => {
            let mut err = s3_error!(InternalError, "invalid upstream response");
            err.set_status_code(
                hyper::StatusCode::from_u16(http_status_code).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR),
            );
            err
        }
        MinioError::S3Server(S3ServerError::HttpError(status, _)) => {
            let mut err = s3_error!(InternalError, "upstream http error");
            err.set_status_code(hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR));
            err
        }
        e => s3_error!(e, InternalError, "{context}: {e}"),
    }
}

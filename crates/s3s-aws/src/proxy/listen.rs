// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `ListenBucketNotification` is a `MinIO` extension with no aws-sdk-s3
//! counterpart, so the generated proxy cannot forward it. It is forwarded
//! through the official `MinIO` SDK (`minio` crate) instead, streaming the
//! upstream response body through untouched.
//!
//! The generated proxy delegates to [`listen_bucket_notification`].

use hyper::HeaderMap;
use hyper::header::HeaderValue;
use minio::s3::MinioClient;
use minio::s3::multimap_ext::{Multimap, MultimapExt};
use minio::s3::types::ToS3Request;

use s3s::dto::{ListenBucketNotificationInput, ListenBucketNotificationOutput, StreamingBlob};
use s3s::{S3Result, s3_error};

pub async fn listen_bucket_notification(
    minio: &MinioClient,
    req: s3s::S3Request<ListenBucketNotificationInput>,
) -> S3Result<s3s::S3Response<ListenBucketNotificationOutput>> {
    let input = req.input;
    tracing::debug!(?input);

    // All parameters go through `extra_query_params` so that the typed
    // builder chain stays unconditional. Repeated `events` keys are the
    // wire format MinIO expects (clients like `mc watch` send one key per
    // event name), so the comma-joined DTO value is split back.
    let mut extra_params = Multimap::default();
    if let Some(ref events) = input.events {
        for event in events.split(',') {
            extra_params.add("events", event);
        }
    }
    if let Some(ref prefix) = input.prefix {
        extra_params.add("prefix", prefix);
    }
    if let Some(ref suffix) = input.suffix {
        extra_params.add("suffix", suffix);
    }

    let builder = minio
        .listen_bucket_notification(input.bucket.as_str())
        .map_err(|e| s3_error!(e, InternalError, "invalid bucket"))?;
    let value = builder.extra_query_params(extra_params).build();
    let mut s3_request = value
        .to_s3request()
        .map_err(|e| s3_error!(e, InternalError, "failed to build request"))?;
    let resp = match s3_request.execute().await {
        Ok(resp) => resp,
        // minio-rs wraps non-2xx responses into `S3Server` errors. Map the
        // MinIO error code to an S3 error code so the s3s protocol derives
        // the correct HTTP status.
        Err(minio::s3::error::Error::S3Server(minio::s3::error::S3ServerError::S3Error(e))) => {
            let mut err = s3s::S3Error::new(s3s::S3ErrorCode::InternalError);
            if let Some(code) = s3s::S3ErrorCode::from_bytes(e.code().to_string().as_bytes()) {
                err.set_code(code);
            }
            if let Some(message) = e.message() {
                err.set_message(message.clone());
            }
            err.set_request_id(e.request_id().to_owned());
            return Err(err);
        }
        Err(minio::s3::error::Error::S3Server(minio::s3::error::S3ServerError::InvalidServerResponse {
            http_status_code,
            ..
        })) => {
            let mut err = s3_error!(InternalError, "invalid upstream response");
            err.set_status_code(
                hyper::StatusCode::from_u16(http_status_code).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR),
            );
            return Err(err);
        }
        Err(minio::s3::error::Error::S3Server(minio::s3::error::S3ServerError::HttpError(status, _))) => {
            let mut err = s3_error!(InternalError, "upstream http error");
            err.set_status_code(hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR));
            return Err(err);
        }
        Err(e) => return Err(s3_error!(e, InternalError, "failed to send request: {e}")),
    };

    let status = resp.status();
    if !status.is_success() {
        let message = resp.text().await.unwrap_or_else(|_| String::from("<no response body>"));
        let mut err = s3_error!(InternalError, "upstream error: {status}: {message}");
        err.set_status_code(status);
        return Err(err);
    }

    let mut headers = HeaderMap::new();
    for name in [hyper::header::CONTENT_TYPE, hyper::header::CACHE_CONTROL] {
        if let Some(value) = resp.headers().get(&name) {
            let value =
                HeaderValue::from_bytes(value.as_bytes()).map_err(|e| s3_error!(e, InternalError, "invalid upstream header"))?;
            headers.insert(name, value);
        }
    }

    let stream = resp.bytes_stream();
    let payload = StreamingBlob::wrap(stream);

    Ok(s3s::S3Response::with_headers(
        ListenBucketNotificationOutput { payload: Some(payload) },
        headers,
    ))
}

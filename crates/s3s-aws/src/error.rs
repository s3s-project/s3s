// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use hyper::HeaderMap;
use hyper::StatusCode;
use hyper::header::{ETAG, LAST_MODIFIED};

macro_rules! wrap_sdk_error {
    ($e:expr) => {{
        use aws_sdk_s3::error::SdkError;
        use aws_sdk_s3::operation::RequestId;
        use s3s::{S3Error, S3ErrorCode};

        let mut err = S3Error::new(S3ErrorCode::InternalError);
        let source = $e;
        tracing::debug!("sdk error: {:?}", source);

        if let SdkError::ServiceError(ref e) = source {
            let meta = e.err().meta();
            if let Some(val) = meta.code().and_then(|s| S3ErrorCode::from_bytes(s.as_bytes())) {
                err.set_code(val);
            }
            if let Some(val) = meta.message() {
                err.set_message(val.to_owned());
            }
            if let Some(val) = meta.request_id() {
                err.set_request_id(val);
            }
            crate::error::SetStatusCode(&mut err, e).call();
        }
        err.set_source(Box::new(source));

        err
    }};
}

// FIXME: this is actually an overloaded function

pub struct SetStatusCode<'a, 'b, E, R>(
    pub &'a mut s3s::S3Error,
    pub &'b aws_smithy_runtime_api::client::result::ServiceError<E, R>,
);

impl<E> SetStatusCode<'_, '_, E, aws_smithy_runtime_api::client::orchestrator::HttpResponse> {
    pub fn call(self) {
        let Self(err, e) = self;
        let status = hyper_status_code_from_aws(e.raw().status());
        // A successful raw status paired with an error body is a protocol
        // violation. It can only originate from a failed response
        // deserialization (e.g. `XmlDecodeError` on a 200 response), which
        // carries no error code; force 500 in that case instead of echoing
        // the 2xx status to the client.
        if status.is_success() {
            err.set_status_code(StatusCode::INTERNAL_SERVER_ERROR);
        } else {
            err.set_status_code(status);
        }
        if status == StatusCode::NOT_MODIFIED
            && let Some(headers) = not_modified_headers(e.raw().headers())
        {
            err.set_headers(headers);
        }
    }
}

impl<E> SetStatusCode<'_, '_, E, aws_smithy_types::event_stream::RawMessage> {
    #[allow(clippy::unused_self)]
    pub fn call(self) {}
}

fn hyper_status_code_from_aws(status_code: aws_smithy_runtime_api::http::StatusCode) -> hyper::StatusCode {
    hyper::StatusCode::from_u16(status_code.as_u16()).unwrap()
}

/// The response headers a `304 Not Modified` has to carry to the client.
///
/// A conditional read hit is answered with the entity tag the condition was matched against, and a
/// client caches on that tag. `aws-sdk-s3` models no output for a `304` (the SDK reports it as a
/// service error), so the headers travel on the [`s3s::S3Error`] the proxy returns;
/// `s3s::ops::serialize_error` writes them into the bodyless response.
///
/// Only entity headers are copied: a body-describing header (`content-length`, `content-type`)
/// describes the backend's body, which the client never receives, and a connection-specific header
/// belongs to the backend's connection.
fn not_modified_headers(headers: &aws_smithy_runtime_api::http::Headers) -> Option<HeaderMap> {
    let mut out = HeaderMap::new();
    for (name, target) in [("etag", ETAG), ("last-modified", LAST_MODIFIED)] {
        let Some(value) = headers.get(name) else {
            continue;
        };
        let Ok(value) = hyper::header::HeaderValue::from_str(value) else {
            continue;
        };
        out.insert(target, value);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::not_modified_headers;

    use aws_smithy_runtime_api::http::Headers;
    use hyper::header::{ETAG, LAST_MODIFIED};

    #[test]
    fn not_modified_headers_copies_the_entity_headers() {
        let mut headers = Headers::new();
        headers.insert("etag", "\"37b51d194a7513e45b56f6524f2d51f2\"");
        headers.insert("last-modified", "Wed, 07 Oct 2026 18:33:28 GMT");

        let copied = not_modified_headers(&headers).expect("the entity headers are copied");

        assert_eq!(
            copied.get(ETAG).and_then(|value| value.to_str().ok()),
            Some("\"37b51d194a7513e45b56f6524f2d51f2\"")
        );
        assert_eq!(
            copied.get(LAST_MODIFIED).and_then(|value| value.to_str().ok()),
            Some("Wed, 07 Oct 2026 18:33:28 GMT")
        );
    }

    /// Negative control: headers that describe the backend's body, or its connection, must not be
    /// copied, because the client never receives that body on this connection.
    #[test]
    fn not_modified_headers_drops_body_and_connection_headers() {
        let mut headers = Headers::new();
        headers.insert("content-length", "0");
        headers.insert("content-type", "application/xml");
        headers.insert("connection", "keep-alive");

        assert!(not_modified_headers(&headers).is_none());
    }
}

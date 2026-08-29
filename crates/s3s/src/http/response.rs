// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::HttpResponse;

use super::Body;

use hyper::HeaderMap;
use hyper::StatusCode;
use hyper::http::Extensions;

#[derive(Default)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
    pub extensions: Extensions,
}

impl From<Response> for HttpResponse {
    fn from(res: Response) -> Self {
        let mut ans = HttpResponse::default();
        *ans.status_mut() = res.status;
        *ans.headers_mut() = res.headers;
        *ans.body_mut() = res.body;
        *ans.extensions_mut() = res.extensions;
        ans
    }
}

impl Response {
    #[must_use]
    pub fn with_status(status: StatusCode) -> Self {
        Self {
            status,
            ..Default::default()
        }
    }
}

/// RFC 9110 §6.4.1: responses with a 1xx, 204, 205 or 304 status code MUST NOT
/// carry a body.
#[must_use]
pub fn is_bodyless_status(status: StatusCode) -> bool {
    status.is_informational() || matches!(status, StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED)
}

/// Clears the response body and removes `Transfer-Encoding`, keeping metadata
/// headers such as `Content-Length` and `Content-Type` intact. Used for `HEAD`
/// responses (RFC 9110 §9.3.2): the body must be suppressed, but the headers
/// may still describe the equivalent `GET` response.
pub fn strip_body(res: &mut Response) {
    res.headers.remove(hyper::header::TRANSFER_ENCODING);
    res.body = Body::empty();
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use hyper::header::HeaderValue;

    #[test]
    fn with_status_sets_status_and_preserves_default_state() {
        let res = Response::with_status(StatusCode::CREATED);

        assert_eq!(res.status, StatusCode::CREATED);
        assert!(res.headers.is_empty());
        assert!(http_body::Body::is_end_stream(&res.body));
        assert!(res.extensions.get::<usize>().is_none());
    }

    #[test]
    fn into_http_response_preserves_headers_body_and_extensions() {
        let mut res = Response::with_status(StatusCode::ACCEPTED);
        res.headers.insert("x-test", HeaderValue::from_static("value"));
        res.extensions.insert::<usize>(42);
        res.body = Body::from(Bytes::from_static(b"body"));

        let ans: HttpResponse = res.into();

        assert_eq!(ans.status(), StatusCode::ACCEPTED);
        assert_eq!(ans.headers().get("x-test").unwrap().to_str().unwrap(), "value");
        assert_eq!(ans.extensions().get::<usize>(), Some(&42));
        assert_eq!(http_body::Body::size_hint(ans.body()).exact(), Some(4));
    }

    #[test]
    fn is_bodyless_status_matches_rfc_9110() {
        assert!(is_bodyless_status(StatusCode::CONTINUE));
        assert!(is_bodyless_status(StatusCode::NO_CONTENT));
        assert!(is_bodyless_status(StatusCode::RESET_CONTENT));
        assert!(is_bodyless_status(StatusCode::NOT_MODIFIED));
        assert!(!is_bodyless_status(StatusCode::OK));
        assert!(!is_bodyless_status(StatusCode::NOT_FOUND));
        assert!(!is_bodyless_status(StatusCode::TEMPORARY_REDIRECT));
    }

    #[test]
    fn strip_body_clears_body_and_transfer_encoding_only() {
        let mut res = Response::with_status(StatusCode::NOT_FOUND);
        res.headers
            .insert(hyper::header::CONTENT_LENGTH, HeaderValue::from_static("123"));
        res.headers
            .insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
        res.headers
            .insert(hyper::header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        res.body = Body::from(Bytes::from_static(b"<Error/>"));

        strip_body(&mut res);

        assert!(http_body::Body::is_end_stream(&res.body));
        assert!(!res.headers.contains_key(hyper::header::TRANSFER_ENCODING));
        assert!(res.headers.contains_key(hyper::header::CONTENT_LENGTH));
        assert!(res.headers.contains_key(hyper::header::CONTENT_TYPE));
    }
}

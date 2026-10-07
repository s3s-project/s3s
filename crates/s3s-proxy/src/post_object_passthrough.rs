// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! POST Object passthrough: hands a form upload to the backend unread.
//!
//! A POST Object request authenticates with a policy that is signed *inside the
//! form*, and the core parses that form before a service is called — by the time
//! an implementation sees the request, the bytes it arrived with are gone. So
//! this passthrough runs ahead of the S3 service, at the HTTP layer, and
//! forwards the request as it arrived: the backend authenticates and validates
//! the form, and its answer goes back unchanged.
//!
//! The body is not read here. A form upload declares its own `Content-Length`,
//! so the forwarded request can declare the same length while the bytes are
//! still arriving — which is why this route does not reuse the buffered
//! forwarding the authentication passthrough needs.

use hyper::HeaderMap;
use hyper::Method;
use hyper::StatusCode;
use hyper::Uri;
use hyper::body::Incoming;
use hyper::header::CONTENT_LENGTH;
use hyper::header::CONTENT_TYPE;
use hyper::header::HOST;
use hyper::http::uri::PathAndQuery;
use s3s::Body;

use crate::auth_passthrough::backend_url;
use crate::auth_passthrough::error_response;
use crate::auth_passthrough::into_http;
use crate::auth_passthrough::send_to_backend;
use crate::hop_by_hop;

/// Forwards POST Object form uploads to the backend without reading them.
#[derive(Debug, Clone)]
pub(crate) struct PostObjectPassthrough {
    /// Backend base URL (e.g. `http://localhost:9000`).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
}

impl PostObjectPassthrough {
    /// Creates a passthrough forwarding POST Object requests to `endpoint_url`.
    #[must_use]
    pub(crate) fn new(endpoint_url: reqwest::Url, client: reqwest::Client) -> Self {
        Self { endpoint_url, client }
    }

    /// Whether the request is a POST Object form upload.
    ///
    /// The shape is `POST` with a `multipart/form-data` body. Every other POST
    /// stays on the typed path: multi-object delete carries `?delete`, and a POST
    /// that is not a form is not a form upload, so the core answers those.
    #[must_use]
    pub(crate) fn is_match(method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        if method != Method::POST {
            return false;
        }
        if uri
            .query()
            .is_some_and(|query| query.split('&').any(|pair| pair == "delete" || pair.starts_with("delete=")))
        {
            return false;
        }
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("multipart/form-data"))
    }

    /// Forwards the request as it arrived and returns the backend's answer.
    pub(crate) async fn forward(&self, req: hyper::Request<Incoming>) -> s3s::HttpResponse {
        let (parts, incoming) = req.into_parts();
        let Some(target) = backend_url(&self.endpoint_url, parts.uri.path_and_query().map(PathAndQuery::as_str)) else {
            return into_http(error_response(
                StatusCode::BAD_GATEWAY,
                "InternalError",
                "invalid passthrough request URI",
            ));
        };

        let headers = forwarded_headers(&parts.headers);

        let body = reqwest::Body::wrap(Body::from(incoming));
        let log_line = || format!("POST {} -> {}", parts.uri.path(), self.endpoint_url);
        into_http(send_to_backend(&self.client, &Method::POST, target, &headers, body, log_line).await)
    }
}

/// The headers a forwarded form upload keeps.
///
/// Every end-to-end header is copied with `append`: iterating a `HeaderMap` yields
/// one pair per value, so inserting would collapse a header that appears more than
/// once down to its last value. `host` and `content-length` are the two this route
/// must not copy — the backend has its own name, and the length is declared by the
/// body, which still reports the length the client sent.
#[must_use]
fn forwarded_headers(headers: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if name == HOST || name == CONTENT_LENGTH || hop_by_hop::is_connection_specific(headers, name.as_str()) {
            continue;
        }
        forwarded.append(name, value.clone());
    }
    forwarded
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    fn with_content_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_str(value).expect("a content type"));
        headers
    }

    #[test]
    fn a_form_upload_is_a_candidate() {
        let uri: Uri = "/bucket".parse().expect("a URI");
        assert!(PostObjectPassthrough::is_match(
            &Method::POST,
            &uri,
            &with_content_type("multipart/form-data; boundary=x")
        ));
    }

    #[test]
    fn other_posts_stay_on_the_typed_path() {
        let bucket: Uri = "/bucket".parse().expect("a URI");
        let form = with_content_type("multipart/form-data; boundary=x");
        assert!(!PostObjectPassthrough::is_match(&Method::GET, &bucket, &form));
        assert!(!PostObjectPassthrough::is_match(
            &Method::POST,
            &bucket,
            &with_content_type("application/xml")
        ));
        assert!(!PostObjectPassthrough::is_match(&Method::POST, &bucket, &HeaderMap::new()));
        // Multi-object delete is a POST with a form-shaped body on the same path.
        let delete: Uri = "/bucket?delete".parse().expect("a URI");
        assert!(!PostObjectPassthrough::is_match(&Method::POST, &delete, &form));
    }

    #[test]
    fn a_repeated_header_keeps_every_value() {
        let mut headers = HeaderMap::new();
        headers.append("x-amz-meta-tag", HeaderValue::from_static("one"));
        headers.append("x-amz-meta-tag", HeaderValue::from_static("two"));
        headers.insert(HOST, HeaderValue::from_static("proxy.example"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("42"));
        headers.insert("connection", HeaderValue::from_static("x-drop-me"));
        headers.insert("x-drop-me", HeaderValue::from_static("1"));

        let forwarded = forwarded_headers(&headers);
        let values: Vec<&str> = forwarded
            .get_all("x-amz-meta-tag")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(values, ["one", "two"], "a repeated header keeps every value");
        assert!(forwarded.get(HOST).is_none(), "the backend has its own name");
        assert!(forwarded.get(CONTENT_LENGTH).is_none(), "the length is declared by the body");
        assert!(forwarded.get("x-drop-me").is_none(), "a connection-specific header is dropped");
        assert!(forwarded.get("connection").is_none(), "the connection header is dropped too");
    }
}

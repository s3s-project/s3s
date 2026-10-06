// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! STS protocol passthrough via an [`S3Route`].
//!
//! `AssumeRole` is not an S3 operation: the `S3` trait has no handler for it,
//! so a client asking the proxy for temporary credentials cannot be served by
//! the typed path. The request is signed with credentials the proxy knows, so it
//! passes signature verification and reaches this route; the route forwards it
//! verbatim and lets the backend issue the credentials.

use crate::auth_passthrough::forward_verbatim;

use s3s::Body;
use s3s::S3Request;
use s3s::S3Response;
use s3s::S3Result;
use s3s::route::S3Route;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use hyper::http::Extensions;

/// Forwards the STS protocol shape (POST / with a form body) to the backend.
#[derive(Debug, Clone)]
pub struct StsRoute {
    /// Backend base URL (e.g. `http://localhost:9000`).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
    /// Upper bound, in bytes, on a forwarded request body.
    max_body_size: u64,
}

impl StsRoute {
    /// Creates a route forwarding the STS protocol shape to `endpoint_url`.
    #[must_use]
    pub fn new(endpoint_url: reqwest::Url, client: reqwest::Client, max_body_size: u64) -> Self {
        Self {
            endpoint_url,
            client,
            max_body_size,
        }
    }

    /// Whether the request uses the AWS query protocol on the service root.
    ///
    /// The `Action` lives in the form body, so the shape is identified by the
    /// method, the path and the content type rather than by reading the body:
    /// nothing is buffered before the route decides.
    #[must_use]
    fn is_sts_shape(method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST
            && uri.path() == "/"
            && headers
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
    }
}

#[async_trait::async_trait]
impl S3Route for StsRoute {
    fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
        Self::is_sts_shape(method, uri, headers)
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let S3Request {
            input,
            method,
            uri,
            headers,
            ..
        } = req;
        Ok(forward_verbatim(&self.client, &self.endpoint_url, &method, &uri, &headers, input, self.max_body_size).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> StsRoute {
        StsRoute::new(reqwest::Url::parse("http://localhost:9000").expect("url"), reqwest::Client::new(), 1024)
    }

    fn form_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8".parse().expect("header"),
        );
        headers
    }

    #[test]
    fn matches_the_sts_protocol_shape() {
        let route = route();
        let mut extensions = Extensions::new();
        assert!(route.is_match(&Method::POST, &Uri::from_static("/"), &form_headers(), &mut extensions));
    }

    #[test]
    fn does_not_match_other_requests() {
        let route = route();
        let mut extensions = Extensions::new();

        assert!(
            !route.is_match(&Method::GET, &Uri::from_static("/"), &form_headers(), &mut extensions),
            "only POST uses the query protocol"
        );

        let mut json = HeaderMap::new();
        json.insert(hyper::header::CONTENT_TYPE, "application/json".parse().expect("header"));
        assert!(
            !route.is_match(&Method::POST, &Uri::from_static("/"), &json, &mut extensions),
            "a form body identifies the shape"
        );

        assert!(
            !route.is_match(&Method::POST, &Uri::from_static("/bucket"), &form_headers(), &mut extensions),
            "the protocol posts to the service root"
        );

        assert!(
            !route.is_match(&Method::POST, &Uri::from_static("/"), &HeaderMap::new(), &mut extensions),
            "a request without a content type is not claimed"
        );
    }
}

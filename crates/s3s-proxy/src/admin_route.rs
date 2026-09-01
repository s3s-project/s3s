// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `MinIO` admin API passthrough via an [`S3Route`].
//!
//! [`MinioAdminRoute`] forwards `/minio/admin/*` requests (the `mc admin`
//! API surface) to the backend. Unlike the health/metrics passthrough in
//! [`ProxyService`](crate::proxy_service), the admin API is protected by
//! `SigV4` signatures: `mc` signs admin requests with the standard AWS
//! `SigV4` (service `s3`), so they pass the S3 signature verification and
//! reach this route with valid credentials. Implementing the passthrough as
//! an [`S3Route`] reuses the S3 authentication machinery: the default
//! [`check_access`](S3Route::check_access) rejects anonymous requests, so
//! unsigned admin calls are denied before they reach the backend.

use s3s::route::S3Route;
use s3s::{Body, S3Request, S3Response, S3Result};

use hyper::http::Extensions;
use hyper::http::uri::PathAndQuery;
use hyper::{HeaderMap, Method, StatusCode, Uri};

/// Headers that must not be forwarded to the backend.
///
/// Only `transfer-encoding` is dropped: `hyper` manages chunked framing
/// itself. The `host` header is part of the `SigV4` signature that `mc`
/// computed against the proxy's address, and the backend verifies the
/// signature against it. The `content-length` header is preserved too:
/// `MinIO` admin endpoints require it even for empty bodies (e.g.
/// `set-user-status`), and the aggregated body below has the exact same
/// length.
const HOP_BY_HOP_HEADERS: &[&str] = &["transfer-encoding"];

/// Defensive cap for admin request bodies forwarded to the backend.
///
/// Admin requests (user/policy definitions) are small JSON documents; this
/// cap is far above any realistic payload. The S3 service already enforces
/// `custom_route_max_body_size` (default 1 MiB) before this route runs, so the
/// effective limit is the stricter of the two.
const MAX_ADMIN_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Forwards `MinIO` admin API requests (`/minio/admin/*`) to the backend.
#[derive(Debug, Clone)]
pub struct MinioAdminRoute {
    /// Backend base URL (e.g. `http://localhost:9000`).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
}

impl MinioAdminRoute {
    /// Creates a new admin passthrough route targeting `endpoint_url`.
    #[must_use]
    pub fn new(endpoint_url: reqwest::Url, client: reqwest::Client) -> Self {
        Self { endpoint_url, client }
    }

    /// Whether the request path targets a `MinIO` admin API endpoint.
    #[must_use]
    fn is_admin_path(path: &str) -> bool {
        path.starts_with("/minio/admin/")
    }

    /// Builds the backend URL for a request path, preserving the path and
    /// query verbatim. Returns `None` when the composed URL fails to parse.
    #[must_use]
    fn backend_url(endpoint_url: &reqwest::Url, path_and_query: Option<&str>) -> Option<reqwest::Url> {
        let path_and_query = path_and_query?;
        let base = endpoint_url.as_str().trim_end_matches('/');
        reqwest::Url::parse(&format!("{base}{path_and_query}")).ok()
    }

    /// Whether `name` is a hop-by-hop header that `reqwest` manages itself.
    #[must_use]
    fn is_hop_by_hop(name: &str) -> bool {
        HOP_BY_HOP_HEADERS.iter().any(|header| header.eq_ignore_ascii_case(name))
    }
}

#[async_trait::async_trait]
impl S3Route for MinioAdminRoute {
    fn is_match(&self, _method: &Method, uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
        // Any method on a `/minio/admin/` path is claimed: `mc admin` uses
        // PUT/POST/GET across the v3 API.
        Self::is_admin_path(uri.path())
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let Some(target) = Self::backend_url(&self.endpoint_url, req.uri.path_and_query().map(PathAndQuery::as_str)) else {
            return Err(bad_gateway("invalid admin request URI"));
        };

        let mut body = req.input;
        let bytes = body
            .store_all_limited(MAX_ADMIN_BODY_SIZE)
            .await
            .map_err(bad_gateway_with_source)?;

        let mut request = self.client.request(req.method.clone(), target.clone());
        for (name, value) in &req.headers {
            if Self::is_hop_by_hop(name.as_str()) {
                continue;
            }
            request = request.header(name, value);
        }
        let request = request
            .body(bytes)
            .build()
            .map_err(|e| bad_gateway_with_source(Box::new(e)))?;

        tracing::debug!(target = %target, request_headers = ?request.headers(), "forwarding MinIO admin request");

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| bad_gateway_with_source(Box::new(e)))?;

        let status = response.status();
        let headers = response.headers().clone();
        let resp_body = response.bytes().await.map_err(|e| bad_gateway_with_source(Box::new(e)))?;

        let mut out = S3Response::new(Body::from(resp_body));
        out.status = Some(status);
        out.headers = headers;
        Ok(out)
    }
}

/// Builds a `502 Bad Gateway` error for a failed admin passthrough.
#[must_use]
fn bad_gateway(message: &'static str) -> s3s::S3Error {
    let mut err = s3s::S3Error::with_message(s3s::S3ErrorCode::InternalError, message);
    err.set_status_code(StatusCode::BAD_GATEWAY);
    err
}

/// Builds a `502 Bad Gateway` error carrying the underlying source.
#[must_use]
fn bad_gateway_with_source(source: s3s::StdError) -> s3s::S3Error {
    let mut err = s3s::S3Error::with_source(s3s::S3ErrorCode::InternalError, source);
    err.set_status_code(StatusCode::BAD_GATEWAY);
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_paths_match() {
        // Prefix matching: any sub-path under `/minio/admin/` is forwarded.
        for path in [
            "/minio/admin/v3/add-user",
            "/minio/admin/v3/list-users",
            "/minio/admin/v3/set-user-status",
            "/minio/admin/v3/update-policy",
            "/minio/admin/v3/info",
        ] {
            assert!(MinioAdminRoute::is_admin_path(path), "{path} should match");
        }
    }

    #[test]
    fn non_admin_paths_do_not_match() {
        for path in [
            "/minio/admin",
            "/minio/health/live",
            "/minio/v2/metrics/cluster",
            "/bucket/key",
            "/",
        ] {
            assert!(!MinioAdminRoute::is_admin_path(path), "{path} should not match");
        }
    }

    #[test]
    fn match_accepts_any_method() {
        let uri = Uri::from_static("/minio/admin/v3/add-user");
        let headers = HeaderMap::new();
        let mut extensions = Extensions::new();
        let route = MinioAdminRoute {
            endpoint_url: reqwest::Url::parse("http://localhost:9000").expect("valid base url"),
            client: reqwest::Client::new(),
        };
        for method in [Method::GET, Method::PUT, Method::POST, Method::DELETE] {
            assert!(route.is_match(&method, &uri, &headers, &mut extensions), "{method} should match");
        }
    }

    #[test]
    fn backend_url_joins_path_and_query() {
        let endpoint = reqwest::Url::parse("http://localhost:9000").expect("valid base url");
        let url = MinioAdminRoute::backend_url(&endpoint, Some("/minio/admin/v3/add-user?accessKey=foo")).expect("composed url");
        assert_eq!(url.as_str(), "http://localhost:9000/minio/admin/v3/add-user?accessKey=foo");
    }

    #[test]
    fn backend_url_trims_trailing_slash() {
        let endpoint = reqwest::Url::parse("http://localhost:9000/").expect("valid base url");
        let url = MinioAdminRoute::backend_url(&endpoint, Some("/minio/admin/v3/info")).expect("composed url");
        assert_eq!(url.as_str(), "http://localhost:9000/minio/admin/v3/info");
    }

    #[test]
    fn backend_url_without_path_and_query_is_none() {
        let endpoint = reqwest::Url::parse("http://localhost:9000").expect("valid base url");
        assert!(MinioAdminRoute::backend_url(&endpoint, None).is_none());
    }

    #[test]
    fn hop_by_hop_headers_are_detected() {
        for name in ["transfer-encoding", "Transfer-Encoding"] {
            assert!(MinioAdminRoute::is_hop_by_hop(name), "{name} should be hop-by-hop");
        }
        // `host` (part of the `SigV4` signature) and `content-length` (required
        // by MinIO admin endpoints even for empty bodies) are preserved.
        for name in ["authorization", "host", "content-length", "accept", "x-test"] {
            assert!(!MinioAdminRoute::is_hop_by_hop(name), "{name} should be forwarded");
        }
    }
}

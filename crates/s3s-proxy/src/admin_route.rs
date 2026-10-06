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

use crate::hop_by_hop;

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
            // Connection-specific headers are dropped (RFC 9110 section 7.6.1);
            // `host` is part of the signature, and `content-length` is preserved
            // because MinIO admin endpoints require it even for empty bodies.
            if hop_by_hop::is_connection_specific(&req.headers, name.as_str()) {
                continue;
            }
            request = request.header(name, value);
        }
        let request = request
            .body(bytes)
            .build()
            .map_err(|e| bad_gateway_with_source(Box::new(e)))?;

        // The request headers are deliberately not logged: admin requests carry
        // the client's `authorization` header, and a debug dump would put a
        // signature (and any session token) into the log.
        tracing::debug!(target = %target, "forwarding MinIO admin request");

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| bad_gateway_with_source(Box::new(e)))?;

        // Response headers are filtered the same way request headers are: the
        // connection-specific ones belong to this hop (RFC 9110 section 7.6.1).
        let status = response.status();
        let headers = hop_by_hop::without_connection_specific(response.headers());
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

    /// Serves one request with a raw response head and body.
    async fn serve_raw(head: &'static str, body: &'static [u8]) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncReadExt as _;
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "connection closed before the request head");
                buf.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(head.as_bytes()).await.expect("write head");
            socket.write_all(body).await.expect("write body");
            socket.flush().await.expect("flush");
        });

        (addr, handle)
    }

    fn admin_request() -> S3Request<Body> {
        S3Request {
            input: Body::empty(),
            method: Method::PUT,
            uri: Uri::from_static("/minio/admin/v3/add-user?accessKey=probe"),
            headers: HeaderMap::new(),
            extensions: Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    #[tokio::test]
    async fn connection_specific_response_headers_are_not_forwarded() {
        let (addr, server) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-backend: yes\r\nconnection: x-foo\r\nx-foo: 1\r\nkeep-alive: timeout=5\r\n\r\n",
            b"ok",
        )
        .await;
        let route = MinioAdminRoute::new(
            reqwest::Url::parse(&format!("http://{addr}")).expect("valid base url"),
            reqwest::Client::new(),
        );

        let response = route.call(admin_request()).await.expect("the admin route answers");

        server.await.expect("server task");
        assert_eq!(response.status, Some(StatusCode::OK));
        assert_eq!(
            response.headers.get("content-length").and_then(|value| value.to_str().ok()),
            Some("2"),
            "the body length is kept"
        );
        assert_eq!(
            response.headers.get("x-backend").and_then(|value| value.to_str().ok()),
            Some("yes"),
            "an end-to-end header is kept"
        );
        for name in ["connection", "x-foo", "keep-alive", "transfer-encoding"] {
            assert!(
                response.headers.get(name).is_none(),
                "{name} must not be forwarded to the client: {:?}",
                response.headers
            );
        }
        assert_eq!(response.output.bytes().as_deref(), Some(b"ok".as_slice()));
    }
}

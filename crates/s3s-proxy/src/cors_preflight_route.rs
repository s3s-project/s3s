// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! CORS preflight passthrough via an [`S3Route`].
//!
//! OPTIONS is not a modeled S3 operation: the typed path has no route for the
//! method, so a preflight that reaches it is answered `NotImplemented` (501)
//! with the message "Unknown operation". The CORS preflight a browser sends is
//! answered by the service that owns the bucket's CORS configuration, against
//! that configuration, and it carries no AWS signature for this service to
//! verify. The request is therefore forwarded verbatim and the backend decides,
//! exactly as it would for a client that talks to the backend directly — the
//! same shape as the `MinIO` admin and STS routes next to this one.
//!
//! Only the method is claimed. A request whose signature this service does
//! verify never reaches a route: signature verification runs before a matched
//! route is dispatched, so a presigned URL used with OPTIONS is still rejected
//! by the signature stage.

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

/// Forwards OPTIONS (CORS preflight) requests to the backend.
#[derive(Debug, Clone)]
pub struct CorsPreflightRoute {
    /// Backend base URL (for example <http://localhost:9000>).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
    /// Upper bound, in bytes, on a forwarded request body.
    max_body_size: u64,
}

impl CorsPreflightRoute {
    /// Creates a route forwarding OPTIONS requests to `endpoint_url`.
    #[must_use]
    pub fn new(endpoint_url: reqwest::Url, client: reqwest::Client, max_body_size: u64) -> Self {
        Self {
            endpoint_url,
            client,
            max_body_size,
        }
    }

    /// Builds the route when `enabled` is set.
    ///
    /// The switch follows the convention of the other passthrough rules in
    /// `s3s-proxy` (for example `--enable-auth-passthrough` and
    /// `--enable-post-object-passthrough`): disabled by default, so an OPTIONS
    /// request keeps the typed-path answer unless the operator opts in.
    #[must_use]
    pub fn from_config(enabled: bool, endpoint_url: reqwest::Url, client: reqwest::Client, max_body_size: u64) -> Option<Self> {
        enabled.then(|| Self::new(endpoint_url, client, max_body_size))
    }

    /// Whether the request is a CORS preflight candidate.
    ///
    /// The method alone decides: the typed path cannot serve OPTIONS at all,
    /// and whether a preflight is allowed is the backend's answer to give.
    #[must_use]
    fn is_preflight(method: &Method) -> bool {
        *method == Method::OPTIONS
    }
}

#[async_trait::async_trait]
impl S3Route for CorsPreflightRoute {
    fn is_match(&self, method: &Method, _uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
        Self::is_preflight(method)
    }

    /// Serves the preflight without a signature.
    ///
    /// The trait default rejects a request that carries no credentials
    /// ("Signature is required"). A CORS preflight is sent without credentials
    /// by design, and the answer a preflight receives (400, 403 or 200 depending
    /// on the bucket's CORS rules) is a CORS decision, not an authentication
    /// one, so this route declines to authenticate and lets the backend decide.
    /// The route performs no S3 operation of its own: it only relays OPTIONS to
    /// the configured backend, which applies its own CORS policy.
    async fn check_access(&self, _req: &mut S3Request<Body>) -> S3Result<()> {
        Ok(())
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

    use std::net::SocketAddr;
    use std::sync::Arc;

    use hyper::StatusCode;
    use hyper::http::Request;
    use s3s::HttpRequest;
    use s3s::S3;
    use s3s::config::{S3Config, StaticConfigProvider};
    use s3s::service::{S3Service, S3ServiceBuilder};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// Serves exactly one request, records its head, answers with the response.
    async fn serve_once(response: &'static str) -> (SocketAddr, Arc<tokio::sync::Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let seen = Arc::new(tokio::sync::Mutex::new(String::new()));
        let recorded = Arc::clone(&seen);

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            let head_end = loop {
                if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "connection closed before the request head");
                buf.extend_from_slice(&chunk[..read]);
            };
            *recorded.lock().await = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            socket.write_all(response.as_bytes()).await.expect("write response");
            socket.flush().await.expect("flush");
        });

        (addr, seen)
    }

    /// An S3 implementation that is never called: the requests under test are
    /// answered before the typed path reaches a handler.
    #[derive(Clone)]
    struct UnusedS3;

    #[async_trait::async_trait]
    impl S3 for UnusedS3 {}

    fn service(route: Option<CorsPreflightRoute>) -> S3Service {
        let mut builder = S3ServiceBuilder::new(UnusedS3);
        builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default()))));
        if let Some(route) = route {
            builder.set_route(route);
        }
        builder.build()
    }

    fn route_to(addr: SocketAddr, max_body_size: u64) -> CorsPreflightRoute {
        CorsPreflightRoute::new(
            format!("http://{addr}").parse().expect("backend url"),
            reqwest::Client::new(),
            max_body_size,
        )
    }

    /// Builds the route the way `build_service` does: the flag alone decides
    /// whether the route exists, so these two tests cover the wiring too.
    fn route_from_config(enabled: bool, addr: SocketAddr, max_body_size: u64) -> Option<CorsPreflightRoute> {
        CorsPreflightRoute::from_config(
            enabled,
            format!("http://{addr}").parse().expect("backend url"),
            reqwest::Client::new(),
            max_body_size,
        )
    }

    fn preflight(uri: &str) -> HttpRequest {
        Request::builder()
            .method(Method::OPTIONS)
            .uri(uri)
            .header("origin", "example.origin")
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "x-amz-meta-header2")
            .body(Body::empty())
            .expect("valid request")
    }

    #[tokio::test]
    async fn predicate_claims_options_only() {
        let route = route_to("127.0.0.1:1".parse().expect("addr"), 1024);
        let uri: Uri = "/bucket/key".parse().expect("uri");
        let mut extensions = Extensions::new();
        assert!(route.is_match(&Method::OPTIONS, &uri, &HeaderMap::new(), &mut extensions));

        for method in [Method::GET, Method::PUT, Method::POST, Method::DELETE, Method::HEAD] {
            let mut extensions = Extensions::new();
            assert!(
                !route.is_match(&method, &uri, &HeaderMap::new(), &mut extensions),
                "{method} must not be claimed"
            );
        }
    }

    /// Flag on: the preflight reaches the backend unchanged and the backend's
    /// answer is returned as it is. A preflight that matches no CORS rule is
    /// answered 403 by the backend, where the typed path answered 501; the
    /// conformance case `test_cors_header_option` checks exactly that.
    #[tokio::test]
    async fn enabled_forwards_the_preflight_and_returns_the_backend_answer() {
        let (addr, seen) = serve_once("HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nx-backend: cors\r\n\r\n").await;
        let service = service(route_from_config(true, addr, 1024));

        let resp = service.call(preflight("/bucket/key?x=1")).await.expect("service call");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "the backend's status is returned");
        assert_eq!(resp.headers().get("x-backend").expect("marker header"), "cors");

        let head = seen.lock().await.clone();
        assert!(head.starts_with("OPTIONS /bucket/key?x=1 "), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("origin: example.origin"), "{head}");
        assert!(lower.contains("access-control-request-method: get"), "{head}");
        assert!(lower.contains("access-control-request-headers: x-amz-meta-header2"), "{head}");
    }

    /// Flag off (the default), and the red control for the fix: no route is
    /// built and the typed path answers 501, which is what an OPTIONS request
    /// got before the route existed. This test fails if s3s ever starts serving
    /// OPTIONS itself (the route would then be redundant) or if the route stops
    /// being gated by the flag.
    #[tokio::test]
    async fn disabled_keeps_the_typed_path_answer_501() {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        assert!(
            route_from_config(false, addr, 1024).is_none(),
            "the flag alone must decide whether the route exists"
        );

        let service = service(route_from_config(false, addr, 1024));
        let resp = service.call(preflight("/bucket/key")).await.expect("service call");
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}

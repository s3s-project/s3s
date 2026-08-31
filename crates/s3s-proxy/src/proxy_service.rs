// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Proxy service layer for `s3s-proxy`.
//!
//! [`ProxyService`] wraps the S3 service and may forward non-S3 endpoints to
//! the backend. Currently it supports forwarding `MinIO` health and metrics
//! endpoints (`/minio/health/*`, `/minio/v2/metrics/*`) as an optional
//! capability, enabled explicitly via `--enable-minio-health-route`.
//!
//! Intercepting at the HTTP layer (rather than via
//! [`S3Route`](s3s::route::S3Route)) keeps the S3 signature machinery out of
//! the way: the prometheus endpoints authenticate with a `Bearer` token that
//! is not an AWS signature, and the S3 signature verification would otherwise
//! reject it with a 400.

use s3s::service::S3Service;
use s3s::{Body, HttpError, HttpResponse};

use hyper::body::Incoming;
use hyper::http::uri::PathAndQuery;
use hyper::service::Service;
use hyper::{Method, StatusCode, Uri};

use std::future::Future;
use std::pin::Pin;

/// Headers that must not be forwarded to the backend.
///
/// `reqwest` derives `host`, `content-length`, and `transfer-encoding` from
/// the request itself; forwarding them would conflict with its own values.
const HOP_BY_HOP_HEADERS: &[&str] = &["host", "content-length", "transfer-encoding"];

/// The proxy service: wraps the S3 service and optionally forwards non-S3
/// endpoints to the backend.
#[derive(Debug, Clone)]
pub struct ProxyService {
    /// The wrapped S3 service handling all non-forwarded requests.
    inner: S3Service,
    /// Optional `MinIO` health/metrics passthrough.
    minio_health: Option<MinioHealthPassthrough>,
}

impl ProxyService {
    /// Wraps `inner` without any passthrough; all requests are delegated to
    /// the S3 service.
    #[must_use]
    pub fn new(inner: S3Service) -> Self {
        Self {
            inner,
            minio_health: None,
        }
    }

    /// Wraps `inner` with the `MinIO` health/metrics passthrough enabled,
    /// forwarding `/minio/health/*` and `/minio/v2/metrics/*` to
    /// `endpoint_url` through `client`.
    #[must_use]
    pub fn with_minio_health(inner: S3Service, endpoint_url: reqwest::Url, client: reqwest::Client) -> Self {
        Self {
            inner,
            minio_health: Some(MinioHealthPassthrough { endpoint_url, client }),
        }
    }
}

/// `MinIO` health and metrics passthrough: forwards `GET /minio/health/*` and
/// `GET /minio/v2/metrics/*` to the backend, bypassing the S3 service entirely.
///
/// Requests that match are forwarded verbatim (including the `Authorization`
/// header, e.g. the `Bearer` token the `MinIO` prometheus endpoints require).
#[derive(Debug, Clone)]
struct MinioHealthPassthrough {
    /// Backend base URL (e.g. `http://localhost:9000`).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
}

impl MinioHealthPassthrough {
    /// Whether the request path targets a `MinIO` health or metrics endpoint.
    #[must_use]
    fn is_minio_health_path(path: &str) -> bool {
        path.starts_with("/minio/health/") || path.starts_with("/minio/v2/metrics/")
    }

    /// Whether the request matches the passthrough: a `GET` on a `MinIO`
    /// health or metrics path.
    #[must_use]
    fn is_match(method: &Method, uri: &Uri) -> bool {
        method == Method::GET && Self::is_minio_health_path(uri.path())
    }

    /// Builds the backend URL for a request path, preserving the path and
    /// query verbatim. Returns `None` when the request has no path or the
    /// composed URL fails to parse.
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

    /// Forwards a matching request to the backend and returns the response.
    async fn forward(&self, req: hyper::Request<Incoming>) -> HttpResponse {
        let Some(target) = Self::backend_url(&self.endpoint_url, req.uri().path_and_query().map(PathAndQuery::as_str)) else {
            return bad_gateway();
        };

        let mut request = self.client.request(req.method().clone(), target.clone());
        for (name, value) in req.headers() {
            if Self::is_hop_by_hop(name.as_str()) {
                continue;
            }
            request = request.header(name, value);
        }

        let Ok(built) = request.build() else {
            return bad_gateway();
        };
        tracing::debug!(target = %target, request_headers = ?built.headers(), "forwarding request");

        let Ok(response) = self.client.execute(built).await else {
            return bad_gateway();
        };
        let status = response.status();
        let headers = response.headers().clone();
        let Ok(bytes) = response.bytes().await else {
            return bad_gateway();
        };

        let mut out = HttpResponse::new(Body::from(bytes));
        *out.status_mut() = status;
        *out.headers_mut() = headers;
        out
    }
}

impl Service<hyper::Request<Incoming>> for ProxyService {
    type Response = HttpResponse;
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: hyper::Request<Incoming>) -> Self::Future {
        if let Some(passthrough) = &self.minio_health
            && MinioHealthPassthrough::is_match(req.method(), req.uri())
        {
            let this = passthrough.clone();
            Box::pin(async move { Ok(this.forward(req).await) })
        } else {
            // Disambiguate from `S3Service::call` (the inherent method on
            // `Request<Body>`) to the hyper `Service` impl.
            Service::call(&self.inner, req)
        }
    }
}

/// Builds a `502 Bad Gateway` response for a failed passthrough.
#[must_use]
fn bad_gateway() -> HttpResponse {
    let mut res = HttpResponse::new(Body::from("bad gateway".to_owned()));
    *res.status_mut() = StatusCode::BAD_GATEWAY;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_paths_match() {
        // Prefix matching: any sub-path under the two MinIO prefixes is
        // forwarded to the backend.
        for path in [
            "/minio/health/live",
            "/minio/health/ready",
            "/minio/v2/metrics/cluster",
            "/minio/v2/metrics/node",
            "/minio/v2/metrics/bucket",
            "/minio/v2/metrics/resource",
            "/minio/v2/metrics/unknown",
        ] {
            assert!(MinioHealthPassthrough::is_minio_health_path(path), "{path} should match");
        }
    }

    #[test]
    fn non_health_paths_do_not_match() {
        for path in ["/minio/health", "/health/live", "/minio/v2/metrics", "/bucket/key", "/"] {
            assert!(!MinioHealthPassthrough::is_minio_health_path(path), "{path} should not match");
        }
    }

    #[test]
    fn match_path_requires_get() {
        let uri = Uri::from_static("/minio/health/live");
        assert!(MinioHealthPassthrough::is_match(&Method::GET, &uri));
        assert!(!MinioHealthPassthrough::is_match(&Method::POST, &uri));
        assert!(!MinioHealthPassthrough::is_match(&Method::PUT, &uri));
    }

    #[test]
    fn backend_url_joins_path_and_query() {
        let endpoint = reqwest::Url::parse("http://localhost:9000").expect("valid base url");
        let url = MinioHealthPassthrough::backend_url(&endpoint, Some("/minio/health/live?x=1")).expect("composed url");
        assert_eq!(url.as_str(), "http://localhost:9000/minio/health/live?x=1");
    }

    #[test]
    fn backend_url_trims_trailing_slash() {
        let endpoint = reqwest::Url::parse("http://localhost:9000/").expect("valid base url");
        let url = MinioHealthPassthrough::backend_url(&endpoint, Some("/minio/health/ready")).expect("composed url");
        assert_eq!(url.as_str(), "http://localhost:9000/minio/health/ready");
    }

    #[test]
    fn backend_url_without_path_and_query_is_none() {
        let endpoint = reqwest::Url::parse("http://localhost:9000").expect("valid base url");
        assert!(MinioHealthPassthrough::backend_url(&endpoint, None).is_none());
    }

    #[test]
    fn hop_by_hop_headers_are_detected() {
        for name in ["host", "content-length", "transfer-encoding", "HOST", "Content-Length"] {
            assert!(MinioHealthPassthrough::is_hop_by_hop(name), "{name} should be hop-by-hop");
        }
        for name in ["authorization", "accept", "x-test"] {
            assert!(!MinioHealthPassthrough::is_hop_by_hop(name), "{name} should be forwarded");
        }
    }
}

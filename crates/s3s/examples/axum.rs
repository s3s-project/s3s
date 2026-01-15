//! Axum integration examples for s3s
//!
//! This file demonstrates two ways to integrate Axum with s3s:
//!
//! 1. **Custom Routes via `S3Route` trait** - Route specific paths to custom Axum handlers
//!    while letting s3s handle standard S3 operations. See `CustomRoute` below.
//!
//! 2. **Direct `S3Service` usage** - Use `S3Service` directly with Axum's router.
//!    `S3Service` implements `tower::Service<http::Request<B>>` for any body type
//!    that implements `http_body::Body`. See `run_s3_service_example` below.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example axum
//! ```

use s3s::route::S3Route;
use s3s::{Body, S3Request, S3Response, S3Result};

use axum::http;
use http::{Extensions, HeaderMap, Method, Uri};
use tower::Service;

// =============================================================================
// Custom Route Example (using S3Route trait)
// =============================================================================

/// A custom route that delegates to an Axum router for specific paths.
/// This allows you to add custom endpoints alongside S3 operations.
pub struct CustomRoute {
    router: axum::Router,
}

impl CustomRoute {
    #[must_use]
    pub fn build() -> Self {
        Self {
            router: self::handlers::register(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extra {
    pub credentials: Option<s3s::auth::Credentials>,
    pub region: Option<String>,
    pub service: Option<String>,
}

fn convert_request(req: S3Request<Body>) -> http::Request<Body> {
    let (mut parts, _) = http::Request::new(Body::empty()).into_parts();
    parts.method = req.method;
    parts.uri = req.uri;
    parts.headers = req.headers;
    parts.extensions = req.extensions;
    parts.extensions.insert(Extra {
        credentials: req.credentials,
        region: req.region,
        service: req.service,
    });
    http::Request::from_parts(parts, req.input)
}

fn convert_response(resp: http::Response<axum::body::Body>) -> S3Response<Body> {
    let (parts, body) = resp.into_parts();
    let mut s3_resp = S3Response::new(Body::http_body_unsync(body));
    s3_resp.status = Some(parts.status);
    s3_resp.headers = parts.headers;
    s3_resp.extensions = parts.extensions;
    s3_resp
}

#[async_trait::async_trait]
impl S3Route for CustomRoute {
    fn is_match(&self, _method: &Method, uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
        let path = uri.path();
        let prefix = const_str::concat!(self::handlers::PREFIX, "/");
        path.starts_with(prefix)
    }

    async fn check_access(&self, req: &mut S3Request<Body>) -> S3Result<()> {
        if req.credentials.is_none() {
            tracing::debug!("anonymous access");
        }
        Ok(()) // allow all requests
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let mut service = self.router.clone().into_service::<Body>();
        let req = convert_request(req);
        let result = service.call(req).await;
        match result {
            Ok(resp) => Ok(convert_response(resp)),
            Err(e) => match e {},
        }
    }
}

mod handlers {
    use std::collections::HashMap;

    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::extract::Query;
    use axum::extract::Request;
    use axum::http::Response;
    use axum::response;
    use axum::routing::get;
    use axum::routing::post;

    pub async fn echo(req: Request) -> Response<Body> {
        Response::new(req.into_body())
    }

    pub async fn hello() -> &'static str {
        "Hello, World!"
    }

    pub async fn show_path(Path(path): Path<String>) -> String {
        path
    }

    pub async fn show_query(Query(query): Query<HashMap<String, String>>) -> String {
        format!("{query:?}")
    }

    pub async fn show_json(Json(json): Json<serde_json::Value>) -> response::Json<serde_json::Value> {
        tracing::debug!(?json);
        response::Json(json)
    }

    pub const PREFIX: &str = "/custom";

    pub fn register() -> Router {
        let router = Router::new()
            .route("/echo", post(echo))
            .route("/hello", get(hello))
            .route("/show_path/{*path}", get(show_path))
            .route("/show_query", get(show_query))
            .route("/show_json", post(show_json));

        Router::new().nest(PREFIX, router)
    }
}

// =============================================================================
// Direct S3Service Example (using tower::Service)
// =============================================================================

/// Example of running `S3Service` directly with Axum.
///
/// `S3Service` implements `tower::Service<http::Request<B>>` for any body type
/// that implements `http_body::Body`, which allows it to work with Axum.
///
/// Note: Axum's `fallback_service` requires `Error = Infallible`, so we wrap
/// `S3Service` in `InfallibleS3Service` to convert errors to HTTP responses.
#[allow(dead_code)]
mod s3_service_example {
    use s3s::dto::{GetObjectInput, GetObjectOutput};
    use s3s::service::{S3Service, S3ServiceBuilder};
    use s3s::{S3, S3Request, S3Response, S3Result};

    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, Response, StatusCode};
    use axum::routing::get;
    use tokio::net::TcpListener;
    use tower::Service;

    /// A minimal S3 implementation for demonstration purposes
    #[derive(Debug, Clone)]
    struct DummyS3;

    #[async_trait::async_trait]
    impl S3 for DummyS3 {
        async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
            Err(s3s::s3_error!(NotImplemented, "GetObject is not implemented"))
        }
    }

    /// A simple health check endpoint
    async fn health_check() -> &'static str {
        "OK"
    }

    /// A wrapper that converts `S3Service` errors to responses, making it infallible for Axum
    #[derive(Clone)]
    struct InfallibleS3Service {
        inner: S3Service,
    }

    impl Service<Request<Body>> for InfallibleS3Service {
        type Response = Response<s3s::Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            // S3Service is always ready
            match <S3Service as Service<Request<Body>>>::poll_ready(&mut self.inner, cx) {
                Poll::Ready(Ok(()) | Err(_)) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            let fut = <S3Service as Service<Request<Body>>>::call(&mut self.inner, req);
            Box::pin(async move {
                match fut.await {
                    Ok(response) => Ok(response),
                    Err(err) => {
                        tracing::error!(?err, "S3 service error");
                        Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(s3s::Body::from("Internal Server Error".to_string()))
                            .unwrap())
                    }
                }
            })
        }
    }

    /// Run the `S3Service` example with Axum
    #[allow(dead_code)]
    pub async fn run() {
        // Setup tracing
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
            .init();

        // Create the S3 service
        let s3_service = S3ServiceBuilder::new(DummyS3).build();

        // Wrap in InfallibleS3Service to make it work with Axum's fallback_service
        let s3_service = InfallibleS3Service { inner: s3_service };

        // Build an Axum router with:
        // - A health check endpoint at /health
        // - S3Service as the fallback for all other routes
        let app = Router::new()
            .route("/health", get(health_check))
            .fallback_service(s3_service);

        // Bind and serve
        let addr = "127.0.0.1:8014";
        let listener = TcpListener::bind(addr).await.unwrap();

        tracing::info!("Axum server listening on http://{}", addr);
        tracing::info!("Health check: http://{}/health", addr);
        tracing::info!("S3 endpoint: http://{}/", addr);
        tracing::info!("Press Ctrl+C to stop");

        axum::serve(listener, app).await.unwrap();
    }
}

fn main() {}

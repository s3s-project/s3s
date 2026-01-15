//! Axum integration example for s3s
//!
//! This example demonstrates how to run `S3Service` with Axum.
//! `S3Service` implements `tower::Service<http::Request<B>>` for any body type
//! that implements `http_body::Body`, which allows it to work directly with Axum.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example axum
//! ```
//!
//! Then you can access the server at <http://localhost:8014>

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

#[tokio::main]
async fn main() {
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

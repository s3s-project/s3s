//! S3 Service - The core entry point for handling S3 protocol requests.
//!
//! This module provides the main service layer that coordinates authentication,
//! authorization, routing, and delegates to the underlying S3 storage implementation.
//!
//! # Overview
//!
//! The [`S3Service`] is built using the [`S3ServiceBuilder`], which allows flexible
//! configuration of various components:
//!
//! - **S3 Storage**: The core implementation that handles actual storage operations
//! - **Authentication**: Verify AWS Signature V2/V4 signatures
//! - **Authorization**: Check permissions for operations
//! - **Host Resolution**: Support path-style and virtual-hosted-style requests
//! - **Routing**: Custom request routing logic
//! - **Validation**: Bucket and object name validation
//!
//! # Basic Usage
//!
//! ## With Hyper
//!
//! ```rust,no_run
//! use s3s::S3ServiceBuilder;
//! use hyper::server::conn::http1;
//! use hyper_util::rt::TokioIo;
//! use tokio::net::TcpListener;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create your S3 storage implementation
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! let s3_impl = MyS3;
//!
//! // Build the service
//! let service = S3ServiceBuilder::new(s3_impl).build();
//!
//! // Serve with hyper
//! let listener = TcpListener::bind("127.0.0.1:8080").await?;
//! loop {
//!     let (stream, _) = listener.accept().await?;
//!     let io = TokioIo::new(stream);
//!     let service_clone = service.clone();
//!     
//!     tokio::spawn(async move {
//!         if let Err(err) = http1::Builder::new()
//!             .serve_connection(io, service_clone)
//!             .await
//!         {
//!             eprintln!("Error serving connection: {:?}", err);
//!         }
//!     });
//! }
//! # }
//! ```
//!
//! ## With Tower/Axum
//!
//! ```rust,no_run
//! use s3s::S3ServiceBuilder;
//! use axum::{Router, routing::any};
//! use tower::ServiceBuilder as TowerServiceBuilder;
//! use tower_http::trace::TraceLayer;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create your S3 storage implementation
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! let s3_impl = MyS3;
//!
//! // Build the S3 service
//! let s3_service = S3ServiceBuilder::new(s3_impl).build();
//!
//! // Wrap with Tower middleware
//! let service = TowerServiceBuilder::new()
//!     .layer(TraceLayer::new_for_http())
//!     .service(s3_service);
//!
//! // Create axum router
//! let app = Router::new()
//!     .route("/*path", any(|req| async move {
//!         // Route all requests to S3 service
//!         service.clone().call(req).await
//!     }));
//!
//! // Serve
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Advanced Configuration
//!
//! ## With Authentication and Authorization
//!
//! ```rust,no_run
//! use s3s::S3ServiceBuilder;
//! use s3s::auth::SimpleAuth;
//!
//! # async fn example() {
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! # struct MyAccessControl;
//! # impl s3s::S3Access for MyAccessControl {}
//! let s3_impl = MyS3;
//!
//! // Configure authentication with credentials
//! let mut auth = SimpleAuth::new();
//! auth.register("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
//!
//! // Configure access control
//! let access_control = MyAccessControl;
//!
//! // Build service with auth and access control
//! let mut builder = S3ServiceBuilder::new(s3_impl);
//! builder.set_auth(auth);
//! builder.set_access(access_control);
//! let service = builder.build();
//! # }
//! ```
//!
//! ## With Custom Host Resolution
//!
//! ```rust,no_run
//! use s3s::S3ServiceBuilder;
//! use s3s::host::HostParser;
//!
//! # async fn example() {
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! let s3_impl = MyS3;
//!
//! // Configure host parser for virtual-hosted-style requests
//! // e.g., bucket-name.s3.amazonaws.com
//! let host_parser = HostParser::new("s3.amazonaws.com");
//!
//! let mut builder = S3ServiceBuilder::new(s3_impl);
//! builder.set_host(host_parser);
//! let service = builder.build();
//! # }
//! ```
//!
//! ## With Custom Name Validation
//!
//! ```rust,no_run
//! use s3s::S3ServiceBuilder;
//! use s3s::validation::NameValidation;
//!
//! # async fn example() {
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! let s3_impl = MyS3;
//!
//! // Implement custom validation logic
//! struct RelaxedValidation;
//! impl NameValidation for RelaxedValidation {
//!     fn validate_bucket_name(&self, name: &str) -> bool {
//!         // Custom validation: allow any non-empty name
//!         !name.is_empty()
//!     }
//! }
//!
//! let mut builder = S3ServiceBuilder::new(s3_impl);
//! builder.set_validation(RelaxedValidation);
//! let service = builder.build();
//! # }
//! ```
//!
//! # Service Cloning
//!
//! The [`S3Service`] is cheaply cloneable due to its internal `Arc` structure.
//! This makes it efficient to share across multiple connections or threads:
//!
//! ```rust,no_run
//! # use s3s::S3ServiceBuilder;
//! # struct MyS3;
//! # impl s3s::S3 for MyS3 {}
//! let service = S3ServiceBuilder::new(MyS3).build();
//!
//! // Clone is cheap - only increments Arc reference count
//! let service1 = service.clone();
//! let service2 = service.clone();
//!
//! // Each clone can be used independently
//! tokio::spawn(async move {
//!     // Use service1 in this task
//! });
//! tokio::spawn(async move {
//!     // Use service2 in this task
//! });
//! ```

use crate::access::S3Access;
use crate::auth::S3Auth;
use crate::host::S3Host;
use crate::http::{Body, Request};
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::validation::NameValidation;
use crate::{HttpError, HttpRequest, HttpResponse};

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use tracing::{debug, error};

/// Builder for constructing an S3 service with optional authentication, authorization,
/// routing, host resolution, and validation components.
pub struct S3ServiceBuilder {
    s3: Arc<dyn S3>,
    host: Option<Box<dyn S3Host>>,
    auth: Option<Box<dyn S3Auth>>,
    access: Option<Box<dyn S3Access>>,
    route: Option<Box<dyn S3Route>>,
    validation: Option<Box<dyn NameValidation>>,
}

impl S3ServiceBuilder {
    #[must_use]
    pub fn new(s3: impl S3) -> Self {
        Self {
            s3: Arc::new(s3),
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        }
    }

    pub fn set_host(&mut self, host: impl S3Host) {
        self.host = Some(Box::new(host));
    }

    // Can customize the authentication provider
    pub fn set_auth(&mut self, auth: impl S3Auth) {
        self.auth = Some(Box::new(auth));
    }

    // Can customize the access control
    pub fn set_access(&mut self, access: impl S3Access) {
        self.access = Some(Box::new(access));
    }

    // Can customize the route
    pub fn set_route(&mut self, route: impl S3Route) {
        self.route = Some(Box::new(route));
    }

    // Can customize the validation by bucket name
    pub fn set_validation(&mut self, validation: impl NameValidation) {
        self.validation = Some(Box::new(validation));
    }

    #[must_use]
    pub fn build(self) -> S3Service {
        S3Service {
            inner: Arc::new(Inner {
                s3: self.s3,
                host: self.host,
                auth: self.auth,
                access: self.access,
                route: self.route,
                validation: self.validation,
            }),
        }
    }
}

/// The main S3 service that handles HTTP requests.
///
/// Implements both `hyper::service::Service` and `tower::Service` for compatibility
/// with various HTTP frameworks. Cheaply cloneable via internal Arc.
#[derive(Clone)]
pub struct S3Service {
    inner: Arc<Inner>,
}

struct Inner {
    s3: Arc<dyn S3>,
    host: Option<Box<dyn S3Host>>,
    auth: Option<Box<dyn S3Auth>>,
    access: Option<Box<dyn S3Access>>,
    route: Option<Box<dyn S3Route>>,
    validation: Option<Box<dyn NameValidation>>,
}

impl S3Service {
    /// Main entry point for processing S3 requests.
    ///
    /// Creates a call context with all configured components (auth, access, routing, etc.),
    /// delegates to the operation dispatcher, and logs results with timing information.
    #[allow(clippy::missing_errors_doc)]
    #[tracing::instrument(
        level = "debug",
        skip(self, req),
        fields(start_time=?crate::time::now_utc())
    )]
    pub async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        debug!(?req);

        let t0 = crate::time::Instant::now();

        let mut req = Request::from(req);

        // Build the call context that bundles all configured components
        // for the operation dispatcher to use during request processing
        let ccx = crate::ops::CallContext {
            s3: &self.inner.s3,
            host: self.inner.host.as_deref(),
            auth: self.inner.auth.as_deref(),
            access: self.inner.access.as_deref(),
            route: self.inner.route.as_deref(),
            validation: self.inner.validation.as_deref(),
        };
        let result = match crate::ops::call(&mut req, &ccx).await {
            Ok(resp) => Ok(HttpResponse::from(resp)),
            Err(err) => Err(HttpError::new(Box::new(err))),
        };

        let duration = t0.elapsed();

        // Log at different levels based on response status
        match result {
            Ok(ref resp) => {
                if resp.status().is_server_error() {
                    error!(?duration, ?resp);
                } else {
                    debug!(?duration, ?resp);
                }
            }
            Err(ref err) => error!(?duration, ?err),
        }

        result
    }

    async fn call_owned(self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.call(req).await
    }
}

impl fmt::Debug for S3Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Service").finish_non_exhaustive()
    }
}

// Automatically implement hyper framework for S3Service
impl hyper::service::Service<http::Request<hyper::body::Incoming>> for S3Service {
    type Response = HttpResponse;

    type Error = HttpError;

    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let req = req.map(Body::from);
        let service = self.clone();
        Box::pin(service.call_owned(req))
    }
}

// Automatically implement tower framework for S3Service
impl tower::Service<http::Request<hyper::body::Incoming>> for S3Service {
    type Response = HttpResponse;

    type Error = HttpError;

    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let req = req.map(Body::from);
        let service = self.clone();
        Box::pin(service.call_owned(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{S3Error, S3Request, S3Response};

    use stdx::mem::output_size;

    macro_rules! print_future_size {
        ($func:path) => {
            println!("{:<24}: {}", stringify!($func), output_size(&$func));
        };
    }

    macro_rules! print_type_size {
        ($ty:path) => {
            println!("{:<24}: {}", stringify!($ty), std::mem::size_of::<$ty>());
        };
    }

    /// Test to ensure future sizes don't grow too large accidentally.
    ///
    /// Large futures can cause stack overflow or performance degradation.
    /// This test acts as a regression check for future size changes.
    #[test]
    fn future_size() {
        print_type_size!(std::time::Instant);

        print_type_size!(HttpRequest);
        print_type_size!(HttpResponse);
        print_type_size!(HttpError);

        print_type_size!(S3Request<()>);
        print_type_size!(S3Response<()>);
        print_type_size!(S3Error);

        print_type_size!(S3Service);

        print_future_size!(crate::ops::call);
        print_future_size!(S3Service::call);
        print_future_size!(S3Service::call_owned);

        // Enforce maximum future sizes to prevent accidental regressions
        assert!(output_size(&crate::ops::call) <= 1600);
        assert!(output_size(&S3Service::call) <= 3000);
        assert!(output_size(&S3Service::call_owned) <= 3300);
    }

    // Test validation functionality
    use crate::validation::NameValidation;

    // Mock S3 implementation for testing
    struct MockS3;
    impl S3 for MockS3 {}

    // Test validation that allows any bucket name
    struct RelaxedValidation;
    impl NameValidation for RelaxedValidation {
        fn validate_bucket_name(&self, _name: &str) -> bool {
            true // Allow any bucket name
        }
    }

    #[test]
    fn test_service_builder_validation() {
        let validation = RelaxedValidation;
        let mut builder = S3ServiceBuilder::new(MockS3);
        builder.set_validation(validation);
        let service = builder.build();

        // Verify validation was set
        assert!(service.inner.validation.is_some());
    }

    #[test]
    fn test_service_builder_default_validation() {
        let builder = S3ServiceBuilder::new(MockS3);
        let service = builder.build();

        // Should have default validation when none is set
        assert!(service.inner.validation.is_none()); // None means it will use AwsNameValidation
    }
}

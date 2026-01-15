use crate::access::S3Access;
use crate::auth::S3Auth;
use crate::config::{S3Config, StaticConfig};
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

pub struct S3ServiceBuilder {
    s3: Arc<dyn S3>,
    config: Option<Arc<dyn S3Config>>,
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
            config: None,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        }
    }

    pub fn set_config(&mut self, config: Arc<dyn S3Config>) -> &mut Self {
        self.config = Some(config);
        self
    }

    pub fn set_host(&mut self, host: impl S3Host) {
        self.host = Some(Box::new(host));
    }

    pub fn set_auth(&mut self, auth: impl S3Auth) {
        self.auth = Some(Box::new(auth));
    }

    pub fn set_access(&mut self, access: impl S3Access) {
        self.access = Some(Box::new(access));
    }

    pub fn set_route(&mut self, route: impl S3Route) {
        self.route = Some(Box::new(route));
    }

    pub fn set_validation(&mut self, validation: impl NameValidation) {
        self.validation = Some(Box::new(validation));
    }

    #[must_use]
    pub fn build(self) -> S3Service {
        let config = self.config.unwrap_or_else(|| Arc::new(StaticConfig::default()));
        S3Service {
            inner: Arc::new(Inner {
                s3: self.s3,
                config,
                host: self.host,
                auth: self.auth,
                access: self.access,
                route: self.route,
                validation: self.validation,
            }),
        }
    }
}

#[derive(Clone)]
pub struct S3Service {
    inner: Arc<Inner>,
}

struct Inner {
    s3: Arc<dyn S3>,
    config: Arc<dyn S3Config>,
    host: Option<Box<dyn S3Host>>,
    auth: Option<Box<dyn S3Auth>>,
    access: Option<Box<dyn S3Access>>,
    route: Option<Box<dyn S3Route>>,
    validation: Option<Box<dyn NameValidation>>,
}

impl S3Service {
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

        let ccx = crate::ops::CallContext {
            s3: &self.inner.s3,
            config: &self.inner.config,
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

        // In case the futures are made too large accidentally
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

    #[test]
    fn test_service_builder_default_config() {
        let builder = S3ServiceBuilder::new(MockS3);
        let service = builder.build();

        // Should have default config values
        assert_eq!(service.inner.config.max_xml_body_size(), 20 * 1024 * 1024);
        assert_eq!(service.inner.config.max_post_object_file_size(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_service_builder_custom_config() {
        use crate::config::StaticConfig;

        let custom_config: Arc<dyn crate::config::S3Config> = Arc::new(StaticConfig {
            max_xml_body_size: 10 * 1024 * 1024,
            max_post_object_file_size: 2 * 1024 * 1024 * 1024,
            ..Default::default()
        });

        let mut builder = S3ServiceBuilder::new(MockS3);
        builder.set_config(custom_config);
        let service = builder.build();

        assert_eq!(service.inner.config.max_xml_body_size(), 10 * 1024 * 1024);
        assert_eq!(service.inner.config.max_post_object_file_size(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_service_builder_hot_reload_config() {
        use crate::config::{HotReloadConfig, StaticConfig};

        let hot_config = Arc::new(HotReloadConfig::new(StaticConfig::default()));

        let mut builder = S3ServiceBuilder::new(MockS3);
        builder.set_config(hot_config.clone());
        let service = builder.build();

        // Initial value
        assert_eq!(service.inner.config.max_xml_body_size(), 20 * 1024 * 1024);

        // Update the config
        hot_config.update(StaticConfig {
            max_xml_body_size: 30 * 1024 * 1024,
            ..Default::default()
        });

        // Service should see the new value
        assert_eq!(service.inner.config.max_xml_body_size(), 30 * 1024 * 1024);
    }
}

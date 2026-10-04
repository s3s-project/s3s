// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Dispatch to the first matching route.
//!
//! The S3 service keeps one custom route, while the proxy has several
//! independent ones (the `MinIO` admin API and the STS protocol shape), so they
//! are chained here. `is_match`, `check_access` and `call` evaluate the same
//! predicates in the same order, so the route that claims a request is the route
//! that handles it.

use s3s::Body;
use s3s::S3Request;
use s3s::S3Response;
use s3s::S3Result;
use s3s::route::S3Route;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use hyper::http::Extensions;

/// A single route slot that dispatches to the first inner route that matches.
pub struct RouteChain {
    routes: Vec<Box<dyn S3Route>>,
}

impl RouteChain {
    /// Chains `routes`, in matching order.
    #[must_use]
    pub fn new(routes: Vec<Box<dyn S3Route>>) -> Self {
        Self { routes }
    }

    /// The first route whose predicate matches.
    fn matching<'a>(
        &'a self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        extensions: &mut Extensions,
    ) -> Option<&'a dyn S3Route> {
        self.routes
            .iter()
            .find(|route| route.is_match(method, uri, headers, extensions))
            .map(|route| &**route)
    }
}

#[async_trait::async_trait]
impl S3Route for RouteChain {
    fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, extensions: &mut Extensions) -> bool {
        self.matching(method, uri, headers, extensions).is_some()
    }

    async fn check_access(&self, req: &mut S3Request<Body>) -> S3Result<()> {
        let Some(route) = self.matching(&req.method, &req.uri, &req.headers, &mut req.extensions) else {
            return Err(s3s::s3_error!(InternalError, "no route matched"));
        };
        route.check_access(req).await
    }

    async fn call(&self, mut req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let Some(route) = self.matching(&req.method, &req.uri, &req.headers, &mut req.extensions) else {
            return Err(s3s::s3_error!(InternalError, "no route matched"));
        };
        route.call(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::Mutex;

    use s3s::S3Result;

    /// A route that claims one path and records that it was called.
    struct TestRoute {
        path: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl S3Route for TestRoute {
        fn is_match(&self, _method: &Method, uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
            uri.path() == self.path
        }

        async fn call(&self, _req: S3Request<Body>) -> S3Result<S3Response<Body>> {
            self.calls.lock().expect("lock").push(self.path);
            Ok(S3Response::new(Body::empty()))
        }
    }

    fn request(path: &'static str) -> S3Request<Body> {
        S3Request {
            input: Body::empty(),
            method: Method::GET,
            uri: Uri::from_static(path),
            headers: HeaderMap::new(),
            extensions: Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    fn chain(calls: &Arc<Mutex<Vec<&'static str>>>) -> RouteChain {
        RouteChain::new(vec![
            Box::new(TestRoute {
                path: "/admin",
                calls: Arc::clone(calls),
            }),
            Box::new(TestRoute {
                path: "/sts",
                calls: Arc::clone(calls),
            }),
        ])
    }

    #[test]
    fn matches_when_any_inner_route_matches() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = chain(&calls);
        let mut extensions = Extensions::new();
        let headers = HeaderMap::new();

        assert!(chain.is_match(&Method::GET, &Uri::from_static("/admin"), &headers, &mut extensions));
        assert!(chain.is_match(&Method::GET, &Uri::from_static("/sts"), &headers, &mut extensions));
        assert!(!chain.is_match(&Method::GET, &Uri::from_static("/other"), &headers, &mut extensions));
    }

    #[tokio::test]
    async fn dispatches_to_the_matching_route() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = chain(&calls);

        chain.call(request("/sts")).await.expect("call");
        assert_eq!(calls.lock().expect("lock").as_slice(), ["/sts"]);
    }

    #[tokio::test]
    async fn an_unmatched_request_is_an_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = chain(&calls);
        assert!(chain.call(request("/other")).await.is_err());
        assert!(calls.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn check_access_is_delegated() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = chain(&calls);
        let mut req = request("/admin");
        let err = chain
            .check_access(&mut req)
            .await
            .expect_err("the default denies an anonymous request");
        assert_eq!(*err.code(), s3s::S3ErrorCode::AccessDenied);
    }
}

use crate::Body;
use crate::S3Request;
use crate::S3Response;
use crate::S3Result;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use hyper::http::Extensions;

#[async_trait::async_trait]
pub trait S3Route: Send + Sync + 'static {
    fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, extensions: &mut Extensions) -> bool;

    async fn check_access(&self, req: &mut S3Request<Body>) -> S3Result<()> {
        match req.credentials {
            Some(_) => Ok(()),
            None => Err(s3_error!(AccessDenied, "Signature is required")),
        }
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::header;

    #[allow(dead_code)]
    pub struct AssumeRole {}

    #[async_trait::async_trait]
    impl S3Route for AssumeRole {
        fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _: &mut Extensions) -> bool {
            if method == Method::POST && uri.path() == "/"
                && let Some(val) = headers.get(header::CONTENT_TYPE)
                && val.as_bytes() == b"application/x-www-form-urlencoded"
            {
                return true;
            }
            false
        }

        async fn call(&self, _: S3Request<Body>) -> S3Result<S3Response<Body>> {
            tracing::debug!("call AssumeRole");
            return Err(s3_error!(NotImplemented));
        }
    }
}

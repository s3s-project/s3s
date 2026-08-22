// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use crate::S3Operation;
use crate::auth::Credentials;
use crate::path::S3Path;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use hyper::http::Extensions;

pub struct S3AccessContext<'a> {
    pub(crate) credentials: Option<&'a Credentials>,
    pub(crate) s3_path: &'a S3Path,
    pub(crate) s3_op: &'a S3Operation,

    pub(crate) method: &'a Method,
    pub(crate) uri: &'a Uri,
    pub(crate) headers: &'a HeaderMap,

    pub(crate) extensions: &'a mut Extensions,
}

impl S3AccessContext<'_> {
    /// Returns the credentials of current request.
    ///
    /// `None` means anonymous request.
    #[must_use]
    pub fn credentials(&self) -> Option<&Credentials> {
        self.credentials
    }

    /// Returns the S3 path of current request.
    ///
    /// An S3 path can be root, bucket, or object.
    #[must_use]
    pub fn s3_path(&self) -> &S3Path {
        self.s3_path
    }

    /// Returns the S3 operation of current request.
    #[must_use]
    pub fn s3_op(&self) -> &S3Operation {
        self.s3_op
    }

    #[must_use]
    pub fn method(&self) -> &Method {
        self.method
    }

    #[must_use]
    pub fn uri(&self) -> &Uri {
        self.uri
    }

    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.headers
    }

    /// Returns the extensions of current request.
    ///
    /// It is used to pass custom data between middlewares.
    #[must_use]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        self.extensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::SecretKey;
    use hyper::header::HeaderValue;

    #[test]
    fn access_context_exposes_all_fields() {
        let credentials = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_key: SecretKey::from("secret"),
        };
        let s3_path = S3Path::object("bucket", "key");
        let s3_op = S3Operation { name: "GetObject" };
        let method = Method::GET;
        let uri = Uri::from_static("http://example.com/bucket/key");
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("value"));
        let mut extensions = Extensions::new();

        let mut ctx = S3AccessContext {
            credentials: Some(&credentials),
            s3_path: &s3_path,
            s3_op: &s3_op,
            method: &method,
            uri: &uri,
            headers: &headers,
            extensions: &mut extensions,
        };

        assert_eq!(ctx.credentials().map(|x| x.access_key.as_str()), Some("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(ctx.s3_path().as_object(), Some(("bucket", "key")));
        assert_eq!(ctx.s3_op().name(), "GetObject");
        assert_eq!(ctx.method(), &Method::GET);
        assert_eq!(ctx.uri(), &uri);
        assert_eq!(ctx.headers().get("x-test").unwrap().to_str().unwrap(), "value");

        ctx.extensions_mut().insert::<usize>(42);
        assert_eq!(ctx.extensions_mut().get::<usize>(), Some(&42));
    }
}

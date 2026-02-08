//! Access control and authorization
//!
//! This module provides the [`S3Access`] trait for implementing fine-grained access control
//! over S3 operations based on authenticated credentials.
//!
//! # Overview
//!
//! The access control system allows you to authorize or deny S3 operations. The generated
//! [`S3Access`] trait provides:
//!
//! - A general `check` method that, when authentication is configured, is called before
//!   deserializing operation input for authenticated requests
//! - Per-operation methods for fine-grained control (e.g., `get_object`, `put_object`)
//!
//! > **Security note**
//! >
//! > `S3Access::check` (and per-operation access methods) are only invoked for requests
//! > that have been authenticated by an auth provider. If no auth provider is configured
//! > (i.e., the internal `CallContext.auth` is `None`), S3 operations skip access checks
//! > entirely. In other words, calling [`S3ServiceBuilder::set_access`](crate::service::S3ServiceBuilder::set_access)
//! > alone does *not* enforce authentication or authorization.
//! >
//! > To enforce authorization, you must configure authentication on `S3ServiceBuilder`
//! > (so that requests populate `CallContext.auth`) and then attach your `S3Access`
//! > implementation via `set_access(...)`.
//!
//! # Example
//!
//! ```
//! use s3s::access::{S3Access, S3AccessContext};
//! use s3s::S3Result;
//!
//! struct MyAccessControl;
//!
//! #[async_trait::async_trait]
//! impl S3Access for MyAccessControl {
//!     async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
//!         // Check if request has valid credentials
//!         match cx.credentials() {
//!             Some(creds) => {
//!                 // You can check the operation, bucket, key, etc.
//!                 let op_name = cx.s3_op().name();
//!                 let path = cx.s3_path();
//!                 
//!                 // Implement your access control logic here
//!                 tracing::info!("User {} accessing {} on {:?}",
//!                     creds.access_key, op_name, path);
//!                 Ok(())
//!             }
//!             None => Err(s3s::s3_error!(AccessDenied, "Authentication required")),
//!         }
//!     }
//! }
//! ```
//!
//! # Integration with `S3Service`
//!
//! ```
//! use s3s::service::S3ServiceBuilder;
//! use s3s::access::{S3Access, S3AccessContext};
//! use s3s::auth::SimpleAuth;
//! use s3s::{S3, S3Request, S3Response, S3Result};
//! use s3s::dto::{GetObjectInput, GetObjectOutput};
//!
//! #[derive(Clone)]
//! struct MyS3;
//!
//! #[async_trait::async_trait]
//! impl S3 for MyS3 {
//! #   async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
//! #       Err(s3s::s3_error!(NotImplemented))
//! #   }
//!     // Implement S3 operations
//! }
//!
//! struct MyAccessControl;
//!
//! #[async_trait::async_trait]
//! impl S3Access for MyAccessControl {
//!     async fn check(&self, _cx: &mut S3AccessContext<'_>) -> S3Result<()> {
//!         Ok(())
//!     }
//! }
//!
//! let mut builder = S3ServiceBuilder::new(MyS3);
//! // Configure both auth and access control for authorization to be enforced
//! builder.set_auth(SimpleAuth::from_single("ACCESS_KEY", "SECRET_KEY"));
//! builder.set_access(MyAccessControl);
//! let service = builder.build();
//! ```

cfg_if::cfg_if! {
    if #[cfg(feature = "minio")] {
        mod generated_minio;
        use self::generated_minio as generated;
    } else {
        mod generated;
    }
}

pub use self::generated::S3Access;

mod context;
pub use self::context::S3AccessContext;

use crate::error::S3Result;

pub(crate) fn default_check(cx: &mut S3AccessContext<'_>) -> S3Result<()> {
    match cx.credentials() {
        Some(_) => Ok(()),
        None => Err(s3_error!(AccessDenied, "Signature is required")),
    }
}

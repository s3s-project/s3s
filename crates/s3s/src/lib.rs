#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(
    clippy::bool_assert_comparison,  // I don't like `assert!(!expression)`. It's very misleading.
    clippy::multiple_crate_versions, // Sometimes not fixable
    clippy::module_name_repetitions,
    clippy::single_match_else,
    clippy::wildcard_imports,
    clippy::let_underscore_untyped,
    clippy::inline_always,
    clippy::needless_continue,
)]

#[macro_use]
mod utils;

#[macro_use]
mod error;

mod http;
mod ops;
mod protocol;
mod s3_op;
mod s3_trait;
mod sig_v2;
mod sig_v4;
mod time;

pub mod access;
pub mod auth;
pub mod checksum;
pub mod crypto;
pub mod dto;
pub mod header;
pub mod host;
pub mod path;
pub mod route;
pub mod service;
pub mod stream;
pub mod validation;
pub mod xml;

pub use self::error::*;
pub use self::http::Body;
pub use self::s3_op::S3Operation;
pub use self::s3_trait::S3;

pub use self::protocol::HttpError;
pub use self::protocol::HttpRequest;
pub use self::protocol::HttpResponse;
pub use self::protocol::S3Request;
pub use self::protocol::S3Response;
pub use self::protocol::TrailingHeaders;

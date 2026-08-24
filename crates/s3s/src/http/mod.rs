// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! HTTP layer types and utilities used internally by the S3 service.
//!
//! Contains request and response wrappers, body types, query-string and header
//! helpers, multipart-form parsing, and the AWS chunked-upload stream decoder.

mod ser;
pub use self::ser::*;

mod de;
pub use self::de::*;

mod ordered_qs;
pub use self::ordered_qs::*;

// Deprecated compatibility shim retained until the next breaking release.
#[allow(dead_code)]
mod ordered_headers;
#[allow(deprecated, unused_imports)]
pub use self::ordered_headers::*;

mod aws_chunked_stream;
pub use self::aws_chunked_stream::*;

mod multipart;
pub use self::multipart::*;

mod body;
pub use self::body::*;

mod keep_alive_body;
pub use self::keep_alive_body::KeepAliveBody;

mod etag;

mod request;
pub use self::request::Request;

mod response;
pub use self::response::Response;

pub(crate) fn header_value_to_str(value: &hyper::header::HeaderValue) -> Option<&str> {
    std::str::from_utf8(value.as_bytes()).ok()
}

pub(crate) fn get_header_str<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(header_value_to_str)
}

pub use hyper::header::{HeaderName, HeaderValue, InvalidHeaderValue};
pub use hyper::http::StatusCode;

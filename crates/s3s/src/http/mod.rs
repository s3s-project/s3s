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

pub use hyper::header::{HeaderName, HeaderValue, InvalidHeaderValue};
pub use hyper::http::StatusCode;

pub(crate) fn header_value_to_str(value: &hyper::header::HeaderValue) -> Option<&str> {
    std::str::from_utf8(value.as_bytes()).ok()
}

/// Returns the header value only when the name maps to exactly one value.
///
/// Repeated header lines are treated as if the header were absent instead of
/// silently picking an arbitrary duplicate, so validation-sensitive consumers
/// (authorization, dates, content lengths) never observe an ambiguous value.
pub(crate) fn get_unique_header_str<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    let mut iter = headers.get_all(name).iter();
    let value = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    header_value_to_str(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_unique_header_str_rejects_duplicates() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::HeaderName::from_static("x-amz-date"),
            hyper::header::HeaderValue::from_static("20260825T000000Z"),
        );
        headers.append(
            hyper::header::HeaderName::from_static("x-amz-date"),
            hyper::header::HeaderValue::from_static("20260825T000001Z"),
        );

        assert_eq!(get_unique_header_str(&headers, "x-amz-date"), None);
    }

    #[test]
    fn get_unique_header_str_returns_single_value() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::HeaderName::from_static("host"),
            hyper::header::HeaderValue::from_static("example.com"),
        );

        assert_eq!(get_unique_header_str(&headers, "host"), Some("example.com"));
        assert_eq!(get_unique_header_str(&headers, "missing"), None);
    }
}

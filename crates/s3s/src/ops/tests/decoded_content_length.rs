// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `x-amz-decoded-content-length` parsing.

use super::*;

mod extract_decoded_content_length_tests {
    use super::extract_decoded_content_length;
    use hyper::HeaderMap;
    use hyper::header::{HeaderName, HeaderValue};

    const HEADER_NAME: &str = "x-amz-decoded-content-length";

    fn headers_from_slice(slice: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in slice {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
                HeaderValue::from_bytes(value.as_bytes()).expect("valid test header value"),
            );
        }
        headers
    }

    #[test]
    fn missing_header_returns_none() {
        let hs = HeaderMap::new();
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn valid_integer_returns_some() {
        let hs = headers_from_slice(&[(HEADER_NAME, "66560")]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(66560));
    }

    #[test]
    fn zero_returns_some() {
        let hs = headers_from_slice(&[(HEADER_NAME, "0")]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(0));
    }

    #[test]
    fn non_numeric_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "not-a-number")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn empty_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn negative_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "-1")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn large_value_within_usize() {
        let val = usize::MAX.to_string();
        let hs = headers_from_slice(&[(HEADER_NAME, &val)]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(usize::MAX));
    }

    #[test]
    fn u64_max_exceeds_usize_on_32bit() {
        let val = u64::MAX.to_string();
        let hs = headers_from_slice(&[(HEADER_NAME, &val)]);
        let result = extract_decoded_content_length(&hs);
        if usize::try_from(u64::MAX).is_err() {
            // On 32-bit platforms, u64::MAX won't fit in usize
            assert!(result.is_err());
        } else {
            assert!(result.is_ok());
        }
    }
}

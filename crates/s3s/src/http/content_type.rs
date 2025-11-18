//! Content-Type handling utilities
//!
//! This module centralizes all Content-Type parsing and validation logic,
//! providing detailed error messages for invalid content types.

use crate::error::*;
use crate::http::Multipart;
use crate::http::{OrderedHeaders, Request};

use mime::Mime;
use std::str::FromStr;

/// Error type for Content-Type parsing failures
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ContentTypeError {
    /// The Content-Type header value is not valid UTF-8
    #[error("Content-Type header contains invalid UTF-8 characters")]
    InvalidUtf8,

    /// The Content-Type header value has an invalid format
    #[error("Content-Type header has invalid format: {reason}")]
    InvalidFormat { reason: String },

    /// The Content-Type header value is missing required components
    #[error("Content-Type header is missing {component}")]
    MissingComponent { component: String },
}

impl From<mime::FromStrError> for ContentTypeError {
    fn from(err: mime::FromStrError) -> Self {
        ContentTypeError::InvalidFormat { reason: err.to_string() }
    }
}

/// Parse Content-Type header from HTTP request headers
///
/// # Errors
/// Returns an error if:
/// - The Content-Type header is present but has an invalid format
/// - The Content-Type header contains invalid UTF-8 characters
pub fn parse_content_type(hs: &OrderedHeaders<'_>) -> S3Result<Option<Mime>> {
    let Some(content_type) = hs.get_unique(crate::header::CONTENT_TYPE) else {
        return Ok(None);
    };

    // https://github.com/s3s-project/s3s/issues/361
    // Empty Content-Type header should be treated as None
    if content_type.is_empty() {
        return Ok(None);
    }

    match content_type.parse::<Mime>() {
        Ok(mime) => Ok(Some(mime)),
        Err(err) => {
            let content_type_error = ContentTypeError::from(err);
            Err(s3_error!(
                content_type_error,
                InvalidArgument,
                "Invalid Content-Type header: {:?}. {}",
                content_type,
                get_detailed_error_message(content_type)
            ))
        }
    }
}

/// Parse Content-Type field from multipart form data
///
/// # Errors
/// Returns an error if the Content-Type field value has an invalid format
#[allow(dead_code)]
pub fn parse_multipart_content_type<T>(m: &Multipart, name: &str) -> S3Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let Some(val) = m.find_field_value(name) else {
        return Ok(None);
    };

    match val.parse() {
        Ok(ans) => Ok(Some(ans)),
        Err(source) => Err(s3_error!(
            source,
            InvalidArgument,
            "Invalid Content-Type field '{}': {:?}. {}",
            name,
            val,
            get_detailed_error_message(val)
        )),
    }
}

/// Parse Content-Type from a direct request (for backwards compatibility)
///
/// This is a convenience function that extracts headers and parses Content-Type.
/// It's primarily used for testing.
///
/// # Errors
/// Returns an error if the Content-Type header has an invalid format
#[allow(dead_code)]
pub fn parse_request_content_type(req: &Request) -> S3Result<Option<Mime>> {
    // Get the content-type header directly from the request
    let content_type = req.headers.get(crate::header::CONTENT_TYPE);

    let Some(content_type_value) = content_type else {
        return Ok(None);
    };

    // Empty Content-Type header should be treated as None
    if content_type_value.is_empty() {
        return Ok(None);
    }

    match content_type_value.to_str() {
        Ok(s) => match s.parse::<Mime>() {
            Ok(mime) => Ok(Some(mime)),
            Err(err) => {
                let content_type_error = ContentTypeError::from(err);
                Err(s3_error!(
                    content_type_error,
                    InvalidArgument,
                    "Invalid Content-Type header: {:?}. {}",
                    s,
                    get_detailed_error_message(s)
                ))
            }
        },
        Err(_) => Err(s3_error!(
            ContentTypeError::InvalidUtf8,
            InvalidArgument,
            "Content-Type header contains invalid UTF-8 characters"
        )),
    }
}

/// Get a detailed error message explaining what might be wrong with the Content-Type
fn get_detailed_error_message(content_type_str: &str) -> String {
    let mut hints = Vec::new();

    // Check for common issues
    if content_type_str.contains(' ') && !content_type_str.contains(';') {
        hints.push("contains spaces without semicolon separator");
    }

    if content_type_str.starts_with('/') || content_type_str.ends_with('/') {
        hints.push("missing type or subtype");
    }

    if content_type_str.split('/').count() != 2 {
        hints.push("should be in format 'type/subtype' (e.g., 'text/plain', 'application/json')");
    }

    if content_type_str.contains(";;") {
        hints.push("contains double semicolons");
    }

    if content_type_str.contains("=;") || content_type_str.contains(";=") {
        hints.push("has malformed parameter syntax");
    }

    // Check for common valid formats to suggest
    if hints.is_empty() {
        hints.push("must be a valid MIME type (e.g., 'text/plain', 'application/json', 'multipart/form-data')");
    }

    if hints.len() == 1 {
        format!("Content-Type {}", hints[0])
    } else {
        format!("Content-Type issues: {}", hints.join("; "))
    }
}

/// Validate that a Content-Type matches expected type/subtype
///
/// This can be used to enforce specific content types for certain operations
#[allow(dead_code)]
pub fn validate_content_type(mime: &Mime, expected_type: &str, expected_subtype: &str) -> S3Result<()> {
    if mime.type_() != expected_type || mime.subtype() != expected_subtype {
        return Err(s3_error!(
            InvalidArgument,
            "Expected Content-Type {}/{}, but got {}/{}",
            expected_type,
            expected_subtype,
            mime.type_(),
            mime.subtype()
        ));
    }
    Ok(())
}

/// Check if a MIME type is multipart/form-data
#[inline]
pub fn is_multipart_form_data(mime: &Mime) -> bool {
    mime.type_() == mime::MULTIPART && mime.subtype() == mime::FORM_DATA
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::HeaderMap;

    fn create_test_headers(content_type: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(crate::header::CONTENT_TYPE, content_type.parse().unwrap());
        headers
    }

    fn extract_ordered_headers(headers: &HeaderMap) -> OrderedHeaders<'_> {
        // Use the public from_headers method
        use crate::http::OrderedHeaders;
        OrderedHeaders::from_headers(headers).unwrap()
    }

    #[test]
    fn test_parse_valid_content_types() {
        let cases = vec![
            "text/plain",
            "application/json",
            "application/xml",
            "multipart/form-data; boundary=something",
            "text/html; charset=utf-8",
        ];

        for case in cases {
            let headers = create_test_headers(case);
            let hs = extract_ordered_headers(&headers);
            let result = parse_content_type(&hs);
            assert!(result.is_ok(), "Failed to parse: {}", case);
            assert!(result.unwrap().is_some(), "Expected Some for: {}", case);
        }
    }

    #[test]
    fn test_parse_empty_content_type() {
        let headers = create_test_headers("");
        let hs = extract_ordered_headers(&headers);
        let result = parse_content_type(&hs);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_parse_invalid_content_types() {
        let cases = vec!["invalid", "/subtype", "type//subtype"];

        for case in cases {
            let headers = create_test_headers(case);
            let hs = extract_ordered_headers(&headers);
            let result = parse_content_type(&hs);
            assert!(result.is_err(), "Should fail for: {}", case);

            // Verify error message contains helpful information
            let err = result.unwrap_err();
            let msg = err.message().unwrap_or("");
            assert!(
                msg.contains("Content-Type") || msg.contains("Invalid"),
                "Error message should be descriptive for: {}. Got: {}",
                case,
                msg
            );
        }
    }

    #[test]
    fn test_is_multipart_form_data() {
        let mime: Mime = "multipart/form-data".parse().unwrap();
        assert!(is_multipart_form_data(&mime));

        let mime: Mime = "application/json".parse().unwrap();
        assert!(!is_multipart_form_data(&mime));
    }

    #[test]
    fn test_validate_content_type() {
        let mime: Mime = "application/json".parse().unwrap();
        assert!(validate_content_type(&mime, "application", "json").is_ok());
        assert!(validate_content_type(&mime, "text", "plain").is_err());
    }

    #[test]
    fn test_detailed_error_messages() {
        let msg = get_detailed_error_message("invalid");
        assert!(msg.contains("type/subtype") || msg.contains("MIME"));

        let msg = get_detailed_error_message("/subtype");
        assert!(msg.contains("missing"));

        let msg = get_detailed_error_message("type/");
        assert!(msg.contains("missing"));
    }
}

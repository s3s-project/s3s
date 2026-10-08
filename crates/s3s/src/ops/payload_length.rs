//! The `x-s3s-payload-length` extension: a signed declaration of the payload length.
//!
//! The value is the number of bytes the sender delivers after transfer decoding and before
//! content decoding, so it equals the object length on `POST Object` and `PUT Object` and the
//! part length on `UploadPart`.
//!
//! The header is optional: a request without it behaves exactly as before, and the declaration
//! never relaxes another check. The bytes are still counted, a `content-length-range` policy
//! condition is still enforced against them, and a declaration that disagrees with the framing
//! is an error rather than a truncation.

use hyper::HeaderMap;

use crate::error::{S3Error, S3ErrorCode, S3Result};

/// The extension header: the payload length the sender declares.
pub(crate) const X_S3S_PAYLOAD_LENGTH: &str = "x-s3s-payload-length";

/// Reads the declared payload length.
///
/// A missing header is `Ok(None)`. The header must appear at most once, because two values are
/// ambiguous and treating them as absent would silently drop a signed declaration, and its value
/// must be a decimal integer without a sign, whitespace or a leading zero.
pub(crate) fn declared_payload_length(headers: &HeaderMap) -> S3Result<Option<u64>> {
    let mut values = headers.get_all(X_S3S_PAYLOAD_LENGTH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid_declaration("the header appears more than once"));
    }
    let raw = value
        .to_str()
        .map_err(|_| invalid_declaration("the value is not valid text"))?;
    let well_formed =
        !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) && (raw.len() == 1 || !raw.starts_with('0'));
    if !well_formed {
        return Err(invalid_declaration("the value must be a decimal integer without a leading zero"));
    }
    raw.parse::<u64>()
        .map(Some)
        .map_err(|_| invalid_declaration("the value does not fit in 64 bits"))
}

/// Checks the declaration against the length the framing carries.
///
/// The two must agree: fewer bytes are answered with `EntityTooSmall`, more with
/// `EntityTooLarge`, and a request whose framing declares no length at all (a chunked body)
/// cannot carry the header, because the header declares what the framing carries.
pub(crate) fn check_declared_payload_length(declared: u64, framing: Option<u64>) -> S3Result<()> {
    let Some(framing) = framing else {
        return Err(invalid_declaration("the request does not declare its length"));
    };
    if framing < declared {
        return Err(s3_error!(
            EntityTooSmall,
            "The declared payload length {declared} is larger than the {framing} bytes the request carries."
        ));
    }
    if framing > declared {
        return Err(s3_error!(
            EntityTooLarge,
            "The declared payload length {declared} is smaller than the {framing} bytes the request carries."
        ));
    }
    Ok(())
}

/// Enforces the declaration of a request whose framing is `framing`, when it carries one.
pub(crate) fn enforce_declaration(headers: &HeaderMap, framing: Option<u64>) -> S3Result<()> {
    match declared_payload_length(headers)? {
        Some(declared) => check_declared_payload_length(declared, framing),
        None => Ok(()),
    }
}

fn invalid_declaration(reason: &str) -> S3Error {
    S3Error::with_message(S3ErrorCode::InvalidRequest, format!("Invalid {X_S3S_PAYLOAD_LENGTH}: {reason}."))
}

#[cfg(test)]
mod tests {
    use hyper::header::{HeaderName, HeaderValue};

    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                HeaderName::from_static(X_S3S_PAYLOAD_LENGTH),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        headers
    }

    #[test]
    fn a_missing_header_declares_nothing() {
        assert_eq!(declared_payload_length(&HeaderMap::new()).expect("ok"), None);
    }

    #[test]
    fn a_decimal_value_is_the_declaration() {
        assert_eq!(declared_payload_length(&headers(&["0"])).expect("ok"), Some(0));
        assert_eq!(declared_payload_length(&headers(&["1024"])).expect("ok"), Some(1024));
    }

    #[test]
    fn a_repeated_header_is_rejected() {
        let err = declared_payload_length(&headers(&["1", "2"])).expect_err("repeated");
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn malformed_values_are_rejected() {
        for value in ["", "+1", "-1", " 1", "01", "0x10", "1_000", "18446744073709551616"] {
            let err = declared_payload_length(&headers(&[value])).expect_err(value);
            assert_eq!(err.code(), &S3ErrorCode::InvalidRequest, "{value}");
        }
    }

    #[test]
    fn a_framing_that_is_shorter_reports_too_small() {
        let err = check_declared_payload_length(10, Some(9)).expect_err("too small");
        assert_eq!(err.code(), &S3ErrorCode::EntityTooSmall);
    }

    #[test]
    fn a_framing_that_is_longer_reports_too_large() {
        let err = check_declared_payload_length(10, Some(11)).expect_err("too large");
        assert_eq!(err.code(), &S3ErrorCode::EntityTooLarge);
    }

    #[test]
    fn a_matching_framing_passes() {
        check_declared_payload_length(10, Some(10)).expect("match");
    }

    #[test]
    fn an_unknown_framing_cannot_carry_the_header() {
        let err = check_declared_payload_length(10, None).expect_err("no framing");
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn an_absent_header_is_not_enforced() {
        enforce_declaration(&HeaderMap::new(), None).expect("absent");
    }
}

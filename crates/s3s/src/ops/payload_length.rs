// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

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
    parse_declaration(raw).map(Some)
}

/// Parses a declared value, whether it arrived as a header or as a form field.
pub(crate) fn parse_declaration(raw: &str) -> S3Result<u64> {
    let well_formed =
        !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) && (raw.len() == 1 || !raw.starts_with('0'));
    if !well_formed {
        return Err(invalid_declaration("the value must be a decimal integer without a leading zero"));
    }
    raw.parse::<u64>()
        .map_err(|_| invalid_declaration("the value does not fit in 64 bits"))
}

/// Reads the declaration of a form upload, where the value is a form field rather than a header:
/// a presigned `POST` signs the policy document, so a header cannot carry the extension.
pub(crate) fn declared_form_field(fields: &[(String, String)]) -> S3Result<Option<u64>> {
    let mut found: Option<&str> = None;
    for (name, value) in fields {
        if name.eq_ignore_ascii_case(X_S3S_PAYLOAD_LENGTH) {
            if found.is_some() {
                return Err(invalid_declaration("the field appears more than once"));
            }
            found = Some(value.as_str());
        }
    }
    found.map(parse_declaration).transpose()
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

/// The rejection for a `POST Object` that carries the declaration as an HTTP header.
///
/// A form upload is authenticated by the policy document inside the form, and the signature
/// covers that document rather than the headers, so the value has to be a form field that the
/// policy conditions cover. A header cannot carry a signed declaration here.
pub(crate) fn unsigned_on_post() -> S3Error {
    S3Error::with_message(
        S3ErrorCode::AccessDenied,
        format!(
            "There were headers present in the request which were not signed: {X_S3S_PAYLOAD_LENGTH} is a form field on a POST Object, covered by the policy conditions."
        ),
    )
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
    fn a_form_field_carries_the_declaration() {
        let fields = vec![
            ("key".to_owned(), "public/file.txt".to_owned()),
            (X_S3S_PAYLOAD_LENGTH.to_owned(), "1024".to_owned()),
        ];
        assert_eq!(declared_form_field(&fields).expect("ok"), Some(1024));
        assert_eq!(declared_form_field(&[]).expect("ok"), None);
    }

    #[test]
    fn a_repeated_form_field_is_rejected() {
        let fields = vec![
            (X_S3S_PAYLOAD_LENGTH.to_owned(), "1".to_owned()),
            (X_S3S_PAYLOAD_LENGTH.to_owned(), "2".to_owned()),
        ];
        let err = declared_form_field(&fields).expect_err("repeated");
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn an_absent_header_is_not_enforced() {
        enforce_declaration(&HeaderMap::new(), None).expect("absent");
    }

    /// The only test that goes through the wiring rather than the two helpers: with the
    /// enforcement short-circuited this one fails, which is what keeps the wiring honest.
    #[test]
    fn the_enforcement_rejects_a_declaration_that_disagrees_with_the_framing() {
        let err = enforce_declaration(&headers(&["10"]), Some(9)).expect_err("too small");
        assert_eq!(err.code(), &S3ErrorCode::EntityTooSmall);
        let err = enforce_declaration(&headers(&["10"]), Some(11)).expect_err("too large");
        assert_eq!(err.code(), &S3ErrorCode::EntityTooLarge);
        let err = enforce_declaration(&headers(&["10"]), None).expect_err("no framing");
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
        enforce_declaration(&headers(&["10"]), Some(10)).expect("match");
    }
}

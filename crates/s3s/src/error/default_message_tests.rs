// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::*;

use crate::ops;

fn xml_body(err: S3Error) -> String {
    let res = ops::serialize_error(err, false).unwrap();
    let bytes = res.body.bytes().unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn every_static_code_has_a_non_empty_single_line_default_message() {
    for code in S3ErrorCode::STATIC_CODE_LIST {
        let err = S3Error::new(S3ErrorCode::from_bytes(code.as_bytes()).unwrap());
        let message = err.message().expect("every static code must have a default message");
        assert!(!message.is_empty(), "{code}");
        assert!(!message.contains('\n'), "default message must be single-line: {code}");
    }
}

#[test]
fn signature_does_not_match_uses_aws_default_message() {
    let err = s3_error!(SignatureDoesNotMatch);
    assert_eq!(
        err.message(),
        Some(
            "The request signature we calculated does not match the signature you provided. \
              Check your AWS secret access key and signing method. For more information, see \
              REST Authentication and SOAP Authentication for details."
        )
    );
}

#[test]
fn access_denied_uses_aws_default_message() {
    let err = s3_error!(AccessDenied);
    assert_eq!(err.message(), Some("Access Denied"));
}

#[test]
fn override_messages_are_concise() {
    assert_eq!(s3_error!(InvalidRequest).message(), Some("Invalid request."));
    assert_eq!(s3_error!(InvalidArgument).message(), Some("Invalid argument."));
    assert_eq!(
        s3_error!(IllegalLocationConstraintException).message(),
        Some("The specified location constraint is not valid.")
    );
    assert_eq!(s3_error!(MissingAuthenticationToken).message(), Some("The request was not signed."));
}

#[test]
fn missing_authentication_token_has_no_mojibake() {
    let err = s3_error!(MissingAuthenticationToken);
    let message = err.message().unwrap();
    assert!(!message.contains('\u{c2}'));
    assert!(!message.contains('\u{a0}'));
}

#[test]
fn xml_contains_default_message() {
    let xml = xml_body(s3_error!(AccessDenied));
    assert!(xml.contains("<Message>Access Denied</Message>"), "{xml}");
}

#[test]
fn with_message_overrides_default() {
    let err = s3_error!(AccessDenied, "custom text");
    assert_eq!(err.message(), Some("custom text"));
    let xml = xml_body(err);
    assert!(xml.contains("<Message>custom text</Message>"), "{xml}");
    assert!(!xml.contains("Access Denied"), "{xml}");
}

#[test]
fn custom_code_has_no_message() {
    let err = S3Error::new(S3ErrorCode::Custom(bytestring::ByteString::from("Xyz")));
    assert_eq!(err.message(), None);
    let xml = xml_body(err);
    assert!(!xml.contains("<Message>"), "{xml}");
}

#[test]
fn display_contains_default_message() {
    let err = S3Error::new(S3ErrorCode::AccessDenied);
    let s = err.to_string();
    assert!(s.contains("Access Denied"), "{s}");
}

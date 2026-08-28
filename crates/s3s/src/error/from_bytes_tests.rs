// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::S3ErrorCode;

fn mixed_case(code: &str) -> String {
    let lowercase = code.to_ascii_lowercase();

    for idx in lowercase
        .bytes()
        .enumerate()
        .filter_map(|(idx, byte)| byte.is_ascii_alphabetic().then_some(idx))
    {
        let mut mixed = lowercase.clone();
        mixed.replace_range(idx..=idx, &char::from(mixed.as_bytes()[idx].to_ascii_uppercase()).to_string());
        if mixed != code {
            return mixed;
        }
    }

    assert_ne!(lowercase, code, "missing alternate case for {code}");
    lowercase
}

#[test]
fn from_bytes_matches_static_codes() {
    let mut saw_code = false;

    for &code in S3ErrorCode::STATIC_CODE_LIST {
        saw_code = true;

        let exact = S3ErrorCode::from_bytes(code.as_bytes()).unwrap();
        assert_eq!(format!("{exact:?}"), code);
        assert_eq!(exact.as_str(), code);

        let lowercase = code.to_ascii_lowercase();
        assert_eq!(
            S3ErrorCode::from_bytes(lowercase.as_bytes()),
            Some(exact.clone()),
            "lowercase lookup should match {code}"
        );

        let mixed = mixed_case(code);
        assert_ne!(mixed, code, "mixed-case input should differ from {code}");
        assert_eq!(
            S3ErrorCode::from_bytes(mixed.as_bytes()),
            Some(exact),
            "mixed-case lookup should match {code}"
        );
    }

    assert!(saw_code, "generated STATIC_CODE_LIST should not be empty");
}

#[test]
fn from_bytes_handles_custom_and_invalid_utf8() {
    let custom = "vendor-specific-error";
    let parsed = S3ErrorCode::from_bytes(custom.as_bytes()).unwrap();

    assert!(matches!(parsed, S3ErrorCode::Custom(_)));
    assert_eq!(parsed.as_str(), custom);
    assert_eq!(S3ErrorCode::from_bytes(&[0xff]), None);
}

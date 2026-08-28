// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::S3ErrorCode;

#[cfg(feature = "minio")]
const GENERATED_SOURCE: &str = include_str!("generated_minio.rs");
#[cfg(not(feature = "minio"))]
const GENERATED_SOURCE: &str = include_str!("generated.rs");

fn static_code_list() -> impl Iterator<Item = &'static str> {
    let (_, tail) = GENERATED_SOURCE
        .split_once("const STATIC_CODE_LIST: &'static [&'static str] = &[\n")
        .unwrap();
    let (list, _) = tail.split_once("    ];").unwrap();

    list.lines().map(|line| {
        let literal = line.trim().strip_suffix(',').unwrap().trim();
        literal
            .strip_prefix('"')
            .and_then(|literal| literal.strip_suffix('"'))
            .unwrap()
    })
}

fn mixed_case(code: &str) -> String {
    code.bytes()
        .enumerate()
        .map(|(idx, byte)| {
            let byte = if idx & 1 == 0 {
                byte.to_ascii_lowercase()
            } else {
                byte.to_ascii_uppercase()
            };
            char::from(byte)
        })
        .collect()
}

#[test]
fn from_bytes_matches_static_codes() {
    let mut saw_code = false;

    for code in static_code_list() {
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

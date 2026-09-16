// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Region and service validation of the credential scope.

use crate::ops::signature::*;

use crate::config::S3Config;
use crate::error::S3ErrorCode;

#[test]
fn sig_v4_region_validation_is_optional_and_reports_mismatch() {
    let mut config = S3Config::default();
    validate_sig_v4_region("us-east-1", &config).expect("unset expected region should accept any region");

    config.expected_region = Some("us-west-2".parse().expect("valid test region"));
    validate_sig_v4_region("us-west-2", &config).expect("matching region should be accepted");

    let err = validate_sig_v4_region("us-east-1", &config).expect_err("mismatched region should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    assert_eq!(
        err.message(),
        Some("The authorization header is malformed; the region is wrong; expecting 'us-west-2'.")
    );
}

#[test]
fn sig_v4_service_validation_uses_configured_allowlist() {
    let mut config = S3Config::default();
    validate_sig_v4_service("s3", &config).expect("default services should include s3");
    validate_sig_v4_service("sts", &config).expect("default services should include sts");

    let err = validate_sig_v4_service("s3tables", &config).expect_err("custom services should be rejected by default");
    assert_eq!(err.code(), &S3ErrorCode::NotImplemented);

    config.sig_v4_allowed_services.push("s3tables".to_owned());
    validate_sig_v4_service("s3tables", &config).expect("configured service should be accepted");
}

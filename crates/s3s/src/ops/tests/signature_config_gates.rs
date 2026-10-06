// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Region and service validation of the credential scope.

use crate::ops::signature::*;

use crate::config::S3Config;
use crate::error::S3ErrorCode;

#[test]
fn sig_v4_region_validation_is_optional_and_reports_mismatch() {
    let mut config = S3Config::default();
    validate_sig_v4_region("us-east-1", &config, SigV4Mechanism::HeaderAuth)
        .expect("unset expected region should accept any region");

    config.expected_region = Some("us-west-2".parse().expect("valid test region"));
    validate_sig_v4_region("us-west-2", &config, SigV4Mechanism::HeaderAuth).expect("matching region should be accepted");

    let err = validate_sig_v4_region("us-east-1", &config, SigV4Mechanism::HeaderAuth)
        .expect_err("mismatched region should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    assert_eq!(
        err.message(),
        Some("The authorization header is malformed; the region 'us-east-1' is wrong; expecting 'us-west-2'")
    );
}

/// The same rejected region is reported with the code and the message of the mechanism that
/// carried the credential; with the endpoint's region configured the text is the one Amazon S3
/// returns.
#[test]
fn sig_v4_region_rejections_name_the_path() {
    let config = S3Config {
        expected_region: Some("ap-southeast-2".parse().expect("valid test region")),
        ..Default::default()
    };

    for (mechanism, code, message) in [
        (
            SigV4Mechanism::HeaderAuth,
            S3ErrorCode::AuthorizationHeaderMalformed,
            "The authorization header is malformed; the region 'US-EAST-1' is wrong; expecting 'ap-southeast-2'",
        ),
        (
            SigV4Mechanism::PresignedUrl,
            S3ErrorCode::AuthorizationQueryParametersError,
            "Error parsing the X-Amz-Credential parameter; the region 'US-EAST-1' is wrong; expecting 'ap-southeast-2'",
        ),
        (
            SigV4Mechanism::PostPolicy,
            S3ErrorCode::InvalidArgument,
            "the region 'US-EAST-1' is wrong; expecting 'ap-southeast-2'",
        ),
    ] {
        // A region that is not a valid region name and a region that is not the configured one
        // are reported the same way.
        for region in ["US-EAST-1", "us-west-2"] {
            let err = validate_sig_v4_region(region, &config, mechanism)
                .expect_err("a region that does not belong to this endpoint must be rejected");
            assert_eq!(*err.code(), code, "mechanism {mechanism:?}");
            let expected = message.replace("US-EAST-1", region);
            assert_eq!(err.message(), Some(expected.as_str()), "mechanism {mechanism:?}");
        }
    }

    // Without a configured region the value is still named, but nothing can be expected.
    let config = S3Config::default();
    for (mechanism, code, message) in [
        (
            SigV4Mechanism::HeaderAuth,
            S3ErrorCode::AuthorizationHeaderMalformed,
            "The authorization header is malformed; the region 'US-EAST-1' is wrong",
        ),
        (
            SigV4Mechanism::PresignedUrl,
            S3ErrorCode::AuthorizationQueryParametersError,
            "Error parsing the X-Amz-Credential parameter; the region 'US-EAST-1' is wrong",
        ),
        (
            SigV4Mechanism::PostPolicy,
            S3ErrorCode::InvalidArgument,
            "the region 'US-EAST-1' is wrong",
        ),
    ] {
        let err = validate_sig_v4_region("US-EAST-1", &config, mechanism)
            .expect_err("a region that is not a valid region name must be rejected");
        assert_eq!(*err.code(), code, "mechanism {mechanism:?}");
        assert_eq!(err.message(), Some(message), "mechanism {mechanism:?}");
    }
}

#[test]
fn sig_v4_region_rejects_empty_region() {
    let config = S3Config::default();

    // The service names the same violation differently on each path.
    for (mechanism, code, message) in [
        (
            SigV4Mechanism::HeaderAuth,
            S3ErrorCode::AuthorizationHeaderMalformed,
            "The authorization header is malformed; a non-empty region must be provided in the credential.",
        ),
        (
            SigV4Mechanism::PresignedUrl,
            S3ErrorCode::AuthorizationQueryParametersError,
            "Error parsing the X-Amz-Credential parameter; a non-empty region must be provided in the credential.",
        ),
        (
            SigV4Mechanism::PostPolicy,
            S3ErrorCode::InvalidArgument,
            "a non-empty region must be provided in the credential.",
        ),
    ] {
        let err = validate_sig_v4_region("", &config, mechanism).expect_err("empty region should be rejected");
        assert_eq!(*err.code(), code, "mechanism {mechanism:?}");
        assert_eq!(err.message(), Some(message), "mechanism {mechanism:?}");
    }
}

/// The escape hatch for clients that sign with an empty region: the deployment accepts one, and
/// every other region rule still applies. The field is honored in every build.
#[test]
fn sig_v4_region_accepts_empty_region_when_configured() {
    let config = S3Config {
        sig_v4_allow_empty_region: true,
        ..Default::default()
    };

    for mechanism in [
        SigV4Mechanism::HeaderAuth,
        SigV4Mechanism::PresignedUrl,
        SigV4Mechanism::PostPolicy,
    ] {
        validate_sig_v4_region("", &config, mechanism).expect("the escape hatch accepts an empty region");
    }

    let err =
        validate_sig_v4_region("US-EAST-1", &config, SigV4Mechanism::HeaderAuth).expect_err("the other region rules still apply");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);

    let lenient = S3Config {
        sig_v4_max_region_len: Some(8),
        ..config
    };
    let err = validate_sig_v4_region("us-east-1", &lenient, SigV4Mechanism::HeaderAuth)
        .expect_err("the length bound still applies with the escape hatch on");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
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

#[test]
fn sig_v4_region_validation_bounds_the_region_length_in_bytes() {
    let config = S3Config::default();

    validate_sig_v4_region(&"r".repeat(64), &config, SigV4Mechanism::HeaderAuth).expect("a 64-byte region should be accepted");

    for region in ["r".repeat(65), "r".repeat(200)] {
        let err = validate_sig_v4_region(&region, &config, SigV4Mechanism::HeaderAuth)
            .expect_err("a region longer than 64 bytes should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
        assert_eq!(
            err.message(),
            Some("The authorization header is malformed; the region is longer than 64 bytes.")
        );
    }

    // The length cap counts bytes, but a region has to be a valid region name first: a
    // multi-byte string is reported as a region that does not belong to this endpoint.
    let err = validate_sig_v4_region(&"地".repeat(22), &config, SigV4Mechanism::HeaderAuth)
        .expect_err("a region with bytes outside [a-z0-9-] should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    assert_eq!(
        err.message(),
        Some("The authorization header is malformed; the region '地地地地地地地地地地地地地地地地地地地地地地' is wrong")
    );

    // The same cap is reported with the code of the path that carried the credential.
    for (mechanism, code, message) in [
        (
            SigV4Mechanism::PresignedUrl,
            S3ErrorCode::AuthorizationQueryParametersError,
            "Error parsing the X-Amz-Credential parameter; the region is longer than 64 bytes.",
        ),
        (
            SigV4Mechanism::PostPolicy,
            S3ErrorCode::InvalidArgument,
            "the region is longer than 64 bytes.",
        ),
    ] {
        let err = validate_sig_v4_region(&"r".repeat(65), &config, mechanism)
            .expect_err("a region longer than 64 bytes should be rejected");
        assert_eq!(*err.code(), code, "mechanism {mechanism:?}");
        assert_eq!(err.message(), Some(message), "mechanism {mechanism:?}");
    }

    // The limit is configurable.
    let config = S3Config {
        sig_v4_max_region_len: Some(8),
        ..Default::default()
    };
    validate_sig_v4_region(&"r".repeat(8), &config, SigV4Mechanism::HeaderAuth)
        .expect("a region at the configured limit should be accepted");
    let err = validate_sig_v4_region("us-east-1", &config, SigV4Mechanism::HeaderAuth)
        .expect_err("a region longer than the configured limit should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    assert_eq!(
        err.message(),
        Some("The authorization header is malformed; the region is longer than 8 bytes.")
    );

    // Disabling the limit accepts any length.
    let config = S3Config {
        sig_v4_max_region_len: None,
        ..Default::default()
    };
    validate_sig_v4_region(&"r".repeat(70), &config, SigV4Mechanism::HeaderAuth)
        .expect("a disabled limit should accept any region length");

    // The length limit is independent of the configured region, and it is reported first.
    let config = S3Config {
        expected_region: Some("us-west-2".parse().expect("valid test region")),
        ..Default::default()
    };
    let err = validate_sig_v4_region(&"r".repeat(65), &config, SigV4Mechanism::HeaderAuth)
        .expect_err("the length limit should win over a mismatch");
    assert_eq!(
        err.message(),
        Some("The authorization header is malformed; the region is longer than 64 bytes.")
    );
}

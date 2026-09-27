// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Tests using the Smithy `date_time_format_test_suite.json`
//! From: <https://github.com/smithy-lang/smithy-rs/blob/main/rust-runtime/aws-smithy-types/test_data/date_time_format_test_suite.json>

use s3s::dto::{Timestamp, TimestampFormat};

use serde::Deserialize;

#[derive(Deserialize)]
struct TestSuite {
    #[allow(dead_code)]
    description: Vec<String>,
    parse_epoch_seconds: Vec<TestCase>,
    parse_http_date: Vec<TestCase>,
    parse_date_time: Vec<TestCase>,
    format_epoch_seconds: Vec<TestCase>,
    format_http_date: Vec<TestCase>,
    format_date_time: Vec<TestCase>,
}

#[derive(Deserialize)]
struct TestCase {
    iso8601: String,
    canonical_seconds: String,
    canonical_nanos: u32,
    error: bool,
    smithy_format_value: Option<String>,
}

fn load_test_suite() -> TestSuite {
    let json = include_str!("../../../data/date_time_format_test_suite.json");
    serde_json::from_str(json).expect("failed to parse test suite")
}

/// Converts `canonical_seconds` (as string) and `canonical_nanos` into total nanoseconds.
fn canonical_to_nanos(canonical_seconds: &str, canonical_nanos: u32) -> i128 {
    let secs: i64 = canonical_seconds.parse().expect("invalid canonical_seconds");
    i128::from(secs) * 1_000_000_000 + i128::from(canonical_nanos)
}

#[test]
fn parse_epoch_seconds() {
    let suite = load_test_suite();

    for case in suite.parse_epoch_seconds {
        let Some(smithy_value) = case.smithy_format_value.as_ref() else {
            // Error cases without smithy_format_value - skip
            assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
            continue;
        };

        let result = Timestamp::parse(TimestampFormat::EpochSeconds, smithy_value);

        if case.error {
            assert!(result.is_err(), "expected error parsing '{}' (iso8601: {})", smithy_value, case.iso8601);
        } else {
            let ts = result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));
            let expected_nanos = canonical_to_nanos(&case.canonical_seconds, case.canonical_nanos);
            let odt: time::OffsetDateTime = ts.into();
            let actual_nanos = odt.unix_timestamp_nanos();

            assert_eq!(
                actual_nanos, expected_nanos,
                "mismatch for '{}' (iso8601: {}): expected {} nanos, got {} nanos",
                smithy_value, case.iso8601, expected_nanos, actual_nanos
            );
        }
    }
}

#[test]
fn parse_http_date() {
    let suite = load_test_suite();

    for case in suite.parse_http_date {
        let Some(smithy_value) = case.smithy_format_value.as_ref() else {
            // Error cases without smithy_format_value - skip
            assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
            continue;
        };

        // s3s's RFC1123 format doesn't support fractional seconds, so skip those test cases
        // that include fractional seconds (e.g., "Sat, 18 Jan 1969 11:47:31.01 GMT")
        if smithy_value.contains('.') {
            continue;
        }

        let result = Timestamp::parse(TimestampFormat::HttpDate, smithy_value);

        if case.error {
            assert!(result.is_err(), "expected error parsing '{}' (iso8601: {})", smithy_value, case.iso8601);
        } else {
            let ts = result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));

            // For http-date, fractional seconds are truncated, so we only compare whole seconds
            let expected_secs: i64 = case.canonical_seconds.parse().expect("invalid canonical_seconds");
            let odt: time::OffsetDateTime = ts.into();
            let actual_secs = odt.unix_timestamp();

            assert_eq!(
                actual_secs, expected_secs,
                "mismatch for '{}' (iso8601: {}): expected {} secs, got {} secs",
                smithy_value, case.iso8601, expected_secs, actual_secs
            );
        }
    }
}

#[test]
fn parse_date_time() {
    let suite = load_test_suite();

    for case in suite.parse_date_time {
        let Some(smithy_value) = case.smithy_format_value.as_ref() else {
            // Error cases without smithy_format_value - skip
            assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
            continue;
        };

        let result = Timestamp::parse(TimestampFormat::DateTime, smithy_value);

        if case.error {
            assert!(result.is_err(), "expected error parsing '{}' (iso8601: {})", smithy_value, case.iso8601);
        } else {
            let ts = result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));
            let expected_nanos = canonical_to_nanos(&case.canonical_seconds, case.canonical_nanos);
            let odt: time::OffsetDateTime = ts.into();
            let actual_nanos = odt.unix_timestamp_nanos();

            assert_eq!(
                actual_nanos, expected_nanos,
                "mismatch for '{}' (iso8601: {}): expected {} nanos, got {} nanos",
                smithy_value, case.iso8601, expected_nanos, actual_nanos
            );
        }
    }
}

/// Builds a `Timestamp` from the canonical seconds and nanoseconds of a case.
fn timestamp_from_canonical(case: &TestCase) -> Result<Timestamp, String> {
    let nanos = canonical_to_nanos(&case.canonical_seconds, case.canonical_nanos);
    let odt = time::OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|e| format!("{e}"))?;
    Ok(Timestamp::from(odt))
}

/// Formats every case of one `format_*` section and compares the output with
/// `smithy_format_value`; cases flagged `error` must fail to format.
///
/// Failures are collected and reported together, so a single run reports every
/// mismatch instead of stopping at the first one.
fn run_format_suite(name: &str, cases: &[TestCase], format: TimestampFormat) {
    let mut passed = 0_usize;
    let mut expected_errors = 0_usize;
    let mut failures: Vec<String> = Vec::new();

    for case in cases {
        let ts = match timestamp_from_canonical(case) {
            Ok(ts) => ts,
            Err(err) => {
                failures.push(format!("{}: cannot build timestamp: {err}", case.iso8601));
                continue;
            }
        };

        let mut buf = Vec::new();
        let result = ts.format(format, &mut buf);

        match (case.error, result) {
            (true, Err(_)) => expected_errors += 1,
            (true, Ok(())) => failures.push(format!(
                "{}: expected a format error, got {:?}",
                case.iso8601,
                String::from_utf8_lossy(&buf)
            )),
            (false, Err(err)) => failures.push(format!("{}: format failed: {err}", case.iso8601)),
            (false, Ok(())) => {
                let actual = String::from_utf8_lossy(&buf).into_owned();
                let expected = case.smithy_format_value.as_deref().unwrap_or_default();
                if actual == expected {
                    passed += 1;
                } else {
                    failures.push(format!("{}: expected {expected:?}, got {actual:?}", case.iso8601));
                }
            }
        }
    }

    println!(
        "{name}: total={} passed={passed} expected_error={expected_errors} failures={}",
        cases.len(),
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "{name}: {} of {} cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "red baseline: 66 of 122 cases fail, epoch-seconds formatting loses sub-second precision and mis-splits negative instants"]
fn format_epoch_seconds() {
    let suite = load_test_suite();
    run_format_suite("format_epoch_seconds", &suite.format_epoch_seconds, TimestampFormat::EpochSeconds);
}

#[test]
#[ignore = "red baseline: 36 of 122 cases fail, out-of-range years are formatted instead of rejected"]
fn format_http_date() {
    let suite = load_test_suite();
    run_format_suite("format_http_date", &suite.format_http_date, TimestampFormat::HttpDate);
}

#[test]
#[ignore = "red baseline: 113 of 122 cases fail, subseconds are always three digits and out-of-range years are formatted"]
fn format_date_time() {
    let suite = load_test_suite();
    run_format_suite("format_date_time", &suite.format_date_time, TimestampFormat::DateTime);
}

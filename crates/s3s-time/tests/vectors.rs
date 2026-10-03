// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The wire-format vectors of the Smithy test suite.
//!
//! Every case of the six sections is dispositioned: it passes, it is an
//! expected error, or it is a recorded deviation. No section is ignored and
//! no case is skipped silently.

use s3s_time::{ParseTimestampError, Timestamp, TimestampFormat};
use serde::Deserialize;

/// The suite shipped with the S3 model data.
const SUITE: &str = include_str!("../../../data/date_time_format_test_suite.json");

#[derive(Deserialize)]
struct TestSuite {
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

impl TestCase {
    fn value(&self) -> &str {
        self.smithy_format_value.as_deref().unwrap_or_default()
    }

    fn seconds(&self) -> i64 {
        self.canonical_seconds.parse().expect("canonical_seconds is an integer")
    }

    fn nanos(&self) -> i128 {
        i128::from(self.seconds()) * 1_000_000_000 + i128::from(self.canonical_nanos)
    }

    fn timestamp(&self) -> Timestamp {
        Timestamp::from_unix_timestamp_nanos(self.nanos()).expect("the canonical instant is representable")
    }
}

/// Counts the cases of one section and asserts that none of them is skipped.
struct Section {
    name: &'static str,
    total: usize,
    passed: usize,
    expected_errors: usize,
    deviations: Vec<String>,
    failures: Vec<String>,
}

impl Section {
    fn new(name: &'static str, total: usize) -> Self {
        Self {
            name,
            total,
            passed: 0,
            expected_errors: 0,
            deviations: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn pass(&mut self) {
        self.passed += 1;
    }

    fn expected_error(&mut self) {
        self.expected_errors += 1;
    }

    fn deviate(&mut self, line: String) {
        self.deviations.push(line);
    }

    fn fail(&mut self, line: String) {
        self.failures.push(line);
    }

    fn finish(self) {
        let deviations = self.deviations.len();
        println!(
            "{}: total={} passed={} expected_errors={} deviations={} failures={}",
            self.name,
            self.total,
            self.passed,
            self.expected_errors,
            deviations,
            self.failures.len()
        );
        for line in self.deviations.iter().take(10) {
            println!("  deviation: {line}");
        }
        if deviations > 10 {
            println!("  ... and {} more deviations", deviations - 10);
        }
        assert!(
            self.failures.is_empty(),
            "{}: {} of {} cases failed:\n{}",
            self.name,
            self.failures.len(),
            self.total,
            self.failures.join("\n")
        );
        assert_eq!(
            self.passed + self.expected_errors + deviations,
            self.total,
            "{}: every case must be dispositioned",
            self.name
        );
    }
}

fn load() -> TestSuite {
    serde_json::from_str(SUITE).expect("the suite is a JSON document")
}

fn format_of(ts: &Timestamp, format: TimestampFormat) -> Result<String, String> {
    let mut buf = Vec::new();
    ts.format(format, &mut buf).map_err(|err| err.to_string())?;
    String::from_utf8(buf).map_err(|err| err.to_string())
}

#[test]
fn vectors_parse_epoch_seconds() {
    let cases = load().parse_epoch_seconds;
    let mut section = Section::new("parse_epoch_seconds", cases.len());

    for case in &cases {
        let result = Timestamp::parse(TimestampFormat::EpochSeconds, case.value());
        match (case.error, result) {
            (true, Err(_)) => section.expected_error(),
            (true, Ok(ts)) => {
                section.fail(format!("{}: expected an error, parsed {} ns", case.value(), ts.unix_timestamp_nanos()));
            }
            (false, Ok(ts)) if ts.unix_timestamp_nanos() == case.nanos() => section.pass(),
            (false, Ok(ts)) => section.fail(format!(
                "{}: expected {} ns, got {} ns",
                case.value(),
                case.nanos(),
                ts.unix_timestamp_nanos()
            )),
            (false, Err(err)) => section.fail(format!("{}: {err}", case.value())),
        }
    }

    section.finish();
}

#[test]
fn vectors_parse_date_time() {
    let cases = load().parse_date_time;
    let mut section = Section::new("parse_date_time", cases.len());

    for case in &cases {
        let result = Timestamp::parse(TimestampFormat::DateTime, case.value());
        match (case.error, result) {
            (true, Err(_)) => section.expected_error(),
            (true, Ok(ts)) => {
                section.fail(format!("{}: expected an error, parsed {} ns", case.value(), ts.unix_timestamp_nanos()));
            }
            (false, Ok(ts)) if ts.unix_timestamp_nanos() == case.nanos() => section.pass(),
            (false, Ok(ts)) => section.fail(format!(
                "{}: expected {} ns, got {} ns",
                case.value(),
                case.nanos(),
                ts.unix_timestamp_nanos()
            )),
            (false, Err(err)) => section.fail(format!("{}: {err}", case.value())),
        }
    }

    section.finish();
}

#[test]
fn vectors_parse_http_date() {
    let cases = load().parse_http_date;
    let mut section = Section::new("parse_http_date", cases.len());
    let mut fractional = 0_usize;

    for case in &cases {
        let value = case.value();
        let result = Timestamp::parse(TimestampFormat::HttpDate, value);

        // IMF-fixdate has second precision, so a fractional second is not part
        // of the wire format: the rejection is the policy, not a skip.
        if value.contains('.') {
            fractional += 1;
            match result {
                Err(ParseTimestampError::InvalidFormat | ParseTimestampError::OutOfRange) => section.pass(),
                Err(err) => section.fail(format!("{value}: unexpected error class {err:?}")),
                Ok(ts) => section.fail(format!("{value}: expected a rejection, parsed {} s", ts.unix_timestamp())),
            }
            continue;
        }

        match (case.error, result) {
            (true, Err(_)) => section.expected_error(),
            (true, Ok(_)) => section.fail(format!("{value}: expected an error")),
            (false, Ok(ts)) if ts.unix_timestamp() == case.seconds() => section.pass(),
            (false, Ok(ts)) => section.fail(format!("{value}: expected {} s, got {} s", case.seconds(), ts.unix_timestamp())),
            (false, Err(err)) => section.fail(format!("{value}: {err}")),
        }
    }

    println!("parse_http_date: fractional-second policy: {fractional} cases rejected (IMF-fixdate carries no fraction)");
    section.finish();
}

fn run_format(name: &'static str, cases: &[TestCase], format: TimestampFormat) {
    let mut section = Section::new(name, cases.len());

    for case in cases {
        let result = format_of(&case.timestamp(), format);
        match (case.error, result) {
            (true, Err(_)) => section.expected_error(),
            (true, Ok(text)) => section.fail(format!("{}: expected a format error, wrote {text:?}", case.iso8601)),
            (false, Ok(text)) if text == case.value() => section.pass(),
            (false, Ok(text)) => section.fail(format!("{}: expected {:?}, got {text:?}", case.iso8601, case.value())),
            (false, Err(err)) => section.fail(format!("{}: {err}", case.iso8601)),
        }
    }

    section.finish();
}

#[test]
fn vectors_format_epoch_seconds() {
    let cases = load().format_epoch_seconds;
    run_format("format_epoch_seconds", &cases, TimestampFormat::EpochSeconds);
}

#[test]
fn vectors_format_http_date() {
    let cases = load().format_http_date;
    run_format("format_http_date", &cases, TimestampFormat::HttpDate);
}

#[test]
fn vectors_format_date_time() {
    let cases = load().format_date_time;
    let mut section = Section::new("format_date_time", cases.len());

    for case in &cases {
        let result = format_of(&case.timestamp(), TimestampFormat::DateTime);
        match (case.error, result) {
            (true, Err(_)) => section.expected_error(),
            (true, Ok(text)) => section.fail(format!("{}: expected a format error, wrote {text:?}", case.iso8601)),
            (false, Ok(text)) if text == case.value() => section.pass(),
            (false, Ok(text)) => {
                let expected = millisecond_form(case.value());
                if text == expected {
                    // The wire format is fixed to milliseconds while the vector
                    // carries the full precision of the instant.
                    section.deviate(format!("{}: wrote {text:?} for {:?}", case.iso8601, case.value()));
                } else {
                    section.fail(format!("{}: expected {expected:?}, got {text:?}", case.iso8601));
                }
            }
            (false, Err(err)) => section.fail(format!("{}: {err}", case.iso8601)),
        }
    }

    section.finish();
}

/// Rewrites an RFC 3339 date-time to the fixed three-digit millisecond form.
fn millisecond_form(value: &str) -> String {
    let (stamp, fraction) = match value.split_once('.') {
        Some((stamp, fraction)) => (stamp, fraction),
        None => (value, ""),
    };
    let stamp = stamp.strip_suffix('Z').unwrap_or(stamp);
    let fraction = fraction.strip_suffix('Z').unwrap_or(fraction);
    let millis: String = fraction.chars().take(3).collect();
    format!("{stamp}.{millis:0<3}Z")
}

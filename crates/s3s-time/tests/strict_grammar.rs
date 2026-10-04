// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The strict grammar of the date-time and HTTP-date readers.
//!
//! Both readers check the input against the profile of the format before the date
//! library sees it, because the library accepts more than the wire format defines.
//! The expectations below are the behaviour of the pre-migration implementation,
//! recorded case by case in the frozen fixture of the differential harness.

use s3s_time::{ParseTimestampError, Timestamp, TimestampFormat};

/// Reads an instant as the canonical seconds and nanoseconds.
fn canonical(text: &str, format: TimestampFormat) -> (i64, u32) {
    let timestamp = Timestamp::parse(format, text).unwrap_or_else(|error| panic!("{text:?}: {error}"));
    (timestamp.unix_seconds(), timestamp.subsec_nanos())
}

/// Reads the contract name of the parse error of a rejected input.
fn error_name(text: &str, format: TimestampFormat) -> &'static str {
    match Timestamp::parse(format, text) {
        Ok(timestamp) => panic!("{text:?} was accepted as {}:{}", timestamp.unix_seconds(), timestamp.subsec_nanos()),
        Err(ParseTimestampError::InvalidFormat) => "InvalidFormat",
        Err(ParseTimestampError::OutOfRange) => "OutOfRange",
        Err(ParseTimestampError::FractionTooLong) => "FractionTooLong",
        Err(ParseTimestampError::Overflow) => "Overflow",
    }
}

#[test]
fn date_time_accepts_the_profile() {
    let cases = [
        ("1985-04-12T23:20:50Z", (482_196_050, 0)),
        ("1985-04-12T23:20:50.123456789Z", (482_196_050, 123_456_789)),
        ("1985-04-12t23:20:50z", (482_196_050, 0)),
        ("1985-04-12 23:20:50Z", (482_196_050, 0)),
        ("1985-04-12\t23:20:50Z", (482_196_050, 0)),
        ("1985-04-12T23:20:50+00:00", (482_196_050, 0)),
        ("1985-04-12T23:20:50-00:00", (482_196_050, 0)),
        ("1985-04-12T23:20:50+02:00", (482_188_850, 0)),
        ("1985-04-12T23:20:50-05:30", (482_215_850, 0)),
        // Interior values of the offset fields: a two-digit hour at the high end
        // of its range, and a minute that ends in a digit other than zero.
        ("1985-04-12T23:20:50+19:00", (482_127_650, 0)),
        ("1985-04-12T23:20:50+00:05", (482_195_750, 0)),
        ("1985-04-12T23:20:50.5+01:00", (482_192_450, 500_000_000)),
        ("2020-02-29T12:00:00Z", (1_582_977_600, 0)),
        ("0000-01-01T00:00:00Z", (-62_167_219_200, 0)),
        ("9999-12-30T22:00:00.999999999Z", (253_402_207_200, 999_999_999)),
    ];
    for (text, expected) in cases {
        assert_eq!(canonical(text, TimestampFormat::DateTime), expected, "{text}");
    }
}

#[test]
fn date_time_rejects_the_spellings_outside_the_profile() {
    let cases = [
        "1985-04-12T23:20:50+0200",
        "1985-04-12T23:20:50+02",
        "1985-04-12T23:20:50+24:00",
        "1985-04-12T23:20:50+00:60",
        "1985-04-12T23:20Z",
        "1985-04-12T23:20:50,52Z",
        "1985-04-12T23:20:50.52Z[UTC]",
        "1985-04-12T23:20:50.Z",
        "1985-04-12T23:20:50",
        "1985-04-12",
        "-0001-01-01T00:00:00Z",
        "-009999-01-01T00:00:00Z",
        "+10000-01-01T00:00:00Z",
        "1985-04-12T23:20:50Zx",
        " 1985-04-12T23:20:50Z",
        "1985-04-12T23:20:50Z\n",
        "",
    ];
    for text in cases {
        assert_eq!(error_name(text, TimestampFormat::DateTime), "InvalidFormat", "{text:?}");
    }
}

#[test]
fn date_time_rejects_a_malformed_offset_without_panicking() {
    // Both offset fields are checked field by field before they are combined, so a
    // non-digit in one of them must be a rejection and never an arithmetic
    // overflow; the assertion is written with `catch_unwind` so a panic is a
    // failure of this test rather than a green run.
    let outcome = std::panic::catch_unwind(|| Timestamp::parse(TimestampFormat::DateTime, "1985-04-12T23:20:50+0/:00"));
    match outcome {
        Ok(Err(ParseTimestampError::InvalidFormat)) => {}
        Ok(other) => panic!("expected an InvalidFormat rejection, got {other:?}"),
        Err(payload) => panic!("the reader panicked on a malformed offset: {payload:?}"),
    }
}

#[test]
fn date_time_rejects_a_truncated_clock_without_panicking() {
    // A clock that is too short to carry a zone must be a rejection and never an
    // index underflow in the zone splitter; the assertion is written with
    // `catch_unwind` so a panic is a failure of this test rather than a green run.
    let outcome = std::panic::catch_unwind(|| Timestamp::parse(TimestampFormat::DateTime, "1985-04-12T2"));
    match outcome {
        Ok(Err(ParseTimestampError::InvalidFormat)) => {}
        Ok(other) => panic!("expected an InvalidFormat rejection, got {other:?}"),
        Err(payload) => panic!("the reader panicked on a truncated clock: {payload:?}"),
    }
}

#[test]
fn date_time_truncates_a_fraction_longer_than_nine_digits() {
    // The pre-migration implementation truncates such an input instead of rejecting
    // it, so the profile keeps it and the reader cuts the fraction before the library
    // sees it.
    let text = "1985-04-12T23:20:50.1234567890Z";
    assert_eq!(canonical(text, TimestampFormat::DateTime), (482_196_050, 123_456_789));
}

#[test]
fn date_time_reads_a_leap_second_as_the_last_nanosecond() {
    // A leap second has no place on the timeline; the pre-migration implementation
    // resolves it to the last nanosecond of the second it follows, and a fraction on
    // a leap second is dropped.
    let expected = (662_687_999, 999_999_999);
    for text in ["1990-12-31T23:59:60Z", "1990-12-31T23:59:60.5Z", "1990-12-31T23:59:60+00:00"] {
        assert_eq!(canonical(text, TimestampFormat::DateTime), expected, "{text}");
    }
}

#[test]
fn http_date_accepts_the_grammar() {
    let cases = [
        ("Sun, 06 Nov 1994 08:49:37 GMT", (784_111_777, 0)),
        // The weekday is read but not checked against the date.
        ("Mon, 06 Nov 1994 08:49:37 GMT", (784_111_777, 0)),
        ("Sat, 01 Jan 0000 00:00:00 GMT", (-62_167_219_200, 0)),
        ("Sat, 29 Feb 2020 08:49:37 GMT", (1_582_966_177, 0)),
    ];
    for (text, expected) in cases {
        assert_eq!(canonical(text, TimestampFormat::HttpDate), expected, "{text}");
    }
}

#[test]
fn http_date_rejects_the_spellings_the_library_would_accept() {
    let cases = [
        "sun, 06 Nov 1994 08:49:37 GMT",
        "Sun, 06 nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:37 gmt",
        "Sun, 6 Nov 1994 08:49:37 GMT",
        "Sun,  06 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 94 08:49:37 GMT",
        "Sun, 06 Nov 10000 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:37\tGMT",
        "Sun, 06 Nov 1994 08:49:37 GMT ",
        "Sun, 06 Nov 1994 08:49:37 GMT\n",
        " Sun, 06 Nov 1994 08:49:37 GMT",
    ];
    for text in cases {
        assert_eq!(error_name(text, TimestampFormat::HttpDate), "InvalidFormat", "{text:?}");
    }
}

#[test]
fn http_date_rejects_the_obsolete_forms() {
    let cases = [
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
        "Sun 06 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:37.000 GMT",
        "Sun, 06 Nov 1994 08:49:37",
        "Sun, 06 Nov 1994 08:49:37 GMT+00:00",
        "",
    ];
    for text in cases {
        assert_eq!(error_name(text, TimestampFormat::HttpDate), "InvalidFormat", "{text:?}");
    }
}

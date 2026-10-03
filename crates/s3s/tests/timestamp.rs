// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Smoke coverage for the timestamp surface of `s3s::dto`.
//!
//! The wire-format vectors, the boundary corpus and the frozen contract live in the
//! `s3s-time` crate; this file only pins that the re-exported path works and that the
//! three formats still round trip through it.

use s3s::dto::FormatTimestampError;
use s3s::dto::ParseTimestampError;
use s3s::dto::Timestamp;
use s3s::dto::TimestampFormat;

fn format_of(ts: &Timestamp, format: TimestampFormat) -> String {
    let mut buf = Vec::new();
    ts.format(format, &mut buf).expect("the instant is representable");
    String::from_utf8(buf).expect("the wire formats are ASCII")
}

#[test]
fn the_reexport_parses_and_formats_each_wire_format() {
    let cases = [
        (TimestampFormat::DateTime, "1985-04-12T23:20:50.520Z"),
        (TimestampFormat::HttpDate, "Tue, 29 Apr 2014 18:30:38 GMT"),
        (TimestampFormat::EpochSeconds, "1515531081.1234"),
    ];

    for (format, text) in cases {
        let ts = Timestamp::parse(format, text).expect("the vector parses");
        assert_eq!(format_of(&ts, format), text, "{format:?}");
    }
}

#[test]
fn the_value_type_keeps_the_expected_semantics() {
    // The fraction of an epoch-seconds value is always positive, so -1.5 is minus one
    // second plus five tenths: the floor second is -1 with a positive fraction.
    let ts = Timestamp::parse(TimestampFormat::EpochSeconds, "-1.5").expect("parse");
    assert_eq!(ts.unix_seconds(), -1);
    assert_eq!(ts.subsec_nanos(), 500_000_000);
    assert_eq!(ts.unix_nanos(), -500_000_000);
    assert_eq!(Timestamp::default(), Timestamp::UNIX_EPOCH);
    assert_eq!(Timestamp::from(std::time::SystemTime::UNIX_EPOCH), Timestamp::UNIX_EPOCH);
}

#[test]
fn the_four_reexported_names_stay_importable() {
    // The pre-migration module exposed the value type, the format selector and the two
    // error types from s3s::dto; the swap must keep all four paths working.
    let refused = Timestamp::parse(TimestampFormat::DateTime, "not-a-date");
    assert!(matches!(refused, Err(ParseTimestampError::InvalidFormat)));

    // One second before 0000-01-01T00:00:00Z is in the year -1, which the date-time
    // wire format cannot spell.
    let before_year_zero = Timestamp::from_unix_seconds(-62_167_219_201).expect("representable");
    let written = before_year_zero.format(TimestampFormat::DateTime, &mut Vec::new());
    assert!(matches!(written, Err(FormatTimestampError::OutOfRange)));
}

#[test]
fn serde_round_trips_through_the_date_time_format() {
    let ts = Timestamp::parse(TimestampFormat::DateTime, "1985-04-12T23:20:50.520Z").expect("parse");
    let json = serde_json::to_string(&ts).expect("serialize");
    assert_eq!(json, "\"1985-04-12T23:20:50.520Z\"");
    let back: Timestamp = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, ts);
}

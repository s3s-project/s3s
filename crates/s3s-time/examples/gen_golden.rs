// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Regenerates the frozen contract fixture from the pre-migration implementation.
//!
//! Run it with:
//!
//! `cargo run -p s3s-time --example gen_golden`
//!
//! The oracle is the timestamp type that the s3s crate ships today, the implementation
//! that this crate is being cultivated to replace. Every case of the fixed corpus is
//! replayed through the oracle, and the result is written to the fixture together with
//! the content-addressed provenance of the run: the blob of the oracle source, the blob
//! of the Smithy suite, and the version of the time library that the workspace resolved.
//! The commit the run happened at is deliberately not written into the fixture: a commit
//! id cannot be true inside the commit that contains it, and recording it would make the
//! artifact change on every rebase. The layer report and the pull request body carry it.
//!
//! The corpus is the six sections of the Smithy date-time format suite plus the
//! boundary inputs that the crate design records. Generation is deterministic: running
//! the example again rewrites the same file, and the file does not depend on the commit
//! it was generated at.

#[path = "../tests/harness/mod.rs"]
mod harness;

use std::fs;
use std::path::Path;
use std::process::Command;

use harness::api::{self, Oracle};
use harness::support::{self, Case, Direction, Expected, Format, Spec};

/// The sections of the Smithy suite, in the order of the file.
const SECTIONS: [&str; 6] = [
    "format_epoch_seconds",
    "format_http_date",
    "format_date_time",
    "parse_epoch_seconds",
    "parse_http_date",
    "parse_date_time",
];

/// The Smithy suite, embedded so that a half-written file cannot be picked up.
const SUITE: &str = include_str!("../../../data/date_time_format_test_suite.json");

/// The first instant that the internal representation holds, in UTC.
const RANGE_MIN: (i64, u32) = (unix_seconds(-9999, 1, 2, 1, 59, 59), 0);

/// The last instant that the internal representation holds, in UTC.
const RANGE_MAX: (i64, u32) = (unix_seconds(9999, 12, 30, 22, 0, 0), 999_999_999);

fn main() {
    let suite = load_suite();
    check_civil_arithmetic(&suite);

    let mut specs = Vec::new();
    suite_specs(&suite, &mut specs);
    boundary_corpus(&mut specs);
    check_format_range(&specs);

    let cases = generate(&specs);
    let text = render_fixture(&cases);
    let path = support::fixture_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
    }
    fs::write(&path, &text).unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    print_summary(&suite, &cases);
}

/// One case of the Smithy suite.
#[derive(Debug)]
struct SuiteCase {
    iso8601: String,
    canonical_seconds: i64,
    canonical_nanos: u32,
    error: bool,
    smithy_format_value: Option<String>,
}

/// Reads the Smithy suite, section by section.
fn load_suite() -> Vec<(&'static str, Vec<SuiteCase>)> {
    let value: serde_json::Value = serde_json::from_str(SUITE).expect("the Smithy suite is valid JSON");
    let object = value.as_object().expect("the Smithy suite is a JSON object");
    let mut sections = Vec::new();
    for name in SECTIONS {
        let entries = object
            .get(name)
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("the Smithy suite has the {name} section"));
        let mut cases = Vec::with_capacity(entries.len());
        for entry in entries {
            cases.push(SuiteCase {
                iso8601: entry["iso8601"].as_str().expect("iso8601 is a string").to_owned(),
                canonical_seconds: entry["canonical_seconds"]
                    .as_str()
                    .expect("canonical_seconds is a string")
                    .parse()
                    .expect("canonical_seconds fits in i64"),
                canonical_nanos: u32::try_from(entry["canonical_nanos"].as_u64().expect("canonical_nanos is an integer"))
                    .expect("canonical_nanos fits in u32"),
                error: entry["error"].as_bool().expect("error is a boolean"),
                smithy_format_value: entry
                    .get("smithy_format_value")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }
        sections.push((name, cases));
    }
    sections
}

/// Turns the six sections into corpus cases.
fn suite_specs(suite: &[(&'static str, Vec<SuiteCase>)], specs: &mut Vec<Spec>) {
    for (section, cases) in suite {
        let direction = direction_of(section);
        let format = format_of(section);
        for (index, case) in cases.iter().enumerate() {
            let id = format!("smithy/{section}/{index:03}");
            let note = suite_note(direction, case);
            let spec = match direction {
                Direction::Parse => Spec::parse(
                    format,
                    case.smithy_format_value
                        .as_deref()
                        .expect("a parse section carries the value"),
                    &id,
                    &note,
                ),
                Direction::Format => Spec::format(format, case.canonical_seconds, case.canonical_nanos, &id, &note),
            };
            specs.push(spec);
        }
    }
}

/// The note of a suite case, which says why the case is worth pinning.
fn suite_note(direction: Direction, case: &SuiteCase) -> String {
    let mut notes = Vec::new();
    if case.error {
        notes.push("error case".to_owned());
    }
    if direction == Direction::Format && !case.canonical_nanos.is_multiple_of(1_000_000) {
        notes.push("sub-millisecond".to_owned());
    }
    notes.join(", ")
}

/// The direction of a suite section.
fn direction_of(section: &str) -> Direction {
    if section.starts_with("parse_") {
        Direction::Parse
    } else {
        Direction::Format
    }
}

/// The wire format of a suite section.
fn format_of(section: &str) -> Format {
    let wire = section.split_once('_').expect("a section names a direction and a format").1;
    Format::from_name(&wire.replace('_', "-")).expect("a section names a known wire format")
}

/// Adds the boundary corpus that the crate design records.
fn boundary_corpus(specs: &mut Vec<Spec>) {
    boundary_date_time_parse(specs);
    boundary_date_time_format(specs);
    boundary_http_date(specs);
    boundary_epoch_seconds_parse(specs);
    boundary_epoch_seconds_format(specs);
}

/// The date-time inputs of the design.
fn boundary_date_time_parse(specs: &mut Vec<Spec>) {
    let parse_cases: [(&str, &str, &str); 46] = [
        ("design-4.1/canonical", "1985-04-12T23:20:50Z", "RFC 3339 without a fraction"),
        ("design-4.1/fraction-one-digit", "1985-04-12T23:20:50.5Z", "one fractional digit"),
        (
            "design-4.1/fraction-nine-digits",
            "1985-04-12T23:20:50.123456789Z",
            "nine fractional digits",
        ),
        (
            "design-4.1/fraction-ten-digits",
            "1985-04-12T23:20:50.1234567890Z",
            "ten fractional digits",
        ),
        ("design-4.1/fraction-empty", "1985-04-12T23:20:50.Z", "a decimal point without digits"),
        ("design-4.1/lowercase", "1985-04-12t23:20:50z", "lower-case separator and zone"),
        ("design-4.1/offset-utc-colon", "1985-04-12T23:20:50+00:00", "a zero offset written out"),
        ("design-4.1/offset-positive", "1985-04-12T23:20:50+02:00", "a positive offset"),
        (
            "design-4.1/offset-negative",
            "1985-04-12T23:20:50-05:30",
            "a negative offset with minutes",
        ),
        ("design-4.1/offset-negative-zero", "1985-04-12T23:20:50-00:00", "an unknown zero offset"),
        ("design-4.1/offset-without-colon", "1985-04-12T23:20:50+0200", "an offset without a colon"),
        ("design-4.1/offset-hour-only", "1985-04-12T23:20:50+02", "an offset without minutes"),
        ("design-4.1/offset-out-of-range", "1985-04-12T23:20:50+24:00", "an offset beyond a day"),
        (
            "design-4.1/offset-minute-out-of-range",
            "1985-04-12T23:20:50+00:60",
            "an offset minute beyond an hour",
        ),
        (
            "design-4.1/fraction-and-offset",
            "1985-04-12T23:20:50.5+01:00",
            "a fraction next to an offset",
        ),
        ("design-4.1/leap-second", "1990-12-31T23:59:60Z", "a leap second"),
        (
            "design-4.1/leap-second-fraction",
            "1990-12-31T23:59:60.5Z",
            "a leap second with a fraction",
        ),
        (
            "design-4.1/leap-second-offset",
            "1990-12-31T23:59:60+00:00",
            "a leap second with a zero offset",
        ),
        ("design-4.1/leap-day", "2020-02-29T12:00:00Z", "a leap day"),
        ("design-4.1/non-leap-day", "2021-02-29T12:00:00Z", "a leap day in a common year"),
        ("design-4.1/day-out-of-range", "1985-02-30T00:00:00Z", "day 30 in February"),
        ("design-4.1/month-out-of-range", "1985-13-01T00:00:00Z", "month 13"),
        ("design-4.1/hour-out-of-range", "1985-04-12T24:00:00Z", "hour 24"),
        ("design-4.1/minute-out-of-range", "1985-04-12T23:60:00Z", "minute 60"),
        ("design-4.1/second-out-of-range", "1985-04-12T23:20:61Z", "second 61"),
        ("design-4.1/date-only", "1985-04-12", "a date without a time"),
        ("design-4.1/space-separator", "1985-04-12 23:20:50Z", "a space instead of the T separator"),
        ("design-4.1/tab-separator", "1985-04-12\t23:20:50Z", "a tab instead of the T separator"),
        ("design-4.1/no-seconds", "1985-04-12T23:20Z", "a time without seconds"),
        ("design-4.1/no-zone", "1985-04-12T23:20:50", "a time without a zone"),
        ("design-4.1/comma-decimal", "1985-04-12T23:20:50,52Z", "a comma as the decimal separator"),
        ("design-4.1/annotation", "1985-04-12T23:20:50.52Z[UTC]", "an RFC 9557 annotation"),
        ("design-4.1/year-zero", "0000-01-01T00:00:00Z", "year zero"),
        ("design-4.1/year-negative", "-0001-01-01T00:00:00Z", "a negative four-digit year"),
        (
            "design-4.1/year-negative-extended",
            "-009999-01-01T00:00:00Z",
            "a negative six-digit year",
        ),
        ("design-4.1/year-five-digits", "+10000-01-01T00:00:00Z", "a five-digit year"),
        ("design-4.1/year-max", "9999-12-31T23:59:59Z", "the last second of year 9999"),
        ("design-4.1/year-min", "-9999-01-01T00:00:00Z", "the first second of year -9999"),
        (
            "design-4.1/range-min",
            "-9999-01-02T01:59:59Z",
            "the early end of the representable range",
        ),
        (
            "design-4.1/range-min-minus-one",
            "-9999-01-02T01:59:58Z",
            "one second before the representable range",
        ),
        (
            "design-4.1/range-max",
            "9999-12-30T22:00:00.999999999Z",
            "the late end of the representable range",
        ),
        (
            "design-4.1/range-max-plus-one",
            "9999-12-30T22:00:01Z",
            "one second past the representable range",
        ),
        ("design-4.1/empty", "", "the empty string"),
        ("design-4.1/trailing-junk", "1985-04-12T23:20:50Zx", "trailing text"),
        ("design-4.1/leading-space", " 1985-04-12T23:20:50Z", "a leading space"),
        ("design-4.1/trailing-newline", "1985-04-12T23:20:50Z\n", "a trailing line feed"),
    ];
    for (id, input, note) in parse_cases {
        specs.push(Spec::parse(Format::DateTime, input, id, note));
    }
}

/// The date-time instants of the design.
fn boundary_date_time_format(specs: &mut Vec<Spec>) {
    let format_cases: [(&str, i64, u32, &str); 12] = [
        ("design-4.1/format-epoch", 0, 0, "the Unix epoch"),
        ("design-4.1/format-millisecond", 0, 1_000_000, "one millisecond"),
        (
            "design-4.1/format-subsecond-truncated",
            0,
            999_999_999,
            "the fraction is truncated, not rounded",
        ),
        ("design-4.1/format-single-nano", 0, 1, "one nanosecond renders as zero milliseconds"),
        ("design-4.1/format-before-epoch", -1, 500_000_000, "half a second before the epoch"),
        ("design-4.1/format-before-epoch-nano", -1, 999_999_999, "one nanosecond before the epoch"),
        ("design-4.1/format-leap-day", unix_seconds(2020, 2, 29, 12, 0, 0), 0, "a leap day"),
        (
            "design-4.1/format-year-zero",
            unix_seconds(0, 1, 1, 0, 0, 0),
            0,
            "year zero carries four digits",
        ),
        (
            "design-4.1/format-year-negative",
            unix_seconds(-1, 1, 1, 0, 0, 0),
            0,
            "a negative year cannot be written",
        ),
        (
            "design-4.1/format-year-negative-fraction",
            unix_seconds(-1, 6, 15, 7, 0, 0),
            500_000_000,
            "a negative year with a fraction",
        ),
        (
            "design-4.1/format-range-min",
            unix_seconds(-9999, 1, 2, 1, 59, 59),
            0,
            "the early end of the range",
        ),
        (
            "design-4.1/format-range-max",
            unix_seconds(9999, 12, 30, 22, 0, 0),
            999_999_999,
            "the late end of the range",
        ),
    ];
    for (id, secs, nanos, note) in format_cases {
        specs.push(Spec::format(Format::DateTime, secs, nanos, id, note));
    }
}

/// The http-date boundaries of the design.
fn boundary_http_date(specs: &mut Vec<Spec>) {
    let parse_cases: [(&str, &str, &str); 26] = [
        ("design-4.2/imf-fixdate", "Sun, 06 Nov 1994 08:49:37 GMT", "the reference IMF-fixdate"),
        (
            "design-4.2/weekday-mismatch",
            "Mon, 06 Nov 1994 08:49:37 GMT",
            "the weekday does not match the date",
        ),
        ("design-4.2/weekday-lowercase", "sun, 06 Nov 1994 08:49:37 GMT", "a lower-case weekday"),
        ("design-4.2/weekday-full", "Sunday, 06 Nov 1994 08:49:37 GMT", "a full weekday name"),
        ("design-4.2/month-lowercase", "Sun, 06 nov 1994 08:49:37 GMT", "a lower-case month"),
        ("design-4.2/zone-lowercase", "Sun, 06 Nov 1994 08:49:37 gmt", "a lower-case zone"),
        ("design-4.2/zone-offset", "Sun, 06 Nov 1994 08:49:37 GMT+00:00", "a zone with an offset"),
        ("design-4.2/fraction", "Sun, 06 Nov 1994 08:49:37.000 GMT", "a fractional second"),
        ("design-4.2/missing-zone", "Sun, 06 Nov 1994 08:49:37", "no zone"),
        ("design-4.2/rfc850", "Sunday, 06-Nov-94 08:49:37 GMT", "the obsolete RFC 850 form"),
        ("design-4.2/asctime", "Sun Nov  6 08:49:37 1994", "the obsolete asctime form"),
        (
            "design-4.2/single-digit-day",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "a day without the leading zero",
        ),
        ("design-4.2/double-space", "Sun,  06 Nov 1994 08:49:37 GMT", "two spaces after the comma"),
        ("design-4.2/no-comma", "Sun 06 Nov 1994 08:49:37 GMT", "no comma after the weekday"),
        ("design-4.2/two-digit-year", "Sun, 06 Nov 94 08:49:37 GMT", "a two-digit year"),
        ("design-4.2/five-digit-year", "Sun, 06 Nov 10000 08:49:37 GMT", "a five-digit year"),
        ("design-4.2/year-zero", "Sat, 01 Jan 0000 00:00:00 GMT", "year zero"),
        ("design-4.2/leap-day", "Sat, 29 Feb 2020 08:49:37 GMT", "a leap day"),
        ("design-4.2/non-leap-day", "Mon, 29 Feb 2021 08:49:37 GMT", "a leap day in a common year"),
        ("design-4.2/day-out-of-range", "Mon, 31 Feb 2020 08:49:37 GMT", "a day out of range"),
        ("design-4.2/hour-out-of-range", "Sun, 06 Nov 1994 24:49:37 GMT", "hour 24"),
        ("design-4.2/tab-separator", "Sun, 06 Nov 1994 08:49:37\tGMT", "a tab instead of the space"),
        ("design-4.2/trailing-space", "Sun, 06 Nov 1994 08:49:37 GMT ", "a trailing space"),
        ("design-4.2/trailing-newline", "Sun, 06 Nov 1994 08:49:37 GMT\n", "a trailing line feed"),
        ("design-4.2/leading-space", " Sun, 06 Nov 1994 08:49:37 GMT", "a leading space"),
        ("design-4.2/empty", "", "the empty string"),
    ];
    for (id, input, note) in parse_cases {
        specs.push(Spec::parse(Format::HttpDate, input, id, note));
    }

    let format_cases: [(&str, i64, u32, &str); 6] = [
        (
            "design-4.2/format-reference",
            unix_seconds(1994, 11, 6, 8, 49, 37),
            0,
            "the reference IMF-fixdate",
        ),
        (
            "design-4.2/format-subsecond-dropped",
            unix_seconds(1994, 11, 6, 8, 49, 37),
            999_999_999,
            "the subsecond is dropped",
        ),
        (
            "design-4.2/format-year-zero",
            unix_seconds(0, 1, 1, 0, 0, 0),
            0,
            "year zero carries four digits",
        ),
        (
            "design-4.2/format-year-negative",
            unix_seconds(-1, 1, 1, 0, 0, 0),
            0,
            "a negative year cannot be written",
        ),
        (
            "design-4.2/format-range-max",
            unix_seconds(9999, 12, 30, 22, 0, 0),
            999_999_999,
            "the late end of the range",
        ),
        (
            "design-4.2/format-documentation",
            unix_seconds(2024, 6, 15, 7, 0, 0),
            123_000_000,
            "the example of the crate documentation",
        ),
    ];
    for (id, secs, nanos, note) in format_cases {
        specs.push(Spec::format(Format::HttpDate, secs, nanos, id, note));
    }
}

/// The epoch-seconds inputs of the design.
fn boundary_epoch_seconds_parse(specs: &mut Vec<Spec>) {
    let parse_cases: [(&str, &str, &str); 37] = [
        ("design-4.3/zero", "0", "the epoch"),
        ("design-4.3/negative-zero", "-0", "a negative zero"),
        ("design-4.3/plus-prefix", "+1", "an explicit plus sign"),
        ("design-4.3/leading-zeros", "0001.5", "leading zeros"),
        ("design-4.3/fraction-one-digit", "100.5", "one fractional digit"),
        ("design-4.3/fraction-nine-digits", "100.123456789", "nine fractional digits"),
        ("design-4.3/fraction-zero", "1.0", "a zero fraction"),
        ("design-4.3/fraction-trailing-zeros", "100.500", "trailing zeros in the fraction"),
        ("design-4.3/fraction-leading-zeros", "100.000000001", "leading zeros in the fraction"),
        ("design-4.3/fraction-ten-digits", "100.1234567890", "ten fractional digits"),
        (
            "design-4.3/fraction-ten-zeros",
            "100.000000000",
            "ten fractional digits that are all zero",
        ),
        ("design-4.3/negative-fraction", "-1.5", "the fraction of a negative value is positive"),
        (
            "design-4.3/negative-fraction-nine",
            "-1.999999999",
            "nine fractional digits below minus one",
        ),
        ("design-4.3/negative-small", "-0.999999999", "a value between minus one and zero"),
        ("design-4.3/negative-whole", "-1", "a negative whole second"),
        ("design-4.3/leading-space", " 1", "a leading space"),
        ("design-4.3/trailing-space", "1 ", "a trailing space"),
        ("design-4.3/tab-prefix", "\t1", "a leading tab"),
        ("design-4.3/empty", "", "the empty string"),
        ("design-4.3/dot-only", ".", "a decimal point without digits"),
        ("design-4.3/leading-dot", ".5", "a fraction without an integral part"),
        ("design-4.3/trailing-dot", "5.", "a decimal point without a fraction"),
        ("design-4.3/two-dots", "1.5.5", "two decimal points"),
        ("design-4.3/double-dot", "1..5", "an empty fraction between two points"),
        ("design-4.3/exponent", "1e3", "an exponent"),
        ("design-4.3/comma", "1,5", "a comma as the decimal separator"),
        ("design-4.3/letters", "abc", "letters"),
        ("design-4.3/i64-max", "9223372036854775807", "the largest signed 64-bit second"),
        (
            "design-4.3/i64-max-plus-one",
            "9223372036854775808",
            "one past the largest signed 64-bit second",
        ),
        ("design-4.3/i64-min", "-9223372036854775808", "the smallest signed 64-bit second"),
        (
            "design-4.3/i64-min-fraction",
            "-9223372036854775808.000000001",
            "a fraction on the smallest second",
        ),
        ("design-4.3/year-10000", "253402300800", "the first second of year 10000"),
        ("design-4.3/year-9999-end", "253402300799", "the last second of year 9999"),
        (
            "design-4.3/year-9999-end-fraction",
            "253402300799.999999999",
            "the last nanosecond of year 9999",
        ),
        ("design-4.3/year-minus-9999-start", "-377705116800", "the first second of year -9999"),
        (
            "design-4.3/year-minus-9999-fraction",
            "-377705116800.000000001",
            "one nanosecond after the first second",
        ),
        ("design-4.3/year-minus-9999-before", "-377705116801", "one second before year -9999"),
    ];
    for (id, input, note) in parse_cases {
        specs.push(Spec::parse(Format::EpochSeconds, input, id, note));
    }
}

/// The epoch-seconds instants of the design.
fn boundary_epoch_seconds_format(specs: &mut Vec<Spec>) {
    let range_min = unix_seconds(-9999, 1, 2, 1, 59, 59);
    let range_max = unix_seconds(9999, 12, 30, 22, 0, 0);
    let computed: [(&str, String, &str); 4] = [
        (
            "design-4.3/range-min",
            epoch_literal(range_min, 0),
            "the early end of the representable range",
        ),
        (
            "design-4.3/range-min-minus-one",
            epoch_literal(range_min - 1, 0),
            "one second before the representable range",
        ),
        (
            "design-4.3/range-max",
            epoch_literal(range_max, 999_999_999),
            "the late end of the representable range",
        ),
        (
            "design-4.3/range-max-plus-one",
            epoch_literal(range_max + 1, 0),
            "one second past the representable range",
        ),
    ];
    for (id, input, note) in &computed {
        specs.push(Spec::parse(Format::EpochSeconds, input, id, note));
    }

    let format_cases: [(&str, i64, u32, &str); 12] = [
        ("design-4.3/format-epoch", 0, 0, "the epoch has no fraction"),
        ("design-4.3/format-nanosecond", 0, 1, "the shortest exact decimal of one nanosecond"),
        ("design-4.3/format-tenth", 0, 100_000_000, "one tenth"),
        ("design-4.3/format-hundred-thousandth", 0, 10_000, "ten microseconds"),
        ("design-4.3/format-nine-digits", 100, 123_456_789, "nine fractional digits"),
        ("design-4.3/format-trailing-zeros", 100, 120_000_000, "trailing zeros are trimmed"),
        ("design-4.3/format-before-epoch-nano", -1, 999_999_999, "one nanosecond before the epoch"),
        ("design-4.3/format-before-epoch-half", -1, 500_000_000, "half a second before the epoch"),
        (
            "design-4.3/format-before-epoch-two",
            -2,
            500_000_000,
            "one and a half seconds before the epoch",
        ),
        ("design-4.3/format-negative-whole", -1, 0, "a negative whole second"),
        ("design-4.3/format-range-min", range_min, 0, "the early end of the range"),
        ("design-4.3/format-range-max", range_max, 999_999_999, "the late end of the range"),
    ];
    for (id, secs, nanos, note) in format_cases {
        specs.push(Spec::format(Format::EpochSeconds, secs, nanos, id, note));
    }
}

/// Rejects a format case whose instant the two implementations cannot both hold.
fn check_format_range(specs: &[Spec]) {
    let model = support::platform_model();
    let mut carried = 0_usize;
    let mut uncarried = Vec::new();
    for spec in specs.iter().filter(|spec| spec.direction == Direction::Format) {
        let instant = support::decode_instant(&spec.input).unwrap_or_else(|error| panic!("{}: {error}", spec.intent));
        assert!(
            instant >= RANGE_MIN && instant <= RANGE_MAX,
            "{}: the instant {instant:?} is outside the shared range",
            spec.intent
        );
        if model.carries(instant.0, instant.1) {
            carried += 1;
        } else {
            uncarried.push(spec.intent.clone());
        }
    }
    // The fixture is frozen on a clock that carries the corpus: a format case the clock
    // cannot keep would record a different instant. An injected model makes this fire on
    // any machine, so the alignment of the corpus can be rehearsed without Windows.
    assert!(
        uncarried.is_empty(),
        "the platform model (tick={}, floor={}) cannot carry {} format cases: {}",
        model.tick_nanos,
        model.floor_seconds,
        uncarried.len(),
        uncarried.join(", ")
    );
    println!(
        "format cases: {carried} carried by the platform model (tick={}, floor={})",
        model.tick_nanos, model.floor_seconds
    );
}

/// Runs every corner of the corpus through the oracle.
fn generate(specs: &[Spec]) -> Vec<Case> {
    specs
        .iter()
        .map(|spec| {
            let outcome = api::try_run::<Oracle>(spec.direction, spec.format, &spec.input)
                .expect("the fixture is generated where the platform clock carries the corpus");
            Case::new(spec, outcome.expected()).unwrap_or_else(|error| panic!("{}: {error}", spec.intent))
        })
        .collect()
}

/// Renders the whole fixture, header included.
fn render_fixture(cases: &[Case]) -> String {
    let parse_count = cases.iter().filter(|case| case.direction == Direction::Parse).count();
    let mut lines = vec!["# s3s-time frozen contract fixture".to_owned(), "#".to_owned()];
    lines.extend(provenance_lines());
    lines.push("#".to_owned());
    lines.extend(
        [
            "# columns (tab separated): <direction>/<format> | <input> | <expected> | <intent>",
            "# direction: parse (the input is text to parse) or format (the input is an instant)",
            "# format: date-time | http-date | epoch-seconds",
            "# input: parse -> the literal text; format -> <seconds>:<nanoseconds>, with floor",
            "#        seconds and a non-negative nanosecond adjustment",
            "# expected: parse -> instant:<seconds>:<nanoseconds> or error:<name>",
            "#           format -> text:<output> or error:<name>",
            "# error names: InvalidFormat | OutOfRange | FractionTooLong | Overflow | Io",
            "# intent: <case id> [note]; the case id is the first whitespace-delimited token",
            "# text fields escape the backslash, the tab, the line feed and the carriage return",
            r"# as \\ \t \n \r; an empty field is the empty string",
            "# format cases use instants that both the pre-migration implementation and the",
            "# jiff-backed implementation can represent; parse inputs are not restricted",
        ]
        .map(ToOwned::to_owned),
    );
    lines.push(format!(
        "# cases: {} (parse {parse_count}, format {})",
        cases.len(),
        cases.len() - parse_count
    ));
    lines.push("#".to_owned());
    for case in cases {
        lines.push(support::render_line(case));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Collects the provenance of the generation run.
fn provenance_lines() -> Vec<String> {
    let root = support::workspace_root();
    let oracle_blob = git(&root, &["rev-parse", "HEAD:crates/s3s/src/dto/timestamp.rs"]);
    let suite_blob = git(&root, &["rev-parse", "HEAD:data/date_time_format_test_suite.json"]);
    let version = support::locked_jiff_version().unwrap_or_else(|| "unknown".to_owned());
    vec![
        "# oracle: crates/s3s/src/dto/timestamp.rs".to_owned(),
        format!("# oracle blob: {oracle_blob}"),
        "# suite: data/date_time_format_test_suite.json".to_owned(),
        format!("# suite blob: {suite_blob}"),
        format!("# jiff: {version}"),
        "# refresh with: cargo run -p s3s-time --example gen_golden".to_owned(),
    ]
}

/// Runs git in the workspace and returns the trimmed standard output.
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("cannot run git: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

/// Validates the civil arithmetic against the suite before the corpus uses it.
fn check_civil_arithmetic(suite: &[(&'static str, Vec<SuiteCase>)]) {
    let mut checked = 0_usize;
    let mut skipped = 0_usize;
    let mut deviations = Vec::new();
    for (section, cases) in suite {
        for case in cases {
            match iso8601_seconds(&case.iso8601) {
                Some(seconds) if seconds == case.canonical_seconds => checked += 1,
                Some(seconds) => deviations.push(format!(
                    "{section}: {} gives {seconds} but canonical_seconds is {}",
                    case.iso8601, case.canonical_seconds
                )),
                None => skipped += 1,
            }
        }
    }
    if !deviations.is_empty() {
        for deviation in deviations.iter().take(10) {
            eprintln!("  {deviation}");
        }
        panic!("the civil arithmetic disagrees with the Smithy suite in {} cases", deviations.len());
    }
    println!("civil arithmetic: {checked} suite dates agree with canonical_seconds ({skipped} not parsed)");
}

/// The days between a civil date and 1970-01-01, by the algorithm of Howard Hinnant.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    // The division truncates towards zero, which is what the algorithm assumes, and the
    // parentheses keep the sign adjustment inside the numerator.
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month_of_year = (month + 9) % 12;
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The Unix second of a civil date and time in UTC.
const fn unix_seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
}

/// Reads the integral seconds of the ISO 8601 date-time of a suite case.
fn iso8601_seconds(iso: &str) -> Option<i64> {
    let (date, time) = iso.split_once('T')?;
    let (negative, date) = match date.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, date),
    };
    let mut date_parts = date.split('-');
    let mut year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }
    if negative {
        year = -year;
    }
    let time = time.strip_suffix('Z')?;
    let time = time.split_once('.').map_or(time, |(whole, _)| whole);
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    Some(unix_seconds(year, month, day, hour, minute, second))
}

/// The epoch-seconds literal of an instant, for a parse case.
fn epoch_literal(secs: i64, nanos: u32) -> String {
    if nanos == 0 {
        return format!("{secs}");
    }
    let digits = format!("{nanos:09}");
    format!("{secs}.{}", digits.trim_end_matches('0'))
}

/// Prints the generation statistics that the migration report quotes.
fn print_summary(suite: &[(&'static str, Vec<SuiteCase>)], cases: &[Case]) {
    let parse_count = cases.iter().filter(|case| case.direction == Direction::Parse).count();
    let error_count = cases
        .iter()
        .filter(|case| matches!(case.expected, Expected::Error(_)))
        .count();
    println!("fixture: {}", support::fixture_path().display());
    println!("cases: {} (parse {parse_count}, format {})", cases.len(), cases.len() - parse_count);
    println!("errors: {error_count}");
    println!();
    println!("external vectors:");
    for (section, section_cases) in suite {
        let prefix = format!("smithy/{section}/");
        let pinned = cases.iter().filter(|case| case.id().starts_with(prefix.as_str())).count();
        println!("  {section}: {pinned} cases of {}", section_cases.len());
    }
    let boundary = cases.iter().filter(|case| case.id().starts_with("design-")).count();
    println!("  boundary inputs: {boundary}");
    println!();
    print_suite_cross_check(suite, cases);
}

/// Compares the oracle against the values that the Smithy suite states.
fn print_suite_cross_check(suite: &[(&'static str, Vec<SuiteCase>)], cases: &[Case]) {
    let mut format_checked = 0_usize;
    let mut format_deviations = Vec::new();
    let mut parse_checked = 0_usize;
    let mut parse_deviations = Vec::new();
    for (section, section_cases) in suite {
        let direction = direction_of(section);
        for (index, case) in section_cases.iter().enumerate() {
            let id = format!("smithy/{section}/{index:03}");
            let Some(generated) = cases.iter().find(|generated| generated.id() == id) else {
                continue;
            };
            match direction {
                Direction::Format => {
                    let Some(value) = case.smithy_format_value.as_deref() else {
                        continue;
                    };
                    format_checked += 1;
                    match &generated.expected {
                        Expected::Text(text) if text == value => {}
                        Expected::Text(text) => format_deviations.push(format!("{id}: oracle {text:?} but suite {value:?}")),
                        Expected::Error(name) => {
                            format_deviations.push(format!("{id}: oracle failed with {name} but suite expects {value:?}"));
                        }
                        Expected::Instant { .. } => {}
                    }
                }
                Direction::Parse => {
                    if case.error {
                        continue;
                    }
                    parse_checked += 1;
                    match generated.expected {
                        Expected::Instant { secs, nanos } if (secs, nanos) == (case.canonical_seconds, case.canonical_nanos) => {}
                        Expected::Instant { secs, nanos } => parse_deviations.push(format!(
                            "{id}: oracle {secs}:{nanos} but suite {}:{}",
                            case.canonical_seconds, case.canonical_nanos
                        )),
                        Expected::Error(ref name) => {
                            parse_deviations.push(format!("{id}: oracle failed with {name} but the suite expects a value"));
                        }
                        Expected::Text(_) => {}
                    }
                }
            }
        }
    }
    println!("suite cross-check (the oracle against the values the suite states):");
    println!("  format values: {format_checked} compared, {} deviations", format_deviations.len());
    for deviation in format_deviations.iter().take(10) {
        println!("    {deviation}");
    }
    println!("  parse instants: {parse_checked} compared, {} deviations", parse_deviations.len());
    for deviation in parse_deviations.iter().take(10) {
        println!("    {deviation}");
    }
    if let Some((_, date_time_cases)) = suite.iter().find(|(name, _)| *name == "format_date_time") {
        let unaligned = date_time_cases
            .iter()
            .filter(|case| case.canonical_nanos % 1_000_000 != 0)
            .count();
        println!(
            "  format_date_time: {unaligned} of {} cases are not millisecond aligned",
            date_time_cases.len()
        );
    }
}

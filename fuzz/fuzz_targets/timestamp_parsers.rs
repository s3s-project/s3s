// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for the three timestamp wire formats of the S3 data model.
//!
//! Input protocol: `data[0] % 3` selects the format, `data[1..]` is the input
//! text:
//! - 0: `date-time` ([`TimestampFormat::DateTime`])
//! - 1: `http-date` ([`TimestampFormat::HttpDate`])
//! - 2: `epoch-seconds` ([`TimestampFormat::EpochSeconds`])
//!
//! The payload is decoded lossily, so a non-UTF-8 mutation still reaches the
//! parser instead of being discarded; the parsers only ever see a `&str`.
//!
//! Oracles:
//! - A: parsing must not panic - malformed input is a returned error.
//! - B: formatting a value that parsed must not panic; it either succeeds or
//!   reports [`FormatTimestampError::OutOfRange`] for an instant whose
//!   year has no four-digit form.
//! - C (lossless inputs): when the format represents the value exactly,
//!   `parse(format(x)) == x` holds - always for `epoch-seconds` (shortest
//!   exact decimal), for `date-time` when the value is millisecond-aligned (the
//!   wire form is fixed at three fractional digits), and for `http-date` when
//!   the value is a whole second (sub-second digits are dropped).
//! - D: formatting is idempotent - writing the same value twice yields the
//!   same bytes.

use libfuzzer_sys::fuzz_target;
use s3s_time::FormatTimestampError;
use s3s_time::Timestamp;
use s3s_time::TimestampFormat;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;

/// Names of the formats selected by the selector byte, for failure messages.
const FORMAT_NAMES: [&str; 3] = ["date-time", "http-date", "epoch-seconds"];

/// Maps the selector byte to a format. Rebuilt on every use so the target does
/// not depend on [`TimestampFormat`] being `Copy`.
fn format_of(selector: u8) -> TimestampFormat {
    match selector % 3 {
        0 => TimestampFormat::DateTime,
        1 => TimestampFormat::HttpDate,
        _ => TimestampFormat::EpochSeconds,
    }
}

/// Whether the selected format stores `nanos` without losing information.
fn is_lossless(selector: u8, nanos: i128) -> bool {
    match selector % 3 {
        0 => nanos % NANOS_PER_MILLI == 0,
        1 => nanos % NANOS_PER_SEC == 0,
        _ => true,
    }
}

fn check(selector: u8, input: &str) {
    let name = FORMAT_NAMES[usize::from(selector) % FORMAT_NAMES.len()];
    let format = format_of(selector);

    // Oracle A: a rejected input is an error value, never a panic.
    let Ok(value) = Timestamp::parse(format, input) else {
        return;
    };

    // Oracle B.
    let mut buf = Vec::new();
    match value.format(format, &mut buf) {
        Ok(()) => {}
        // The parser accepts the full instant range while the wire forms carry
        // four-digit years, so a parsed value can be unwritable.
        Err(FormatTimestampError::OutOfRange) => return,
        Err(err) => panic!("{name}: formatting a parsed value failed: {err}"),
    }

    let text = std::str::from_utf8(&buf).expect("formatted output must be ASCII");

    // Oracle D.
    let mut again = Vec::new();
    value
        .format(format, &mut again)
        .unwrap_or_else(|err| panic!("{name}: the second format call failed: {err}"));
    assert_eq!(again, buf, "{name}: formatting is not idempotent for {input:?}");

    if !is_lossless(selector, value.unix_nanos()) {
        return;
    }

    // Oracle C.
    let reparsed = Timestamp::parse(format, text).unwrap_or_else(|err| panic!("{name}: re-parsing {text:?} failed: {err}"));
    assert_eq!(reparsed, value, "{name}: parse(format(x)) != x for {input:?}");
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else { return };
    let input = String::from_utf8_lossy(payload);
    check(selector, &input);
});

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The readers of the three wire formats.

use jiff::fmt::rfc2822;
use jiff::fmt::temporal;

use crate::ParseTimestampError;
use crate::Timestamp;

/// The nanosecond scale of an epoch-seconds fraction with `index` digits.
const FRACTION_SCALE: [i128; 10] = [
    1_000_000_000,
    100_000_000,
    10_000_000,
    1_000_000,
    100_000,
    10_000,
    1_000,
    100,
    10,
    1,
];

/// Parses an RFC 3339 date-time and normalizes it to UTC.
pub(crate) fn date_time(s: &str) -> Result<Timestamp, ParseTimestampError> {
    temporal::DateTimeParser::new()
        .parse_timestamp(s)
        .map(Timestamp::from_inner)
        .map_err(|err| classify(&err))
}

/// Parses an IMF-fixdate HTTP date.
///
/// The weekday carried by the input is not checked against the date: the
/// instant does not depend on it, so an inconsistent weekday is accepted.
pub(crate) fn http_date(s: &str) -> Result<Timestamp, ParseTimestampError> {
    rfc2822::DateTimeParser::new()
        .relaxed_weekday(true)
        .parse_timestamp(s)
        .map(Timestamp::from_inner)
        .map_err(|err| classify(&err))
}

/// Parses epoch seconds with an optional fraction of up to nine digits.
///
/// The grammar is an optional minus sign, one or more decimal digits, and an
/// optional dot followed by one to nine digits. The fraction is always
/// positive: `-1.5` denotes minus one second plus five tenths, that is
/// `-0.5` seconds.
pub(crate) fn epoch_seconds(s: &str) -> Result<Timestamp, ParseTimestampError> {
    let (seconds, fraction) = match s.split_once('.') {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (s, None),
    };

    let seconds = seconds_of(seconds)?;
    let fraction = match fraction {
        None => 0,
        Some(fraction) => fraction_nanos(fraction)?,
    };

    let nanos = i128::from(seconds)
        .checked_mul(1_000_000_000)
        .and_then(|nanos| nanos.checked_add(fraction))
        .ok_or(ParseTimestampError::Overflow)?;

    Timestamp::from_unix_timestamp_nanos(nanos).map_err(|_| ParseTimestampError::OutOfRange)
}

/// Parses the seconds field of an epoch-seconds value.
fn seconds_of(s: &str) -> Result<i64, ParseTimestampError> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseTimestampError::InvalidFormat);
    }

    s.parse().map_err(|_| ParseTimestampError::InvalidFormat)
}

/// Converts a fraction of a second into nanoseconds.
fn fraction_nanos(fraction: &str) -> Result<i128, ParseTimestampError> {
    if fraction.is_empty() {
        return Err(ParseTimestampError::InvalidFormat);
    }
    if fraction.len() > 9 {
        return Err(ParseTimestampError::FractionTooLong);
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseTimestampError::InvalidFormat);
    }

    let digits: i128 = fraction.parse().map_err(|_| ParseTimestampError::InvalidFormat)?;
    Ok(digits * FRACTION_SCALE[fraction.len()])
}

/// Maps a date library error to a wire-format error.
///
/// The library reports range failures separately from syntax failures; both
/// are wire-format errors here, but they keep distinct variants.
fn classify(err: &jiff::Error) -> ParseTimestampError {
    if err.is_range() {
        ParseTimestampError::OutOfRange
    } else {
        ParseTimestampError::InvalidFormat
    }
}

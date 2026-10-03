// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The readers of the three wire formats.

use std::borrow::Cow;

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
pub fn date_time(s: &str) -> Result<Timestamp, ParseTimestampError> {
    let Some(text) = normalize_date_time(s) else {
        return Err(ParseTimestampError::InvalidFormat);
    };

    temporal::DateTimeParser::new()
        .parse_timestamp(&*text)
        .map(Timestamp::from_inner)
        .map_err(|err| classify(&err))
}

/// Checks the input against the profile of the date-time format and normalizes the
/// spellings that the temporal parser does not accept.
///
/// The profile is the one the pre-migration implementation accepts: a four-digit
/// year, a two-digit month and day, a `T`, lower-case `t`, space or tab separator, a
/// two-digit hour, minute and second, an optional fraction, and a `Z`, lower-case
/// `z` or `±HH:MM` zone with both fields in range. The temporal parser accepts more
/// than this profile — an offset without a colon or without minutes, a missing
/// seconds field, a comma as the decimal separator, an out-of-range offset and an
/// annotation after the zone — so the input is checked before the parser sees it.
///
/// Two spellings are normalized instead of rejected, because the pre-migration
/// implementation accepts them: a fraction longer than nine digits is truncated, and
/// a leap second becomes the last nanosecond of the second it follows (a fraction on
/// a leap second is dropped). A separator other than `T` is written as `T`.
///
/// Returns none when the input does not match the profile.
fn normalize_date_time(s: &str) -> Option<Cow<'_, str>> {
    // The wire format is ASCII, and the checks below index bytes.
    if !s.is_ascii() {
        return None;
    }

    let split = s.find(['T', 't', ' ', '\t'])?;
    let (date, rest) = (&s[..split], &s[split + 1..]);
    if !is_date(date) {
        return None;
    }

    let (clock, zone) = split_zone(rest)?;
    let (hour_minute, seconds, fraction) = split_clock(clock)?;

    let leap = seconds == "60";
    let truncated = fraction.filter(|fraction| fraction.len() > 9);
    let separator = &s[split..=split];

    if separator == "T" && !leap && truncated.is_none() {
        return Some(Cow::Borrowed(s));
    }

    let mut text = String::with_capacity(s.len() + 10);
    text.push_str(date);
    text.push('T');
    text.push_str(hour_minute);
    text.push(':');
    if leap {
        text.push_str("59.999999999");
    } else {
        text.push_str(seconds);
        if let Some(fraction) = truncated.or(fraction) {
            text.push('.');
            text.push_str(&fraction[..fraction.len().min(9)]);
        }
    }
    text.push_str(zone);
    Some(Cow::Owned(text))
}

/// Whether the text is a four-digit year, a two-digit month and a two-digit day.
fn is_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

/// Splits the clock from the zone suffix, or returns none when the zone is malformed.
fn split_zone(rest: &str) -> Option<(&str, &str)> {
    let zone = match rest.as_bytes().last()? {
        b'Z' | b'z' => &rest[rest.len() - 1..],
        _ => {
            if rest.len() < 6 {
                return None;
            }
            let zone = &rest[rest.len() - 6..];
            if !is_offset(zone) {
                return None;
            }
            zone
        }
    };
    let clock = &rest[..rest.len() - zone.len()];
    Some((clock, zone))
}

/// Whether the text is an offset whose hour and minute are in range.
fn is_offset(zone: &str) -> bool {
    let bytes = zone.as_bytes();
    if bytes.len() != 6 || !matches!(bytes[0], b'+' | b'-') || bytes[3] != b':' {
        return false;
    }
    if !bytes[1..3].iter().all(u8::is_ascii_digit) || !bytes[4..6].iter().all(u8::is_ascii_digit) {
        return false;
    }

    let hour = (bytes[1] - b'0') * 10 + (bytes[2] - b'0');
    let minute = (bytes[4] - b'0') * 10 + (bytes[5] - b'0');
    hour <= 23 && minute <= 59
}

/// Splits a clock into the hour and minute, the seconds field and the fraction.
fn split_clock(clock: &str) -> Option<(&str, &str, Option<&str>)> {
    let (whole, fraction) = match clock.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (clock, None),
    };
    if !is_clock(whole) {
        return None;
    }
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }

    let (hour_minute, seconds) = whole.rsplit_once(':')?;
    Some((hour_minute, seconds, fraction))
}

/// Whether the text is a two-digit hour, a two-digit minute and a two-digit second.
fn is_clock(whole: &str) -> bool {
    let bytes = whole.as_bytes();
    bytes.len() == 8
        && bytes[2] == b':'
        && bytes[5] == b':'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 2 | 5) || byte.is_ascii_digit())
}

/// Parses an IMF-fixdate HTTP date.
///
/// The weekday carried by the input is not checked against the date: the
/// instant does not depend on it, so an inconsistent weekday is accepted.
pub fn http_date(s: &str) -> Result<Timestamp, ParseTimestampError> {
    if !is_imf_fixdate(s) {
        return Err(ParseTimestampError::InvalidFormat);
    }

    rfc2822::DateTimeParser::new()
        .relaxed_weekday(true)
        .parse_timestamp(s)
        .map(Timestamp::from_inner)
        .map_err(|err| classify(&err))
}

/// Checks the input against the IMF-fixdate grammar of the format description.
///
/// The parser of the date library is more permissive than the grammar: it also
/// accepts lower-case names, a day without a leading zero, a two-digit year, extra
/// whitespace around the value, and a tab as the separator. The wire format is
/// `day-name "," SP 2DIGIT SP month SP 4DIGIT SP 2DIGIT ":" 2DIGIT ":" 2DIGIT SP
/// "GMT"`, so the input is checked before it reaches the library. The instant does
/// not depend on the weekday, so the name is only checked for spelling.
fn is_imf_fixdate(s: &str) -> bool {
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let Some(rest) = s.strip_suffix(" GMT") else {
        return false;
    };
    let Some((weekday, rest)) = rest.split_once(", ") else {
        return false;
    };
    if !WEEKDAYS.contains(&weekday) {
        return false;
    }
    let Some((day, rest)) = rest.split_once(' ') else {
        return false;
    };
    let Some((month, rest)) = rest.split_once(' ') else {
        return false;
    };
    if !MONTHS.contains(&month) {
        return false;
    }
    let Some((year, time)) = rest.split_once(' ') else {
        return false;
    };
    if year.len() != 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let mut parts = time.split(':');
    let (Some(hour), Some(minute), Some(second), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [day, hour, minute, second]
        .iter()
        .all(|field| field.len() == 2 && field.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Parses epoch seconds with an optional fraction of up to nine digits.
///
/// The grammar is an optional minus sign, one or more decimal digits, and an
/// optional dot followed by one to nine digits. The fraction is always
/// positive: `-1.5` denotes minus one second plus five tenths, that is
/// `-0.5` seconds.
pub fn epoch_seconds(s: &str) -> Result<Timestamp, ParseTimestampError> {
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

    Timestamp::from_unix_nanos(nanos).map_err(|_| ParseTimestampError::OutOfRange)
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

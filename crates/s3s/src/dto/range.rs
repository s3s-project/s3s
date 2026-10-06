// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
//! HTTP Range header

use crate::S3Error;
use crate::S3ErrorCode;
use crate::http;

use std::ops;

use stdx::str::StrExt;

/// HTTP Range header
///
/// Amazon S3 doesn't support retrieving multiple ranges of data per GET request.
///
/// See <https://www.rfc-editor.org/rfc/rfc9110.html#section-14.1.2>
#[allow(clippy::exhaustive_enums)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Int range in bytes. This range is **inclusive**.
    ///
    /// See <https://www.rfc-editor.org/rfc/rfc9110.html#rule.int-range>
    Int {
        /// first position
        first: u64,
        /// last position
        last: Option<u64>,
    },
    /// Suffix range in bytes.
    ///
    /// See <https://www.rfc-editor.org/rfc/rfc9110.html#rule.suffix-range>
    Suffix {
        /// suffix length
        length: u64,
    },
}

/// [`Range`]
#[derive(Debug, thiserror::Error)]
#[error("ParseRangeError")]
pub struct ParseRangeError {
    /// private place holder
    _priv: (),
}

/// [`Range`]
#[derive(Debug, thiserror::Error)]
#[error("RangeNotSatisfiable")]
pub struct RangeNotSatisfiable {
    /// private place holder
    _priv: (),
}

impl Range {
    /// Parses `Range` from header
    /// # Errors
    /// Returns an error if the header is invalid
    pub fn parse(header: &str) -> Result<Self, ParseRangeError> {
        let err = || ParseRangeError { _priv: () };
        let s = header.strip_prefix("bytes=").ok_or_else(err)?.as_bytes();

        // Strict: no unit-case folding, no comma handling, no overflow saturation.
        // The dispatch path uses [`parse_lenient`] instead.
        let range = parse_single_spec(s, false).ok_or_else(err)?;

        // Keep the historical `i64` bound on the strict API: downstream code does
        // arithmetic on these values in `i64`.
        if let Range::Int { first, last } = range {
            if first > (i64::MAX as u64) {
                return Err(err());
            }
            if last.is_some_and(|last| last > (i64::MAX as u64)) {
                return Err(err());
            }
        }

        Ok(range)
    }

    #[must_use]
    pub fn to_header_string(&self) -> String {
        match self {
            Range::Int { first, last } => match last {
                Some(last) => format!("bytes={first}-{last}"),
                None => format!("bytes={first}-"),
            },
            Range::Suffix { length } => format!("bytes=-{length}"),
        }
    }

    /// Checks if the range is satisfiable
    ///  according to [RFC9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-byte-ranges).
    /// # Errors
    /// Returns an error if the range is not satisfiable
    #[allow(clippy::range_plus_one)] // cannot be fixed
    pub fn check(&self, full_length: u64) -> Result<ops::Range<u64>, RangeNotSatisfiable> {
        let err = || RangeNotSatisfiable { _priv: () };
        match *self {
            Range::Int { first, last } => {
                if first >= full_length {
                    return Err(err());
                }
                // 0 <= first < full_length

                match last {
                    Some(last) => {
                        let last = last.min(full_length - 1);
                        if first > last {
                            return Err(err());
                        }
                        // 0 <= first <= last < full_length
                        Ok(first..last + 1)
                    }
                    // 0 <= first < full_length
                    None => Ok(first..full_length),
                }
            }
            Range::Suffix { length } => {
                if length == 0 {
                    return Err(err());
                }
                let length = length.min(full_length);
                Ok((full_length - length)..full_length)
            }
        }
    }
}

impl From<RangeNotSatisfiable> for S3Error {
    #[inline]
    fn from(_: RangeNotSatisfiable) -> Self {
        S3ErrorCode::InvalidRange.into()
    }
}

impl http::TryFromHeaderValue for Range {
    type Error = ParseRangeError;

    fn try_from_header_value(val: &http::HeaderValue) -> Result<Self, Self::Error> {
        let header = str::from_ascii_simd(val.as_bytes()).map_err(|_| ParseRangeError { _priv: () })?;
        Self::parse(header)
    }
}

/// One decimal numeral inside a byte-range specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decimal {
    /// Fits in [`u64`].
    Value(u64),
    /// Syntactically valid but larger than [`u64::MAX`].
    Overflow,
    /// Empty, or not `1*DIGIT`.
    Invalid,
}

fn parse_decimal(digits: &[u8]) -> Decimal {
    if digits.is_empty() {
        return Decimal::Invalid;
    }

    let mut value = 0_u64;
    for byte in digits {
        if !byte.is_ascii_digit() {
            return Decimal::Invalid;
        }

        let digit = u64::from(byte - b'0');
        match value.checked_mul(10).and_then(|value| value.checked_add(digit)) {
            Some(next) => value = next,
            None => return Decimal::Overflow,
        }
    }

    Decimal::Value(value)
}

fn parse_decimal_prefix(s: &[u8]) -> (Decimal, &[u8]) {
    let end = s.iter().position(|byte| !byte.is_ascii_digit()).unwrap_or(s.len());
    let Some((digits, rest)) = s.split_at_checked(end) else {
        return (Decimal::Invalid, s);
    };

    (parse_decimal(digits), rest)
}

/// Parses one byte-range specifier, i.e. the part after `bytes=`.
///
/// With `lenient`, a numeral larger than [`u64::MAX`] is saturated instead of
/// rejected: `first-pos` becomes [`u64::MAX`] (unsatisfiable, answered with
/// `416`) and an overflowing `last-pos` is treated as absent, i.e. "to the end"
/// (RFC 9110 Section 14.1.2). Without `lenient`, every overflow is a parse error.
fn parse_single_spec(s: &[u8], lenient: bool) -> Option<Range> {
    // suffix-range = "-" suffix-length
    if let [b'-', digits @ ..] = s {
        if digits.is_empty() {
            // `bytes=-` has an empty suffix-length, but `suffix-length = 1*DIGIT`.
            return None;
        }

        return match parse_decimal(digits) {
            Decimal::Value(length) => Some(Range::Suffix { length }),
            Decimal::Overflow if lenient => Some(Range::Suffix { length: u64::MAX }),
            Decimal::Overflow | Decimal::Invalid => None,
        };
    }

    // int-range = first-pos "-" [ last-pos ]
    let (first, rest) = parse_decimal_prefix(s);
    let first = match first {
        Decimal::Value(first) => first,
        Decimal::Overflow if lenient => u64::MAX,
        Decimal::Overflow | Decimal::Invalid => return None,
    };

    let [b'-', digits @ ..] = rest else { return None };

    if digits.is_empty() {
        return Some(Range::Int { first, last: None });
    }

    let last = match parse_decimal(digits) {
        Decimal::Value(last) => Some(last),
        // An overflowing `last-pos` means "to the end"; RFC 9110 Section 14.1.2.
        Decimal::Overflow if lenient => None,
        Decimal::Overflow | Decimal::Invalid => return None,
    };

    if last.is_some_and(|last| first > last) {
        return None;
    }

    Some(Range::Int { first, last })
}

/// Parses a `Range` header field value for request dispatch.
///
/// Everything that cannot be served as exactly one byte range is ignored
/// (returns `None`) instead of being rejected, matching Amazon S3: the request
/// is then answered with the whole representation and `200 OK`.
///
/// Ignored: an unknown range unit (RFC 9110 Section 14.2 requires an origin
/// server to ignore it), a syntax error, a range set with no specifier, and
/// multiple ranges (S3 does not support retrieving multiple ranges per
/// request). Empty list elements are skipped (Section 5.6.1.2) and the unit
/// name is matched case-insensitively (Section 14.1).
pub(crate) fn parse_lenient(header: &str) -> Option<Range> {
    let (unit, rest) = header.split_once('=')?;

    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }

    let is_ows = |c: char| c == ' ' || c == '\t';

    let mut iter = rest.split(',').peekable();
    let mut first = true;
    let mut spec = None;

    while let Some(element) = iter.next() {
        let is_last = iter.peek().is_none();

        // OWS is allowed on both sides of the list separators, but not around
        // `=`: `bytes=0-1, 3-4` is valid, `bytes= 0-3` is not (Section 5.6.1).
        let element = if first {
            if element.starts_with(is_ows) {
                return None;
            }
            element
        } else {
            element.trim_start_matches(is_ows)
        };

        let element = if is_last { element } else { element.trim_end_matches(is_ows) };

        first = false;

        if element.is_empty() {
            continue;
        }

        if spec.is_some() {
            // Multiple ranges: S3 answers the whole representation instead.
            return None;
        }

        spec = Some(element);
    }

    parse_single_spec(spec?.as_bytes(), true)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    fn range_int_inclusive(first: u64, last: u64) -> Range {
        Range::Int { first, last: Some(last) }
    }

    fn range_int_from(first: u64) -> Range {
        Range::Int { first, last: None }
    }

    fn range_suffix(length: u64) -> Range {
        Range::Suffix { length }
    }

    #[test]
    fn byte_range() {
        let cases = [
            ("bytes=0-499", Ok(range_int_inclusive(0, 499))),
            ("bytes=0-499;", Err(())),
            ("bytes=9500-", Ok(range_int_from(9500))),
            ("bytes=9500-0-", Err(())),
            ("bytes=9500", Err(())),
            ("bytes=0-0", Ok(range_int_inclusive(0, 0))),
            ("bytes=-500", Ok(range_suffix(500))),
            ("bytes=-500 ", Err(())),
            ("bytes=-+500", Err(())),
            ("bytes=-1000000000000000000000000", Err(())),
            ("bytes=-0", Ok(range_suffix(0))),
            ("bytes=-000", Ok(range_suffix(0))),
        ];

        for (input, expected) in &cases {
            let output = Range::parse(input);
            match expected {
                Ok(expected) => assert_eq!(output.unwrap(), *expected),
                Err(()) => assert!(output.is_err()),
            }
        }
    }

    #[test]
    fn satisfiable() {
        let cases = [
            (10000, range_int_from(9500), Ok(9500..10000)),
            (10000, range_int_from(10000), Err(())),
            (10000, range_int_inclusive(0, 499), Ok(0..500)),
            (10000, range_int_inclusive(0, 0), Ok(0..1)),
            (10000, range_int_inclusive(9500, 50000), Ok(9500..10000)),
            (10000, range_int_inclusive(10000, 10000), Err(())),
            (10000, range_suffix(500), Ok(9500..10000)),
            (10000, range_suffix(10000), Ok(0..10000)),
            (10000, range_suffix(0), Err(())),
            (0, range_int_from(0), Err(())),
            (0, range_suffix(1), Ok(0..0)),
            (0, range_suffix(0), Err(())),
        ];

        for &(full_length, ref range, ref expected) in &cases {
            let output = range.check(full_length);
            match expected {
                Ok(expected) => assert_eq!(output.unwrap(), *expected),
                Err(()) => assert!(output.is_err(), "{full_length:?}, {range:?}"),
            }
        }
    }

    #[test]
    fn to_header_string_int_inclusive() {
        let range = range_int_inclusive(0, 499);
        assert_eq!(range.to_header_string(), "bytes=0-499");
    }

    #[test]
    fn to_header_string_int_from() {
        let range = range_int_from(9500);
        assert_eq!(range.to_header_string(), "bytes=9500-");
    }

    #[test]
    fn to_header_string_suffix() {
        let range = range_suffix(500);
        assert_eq!(range.to_header_string(), "bytes=-500");
    }

    #[test]
    fn to_header_string_roundtrip() {
        let cases = [range_int_inclusive(0, 499), range_int_from(9500), range_suffix(500)];
        for range in &cases {
            let header = range.to_header_string();
            let parsed = Range::parse(&header).unwrap();
            assert_eq!(*range, parsed);
        }
    }

    #[test]
    fn try_from_header_value() {
        use crate::http::TryFromHeaderValue;
        let hv = http::HeaderValue::from_static("bytes=0-499");
        let range = Range::try_from_header_value(&hv).unwrap();
        assert_eq!(range, range_int_inclusive(0, 499));

        let hv = http::HeaderValue::from_static("bytes=100-");
        let range = Range::try_from_header_value(&hv).unwrap();
        assert_eq!(range, range_int_from(100));

        let hv = http::HeaderValue::from_static("bytes=-500");
        let range = Range::try_from_header_value(&hv).unwrap();
        assert_eq!(range, range_suffix(500));
    }

    #[test]
    fn range_not_satisfiable_to_s3_error() {
        let err = RangeNotSatisfiable { _priv: () };
        let s3_err: S3Error = err.into();
        let code = s3_err.code();
        assert_eq!(code, &S3ErrorCode::InvalidRange);
    }

    #[test]
    fn parse_first_exceeds_i64_max() {
        let big = format!("bytes={}-", i64::MAX as u64 + 1);
        assert!(Range::parse(&big).is_err());
    }

    #[test]
    fn parse_last_exceeds_i64_max() {
        let big = format!("bytes=0-{}", i64::MAX as u64 + 1);
        assert!(Range::parse(&big).is_err());
    }

    #[test]
    fn parse_first_greater_than_last() {
        assert!(Range::parse("bytes=500-100").is_err());
    }

    #[test]
    fn strict_parse_rejects_an_empty_suffix_length() {
        // `suffix-length = 1*DIGIT`: `bytes=-` is not `bytes=-0`.
        assert!(Range::parse("bytes=-").is_err());
        assert_eq!(Range::parse("bytes=-0").unwrap(), range_suffix(0));
    }

    #[test]
    fn strict_parse_still_rejects_overflow() {
        let huge = "9".repeat(30);
        assert!(Range::parse(&format!("bytes=0-{huge}")).is_err());
        assert!(Range::parse(&format!("bytes={huge}-")).is_err());
        assert!(Range::parse(&format!("bytes=-{huge}")).is_err());
    }

    #[test]
    fn lenient_accepts_a_single_byte_range() {
        let cases = [
            ("bytes=0-3", range_int_inclusive(0, 3)),
            ("bytes=00-03", range_int_inclusive(0, 3)),
            ("bytes=0-", range_int_from(0)),
            ("bytes=-5", range_suffix(5)),
            ("bytes=-0", range_suffix(0)),
            ("bytes=100-200", range_int_inclusive(100, 200)),
            // RFC 9110 Section 14.1: range unit names are case-insensitive.
            ("BYTES=0-3", range_int_inclusive(0, 3)),
            ("bYtEs=0-3", range_int_inclusive(0, 3)),
            // RFC 9110 Section 5.6.1.2: empty list elements are ignored.
            ("bytes=,0-3", range_int_inclusive(0, 3)),
            ("bytes=0-3,", range_int_inclusive(0, 3)),
            ("bytes=0-3, ", range_int_inclusive(0, 3)),
            ("bytes=0-3,,", range_int_inclusive(0, 3)),
            // OWS is allowed around the list separators.
            ("bytes=0-3 ,", range_int_inclusive(0, 3)),
            ("bytes=, 0-3", range_int_inclusive(0, 3)),
        ];

        for (input, expected) in &cases {
            assert_eq!(parse_lenient(input), Some(*expected), "{input}");
        }
    }

    #[test]
    fn lenient_ignores_unsupported_range_fields() {
        let cases = [
            // Unknown range unit: RFC 9110 Section 14.2 requires ignoring it.
            "chars=0-1",
            "items=0-3",
            "items=abc-def",
            // Multiple ranges: S3 does not retrieve multiple ranges per request.
            "bytes=0-1,3-4",
            "bytes=0-1, 3-4",
            "bytes=0-3,bytes=5-6",
            "bytes=0-1,,3-4",
            "bytes=0-3, ,0-4",
            // Invalid syntax.
            "bytes=",
            "bytes",
            "bytes=,",
            "bytes=5-3",
            "bytes=abc",
            "bytes=-",
            "bytes=+0-3",
            "bytes=0x1-3",
            "bytes=0-3;",
            "bytes=0-3/2",
            "bytes =0-3",
            "bytes= 0-3",
            "bytes=0-3 ",
            "bytes=0-\t3",
        ];

        for input in &cases {
            assert_eq!(parse_lenient(input), None, "{input}");
        }
    }

    #[test]
    fn lenient_saturates_overflowing_numerals() {
        let huge = "9".repeat(30);

        // An overflowing last-pos means "to the end" (Section 14.1.2).
        assert_eq!(parse_lenient(&format!("bytes=0-{huge}")), Some(range_int_from(0)));
        // An overflowing first-pos is unsatisfiable for any representation.
        assert_eq!(
            parse_lenient(&format!("bytes={huge}-")),
            Some(Range::Int {
                first: u64::MAX,
                last: None
            })
        );
        assert!(parse_lenient(&format!("bytes={huge}-")).unwrap().check(10).is_err());
        // An overflowing suffix-length covers the whole representation.
        assert_eq!(parse_lenient(&format!("bytes=-{huge}")), Some(range_suffix(u64::MAX)));
    }
}

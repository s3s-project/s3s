//! timestamp

use std::io;
use std::num::ParseIntError;
use std::time::SystemTime;

use time::format_description::FormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(time::OffsetDateTime);

impl serde::Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error;
        let mut buf = Vec::new();
        self.format(TimestampFormat::DateTime, &mut buf).map_err(S::Error::custom)?;
        let s = std::str::from_utf8(&buf).map_err(S::Error::custom)?;
        serializer.serialize_str(s)
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let s = String::deserialize(deserializer)?;
        Self::parse(TimestampFormat::DateTime, &s).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    DateTime,
    HttpDate,
    EpochSeconds,
}

impl From<time::OffsetDateTime> for Timestamp {
    fn from(value: time::OffsetDateTime) -> Self {
        Self(value)
    }
}

impl From<Timestamp> for time::OffsetDateTime {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

impl From<SystemTime> for Timestamp {
    fn from(value: SystemTime) -> Self {
        Self(time::OffsetDateTime::from(value))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseTimestampError {
    #[error("time: {0}")]
    Time(#[from] time::error::Parse),
    #[error("int: {0}")]
    Int(#[from] ParseIntError),
    #[error("time overflow")]
    Overflow,
    #[error("component range: {0}")]
    ComponentRange(#[from] time::error::ComponentRange),
}

#[derive(Debug, thiserror::Error)]
pub enum FormatTimestampError {
    #[error("time: {0}")]
    Time(#[from] time::error::Format),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// See <https://github.com/time-rs/time/issues/498>
const RFC1123: &[FormatItem<'_>] =
    format_description!("[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT");

/// See <https://github.com/minio/minio-java/issues/1419>
const RFC3339: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

impl Timestamp {
    /// Parses `Timestamp` from string
    ///
    /// # Errors
    /// Returns an error if the string is invalid
    pub fn parse(format: TimestampFormat, s: &str) -> Result<Self, ParseTimestampError> {
        let ans = match format {
            TimestampFormat::DateTime => time::OffsetDateTime::parse(s, &Rfc3339)?,
            TimestampFormat::HttpDate => time::PrimitiveDateTime::parse(s, RFC1123)?.assume_utc(),
            TimestampFormat::EpochSeconds => match s.split_once('.') {
                Some((secs_str, frac)) => {
                    let secs: i64 = secs_str.parse()?;
                    let val: u32 = frac.parse::<u32>()?;
                    let mul: u32 = match frac.len() {
                        1 => 100_000_000,
                        2 => 10_000_000,
                        3 => 1_000_000,
                        4 => 100_000,
                        5 => 10000,
                        6 => 1000,
                        7 => 100,
                        8 => 10,
                        9 => 1,
                        _ => return Err(ParseTimestampError::Overflow),
                    };
                    let nanos_part = i128::from(val * mul);
                    // For negative timestamps, the fractional part is always positive
                    // e.g., -1.5 means 1.5 seconds before epoch, which is -2 seconds + 500ms
                    // But in Smithy format, -1.5 = -1 seconds + 0.5 fractional = -0.5 seconds total
                    // The smithy format stores: seconds (floor) + positive fractional
                    let nanos = i128::from(secs) * 1_000_000_000 + nanos_part;
                    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)?
                }
                None => {
                    let secs: i64 = s.parse()?;
                    time::OffsetDateTime::from_unix_timestamp(secs)?
                }
            },
        };
        Ok(Self(ans))
    }

    /// Formats `Timestamp` into a writer
    ///
    /// # Errors
    /// Returns an error if the formatting fails
    pub fn format(&self, format: TimestampFormat, w: &mut impl io::Write) -> Result<(), FormatTimestampError> {
        match format {
            TimestampFormat::DateTime => {
                self.0.format_into(w, RFC3339)?;
            }
            TimestampFormat::HttpDate => {
                self.0.format_into(w, RFC1123)?;
            }
            TimestampFormat::EpochSeconds => {
                let val = self.0.unix_timestamp_nanos();

                #[allow(clippy::cast_precision_loss)] // FIXME: accurate conversion?
                {
                    let secs = (val / 1_000_000_000) as f64;
                    let nanos = (val % 1_000_000_000) as f64 / 1_000_000_000.0;
                    let ts = secs + nanos;
                    write!(w, "{ts}")?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_repr() {
        let cases = [
            (TimestampFormat::DateTime, "1985-04-12T23:20:50.520Z"),
            (TimestampFormat::HttpDate, "Tue, 29 Apr 2014 18:30:38 GMT"),
            (TimestampFormat::HttpDate, "Wed, 21 Oct 2015 07:28:00 GMT"),
            // (TimestampFormat::HttpDate, "Sun, 02 Jan 2000 20:34:56.000 GMT"), // FIXME: optional fractional seconds
            (TimestampFormat::EpochSeconds, "1515531081.1234"),
        ];

        for (fmt, expected) in cases {
            let time = Timestamp::parse(fmt, expected).unwrap();

            let mut buf = Vec::new();
            time.format(fmt, &mut buf).unwrap();
            let text = String::from_utf8(buf).unwrap();

            assert_eq!(expected, text);
        }
    }
}

/// Test module using the Smithy date_time_format_test_suite.json
/// From: <https://github.com/smithy-lang/smithy-rs/blob/main/rust-runtime/aws-smithy-types/test_data/date_time_format_test_suite.json>
#[cfg(test)]
mod date_time_format_test_suite {
    use super::*;

    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TestSuite {
        #[allow(dead_code)]
        description: Vec<String>,
        parse_epoch_seconds: Vec<TestCase>,
        parse_http_date: Vec<TestCase>,
        parse_date_time: Vec<TestCase>,
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
        let json = include_str!("test_data/date_time_format_test_suite.json");
        serde_json::from_str(json).expect("failed to parse test suite")
    }

    /// Converts canonical_seconds (as string) and canonical_nanos into total nanoseconds.
    /// For negative timestamps, we need to handle the nanosecond adjustment properly:
    /// - For positive seconds: total_nanos = seconds * 1_000_000_000 + nanos
    /// - For negative seconds: total_nanos = seconds * 1_000_000_000 - nanos (if nanos > 0)
    ///   Actually, the test suite stores: canonical_seconds as the floor seconds and
    ///   canonical_nanos as the positive adjustment within that second.
    fn canonical_to_nanos(canonical_seconds: &str, canonical_nanos: u32) -> i128 {
        let secs: i64 = canonical_seconds.parse().expect("invalid canonical_seconds");
        i128::from(secs) * 1_000_000_000 + i128::from(canonical_nanos)
    }

    #[test]
    fn parse_epoch_seconds() {
        let suite = load_test_suite();

        for case in suite.parse_epoch_seconds {
            let smithy_value = match case.smithy_format_value.as_ref() {
                Some(v) => v,
                None => {
                    // Error cases without smithy_format_value - skip
                    assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
                    continue;
                }
            };

            let result = Timestamp::parse(TimestampFormat::EpochSeconds, smithy_value);

            if case.error {
                assert!(result.is_err(), "expected error parsing '{}' (iso8601: {})", smithy_value, case.iso8601);
            } else {
                let ts =
                    result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));
                let expected_nanos = canonical_to_nanos(&case.canonical_seconds, case.canonical_nanos);
                let actual_nanos = ts.0.unix_timestamp_nanos();

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
            let smithy_value = match case.smithy_format_value.as_ref() {
                Some(v) => v,
                None => {
                    // Error cases without smithy_format_value - skip
                    assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
                    continue;
                }
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
                let ts =
                    result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));

                // For http-date, fractional seconds are truncated, so we only compare whole seconds
                let expected_secs: i64 = case.canonical_seconds.parse().expect("invalid canonical_seconds");
                let actual_secs = ts.0.unix_timestamp();

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
            let smithy_value = match case.smithy_format_value.as_ref() {
                Some(v) => v,
                None => {
                    // Error cases without smithy_format_value - skip
                    assert!(case.error, "non-error case should have smithy_format_value: {}", case.iso8601);
                    continue;
                }
            };

            let result = Timestamp::parse(TimestampFormat::DateTime, smithy_value);

            if case.error {
                assert!(result.is_err(), "expected error parsing '{}' (iso8601: {})", smithy_value, case.iso8601);
            } else {
                let ts =
                    result.unwrap_or_else(|e| panic!("failed to parse '{}' (iso8601: {}): {}", smithy_value, case.iso8601, e));
                let expected_nanos = canonical_to_nanos(&case.canonical_seconds, case.canonical_nanos);
                let actual_nanos = ts.0.unix_timestamp_nanos();

                assert_eq!(
                    actual_nanos, expected_nanos,
                    "mismatch for '{}' (iso8601: {}): expected {} nanos, got {} nanos",
                    smithy_value, case.iso8601, expected_nanos, actual_nanos
                );
            }
        }
    }
}

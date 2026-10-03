// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The timestamp value type and its wire formats.

use std::io;
use std::time::Duration;
use std::time::SystemTime;

use crate::ConvertTimestampError;
use crate::FormatTimestampError;
use crate::ParseTimestampError;
use crate::parse;

/// The wire format of a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimestampFormat {
    /// RFC 3339 date-time, for example `2024-06-15T07:00:00.123Z`.
    DateTime,
    /// IMF-fixdate, the date form used by HTTP, for example
    /// `Sat, 15 Jun 2024 07:00:00 GMT`.
    HttpDate,
    /// Seconds since the Unix epoch with an optional fraction, for example
    /// `1718434800.123`.
    EpochSeconds,
}

/// An instant on the UTC timeline, as used by the S3 API.
///
/// The value is a nanosecond-precision instant; the wire format is selected
/// per call through [`TimestampFormat`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(jiff::Timestamp);

impl Timestamp {
    /// The Unix epoch, `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Self = Self(jiff::Timestamp::UNIX_EPOCH);

    /// Creates a timestamp from a number of seconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns an error if the value is outside the representable range.
    pub fn from_unix_seconds(seconds: i64) -> Result<Self, ConvertTimestampError> {
        Self::from_unix_nanos(i128::from(seconds) * 1_000_000_000)
    }

    /// Creates a timestamp from a number of nanoseconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns an error if the value is outside the representable range. The
    /// range is narrower than the four-digit year range of the wire formats:
    /// it stops at `-9999-01-02T01:59:59Z` on the early side and at
    /// `9999-12-30T22:00:00.999999999Z` on the late side.
    pub fn from_unix_nanos(nanoseconds: i128) -> Result<Self, ConvertTimestampError> {
        if !in_range(nanoseconds) {
            return Err(ConvertTimestampError::OutOfRange);
        }

        jiff::Timestamp::from_nanosecond(nanoseconds)
            .map(Self)
            .map_err(|_| ConvertTimestampError::OutOfRange)
    }

    /// Wraps an instant of the internal representation.
    pub(crate) fn from_inner(ts: jiff::Timestamp) -> Self {
        Self(ts)
    }

    /// Parses a timestamp from its representation in the given format.
    ///
    /// # Errors
    ///
    /// Returns [`ParseTimestampError::InvalidFormat`] when the input does not
    /// match the grammar of the format, [`ParseTimestampError::OutOfRange`]
    /// when a field or the instant is outside the representable range,
    /// [`ParseTimestampError::FractionTooLong`] when an epoch-seconds fraction
    /// carries more than nine digits, and [`ParseTimestampError::Overflow`]
    /// when combining the seconds and the fraction overflows.
    pub fn parse(format: TimestampFormat, s: &str) -> Result<Self, ParseTimestampError> {
        match format {
            TimestampFormat::DateTime => parse::date_time(s),
            TimestampFormat::HttpDate => parse::http_date(s),
            TimestampFormat::EpochSeconds => parse::epoch_seconds(s),
        }
    }

    /// Writes the timestamp in the given format to `w`.
    ///
    /// # Errors
    ///
    /// Returns [`FormatTimestampError::OutOfRange`] when the instant cannot be
    /// represented in the requested format, because the date-time and the HTTP
    /// date carry a four-digit year, and [`FormatTimestampError::Io`] when the
    /// writer fails.
    pub fn format(&self, format: TimestampFormat, w: &mut impl io::Write) -> Result<(), FormatTimestampError> {
        match format {
            TimestampFormat::DateTime => crate::format::date_time(&self.0, w),
            TimestampFormat::HttpDate => crate::format::http_date(&self.0, w),
            TimestampFormat::EpochSeconds => crate::format::epoch_seconds(&self.0, w),
        }
    }

    /// Returns the number of whole seconds since the Unix epoch.
    ///
    /// The value is rounded towards negative infinity, so an instant before the
    /// epoch reports the second that its subsecond component belongs to: minus
    /// half a second is minus one second, with five hundred million nanoseconds
    /// of [`Timestamp::subsec_nanos`]. The identity
    /// `unix_seconds() * 1_000_000_000 + subsec_nanos() == unix_nanos()`
    /// therefore holds for every instant.
    #[must_use]
    pub fn unix_seconds(&self) -> i64 {
        let seconds = self.0.as_second();
        if self.0.subsec_nanosecond() < 0 {
            return seconds - 1;
        }
        seconds
    }

    /// Returns the number of nanoseconds since the Unix epoch.
    #[must_use]
    pub fn unix_nanos(&self) -> i128 {
        self.0.as_nanosecond()
    }

    /// Returns the subsecond component as a non-negative number of nanoseconds.
    ///
    /// # Panics
    ///
    /// Never panics: the subsecond component is always within one second.
    #[must_use]
    pub fn subsec_nanos(&self) -> u32 {
        let nanos = self.0.subsec_nanosecond().rem_euclid(1_000_000_000);
        u32::try_from(nanos).expect("the subsecond component is less than one second")
    }
}

/// Returns whether a nanosecond count is within the representable range.
///
/// The range of the internal representation is narrower than the four-digit
/// year range of the wire formats: the library reserves the maximum UTC offset
/// at both ends.
fn in_range(nanoseconds: i128) -> bool {
    let min = jiff::Timestamp::MIN.as_nanosecond();
    let max = jiff::Timestamp::MAX.as_nanosecond();
    (min..=max).contains(&nanoseconds)
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::UNIX_EPOCH
    }
}

impl From<SystemTime> for Timestamp {
    /// Converts a system time; values outside the representable range saturate
    /// to the earliest or the latest representable instant.
    fn from(value: SystemTime) -> Self {
        if let Ok(ts) = jiff::Timestamp::try_from(value) {
            return Self(ts);
        }

        let after_epoch = value.duration_since(SystemTime::UNIX_EPOCH).is_ok();
        if after_epoch {
            Self(jiff::Timestamp::MAX)
        } else {
            Self(jiff::Timestamp::MIN)
        }
    }
}

impl TryFrom<Timestamp> for SystemTime {
    type Error = ConvertTimestampError;

    /// Converts the instant into a platform [`SystemTime`].
    ///
    /// The representable range is platform dependent: a Unix `SystemTime` counts signed
    /// seconds from the epoch, while a Windows one counts unsigned 100-nanosecond ticks
    /// from 1601. An instant the platform cannot represent is reported as
    /// [`ConvertTimestampError::OutOfRange`] instead of panicking in the conversion.
    fn try_from(value: Timestamp) -> Result<Self, Self::Error> {
        let seconds = value.unix_seconds();
        let nanoseconds = value.subsec_nanos();
        let converted = if seconds >= 0 {
            let magnitude = u64::try_from(seconds).expect("a non-negative i64 fits in u64");
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(magnitude, nanoseconds))
        } else {
            let magnitude = seconds.unsigned_abs();
            let (whole, part) = if nanoseconds == 0 {
                (magnitude, 0)
            } else {
                (magnitude - 1, 1_000_000_000 - nanoseconds)
            };
            SystemTime::UNIX_EPOCH.checked_sub(Duration::new(whole, part))
        };

        converted.ok_or(ConvertTimestampError::OutOfRange)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error;

        let mut buf = Vec::new();
        self.format(TimestampFormat::DateTime, &mut buf).map_err(S::Error::custom)?;
        let text = std::str::from_utf8(&buf).map_err(S::Error::custom)?;
        serializer.serialize_str(text)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(TimestampFormat::DateTime, &text).map_err(D::Error::custom)
    }
}

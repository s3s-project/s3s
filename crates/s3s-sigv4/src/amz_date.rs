// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! x-amz-date

use std::fmt::Write as _;

use arrayvec::ArrayString;

/// x-amz-date
#[derive(Debug, Clone)]
pub struct AmzDate {
    /// year
    year: u16,
    /// month
    month: u8,
    /// day
    day: u8,
    /// hour
    hour: u8,
    /// minute
    minute: u8,
    /// second
    second: u8,
}

/// [`AmzDate`] parse error
#[derive(Debug, thiserror::Error)]
#[error("ParseAmzDateError")]
pub struct ParseAmzDateError(());

impl AmzDate {
    /// Parses `AmzDate` from header
    ///
    /// # Errors
    /// Returns an error if the header is invalid
    pub fn parse(header: &str) -> Result<Self, ParseAmzDateError> {
        self::parser::parse(header).map_err(|_| ParseAmzDateError(()))
    }

    /// `{YYYY}{MM}{DD}T{HH}{MM}{SS}Z`
    #[must_use]
    pub fn fmt_iso8601(&self) -> ArrayString<16> {
        let mut buf = <ArrayString<16>>::new();
        let (y, m, d, hh, mm, ss) = (self.year, self.month, self.day, self.hour, self.minute, self.second);
        write!(&mut buf, "{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z").unwrap();
        buf
    }

    /// `{YYYY}{MM}{DD}`
    #[must_use]
    pub fn fmt_date(&self) -> ArrayString<8> {
        let mut buf = <ArrayString<8>>::new();
        write!(&mut buf, "{:04}{:02}{:02}", self.year, self.month, self.day).unwrap();
        buf
    }

    /// Converts to a UTC timestamp; returns `None` for out-of-range calendar values.
    #[must_use]
    pub fn to_time(&self) -> Option<jiff::Timestamp> {
        let dt = jiff::civil::DateTime::new(
            i16::try_from(self.year).ok()?,
            i8::try_from(self.month).ok()?,
            i8::try_from(self.day).ok()?,
            i8::try_from(self.hour).ok()?,
            i8::try_from(self.minute).ok()?,
            i8::try_from(self.second).ok()?,
            0,
        )
        .ok()?;
        Some(dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp())
    }
}

mod parser {
    use super::*;

    use crate::parser::{Error, digit2, digit4};

    macro_rules! ensure {
        ($cond:expr) => {
            if !$cond {
                return Err(Error);
            }
        };
    }

    pub fn parse(input: &str) -> Result<AmzDate, Error> {
        let x = input.as_bytes();
        ensure!(x.len() == 16);

        let year = digit4([x[0], x[1], x[2], x[3]])?;
        let month = digit2([x[4], x[5]])?;
        let day = digit2([x[6], x[7]])?;
        ensure!(x[8] == b'T');

        let hour = digit2([x[9], x[10]])?;
        let minute = digit2([x[11], x[12]])?;
        let second = digit2([x[13], x[14]])?;
        ensure!(x[15] == b'Z');

        Ok(AmzDate {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_fmt() {
        let date = AmzDate::parse("20130524T000000Z").unwrap();
        assert_eq!(date.fmt_iso8601().as_str(), "20130524T000000Z");
        assert_eq!(date.fmt_date().as_str(), "20130524");
    }

    #[test]
    fn to_time() {
        let date = AmzDate::parse("20130524T000000Z").unwrap();
        let ts = date.to_time().unwrap();
        assert_eq!(ts.as_second(), 1_369_353_600);
    }

    #[test]
    fn parse_rejects_invalid_input() {
        assert!(AmzDate::parse("20130524").is_err());
        assert!(AmzDate::parse("20130524000000Z").is_err());
        assert!(AmzDate::parse("20130524T000000").is_err());
        assert!(AmzDate::parse("abcdefghijklmnop").is_err());
    }

    #[test]
    fn to_time_rejects_out_of_range_values() {
        // Digits parse, but the calendar value is out of range.
        assert!(matches!(AmzDate::parse("20130524T999999Z"), Ok(d) if d.to_time().is_none()));
        assert!(matches!(AmzDate::parse("20131324T000000Z"), Ok(d) if d.to_time().is_none()));
        assert!(matches!(AmzDate::parse("20130230T000000Z"), Ok(d) if d.to_time().is_none()));
    }
}

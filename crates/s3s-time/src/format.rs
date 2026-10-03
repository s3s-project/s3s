// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The writers of the three wire formats.

use std::io;

use jiff::Timestamp;
use jiff::fmt::rfc2822;
use jiff::fmt::temporal;
use jiff::tz::TimeZone;

use crate::FormatTimestampError;

/// The date-time printer: the wire format carries exactly three fractional digits.
static DATE_TIME: temporal::DateTimePrinter = temporal::DateTimePrinter::new().precision(Some(3));

/// The HTTP date printer: IMF-fixdate always ends with `GMT`.
static HTTP_DATE: rfc2822::DateTimePrinter = rfc2822::DateTimePrinter::new();

/// Writes an RFC 3339 date-time with exactly three fractional digits.
pub fn date_time(ts: &Timestamp, w: &mut impl io::Write) -> Result<(), FormatTimestampError> {
    check_year(ts)?;
    let mut wtr = IoWriter::new(w);
    DATE_TIME.print_timestamp(ts, &mut wtr).map_err(|_| wtr.error())
}

/// Writes an IMF-fixdate HTTP date, dropping the subsecond component.
pub fn http_date(ts: &Timestamp, w: &mut impl io::Write) -> Result<(), FormatTimestampError> {
    check_year(ts)?;
    let mut wtr = IoWriter::new(w);
    HTTP_DATE.print_timestamp_rfc9110(ts, &mut wtr).map_err(|_| wtr.error())
}

/// Writes epoch seconds as the shortest exact decimal.
///
/// The fraction is always positive, so an instant before the epoch is written
/// as the floor second plus the fraction: minus half a second is `-1.5`.
pub fn epoch_seconds(ts: &Timestamp, w: &mut impl io::Write) -> Result<(), FormatTimestampError> {
    let nanos = ts.as_nanosecond();
    let seconds = nanos.div_euclid(1_000_000_000);
    let subsecond = nanos.rem_euclid(1_000_000_000);

    if subsecond == 0 {
        write!(w, "{seconds}")?;
        return Ok(());
    }

    // Drop the trailing zeros by dividing them off and pad to what is left,
    // so the value goes straight to the writer without an intermediate string.
    let mut digits = subsecond;
    let mut width = 9_usize;
    while digits % 10 == 0 {
        digits /= 10;
        width -= 1;
    }

    write!(w, "{seconds}.{digits:0width$}")?;
    Ok(())
}

/// Rejects instants whose year does not fit the four digits of the wire formats.
fn check_year(ts: &Timestamp) -> Result<(), FormatTimestampError> {
    let year = ts.to_zoned(TimeZone::UTC).year();
    if (0..=9999).contains(&year) {
        Ok(())
    } else {
        Err(FormatTimestampError::OutOfRange)
    }
}

/// An adapter that records the error of an [`io::Write`] writer.
struct IoWriter<'a, W> {
    inner: &'a mut W,
    error: Option<io::Error>,
}

impl<'a, W: io::Write> IoWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, error: None }
    }

    /// Returns the writer error, or an out-of-range error when the format refused the value.
    fn error(self) -> FormatTimestampError {
        match self.error {
            Some(err) => FormatTimestampError::Io(err),
            None => FormatTimestampError::OutOfRange,
        }
    }
}

impl<W: io::Write> jiff::fmt::Write for IoWriter<'_, W> {
    fn write_str(&mut self, string: &str) -> Result<(), jiff::Error> {
        match self.inner.write_all(string.as_bytes()) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.error = Some(err);
                Err(jiff::Error::from_args(format_args!("the writer failed")))
            }
        }
    }
}

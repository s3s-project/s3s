// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Date and time wire formats for the S3 API.
//!
//! The crate carries the wire formats that the S3 data model uses for time
//! values:
//!
//! - `date-time`: RFC 3339 date-time, written with exactly three fractional
//!   digits,
//! - `http-date`: IMF-fixdate, the date form used by HTTP,
//! - `epoch-seconds`: seconds since the Unix epoch with an optional fraction,
//!   written as the shortest exact decimal.
//!
//! # Conventions
//!
//! - `date-time` is written in UTC with exactly three fractional digits, for
//!   example `2024-06-15T07:00:00.123Z`. The fraction is truncated, not rounded.
//! - `http-date` is IMF-fixdate with second precision, for example
//!   `Sat, 15 Jun 2024 07:00:00 GMT`; the subsecond component is dropped.
//! - `epoch-seconds` is written as the shortest exact decimal, for example
//!   `1718434800.123`. A fraction is always positive, so half a second before
//!   the epoch is `-1.5`.
//!
//! [`Timestamp`] carries a UTC instant with nanosecond precision, and
//! [`TimestampFormat`] selects one of the three representations; the variant
//! names are the format names. The accepted input and the emitted bytes of each
//! format are the ones recorded by the wire-format vectors of the model data and
//! by the contract fixture of the migration. [`ParseTimestampError`] and
//! [`FormatTimestampError`] classify the failures.
//!
//! The date-time and the HTTP date carry a four-digit year, so writing an
//! instant outside `0..=9999` fails instead of writing an expanded year. The
//! representable instant range is narrower than that year range: instants before
//! `-9999-01-02T01:59:59Z` and after `9999-12-30T22:00:00.999999999Z` are
//! rejected.
//!
//! [`ParseTimestampError::Overflow`] is defensive: an epoch-seconds seconds field
//! is a 64-bit integer and a fraction is less than one second, so combining them
//! cannot overflow the intermediate representation. The variant stays for that
//! defence, not because an input reaches it.
//!
//! # Features
//!
//! `serde` — off by default. When it is enabled, [`Timestamp`] implements
//! `serde::Serialize` and `serde::Deserialize`: serialization writes the
//! date-time form, and deserialization parses it. The documentation build
//! enables every feature.
//!
//! # Example
//!
//! ```
//! use s3s_time::{Timestamp, TimestampFormat};
//!
//! let ts = Timestamp::parse(TimestampFormat::DateTime, "2024-06-15T07:00:00.123Z").unwrap();
//!
//! let mut buf = Vec::new();
//! ts.format(TimestampFormat::HttpDate, &mut buf).unwrap();
//! assert_eq!(String::from_utf8(buf).unwrap(), "Sat, 15 Jun 2024 07:00:00 GMT");
//! ```

#![deny(missing_docs)]

mod error;
mod format;
mod parse;
mod timestamp;

pub use self::error::ConvertTimestampError;
pub use self::error::FormatTimestampError;
pub use self::error::ParseTimestampError;
pub use self::timestamp::Timestamp;
pub use self::timestamp::TimestampFormat;

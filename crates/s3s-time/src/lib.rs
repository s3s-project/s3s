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
//! # Stability
//!
//! The stable surface is the wire format of each representation and the
//! classification of the errors:
//!
//! - [`Timestamp`] and [`TimestampFormat`], including the variant names,
//! - the accepted input and the emitted bytes of the three formats,
//! - the variant names of [`ParseTimestampError`] and [`FormatTimestampError`].
//!
//! Everything else may change in a minor release: the internal representation,
//! the error messages and the helper methods.
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

pub use self::error::ComponentRangeError;
pub use self::error::FormatTimestampError;
pub use self::error::ParseTimestampError;
pub use self::timestamp::Timestamp;
pub use self::timestamp::TimestampFormat;

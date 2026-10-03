// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The error types of the wire formats.

use std::io;

/// The error returned when a value is outside the range of a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("timestamp component is out of range")]
pub struct ComponentRangeError;

/// The error returned when a timestamp cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseTimestampError {
    /// The input does not match the grammar of the requested format.
    #[error("invalid timestamp format")]
    InvalidFormat,
    /// The input denotes a field or an instant outside the representable range.
    #[error("timestamp is out of range")]
    OutOfRange,
    /// An epoch-seconds fraction carries more than nine digits.
    #[error("fractional second has more than nine digits")]
    FractionTooLong,
    /// Combining the seconds and the fraction overflowed the representable range.
    #[error("timestamp overflow")]
    Overflow,
}

/// The error returned when a timestamp cannot be written.
#[derive(Debug, thiserror::Error)]
pub enum FormatTimestampError {
    /// The timestamp cannot be represented in the requested format.
    #[error("timestamp is out of range")]
    OutOfRange,
    /// The writer returned an error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::fmt;

use crate::Error;

/// A multipart boundary.
///
/// Construction validates the boundary according to RFC 2046 section
/// 5.1.1: 1 to 70 characters, only `bchars`, and the final character must
/// not be whitespace.
///
/// Validation is mandatory: a boundary that some other parsers accept but
/// RFC 2046 forbids (for example a value longer than 70 characters, or one
/// containing `*`) is always rejected, and there is no unchecked constructor.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Boundary(Box<[u8]>);

impl Boundary {
    /// Constructs and validates a boundary.
    ///
    /// The input is the raw `boundary` parameter value from a
    /// `Content-Type` header, without the leading `--`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidBoundary`] when the input is not a valid
    /// boundary.
    pub fn new(boundary: &[u8]) -> Result<Self, Error> {
        if boundary.is_empty() || boundary.len() > 70 {
            return Err(Error::InvalidBoundary);
        }

        if !boundary.iter().copied().all(is_bchar) {
            return Err(Error::InvalidBoundary);
        }

        if boundary.last().is_some_and(u8::is_ascii_whitespace) {
            return Err(Error::InvalidBoundary);
        }

        Ok(Self(boundary.into()))
    }

    /// Returns the raw boundary bytes, without the leading `--`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the length of the boundary in bytes, without the leading `--`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the boundary is empty.
    ///
    /// This is always `false`: [`Boundary::new`] rejects empty input.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Boundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for Boundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Boundary").field(&String::from_utf8_lossy(&self.0)).finish()
    }
}

/// Returns true when `byte` is a valid `bcharsnospace` or space character.
fn is_bchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'\'' | b'(' | b')' | b'+' | b'_' | b',' | b'-' | b'.' | b'/' | b':' | b'=' | b'?' | b' '
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn boundary_accepts_valid_forms() {
        for boundary in [
            &b"a"[..],
            b"----WebKitFormBoundary7MA4YWxkTrZu0gW",
            b"simple boundary",
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ'()+_,-.",
        ] {
            let parsed = Boundary::new(boundary).unwrap();
            assert_eq!(parsed.as_bytes(), boundary);
            assert_eq!(parsed.len(), boundary.len());
            assert!(!parsed.is_empty());
            assert!(!parsed.to_string().is_empty());
            assert!(!format!("{parsed:?}").is_empty());
        }
    }

    #[test]
    fn boundary_rejects_invalid_forms() {
        for boundary in [
            &b""[..],
            b" ",
            b"trailing ",
            &[b'a'; 71],
            b"bad\0byte",
            b"bad\nbyte",
            b"bad\tbyte",
            b"\xe4\xb8\xad",
            // Printable characters outside RFC 2046 `bchars`: accepted by the
            // legacy parser (via the `mime` crate), rejected here.
            b"a*b",
            b"a;b",
            b"a!b",
        ] {
            assert!(matches!(Boundary::new(boundary), Err(Error::InvalidBoundary)), "{boundary:?}");
        }
    }

    #[test]
    fn boundary_display_is_lossy_but_readable() {
        let boundary = Boundary::new(b"----boundary").unwrap();
        assert_eq!(boundary.to_string(), "----boundary");
        let debug = format!("{boundary:?}");
        assert!(debug.contains("----boundary"));
    }
}

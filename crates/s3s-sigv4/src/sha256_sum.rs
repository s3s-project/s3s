// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use hex_simd::{AsciiCase, Out};
use subtle::ConstantTimeEq;

use crate::crypto::is_sha256_checksum;

/// A normalized SHA-256 digest.
///
/// [`PartialEq`] compares digests in constant time.
#[derive(Debug, Clone, Copy)]
pub struct Sha256Sum([u8; 32]);

impl PartialEq for Sha256Sum {
    fn eq(&self, other: &Self) -> bool {
        self.ct_equal(other)
    }
}

impl Eq for Sha256Sum {}

impl Sha256Sum {
    /// Creates a digest from its raw bytes.
    #[must_use]
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Parses a canonical lowercase hexadecimal SHA-256 digest.
    ///
    /// Returns `None` unless `value` is exactly 64 lowercase hexadecimal characters.
    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        if !is_sha256_checksum(value) {
            return None;
        }

        let mut digest = [0_u8; 32];
        let decoded = hex_simd::decode(value.as_bytes(), Out::from_slice(&mut digest)).ok()?;
        (decoded.len() == digest.len()).then_some(Self(digest))
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns whether the digest equals `other` in constant time.
    ///
    /// [`PartialEq`] is implemented in constant time through this method.
    #[must_use]
    pub fn ct_equal(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }

    /// Writes the lowercase hexadecimal form into `buf` and returns it as a string slice.
    #[must_use]
    pub fn to_hex<'a>(&self, buf: &'a mut [u8; 64]) -> &'a str {
        &*hex_simd::encode_as_str(self.0.as_ref(), Out::from_slice(buf), AsciiCase::Lower)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    const HEX: &str = "083fe500b5dc034edaba07dd39da7bd80c0883ce5af73583279ce85eb66e6fcd";

    const BYTES: [u8; 32] = [
        0x08, 0x3f, 0xe5, 0x00, 0xb5, 0xdc, 0x03, 0x4e, 0xda, 0xba, 0x07, 0xdd, 0x39, 0xda, 0x7b, 0xd8, 0x0c, 0x08, 0x83, 0xce,
        0x5a, 0xf7, 0x35, 0x83, 0x27, 0x9c, 0xe8, 0x5e, 0xb6, 0x6e, 0x6f, 0xcd,
    ];

    #[test]
    fn sha256_sum_parses_lowercase_hex() {
        let sum = Sha256Sum::from_hex(HEX).expect("valid lowercase SHA-256 hex");

        assert_eq!(sum, Sha256Sum::from_bytes(BYTES));
        assert_eq!(sum.as_bytes(), &BYTES);
    }

    #[test]
    fn sha256_sum_rejects_noncanonical_or_wrong_length_encodings() {
        assert!(Sha256Sum::from_hex(&HEX.to_uppercase()).is_none());
        assert!(Sha256Sum::from_hex("00").is_none());
        assert!(Sha256Sum::from_hex("").is_none());
        assert!(Sha256Sum::from_hex("not-a-sha256").is_none());
    }

    #[test]
    fn sha256_sum_round_trips_through_hex() {
        let sum = Sha256Sum::from_bytes(BYTES);
        let mut buf = [0_u8; 64];

        let hex = sum.to_hex(&mut buf);

        assert_eq!(hex, HEX);
        assert_eq!(Sha256Sum::from_hex(hex), Some(sum));
    }

    #[test]
    fn sha256_sum_compares_in_constant_time() {
        let sum = Sha256Sum::from_hex(HEX).expect("valid lowercase SHA-256 hex");
        let same = Sha256Sum::from_bytes(BYTES);
        let other = Sha256Sum::from_bytes([0; 32]);

        assert!(sum.ct_equal(&same));
        assert_eq!(sum, same);
        assert!(!sum.ct_equal(&other));
        assert_ne!(sum, other);
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Request signatures used for AWS Signature verification.
//!
//! # Invariants
//!
//! - Values are validated at construction and always hold a canonical encoded
//!   form (`SigV4`: 64 lowercase hex chars; `SigV2`: 28-char standard Base64 with
//!   padding). Comparisons within the same form are always equal-length and
//!   constant-time over the content; cross-form comparison is always unequal
//!   (the length difference is algorithm-public, not secret).
//! - `PartialEq` / `Eq` / `Hash` are deliberately NOT implemented: an ordinary
//!   `==` comparison is a compile error; comparison must go through
//!   [`Signature::compare`] or [`ConstantTimeEq`] to avoid timing side channels.
//! - [`Signature::as_str`] is for string-to-sign chaining and logging, NOT for
//!   comparison.
//! - Do NOT add unchecked constructors (e.g. `From<&str>`); doing so would
//!   break both invariants above.

use crate::utils::crypto::is_sha256_checksum;
use subtle::ConstantTimeEq;

/// An AWS request signature in its canonical encoded form.
///
/// Not a secret: the value is transmitted in cleartext with the request itself.
/// `Debug` therefore prints the value (unlike [`SecretKey`](crate::auth::SecretKey)).
#[derive(Debug)]
pub struct Signature(Box<str>);

impl Signature {
    /// Parses a `SigV4` signature (64 lowercase hexadecimal characters).
    ///
    /// Returns `None` if `value` is not a canonical `SigV4` signature.
    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        is_sha256_checksum(value).then(|| Self(value.into()))
    }

    /// Parses a `SigV2` signature (standard Base64 with padding; HMAC-SHA1, 20 bytes).
    ///
    /// Returns `None` if `value` is not a canonical `SigV2` signature.
    #[must_use]
    pub fn from_base64(value: &str) -> Option<Self> {
        // SigV2 signatures are always 28 chars (20-byte HMAC-SHA1 with padding);
        // reject other lengths before any validation work.
        if value.len() != 28 {
            return None;
        }

        // `check` validates the alphabet, the padding structure, and the
        // canonical padding bits (`STANDARD` is not forgiving). The
        // decoded-length check pins the payload to exactly 20 bytes.
        base64_simd::STANDARD.check(value.as_bytes()).ok()?;
        (base64_simd::STANDARD.decoded_length(value.as_bytes()).ok()? == 20).then(|| Self(value.into()))
    }

    /// Wraps a signature produced by `sig_v4::calculate_signature` /
    /// `sig_v2::calculate_signature`, whose output is canonical by construction.
    ///
    /// Called only inside the two `calculate_signature` functions.
    /// Do not use for data received from the wire; use [`Self::from_hex`] /
    /// [`Self::from_base64`] instead.
    #[must_use]
    pub(crate) fn from_computed(value: String) -> Self {
        debug_assert!(
            Self::from_hex(&value).is_some() || Self::from_base64(&value).is_some(),
            "internal error: computed signature is not canonical"
        );
        Self(value.into_boxed_str())
    }

    /// Returns the canonical encoded form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares two signatures in constant time.
    ///
    /// Returns `true` if `left` and `right` hold the same canonical value.
    /// Cross-form comparisons (`SigV4` vs `SigV2`) are always `false`.
    ///
    /// This is the canonical way to compare signatures; see the module-level
    /// invariants for why `==` / `!=` must not be used.
    #[must_use]
    pub fn compare(left: &Self, right: &Self) -> bool {
        bool::from(left.ct_eq(right))
    }
}

impl ConstantTimeEq for Signature {
    fn ct_eq(&self, other: &Self) -> subtle::Choice {
        self.0.as_bytes().ct_eq(other.0.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX_64: &str = "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404";
    const B64_20: &str = "1No4mq5ETf02z8aet9voy6gui6E=";
    const B64_20_PLUS: &str = "++++AAAAAAAAAAAAAAAAAAAAAAA=";

    #[test]
    fn from_hex_accepts_canonical() {
        let sig = Signature::from_hex(HEX_64).expect("canonical hex should parse");
        assert_eq!(sig.as_str(), HEX_64);
    }

    #[test]
    fn from_hex_rejects_uppercase() {
        assert!(Signature::from_hex(&HEX_64.to_uppercase()).is_none());
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        assert!(Signature::from_hex(&HEX_64[..63]).is_none());
    }

    #[test]
    fn from_hex_rejects_non_hex_chars() {
        assert!(Signature::from_hex(&format!("z{}", &HEX_64[1..])).is_none());
    }

    #[test]
    fn from_base64_accepts_canonical() {
        let sig = Signature::from_base64(B64_20).expect("canonical base64 should parse");
        assert_eq!(sig.as_str(), B64_20);
    }

    #[test]
    fn from_base64_accepts_plus_and_slash() {
        let sig = Signature::from_base64(B64_20_PLUS).expect("base64 with '+' should parse");
        assert_eq!(sig.as_str(), B64_20_PLUS);
    }

    #[test]
    fn from_base64_rejects_non_base64_alphabet() {
        // `decoded_length` alone accepts this (structure is valid); the full
        // decode must reject it.
        assert!(Signature::from_base64("!!!!!!!!!!!!!!!!!!!!!!!=").is_none());
    }

    #[test]
    fn from_base64_rejects_missing_padding() {
        assert!(Signature::from_base64(&B64_20[..27]).is_none());
    }

    #[test]
    fn from_base64_rejects_non_20_byte_payload() {
        // 24 chars decode to 18 bytes, not 20.
        assert!(Signature::from_base64("AAAAAAAAAAAAAAAAAAAAAAAA").is_none());
        // 32 chars would decode to 24 bytes, not 20.
        assert!(Signature::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_none());
        // 28 chars with double padding decode to 19 bytes, not 20:
        // `check` alone would accept this, the decoded-length check rejects it.
        assert!(Signature::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAA==").is_none());
    }

    #[test]
    fn from_computed_round_trip() {
        let sig = Signature::from_computed(HEX_64.to_owned());
        assert_eq!(sig.as_str(), HEX_64);
    }

    #[test]
    fn ct_eq_matches_equal_values() {
        let a = Signature::from_hex(HEX_64).unwrap();
        let b = Signature::from_hex(HEX_64).unwrap();
        assert!(bool::from(a.ct_eq(&b)));
    }

    #[test]
    fn ct_eq_reports_mismatch() {
        let a = Signature::from_hex(HEX_64).unwrap();
        let b = Signature::from_hex(&format!("0{}", &HEX_64[1..])).unwrap();
        assert!(!bool::from(a.ct_eq(&b)));
    }

    #[test]
    fn ct_eq_cross_encoding_is_always_unequal() {
        let hex_sig = Signature::from_hex(HEX_64).unwrap();
        let b64_sig = Signature::from_base64(B64_20).unwrap();
        assert!(!bool::from(hex_sig.ct_eq(&b64_sig)));
    }

    #[test]
    fn compare_reports_match_and_mismatch() {
        let a = Signature::from_hex(HEX_64).unwrap();
        let same = Signature::from_hex(HEX_64).unwrap();
        let different = Signature::from_hex(&format!("0{}", &HEX_64[1..])).unwrap();
        let b64 = Signature::from_base64(B64_20).unwrap();

        assert!(Signature::compare(&a, &same));
        assert!(!Signature::compare(&a, &different));
        assert!(!Signature::compare(&a, &b64));
    }

    #[test]
    fn debug_prints_plaintext_value() {
        let sig = Signature::from_hex(HEX_64).unwrap();
        assert!(format!("{sig:?}").contains(HEX_64));
    }
}

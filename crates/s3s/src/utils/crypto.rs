// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::mem::MaybeUninit;

use hex_simd::{AsOut, AsciiCase};

/// A normalized SHA-256 digest.
///
/// [`PartialEq`] compares in constant time.
#[derive(Debug, Clone, Copy)]
pub struct Sha256Sum([u8; 32]);

impl PartialEq for Sha256Sum {
    fn eq(&self, other: &Self) -> bool {
        self.ct_equal(other)
    }
}

impl Eq for Sha256Sum {}

impl Sha256Sum {
    /// Parses a lowercase hexadecimal SHA-256 digest.
    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        if !is_sha256_checksum(value) {
            return None;
        }

        let mut digest = [0_u8; 32];
        let decoded = hex_simd::decode(value.as_bytes(), hex_simd::Out::from_slice(&mut digest)).ok()?;
        (decoded.len() == digest.len()).then_some(Self(digest))
    }

    /// Creates a digest from its raw bytes.
    #[must_use]
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns whether the digest equals `other` in constant time.
    ///
    /// [`PartialEq`] is implemented in constant time through this method.
    #[must_use]
    pub fn ct_equal(&self, other: &Sha256Sum) -> bool {
        use subtle::ConstantTimeEq;
        bool::from(self.0.ct_eq(&other.0))
    }
}

/// verify sha256 checksum string
pub fn is_sha256_checksum(s: &str) -> bool {
    // TODO: optimize
    let is_lowercase_hex = |c: u8| matches!(c, b'0'..=b'9' | b'a'..=b'f');
    s.len() == 64 && s.as_bytes().iter().copied().all(is_lowercase_hex)
}

/// `f(hex(src))`
pub(crate) fn hex_bytes32<R>(src: &[u8; 32], f: impl FnOnce(&str) -> R) -> R {
    let buf: &mut [_] = &mut [MaybeUninit::uninit(); 64];
    let ans = hex_simd::encode_as_str(src.as_ref(), buf.as_out(), AsciiCase::Lower);
    f(ans)
}

#[cfg(not(all(feature = "openssl", not(windows))))]
fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    <Sha256 as Digest>::digest(data).into()
}

#[cfg(all(feature = "openssl", not(windows)))]
fn sha256(data: &[u8]) -> [u8; 32] {
    use openssl::hash::{Hasher, MessageDigest};
    let mut h = Hasher::new(MessageDigest::sha256()).unwrap();
    h.update(data).unwrap();
    let digest = h.finish().unwrap();
    let mut ans = [0_u8; 32];
    ans.copy_from_slice(&digest);
    ans
}

/// `f(hex(sha256(data)))`
pub fn hex_sha256<R>(data: &[u8], f: impl FnOnce(&str) -> R) -> R {
    hex_bytes32(&sha256(data), f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_sum_normalizes_hex() {
        let hex = "083fe500b5dc034edaba07dd39da7bd80c0883ce5af73583279ce85eb66e6fcd";

        let from_hex = Sha256Sum::from_hex(hex).expect("valid lowercase SHA-256 hex");

        assert_eq!(
            from_hex,
            Sha256Sum::from_bytes([
                0x08, 0x3f, 0xe5, 0x00, 0xb5, 0xdc, 0x03, 0x4e, 0xda, 0xba, 0x07, 0xdd, 0x39, 0xda, 0x7b, 0xd8, 0x0c, 0x08, 0x83,
                0xce, 0x5a, 0xf7, 0x35, 0x83, 0x27, 0x9c, 0xe8, 0x5e, 0xb6, 0x6e, 0x6f, 0xcd
            ])
        );
        assert_eq!(hex_bytes32(from_hex.as_bytes(), str::to_owned), hex);
    }

    #[test]
    fn sha256_sum_rejects_noncanonical_or_wrong_length_encodings() {
        assert!(Sha256Sum::from_hex("083FE500B5DC034EDABA07DD39DA7BD80C0883CE5AF73583279CE85EB66E6FCD").is_none());
        assert!(Sha256Sum::from_hex("00").is_none());
    }
}

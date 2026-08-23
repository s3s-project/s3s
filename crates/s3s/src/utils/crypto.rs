// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::mem::MaybeUninit;

use hex_simd::{AsOut, AsciiCase};
use hyper::body::Bytes;

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
    pub fn from_hex(value: &str) -> Option<Self> {
        if !is_sha256_checksum(value) {
            return None;
        }

        let mut digest = [0_u8; 32];
        let decoded = hex_simd::decode(value.as_bytes(), hex_simd::Out::from_slice(&mut digest)).ok()?;
        (decoded.len() == digest.len()).then_some(Self(digest))
    }

    /// Parses a standard Base64-encoded SHA-256 digest.
    pub fn from_base64(value: &str) -> Option<Self> {
        if base64_simd::STANDARD.decoded_length(value.as_bytes()).ok()? != 32 {
            return None;
        }

        let mut digest = [0_u8; 32];
        let decoded = base64_simd::STANDARD
            .decode(value.as_bytes(), base64_simd::Out::from_slice(&mut digest))
            .ok()?;
        (decoded.len() == digest.len()).then_some(Self(digest))
    }

    /// Creates a digest from its raw bytes.
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Encodes the digest as lowercase hexadecimal.
    pub fn to_hex_string(self) -> String {
        hex(self.0)
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

/// `hmac_sha1(key, data)`
pub fn hmac_sha1(key: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> [u8; 20] {
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;

    let mut m = <Hmac<Sha1>>::new_from_slice(key.as_ref()).unwrap();
    m.update(data.as_ref());
    m.finalize().into_bytes().into()
}

/// `hmac_sha256(key, data)`
pub fn hmac_sha256(key: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let mut m = <Hmac<Sha256>>::new_from_slice(key.as_ref()).unwrap();
    m.update(data.as_ref());
    m.finalize().into_bytes().into()
}

pub fn hex(data: impl AsRef<[u8]>) -> String {
    hex_simd::encode_to_string(data, hex_simd::AsciiCase::Lower)
}

/// `f(hex(src))`
pub(crate) fn hex_bytes32<R>(src: impl AsRef<[u8]>, f: impl FnOnce(&str) -> R) -> R {
    let buf: &mut [_] = &mut [MaybeUninit::uninit(); 64];
    let ans = hex_simd::encode_as_str(src.as_ref(), buf.as_out(), AsciiCase::Lower);
    f(ans)
}

#[cfg(not(all(feature = "openssl", not(windows))))]
fn sha256(data: &[u8]) -> impl AsRef<[u8; 32]> + use<> {
    use sha2::{Digest, Sha256};
    <Sha256 as Digest>::digest(data)
}

#[cfg(all(feature = "openssl", not(windows)))]
fn sha256(data: &[u8]) -> impl AsRef<[u8]> {
    use openssl::hash::{Hasher, MessageDigest};
    let mut h = Hasher::new(MessageDigest::sha256()).unwrap();
    h.update(data).unwrap();
    h.finish().unwrap()
}

#[cfg(not(all(feature = "openssl", not(windows))))]
fn sha256_chunk(chunk: &[Bytes]) -> impl AsRef<[u8; 32]> + use<> {
    use sha2::{Digest, Sha256};
    let mut h = <Sha256 as Digest>::new();
    for data in chunk {
        h.update(data);
    }
    h.finalize()
}

#[cfg(all(feature = "openssl", not(windows)))]
fn sha256_chunk(chunk: &[Bytes]) -> impl AsRef<[u8]> {
    use openssl::hash::{Hasher, MessageDigest};
    let mut h = Hasher::new(MessageDigest::sha256()).unwrap();
    for data in chunk {
        h.update(data).unwrap();
    }
    h.finish().unwrap()
}

/// `f(hex(sha256(data)))`
pub fn hex_sha256<R>(data: &[u8], f: impl FnOnce(&str) -> R) -> R {
    hex_bytes32(sha256(data).as_ref(), f)
}

/// `f(hex(sha256(chunk)))`
pub fn hex_sha256_chunk<R>(chunk: &[Bytes], f: impl FnOnce(&str) -> R) -> R {
    hex_bytes32(sha256_chunk(chunk).as_ref(), f)
}

#[cfg(test)]
pub fn hex_sha256_string(data: &[u8]) -> String {
    hex_sha256(data, str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_sum_normalizes_hex_and_base64() {
        let hex = "083fe500b5dc034edaba07dd39da7bd80c0883ce5af73583279ce85eb66e6fcd";
        let base64 = "CD/lALXcA07augfdOdp72AwIg85a9zWDJ5zoXrZub80=";

        let from_hex = Sha256Sum::from_hex(hex).expect("valid lowercase SHA-256 hex");
        let from_base64 = Sha256Sum::from_base64(base64).expect("valid standard Base64 SHA-256");

        assert_eq!(from_hex, from_base64);
        assert_eq!(from_base64.to_hex_string(), hex);
    }

    #[test]
    fn sha256_sum_rejects_noncanonical_or_wrong_length_encodings() {
        assert!(Sha256Sum::from_hex("083FE500B5DC034EDABA07DD39DA7BD80C0883CE5AF73583279CE85EB66E6FCD").is_none());
        assert!(Sha256Sum::from_hex("00").is_none());
        assert!(Sha256Sum::from_base64("aGVsbG8=").is_none());
        assert!(Sha256Sum::from_base64("CD_lALXcA07augfdOdp72AwIg85a9zWDJ5zoXrZub80=").is_none());
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use hex_simd::{AsOut, AsciiCase};
#[cfg(not(feature = "fips"))]
use sha2::{Digest, Sha256};

use core::mem::MaybeUninit;

/// Returns whether `s` is a canonical SHA-256 checksum string
/// (64 lowercase hexadecimal characters).
#[must_use]
pub fn is_sha256_checksum(s: &str) -> bool {
    // TODO: optimize
    let is_lowercase_hex = |c: u8| matches!(c, b'0'..=b'9' | b'a'..=b'f');
    s.len() == 64 && s.as_bytes().iter().copied().all(is_lowercase_hex)
}

pub(crate) fn hex_bytes32<R>(src: &[u8; 32], f: impl FnOnce(&str) -> R) -> R {
    let buf: &mut [_] = &mut [MaybeUninit::uninit(); 64];
    let ans = hex_simd::encode_as_str(src.as_ref(), buf.as_out(), AsciiCase::Lower);
    f(ans)
}

/// Incremental SHA-256 for signed request bodies and streaming chunks.
pub struct PayloadSha256 {
    #[cfg(feature = "fips")]
    inner: aws_lc_rs::digest::Context,
    #[cfg(not(feature = "fips"))]
    inner: Sha256,
}

impl Default for PayloadSha256 {
    fn default() -> Self {
        Self {
            #[cfg(feature = "fips")]
            inner: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
            #[cfg(not(feature = "fips"))]
            inner: Sha256::new(),
        }
    }
}

impl PayloadSha256 {
    /// Adds a fragment of the signed payload.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Returns the digest of the concatenated fragments.
    #[must_use]
    pub fn finalize(self) -> [u8; 32] {
        #[cfg(feature = "fips")]
        {
            bytes32(self.inner.finish().as_ref())
        }
        #[cfg(not(feature = "fips"))]
        {
            self.inner.finalize().into()
        }
    }
}

#[cfg(feature = "fips")]
fn bytes32(bytes: &[u8]) -> [u8; 32] {
    let mut output = [0; 32];
    output.copy_from_slice(bytes);
    output
}

#[cfg(feature = "fips")]
fn sha256(data: &[u8]) -> [u8; 32] {
    use aws_lc_rs::digest::{SHA256, digest};
    bytes32(digest(&SHA256, data).as_ref())
}

#[cfg(all(not(feature = "fips"), not(all(feature = "openssl", not(windows)))))]
fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[cfg(all(not(feature = "fips"), feature = "openssl", not(windows)))]
fn sha256(data: &[u8]) -> [u8; 32] {
    sha256_chunk(&[data])
}

#[cfg(any(feature = "fips", not(all(feature = "openssl", not(windows)))))]
fn sha256_chunk(chunk: &[impl AsRef<[u8]>]) -> [u8; 32] {
    let mut hasher = PayloadSha256::default();
    for data in chunk {
        hasher.update(data.as_ref());
    }
    hasher.finalize()
}

#[cfg(all(not(feature = "fips"), feature = "openssl", not(windows)))]
///
/// # Panics
///
/// `openssl` hash operations fail only on invalid internal state; a digest
/// over in-memory data cannot trigger it. The lint is allowed here and the
/// invariant is documented.
#[allow(clippy::unwrap_used)]
fn sha256_chunk(chunk: &[impl AsRef<[u8]>]) -> [u8; 32] {
    use openssl::hash::{Hasher, MessageDigest};
    let mut h = Hasher::new(MessageDigest::sha256()).unwrap();
    for data in chunk {
        h.update(data.as_ref()).unwrap();
    }
    let digest = h.finish().unwrap();
    let mut ans = [0_u8; 32];
    ans.copy_from_slice(&digest);
    ans
}

/// Passes the lowercase hexadecimal SHA-256 digest of `data` to `f`.
pub fn hex_sha256<R>(data: &[u8], f: impl FnOnce(&str) -> R) -> R {
    hex_bytes32(&sha256(data), f)
}

/// `f(hex(sha256(chunk)))`
pub(crate) fn hex_sha256_chunk<R>(chunk: &[impl AsRef<[u8]>], f: impl FnOnce(&str) -> R) -> R {
    hex_bytes32(&sha256_chunk(chunk), f)
}

#[cfg(feature = "fips")]
pub(crate) fn hmac_sha256(key: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> [u8; 32] {
    use aws_lc_rs::hmac::{HMAC_SHA256, Key, sign};
    bytes32(sign(&Key::new(HMAC_SHA256, key.as_ref()), data.as_ref()).as_ref())
}

#[cfg(not(feature = "fips"))]
/// `hmac_sha256(key, data)`
///
/// # Panics
///
/// `HMAC-SHA256` accepts keys of any length, so `new_from_slice` never
/// fails. This `expect` is a structural invariant of the `hmac` crate API;
/// no request input can influence it.
#[allow(clippy::expect_used)]
pub(crate) fn hmac_sha256(key: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    let mut m = <Hmac<Sha256>>::new_from_slice(key.as_ref()).expect("Hmac accepts keys of any length");
    m.update(data.as_ref());
    m.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lowercase_hex() {
        assert!(is_sha256_checksum("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
    }

    #[test]
    fn rejects_non_canonical_input() {
        assert!(!is_sha256_checksum("E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"));
        assert!(!is_sha256_checksum("00"));
        assert!(!is_sha256_checksum("not-a-sha256"));
    }

    #[test]
    fn sha256_helpers_produce_canonical_hashes() {
        assert_eq!(
            hex_sha256(b"", str::to_owned),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_sha256_chunk(&[b"Welcome ".as_slice(), b"to Amazon S3."], str::to_owned),
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        assert_eq!(
            hex_bytes32(&hmac_sha256(b"key", b""), str::to_owned),
            "5d5d139563c95b5967b9bd9a8c9b233a9dedb45072794cd232dc1b74832607d0"
        );
    }

    #[cfg(feature = "fips")]
    #[test]
    fn aws_lc_reports_fips_mode() {
        assert_eq!(aws_lc_rs::try_fips_mode(), Ok(()));
    }
}

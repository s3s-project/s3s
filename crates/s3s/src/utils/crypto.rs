// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::mem::MaybeUninit;

use hex_simd::{AsOut, AsciiCase};

pub use s3s_sigv4::Sha256Sum;

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

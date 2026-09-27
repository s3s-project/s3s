// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Small helpers shared by the decoders.

// The digest type lives in s3s-sigv4 so the signing crate and the decoders share
// one implementation; re-exported here to keep a single import path in-crate.
pub use s3s_sigv4::Sha256Sum;

/// Boxed error type produced by the underlying byte stream.
pub type StdError = Box<dyn std::error::Error + Send + Sync>;

/// Trims ASCII spaces and tabs from both ends.
pub fn trim_ascii_whitespace(mut s: &[u8]) -> &[u8] {
    while matches!(s.first(), Some(b' ' | b'\t')) {
        s = &s[1..];
    }
    while matches!(s.last(), Some(b' ' | b'\t')) {
        s = &s[..s.len().saturating_sub(1)];
    }
    s
}

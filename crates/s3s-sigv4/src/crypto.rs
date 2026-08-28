// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// Returns whether `s` is a canonical SHA-256 checksum string
/// (64 lowercase hexadecimal characters).
#[must_use]
pub fn is_sha256_checksum(s: &str) -> bool {
    // TODO: optimize
    let is_lowercase_hex = |c: u8| matches!(c, b'0'..=b'9' | b'a'..=b'f');
    s.len() == 64 && s.as_bytes().iter().copied().all(is_lowercase_hex)
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
}

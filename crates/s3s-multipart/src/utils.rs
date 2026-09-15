// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Small byte helpers shared by the parsers.

/// Whether `byte` is optional whitespace (`SP` / `HTAB`, RFC 9110 OWS).
///
/// `u8::is_ascii_whitespace` is deliberately not used: it also accepts `LF`,
/// `CR`, `VT` and `FF`, which are not optional whitespace.
const fn is_ows(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

/// Trims optional whitespace (`SP` / `HTAB`) from both ends of `value`.
///
/// Shared by the header block parser and the `Content-Disposition` parser,
/// which used to carry byte-identical private copies.
pub fn trim_ows(mut value: &[u8]) -> &[u8] {
    while let Some((first, rest)) = value.split_first() {
        if !is_ows(*first) {
            break;
        }
        value = rest;
    }
    while let Some((last, rest)) = value.split_last() {
        if !is_ows(*last) {
            break;
        }
        value = rest;
    }
    value
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Mixed alphabet: both optional-whitespace bytes, two bytes that look like
    /// whitespace but are not, and an ordinary byte.
    const ALPHABET: [u8; 5] = *b" \t\r\na";

    /// Reference implementation: the first and the last byte that are not
    /// optional whitespace delimit the trimmed slice.
    fn oracle(value: &[u8]) -> &[u8] {
        let start = value.iter().position(|byte| !is_ows(*byte)).unwrap_or(value.len());
        let end = value.iter().rposition(|byte| !is_ows(*byte)).map_or(start, |idx| idx + 1);
        &value[start..end]
    }

    fn assert_matches_oracle(candidate: &mut Vec<u8>, remaining: usize) {
        assert_eq!(trim_ows(candidate), oracle(candidate), "input={candidate:?}");
        if remaining == 0 {
            return;
        }
        for byte in ALPHABET {
            candidate.push(byte);
            assert_matches_oracle(candidate, remaining - 1);
            let _ = candidate.pop();
        }
    }

    #[test]
    fn only_sp_and_htab_count_as_optional_whitespace() {
        for byte in 0..=u8::MAX {
            assert_eq!(is_ows(byte), matches!(byte, b' ' | b'\t'), "byte={byte:#04x}");
        }
    }

    #[test]
    fn trims_optional_whitespace_on_both_ends() {
        assert_eq!(trim_ows(b" \tvalue \t"), b"value");
        assert_eq!(trim_ows(b"value"), b"value");
        assert_eq!(trim_ows(b"\t value"), b"value");
        assert_eq!(trim_ows(b"   "), b"");
        assert_eq!(trim_ows(b""), b"");
        // `LF`, `CR` and the remaining ASCII whitespace are not OWS.
        assert_eq!(trim_ows(b"\r\nvalue\r\n"), b"\r\nvalue\r\n");
        assert_eq!(trim_ows(b"\x0bvalue\x0c"), b"\x0bvalue\x0c");
    }

    /// Every byte string up to length four over the mixed alphabet (781
    /// inputs) must be trimmed exactly like the reference implementation.
    #[test]
    fn matches_the_oracle_on_every_short_input() {
        let mut candidate = Vec::new();
        assert_matches_oracle(&mut candidate, 4);
    }
}

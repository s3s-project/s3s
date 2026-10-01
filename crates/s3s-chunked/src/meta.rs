// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Chunk metadata parsing.

/// Parsed chunk metadata.
#[derive(Debug)]
pub struct ChunkMeta {
    /// Declared chunk size in bytes.
    pub size: usize,
    /// Chunk signature, when the chunk carries one.
    pub signature: Option<[u8; 64]>,
}

/// Parses a complete chunk metadata line, including the trailing newline.
///
/// The accepted forms are a bare hex size followed by CRLF, or a hex size
/// followed by a chunk-signature extension and CRLF.
pub fn parse_chunk_meta(line: &[u8]) -> Option<ChunkMeta> {
    let line = line.strip_suffix(b"\n")?;
    let index = line.iter().position(|&b| b == b';' || b == b'\r')?;
    let size = parse_hex_u32(&line[..index])?;
    let size = usize::try_from(size).ok()?;
    let rest = &line[index..];

    if let Some(signature) = rest.strip_prefix(b";chunk-signature=") {
        let (signature, rest) = signature.split_at_checked(64)?;
        if rest != b"\r" {
            return None;
        }
        let signature: [u8; 64] = signature.try_into().ok()?;
        return Some(ChunkMeta {
            size,
            signature: Some(signature),
        });
    }

    if rest == b"\r" {
        return Some(ChunkMeta { size, signature: None });
    }

    None
}

/// Parses a hexadecimal chunk size with the historical 32-bit bound.
fn parse_hex_u32(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }

    let mut value = 0_u32;
    for &digit in digits {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(u32::from(nibble))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_signed_lines() {
        let meta = parse_chunk_meta(b"5a\r\n").expect("bare size");
        assert_eq!(meta.size, 90);
        assert!(meta.signature.is_none());

        let signature = [b'a'; 64];
        let mut line = b"10;chunk-signature=".to_vec();
        line.extend_from_slice(&signature);
        line.extend_from_slice(b"\r\n");
        let meta = parse_chunk_meta(&line).expect("signed size");
        assert_eq!(meta.size, 16);
        assert_eq!(meta.signature, Some(signature));
    }

    #[test]
    fn parses_uppercase_hex_sizes() {
        assert_eq!(parse_chunk_meta(b"A\r\n").expect("uppercase digit").size, 10);
        assert_eq!(parse_chunk_meta(b"1F\r\n").expect("mixed case digits").size, 31);
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse_chunk_meta(b"").is_none());
        assert!(parse_chunk_meta(b"\r\n").is_none());
        assert!(parse_chunk_meta(b"5\n").is_none());
        assert!(parse_chunk_meta(b"zz\r\n").is_none());
        assert!(parse_chunk_meta(b"100000000\r\n").is_none());
        assert!(parse_chunk_meta(b"5;chunk-signature=abc\r\n").is_none());
        assert!(parse_chunk_meta(b"5 \r\n").is_none());
        assert!(parse_chunk_meta(b"5\r\n\r\n").is_none());
    }
}

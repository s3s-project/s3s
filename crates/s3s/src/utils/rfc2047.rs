//! RFC 2047 MIME encoded-word support for non-ASCII header values.
//!
//! See <https://datatracker.ietf.org/doc/html/rfc2047> for the specification.

/// Checks if a string contains only ASCII characters that are valid in HTTP header values.
fn is_ascii_header_safe(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii() && b >= 0x20 && b != 0x7f)
}

/// Encodes a string using RFC 2047 Base64 encoding if it contains non-ASCII characters.
/// Returns the original string if it only contains ASCII characters.
pub fn encode(s: &str) -> String {
    if is_ascii_header_safe(s) {
        return s.to_owned();
    }
    // Use UTF-8 charset with Base64 encoding
    let encoded = base64_simd::STANDARD.encode_to_string(s.as_bytes());
    format!("=?UTF-8?B?{encoded}?=")
}

/// Decodes an RFC 2047 encoded-word string.
/// If the string is not RFC 2047 encoded, returns it unchanged.
/// Supports both Base64 (B) and Quoted-Printable (Q) encodings.
///
/// # Charset Handling
/// This implementation primarily supports UTF-8 charset. For other charsets,
/// it attempts to decode the bytes as UTF-8, which may fail if the original
/// encoding used a different character set. A full implementation would need
/// to support additional charsets like ISO-8859-1, etc.
pub fn decode(s: &str) -> Result<String, DecodeError> {
    // Check if this looks like an RFC 2047 encoded word
    let s = s.trim();
    if !s.starts_with("=?") || !s.ends_with("?=") {
        // Not encoded, return as-is
        return Ok(s.to_owned());
    }

    // Parse the encoded word: =?charset?encoding?encoded_text?=
    let inner = &s[2..s.len() - 2];
    let mut parts = inner.splitn(3, '?');

    let charset = parts.next().ok_or(DecodeError::InvalidFormat)?;
    let encoding = parts.next().ok_or(DecodeError::InvalidFormat)?;
    let encoded_text = parts.next().ok_or(DecodeError::InvalidFormat)?;

    // Decode based on encoding type
    let decoded_bytes = match encoding.to_ascii_uppercase().as_str() {
        "B" => base64_simd::STANDARD
            .decode_to_vec(encoded_text)
            .map_err(|_| DecodeError::Base64Error)?,
        "Q" => decode_quoted_printable(encoded_text)?,
        _ => return Err(DecodeError::UnsupportedEncoding),
    };

    // Convert to string based on charset
    // Note: For non-UTF-8 charsets, we attempt UTF-8 decoding which may fail
    match charset.to_ascii_uppercase().as_str() {
        "UTF-8" | "UTF8" => String::from_utf8(decoded_bytes).map_err(|_| DecodeError::InvalidUtf8),
        _ => String::from_utf8(decoded_bytes).map_err(|_| DecodeError::InvalidUtf8),
    }
}

/// Decodes a Quoted-Printable encoded string according to RFC 2047.
/// According to RFC 2047, only ASCII printable characters should appear
/// directly in Q-encoded text, with non-ASCII bytes encoded as =XX.
fn decode_quoted_printable(s: &str) -> Result<Vec<u8>, DecodeError> {
    let mut result = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '=' => {
                // Hex-encoded byte
                let h1 = chars.next().ok_or(DecodeError::InvalidFormat)?;
                let h2 = chars.next().ok_or(DecodeError::InvalidFormat)?;
                let hex_str: String = [h1, h2].iter().collect();
                let byte = u8::from_str_radix(&hex_str, 16).map_err(|_| DecodeError::InvalidHex)?;
                result.push(byte);
            }
            '_' => {
                // Underscore represents space in RFC 2047 Q encoding
                result.push(b' ');
            }
            c if c.is_ascii() => {
                // Regular ASCII character - safe to cast to u8
                result.push(c as u8);
            }
            _ => {
                // Non-ASCII character in Q-encoded text is invalid
                return Err(DecodeError::InvalidFormat);
            }
        }
    }

    Ok(result)
}

/// Errors that can occur during RFC 2047 decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The encoded word format is invalid.
    InvalidFormat,
    /// Base64 decoding failed.
    Base64Error,
    /// Hex decoding failed in Quoted-Printable.
    InvalidHex,
    /// The decoded bytes are not valid UTF-8.
    InvalidUtf8,
    /// The encoding type is not supported.
    UnsupportedEncoding,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat => write!(f, "invalid RFC 2047 encoded-word format"),
            Self::Base64Error => write!(f, "base64 decoding failed"),
            Self::InvalidHex => write!(f, "invalid hex in quoted-printable encoding"),
            Self::InvalidUtf8 => write!(f, "decoded bytes are not valid UTF-8"),
            Self::UnsupportedEncoding => write!(f, "unsupported encoding type"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_ascii() {
        let input = "hello world";
        let encoded = encode(input);
        assert_eq!(encoded, "hello world");
    }

    #[test]
    fn test_encode_non_ascii() {
        let input = "你好世界";
        let encoded = encode(input);
        assert!(encoded.starts_with("=?UTF-8?B?"));
        assert!(encoded.ends_with("?="));
    }

    #[test]
    fn test_decode_plain() {
        let input = "hello world";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "hello world");
    }

    #[test]
    fn test_decode_base64() {
        // "你好" in UTF-8, then Base64 encoded
        let input = "=?UTF-8?B?5L2g5aW9?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "你好");
    }

    #[test]
    fn test_decode_quoted_printable() {
        // "café" with the é encoded
        let input = "=?UTF-8?Q?caf=C3=A9?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "café");
    }

    #[test]
    fn test_roundtrip() {
        let original = "Hello 世界 🌍";
        let encoded = encode(original);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_decode_underscore_as_space() {
        let input = "=?UTF-8?Q?hello_world?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "hello world");
    }

    #[test]
    fn test_decode_invalid_format() {
        // This string starts with =? and ends with ?= but has invalid Base64 content
        let input = "=?UTF-8?B?!!!?=";
        let result = decode(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_control_characters() {
        // Control characters should be encoded
        let input = "hello\x00world";
        let encoded = encode(input);
        assert!(encoded.starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn test_decode_lowercase_encoding() {
        // Encoding specifier should be case-insensitive
        let input = "=?utf-8?b?5L2g5aW9?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "你好");
    }

    #[test]
    fn test_decode_qp_non_ascii_rejected() {
        // Non-ASCII characters should not appear directly in Q-encoded text
        // They should be encoded as =XX sequences
        let input = "=?UTF-8?Q?café?="; // The 'é' should have been =C3=A9
        let result = decode(input);
        assert!(result.is_err());
    }
}

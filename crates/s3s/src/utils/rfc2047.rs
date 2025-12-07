//! RFC 2047 MIME encoded-word support for non-ASCII header values.
//!
//! See <https://datatracker.ietf.org/doc/html/rfc2047> for the specification.

#![allow(dead_code)] // Functions will be used when integrating with http/de.rs and http/ser.rs

use std::borrow::Cow;

/// Checks if a string contains only ASCII characters that are valid in HTTP header values.
fn is_ascii_header_safe(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii() && b >= 0x20 && b != 0x7f)
}

/// Encodes a string using RFC 2047 Base64 encoding if it contains non-ASCII characters.
/// Returns the original string if it only contains ASCII characters.
pub fn encode(s: &str) -> Cow<'_, str> {
    if is_ascii_header_safe(s) {
        return Cow::Borrowed(s);
    }
    // Use UTF-8 charset with Base64 encoding
    let encoded = base64_simd::STANDARD.encode_to_string(s.as_bytes());
    Cow::Owned(format!("=?UTF-8?B?{encoded}?="))
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
pub fn decode(s: &str) -> Result<Cow<'_, str>, DecodeError> {
    // Check if this looks like an RFC 2047 encoded word
    let s = s.trim();
    if !s.starts_with("=?") || !s.ends_with("?=") {
        // Not encoded, return as-is
        return Ok(Cow::Borrowed(s));
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
        "UTF-8" | "UTF8" => String::from_utf8(decoded_bytes)
            .map(Cow::Owned)
            .map_err(|_| DecodeError::InvalidUtf8),
        _ => String::from_utf8(decoded_bytes)
            .map(Cow::Owned)
            .map_err(|_| DecodeError::InvalidUtf8),
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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The encoded word format is invalid.
    #[error("invalid RFC 2047 encoded-word format")]
    InvalidFormat,
    /// Base64 decoding failed.
    #[error("base64 decoding failed")]
    Base64Error,
    /// Hex decoding failed in Quoted-Printable.
    #[error("invalid hex in quoted-printable encoding")]
    InvalidHex,
    /// The decoded bytes are not valid UTF-8.
    #[error("decoded bytes are not valid UTF-8")]
    InvalidUtf8,
    /// The encoding type is not supported.
    #[error("unsupported encoding type")]
    UnsupportedEncoding,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== Encoding Tests ====================

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
    fn test_encode_control_characters() {
        // Control characters (< 0x20) should be encoded
        let input = "hello\x00world";
        let encoded = encode(input);
        assert!(encoded.starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn test_encode_del_character() {
        // DEL character (0x7f) should be encoded
        let input = "hello\x7fworld";
        let encoded = encode(input);
        assert!(encoded.starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn test_encode_empty_string() {
        let input = "";
        let encoded = encode(input);
        assert_eq!(encoded, "");
    }

    #[test]
    fn test_encode_mixed_content() {
        // Mixed ASCII and non-ASCII should trigger encoding
        let input = "Hello 世界";
        let encoded = encode(input);
        assert!(encoded.starts_with("=?UTF-8?B?"));
    }

    // ==================== Decoding Tests ====================

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
    fn test_decode_underscore_as_space() {
        let input = "=?UTF-8?Q?hello_world?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "hello world");
    }

    #[test]
    fn test_decode_lowercase_encoding() {
        // Encoding specifier should be case-insensitive
        let input = "=?utf-8?b?5L2g5aW9?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "你好");
    }

    #[test]
    fn test_decode_lowercase_q_encoding() {
        // Q encoding specifier should be case-insensitive
        let input = "=?UTF-8?q?hello_world?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "hello world");
    }

    #[test]
    fn test_decode_with_whitespace_trim() {
        // Input with leading/trailing whitespace should be trimmed
        let input = "  =?UTF-8?B?5L2g5aW9?=  ";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "你好");
    }

    #[test]
    fn test_decode_utf8_charset_variant() {
        // UTF8 without hyphen should also work
        let input = "=?UTF8?B?5L2g5aW9?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "你好");
    }

    #[test]
    fn test_decode_other_charset_as_utf8() {
        // Non-UTF-8 charset falls back to UTF-8 decoding
        // This works if the actual bytes are valid UTF-8
        let input = "=?ISO-8859-1?B?SGVsbG8=?="; // "Hello" in Base64
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "Hello");
    }

    // ==================== Roundtrip Tests ====================

    #[test]
    fn test_roundtrip() {
        let original = "Hello 世界 🌍";
        let encoded = encode(original);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_roundtrip_ascii() {
        let original = "plain ascii text";
        let encoded = encode(original);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_roundtrip_emoji() {
        let original = "🎉🎊🎁";
        let encoded = encode(original);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    // ==================== Error Cases ====================

    #[test]
    fn test_decode_invalid_base64() {
        let input = "=?UTF-8?B?!!!?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::Base64Error));
    }

    #[test]
    fn test_decode_unsupported_encoding() {
        // X is not a valid encoding type
        let input = "=?UTF-8?X?dGVzdA==?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::UnsupportedEncoding));
    }

    #[test]
    fn test_decode_missing_encoding_part() {
        // Missing the encoding part (only has charset)
        let input = "=?UTF-8?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn test_decode_missing_encoded_text() {
        // Input: "=?UTF-8?B?=" - after removing delimiters we get "UTF-8?B"
        // This only has 2 parts when split by '?', but we need 3 (charset, encoding, encoded_text)
        let input = "=?UTF-8?B?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn test_decode_qp_non_ascii_rejected() {
        // Non-ASCII characters should not appear directly in Q-encoded text
        let input = "=?UTF-8?Q?café?="; // The 'é' should have been =C3=A9
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn test_decode_qp_incomplete_hex_one_char() {
        // Only one hex digit after =
        let input = "=?UTF-8?Q?test=A?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn test_decode_qp_incomplete_hex_no_chars() {
        // = at the end with no hex digits
        let input = "=?UTF-8?Q?test=?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn test_decode_qp_invalid_hex() {
        // Invalid hex digits (GG is not valid hex)
        let input = "=?UTF-8?Q?test=GG?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidHex));
    }

    #[test]
    fn test_decode_invalid_utf8_bytes() {
        // Base64 encoded invalid UTF-8 sequence (0xFF 0xFE)
        let input = "=?UTF-8?B?//4=?=";
        let result = decode(input);
        assert_eq!(result, Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn test_decode_not_starting_with_marker() {
        // Ends with ?= but doesn't start with =?
        let input = "hello?=";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "hello?=");
    }

    #[test]
    fn test_decode_not_ending_with_marker() {
        // Starts with =? but doesn't end with ?=
        let input = "=?hello";
        let decoded = decode(input).unwrap();
        assert_eq!(decoded, "=?hello");
    }

    // ==================== DecodeError Display Tests ====================

    #[test]
    fn test_decode_error_display_invalid_format() {
        let err = DecodeError::InvalidFormat;
        assert_eq!(err.to_string(), "invalid RFC 2047 encoded-word format");
    }

    #[test]
    fn test_decode_error_display_base64_error() {
        let err = DecodeError::Base64Error;
        assert_eq!(err.to_string(), "base64 decoding failed");
    }

    #[test]
    fn test_decode_error_display_invalid_hex() {
        let err = DecodeError::InvalidHex;
        assert_eq!(err.to_string(), "invalid hex in quoted-printable encoding");
    }

    #[test]
    fn test_decode_error_display_invalid_utf8() {
        let err = DecodeError::InvalidUtf8;
        assert_eq!(err.to_string(), "decoded bytes are not valid UTF-8");
    }

    #[test]
    fn test_decode_error_display_unsupported_encoding() {
        let err = DecodeError::UnsupportedEncoding;
        assert_eq!(err.to_string(), "unsupported encoding type");
    }

    // ==================== DecodeError Trait Tests ====================

    #[test]
    fn test_decode_error_is_error() {
        let err: &dyn std::error::Error = &DecodeError::InvalidFormat;
        assert!(err.source().is_none());
    }

    #[test]
    fn test_decode_error_debug() {
        let err = DecodeError::InvalidFormat;
        assert_eq!(format!("{err:?}"), "InvalidFormat");
    }

    #[test]
    fn test_decode_error_clone() {
        let err = DecodeError::Base64Error;
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }

    #[test]
    fn test_decode_error_eq() {
        assert_eq!(DecodeError::InvalidFormat, DecodeError::InvalidFormat);
        assert_ne!(DecodeError::InvalidFormat, DecodeError::Base64Error);
    }
}

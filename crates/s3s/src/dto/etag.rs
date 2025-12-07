use std::str::FromStr;

use http::HeaderValue;
use http::header::InvalidHeaderValue;
use stdx::str::StrExt;

/// Entity Tag for the HTTP `ETag` header.
///
/// Strong: "value"; Weak: W/"value".
///
/// See RFC 7232 §2.3 and MDN:
/// + <https://www.rfc-editor.org/rfc/rfc7232#section-2.3>
/// + <https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/ETag>
#[derive(Debug, Clone, PartialEq)]
pub enum ETag {
    /// Strong validator: "value"
    Strong(String),
    /// Weak validator: W/"value"
    Weak(String),
}

/// Errors returned when parsing an `ETag` header.
#[derive(Debug, thiserror::Error)]
pub enum ParseETagError {
    /// The bytes do not match the `ETag` syntax.
    #[error("ParseETagError: InvalidFormat")]
    InvalidFormat,
    /// Contains invalid characters (control chars, DEL 0x7f, or non-ASCII).
    #[error("ParseETagError: InvalidChar")]
    InvalidChar,
}

impl ETag {
    /// Returns the raw value without strength information.
    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            ETag::Strong(s) | ETag::Weak(s) => s,
        }
    }

    /// Converts this `ETag` into its strong value if present.
    #[must_use]
    pub fn into_strong(self) -> Option<String> {
        match self {
            ETag::Strong(s) => Some(s),
            ETag::Weak(_) => None,
        }
    }

    /// Returns the strong value if this is an [`ETag::Strong`]; otherwise `None`.
    #[must_use]
    pub fn as_strong(&self) -> Option<&str> {
        match self {
            ETag::Strong(s) => Some(s),
            ETag::Weak(_) => None,
        }
    }

    /// Returns the weak value if this is an [`ETag::Weak`]; otherwise `None`.
    #[must_use]
    pub fn as_weak(&self) -> Option<&str> {
        match self {
            ETag::Weak(s) => Some(s),
            ETag::Strong(_) => None,
        }
    }

    /// Consumes the `ETag`, discarding the strength and returning its raw value.
    #[must_use]
    pub fn into_value(self) -> String {
        match self {
            ETag::Strong(s) | ETag::Weak(s) => s,
        }
    }

    /// Converts this `ETag` into its weak value if present.
    #[must_use]
    pub fn into_weak(self) -> Option<String> {
        match self {
            ETag::Weak(s) => Some(s),
            ETag::Strong(_) => None,
        }
    }

    /// Returns the raw value as a string slice (alias for `value()`).
    #[must_use]
    pub fn to_str(&self) -> &str {
        self.value()
    }
}

impl PartialEq<String> for ETag {
    fn eq(&self, other: &String) -> bool {
        self.value() == other
    }
}

impl PartialEq<ETag> for String {
    fn eq(&self, other: &ETag) -> bool {
        self == other.value()
    }
}

impl PartialEq<str> for ETag {
    fn eq(&self, other: &str) -> bool {
        self.value() == other
    }
}

impl PartialEq<ETag> for str {
    fn eq(&self, other: &ETag) -> bool {
        self == other.value()
    }
}

impl ETag {
    fn check_header_value(s: &[u8]) -> bool {
        s.iter().all(|&b| b >= 32 && b != 127 || b == b'\t')
    }

    /// Parses an `ETag` from header bytes.
    ///
    /// # Errors
    /// + Returns `ParseETagError::InvalidFormat` if the bytes do not match the `ETag` syntax.
    /// + Returns `ParseETagError::InvalidChar` if the value contains invalid characters
    pub fn parse_http_header(src: &[u8]) -> Result<Self, ParseETagError> {
        // FIXME: this impl is not optimal unless `unsafe` is used
        match src {
            [b'"', val @ .., b'"'] => {
                if !Self::check_header_value(val) {
                    return Err(ParseETagError::InvalidChar);
                }
                let val = str::from_ascii_simd(val).map_err(|_| ParseETagError::InvalidChar)?;
                Ok(ETag::Strong(val.to_owned()))
            }
            [b'W', b'/', b'"', val @ .., b'"'] => {
                if !Self::check_header_value(val) {
                    return Err(ParseETagError::InvalidChar);
                }
                let val = str::from_ascii_simd(val).map_err(|_| ParseETagError::InvalidChar)?;
                Ok(ETag::Weak(val.to_owned()))
            }
            _ => Err(ParseETagError::InvalidFormat),
        }
    }

    /// Encodes this `ETag` as an HTTP header value.
    ///
    /// # Errors
    /// Returns `InvalidHeaderValue` if the `ETag` value contains invalid characters for HTTP headers.
    pub fn to_http_header(&self) -> Result<HeaderValue, InvalidHeaderValue> {
        let buf = match self {
            ETag::Strong(s) => {
                let mut buf = Vec::with_capacity(s.len() + 2);
                buf.push(b'"');
                buf.extend_from_slice(s.as_bytes());
                buf.push(b'"');
                buf
            }
            ETag::Weak(s) => {
                let mut buf = Vec::with_capacity(s.len() + 4);
                buf.extend_from_slice(b"W/\"");
                buf.extend_from_slice(s.as_bytes());
                buf.push(b'"');
                buf
            }
        };
        HeaderValue::try_from(buf)
    }
}

impl FromStr for ETag {
    type Err = ParseETagError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_http_header(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::{ETag, ParseETagError};

    #[test]
    fn strong_value_and_header_ok() {
        let etag = ETag::Strong("abc123".to_string());
        assert_eq!(etag.value(), "abc123");
        let hv = etag.to_http_header().expect("strong etag header");
        assert_eq!(hv.as_bytes(), b"\"abc123\"");
    }

    #[test]
    fn weak_value_and_header_ok() {
        let etag = ETag::Weak("xyz".to_string());
        assert_eq!(etag.value(), "xyz");
        // 标准：弱 ETag 前缀为 W/"
        let hv = etag.to_http_header().expect("weak etag header");
        assert_eq!(hv.as_bytes(), b"W/\"xyz\"");
    }

    #[test]
    fn strong_empty_header_ok() {
        let etag = ETag::Strong(String::new());
        let hv = etag.to_http_header().expect("empty strong etag");
        assert_eq!(hv.as_bytes(), b"\"\"");
    }

    #[test]
    fn weak_empty_header_ok() {
        let etag = ETag::Weak(String::new());
        let hv = etag.to_http_header().expect("empty weak etag");
        assert_eq!(hv.as_bytes(), b"W/\"\"");
    }

    #[test]
    fn header_invalid_when_contains_newline() {
        // 含有换行等控制字符应返回错误
        let strong_bad = ETag::Strong("a\nb".to_string());
        assert!(strong_bad.to_http_header().is_err());

        let weak_bad = ETag::Weak("a\r\nb".to_string());
        assert!(weak_bad.to_http_header().is_err());
    }

    #[test]
    fn parse_strong_ok() {
        let etag = ETag::parse_http_header(b"\"abc123\"").expect("parse strong");
        assert_eq!(etag.as_strong(), Some("abc123"));
    }

    #[test]
    fn parse_weak_ok() {
        let etag = ETag::parse_http_header(b"W/\"xyz\"").expect("parse weak");
        assert_eq!(etag.as_weak(), Some("xyz"));
    }

    #[test]
    fn parse_empty_ok() {
        let s = ETag::parse_http_header(b"\"\"").expect("parse empty strong");
        assert_eq!(s.as_strong(), Some(""));

        let w = ETag::parse_http_header(b"W/\"\"").expect("parse empty weak");
        assert_eq!(w.as_weak(), Some(""));
    }

    #[test]
    fn parse_allows_tab() {
        let s = ETag::parse_http_header(b"\"a\tb\"").expect("tab in strong");
        assert_eq!(s.as_strong(), Some("a\tb"));

        let w = ETag::parse_http_header(b"W/\"a\tb\"").expect("tab in weak");
        assert_eq!(w.as_weak(), Some("a\tb"));
    }

    #[test]
    fn parse_invalid_format_cases() {
        let cases: &[&[u8]] = &[
            b"",
            b"abc",
            b"\"unclosed",
            b"W/\"unclosed",
            b"W/xyz",      // 缺少引号
            b"\"abc\"x",   // 尾随字符
            b"W/\"abc\"x", // 尾随字符
        ];
        for &c in cases {
            let err = ETag::parse_http_header(c).unwrap_err();
            assert!(matches!(err, ParseETagError::InvalidFormat), "case={c:?}");
        }
    }

    #[test]
    fn parse_invalid_char_cases() {
        // 含有换行/回车
        let err = ETag::parse_http_header(b"\"a\nb\"").unwrap_err();
        assert!(matches!(err, ParseETagError::InvalidChar));

        let err = ETag::parse_http_header(b"W/\"a\rb\"").unwrap_err();
        assert!(matches!(err, ParseETagError::InvalidChar));

        // 含有 DEL(0x7f)
        let err = ETag::parse_http_header(b"\"a\x7fb\"").unwrap_err();
        assert!(matches!(err, ParseETagError::InvalidChar));

        let err = ETag::parse_http_header(b"W/\"a\x7fb\"").unwrap_err();
        assert!(matches!(err, ParseETagError::InvalidChar));

        // 含有非 ASCII（触发 from_ascii_simd 错误）
        let err = ETag::parse_http_header(b"\"a\xc2\xb5b\"").unwrap_err(); // µ
        assert!(matches!(err, ParseETagError::InvalidChar));
    }

    #[test]
    fn to_header_allows_tab() {
        let etag = ETag::Strong("a\tb".to_string());
        let hv = etag.to_http_header().expect("header with tab");
        assert_eq!(hv.as_bytes(), b"\"a\tb\"");
    }

    #[test]
    fn header_invalid_when_contains_del_127() {
        let s = String::from_utf8(vec![b'a', 0x7f, b'b']).unwrap();
        assert!(ETag::Strong(s.clone()).to_http_header().is_err());
        assert!(ETag::Weak(s).to_http_header().is_err());
    }

    #[test]
    fn parse_and_header_roundtrip() {
        let values = ["", "abc", "a\tb", " !#$%&()*+,-./:;<=>?@[]^_`{|}~"];
        for v in values {
            // strong
            let e = ETag::Strong(v.to_string());
            let hv = e.to_http_header().expect("strong header");
            let p = ETag::parse_http_header(hv.as_bytes()).expect("parse strong back");
            assert_eq!(p.as_strong(), Some(v));

            // weak
            let e = ETag::Weak(v.to_string());
            let hv = e.to_http_header().expect("weak header");
            let p = ETag::parse_http_header(hv.as_bytes()).expect("parse weak back");
            assert_eq!(p.as_weak(), Some(v));
        }
    }

    #[test]
    fn from_str_trait() {
        // strong ETag via FromStr
        let e: ETag = "\"abc123\"".parse().expect("parse strong from str");
        assert_eq!(e.as_strong(), Some("abc123"));

        // weak ETag via FromStr
        let e: ETag = "W/\"xyz\"".parse().expect("parse weak from str");
        assert_eq!(e.as_weak(), Some("xyz"));

        // invalid format via FromStr
        let err = "abc".parse::<ETag>().unwrap_err();
        assert!(matches!(err, ParseETagError::InvalidFormat));
    }

    #[test]
    fn test_to_str() {
        let etag = ETag::Strong("abc".to_string());
        let s = etag.to_str();
        assert_eq!(s, "abc");
        let ref expected = String::from("abc");
        assert_eq!(&etag == expected, true);
    }
}

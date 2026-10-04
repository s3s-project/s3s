// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::str::FromStr;

use http::HeaderValue;
use http::header::InvalidHeaderValue;
use smallvec::SmallVec;

use super::etag::{ETag, ParseETagError};

/// Condition value for `If-Match`, `If-None-Match` and related headers.
///
/// According to RFC 9110 §13.1.1 and §13.1.2, these headers can contain either:
/// - A single `ETag` value (strong or weak): `"value"` or `W/"value"`
/// - A comma-separated list of `ETag` values (`1#entity-tag`), satisfied when any member matches
/// - A wildcard: `*` (matches any existing entity)
///
/// The wildcard is commonly used for conditional requests like:
/// - `If-None-Match: *` - Only create if the resource doesn't exist (PUT)
/// - `If-Match: *` - Only modify if the resource exists
///
/// Entity tags are compared with the function required by the header:
/// [`ETagCondition::matches_strong`] for `If-Match` and
/// [`ETagCondition::matches_weak`] for `If-None-Match`.
///
/// See RFC 9110 §13.1 and MDN:
/// + <https://www.rfc-editor.org/rfc/rfc9110#section-13.1>
/// + <https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/If-None-Match>
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ETagCondition {
    /// A single `ETag` value (strong or weak)
    ETag(ETag),
    /// The wildcard `*` that matches any existing entity
    Any,
    /// A comma-separated list of entity tags, satisfied when any member matches
    List(Vec<ETag>),
}

/// Errors returned when parsing an `ETagCondition` header.
#[derive(Debug, thiserror::Error)]
pub enum ParseETagConditionError {
    /// The bytes do not match the expected syntax.
    #[error("ParseETagConditionError: InvalidFormat")]
    InvalidFormat,
    /// Contains invalid characters.
    #[error("ParseETagConditionError: InvalidChar")]
    InvalidChar,
    /// Error parsing the `ETag` value
    #[error("ParseETagConditionError: {0}")]
    ETagError(#[from] ParseETagError),
}

/// Returns true when `byte` is optional whitespace (SP / HTAB).
fn is_ows(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// Trims optional whitespace from both ends of a list element.
fn trim_ows(src: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = src.len();
    while start < end && is_ows(src[start]) {
        start += 1;
    }
    while end > start && is_ows(src[end - 1]) {
        end -= 1;
    }
    &src[start..end]
}

/// Splits a comma-separated entity tag list on the commas that are not inside a
/// quoted opaque tag.
///
/// A comma is a valid character of an entity tag (`"a,b"` is a single tag), so
/// the split has to be quote-aware. Returns `false` when the double quotes are
/// unbalanced.
///
/// The elements are pushed into `elements`. The receiver is a `SmallVec` so the
/// usual one to four tags stay inline and parsing a conditional header does not
/// allocate for the split itself.
fn split_list<'a>(src: &'a [u8], elements: &mut SmallVec<[&'a [u8]; 4]>) -> bool {
    let mut start = 0;
    let mut in_quotes = false;
    for (index, &byte) in src.iter().enumerate() {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                elements.push(&src[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if in_quotes {
        return false;
    }
    elements.push(&src[start..]);
    true
}

/// Parses one list element.
///
/// The empty element and the wildcard are rejected: RFC 9110 defines the field
/// value as `"*" / 1#entity-tag`, so the wildcard always stands alone.
fn parse_element(element: &[u8]) -> Result<ETag, ParseETagConditionError> {
    let element = trim_ows(element);
    if element.is_empty() || element == b"*" {
        return Err(ParseETagConditionError::InvalidFormat);
    }
    Ok(ETag::parse_http_header(element)?)
}

impl ETagCondition {
    /// Parses an `ETagCondition` from header bytes.
    ///
    /// The field value is the wildcard `*`, a single entity tag, or a
    /// comma-separated list of entity tags. Optional whitespace is allowed
    /// around each entity tag of a list, and the wildcard has to be the whole
    /// field value.
    ///
    /// # Errors
    /// + Returns `ParseETagConditionError::InvalidFormat` if the bytes do not match the expected syntax:
    ///   an empty value, an empty list element, a trailing comma, the wildcard mixed with entity tags,
    ///   or unbalanced double quotes.
    /// + Returns `ParseETagConditionError::InvalidChar` if the value contains invalid characters
    pub fn parse_http_header(src: &[u8]) -> Result<Self, ParseETagConditionError> {
        // RFC 9110 defines the field value as `"*" / 1#entity-tag`, so the
        // wildcard always stands alone.
        if src == b"*" {
            return Ok(ETagCondition::Any);
        }

        let mut elements = SmallVec::new();
        if !split_list(src, &mut elements) {
            return Err(ParseETagConditionError::InvalidFormat);
        }

        // A single entity tag is the common case: it goes straight into the
        // `ETag` variant, so no list container is allocated for it.
        match elements.as_slice() {
            [only] => Ok(ETagCondition::ETag(parse_element(only)?)),
            elements => {
                let mut etags = Vec::with_capacity(elements.len());
                for element in elements {
                    etags.push(parse_element(element)?);
                }
                Ok(ETagCondition::List(etags))
            }
        }
    }

    /// Encodes this `ETagCondition` as an HTTP header value.
    ///
    /// A list is encoded as comma-separated entity tags.
    ///
    /// # Errors
    /// Returns `InvalidHeaderValue` if the `ETag` value contains invalid characters for HTTP headers.
    pub fn to_http_header(&self) -> Result<HeaderValue, InvalidHeaderValue> {
        match self {
            ETagCondition::ETag(etag) => etag.to_http_header(),
            ETagCondition::Any => HeaderValue::try_from("*"),
            ETagCondition::List(etags) => {
                let mut buf = Vec::new();
                for (index, etag) in etags.iter().enumerate() {
                    if index > 0 {
                        buf.extend_from_slice(b", ");
                    }
                    buf.extend_from_slice(etag.to_http_header()?.as_bytes());
                }
                HeaderValue::from_bytes(&buf)
            }
        }
    }

    /// Returns the single `ETag` if this is an [`ETagCondition::ETag`], otherwise `None`.
    ///
    /// A list has no single entity tag and also returns `None`; use
    /// [`ETagCondition::etags`] to inspect all members instead.
    #[must_use]
    pub fn as_etag(&self) -> Option<&ETag> {
        match self {
            ETagCondition::ETag(etag) => Some(etag),
            ETagCondition::Any | ETagCondition::List(_) => None,
        }
    }

    /// Consumes self and returns the `ETag` if this is an [`ETagCondition::ETag`], otherwise `None`.
    ///
    /// A list has no single entity tag and also returns `None`; use
    /// [`ETagCondition::etags`] to inspect all members instead.
    #[must_use]
    pub fn into_etag(self) -> Option<ETag> {
        match self {
            ETagCondition::ETag(etag) => Some(etag),
            ETagCondition::Any | ETagCondition::List(_) => None,
        }
    }

    /// Returns true if this is the wildcard `*`.
    #[must_use]
    pub fn is_any(&self) -> bool {
        matches!(self, ETagCondition::Any)
    }

    /// Returns the entity tags carried by this condition.
    ///
    /// [`ETagCondition::ETag`] yields a single element, [`ETagCondition::List`]
    /// yields all members, and [`ETagCondition::Any`] yields an empty slice.
    #[must_use]
    pub fn etags(&self) -> &[ETag] {
        match self {
            ETagCondition::ETag(etag) => std::slice::from_ref(etag),
            ETagCondition::List(etags) => etags,
            ETagCondition::Any => &[],
        }
    }

    /// Returns true when this `If-Match` condition is satisfied by `current`.
    ///
    /// The wildcard is satisfied by any existing representation, so the caller
    /// still has to check that the representation exists. Otherwise the
    /// condition is satisfied when any entity tag is a strong match
    /// ([`ETag::strong_cmp`]), as required by RFC 9110 §13.1.1.
    #[must_use]
    pub fn matches_strong(&self, current: &ETag) -> bool {
        match self {
            ETagCondition::Any => true,
            ETagCondition::ETag(etag) => etag.strong_cmp(current),
            ETagCondition::List(etags) => etags.iter().any(|etag| etag.strong_cmp(current)),
        }
    }

    /// Returns true when this `If-None-Match` condition is satisfied by `current`.
    ///
    /// The wildcard is satisfied by any existing representation, so the caller
    /// still has to check that the representation exists. Otherwise the
    /// condition is satisfied when any entity tag is a weak match
    /// ([`ETag::weak_cmp`]), as required by RFC 9110 §13.1.2.
    #[must_use]
    pub fn matches_weak(&self, current: &ETag) -> bool {
        match self {
            ETagCondition::Any => true,
            ETagCondition::ETag(etag) => etag.weak_cmp(current),
            ETagCondition::List(etags) => etags.iter().any(|etag| etag.weak_cmp(current)),
        }
    }
}

impl FromStr for ETagCondition {
    type Err = ParseETagConditionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_http_header(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::{ETag, ETagCondition, ParseETagConditionError};

    #[test]
    fn parse_wildcard() {
        let cond = ETagCondition::parse_http_header(b"*").expect("parse wildcard");
        assert!(cond.is_any());
        assert_eq!(cond.as_etag(), None);
    }

    #[test]
    fn parse_strong_etag() {
        let cond = ETagCondition::parse_http_header(b"\"abc123\"").expect("parse strong etag");
        assert!(!cond.is_any());
        let etag = cond.as_etag().expect("should be etag");
        assert_eq!(etag.as_strong(), Some("abc123"));
    }

    #[test]
    fn parse_weak_etag() {
        let cond = ETagCondition::parse_http_header(b"W/\"xyz\"").expect("parse weak etag");
        assert!(!cond.is_any());
        let etag = cond.as_etag().expect("should be etag");
        assert_eq!(etag.as_weak(), Some("xyz"));
    }

    #[test]
    fn to_header_wildcard() {
        let cond = ETagCondition::Any;
        let hv = cond.to_http_header().expect("wildcard header");
        assert_eq!(hv.as_bytes(), b"*");
    }

    #[test]
    fn to_header_strong_etag() {
        let cond = ETagCondition::ETag(ETag::Strong("abc123".to_string()));
        let hv = cond.to_http_header().expect("strong etag header");
        assert_eq!(hv.as_bytes(), b"\"abc123\"");
    }

    #[test]
    fn to_header_weak_etag() {
        let cond = ETagCondition::ETag(ETag::Weak("xyz".to_string()));
        let hv = cond.to_http_header().expect("weak etag header");
        assert_eq!(hv.as_bytes(), b"W/\"xyz\"");
    }

    #[test]
    fn parse_and_header_roundtrip() {
        let cases = [("*", true), ("\"abc\"", false), ("W/\"xyz\"", false)];
        for (input, is_any) in cases {
            let cond = ETagCondition::parse_http_header(input.as_bytes()).expect("parse");
            assert_eq!(cond.is_any(), is_any);
            let hv = cond.to_http_header().expect("to header");
            let parsed_back = ETagCondition::parse_http_header(hv.as_bytes()).expect("parse back");
            assert_eq!(cond, parsed_back);
        }
    }

    #[test]
    fn from_str_trait() {
        let cond: ETagCondition = "*".parse().expect("parse wildcard from str");
        assert!(cond.is_any());

        let cond: ETagCondition = "\"abc123\"".parse().expect("parse strong from str");
        assert!(!cond.is_any());
        let etag = cond.as_etag().expect("should be etag");
        assert_eq!(etag.as_strong(), Some("abc123"));

        let cond: ETagCondition = "W/\"xyz\"".parse().expect("parse weak from str");
        assert!(!cond.is_any());
        let etag = cond.as_etag().expect("should be etag");
        assert_eq!(etag.as_weak(), Some("xyz"));
    }

    #[test]
    fn parse_invalid() {
        // Empty string should return error
        let err = ETagCondition::parse_http_header(b"").unwrap_err();
        assert!(matches!(
            err,
            ParseETagConditionError::InvalidFormat | ParseETagConditionError::ETagError(_)
        ));

        // Malformed values should return error
        let err = ETagCondition::parse_http_header(b"**").unwrap_err();
        assert!(matches!(
            err,
            ParseETagConditionError::InvalidFormat | ParseETagConditionError::ETagError(_)
        ));

        let err = ETagCondition::parse_http_header(b"* ").unwrap_err();
        assert!(matches!(
            err,
            ParseETagConditionError::InvalidFormat | ParseETagConditionError::ETagError(_)
        ));

        let err = ETagCondition::parse_http_header(b"\"unclosed").unwrap_err();
        assert!(matches!(
            err,
            ParseETagConditionError::InvalidFormat | ParseETagConditionError::ETagError(_)
        ));
    }

    #[test]
    fn parse_unquoted_values() {
        // Typical S3 ETag values without quotes (alphanumeric only)
        let cond = ETagCondition::parse_http_header(b"ABCORZ").expect("parse simple string");
        assert_eq!(cond.as_etag().unwrap().as_strong(), Some("ABCORZ"));

        let cond = ETagCondition::parse_http_header(b"4fcec74691ff529f6d016ec3629ff11b").expect("parse md5 hash");
        assert_eq!(cond.as_etag().unwrap().as_strong(), Some("4fcec74691ff529f6d016ec3629ff11b"));

        // Multipart upload ETag format
        let cond = ETagCondition::parse_http_header(b"4fcec74691ff529f6d016ec3629ff11b-5").expect("parse multipart etag");
        assert_eq!(cond.as_etag().unwrap().as_strong(), Some("4fcec74691ff529f6d016ec3629ff11b-5"));

        // Single-character alphanumeric should be parsed as ETag, not confused with wildcard "*"
        let cond = ETagCondition::parse_http_header(b"a").expect("parse single char");
        assert!(!cond.is_any()); // Should NOT be wildcard
        assert_eq!(cond.as_etag().unwrap().as_strong(), Some("a"));

        let cond = ETagCondition::parse_http_header(b"1").expect("parse single digit");
        assert!(!cond.is_any()); // Should NOT be wildcard
        assert_eq!(cond.as_etag().unwrap().as_strong(), Some("1"));
    }

    #[test]
    fn comma_separated_list_is_not_one_opaque_tag() {
        // RFC 9110 §13.1.1/§13.1.2 and RFC 7232 §3.1/§3.2 define the field value
        // as a list of entity tags. A list must never collapse into a single
        // opaque tag whose value embeds a quote and a comma.
        let cond = ETagCondition::parse_http_header(b"\"a\", \"b\"").expect("parse list");
        let single = cond.as_etag().map(|etag| etag.value().to_owned());
        assert_ne!(single.as_deref(), Some("a\", \"b"), "list parsed as one opaque tag");
    }

    #[test]
    fn parse_list_of_strong_etags() {
        let cond = ETagCondition::parse_http_header(b"\"a\", \"b\"").expect("parse list");
        assert_eq!(
            cond,
            ETagCondition::List(vec![ETag::Strong("a".to_owned()), ETag::Strong("b".to_owned())])
        );
        assert!(!cond.is_any());
        assert_eq!(cond.as_etag(), None);
        assert_eq!(cond.etags().len(), 2);
        assert_eq!(cond.into_etag(), None);
    }

    #[test]
    fn parse_list_allows_ows_between_tags() {
        let cond = ETagCondition::parse_http_header(b"W/\"a\" ,\t\"b\"").expect("parse list");
        assert_eq!(cond, ETagCondition::List(vec![ETag::Weak("a".to_owned()), ETag::Strong("b".to_owned())]));
    }

    #[test]
    fn parse_list_of_three_tags() {
        let cond = ETagCondition::parse_http_header(b"\"a\", \"b\", \"c\"").expect("parse list");
        assert_eq!(cond.etags().len(), 3);
    }

    #[test]
    fn parse_single_tag_with_ows_stays_single_variant() {
        // The single-element path parses straight into the `ETag` variant; it
        // must not turn the value into a one-member list.
        let cond = ETagCondition::parse_http_header(b"  \"a\"  ").expect("parse single tag with ows");
        assert_eq!(cond, ETagCondition::ETag(ETag::Strong("a".to_owned())));
    }

    #[test]
    fn parse_list_longer_than_the_inline_buffer() {
        // More members than the `SmallVec` inline capacity, so the split spills
        // to the heap; parsing and the roundtrip must not depend on the capacity.
        let input = b"\"a\", \"b\", \"c\", \"d\", \"e\", \"f\"";
        let cond = ETagCondition::parse_http_header(input).expect("parse long list");
        assert_eq!(cond.etags().len(), 6);
        assert_eq!(
            cond,
            ETagCondition::List(vec![
                ETag::Strong("a".to_owned()),
                ETag::Strong("b".to_owned()),
                ETag::Strong("c".to_owned()),
                ETag::Strong("d".to_owned()),
                ETag::Strong("e".to_owned()),
                ETag::Strong("f".to_owned()),
            ])
        );
        let hv = cond.to_http_header().expect("to header");
        assert_eq!(hv.as_bytes(), input);
        assert_eq!(ETagCondition::parse_http_header(hv.as_bytes()).expect("parse back"), cond);
    }

    #[test]
    fn parse_single_tag_may_contain_comma() {
        // A comma is a valid character of an opaque tag, so splitting must be
        // quote-aware and keep "a,b" as one tag.
        let cond = ETagCondition::parse_http_header(b"\"a,b\"").expect("parse single tag");
        assert_eq!(cond, ETagCondition::ETag(ETag::Strong("a,b".to_owned())));
    }

    #[test]
    fn parse_list_rejects_invalid_forms() {
        let invalid = [
            "*, \"a\"",
            "\"a\", *",
            "\"a\",",
            "\"a\",, \"b\"",
            "\"a\", ",
            "\"a\", unquoted value",
            "\"unclosed",
            "\"a\", \"b",
        ];
        for input in invalid {
            assert!(
                ETagCondition::parse_http_header(input.as_bytes()).is_err(),
                "expected a parse error for {input:?}"
            );
        }
    }

    #[test]
    fn list_header_roundtrip() {
        let cond = ETagCondition::List(vec![ETag::Strong("a".to_owned()), ETag::Weak("b".to_owned())]);
        let hv = cond.to_http_header().expect("to header");
        assert_eq!(hv.as_bytes(), b"\"a\", W/\"b\"");
        let parsed = ETagCondition::parse_http_header(hv.as_bytes()).expect("parse back");
        assert_eq!(cond, parsed);
    }

    #[test]
    fn list_matching_uses_strong_or_weak_comparison() {
        let current = ETag::Strong("good".to_owned());
        let mixed = ETagCondition::List(vec![ETag::Strong("wrong".to_owned()), ETag::Strong("good".to_owned())]);
        assert!(mixed.matches_strong(&current));
        assert!(mixed.matches_weak(&current));

        let weak_only = ETagCondition::List(vec![ETag::Weak("good".to_owned())]);
        assert!(!weak_only.matches_strong(&current));
        assert!(weak_only.matches_weak(&current));

        let none = ETagCondition::List(vec![ETag::Strong("other".to_owned())]);
        assert!(!none.matches_strong(&current));
        assert!(!none.matches_weak(&current));

        assert!(ETagCondition::Any.matches_strong(&current));
        assert!(ETagCondition::Any.matches_weak(&current));
        assert_eq!(ETagCondition::Any.etags(), []);
    }

    #[test]
    fn single_etag_condition_accessors() {
        let cond = ETagCondition::ETag(ETag::Strong("a".to_owned()));
        assert_eq!(cond.etags().len(), 1);
        assert_eq!(cond.as_etag(), Some(&ETag::Strong("a".to_owned())));
        assert!(cond.matches_strong(&ETag::Strong("a".to_owned())));
        assert!(!cond.matches_strong(&ETag::Strong("b".to_owned())));
    }
}

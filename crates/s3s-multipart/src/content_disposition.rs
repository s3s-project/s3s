// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::utils::trim_ows;

/// A parsed `Content-Disposition` value.
///
/// The `name` and `file_name` fields borrow the original header value bytes
/// and are not guaranteed to be UTF-8. Callers decide how to convert them.
///
/// Only the `form-data` disposition type is recognized; a value with any other
/// type (for example `attachment`) is rejected by
/// [`parse_content_disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentDisposition<'a> {
    /// The `name` parameter, if present.
    pub name: Option<&'a [u8]>,
    /// The `filename` parameter, if present.
    pub file_name: Option<&'a [u8]>,
}

/// Parses a single `Content-Disposition` header value.
///
/// The parser is deliberately tolerant:
///
/// - the disposition type is matched case-insensitively and must be
///   `form-data`;
/// - parameter names are matched case-insensitively;
/// - parameters may appear in any order;
/// - unknown parameters (including `filename*`) are ignored;
/// - parameter values may be quoted strings or bare tokens;
/// - a missing `name` is represented as `None`.
///
/// Returned `name` and `file_name` values borrow the input and are returned
/// verbatim: they are always sub-slices of `value`, so parsing never
/// allocates. Quoted values keep their backslash escape sequences
/// (`quoted-pair`), which RFC 2046 expects a parser to decode; decoding and
/// UTF-8 conversion are the caller's responsibility. For example,
/// `filename="a\"b.txt"` yields `a\"b.txt`, not `a"b.txt`.
///
/// # Best-effort semantics
///
/// Parsing stops at the first malformed parameter and the parameters parsed so
/// far are returned. A malformed tail is therefore indistinguishable from a
/// part without a `name`: both yield `name: None`. For example
/// `form-data; name="unterminated` and `form-data; junk` both return
/// `Some(ContentDisposition { name: None, file_name: None })`. Callers that
/// must reject malformed values have to validate the raw header themselves; a
/// `None` return only means that the value is not a `form-data`
/// `Content-Disposition`.
#[must_use]
pub fn parse_content_disposition(value: &[u8]) -> Option<ContentDisposition<'_>> {
    let input = trim_ows(value);
    let semicolon = memchr::memchr(b';', input);

    let disposition = match semicolon {
        Some(idx) => trim_ows(&input[..idx]),
        None => input,
    };
    if !disposition.eq_ignore_ascii_case(b"form-data") {
        return None;
    }

    let Some(semicolon) = semicolon else {
        return Some(ContentDisposition {
            name: None,
            file_name: None,
        });
    };

    let mut name = None;
    let mut file_name = None;
    let mut rest = &input[semicolon.saturating_add(1)..];

    while !trim_ows(rest).is_empty() {
        let Some((key, parsed_value, next)) = parse_parameter(rest) else {
            break;
        };

        if name.is_none() && key.eq_ignore_ascii_case(b"name") {
            name = Some(parsed_value);
        } else if file_name.is_none() && key.eq_ignore_ascii_case(b"filename") {
            file_name = Some(parsed_value);
        }

        rest = next;
    }

    Some(ContentDisposition { name, file_name })
}

fn parse_parameter(input: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let input = trim_ows(input);
    let equals = memchr::memchr(b'=', input)?;
    let key = trim_ows(&input[..equals]);
    if key.is_empty() {
        return None;
    }

    let value_input = trim_ows(&input[equals.saturating_add(1)..]);
    let (value, next) = if value_input.first() == Some(&b'"') {
        let value = parse_quoted(value_input)?;
        let next = skip_until_semicolon(value.next);
        (value.text, next)
    } else {
        let semicolon = memchr::memchr(b';', value_input);
        match semicolon {
            Some(idx) => (trim_ows(&value_input[..idx]), &value_input[idx.saturating_add(1)..]),
            None => (trim_ows(value_input), &b""[..]),
        }
    };

    Some((key, value, next))
}

fn parse_quoted(input: &[u8]) -> Option<ParsedQuoted<'_>> {
    if input.first() != Some(&b'"') {
        return None;
    }

    // Bulk-scan for the closing quote or the first backslash. A value that
    // contains escapes falls back to the byte-wise scan below, so escape-heavy
    // input is never slower than scanning every byte up front.
    let idx = memchr::memchr2(b'"', b'\\', &input[1..])?;
    let pos = idx.saturating_add(1);
    if input[pos] == b'"' {
        return Some(ParsedQuoted {
            text: &input[1..pos],
            next: &input[pos.saturating_add(1)..],
        });
    }

    let mut escaped = false;
    let mut end = None;
    for (offset, byte) in input[pos..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            end = Some(pos.saturating_add(offset));
            break;
        }
    }

    let end = end?;
    Some(ParsedQuoted {
        text: &input[1..end],
        next: &input[end.saturating_add(1)..],
    })
}

fn skip_until_semicolon(input: &[u8]) -> &[u8] {
    match memchr::memchr(b';', input) {
        Some(idx) => &input[idx.saturating_add(1)..],
        None => &b""[..],
    }
}

struct ParsedQuoted<'a> {
    text: &'a [u8],
    next: &'a [u8],
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn parse(value: &[u8]) -> Option<(Option<&[u8]>, Option<&[u8]>)> {
        let cd = parse_content_disposition(value)?;
        Some((cd.name, cd.file_name))
    }

    #[allow(clippy::type_complexity, clippy::unnecessary_wraps)]
    fn expected<'a>(name: Option<&'a [u8]>, file_name: Option<&'a [u8]>) -> Option<(Option<&'a [u8]>, Option<&'a [u8]>)> {
        Some((name, file_name))
    }

    #[test]
    fn parses_canonical_form() {
        assert_eq!(
            parse(b"form-data; name=\"file\"; filename=\"a.txt\""),
            expected(Some(b"file"), Some(b"a.txt"))
        );
    }

    #[test]
    fn accepts_parameter_order_and_case() {
        assert_eq!(
            parse(b"FORM-DATA; FILENAME=\"a.txt\"; NAME=\"file\""),
            expected(Some(b"file"), Some(b"a.txt"))
        );
    }

    #[test]
    fn accepts_token_values() {
        assert_eq!(parse(b"form-data; name=file; filename=a.txt"), expected(Some(b"file"), Some(b"a.txt")));
    }

    #[test]
    fn ignores_unknown_parameters() {
        assert_eq!(
            parse(b"form-data; size=10; name=\"file\"; filename*=UTF-8''a.txt; x=y"),
            expected(Some(b"file"), None)
        );
    }

    #[test]
    fn handles_escaped_quote_without_early_termination() {
        // Deliberate RFC 2046 difference: `quoted-pair` is not decoded, the
        // value is returned verbatim (see the function documentation).
        assert_eq!(
            parse(b"form-data; name=\"fi\\\"le\"; filename=\"a.txt\""),
            expected(Some(b"fi\\\"le"), Some(b"a.txt"))
        );
    }

    #[test]
    fn handles_missing_name() {
        assert_eq!(parse(b"form-data; filename=\"a.txt\""), expected(None, Some(b"a.txt")));
        assert_eq!(parse(b"form-data"), expected(None, None));
    }

    #[test]
    fn rejects_non_form_data() {
        assert_eq!(parse(b"attachment; filename=\"a.txt\""), None);
        assert_eq!(parse(b""), None);
    }

    #[test]
    fn tolerates_optional_whitespace() {
        assert_eq!(
            parse(b" form-data ; name = \"file\" ; filename = \"a.txt\" "),
            expected(Some(b"file"), Some(b"a.txt"))
        );
        // OWS is trimmed around the quotes and kept inside them.
        assert_eq!(parse(b"form-data; name=\" a \""), expected(Some(b" a "), None));
        assert_eq!(parse(b"form-data; name=\"\ta\t\""), expected(Some(b"\ta\t"), None));
    }

    #[test]
    fn quoted_value_is_returned_verbatim() {
        assert_eq!(parse(b"form-data; name=\"a\\tb\""), expected(Some(b"a\\tb"), None));
    }

    #[test]
    fn malformed_tail_parameter_stops_parsing() {
        assert_eq!(parse(b"form-data; name=\"file\"; badparam"), expected(Some(b"file"), None));
    }

    #[test]
    fn unterminated_quoted_value_stops_parsing() {
        assert_eq!(parse(b"form-data; name=\"unterminated"), expected(None, None));
        assert_eq!(parse(b"form-data; name=\"file\"; filename=\"a.txt"), expected(Some(b"file"), None));
        // Trailing backslash: the escaped byte never arrives.
        assert_eq!(parse(b"form-data; name=\"abc\\"), expected(None, None));
    }

    #[test]
    fn first_duplicate_parameter_wins() {
        assert_eq!(parse(b"form-data; name=\"one\"; name=\"two\""), expected(Some(b"one"), None));
        assert_eq!(
            parse(b"form-data; name=\"file\"; filename=\"a\"; filename=\"b\""),
            expected(Some(b"file"), Some(b"a"))
        );
    }

    #[test]
    fn parameter_helpers_reject_invalid_shapes() {
        assert_eq!(parse_parameter(b"=value"), None);
        assert!(parse_quoted(b"not-quoted").is_none());
    }
}

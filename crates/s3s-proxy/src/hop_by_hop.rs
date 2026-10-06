// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Hop-by-hop headers, which a forwarding proxy must not pass on.
//!
//! RFC 9110 section 7.6.1: an intermediary removes the connection-specific
//! header fields before forwarding a message — the ones listed here, plus every
//! field the message's `connection` header names. `transfer-encoding` is
//! removed for a second reason: `hyper` frames the forwarded body itself.
//!
//! `host` and `content-length` are deliberately **not** here. The signature a
//! client computed covers `host`, and the backend verifies it against that
//! value, while a forwarded body keeps the exact bytes and length it arrived
//! with.

use hyper::HeaderMap;

/// Headers that must not be forwarded to the backend.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Whether `name` is a hop-by-hop header.
#[must_use]
pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP_HEADERS.iter().any(|header| header.eq_ignore_ascii_case(name))
}

/// Whether `name` must not be forwarded: it is hop-by-hop itself, or the
/// message's `connection` header names it.
#[must_use]
pub(crate) fn is_connection_specific(headers: &HeaderMap, name: &str) -> bool {
    if is_hop_by_hop(name) {
        return true;
    }
    headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|named| named.trim().eq_ignore_ascii_case(name))
}

/// Copies `headers`, leaving the connection-specific fields behind.
///
/// Both halves of a forwarded exchange use this: the request headers on their way
/// to the backend and the response headers on their way back to the client.
#[must_use]
pub(crate) fn without_connection_specific(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (name, value) in headers {
        if is_connection_specific(headers, name.as_str()) {
            continue;
        }
        filtered.append(name, value.clone());
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(hyper::header::CONNECTION, value.parse().expect("header"));
        headers
    }

    #[test]
    fn hop_by_hop_headers_are_detected() {
        for name in [
            "connection",
            "Connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "Transfer-Encoding",
            "upgrade",
        ] {
            assert!(is_hop_by_hop(name), "{name} should be hop-by-hop");
        }
    }

    #[test]
    fn end_to_end_headers_are_forwarded() {
        for name in ["authorization", "host", "content-length", "content-type", "x-amz-date"] {
            assert!(!is_hop_by_hop(name), "{name} should be forwarded");
        }
    }

    #[test]
    fn filtering_keeps_end_to_end_headers() {
        let mut headers = connection_header("x-drop-me");
        headers.insert("x-drop-me", "1".parse().expect("header"));
        headers.insert("keep-alive", "timeout=5".parse().expect("header"));
        headers.insert("transfer-encoding", "chunked".parse().expect("header"));
        headers.insert("content-length", "2".parse().expect("header"));
        headers.insert("x-backend", "yes".parse().expect("header"));

        let filtered = without_connection_specific(&headers);

        assert_eq!(
            filtered.get("content-length").and_then(|value| value.to_str().ok()),
            Some("2"),
            "the length of the forwarded body is kept"
        );
        assert_eq!(filtered.get("x-backend").and_then(|value| value.to_str().ok()), Some("yes"));
        for name in ["connection", "x-drop-me", "keep-alive", "transfer-encoding"] {
            assert!(filtered.get(name).is_none(), "{name} must not be forwarded");
        }
    }

    #[test]
    fn connection_names_its_own_headers() {
        let headers = connection_header("keep-alive, x-drop-me");
        assert!(is_connection_specific(&headers, "x-drop-me"), "a named header is connection-specific");
        assert!(is_connection_specific(&headers, "X-Drop-Me"), "the name is case-insensitive");
        assert!(!is_connection_specific(&headers, "x-keep-me"));
        assert!(!is_connection_specific(&HeaderMap::new(), "x-keep-me"));
    }
}

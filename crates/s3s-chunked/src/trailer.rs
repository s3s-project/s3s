// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Handle for trailing headers produced by a decoder.

use crate::error::Error;
use crate::limits::Limits;
use crate::utils::trim_ascii_whitespace;

use std::fmt::{self, Debug, Formatter};
use std::sync::{Arc, Mutex};

use http::HeaderMap;

/// Parsed trailer block.
pub struct ParsedTrailers {
    /// Canonical form used for signature verification.
    pub canonical: Vec<u8>,
    /// The declared trailer signature, when present.
    pub signature: Option<Vec<u8>>,
    /// Trailing headers, duplicate names preserved.
    pub headers: HeaderMap,
}

/// The header carrying the trailer signature.
const TRAILER_SIGNATURE: &str = "x-amz-trailer-signature";

/// Trailer data collected from a block, before canonicalization.
struct CollectedTrailers {
    /// Header entries, sorted by name.
    entries: Vec<(String, String)>,
    /// The declared trailer signature, when present.
    signature: Option<Vec<u8>>,
}

/// Parses a non-empty trailer block.
pub fn parse_trailers(buf: &[u8], limits: &Limits) -> Result<ParsedTrailers, Error> {
    let collected = collect_trailer_entries(buf, limits)?;
    let canonical = build_canonical(&collected.entries, limits)?;
    let headers = build_header_map(collected.entries)?;

    Ok(ParsedTrailers {
        canonical,
        signature: collected.signature,
        headers,
    })
}

/// Collects the trailing headers and the declared signature.
///
/// Entries are sorted by name, which is the order of the canonical form.
fn collect_trailer_entries(buf: &[u8], limits: &Limits) -> Result<CollectedTrailers, Error> {
    let mut entries = Vec::new();
    let mut signature = None;
    let mut header_count = 0_usize;

    for line in trailer_lines(buf) {
        if line.is_empty() {
            continue;
        }

        header_count = header_count.saturating_add(1);
        if header_count > limits.max_trailer_headers {
            return Err(Error::TooManyTrailerHeaders(header_count, limits.max_trailer_headers));
        }

        let (name, value) = parse_trailer_line(line)?;
        if name == TRAILER_SIGNATURE {
            signature = Some(value.into_bytes());
        } else {
            entries.push((name, value));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(CollectedTrailers { entries, signature })
}

/// Iterates the lines of a trailer block, dropping the CR of a CRLF pair.
fn trailer_lines(buf: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    let mut rest = buf;

    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }

        let (mut line, tail) = match memchr::memchr(b'\n', rest) {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = tail;

        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }

        Some(line)
    })
}

/// Parses one name:value line, lowercasing the name and trimming the value.
fn parse_trailer_line(line: &[u8]) -> Result<(String, String), Error> {
    let colon = memchr::memchr(b':', line).ok_or(Error::FormatError)?;
    let name = String::from_utf8(line[..colon].to_ascii_lowercase()).map_err(|_| Error::FormatError)?;
    let value = String::from_utf8(trim_ascii_whitespace(&line[colon + 1..]).to_vec()).map_err(|_| Error::FormatError)?;

    Ok((name, value))
}

/// Builds the canonical name:value form, enforcing the size limit.
fn build_canonical(entries: &[(String, String)], limits: &Limits) -> Result<Vec<u8>, Error> {
    let declared = entries.iter().fold(0_usize, |total, (name, value)| {
        total.saturating_add(name.len()).saturating_add(value.len()).saturating_add(2)
    });
    let mut canonical = Vec::with_capacity(declared.min(limits.max_trailers_size));

    for (name, value) in entries {
        let total = canonical
            .len()
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(2);
        if total > limits.max_trailers_size {
            return Err(Error::TrailersTooLarge(total, limits.max_trailers_size));
        }

        canonical.extend_from_slice(name.as_bytes());
        canonical.push(b':');
        canonical.extend_from_slice(value.as_bytes());
        canonical.push(b'\n');
    }

    Ok(canonical)
}

/// Builds the trailing header map, preserving duplicate names.
fn build_header_map(entries: Vec<(String, String)>) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();

    for (name, value) in entries {
        let name: http::HeaderName = name.parse().map_err(|_| Error::FormatError)?;
        let value: http::HeaderValue = value.parse().map_err(|_| Error::FormatError)?;
        headers.append(name, value);
    }

    Ok(headers)
}

/// A handle to the trailing headers produced while a stream is consumed.
///
/// The handle can be cloned and kept outside the stream; it becomes ready once
/// the trailer block has been decoded and verified.
#[derive(Clone)]
pub struct TrailerHandle {
    slot: Arc<Mutex<Option<HeaderMap>>>,
}

impl TrailerHandle {
    /// Creates a handle that will never become ready.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Creates a ready handle holding `headers`.
    #[must_use]
    pub fn ready(headers: HeaderMap) -> Self {
        Self {
            slot: Arc::new(Mutex::new(Some(headers))),
        }
    }

    /// Returns whether the trailing headers are still available.
    ///
    /// The value moves out of the slot when it is taken, so this reports
    /// `false` after a successful [`TrailerHandle::take`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.slot.lock().is_ok_and(|slot| slot.is_some())
    }

    /// Takes the trailing headers if they are available.
    ///
    /// This is a one-shot operation: once it returns `Some`, later calls return
    /// `None`. Calling it before the headers are ready does not consume them.
    #[must_use]
    pub fn take(&self) -> Option<HeaderMap> {
        self.slot.lock().ok().and_then(|mut slot| slot.take())
    }

    /// Reads the trailing headers if they are available, without taking them.
    pub fn read<R>(&self, f: impl FnOnce(&HeaderMap) -> R) -> Option<R> {
        let slot = self.slot.lock().ok()?;
        Some(f(slot.as_ref()?))
    }

    /// Stores verified trailing headers.
    pub fn set(&self, headers: HeaderMap) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(headers);
        }
    }
}

impl Debug for TrailerHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrailerHandle")
            .field("ready", &self.is_ready())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert("x-amz-checksum-crc32", "AAAAAA==".parse().unwrap());
        map
    }

    #[test]
    fn empty_handle_is_not_ready() {
        let handle = TrailerHandle::empty();
        assert!(!handle.is_ready());
        assert!(handle.take().is_none());
        assert!(handle.read(|_| ()).is_none());
    }

    #[test]
    fn ready_handle_is_one_shot() {
        let handle = TrailerHandle::ready(headers());
        assert!(handle.is_ready());
        assert_eq!(handle.read(http::HeaderMap::len), Some(1));
        assert!(handle.take().is_some());
        assert!(handle.take().is_none());
        // The value moved out of the slot: the handle reports no longer ready.
        assert!(!handle.is_ready());
        assert!(handle.read(|_| ()).is_none());
    }

    #[test]
    fn clone_shares_the_slot() {
        let handle = TrailerHandle::ready(headers());
        let clone = handle.clone();
        assert!(clone.take().is_some());
        assert!(handle.take().is_none());
    }

    #[test]
    fn canonical_size_is_enforced_by_the_parser() {
        let limits = Limits {
            max_trailers_size: 4,
            ..Limits::default()
        };
        let result = parse_trailers(b"x-amz-meta-a:value\r\n", &limits);
        assert!(matches!(result, Err(Error::TrailersTooLarge(19, 4))));

        let limits = Limits {
            max_trailers_size: 19,
            ..Limits::default()
        };
        assert!(parse_trailers(b"x-amz-meta-a:value\r\n", &limits).is_ok());
    }

    #[test]
    fn debug_reports_readiness_only() {
        let handle = TrailerHandle::ready(headers());
        assert_eq!(format!("{handle:?}"), "TrailerHandle { ready: true, .. }");
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Part header block parsing and the header stage of the state machine.

use std::ops::Range;
use std::task::{Context, Poll, ready};

use arrayvec::ArrayVec;
use bytes::Bytes;
use futures_core::Stream;
use memchr::memmem;

use crate::Error;
use crate::multipart::{Multipart, State};
use crate::utils::trim_ows;

/// Header fields read per part. RFC 7578 section 4.8 defines exactly three —
/// `Content-Disposition`, `Content-Type` and the deprecated
/// `Content-Transfer-Encoding` — and requires any other header field to be
/// ignored. The limit is therefore the conforming set itself: a part with more
/// fields is still accepted, and the surplus fields are dropped.
pub const MAX_HEADERS: usize = 3;

#[derive(Debug)]
pub struct HeaderBlock {
    /// At most [`MAX_HEADERS`] spans fit here, so the container is exactly the
    /// size the protocol allows and never allocates: a part with more header
    /// fields than that is truncated rather than grown.
    spans: ArrayVec<HeaderSpan, MAX_HEADERS>,
    next: usize,
    /// Offset of the first part data byte, always inside the block that
    /// `parse_header_block` was given. [`Multipart::finish_headers`] splits the
    /// buffer there, and `split_to` panics past the end instead of returning an
    /// error, so the offset has to stay within the parsed block.
    data_start: usize,
}

#[derive(Debug, Clone)]
struct HeaderSpan {
    name: Range<usize>,
    value: Range<usize>,
}

fn parse_header_block(block: &[u8]) -> Result<HeaderBlock, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    match httparse::parse_headers(block, &mut headers) {
        Ok(httparse::Status::Complete((data_start, parsed))) => {
            let mut spans = ArrayVec::<HeaderSpan, MAX_HEADERS>::new();
            for header in parsed {
                let name_start = header.name.as_ptr() as usize - block.as_ptr() as usize;
                let value_start = header.value.as_ptr() as usize - block.as_ptr() as usize;
                // `httparse` never writes more than `MAX_HEADERS` entries, so the
                // push always fits; dropping the result keeps the path panic-free
                // instead of relying on `push`, which panics when full.
                let _ = spans.try_push(HeaderSpan {
                    name: name_start..name_start.saturating_add(header.name.len()),
                    value: value_start..value_start.saturating_add(header.value.len()),
                });
            }
            Ok(HeaderBlock {
                spans,
                next: 0,
                data_start,
            })
        }
        // The caller only parses a block it sliced through the terminating blank
        // line (or the bare `\r\n` of an empty header block), so `httparse`
        // cannot report `Partial` here; this arm guards the contract rather than
        // describing a reachable outcome.
        Ok(httparse::Status::Partial) => Err(Error::IncompleteStream),
        Err(httparse::Error::TooManyHeaders) => parse_header_block_fallback(block),
        Err(_) => Err(Error::InvalidFormat),
    }
}

/// Reads a part header block that carries more fields than [`MAX_HEADERS`].
///
/// Only reachable once `httparse` reports [`httparse::Error::TooManyHeaders`],
/// so the block always ends with the terminating blank line and holds at least
/// `MAX_HEADERS` fields; the surplus fields are ignored, as RFC 7578 section
/// 4.8 requires of a receiver.
///
/// The fields it keeps were read and validated by `httparse` first, which is
/// why this reader only has to find the colon and trim optional whitespace: a
/// field name that is not a valid token, or a field with no colon at all, was
/// already rejected by `httparse` before the window overflowed. That is a
/// property of the call site, not of this function — calling it on arbitrary
/// bytes would accept names `httparse` would refuse.
fn parse_header_block_fallback(block: &[u8]) -> Result<HeaderBlock, Error> {
    // The caller already slices through the terminating blank line, so do not
    // scan for `\r\n\r\n` again here.
    if !block.ends_with(b"\r\n\r\n") {
        return Err(Error::InvalidFormat);
    }
    let blank = block.len().saturating_sub(4);
    let header_text = &block[..blank];

    let mut spans = ArrayVec::<HeaderSpan, MAX_HEADERS>::new();
    let mut rest = header_text;
    while !rest.is_empty() && spans.len() < MAX_HEADERS {
        let (line, next_rest) = match memmem::find(rest, b"\r\n") {
            Some(line_end) => (&rest[..line_end], &rest[line_end.saturating_add(2)..]),
            None => (rest, &b""[..]),
        };
        let colon = memchr::memchr(b':', line).ok_or(Error::InvalidFormat)?;
        let name = &line[..colon];
        if name.is_empty() {
            return Err(Error::InvalidFormat);
        }
        let value = trim_ows(&line[colon.saturating_add(1)..]);
        let name_start = name.as_ptr() as usize - block.as_ptr() as usize;
        let name_end = name_start.saturating_add(name.len());
        let value_start = value.as_ptr() as usize - block.as_ptr() as usize;
        let value_end = value_start.saturating_add(value.len());
        // The loop condition already caps the count at `MAX_HEADERS`; ignoring
        // a rejected push keeps the path panic-free without an `unwrap`.
        let _ = spans.try_push(HeaderSpan {
            name: name_start..name_end,
            value: value_start..value_end,
        });
        rest = next_rest;
    }

    Ok(HeaderBlock {
        spans,
        next: 0,
        data_start: blank.saturating_add(4),
    })
}

impl<S> Multipart<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    pub(super) fn next_header_inner(&mut self) -> Result<Option<httparse::Header<'_>>, Error> {
        let Some(block) = self.headers.as_mut() else {
            return Ok(None);
        };

        let Some(span) = block.spans.get(block.next).cloned() else {
            self.finish_headers();
            return Ok(None);
        };
        block.next = block.next.saturating_add(1);

        let buffer = self.buffer_mut()?;
        let name_bytes = buffer.buf.get(span.name.clone()).ok_or(Error::InvalidFormat)?;
        let value = buffer.buf.get(span.value.clone()).ok_or(Error::InvalidFormat)?;
        let name = std::str::from_utf8(name_bytes).map_err(|_| Error::InvalidFormat)?;
        Ok(Some(httparse::Header { name, value }))
    }

    pub(super) fn finish_headers(&mut self) {
        let Some(block) = self.headers.take() else {
            return;
        };
        if self.state == State::ReadingPartHeaders {
            let _ = self.buffer_mut().map(|buf| buf.buf.split_to(block.data_start));
            self.state = State::ReadingPartData;
        }
    }

    pub(super) fn poll_ensure_headers(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if self.headers.is_some() || self.state == State::ReadingPartData {
            return Poll::Ready(Ok(()));
        }
        if self.state != State::ReadingPartHeaders {
            return Poll::Ready(Ok(()));
        }

        let max = self.max_buffer_size;
        let end = {
            let buffer = match self.buffer_mut() {
                Ok(buffer) => buffer,
                Err(err) => return Poll::Ready(Err(err)),
            };
            ready!(buffer.poll_read_header_block(max, cx))?
        };

        let block = match self.buffer_mut() {
            Ok(buffer) => parse_header_block(&buffer.buf[..end]),
            Err(err) => return Poll::Ready(Err(err)),
        };
        match block {
            Ok(block) => {
                self.headers = Some(block);
                Poll::Ready(Ok(()))
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    use std::fmt::Write as _;

    use futures_util::stream;

    use crate::Boundary;

    fn parser_from_chunks(
        chunks: Vec<Result<&'static [u8], std::io::Error>>,
    ) -> Multipart<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        Multipart::new(
            stream::iter(
                chunks
                    .into_iter()
                    .map(|item| item.map(Bytes::from_static).map_err(Error::stream_read_failed)),
            ),
            &Boundary::new(b"boundary").unwrap(),
            4096,
        )
    }

    #[test]
    fn header_parser_error_and_fallback_paths() {
        assert!(matches!(parse_header_block(b"X: y\r"), Err(Error::IncompleteStream)));
        assert!(matches!(parse_header_block(b"\0bad"), Err(Error::InvalidFormat)));

        // Forty fields overflow the scanning window: the fallback keeps the
        // first `MAX_HEADERS` and ignores the rest instead of failing the part.
        let mut block = String::new();
        for idx in 0..40 {
            let _ = write!(block, "X-{idx}: value-{idx}\r\n");
        }
        block.push_str("\r\n");
        let parsed = parse_header_block(block.as_bytes()).unwrap();
        assert_eq!(parsed.spans.len(), MAX_HEADERS);

        // The fallback validates the terminating blank line before deriving
        // `blank` from it, so an unterminated block is a format error here.
        assert!(matches!(parse_header_block_fallback(b": y\r\n\r\n"), Err(Error::InvalidFormat)));
        assert!(matches!(parse_header_block_fallback(b"X: y"), Err(Error::InvalidFormat)));
        assert!(matches!(parse_header_block_fallback(b"X: y\r\n"), Err(Error::InvalidFormat)));
        assert!(parse_header_block_fallback(b"X: y\r\n\r\n").is_ok());
    }

    /// The fallback stops as soon as it holds `MAX_HEADERS` spans, so a field
    /// beyond the cap is ignored *without being read*: a malformed surplus line
    /// cannot fail a part whose first fields are fine.
    #[test]
    fn fallback_stops_at_the_cap_without_reading_surplus_fields() {
        let mut block = String::from("X-0: 0\r\nX-1: 1\r\nX-2: 2\r\nnot a header line\r\n");
        block.push_str("\r\n");

        let parsed = parse_header_block_fallback(block.as_bytes()).unwrap();
        assert_eq!(parsed.spans.len(), MAX_HEADERS);
        assert_eq!(parsed.data_start, block.len());

        // Through the httparse path this block never reaches the fallback: a
        // fourth line that is not a header fails as a format error before the
        // window overflows, so the cap only has to guard the fallback's own
        // loop — which is what the assertion above pins.
        assert!(matches!(parse_header_block(block.as_bytes()), Err(Error::InvalidFormat)));
    }

    /// Offsets are the ones `httparse` reports for the block the caller sliced
    /// through the terminating blank line: an empty value yields an empty span
    /// rather than a bogus one, an empty header block is still a valid block,
    /// and a block without its terminating blank line is an incomplete stream
    /// rather than a format error.
    #[test]
    fn header_offsets_come_from_the_parsed_block() {
        assert!(matches!(parse_header_block(b"X: y"), Err(Error::IncompleteStream)));
        assert!(matches!(parse_header_block(b"X: y\r\n"), Err(Error::IncompleteStream)));

        // `X:` + blank line is six bytes, so `data_start` is the whole block.
        let parsed = parse_header_block(b"X:\r\n\r\n").unwrap();
        assert_eq!(parsed.spans.len(), 1);
        assert_eq!(parsed.spans[0].name, 0..1);
        assert_eq!(parsed.spans[0].value, 2..2);
        assert_eq!(parsed.data_start, 6);

        // An empty header block is still a valid one: no headers, data at the
        // end. `poll_read_header_block` hands over the bare two bytes here, and
        // `httparse` reads the leading CRLF as the terminator in both shapes.
        let parsed = parse_header_block(b"\r\n").unwrap();
        assert!(parsed.spans.is_empty());
        assert_eq!(parsed.data_start, 2);

        let parsed = parse_header_block(b"\r\n\r\n").unwrap();
        assert!(parsed.spans.is_empty());
        assert_eq!(parsed.data_start, 2);

        // The fallback only ever sees blocks with more fields than
        // `MAX_HEADERS`, where it must also land on the end of the block it was
        // given: the caller slices through the terminating blank line, so the
        // first data byte is whatever follows that block.
        let mut many = String::from("X-0: 0\r\nX-1: 1\r\nX-2: 2\r\nX-3: 3\r\n");
        let len = many.len();
        many.push_str("\r\n");
        let parsed = parse_header_block_fallback(many.as_bytes()).unwrap();
        assert_eq!(parsed.spans.len(), MAX_HEADERS);
        assert_eq!(parsed.data_start, len + 2);
    }

    /// The header stage must stay out of the way when the parser is already
    /// positioned somewhere else (moved here with `finish_headers`).
    #[test]
    fn finish_headers_keeps_non_header_state() {
        let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n")]);
        mp.state = State::ReadingBoundary;
        mp.headers = Some(HeaderBlock {
            spans: ArrayVec::new(),
            next: 0,
            data_start: 0,
        });
        mp.finish_headers();
        assert_eq!(mp.state, State::ReadingBoundary);
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! A single part view borrowed from the parser.

use std::fmt;
use std::future::poll_fn;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;

use crate::Error;
use crate::multipart::Multipart;
use crate::part_data_stream::PartDataStream;

/// A single multipart part yielded by [`Multipart`].
///
/// The part borrows the parser mutably. The returned headers and data borrow
/// the parser's internal buffer, so callers process each item before asking
/// for the next one.
pub struct Part<'m, S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    mp: &'m mut Multipart<S>,
}

impl<S> fmt::Debug for Part<'_, S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Part").finish_non_exhaustive()
    }
}

impl<'m, S> Part<'m, S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    pub(super) fn new(mp: &'m mut Multipart<S>) -> Self {
        Part { mp }
    }

    /// Poll-based variant of [`Part::next_header`].
    ///
    /// The two paths repeat the same `poll_ensure_headers` handshake by hand;
    /// see the note in [`Part::next_header`] for why they cannot share it.
    pub fn poll_next_header(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<httparse::Header<'_>>, Error>> {
        ready!(self.mp.poll_ensure_headers(cx)?);
        Poll::Ready(self.mp.next_header_inner())
    }

    /// Poll-based variant of [`Part::next_data`].
    pub fn poll_next_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, Error>> {
        ready!(self.mp.poll_ensure_headers(cx)?);
        self.mp.finish_headers();
        self.mp.poll_next_part_data_chunk(cx)
    }

    /// Returns the next header of this part.
    ///
    /// Headers are yielded one at a time and borrow the parser's internal
    /// buffer for the duration of the call. `None` marks the end of the
    /// header block; the part is then positioned at the start of its data.
    ///
    /// Parts with more than 32 headers are accepted; headers beyond the
    /// first 32 are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error when the header block is malformed, exceeds the
    /// configured buffer limit, or the underlying stream fails.
    pub async fn next_header(&mut self) -> Result<Option<httparse::Header<'_>>, Error> {
        // Deliberately not `poll_fn(|cx| self.poll_next_header(cx)).await`: the
        // returned header borrows the parser buffer, and such a reference cannot
        // escape the `FnMut` closure capture ("captured variable cannot escape
        // `FnMut` closure body"). `next_data` can delegate to `poll_next_data`
        // because it hands out owned `Bytes`. Keep both handshakes in sync.
        poll_fn(|cx| self.mp.poll_ensure_headers(cx)).await?;
        self.mp.next_header_inner()
    }

    /// Returns the next data chunk of this part.
    ///
    /// Entering the data phase discards headers that have not been yielded yet:
    /// the header block is parsed if needed and the parser then moves past it.
    /// Data chunks are passed through from the underlying stream whenever
    /// possible. Every yielded chunk is non-empty: when the delimiter is
    /// already at the front there is no data to hand out, and `None` marks the
    /// end of this part's data right away. The parser has then consumed the
    /// delimiter and is positioned before the next part.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream ends without a delimiter, the data
    /// is malformed, or the underlying stream fails.
    pub async fn next_data(&mut self) -> Result<Option<Bytes>, Error> {
        poll_fn(|cx| self.poll_next_data(cx)).await
    }

    /// Takes the remaining multipart stream, returning a self-contained
    /// [`PartDataStream`].
    ///
    /// The header block must already have been parsed, which the first
    /// completed [`Part::next_header`] call guarantees (a part without headers
    /// returns `None` from it). Headers that have not been yielded yet are
    /// discarded, exactly as when the data phase is entered through
    /// [`Part::next_data`]. The returned stream yields this part's data
    /// and ends at the closing delimiter `\r\n--boundary`; it does not
    /// interpret what follows. Convert it with
    /// [`PartDataStream::into_final`] to validate the strict closing
    /// trailer (the `--boundary--\r\n` form followed by end of stream).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFormat`] when the header block has not been
    /// parsed at all.
    pub fn take_data_stream(self) -> Result<PartDataStream<S>, Error> {
        self.mp.take_data_stream()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::pin::Pin;

    use futures::executor::block_on;
    use futures::task::noop_waker;
    use futures_util::StreamExt;

    use crate::Boundary;

    const BOUNDARY: &[u8] = b"B";
    const DATA: &[u8] = b"hello file data";

    /// Two headers, then the part data, then the closing delimiter.
    fn body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n");
        body.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
        body.extend_from_slice(DATA);
        body.extend_from_slice(b"\r\n--B--\r\n");
        body
    }

    /// Byte offset at which the header block ends (start of the part data).
    fn data_start(body: &[u8]) -> usize {
        body.windows(4).position(|w| w == b"\r\n\r\n").map_or(0, |pos| pos + 4)
    }

    fn stream_error() -> Error {
        Error::stream_read_failed(std::io::Error::other("boom"))
    }

    fn error_kind(err: &Error) -> &'static str {
        if matches!(err, Error::StreamReadFailed(_)) {
            "StreamReadFailed"
        } else if matches!(err, Error::InvalidFormat) {
            "InvalidFormat"
        } else if matches!(err, Error::IncompleteStream) {
            "IncompleteStream"
        } else if matches!(err, Error::HeaderSizeExceeded { .. }) {
            "HeaderSizeExceeded"
        } else {
            "other"
        }
    }

    /// Yields scripted items, going `Pending` once right before the item that
    /// starts at byte offset `pending_at` (waking immediately). Byte offsets,
    /// unlike item indices, place the `Pending` in a chosen parsing phase
    /// regardless of chunking.
    struct Scripted {
        items: VecDeque<Result<Bytes, Error>>,
        pending_at: Option<usize>,
        offset: usize,
        pending_done: bool,
    }

    impl Scripted {
        fn new(items: Vec<Result<Bytes, Error>>, pending_at: Option<usize>) -> Self {
            Scripted {
                items: items.into(),
                pending_at,
                offset: 0,
                pending_done: false,
            }
        }

        fn chunked(body: &[u8], chunk: usize, pending_at: Option<usize>) -> Self {
            Scripted::new(body.chunks(chunk.max(1)).map(|c| Ok(Bytes::copy_from_slice(c))).collect(), pending_at)
        }
    }

    impl Stream for Scripted {
        type Item = Result<Bytes, Error>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if !self.pending_done && self.pending_at.is_some_and(|at| self.offset >= at) {
                self.pending_done = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let item = self.items.pop_front();
            if let Some(Ok(chunk)) = &item {
                self.offset = self.offset.saturating_add(chunk.len());
            }
            Poll::Ready(item)
        }
    }

    fn parser(stream: Scripted) -> Multipart<Scripted> {
        Multipart::new(stream, &Boundary::new(BOUNDARY).unwrap(), 4096)
    }

    fn drain_headers_async<S>(part: &mut Part<'_, S>) -> Vec<(String, Vec<u8>)>
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
    {
        block_on(async {
            let mut headers = Vec::new();
            while let Some(header) = part.next_header().await.unwrap() {
                headers.push((header.name.to_string(), header.value.to_vec()));
            }
            headers
        })
    }

    fn drain_headers_poll<S>(part: &mut Part<'_, S>, cx: &mut Context<'_>) -> (Vec<(String, Vec<u8>)>, usize)
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
    {
        let mut headers = Vec::new();
        let mut pending = 0;
        loop {
            match part.poll_next_header(cx) {
                Poll::Pending => pending += 1,
                Poll::Ready(Ok(Some(header))) => headers.push((header.name.to_string(), header.value.to_vec())),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected error: {err}"),
            }
        }
        (headers, pending)
    }

    /// F27: the poll path must yield exactly what the async path yields, must
    /// resume across `Pending`, and must stay exhausted once the block is done.
    #[test]
    fn poll_next_header_matches_the_async_path() {
        let body = body();
        let data_start = data_start(&body);
        for (chunk, pending_at) in [(body.len(), None), (1, None), (7, Some(20)), (1, Some(data_start + 1))] {
            let mut mp = parser(Scripted::chunked(&body, chunk, pending_at));
            let mut part = block_on(mp.next_part()).unwrap().unwrap();
            let expected = drain_headers_async(&mut part);
            assert_eq!(expected.len(), 2, "chunk={chunk}");

            let mut mp = parser(Scripted::chunked(&body, chunk, pending_at));
            let mut part = block_on(mp.next_part()).unwrap().unwrap();
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let (headers, pending) = drain_headers_poll(&mut part, &mut cx);
            assert_eq!(headers, expected, "chunk={chunk}");
            assert_eq!(headers[0].0, "Content-Disposition", "chunk={chunk}");
            assert_eq!(headers[1].0, "Content-Type", "chunk={chunk}");

            // A `Pending` is expected only when it falls inside the header block:
            // `next_part` already consumed one that lands earlier in the stream.
            let expect_pending = pending_at.is_some_and(|at| at < data_start);
            assert_eq!(pending > 0, expect_pending, "chunk={chunk}");

            // An exhausted block stays exhausted instead of restarting or erroring.
            for _ in 0..2 {
                assert!(matches!(part.poll_next_header(&mut cx), Poll::Ready(Ok(None))), "chunk={chunk}");
            }
        }
    }

    /// F27: both header paths must classify malformed, failing and truncated
    /// streams identically.
    #[test]
    fn poll_next_header_reports_the_same_errors_as_the_async_path() {
        type Items = fn() -> Vec<Result<Bytes, Error>>;
        let cases: [(&str, &str, Items); 3] = [
            ("malformed header line", "InvalidFormat", || {
                vec![Ok(Bytes::from_static(b"--B\r\nBad Header Line\r\n\r\nx\r\n--B--\r\n"))]
            }),
            ("stream error inside the header block", "StreamReadFailed", || {
                vec![Ok(Bytes::from_static(b"--B\r\nX: y\r\n")), Err(stream_error())]
            }),
            ("stream ends inside the header block", "IncompleteStream", || {
                vec![Ok(Bytes::from_static(b"--B\r\nX: y\r\n"))]
            }),
        ];

        for (label, expected, items) in cases {
            let mut mp = parser(Scripted::new(items(), None));
            let mut part = block_on(mp.next_part()).unwrap().unwrap();
            let async_kind = block_on(async {
                loop {
                    match part.next_header().await {
                        Ok(Some(_)) => {}
                        Ok(None) => break "none",
                        Err(err) => break error_kind(&err),
                    }
                }
            });
            assert_eq!(async_kind, expected, "{label}");

            let mut mp = parser(Scripted::new(items(), None));
            let mut part = block_on(mp.next_part()).unwrap().unwrap();
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let poll_kind = loop {
                match part.poll_next_header(&mut cx) {
                    Poll::Pending | Poll::Ready(Ok(Some(_))) => {}
                    Poll::Ready(Ok(None)) => break "none",
                    Poll::Ready(Err(err)) => break error_kind(&err),
                }
            };
            assert_eq!(poll_kind, expected, "{label}");
        }
    }

    /// F28: the data phase may suspend before it hands out a chunk.
    #[test]
    fn poll_next_data_reports_pending_before_the_next_chunk() {
        let body = body();
        let mut mp = parser(Scripted::chunked(&body, 1, Some(data_start(&body) + 1)));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let (headers, _) = drain_headers_poll(&mut part, &mut cx);
        assert_eq!(headers.len(), 2);

        let mut data = Vec::new();
        let mut pending = 0;
        loop {
            match part.poll_next_data(&mut cx) {
                Poll::Pending => pending += 1,
                Poll::Ready(Ok(Some(chunk))) => data.extend_from_slice(&chunk),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected error: {err}"),
            }
        }
        assert!(pending > 0, "expected a Pending during the data phase");
        assert_eq!(data, DATA);
    }

    /// F28: a failing underlying stream surfaces from the data phase.
    #[test]
    fn poll_next_data_reports_a_stream_error() {
        let items = vec![Ok(Bytes::from_static(b"--B\r\nX: y\r\n\r\nhel")), Err(stream_error())];
        let mut mp = parser(Scripted::new(items, None));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let (headers, _) = drain_headers_poll(&mut part, &mut cx);
        assert_eq!(headers.len(), 1);

        let kind = loop {
            match part.poll_next_data(&mut cx) {
                Poll::Pending | Poll::Ready(Ok(Some(_))) => {}
                Poll::Ready(Ok(None)) => break "none",
                Poll::Ready(Err(err)) => break error_kind(&err),
            }
        };
        assert_eq!(kind, "StreamReadFailed");
    }

    /// F28: a malformed header block surfaces from `poll_next_data` as well,
    /// because that call parses the block itself when no header was read.
    #[test]
    fn poll_next_data_reports_a_header_block_error() {
        let items = vec![Ok(Bytes::from_static(b"--B\r\nBad Header Line\r\n\r\nx\r\n--B--\r\n"))];
        let mut mp = parser(Scripted::new(items, None));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match part.poll_next_data(&mut cx) {
            Poll::Ready(Err(err)) => assert_eq!(error_kind(&err), "InvalidFormat"),
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    /// F28: the first `poll_next_data` may suspend while it is still reading the
    /// header block, i.e. before any data chunk exists.
    #[test]
    fn poll_next_data_reports_pending_while_the_header_block_is_read() {
        let body = body();
        let mut mp = parser(Scripted::chunked(&body, 7, Some(20)));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(part.poll_next_data(&mut cx), Poll::Pending));

        let mut data = Vec::new();
        loop {
            match part.poll_next_data(&mut cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(Some(chunk))) => data.extend_from_slice(&chunk),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected error: {err}"),
            }
        }
        assert_eq!(data, DATA);
    }

    /// F29: entering the data phase discards headers that were never yielded.
    /// Taking the stream after the first header therefore succeeds and still
    /// delivers the whole part body.
    #[test]
    fn take_data_stream_after_a_partial_header_read_discards_the_rest() {
        let body = body();
        let mut mp = parser(Scripted::chunked(&body, body.len(), None));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let first = block_on(part.next_header()).unwrap().unwrap();
        assert_eq!(first.name, "Content-Disposition");

        let mut stream = part.take_data_stream().unwrap();
        let data = block_on(async {
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend_from_slice(&chunk.unwrap());
            }
            data
        });
        assert_eq!(data, DATA);

        // The unread `Content-Type` header is gone, the trailer is still strict.
        let mut trailer = stream.into_final();
        assert!(block_on(trailer.next()).is_none());
    }

    /// F29: `next_data` discards unread headers as well, so a caller that only
    /// wants the body does not have to drain them first.
    #[test]
    fn next_data_without_reading_headers_yields_the_part_data() {
        let body = body();
        let mut mp = parser(Scripted::chunked(&body, body.len(), None));
        let mut part = block_on(mp.next_part()).unwrap().unwrap();
        let data = block_on(async {
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                data.extend_from_slice(&chunk);
            }
            data
        });
        assert_eq!(data, DATA);
    }
}

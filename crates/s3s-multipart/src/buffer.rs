// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::Error;

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use memchr::memmem;

/// The byte buffer owned by [`super::Multipart`].
///
/// `buf` only holds structural bytes: a partial boundary match, the bytes
/// after a matched boundary, or the data prefix that followed the header
/// block. Data chunks are yielded directly whenever possible.
pub struct StreamBuffer<S> {
    pub buf: BytesMut,
    stream: Option<S>,
    eof: bool,
    polled_total: u64,
    read_until_cursor: usize,
    read_until_active: bool,
}

impl<S> StreamBuffer<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self {
            buf: BytesMut::new(),
            stream: Some(stream),
            eof: false,
            polled_total: 0,
            read_until_cursor: 0,
            read_until_active: false,
        }
    }

    /// Polls the underlying stream for the next chunk.
    pub fn poll_stream(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>> {
        let Some(stream) = self.stream.as_mut() else {
            self.eof = true;
            return Poll::Ready(None);
        };

        match ready!(Pin::new(stream).poll_next(cx)) {
            Some(Ok(bytes)) => {
                self.polled_total = self.polled_total.saturating_add(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(err)) => Poll::Ready(Some(Err(err))),
            None => {
                self.eof = true;
                Poll::Ready(None)
            }
        }
    }

    /// Appends chunks to `buf` until `needle` is present.
    ///
    /// Returns the end offset of the first `needle` occurrence. The caller
    /// owns interpreting the returned offset.
    ///
    /// `max` limits the accumulated buffer, so it also bounds the arriving
    /// chunk: a chunk that would push `buf` over `max` is rejected before it
    /// is searched, even when it contains `needle` (the bytes after the
    /// needle are retained until the caller consumes the block).
    ///
    /// Only bytes at the end of the existing buffer can combine with the next
    /// chunk to form a needle that spans chunk boundaries, so the search
    /// resumes at the previous tail (`read_until_cursor`); re-scanning the
    /// whole block on every append would make header parsing quadratic for
    /// inputs delivered as many small chunks. The cursor is reset on every
    /// `Ready`.
    ///
    /// The cursor is only meaningful while `buf` keeps its contents: a
    /// `Pending` return leaves the read active, so callers must not shrink
    /// `buf` before resuming the same read.
    pub fn poll_read_until(&mut self, needle: &[u8], max: usize, cx: &mut Context<'_>) -> Poll<Result<usize, Error>> {
        if !self.read_until_active {
            self.read_until_cursor = 0;
            self.read_until_active = true;
        }
        debug_assert!(self.read_until_cursor <= self.buf.len());

        loop {
            if let Some(rel) = memmem::find(&self.buf[self.read_until_cursor..], needle) {
                let idx = self.read_until_cursor.saturating_add(rel);
                let end = idx.saturating_add(needle.len());
                self.read_until_active = false;
                if end > max {
                    return Poll::Ready(Err(Error::HeaderSizeExceeded { limit: max }));
                }
                return Poll::Ready(Ok(end));
            }

            if self.buf.len() > max {
                self.read_until_active = false;
                return Poll::Ready(Err(Error::HeaderSizeExceeded { limit: max }));
            }

            if self.eof {
                self.read_until_active = false;
                return Poll::Ready(Err(Error::IncompleteStream));
            }

            match ready!(self.poll_stream(cx)) {
                Some(Ok(chunk)) => {
                    let next_len = self.buf.len().saturating_add(chunk.len());
                    if next_len > max {
                        self.read_until_active = false;
                        return Poll::Ready(Err(Error::HeaderSizeExceeded { limit: max }));
                    }
                    let old_len = self.buf.len();
                    self.buf.extend_from_slice(&chunk);
                    self.read_until_cursor = old_len.saturating_sub(needle.len().saturating_sub(1));
                }
                Some(Err(err)) => {
                    self.read_until_active = false;
                    return Poll::Ready(Err(err));
                }
                None => {
                    self.eof = true;
                    self.read_until_active = false;
                    return Poll::Ready(Err(Error::IncompleteStream));
                }
            }
        }
    }

    /// Discards input until `needle` appears, keeping the bytes after the
    /// match in `buf`.
    ///
    /// Used to skip the preamble before the first boundary. The unmatched
    /// prefix is discarded incrementally, so memory use is bounded by the
    /// needle length plus one chunk.
    pub fn poll_read_to(&mut self, needle: &[u8], cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            if let Some(idx) = memmem::find(&self.buf, needle) {
                let end = idx.saturating_add(needle.len());
                let _ = self.buf.split_to(end);
                return Poll::Ready(Ok(()));
            }

            if self.eof {
                return Poll::Ready(Err(Error::InvalidFormat));
            }

            match ready!(self.poll_stream(cx)) {
                Some(Ok(chunk)) => {
                    if self.buf.is_empty() {
                        if let Some(idx) = memmem::find(&chunk, needle) {
                            let end = idx.saturating_add(needle.len());
                            self.buf.extend_from_slice(&chunk[end..]);
                            return Poll::Ready(Ok(()));
                        }
                        let keep = needle.len().saturating_sub(1).min(chunk.len());
                        let cut = chunk.len().saturating_sub(keep);
                        self.buf.extend_from_slice(&chunk[cut..]);
                    } else {
                        self.buf.extend_from_slice(&chunk);
                        if let Some(idx) = memmem::find(&self.buf, needle) {
                            let end = idx.saturating_add(needle.len());
                            let _ = self.buf.split_to(end);
                            return Poll::Ready(Ok(()));
                        }
                        let keep = needle.len().saturating_sub(1).min(self.buf.len());
                        let cut = self.buf.len().saturating_sub(keep);
                        let _ = self.buf.split_to(cut);
                    }
                }
                Some(Err(err)) => return Poll::Ready(Err(err)),
                None => {
                    self.eof = true;
                    return Poll::Ready(Err(Error::InvalidFormat));
                }
            }
        }
    }

    /// Poll-based header-block reader, driving [`poll_read_until`].
    pub fn poll_read_header_block(&mut self, max: usize, cx: &mut Context<'_>) -> Poll<Result<usize, Error>> {
        if self.buf.starts_with(b"\r\n") {
            self.read_until_active = false;
            return Poll::Ready(Ok(2));
        }
        self.poll_read_until(b"\r\n\r\n", max, cx)
    }

    /// Returns the number of bytes consumed from the underlying stream that
    /// are not currently retained in `buf`.
    pub fn consumed(&self) -> u64 {
        self.polled_total.saturating_sub(self.buf.len() as u64)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    use futures::executor::block_on;
    use futures_util::stream;

    fn buffer(items: Vec<Result<Bytes, Error>>) -> StreamBuffer<impl Stream<Item = Result<Bytes, Error>> + Unpin> {
        StreamBuffer::new(stream::iter(items))
    }

    /// Drives the production [`StreamBuffer::poll_read_until`] to completion.
    async fn read_until<S>(buf: &mut StreamBuffer<S>, needle: &[u8], max: usize) -> Result<usize, Error>
    where
        S: Stream<Item = Result<Bytes, Error>> + Unpin,
    {
        std::future::poll_fn(|cx| buf.poll_read_until(needle, max, cx)).await
    }

    /// Drives the production [`StreamBuffer::poll_read_to`] to completion.
    async fn read_to<S>(buf: &mut StreamBuffer<S>, needle: &[u8]) -> Result<(), Error>
    where
        S: Stream<Item = Result<Bytes, Error>> + Unpin,
    {
        std::future::poll_fn(|cx| buf.poll_read_to(needle, cx)).await
    }

    #[test]
    fn read_until_finds_within_one_chunk() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"ab\r\n\r\nrest"))]);
            let end = read_until(&mut buf, b"\r\n\r\n", 64).await.unwrap();
            assert_eq!(end, 6);
            assert_eq!(&buf.buf[..], b"ab\r\n\r\nrest");
        });
    }

    #[test]
    fn read_until_finds_across_chunks() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"ab\r")), Ok(Bytes::from_static(b"\n\r\nrest"))]);
            let end = read_until(&mut buf, b"\r\n\r\n", 64).await.unwrap();
            assert_eq!(end, 6);
            assert_eq!(buf.consumed(), 0);
        });
    }

    #[test]
    fn read_until_finds_after_many_small_chunks() {
        block_on(async {
            let mut chunks = vec![Ok::<Bytes, Error>(Bytes::from_static(b"X-Test: "))];
            for _ in 0..64 {
                chunks.push(Ok(Bytes::from_static(b"a")));
            }
            chunks.push(Ok(Bytes::from_static(b"\r\n\r\nrest")));

            let mut buf = buffer(chunks);
            let end = read_until(&mut buf, b"\r\n\r\n", 256).await.unwrap();
            assert!(end > 10);
            assert!(buf.buf[..end].ends_with(b"\r\n\r\n"));
            assert_eq!(&buf.buf[end..], b"rest");
        });
    }

    #[test]
    fn read_until_respects_header_limit() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"aaaa")), Ok(Bytes::from_static(b"bbbb"))]);
            let err = read_until(&mut buf, b"zz", 6).await.unwrap_err();
            assert!(matches!(err, Error::HeaderSizeExceeded { limit: 6 }));
        });
    }

    #[test]
    fn read_until_reports_incomplete_on_eof() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"abc"))]);
            let err = read_until(&mut buf, b"zz", 64).await.unwrap_err();
            assert!(matches!(err, Error::IncompleteStream));
        });
    }

    #[test]
    fn read_to_skips_preamble_and_keeps_tail() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"preamble\r\n--boundary\r\nContent-Type: x"))]);
            read_to(&mut buf, b"--boundary").await.unwrap();
            assert_eq!(&buf.buf[..], b"\r\nContent-Type: x");
            assert!(buf.consumed() > 0);
        });
    }

    #[test]
    fn read_to_handles_cross_chunk_needle() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"aaaa--boun")), Ok(Bytes::from_static(b"dary\r\nrest"))]);
            read_to(&mut buf, b"--boundary").await.unwrap();
            assert_eq!(&buf.buf[..], b"\r\nrest");
        });
    }

    #[test]
    fn read_to_reports_invalid_format_on_eof() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"junk"))]);
            let err = read_to(&mut buf, b"--boundary").await.unwrap_err();
            assert!(matches!(err, Error::InvalidFormat));
        });
    }

    #[test]
    fn underlying_error_is_propagated() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"a")), Err(Error::InvalidFormat)]);
            let err = read_until(&mut buf, b"zz", 64).await.unwrap_err();
            assert!(matches!(err, Error::InvalidFormat));
        });
    }

    #[test]
    fn poll_stream_after_stream_taken_is_none() {
        let mut buf = buffer(Vec::new());
        buf.stream = None;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(buf.poll_stream(&mut cx), Poll::Ready(None)));
        assert!(buf.eof);
    }

    #[test]
    fn read_until_after_eof_reports_incomplete() {
        block_on(async {
            let mut buf = buffer(Vec::new());
            assert!(matches!(read_until(&mut buf, b"zz", 64).await, Err(Error::IncompleteStream)));
            assert!(matches!(read_until(&mut buf, b"zz", 64).await, Err(Error::IncompleteStream)));
        });
    }

    #[test]
    fn read_to_with_buffered_needle_and_no_match_discard() {
        block_on(async {
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"more-junk"))]);
            buf.buf.extend_from_slice(b"--boun");
            assert!(matches!(read_to(&mut buf, b"--boundary").await, Err(Error::InvalidFormat)));

            let mut buf = buffer(Vec::new());
            buf.buf.extend_from_slice(b"pre--boundary\r\ntail");
            read_to(&mut buf, b"--boundary").await.unwrap();
            assert_eq!(&buf.buf[..], b"\r\ntail");
        });
    }

    #[test]
    fn read_to_after_eof_reports_invalid_format() {
        block_on(async {
            let mut buf = buffer(Vec::new());
            buf.eof = true;
            assert!(matches!(read_to(&mut buf, b"--boundary").await, Err(Error::InvalidFormat)));
        });
    }

    #[test]
    fn read_until_initial_buf_over_limit_without_needle() {
        block_on(async {
            let mut buf = buffer(Vec::new());
            buf.buf.extend_from_slice(b"aaaa");
            assert!(matches!(
                read_until(&mut buf, b"zz", 2).await,
                Err(Error::HeaderSizeExceeded { limit: 2 })
            ));
        });
    }

    #[test]
    fn read_until_accepts_a_block_of_exactly_max_bytes() {
        block_on(async {
            // The accumulated buffer reaches `max` (6) exactly when the needle
            // is complete; `max` is inclusive.
            let mut buf = buffer(vec![
                Ok(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"\r\n")),
                Ok(Bytes::from_static(b"\r\n")),
            ]);
            assert_eq!(read_until(&mut buf, b"\r\n\r\n", 6).await.unwrap(), 6);
            assert_eq!(&buf.buf[..], b"ab\r\n\r\n");
        });
    }

    #[test]
    fn read_until_at_exactly_max_without_needle_reports_eof() {
        block_on(async {
            // The accumulated bytes are exactly `max`: the limit is not
            // exceeded, so the read continues and reports end of stream.
            let mut buf = buffer(Vec::new());
            buf.buf.extend_from_slice(b"abcdef");
            assert!(matches!(read_until(&mut buf, b"zz", 6).await, Err(Error::IncompleteStream)));
        });
    }

    #[test]
    fn read_until_rejects_an_oversized_chunk_before_searching_it() {
        block_on(async {
            // The limit also bounds the arriving chunk: the chunk that would
            // complete the header block is rejected before it is searched, so
            // nothing of it is retained.
            let mut buf = buffer(vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"\r\n\r\nrest"))]);
            assert!(matches!(
                read_until(&mut buf, b"\r\n\r\n", 6).await,
                Err(Error::HeaderSizeExceeded { limit: 6 })
            ));
            assert_eq!(&buf.buf[..], b"ab");
        });
    }

    #[test]
    fn read_until_resets_the_cursor_between_reads() {
        block_on(async {
            let mut buf = buffer(vec![
                Ok(Bytes::from_static(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
                Ok(Bytes::from_static(b"\r\n\r\n")),
                Ok(Bytes::from_static(b"\r\n\r\nrest")),
            ]);
            assert_eq!(read_until(&mut buf, b"\r\n\r\n", 64).await.unwrap(), 34);

            // The parser drains a finished header block before the next read,
            // so the cursor from the first read must not leak into the second.
            let _ = buf.buf.split_to(34);
            assert_eq!(read_until(&mut buf, b"\r\n\r\n", 64).await.unwrap(), 4);
        });
    }
}

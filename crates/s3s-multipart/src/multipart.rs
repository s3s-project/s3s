// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::buffer::StreamBuffer;
use crate::delimiter::{DataSearch, make_delimiter_finder, make_first_boundary, search_data};
use crate::header::HeaderBlock;
use crate::part::Part;
use crate::part_data_stream::PartDataStream;
use crate::{Boundary, Error};

use std::future::poll_fn;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;
use memchr::memmem;

/// A streaming parser for `multipart/form-data`.
///
/// The parser owns the underlying stream and yields one [`Part`] at a time.
/// At most one `Part` is active at any moment; the borrow checker enforces
/// this property at compile time.
///
/// The stream has to be `Send + Unpin`, but not `Sync`: the parser only ever
/// polls it through `&mut`, so it never needs shared access, and requiring
/// `Sync` would turn away streams that can be moved between threads but not
/// shared between them.
pub struct Multipart<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    /// `None` only after [`Multipart::take_data_stream`] handed the buffer to a
    /// [`PartDataStream`], which also moves the state to `StreamTaken`. Both
    /// public entry points check that state first, so the
    /// `Err(Error::StreamAlreadyTaken)` arms in the poll steps below are guards
    /// rather than reachable outcomes.
    buffer: Option<StreamBuffer<S>>,
    first_boundary: Box<[u8]>,
    /// Taken (not cloned) by [`Multipart::take_data_stream`]: `Some` while the
    /// parser may still read part data, `None` once the stream is handed over.
    /// Boxing keeps the parser compact — the finder carries a 288-byte searcher.
    delimiter_finder: Option<Box<memmem::Finder<'static>>>,
    pub(super) max_buffer_size: usize,
    pub(super) state: State,
    pub(super) headers: Option<HeaderBlock>,
}

impl<S> Multipart<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    /// Constructs a parser.
    ///
    /// `max_buffer_size` limits the bytes accumulated in the internal buffer
    /// (part headers and boundary residue). Data chunks yielded by `stream`
    /// while part data is being streamed are passed through without being
    /// retained, so they are not charged to this limit — with one exception:
    /// while a header block is still being read, the limit also bounds the
    /// arriving chunk (the bytes after the header terminator are retained
    /// until the header block is consumed), so a single chunk larger than
    /// `max_buffer_size` is rejected at that point.
    pub fn new(stream: S, boundary: &Boundary, max_buffer_size: usize) -> Self {
        let first_boundary = make_first_boundary(boundary.as_bytes());
        let delimiter_finder = make_delimiter_finder(boundary.as_bytes());
        Self {
            buffer: Some(StreamBuffer::new(stream)),
            first_boundary,
            delimiter_finder: Some(delimiter_finder),
            max_buffer_size,
            state: State::FindingFirstBoundary,
            headers: None,
        }
    }

    /// Returns the next part, or `None` after the closing delimiter.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed input, stream errors, or when the
    /// multipart stream has already been taken by a part data stream.
    pub async fn next_part(&mut self) -> Result<Option<Part<'_, S>>, Error> {
        loop {
            match self.state {
                State::StreamTaken => return Err(Error::StreamAlreadyTaken),
                State::Done => return Ok(None),
                State::FindingFirstBoundary => {
                    poll_fn(|cx| self.poll_advance_first_boundary(cx)).await?;
                }
                State::ReadingPartHeaders => return Ok(Some(Part::new(self))),
                State::ReadingPartData => {
                    poll_fn(|cx| self.poll_skip_part_data(cx)).await?;
                    poll_fn(|cx| self.poll_advance_after_boundary(cx)).await?;
                }
                State::ReadingBoundary => {
                    poll_fn(|cx| self.poll_advance_after_boundary(cx)).await?;
                }
                State::ReadingClosingDelimiter => {
                    poll_fn(|cx| self.poll_advance_closing(cx)).await?;
                }
                State::ReadingEpilogue => {
                    poll_fn(|cx| self.poll_epilogue(cx)).await?;
                }
            }
        }
    }

    /// Poll-based core of the state machine, driven by [`Multipart::next_part`].
    pub fn poll_next_part(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Part<'_, S>>, Error>> {
        loop {
            match self.state {
                State::StreamTaken => return Poll::Ready(Err(Error::StreamAlreadyTaken)),
                State::Done => return Poll::Ready(Ok(None)),
                State::FindingFirstBoundary => {
                    ready!(self.poll_advance_first_boundary(cx)?);
                }
                State::ReadingPartHeaders => return Poll::Ready(Ok(Some(Part::new(self)))),
                State::ReadingPartData => {
                    ready!(self.poll_skip_part_data(cx)?);
                    ready!(self.poll_advance_after_boundary(cx)?);
                }
                State::ReadingBoundary => {
                    ready!(self.poll_advance_after_boundary(cx)?);
                }
                State::ReadingClosingDelimiter => {
                    ready!(self.poll_advance_closing(cx)?);
                }
                State::ReadingEpilogue => {
                    ready!(self.poll_epilogue(cx)?);
                }
            }
        }
    }

    /// Consumes the leading `--boundary`. Only this step runs here: once the
    /// needle is gone the caller loops and re-matches the state, so a `Pending`
    /// after the state moved on cannot re-run the search (which would look for
    /// the *next* boundary and silently skip the part in between).
    fn poll_advance_first_boundary(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // Borrow the fields instead of cloning `first_boundary` into a local:
        // the needle is a `Box<[u8]>`, so a clone would allocate on every poll
        // of this step. Destructuring splits the borrows, which `buffer_mut()`
        // (a `&mut self` borrow) cannot do for us.
        let Self {
            buffer, first_boundary, ..
        } = self;
        let Some(buffer) = buffer.as_mut() else {
            return Poll::Ready(Err(Error::StreamAlreadyTaken));
        };
        ready!(buffer.poll_read_to(first_boundary, cx)?);
        self.state = State::ReadingBoundary;
        Poll::Ready(Ok(()))
    }

    fn poll_advance_after_boundary(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        ready!(self.poll_ensure_buf_len(1, cx)?);
        let first = match self.buffer_mut() {
            Ok(buffer) => buffer.buf[0],
            Err(err) => return Poll::Ready(Err(err)),
        };
        match first {
            b'-' => {
                ready!(self.poll_ensure_buf_len(2, cx)?);
                let bytes = match self.buffer_mut() {
                    Ok(buffer) => &buffer.buf,
                    Err(err) => return Poll::Ready(Err(err)),
                };
                if bytes.get(..2) != Some(&b"--"[..]) {
                    return Poll::Ready(Err(Error::InvalidFormat));
                }
                if let Ok(buffer) = self.buffer_mut() {
                    let _ = buffer.buf.split_to(2);
                }
                self.state = State::ReadingClosingDelimiter;
                Poll::Ready(Ok(()))
            }
            b' ' | b'\t' => {
                ready!(self.poll_consume_padding_and_crlf(cx)?);
                self.state = State::ReadingPartHeaders;
                Poll::Ready(Ok(()))
            }
            b'\r' => {
                ready!(self.poll_consume_crlf(cx)?);
                self.state = State::ReadingPartHeaders;
                Poll::Ready(Ok(()))
            }
            _ => Poll::Ready(Err(Error::InvalidFormat)),
        }
    }

    /// Consumes the closing delimiter's padding and CRLF, then hands the strict
    /// trailer check to [`Multipart::poll_epilogue`]. Keeping the epilogue in
    /// its own state is what makes the resume safe: re-running this step after a
    /// `Pending` would consume a CRLF that is already gone and then wait for
    /// bytes that never come.
    fn poll_advance_closing(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        ready!(self.poll_consume_padding_and_crlf(cx)?);
        self.state = State::ReadingEpilogue;
        Poll::Ready(Ok(()))
    }

    /// Strict trailer: after the closing delimiter only end of stream may
    /// follow, with the closing CRLF already consumed.
    fn poll_epilogue(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let buffer = match self.buffer_mut() {
            Ok(buffer) => buffer,
            Err(err) => return Poll::Ready(Err(err)),
        };
        if !buffer.buf.is_empty() {
            return Poll::Ready(Err(Error::StreamPartNotLast));
        }
        match ready!(buffer.poll_stream(cx)) {
            None => {
                self.state = State::Done;
                Poll::Ready(Ok(()))
            }
            // A chunk that carries no bytes is not trailing content: leave the
            // state alone so the caller polls this step again.
            Some(Ok(chunk)) if chunk.is_empty() => Poll::Ready(Ok(())),
            Some(Ok(_)) => Poll::Ready(Err(Error::StreamPartNotLast)),
            Some(Err(err)) => Poll::Ready(Err(err)),
        }
    }

    fn poll_consume_padding_and_crlf(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            ready!(self.poll_ensure_buf_len(1, cx)?);
            let first = match self.buffer_mut() {
                Ok(buffer) => buffer.buf[0],
                Err(err) => return Poll::Ready(Err(err)),
            };
            if first == b'\r' {
                return self.poll_consume_crlf(cx);
            }
            if !matches!(first, b' ' | b'\t') {
                return Poll::Ready(Err(Error::InvalidFormat));
            }

            let padding_len = match self.buffer_mut() {
                Ok(buffer) => buffer.buf.iter().take_while(|&&b| matches!(b, b' ' | b'\t')).count(),
                Err(err) => return Poll::Ready(Err(err)),
            };
            if let Ok(buffer) = self.buffer_mut() {
                let _ = buffer.buf.split_to(padding_len);
            }
        }
    }

    fn poll_consume_crlf(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        ready!(self.poll_ensure_buf_len(2, cx)?);
        let bytes = match self.buffer_mut() {
            Ok(buffer) => &buffer.buf,
            Err(err) => return Poll::Ready(Err(err)),
        };
        if bytes.get(..2) != Some(&b"\r\n"[..]) {
            return Poll::Ready(Err(Error::InvalidFormat));
        }
        if let Ok(buffer) = self.buffer_mut() {
            let _ = buffer.buf.split_to(2);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_ensure_buf_len(&mut self, len: usize, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        while match self.buffer_mut() {
            Ok(buffer) => buffer.buf.len() < len,
            Err(err) => return Poll::Ready(Err(err)),
        } {
            let buffer = match self.buffer_mut() {
                Ok(buffer) => buffer,
                Err(err) => return Poll::Ready(Err(err)),
            };
            match ready!(buffer.poll_stream(cx)) {
                Some(Ok(chunk)) => {
                    if let Ok(buffer) = self.buffer_mut() {
                        buffer.buf.extend_from_slice(&chunk);
                    }
                }
                Some(Err(err)) => return Poll::Ready(Err(err)),
                None => return Poll::Ready(Err(Error::IncompleteStream)),
            }
        }
        Poll::Ready(Ok(()))
    }

    pub(super) fn poll_next_part_data_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, Error>> {
        if self.state != State::ReadingPartData {
            return Poll::Ready(Ok(None));
        }

        // While `ReadingPartData` the finder is always present: only
        // `take_data_stream` clears it, and that also moves the state away.
        let Some(delimiter_finder) = self.delimiter_finder.as_deref() else {
            return Poll::Ready(Ok(None));
        };
        let delimiter_len = delimiter_finder.needle().len();

        loop {
            {
                let Some(buffer) = self.buffer.as_mut() else {
                    return Poll::Ready(Err(Error::StreamAlreadyTaken));
                };
                if !buffer.buf.is_empty() {
                    match search_data(&buffer.buf, delimiter_finder) {
                        DataSearch::Found { index } => {
                            let data = buffer.buf.split_to(index).freeze();
                            let _ = buffer.buf.split_to(delimiter_len);
                            self.state = State::ReadingBoundary;
                            return if data.is_empty() {
                                Poll::Ready(Ok(None))
                            } else {
                                Poll::Ready(Ok(Some(data)))
                            };
                        }
                        DataSearch::Emit { end } => {
                            let data = buffer.buf.split_to(end).freeze();
                            return Poll::Ready(Ok(Some(data)));
                        }
                        DataSearch::KeepAll => {}
                    }
                }
            }

            let Some(buffer) = self.buffer.as_mut() else {
                return Poll::Ready(Err(Error::StreamAlreadyTaken));
            };
            match ready!(buffer.poll_stream(cx)) {
                Some(Ok(chunk)) => {
                    let Some(buffer) = self.buffer.as_mut() else {
                        return Poll::Ready(Err(Error::StreamAlreadyTaken));
                    };
                    if !buffer.buf.is_empty() {
                        buffer.buf.extend_from_slice(&chunk);
                        continue;
                    }

                    match search_data(&chunk, delimiter_finder) {
                        DataSearch::Found { index } => {
                            let data = chunk.slice(..index);
                            buffer.buf.clear();
                            buffer.buf.extend_from_slice(&chunk[index.saturating_add(delimiter_len)..]);
                            self.state = State::ReadingBoundary;
                            return if data.is_empty() {
                                Poll::Ready(Ok(None))
                            } else {
                                Poll::Ready(Ok(Some(data)))
                            };
                        }
                        DataSearch::Emit { end } => {
                            let data = chunk.slice(..end);
                            buffer.buf.extend_from_slice(&chunk[end..]);
                            return Poll::Ready(Ok(Some(data)));
                        }
                        DataSearch::KeepAll => {
                            buffer.buf.extend_from_slice(&chunk);
                        }
                    }
                }
                Some(Err(err)) => return Poll::Ready(Err(err)),
                None => return Poll::Ready(Err(Error::IncompleteStream)),
            }
        }
    }

    fn poll_skip_part_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            match ready!(self.poll_next_part_data_chunk(cx)) {
                Ok(Some(_)) => {}
                Ok(None) => return Poll::Ready(Ok(())),
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    pub(super) fn take_data_stream(&mut self) -> Result<PartDataStream<S>, Error> {
        if self.state == State::ReadingPartHeaders {
            self.finish_headers();
        }
        if self.state != State::ReadingPartData {
            return Err(Error::InvalidFormat);
        }

        let Some(buffer) = self.buffer.take() else {
            return Err(Error::StreamAlreadyTaken);
        };
        // Taken, not cloned: after the handover the parser cannot read part data
        // any more, so `PartDataStream` may as well own the finder. Reaching the
        // `else` arm would need a buffer without a finder, which nothing creates
        // (both are cleared together, after the state guard above).
        let Some(delimiter_finder) = self.delimiter_finder.take() else {
            return Err(Error::StreamAlreadyTaken);
        };
        let multipart_consumed = buffer.consumed();
        self.state = State::StreamTaken;

        Ok(PartDataStream::new(buffer, delimiter_finder, multipart_consumed))
    }

    pub(super) fn buffer_mut(&mut self) -> Result<&mut StreamBuffer<S>, Error> {
        self.buffer.as_mut().ok_or(Error::StreamAlreadyTaken)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// State of the parser state machine.
///
/// The module is private and `State` is not re-exported, so `pub` cannot make
/// it reachable from outside the crate.
pub enum State {
    FindingFirstBoundary,
    ReadingPartHeaders,
    ReadingPartData,
    ReadingBoundary,
    ReadingClosingDelimiter,
    /// Closing delimiter consumed; only end of stream may follow.
    ReadingEpilogue,
    Done,
    StreamTaken,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
mod tests {
    use super::*;

    use futures::executor::block_on;
    use futures_util::StreamExt;
    use futures_util::stream;

    fn parser(payload: &'static [u8]) -> Multipart<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        Multipart::new(
            stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(payload))])
                .map(|item| item.map_err(Error::stream_read_failed)),
            &Boundary::new(b"boundary").unwrap(),
            1024,
        )
    }

    fn chunked_parser(chunks: Vec<&'static [u8]>) -> Multipart<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        Multipart::new(
            stream::iter(chunks.into_iter().map(|c| Ok::<Bytes, std::io::Error>(Bytes::from_static(c))))
                .map(|item| item.map_err(Error::stream_read_failed)),
            &Boundary::new(b"boundary").unwrap(),
            1024,
        )
    }

    fn owned_chunk_parser(chunks: Vec<Vec<u8>>) -> Multipart<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        Multipart::new(
            stream::iter(chunks.into_iter().map(|c| Ok::<Bytes, std::io::Error>(Bytes::from(c))))
                .map(|item| item.map_err(Error::stream_read_failed)),
            &Boundary::new(b"boundary").unwrap(),
            1024,
        )
    }

    async fn drain_part<S>(part: &mut Part<'_, S>) -> (Vec<(Vec<u8>, Vec<u8>)>, Vec<u8>)
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
    {
        let mut headers = Vec::new();
        while let Some(header) = part.next_header().await.unwrap() {
            headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
        }
        let mut data = Vec::new();
        while let Some(chunk) = part.next_data().await.unwrap() {
            data.extend_from_slice(&chunk);
        }
        (headers, data)
    }

    #[test]
    fn parses_preamble_headers_data_and_close() {
        block_on(async {
            let mut mp = parser(b"preamble\r\n--boundary\r\nX-Test: one\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary--\r\n");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (headers, data) = drain_part(&mut part).await;
            assert_eq!(headers[0].0, b"X-Test");
            assert_eq!(headers[0].1, b"one");
            assert_eq!(headers[1].0, b"Content-Disposition");
            assert_eq!(data, b"hello");
            assert!(mp.next_part().await.unwrap().is_none());
        });
    }

    /// An empty header block is terminated by the blank line that follows the
    /// boundary line, so the parse must not depend on where the transport split
    /// the body: the block ends two bytes into the header area no matter which
    /// chunk those bytes arrive in.
    #[test]
    fn empty_header_block_parses_at_every_split_position() {
        block_on(async {
            for (body, expected) in [
                (&b"--boundary\r\n\r\nDATA\r\n--boundary--\r\n"[..], vec![(0usize, &b"DATA"[..])]),
                (
                    // Same shape on the second part, reached through the boundary skip.
                    &b"--boundary\r\nX: y\r\n\r\nA\r\n--boundary\r\n\r\nDATA\r\n--boundary--\r\n"[..],
                    vec![(1usize, &b"A"[..]), (0usize, &b"DATA"[..])],
                ),
            ] {
                let want: Vec<(usize, Vec<u8>)> = expected.iter().map(|(n, data)| (*n, data.to_vec())).collect();
                // Every split, including the degenerate ones that hand the parser
                // an empty chunk: this is the shape a streaming transport
                // produces, and the shape that used to fail whenever the boundary
                // line ended the first chunk.
                for split in 0..=body.len() {
                    let mut mp = owned_chunk_parser(vec![body[..split].to_vec(), body[split..].to_vec()]);
                    let mut seen: Vec<(usize, Vec<u8>)> = Vec::new();
                    while let Some(mut part) = mp
                        .next_part()
                        .await
                        .unwrap_or_else(|err| panic!("next_part failed at split {split}: {err}"))
                    {
                        let (headers, data) = drain_part(&mut part).await;
                        seen.push((headers.len(), data));
                    }
                    assert_eq!(seen, want, "parts at split {split}");
                }
            }
        });
    }

    /// A stream may hand out chunks that carry no bytes. They are not content,
    /// so they must not be mistaken for an epilogue after the closing delimiter.
    #[test]
    fn empty_chunks_after_the_closing_delimiter_are_not_an_epilogue() {
        block_on(async {
            let mut mp = owned_chunk_parser(vec![b"--boundary\r\n\r\nDATA\r\n--boundary--\r\n".to_vec(), Vec::new(), Vec::new()]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            assert!(part.next_header().await.unwrap().is_none());
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, b"DATA");
            assert!(mp.next_part().await.unwrap().is_none());
        });
    }

    /// A stream that never stops answering with empty chunks is not progress:
    /// the parser must report a stream failure instead of polling it forever,
    /// whether the run happens while reading the body or while checking for an
    /// epilogue.
    #[test]
    fn an_endless_run_of_empty_chunks_is_a_stream_failure() {
        struct BodyThenEmptyForever {
            body: Option<Bytes>,
        }

        impl Stream for BodyThenEmptyForever {
            type Item = Result<Bytes, Error>;

            fn poll_next(mut self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                match self.body.take() {
                    Some(body) => Poll::Ready(Some(Ok(body))),
                    None => Poll::Ready(Some(Ok(Bytes::new()))),
                }
            }
        }

        block_on(async {
            let mut mp = Multipart::new(
                BodyThenEmptyForever {
                    body: Some(Bytes::from_static(b"--boundary\r\n\r\nDATA\r\n--boundary--\r\n")),
                },
                &Boundary::new(b"boundary").unwrap(),
                4096,
            );
            let mut part = mp.next_part().await.unwrap().unwrap();
            assert!(part.next_header().await.unwrap().is_none());
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, b"DATA");
            assert!(matches!(mp.next_part().await, Err(Error::StreamReadFailed(_))));
        });

        block_on(async {
            // The same run while the header block is still incomplete.
            let mut mp = Multipart::new(
                BodyThenEmptyForever {
                    body: Some(Bytes::from_static(b"--boundary\r\nA: b\r\n")),
                },
                &Boundary::new(b"boundary").unwrap(),
                4096,
            );
            let mut part = mp.next_part().await.unwrap().unwrap();
            assert!(matches!(part.next_header().await, Err(Error::StreamReadFailed(_))));
        });
    }

    #[test]
    fn skips_unread_part_data_when_dropped() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\n\r\nskipped data\r\n--boundary\r\nX: y\r\n\r\nnext\r\n--boundary--\r\n");
            {
                let mut part = mp.next_part().await.unwrap().unwrap();
                let header = part.next_header().await.unwrap();
                assert!(header.is_none());
                // drop without reading data
            }
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (headers, data) = drain_part(&mut part).await;
            assert_eq!(headers[0].1, b"y");
            assert_eq!(data, b"next");
        });
    }

    #[test]
    fn handles_boundary_split_across_chunks() {
        block_on(async {
            let mut mp = chunked_parser(vec![
                b"--boundary\r\n\r\nabc\r\n--boun",
                b"dary\r\nX: y\r\n\r\nz\r\n--boundary--\r\n",
            ]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (_, data) = drain_part(&mut part).await;
            assert_eq!(data, b"abc");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (_, data) = drain_part(&mut part).await;
            assert_eq!(data, b"z");
        });
    }

    #[test]
    fn accepts_transport_padding_after_boundary() {
        block_on(async {
            let mut mp = parser(b"--boundary \t\r\nX: y\r\n\r\ndata\r\n--boundary  \r\n\r\nnext\r\n--boundary--\t\r\n");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (_, data) = drain_part(&mut part).await;
            assert_eq!(data, b"data");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (_, data) = drain_part(&mut part).await;
            assert_eq!(data, b"next");
            assert!(mp.next_part().await.unwrap().is_none());
        });
    }

    #[test]
    fn accepts_empty_header_and_empty_body_parts() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\n\r\n\r\n--boundary\r\nX: y\r\n\r\n\r\n--boundary--\r\n");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (headers, data) = drain_part(&mut part).await;
            assert_eq!(headers.len(), 0);
            assert_eq!(data, b"");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let (headers, data) = drain_part(&mut part).await;
            assert_eq!(headers[0].1, b"y");
            assert_eq!(data, b"");
        });
    }

    #[test]
    fn rejects_epilogue_after_close() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\nepilogue");
            let mut part = mp.next_part().await.unwrap().unwrap();
            let _ = drain_part(&mut part).await;
            let err = mp.next_part().await.unwrap_err();
            assert!(matches!(err, Error::StreamPartNotLast));
        });
    }

    #[test]
    fn rejects_missing_first_boundary() {
        block_on(async {
            let mut mp = parser(b"not multipart");
            let err = mp.next_part().await.unwrap_err();
            assert!(matches!(err, Error::InvalidFormat));
        });
    }

    #[test]
    fn enforces_header_size_limit() {
        block_on(async {
            let mut mp = Multipart::new(
                stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(b"--boundary\r\nX: y\r\n\r\n"))])
                    .map(|item| item.map_err(Error::stream_read_failed)),
                &Boundary::new(b"boundary").unwrap(),
                4,
            );
            let mut part = mp.next_part().await.unwrap().unwrap();
            let err = part.next_header().await.unwrap_err();
            assert!(matches!(err, Error::HeaderSizeExceeded { limit: 4 }));
        });
    }

    async fn collect_taken<St>(mut stream: St) -> Result<Vec<u8>, Error>
    where
        St: Stream<Item = Result<Bytes, Error>> + Unpin,
    {
        let mut data = Vec::new();
        while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
            data.extend_from_slice(&item?);
        }
        Ok(data)
    }

    #[test]
    fn take_data_stream_yields_data_and_validates_trailer() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n");
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let mut stream = part.take_data_stream().unwrap();
            let snapshot = stream.multipart_consumed();
            assert!(snapshot > 0);
            let data = collect_taken(&mut stream).await.unwrap();
            assert_eq!(data, b"hello");
            let final_stream = stream.into_final();
            assert!(collect_taken(final_stream).await.is_ok());
            assert!(matches!(mp.next_part().await, Err(Error::StreamAlreadyTaken)));
        });
    }

    #[test]
    fn take_data_stream_rejects_epilogue() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\nepilogue");
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            let err = collect_taken(stream.into_final()).await.unwrap_err();
            assert!(matches!(err, Error::StreamPartNotLast));
        });
    }

    #[test]
    fn take_data_stream_rejects_second_part() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary\r\nX: z\r\n\r\nworld\r\n--boundary--\r\n");
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            let err = collect_taken(stream.into_final()).await.unwrap_err();
            assert!(matches!(err, Error::StreamPartNotLast));
        });
    }

    #[test]
    fn take_data_stream_reports_incomplete_trailer() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--");
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            let err = collect_taken(stream.into_final()).await.unwrap_err();
            assert!(matches!(err, Error::IncompleteStreamPart));
        });
    }

    /// The precondition is that the header block has been parsed, not that every
    /// header has been yielded: dropping the unread ones is allowed and covered
    /// in `part.rs`. Without any `next_header` call at all the take is refused.
    #[test]
    fn take_data_stream_requires_a_parsed_header_block() {
        block_on(async {
            let mut mp = parser(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n");
            let part = mp.next_part().await.unwrap().unwrap();
            assert!(matches!(part.take_data_stream(), Err(Error::InvalidFormat)));
        });
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
mod coverage_tests {
    use super::*;

    use crate::delimiter::make_delimiter;
    use crate::header::MAX_HEADERS;

    use std::io;

    use futures::executor::block_on;
    use futures_util::StreamExt;
    use futures_util::stream;

    fn parser_from_chunks(
        chunks: Vec<Result<&'static [u8], io::Error>>,
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

    fn ok_chunks(chunks: Vec<&'static [u8]>) -> Multipart<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        parser_from_chunks(chunks.into_iter().map(Ok).collect())
    }

    async fn collect_taken<St>(mut stream: St) -> Result<Vec<u8>, Error>
    where
        St: Stream<Item = Result<Bytes, Error>> + Unpin,
    {
        let mut data = Vec::new();
        while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
            data.extend_from_slice(&item?);
        }
        Ok(data)
    }

    #[test]
    fn next_part_drives_closing_state() {
        block_on(async {
            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_data().await.unwrap().is_some() {}
            let _ = part;
            assert!(mp.next_part().await.unwrap().is_none());
        });
    }

    #[test]
    fn closing_trailer_error_paths() {
        block_on(async {
            for body in [
                &b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary-x\r\n"[..],
                b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundaryX\r\n",
                b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--x\r\n",
            ] {
                let mut mp = parser_from_chunks(vec![Ok(body)]);
                let mut part = mp.next_part().await.unwrap().unwrap();
                while part.next_data().await.unwrap().is_some() {}
                let _ = part;
                assert!(matches!(mp.next_part().await, Err(Error::InvalidFormat)), "{body:?}");
            }

            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\rx")]);
            assert!(matches!(mp.next_part().await, Err(Error::InvalidFormat)));
        });
    }

    #[test]
    fn closing_poll_sees_separate_epilogue_and_errors() {
        block_on(async {
            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n"), Ok(b"epilogue")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_data().await.unwrap().is_some() {}
            let _ = part;
            assert!(matches!(mp.next_part().await, Err(Error::StreamPartNotLast)));

            let mut mp = parser_from_chunks(vec![
                Ok(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n"),
                Err(io::Error::other("boom")),
            ]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_data().await.unwrap().is_some() {}
            let _ = part;
            assert!(matches!(mp.next_part().await, Err(Error::StreamReadFailed(_))));
        });
    }

    #[test]
    fn first_boundary_ensure_buf_cross_chunk_and_eof() {
        block_on(async {
            let mut mp = ok_chunks(vec![b"--boundary", b"\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, b"data");

            let mut mp = ok_chunks(vec![b"--boundary"]);
            assert!(matches!(mp.next_part().await, Err(Error::IncompleteStream)));
        });
    }

    #[test]
    fn next_header_after_data_returns_none() {
        block_on(async {
            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\ndata\r\n--boundary--\r\n")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let _ = part.next_data().await.unwrap();
            assert!(part.next_header().await.unwrap().is_none());
        });
    }

    #[test]
    fn next_data_direct_chunk_paths() {
        block_on(async {
            let mut mp = ok_chunks(vec![
                b"--boundary\r\n\r\n",
                b"abcdefghijklmnopqrstuvwxyz0123456789",
                b"more-data\r\n--boundary--\r\n",
            ]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                data.extend_from_slice(&chunk);
            }
            assert!(data.starts_with(b"abcdefghijklmnopqrstuvwxyz0123456789more-data"));

            let mut mp = ok_chunks(vec![b"--boundary\r\n\r\n", b"\r\n--boundary\r\nX: y\r\n\r\nz\r\n--boundary--\r\n"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            // An empty part yields no data item at all: the delimiter is already
            // at the front, so there is nothing to hand out.
            assert!(part.next_data().await.unwrap().is_none());

            let mut mp = ok_chunks(vec![b"--boundary\r\nX: y\r\n\r\n", b"abc"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            assert!(matches!(part.next_data().await, Err(Error::IncompleteStream)));

            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\n"), Err(io::Error::other("boom"))]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            assert!(matches!(part.next_data().await, Err(Error::StreamReadFailed(_))));
        });
    }

    #[test]
    fn double_take_is_rejected() {
        block_on(async {
            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let _ = part.take_data_stream().unwrap();
            mp.state = State::ReadingPartData;
            mp.buffer = None;
            assert!(matches!(mp.take_data_stream(), Err(Error::StreamAlreadyTaken)));
        });
    }

    #[test]
    fn debug_impls_render() {
        block_on(async {
            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            assert!(format!("{part:?}").contains("Part"));
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert!(format!("{stream:?}").contains("PartDataStream"));
            assert_eq!(stream.size_hint(), (0, None));
        });
    }

    #[test]
    fn part_data_stream_poll_paths() {
        block_on(async {
            let mut mp = ok_chunks(vec![
                b"--boundary\r\nX: y\r\n\r\n",
                b"abcdefghijklmnopqrstuvwxyz0123456789",
                b"rest\r\n--boundary--\r\n",
            ]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            let data = collect_taken(stream).await.unwrap();
            assert!(data.starts_with(b"abcdefghijklmnopqrstuvwxyz0123456789rest"));

            let mut mp = ok_chunks(vec![b"--boundary\r\nX: y\r\n\r\n", b"\r\n--boundary--\r\n"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            let mut chunks = 0;
            let mut stream = Box::pin(stream);
            while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                item.unwrap();
                chunks += 1;
            }
            assert_eq!(chunks, 0);

            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\n"), Err(io::Error::other("boom"))]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert!(matches!(collect_taken(stream.into_final()).await, Err(Error::StreamReadFailed(_))));

            let mut mp = ok_chunks(vec![b"--boundary\r\nX: y\r\n\r\nabc"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert!(matches!(collect_taken(stream).await, Err(Error::IncompleteStreamPart)));
        });
    }

    #[test]
    fn part_data_stream_trailer_paths() {
        block_on(async {
            for body in [
                &b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary-x\r\n"[..],
                b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--x\r\n",
            ] {
                let mut mp = parser_from_chunks(vec![Ok(body)]);
                let mut part = mp.next_part().await.unwrap().unwrap();
                while part.next_header().await.unwrap().is_some() {}
                let stream = part.take_data_stream().unwrap();
                assert!(matches!(collect_taken(stream.into_final()).await, Err(Error::InvalidFormat)), "{body:?}");
            }

            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary-- \t\r\n")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert_eq!(collect_taken(stream.into_final()).await.unwrap(), b"hello");

            let mut mp = parser_from_chunks(vec![Ok(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n"), Ok(b"epilogue")]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert!(matches!(collect_taken(stream.into_final()).await, Err(Error::StreamPartNotLast)));

            let mut mp = parser_from_chunks(vec![
                Ok(b"--boundary\r\nX: y\r\n\r\nhello\r\n--boundary--\r\n"),
                Err(io::Error::other("boom")),
            ]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let stream = part.take_data_stream().unwrap();
            assert!(matches!(collect_taken(stream.into_final()).await, Err(Error::StreamReadFailed(_))));
        });
    }

    #[test]
    fn next_data_yields_buffered_data_prefix() {
        block_on(async {
            let mut mp = ok_chunks(vec![b"--boundary\r\nX: y\r\n\r\n0123456789abcdef", b"rest\r\n--boundary--\r\n"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let first = part.next_data().await.unwrap().unwrap();
            assert!(!first.is_empty());
        });
    }

    #[test]
    fn data_chunk_exactly_keep_is_buffered_not_split() {
        block_on(async {
            let delimiter_len = make_delimiter(b"boundary").len();
            let keep = delimiter_len - 1;
            let prefix = vec![b'x'; keep];
            let mut chunks = vec![b"--boundary\r\n\r\n".to_vec()];
            chunks.push(prefix);
            chunks.push(b"tail\r\n--boundary--\r\n".to_vec());
            let items = chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk)))
                .collect::<Vec<_>>();
            let mut multipart = Multipart::new(
                stream::iter(items).map(|item| item.map_err(Error::stream_read_failed)),
                &Boundary::new(b"boundary").unwrap(),
                4096,
            );
            let mut part = multipart.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let mut data_chunks = 0;
            while part.next_data().await.unwrap().is_some() {
                data_chunks += 1;
            }
            assert_eq!(data_chunks, 1);

            let delimiter_len = make_delimiter(b"boundary").len();
            let keep = delimiter_len - 1;
            let prefix = vec![b'x'; keep];
            let mut chunks = vec![b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n".to_vec()];
            chunks.push(prefix);
            chunks.push(b"tail\r\n--boundary--\r\n".to_vec());
            let items = chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk)))
                .collect::<Vec<_>>();
            let mut multipart = Multipart::new(
                stream::iter(items).map(|item| item.map_err(Error::stream_read_failed)),
                &Boundary::new(b"boundary").unwrap(),
                4096,
            );
            let mut part = multipart.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let mut stream = Box::pin(part.take_data_stream().unwrap());
            let mut data_chunks = 0;
            while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                item.unwrap();
                data_chunks += 1;
            }
            assert_eq!(data_chunks, 1);
        });
    }

    /// A part carrying more fields than [`MAX_HEADERS`] is still accepted: the
    /// fallback keeps the first fields, and their names and values read back
    /// exactly like the ones the httparse path produced.
    #[test]
    fn fallback_headers_keep_names_and_values() {
        block_on(async {
            let mut body = Vec::new();
            body.extend_from_slice(b"--boundary\r\n");
            for idx in 0..33 {
                body.extend_from_slice(format!("X-{idx}: value-{idx}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\ndata\r\n--boundary--\r\n");
            let mut multipart = Multipart::new(
                stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from(body))])
                    .map(|item| item.map_err(Error::stream_read_failed)),
                &Boundary::new(b"boundary").unwrap(),
                8192,
            );
            let mut part = multipart.next_part().await.unwrap().unwrap();
            let mut count = 0;
            while let Some(header) = part.next_header().await.unwrap() {
                assert_eq!(header.name, format!("X-{count}"));
                assert_eq!(header.value, format!("value-{count}").as_bytes());
                count += 1;
            }
            assert_eq!(count, MAX_HEADERS);
        });
    }

    /// F31: `search_data` only switches to its adaptive tail for haystacks
    /// above `ADAPTIVE_TAIL_THRESHOLD`, which is the common case for real
    /// streams. A chunk boundary that falls inside the delimiter must still
    /// keep the leading bytes back, for every split position.
    #[test]
    fn large_data_chunk_keeps_a_split_delimiter() {
        block_on(async {
            const BIG: usize = 5000;

            let delimiter = b"\r\n--boundary";
            for split in 1..delimiter.len() {
                let mut first = b"--boundary\r\nX: y\r\n\r\n".to_vec();
                first.extend(std::iter::repeat_n(b'x', BIG));
                first.extend_from_slice(&delimiter[..split]);
                let mut second = delimiter[split..].to_vec();
                second.extend_from_slice(b"--\r\n");

                let mut mp = Multipart::new(
                    stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from(first)), Ok(Bytes::from(second))])
                        .map(|item| item.map_err(Error::stream_read_failed)),
                    &Boundary::new(b"boundary").unwrap(),
                    4096,
                );
                let mut part = mp.next_part().await.unwrap().unwrap();
                while part.next_header().await.unwrap().is_some() {}
                let mut stream = part.take_data_stream().unwrap();

                let data = collect_taken(&mut stream).await.unwrap();
                assert_eq!(data.len(), BIG, "split={split}");
                assert!(data.iter().all(|byte| *byte == b'x'), "split={split}");

                let final_stream = stream.into_final();
                assert!(collect_taken(final_stream).await.is_ok(), "split={split}");
            }
        });
    }

    /// Body with two parts, used by the poll-path tests below.
    const TWO_PARTS: &[u8] = b"--boundary\r\nX: a\r\n\r\nalpha\r\n--boundary\r\nX: b\r\n\r\nbeta\r\n--boundary--\r\n";

    /// Consumes a parser through the async API, propagating every error.
    async fn drain_all<S>(mp: &mut Multipart<S>) -> Result<Vec<(Vec<(String, Vec<u8>)>, Vec<u8>)>, Error>
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
    {
        let mut parts = Vec::new();
        while let Some(mut part) = mp.next_part().await? {
            let mut headers = Vec::new();
            while let Some(header) = part.next_header().await? {
                headers.push((header.name.to_string(), header.value.to_vec()));
            }
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await? {
                data.extend_from_slice(&chunk);
            }
            parts.push((headers, data));
        }
        Ok(parts)
    }

    /// One part as `(headers, data)`; the parsed form used by the poll tests.
    type ParsedPart = (Vec<(String, Vec<u8>)>, Vec<u8>);

    /// The same walk through the poll API.
    fn drain_all_poll<S>(mp: &mut Multipart<S>, cx: &mut Context<'_>) -> Vec<ParsedPart>
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
    {
        let mut parts = Vec::new();
        loop {
            match mp.poll_next_part(cx) {
                // `Pending` only needs another poll: the waker already fired.
                Poll::Pending => {}
                Poll::Ready(Ok(Some(mut part))) => parts.push(drain_part_poll(&mut part, cx)),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected error: {err}"),
            }
        }
        parts
    }

    fn drain_part_poll<S>(part: &mut Part<'_, S>, cx: &mut Context<'_>) -> ParsedPart
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
    {
        let mut headers = Vec::new();
        loop {
            match part.poll_next_header(cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(Some(header))) => headers.push((header.name.to_string(), header.value.to_vec())),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected header error: {err}"),
            }
        }
        let mut data = Vec::new();
        loop {
            match part.poll_next_data(cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(Some(chunk))) => data.extend_from_slice(&chunk),
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected data error: {err}"),
            }
        }
        (headers, data)
    }

    /// Yields `chunks`, going `Pending` once before the chunk at index
    /// `pending_before` (waking immediately).
    fn pending_stream(
        chunks: Vec<Result<Bytes, Error>>,
        pending_before: usize,
    ) -> impl Stream<Item = Result<Bytes, Error>> + Send + Sync {
        let mut chunks = chunks.into_iter();
        let mut yielded = 0usize;
        stream::poll_fn(move |cx| {
            if yielded == pending_before {
                yielded += 1;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            yielded += 1;
            Poll::Ready(chunks.next())
        })
    }

    /// F32a: the public poll entry point walks the same state machine as the
    /// async one, including its terminal arms and a sticky `Done`.
    #[test]
    fn poll_next_part_matches_the_async_path() {
        let reference = block_on(async {
            let mut mp = ok_chunks(vec![TWO_PARTS]);
            drain_all(&mut mp).await.unwrap()
        });
        assert_eq!(reference.len(), 2);

        let mut mp = ok_chunks(vec![TWO_PARTS]);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(drain_all_poll(&mut mp, &mut cx), reference);

        // `Done` is sticky: polling again must not restart or error.
        assert!(matches!(mp.poll_next_part(&mut cx), Poll::Ready(Ok(None))));
        assert!(matches!(mp.poll_next_part(&mut cx), Poll::Ready(Ok(None))));
    }

    /// F45: driving the poll API and dropping a part without reading its data
    /// must skip that data and still yield every following part. The async
    /// driver is covered by `skips_unread_part_data_when_dropped`; the poll
    /// driver's skip arm — the state between "part handed out" and "data
    /// drained" — had no test at all.
    #[test]
    fn poll_next_part_skips_unread_part_data() {
        let reference = block_on(async {
            let mut mp = ok_chunks(vec![TWO_PARTS]);
            drain_all(&mut mp).await.unwrap()
        });
        assert_eq!(reference.len(), 2);

        let mut mp = ok_chunks(vec![TWO_PARTS]);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut headers_seen = Vec::new();
        loop {
            match mp.poll_next_part(&mut cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(Some(mut part))) => {
                    let mut headers = Vec::new();
                    loop {
                        match part.poll_next_header(&mut cx) {
                            Poll::Pending => {}
                            Poll::Ready(Ok(Some(header))) => headers.push((header.name.to_string(), header.value.to_vec())),
                            Poll::Ready(Ok(None)) => break,
                            Poll::Ready(Err(err)) => panic!("unexpected header error: {err}"),
                        }
                    }
                    // The part is dropped here without reading any data, so the
                    // next poll skips it.
                    headers_seen.push(headers);
                }
                Poll::Ready(Ok(None)) => break,
                Poll::Ready(Err(err)) => panic!("unexpected error: {err}"),
            }
        }

        let expected: Vec<_> = reference.into_iter().map(|(headers, _data)| headers).collect();
        assert_eq!(headers_seen, expected);
    }

    /// F46: skipping unread data must surface a truncated part instead of
    /// reporting the end of the stream — on both drivers. The existing
    /// truncation sweep drives the parts it reads, not the ones it skips.
    #[test]
    fn skipping_unread_data_reports_a_truncated_part() {
        // Cut the closing delimiter's tail, so the second part's data never
        // reaches a delimiter.
        let truncated = &TWO_PARTS[..TWO_PARTS.len() - 7];

        let async_err = block_on(async {
            let mut mp = ok_chunks(vec![truncated]);
            for _ in 0..2 {
                let mut part = mp.next_part().await.unwrap().unwrap();
                // The headers have to be read: until the data phase is entered,
                // the same part would be handed out again. The part then goes
                // out of scope without its data being read, so it is skipped.
                while part.next_header().await.unwrap().is_some() {}
            }
            mp.next_part().await.unwrap_err()
        });
        assert!(matches!(async_err, Error::IncompleteStream), "{async_err}");

        let mut mp = ok_chunks(vec![truncated]);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let poll_err = loop {
            match mp.poll_next_part(&mut cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(Some(mut part))) => while let Poll::Ready(Ok(Some(_))) = part.poll_next_header(&mut cx) {},
                Poll::Ready(Ok(None)) => panic!("a truncated body must not report the end"),
                Poll::Ready(Err(err)) => break err,
            }
        };
        assert!(matches!(poll_err, Error::IncompleteStream), "{poll_err}");
    }

    /// F32a: `StreamTaken` is the other terminal arm — after the handover the
    /// poll entry point reports it instead of handing out another part.
    #[test]
    fn poll_next_part_reports_a_taken_stream() {
        let mut mp = ok_chunks(vec![b"--boundary\r\nX: a\r\n\r\nalpha\r\n--boundary--\r\n"]);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut part = match mp.poll_next_part(&mut cx) {
            Poll::Ready(Ok(Some(part))) => part,
            other => panic!("expected a part, got {other:?}"),
        };
        while let Poll::Ready(Ok(Some(_))) = part.poll_next_header(&mut cx) {}
        let _stream = part.take_data_stream().unwrap();

        assert!(matches!(mp.poll_next_part(&mut cx), Poll::Ready(Err(Error::StreamAlreadyTaken))));
    }

    /// The stream has to be `Send + Unpin`, but not `Sync`: the parser polls it
    /// through `&mut`, so a stream that cannot be shared must still be accepted.
    /// A `Cell` is `Send` but not `Sync`, which is the smallest way to put that
    /// in a type.
    #[test]
    fn accepts_a_stream_that_is_send_but_not_sync() {
        struct SendOnlyStream {
            _not_sync: std::cell::Cell<u8>,
            chunks: std::vec::IntoIter<Result<Bytes, Error>>,
        }

        impl Stream for SendOnlyStream {
            type Item = Result<Bytes, Error>;

            fn poll_next(mut self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Ready(self.chunks.next())
            }
        }

        let chunks: Vec<Result<Bytes, Error>> =
            vec![Ok(Bytes::from_static(b"--boundary\r\nX: a\r\n\r\nalpha\r\n--boundary--\r\n"))];
        let stream = SendOnlyStream {
            _not_sync: std::cell::Cell::new(0),
            chunks: chunks.into_iter(),
        };
        let boundary = Boundary::new(b"boundary").unwrap();
        let mut mp = Multipart::new(stream, &boundary, 4096);
        let parsed = block_on(drain_all(&mut mp)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].1, b"alpha");
    }

    /// F32a: a `Pending` at any byte offset must resume into the same parse.
    /// The ready-only tests never suspend inside the boundary steps, so their
    /// `Pending` arms had no coverage at all.
    #[test]
    fn poll_next_part_resumes_after_pending_at_every_offset() {
        let reference = block_on(async {
            let mut mp = ok_chunks(vec![TWO_PARTS]);
            drain_all(&mut mp).await.unwrap()
        });

        for pending_before in 0..=TWO_PARTS.len() {
            let chunks: Vec<Result<Bytes, Error>> = TWO_PARTS.chunks(1).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
            let mut mp = Multipart::new(pending_stream(chunks, pending_before), &Boundary::new(b"boundary").unwrap(), 4096);
            let parsed = block_on(drain_all(&mut mp)).unwrap_or_else(|err| panic!("pending_before={pending_before}: {err}"));
            assert_eq!(parsed, reference, "pending_before={pending_before}");
        }
    }

    /// F32a: truncating the body, or failing the stream, at any byte offset must
    /// surface an error instead of panicking or silently accepting the prefix —
    /// this is what covers the error arms of the poll steps.
    #[test]
    fn truncated_or_failing_streams_error_at_every_offset() {
        for cut in 0..TWO_PARTS.len() {
            let truncated = stream::iter(vec![Ok::<Bytes, Error>(Bytes::copy_from_slice(&TWO_PARTS[..cut]))]);
            let mut mp = Multipart::new(truncated, &Boundary::new(b"boundary").unwrap(), 4096);
            assert!(block_on(drain_all(&mut mp)).is_err(), "truncated at {cut}");

            let failing = stream::iter(vec![
                Ok::<Bytes, Error>(Bytes::copy_from_slice(&TWO_PARTS[..cut])),
                Err(Error::stream_read_failed(io::Error::other("boom"))),
            ]);
            let mut mp = Multipart::new(failing, &Boundary::new(b"boundary").unwrap(), 4096);
            assert!(block_on(drain_all(&mut mp)).is_err(), "failing at {cut}");
        }
    }

    /// F32b: a fresh chunk that carries data *and* the delimiter must yield the
    /// data prefix. With the delimiter at offset zero both branches of the
    /// `index == 0` split produce an empty chunk, so only this shape pins it.
    #[test]
    fn fresh_chunk_with_data_and_delimiter_yields_the_prefix() {
        block_on(async {
            let mut mp = ok_chunks(vec![b"--boundary\r\nX: y\r\n\r\n", b"hello\r\n--boundary--\r\n"]);
            let mut part = mp.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}

            let first = part.next_data().await.unwrap().unwrap();
            assert_eq!(first.as_ref(), b"hello");
            assert!(part.next_data().await.unwrap().is_none());
        });
    }
}

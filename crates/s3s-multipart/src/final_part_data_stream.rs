// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Strict closing validation for a taken part stream.
//!
//! [`FinalPartDataStream`] continues to yield the part's data — including
//! data that was not yet read when [`PartDataStream::into_final`](crate::PartDataStream::into_final) was called
//! — and then validates the strict closing trailer: the `--` suffix of the
//! closing delimiter, optional transport padding, `\r\n`, and end of stream.
//!
//! An epilogue or another part is rejected (`Error::StreamPartNotLast`)
//! because callers derive the exact file length from the multipart byte
//! accounting: trailing content of unknown size would break that accounting.
//! Note that RFC 2046 allows an epilogue; this crate deliberately rejects
//! it.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;
use memchr::memmem;

use crate::Error;
use crate::buffer::StreamBuffer;
use crate::delimiter::{DataSearch, search_data};

/// The strict variant of a taken part stream.
///
/// Created with [`PartDataStream::into_final`](crate::PartDataStream::into_final), it yields the remaining data
/// chunks of the part — each one non-empty — and then validates the strict
/// closing trailer: `\r\n--boundary--\r\n` followed by end of stream, with
/// transport padding (SP / HTAB) allowed between the closing `--` and the
/// final CRLF, as RFC 2046 section 5.1.1 allows for a boundary line. An
/// epilogue or another part is rejected because callers derive the exact
/// content length from the multipart byte accounting.
pub struct FinalPartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    buffer: StreamBuffer<S>,
    delimiter_finder: Box<memmem::Finder<'static>>,
    state: DataState,
    multipart_consumed: u64,
    /// The taken stream ended abnormally — with an error, or at end of stream
    /// before the delimiter — so the first poll reports an incomplete part
    /// instead of validating a trailer that was never reached.
    aborted: bool,
    terminated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataState {
    /// The closing delimiter has not been reached yet; data chunks are
    /// yielded as they are located.
    Data,
    /// The delimiter was consumed; the `--` suffix of the closing delimiter
    /// is expected next.
    AfterBoundary,
    /// The `--` suffix was consumed; optional transport padding and the
    /// final `\r\n` are expected.
    FinalCRLF,
    /// The closing trailer is complete; only end of stream may follow.
    Eof,
    Done,
}

impl<S> FinalPartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    /// Constructs the final stream from the state of a taken part stream.
    ///
    /// `data_done` indicates whether the closing delimiter was already
    /// consumed (and the post-delimiter bytes are retained in `buffer`);
    /// `aborted` indicates that the taken stream ended abnormally, so this
    /// stream has to report the incomplete part. The two cannot both be true:
    /// reaching the delimiter ends the taken stream's data phase successfully.
    pub(super) fn new(
        buffer: StreamBuffer<S>,
        delimiter_finder: Box<memmem::Finder<'static>>,
        multipart_consumed: u64,
        data_done: bool,
        aborted: bool,
    ) -> Self {
        Self {
            buffer,
            delimiter_finder,
            state: if data_done {
                DataState::AfterBoundary
            } else {
                DataState::Data
            },
            multipart_consumed,
            aborted,
            terminated: false,
        }
    }

    /// Returns the total number of multipart bytes consumed at the moment
    /// the stream was taken, excluding bytes retained in the internal
    /// buffer.
    ///
    /// This is the value to subtract from the request `Content-Length` when
    /// deriving the exact file length. It does not count bytes yielded by
    /// this stream itself.
    #[must_use]
    pub fn multipart_consumed(&self) -> u64 {
        self.multipart_consumed
    }

    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, Error>> {
        if self.aborted {
            // The taken stream failed before reaching the delimiter, so there is
            // no trailer to validate: report the incomplete part instead of
            // reading on, which could otherwise turn the failure into a success.
            self.terminated = true;
            return Poll::Ready(Err(Error::IncompleteStreamPart));
        }

        let delimiter_finder = &self.delimiter_finder;
        let delimiter_len = delimiter_finder.needle().len();

        loop {
            if !self.buffer.buf.is_empty() {
                match search_data(&self.buffer.buf, delimiter_finder) {
                    DataSearch::Found { index } => {
                        let data = self.buffer.buf.split_to(index).freeze();
                        let _ = self.buffer.buf.split_to(delimiter_len);
                        self.state = DataState::AfterBoundary;
                        return Poll::Ready(Ok((!data.is_empty()).then_some(data)));
                    }
                    DataSearch::Emit { end } => {
                        return Poll::Ready(Ok(Some(self.buffer.buf.split_to(end).freeze())));
                    }
                    DataSearch::KeepAll => {}
                }
            }

            match ready!(self.buffer.poll_stream(cx)) {
                Some(Ok(chunk)) => {
                    if !self.buffer.buf.is_empty() {
                        self.buffer.buf.extend_from_slice(&chunk);
                        continue;
                    }

                    match search_data(&chunk, delimiter_finder) {
                        DataSearch::Found { index } => {
                            let data = chunk.slice(..index);
                            self.buffer.buf.clear();
                            self.buffer
                                .buf
                                .extend_from_slice(&chunk[index.saturating_add(delimiter_len)..]);
                            self.state = DataState::AfterBoundary;
                            return Poll::Ready(Ok((!data.is_empty()).then_some(data)));
                        }
                        DataSearch::Emit { end } => {
                            let data = chunk.slice(..end);
                            self.buffer.buf.extend_from_slice(&chunk[end..]);
                            return Poll::Ready(Ok(Some(data)));
                        }
                        DataSearch::KeepAll => {
                            self.buffer.buf.extend_from_slice(&chunk);
                        }
                    }
                }
                Some(Err(err)) => return Poll::Ready(Err(err)),
                None => return Poll::Ready(Err(Error::IncompleteStreamPart)),
            }
        }
    }

    fn poll_trailer(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            match self.state {
                DataState::AfterBoundary => {
                    ready!(self.fill_buf(2, cx))?;
                    if !self.buffer.buf.starts_with(b"--") {
                        let err = if self.buffer.buf.starts_with(b"\r")
                            || self.buffer.buf.first().is_some_and(|b| matches!(b, b' ' | b'\t'))
                        {
                            Error::StreamPartNotLast
                        } else {
                            Error::InvalidFormat
                        };
                        return Poll::Ready(Err(err));
                    }
                    let _ = self.buffer.buf.split_to(2);
                    self.state = DataState::FinalCRLF;
                }
                DataState::FinalCRLF => {
                    loop {
                        ready!(self.fill_buf(1, cx))?;

                        let first = self.buffer.buf[0];
                        match first {
                            b' ' | b'\t' => {
                                let _ = self.buffer.buf.split_to(1);
                            }
                            b'\r' => break,
                            _ => return Poll::Ready(Err(Error::InvalidFormat)),
                        }
                    }

                    ready!(self.fill_buf(2, cx))?;
                    if !self.buffer.buf.starts_with(b"\r\n") {
                        return Poll::Ready(Err(Error::InvalidFormat));
                    }
                    let _ = self.buffer.buf.split_to(2);
                    self.state = DataState::Eof;
                }
                DataState::Eof => {
                    if !self.buffer.buf.is_empty() {
                        return Poll::Ready(Err(Error::StreamPartNotLast));
                    }

                    match ready!(self.buffer.poll_stream(cx)) {
                        None => {
                            self.state = DataState::Done;
                            return Poll::Ready(Ok(()));
                        }
                        Some(Ok(_)) => return Poll::Ready(Err(Error::StreamPartNotLast)),
                        Some(Err(err)) => return Poll::Ready(Err(err)),
                    }
                }
                // `poll_trailer` is only reached in the three trailer states, so
                // this arm guards the match rather than describing a reachable
                // outcome.
                DataState::Data | DataState::Done => return Poll::Ready(Ok(())),
            }
        }
    }

    fn fill_buf(&mut self, len: usize, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        while self.buffer.buf.len() < len {
            match ready!(self.buffer.poll_stream(cx)) {
                Some(Ok(chunk)) => self.buffer.buf.extend_from_slice(&chunk),
                Some(Err(err)) => return Poll::Ready(Err(err)),
                None => return Poll::Ready(Err(Error::IncompleteStreamPart)),
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<S> fmt::Debug for FinalPartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinalPartDataStream")
            .field("state", &self.state)
            .field("multipart_consumed", &self.multipart_consumed)
            .finish_non_exhaustive()
    }
}

impl<S> Stream for FinalPartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if this.terminated {
                return Poll::Ready(None);
            }

            match this.state {
                DataState::Data => match ready!(this.poll_data(cx)) {
                    Ok(Some(item)) => return Poll::Ready(Some(Ok(item))),
                    // No data in this step: the delimiter was already reached, so
                    // go on and validate the trailer in the same poll instead of
                    // handing the caller an empty chunk.
                    Ok(None) => {}
                    Err(err) => {
                        this.terminated = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                },
                DataState::AfterBoundary | DataState::FinalCRLF | DataState::Eof => {
                    if let Err(err) = ready!(this.poll_trailer(cx)) {
                        this.terminated = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                DataState::Done => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
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

    use std::pin::Pin as StdPin;
    use std::task::{Context as TaskContext, Poll as TaskPoll};

    use futures_util::StreamExt;
    use futures_util::stream;
    use futures_util::task::noop_waker;

    use crate::delimiter::make_delimiter_finder;
    use crate::part_data_stream::PartDataStream;

    struct PendingStream;

    impl Stream for PendingStream {
        type Item = Result<Bytes, Error>;

        fn poll_next(self: StdPin<&mut Self>, _cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
            TaskPoll::Pending
        }
    }

    fn with_cx<R>(f: impl FnOnce(&mut TaskContext<'_>) -> R) -> R {
        let waker = noop_waker();
        let mut cx = TaskContext::from_waker(&waker);
        f(&mut cx)
    }

    fn final_with_buffer(
        items: Vec<Result<Bytes, Error>>,
        state: DataState,
    ) -> FinalPartDataStream<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        FinalPartDataStream {
            buffer: StreamBuffer::new(stream::iter(items)),
            delimiter_finder: make_delimiter_finder(b"boundary"),
            state,
            multipart_consumed: 0,
            aborted: false,
            terminated: false,
        }
    }

    fn final_with_error(
        err: Error,
        state: DataState,
    ) -> FinalPartDataStream<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        final_with_buffer(vec![Err(err)], state)
    }

    fn final_with_prefix(
        prefix: &[u8],
        state: DataState,
    ) -> FinalPartDataStream<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        let mut buffer = StreamBuffer::new(PendingStream);
        buffer.buf.extend_from_slice(prefix);
        FinalPartDataStream {
            buffer,
            delimiter_finder: make_delimiter_finder(b"boundary"),
            state,
            multipart_consumed: 0,
            aborted: false,
            terminated: false,
        }
    }

    #[test]
    fn part_data_stream_pending_polls() {
        with_cx(|cx| {
            let buffer = StreamBuffer::new(PendingStream);
            let mut stream = FinalPartDataStream {
                buffer,
                delimiter_finder: make_delimiter_finder(b"boundary"),
                state: DataState::Data,
                multipart_consumed: 0,
                aborted: false,
                terminated: false,
            };
            assert!(matches!(stream.poll_next_unpin(cx), TaskPoll::Pending));
        });
    }

    #[test]
    fn poll_data_buffered_empty_and_prefix_paths() {
        with_cx(|cx| {
            let mut ds = final_with_buffer(Vec::new(), DataState::Data);
            ds.buffer.buf.extend_from_slice(b"\r\n--boundary--\r\n");
            // No data item; the trailer is validated in the same poll.
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));

            let mut ds = final_with_buffer(Vec::new(), DataState::Data);
            ds.buffer.buf.extend_from_slice(b"0123456789abcdefghij");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Ok(_)))));

            let mut ds = final_with_buffer(vec![Ok(Bytes::from_static(b"tiny"))], DataState::Data);
            let poll = ds.poll_next_unpin(cx);
            assert!(matches!(poll, TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))));
        });
    }

    #[test]
    fn fill_buf_error_and_pending_paths() {
        with_cx(|cx| {
            let mut ds = final_with_error(Error::InvalidFormat, DataState::AfterBoundary);
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));

            let mut ds = final_with_buffer(Vec::new(), DataState::AfterBoundary);
            ds.buffer.buf.extend_from_slice(b"xx");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));

            let mut ds = final_with_buffer(vec![Ok(Bytes::from_static(b"--\r\n"))], DataState::AfterBoundary);
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    #[test]
    fn trailer_padding_error_and_pending_paths() {
        with_cx(|cx| {
            let mut ds = final_with_error(Error::InvalidFormat, DataState::FinalCRLF);
            ds.buffer.buf.extend_from_slice(b" ");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));

            let mut ds = final_with_prefix(b" ", DataState::FinalCRLF);
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Pending));

            let mut ds = final_with_buffer(Vec::new(), DataState::FinalCRLF);
            ds.buffer.buf.extend_from_slice(b" ");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))));

            let mut ds = final_with_error(Error::InvalidFormat, DataState::FinalCRLF);
            ds.buffer.buf.extend_from_slice(b"\r");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));

            let mut ds = final_with_prefix(b"\r", DataState::FinalCRLF);
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Pending));

            let mut ds = final_with_buffer(Vec::new(), DataState::FinalCRLF);
            ds.buffer.buf.extend_from_slice(b"xx");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));
        });
    }

    #[test]
    fn trailer_final_crlf_requires_exact_bytes() {
        with_cx(|cx| {
            let mut ds = final_with_buffer(Vec::new(), DataState::FinalCRLF);
            ds.buffer.buf.extend_from_slice(b"\rX");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));
        });
    }

    #[test]
    fn terminated_stream_returns_none_on_second_poll() {
        with_cx(|cx| {
            let ds = final_with_error(Error::InvalidFormat, DataState::Data);
            let mut ds = Box::pin(ds);
            assert!(matches!(
                ds.as_mut().poll_next_unpin(cx),
                TaskPoll::Ready(Some(Err(Error::InvalidFormat)))
            ));
            assert!(matches!(ds.as_mut().poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    #[test]
    fn yields_remaining_data_then_rejects_another_part() {
        with_cx(|cx| {
            // into_final was called before the delimiter was reached: the
            // remaining data is yielded, then the strict ending rejects the
            // part that follows.
            let mut ds = final_with_buffer(
                vec![Ok(Bytes::from_static(
                    b"hello\r\n--boundary\r\nX: y\r\n\r\nworld\r\n--boundary--\r\n",
                ))],
                DataState::Data,
            );
            assert!(matches!(
                ds.poll_next_unpin(cx),
                TaskPoll::Ready(Some(Ok(bytes))) if bytes.as_ref() == b"hello"
            ));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::StreamPartNotLast)))));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    /// The observable accessors of the strict stream: the size hint (the length
    /// cannot be known, so `(0, None)` is the honest answer), the `Debug`
    /// rendering — `boundary.rs` also pins the rendering of its own type — and
    /// the consumed offset, which `into_final` has to carry over because
    /// callers derive the exact file length from it.
    #[test]
    fn accessors_and_debug_are_stable() {
        let ds = final_with_buffer(Vec::new(), DataState::Data);
        assert_eq!(ds.size_hint(), (0, None));
        assert_eq!(ds.multipart_consumed(), 0);

        let rendered = format!("{ds:?}");
        assert!(rendered.starts_with("FinalPartDataStream"), "{rendered}");
        assert!(rendered.contains("state"), "{rendered}");
        assert!(rendered.contains("multipart_consumed"), "{rendered}");

        // A taken stream that had consumed 42 multipart bytes keeps reporting
        // that offset after the handover.
        let stream = PartDataStream::new(
            StreamBuffer::new(stream::iter(Vec::<Result<Bytes, Error>>::new())),
            make_delimiter_finder(b"boundary"),
            42,
        );
        assert_eq!(stream.multipart_consumed(), 42);
        assert_eq!(stream.into_final().multipart_consumed(), 42);
    }

    /// A taken stream that failed before the delimiter must not be able to turn
    /// into a successful strict stream: the handover keeps the failure and
    /// reports the incomplete part instead of validating a trailer that was
    /// never reached.
    #[test]
    fn a_failed_taken_stream_stays_failed() {
        with_cx(|cx| {
            let mut stream = PartDataStream::new(
                StreamBuffer::new(stream::iter(vec![
                    Ok(Bytes::from_static(b"0123456789abcdefghij")),
                    Err(Error::InvalidFormat),
                    Ok(Bytes::from_static(b"world\r\n--boundary--\r\n")),
                ])),
                make_delimiter_finder(b"boundary"),
                5,
            );
            assert!(matches!(
                stream.poll_next_unpin(cx),
                TaskPoll::Ready(Some(Ok(bytes))) if bytes.as_ref() == b"012345678"
            ));
            assert!(matches!(stream.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));

            let mut ds = stream.into_final();
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    /// The same handover when the taken stream ended at end of stream instead of
    /// erroring: still no trailer to validate.
    #[test]
    fn a_truncated_taken_stream_stays_failed() {
        with_cx(|cx| {
            let mut stream = PartDataStream::new(
                StreamBuffer::new(stream::iter(vec![Ok(Bytes::from_static(b"0123456789abcdefghij"))])),
                make_delimiter_finder(b"boundary"),
                0,
            );
            assert!(matches!(stream.poll_next_unpin(cx), TaskPoll::Ready(Some(Ok(_)))));
            assert!(matches!(
                stream.poll_next_unpin(cx),
                TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))
            ));

            let mut ds = stream.into_final();
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }
}

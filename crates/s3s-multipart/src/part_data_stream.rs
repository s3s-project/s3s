// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The self-contained stream handed out by [`Part::take_data_stream`](crate::Part::take_data_stream).
//!
//! The stream yields the part's data chunks and ends at the part's closing
//! delimiter `\r\n--boundary`. It does not interpret what follows the
//! delimiter: the strict closing contract (the `--` suffix, transport
//! padding, the final `\r\n`, and the absence of an epilogue or further
//! parts) is enforced by [`FinalPartDataStream`], reached through
//! [`PartDataStream::into_final`](crate::PartDataStream::into_final) at any point.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;
use memchr::memmem;

use crate::Error;
use crate::buffer::StreamBuffer;
use crate::delimiter::{DataSearch, search_data};
use crate::final_part_data_stream::FinalPartDataStream;

/// A self-contained stream produced by [`Part::take_data_stream`](crate::Part::take_data_stream).
///
/// The stream yields the part's data chunks — each one non-empty, so a part
/// with no data yields nothing at all — and ends at the closing
/// delimiter `\r\n--boundary`. What follows the delimiter (the strict
/// closing trailer, an epilogue, or another part) is not interpreted; call
/// [`PartDataStream::into_final`](crate::PartDataStream::into_final) to obtain a [`FinalPartDataStream`] that
/// yields any remaining data and enforces the strict closing trailer.
pub struct PartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    buffer: StreamBuffer<S>,
    delimiter_finder: Box<memmem::Finder<'static>>,
    multipart_consumed: u64,
    done: bool,
    terminated: bool,
}

impl<S> PartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    /// Constructs the stream from the parser state at the take point.
    pub(super) fn new(buffer: StreamBuffer<S>, delimiter_finder: Box<memmem::Finder<'static>>, multipart_consumed: u64) -> Self {
        Self {
            buffer,
            delimiter_finder,
            multipart_consumed,
            done: false,
            terminated: false,
        }
    }

    /// Returns the total number of multipart bytes consumed at the moment
    /// this stream was taken, excluding bytes retained in the internal
    /// buffer.
    ///
    /// This is the value to subtract from the request `Content-Length` when
    /// deriving the exact file length. It does not count bytes yielded by
    /// this stream itself.
    #[must_use]
    pub fn multipart_consumed(&self) -> u64 {
        self.multipart_consumed
    }

    /// Converts this stream into a [`FinalPartDataStream`], which yields any
    /// remaining data and then enforces the strict closing trailer.
    ///
    /// No data is discarded: chunks already read stay with the caller, and
    /// chunks not yet read are yielded by the returned stream before the
    /// closing trailer is validated.
    ///
    /// A stream that ended abnormally — with an error, or at end of stream
    /// before the delimiter — stays failed: the returned stream reports
    /// [`Error::IncompleteStreamPart`] instead of validating a trailer that was
    /// never reached.
    #[must_use]
    pub fn into_final(self) -> FinalPartDataStream<S> {
        FinalPartDataStream::new(self.buffer, self.delimiter_finder, self.multipart_consumed, self.done, self.terminated)
    }

    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>> {
        if self.done || self.terminated {
            return Poll::Ready(None);
        }

        let delimiter_finder = &self.delimiter_finder;
        let delimiter_len = delimiter_finder.needle().len();

        loop {
            if !self.buffer.buf.is_empty() {
                match search_data(&self.buffer.buf, delimiter_finder) {
                    DataSearch::Found { index } => {
                        let data = self.buffer.buf.split_to(index).freeze();
                        let _ = self.buffer.buf.split_to(delimiter_len);
                        self.done = true;
                        return if data.is_empty() {
                            Poll::Ready(None)
                        } else {
                            Poll::Ready(Some(Ok(data)))
                        };
                    }
                    DataSearch::Emit { end } => {
                        return Poll::Ready(Some(Ok(self.buffer.buf.split_to(end).freeze())));
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
                            self.done = true;
                            return if data.is_empty() {
                                Poll::Ready(None)
                            } else {
                                Poll::Ready(Some(Ok(data)))
                            };
                        }
                        DataSearch::Emit { end } => {
                            let data = chunk.slice(..end);
                            self.buffer.buf.extend_from_slice(&chunk[end..]);
                            return Poll::Ready(Some(Ok(data)));
                        }
                        DataSearch::KeepAll => {
                            self.buffer.buf.extend_from_slice(&chunk);
                        }
                    }
                }
                Some(Err(err)) => {
                    self.terminated = true;
                    return Poll::Ready(Some(Err(err)));
                }
                None => {
                    self.terminated = true;
                    return Poll::Ready(Some(Err(Error::IncompleteStreamPart)));
                }
            }
        }
    }
}

impl<S> fmt::Debug for PartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartDataStream")
            .field("multipart_consumed", &self.multipart_consumed)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<S> Stream for PartDataStream<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Sync + Unpin,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_data(cx)
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

    fn pending_part_data_stream() -> PartDataStream<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        PartDataStream::new(StreamBuffer::new(PendingStream), make_delimiter_finder(b"boundary"), 0)
    }

    fn ds_with_buffer(
        items: Vec<Result<Bytes, Error>>,
    ) -> PartDataStream<impl Stream<Item = Result<Bytes, Error>> + Send + Sync> {
        PartDataStream::new(StreamBuffer::new(stream::iter(items)), make_delimiter_finder(b"boundary"), 0)
    }

    #[test]
    fn part_data_stream_pending_polls() {
        with_cx(|cx| {
            let mut stream = pending_part_data_stream();
            assert!(matches!(stream.poll_next_unpin(cx), TaskPoll::Pending));
        });
    }

    #[test]
    fn poll_data_buffered_empty_and_prefix_paths() {
        with_cx(|cx| {
            let mut ds = ds_with_buffer(Vec::new());
            ds.buffer.buf.extend_from_slice(b"\r\n--boundary--\r\n");
            // The delimiter is at the front, so the stream ends without an item.
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));

            let mut ds = ds_with_buffer(Vec::new());
            ds.buffer.buf.extend_from_slice(b"0123456789abcdefghij");
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Ok(_)))));

            let mut ds = ds_with_buffer(vec![Ok(Bytes::from_static(b"tiny"))]);
            let poll = ds.poll_next_unpin(cx);
            assert!(matches!(poll, TaskPoll::Ready(Some(Err(Error::IncompleteStreamPart)))));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    #[test]
    fn stream_ends_at_delimiter_without_trailer_checks() {
        with_cx(|cx| {
            // Another part follows the taken part's data; the data stream
            // ends at the delimiter without interpreting the rest.
            let mut ds = ds_with_buffer(vec![Ok(Bytes::from_static(
                b"hello\r\n--boundary\r\nX: y\r\n\r\nworld\r\n--boundary--\r\n",
            ))]);
            assert!(matches!(
                ds.poll_next_unpin(cx),
                TaskPoll::Ready(Some(Ok(bytes))) if bytes.as_ref() == b"hello"
            ));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    #[test]
    fn into_final_after_delimiter_validates_trailer() {
        with_cx(|cx| {
            let mut ds = ds_with_buffer(vec![Ok(Bytes::from_static(b"hello\r\n--boundary--\r\n"))]);
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Ok(_)))));
            let mut final_stream = ds.into_final();
            assert!(matches!(final_stream.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }

    /// A failing underlying stream is reported once and then ends this stream,
    /// exactly like the strict variant
    /// (`FinalPartDataStream::terminated_stream_returns_none_on_second_poll`):
    /// the chunk that follows the failure is never handed out.
    #[test]
    fn stream_error_is_reported_then_the_stream_ends() {
        with_cx(|cx| {
            let mut ds = ds_with_buffer(vec![
                // Twenty bytes emit the first nine and keep the rest, because
                // the delimiter is twelve bytes long, so this poll must yield a
                // chunk instead of buffering everything.
                Ok(Bytes::from_static(b"0123456789abcdefghij")),
                Err(Error::InvalidFormat),
                Ok(Bytes::from_static(b"world")),
            ]);
            assert!(matches!(
                ds.poll_next_unpin(cx),
                TaskPoll::Ready(Some(Ok(bytes))) if bytes.as_ref() == b"012345678"
            ));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(Some(Err(Error::InvalidFormat)))));
            assert!(matches!(ds.poll_next_unpin(cx), TaskPoll::Ready(None)));
        });
    }
}

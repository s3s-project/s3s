// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Shared harness for the `multipart_parser` fuzz target.
//!
//! The target (`fuzz_targets/multipart_parser.rs`) and the corpus checker
//! (`bin/check_seeds.rs`) both build the parser input with [`make_items`] and
//! parse it with [`run_full`], so the committed seeds are checked against the
//! same delivery the fuzzer uses instead of a second implementation of it.
//!
//! Input protocol: `data[0]` is a control byte, `data[1..]` the payload. The
//! bits combine:
//! - 0x01 inject one recognizable `Err` fragment
//! - 0x02 feed the payload as a single fragment (the O2 reference side)
//! - 0x04 consume the first file part through `take_data_stream`
//! - 0x08 walk the parts through the skip path (each `Part` is dropped unread)
//! - 0x10 use a small `max_buffer_size` (32)
//! - 0x20 consume the first file part through `into_final`
//! - 0x40 interleave an empty chunk before every fragment
//!
//! Only 0x01, 0x02, 0x10 and 0x40 change what the parser is handed; 0x04, 0x08
//! and 0x20 select an additional consumption strategy.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use futures::stream;
use s3s_multipart::{Boundary, Error, Multipart};

/// The limit carried by the injected error variant, so an injected error can be
/// told apart from one the parser produced.
pub const INJECTED_LIMIT: usize = 0x5A5A;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartOutcome {
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub data: Vec<u8>,
    pub is_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Ok,
    StreamReadFailed,
    InvalidBoundary,
    InvalidFormat,
    IncompleteStream,
    HeaderSizeExceeded,
    StreamPartNotLast,
    StreamAlreadyTaken,
    IncompleteStreamPart,
    /// A variant added by the crate after this target was written.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub parts: Vec<PartOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeOutcome {
    pub data: Vec<u8>,
    pub consumed: u64,
    pub kind: OutcomeKind,
}

pub fn error_kind(err: Error) -> OutcomeKind {
    match err {
        Error::StreamReadFailed(_) => OutcomeKind::StreamReadFailed,
        Error::InvalidBoundary => OutcomeKind::InvalidBoundary,
        Error::InvalidFormat => OutcomeKind::InvalidFormat,
        Error::IncompleteStream => OutcomeKind::IncompleteStream,
        Error::HeaderSizeExceeded { .. } => OutcomeKind::HeaderSizeExceeded,
        Error::StreamPartNotLast => OutcomeKind::StreamPartNotLast,
        Error::StreamAlreadyTaken => OutcomeKind::StreamAlreadyTaken,
        Error::IncompleteStreamPart => OutcomeKind::IncompleteStreamPart,
        // `Error` is `#[non_exhaustive]`: compare the variants that exist here
        // and keep anything new out of the assertion rather than failing on it.
        _ => OutcomeKind::Other,
    }
}

pub fn fragment_payload(payload: &[u8], single: bool) -> Vec<Bytes> {
    if single || payload.is_empty() {
        return if payload.is_empty() {
            Vec::new()
        } else {
            vec![Bytes::copy_from_slice(payload)]
        };
    }

    let mut fragments = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let len = 1 + usize::from(payload[pos] % 16);
        let end = pos.saturating_add(len).min(payload.len());
        fragments.push(Bytes::copy_from_slice(&payload[pos..end]));
        pos = end;
    }
    fragments
}

pub fn make_items(payload: &[u8], control: u8) -> Vec<Result<Bytes, Error>> {
    let single = control & 0x02 != 0;
    let inject_error = control & 0x01 != 0;
    let interleave_empty = control & 0x40 != 0;

    let mut items: Vec<Result<Bytes, Error>> = Vec::new();
    for fragment in fragment_payload(payload, single) {
        if interleave_empty {
            items.push(Ok(Bytes::new()));
        }
        items.push(Ok(fragment));
    }
    if inject_error {
        let idx = items.len() / 2;
        items.insert(idx, Err(Error::HeaderSizeExceeded { limit: INJECTED_LIMIT }));
    }
    items
}

pub fn max_buffer_size(control: u8) -> usize {
    if control & 0x10 != 0 { 32 } else { 4096 }
}

async fn parse_headers<S>(part: &mut s3s_multipart::Part<'_, S>) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, bool), Error>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    let mut headers = Vec::new();
    let mut is_file = false;
    while let Some(header) = part.next_header().await? {
        if header.name.eq_ignore_ascii_case("content-disposition")
            && let Some(cd) = s3s_multipart::parse_content_disposition(header.value)
        {
            is_file = cd.name.is_some_and(|name| name.eq_ignore_ascii_case(b"file"));
        }
        headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
    }
    Ok((headers, is_file))
}

pub async fn run_full(payload: &[u8], control: u8) -> Outcome {
    let mut multipart = Multipart::new(
        stream::iter(make_items(payload, control)),
        &Boundary::new(b"boundary").unwrap(),
        max_buffer_size(control),
    );
    let mut parts = Vec::new();
    loop {
        match multipart.next_part().await {
            Ok(None) => {
                return Outcome {
                    kind: OutcomeKind::Ok,
                    parts,
                };
            }
            Ok(Some(mut part)) => match parse_headers(&mut part).await {
                Ok((headers, is_file)) => {
                    let mut data = Vec::new();
                    loop {
                        match part.next_data().await {
                            Ok(Some(chunk)) => data.extend_from_slice(&chunk),
                            Ok(None) => break,
                            Err(err) => {
                                return Outcome {
                                    kind: error_kind(err),
                                    parts,
                                };
                            }
                        }
                    }
                    parts.push(PartOutcome { headers, data, is_file });
                }
                Err(err) => {
                    return Outcome {
                        kind: error_kind(err),
                        parts,
                    };
                }
            },
            Err(err) => {
                return Outcome {
                    kind: error_kind(err),
                    parts,
                };
            }
        }
    }
}

/// Consumes the first file part through `take_data_stream`, then either drains
/// it or converts it into the strict stream, depending on the control byte.
pub async fn run_taken(payload: &[u8], control: u8, strict: bool) -> Option<TakeOutcome> {
    let mut multipart = Multipart::new(
        stream::iter(make_items(payload, control)),
        &Boundary::new(b"boundary").unwrap(),
        max_buffer_size(control),
    );
    loop {
        match multipart.next_part().await {
            Ok(None) => return None,
            Ok(Some(mut part)) => {
                let (_, is_file) = parse_headers(&mut part).await.ok()?;
                if is_file {
                    let stream = match part.take_data_stream() {
                        Ok(stream) => stream,
                        Err(err) => {
                            return Some(TakeOutcome {
                                data: Vec::new(),
                                consumed: 0,
                                kind: error_kind(err),
                            });
                        }
                    };
                    let consumed = stream.multipart_consumed();
                    let mut data = Vec::new();
                    let mut stream = Box::pin(if strict {
                        Either::Final(stream.into_final())
                    } else {
                        Either::Plain(stream)
                    });
                    while let Some(item) = StreamExt::next(&mut stream).await {
                        match item {
                            Ok(chunk) => data.extend_from_slice(&chunk),
                            Err(err) => {
                                return Some(TakeOutcome {
                                    data,
                                    consumed,
                                    kind: error_kind(err),
                                });
                            }
                        }
                    }
                    return Some(TakeOutcome {
                        data,
                        consumed,
                        kind: OutcomeKind::Ok,
                    });
                }
                while part.next_data().await.ok().flatten().is_some() {}
            }
            Err(err) => {
                return Some(TakeOutcome {
                    data: Vec::new(),
                    consumed: 0,
                    kind: error_kind(err),
                });
            }
        }
    }
}

/// Either taken stream, so one driver can drain both.
enum Either<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    Plain(s3s_multipart::PartDataStream<S>),
    Final(s3s_multipart::FinalPartDataStream<S>),
}

impl<S> Stream for Either<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            Either::Plain(stream) => Pin::new(stream).poll_next(cx),
            Either::Final(stream) => Pin::new(stream).poll_next(cx),
        }
    }
}

/// Walks the parts by dropping each `Part` unread (O6).
///
/// The loop is bounded by the payload, so a parser that re-delivers a part
/// trips the assertion instead of hanging.
pub async fn run_skip(payload: &[u8], control: u8) -> usize {
    let mut multipart = Multipart::new(
        stream::iter(make_items(payload, control)),
        &Boundary::new(b"boundary").unwrap(),
        max_buffer_size(control),
    );
    let mut visits = 0usize;
    let cap = payload.len().saturating_add(2);
    loop {
        match multipart.next_part().await {
            Ok(None) | Err(_) => return visits,
            Ok(Some(part)) => {
                // Dropped at the end of this block, without reading anything.
                let _ = part;
            }
        }
        visits += 1;
        assert!(visits <= cap, "O6 progress: next_part kept yielding parts");
    }
}

struct CountingStream {
    items: VecDeque<Result<Bytes, Error>>,
    polls: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
}

impl CountingStream {
    fn new(items: Vec<Result<Bytes, Error>>) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let stream = Self {
            items: items.into(),
            polls: polls.clone(),
            bytes: bytes.clone(),
        };
        (stream, polls, bytes)
    }
}

impl futures::Stream for CountingStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        match self.items.pop_front() {
            Some(Ok(bytes)) => {
                self.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(err)) => Poll::Ready(Some(Err(err))),
            None => Poll::Ready(None),
        }
    }
}

pub async fn run_counted(payload: &[u8], control: u8) -> (Outcome, usize, usize) {
    let items = make_items(payload, control);
    let (stream, polls, bytes) = CountingStream::new(items);
    let mut multipart = Multipart::new(stream, &Boundary::new(b"boundary").unwrap(), max_buffer_size(control));
    let mut parts = Vec::new();
    loop {
        match multipart.next_part().await {
            Ok(None) => {
                return (
                    Outcome {
                        kind: OutcomeKind::Ok,
                        parts,
                    },
                    polls.load(Ordering::Relaxed),
                    bytes.load(Ordering::Relaxed),
                );
            }
            Ok(Some(mut part)) => {
                let (headers, is_file) = match parse_headers(&mut part).await {
                    Ok(value) => value,
                    Err(err) => {
                        return (
                            Outcome {
                                kind: error_kind(err),
                                parts,
                            },
                            polls.load(Ordering::Relaxed),
                            bytes.load(Ordering::Relaxed),
                        );
                    }
                };
                let mut data = Vec::new();
                loop {
                    match part.next_data().await {
                        Ok(Some(chunk)) => data.extend_from_slice(&chunk),
                        Ok(None) => break,
                        Err(err) => {
                            return (
                                Outcome {
                                    kind: error_kind(err),
                                    parts,
                                },
                                polls.load(Ordering::Relaxed),
                                bytes.load(Ordering::Relaxed),
                            );
                        }
                    }
                }
                parts.push(PartOutcome { headers, data, is_file });
            }
            Err(err) => {
                return (
                    Outcome {
                        kind: error_kind(err),
                        parts,
                    },
                    polls.load(Ordering::Relaxed),
                    bytes.load(Ordering::Relaxed),
                );
            }
        }
    }
}

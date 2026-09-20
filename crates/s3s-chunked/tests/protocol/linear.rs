// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Linear guardrail: decoding work must grow linearly with the body.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::common::{drain, items};
use s3s_chunked::{ChunkedStream, Limits};
use std::cell::Cell;
use std::rc::Rc;
use std::task::Poll;

/// Builds a body of `chunks` 1 KiB chunks, decodes it, and returns the
/// decoded length plus the number of times the input stream was polled.
fn decode_with_polls(chunks: usize) -> (usize, usize) {
    let mut wire = Vec::new();
    for index in 0..chunks {
        wire.extend_from_slice(b"400\r\n");
        wire.extend_from_slice(&vec![u8::try_from(index % 251).unwrap(); 1024]);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");

    let fragments: Vec<&[u8]> = wire.chunks(1024).collect();
    let polls = Rc::new(Cell::new(0_usize));
    let polls_for_stream = Rc::clone(&polls);
    let mut iter = items(&fragments).into_iter();
    let body = futures::stream::poll_fn(move |_cx| {
        polls_for_stream.set(polls_for_stream.get() + 1);
        Poll::Ready(iter.next())
    });

    let declared = chunks * 1024;
    let (payload, error) = drain(ChunkedStream::unsigned(body, declared, Limits::default()));
    assert!(error.is_none(), "{error:?}");
    assert_eq!(payload.len(), declared);
    (payload.len(), polls.get())
}

#[test]
fn polls_grow_linearly_with_the_body() {
    let (payload_a, polls_a) = decode_with_polls(64);
    let (payload_b, polls_b) = decode_with_polls(128);

    assert_eq!(payload_b, 2 * payload_a, "the doubling fixture is wrong");
    // Doubling the body must at most double the polls (plus a constant term).
    assert!(polls_b <= 2 * polls_a + 8, "polls grew faster than the body: {polls_a} -> {polls_b}");

    let (payload_c, polls_c) = decode_with_polls(256);
    assert_eq!(payload_c, 4 * payload_a);
    assert!(polls_c <= 4 * polls_a + 8, "polls grew faster than the body: {polls_a} -> {polls_c}");
}

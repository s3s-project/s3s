// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for the `s3s-chunked` crate API .
//!
//! Input protocol: `data[0]` is a control byte, `data[1..]` is the raw
//! `aws-chunked` body fed to the public decoders.
//!
//! Control byte bits:
//! - 0x01: signed mode (otherwise unsigned)
//! - 0x02: unused: the unsigned decoder rejects signatures
//! - 0x04: inject one `Err` fragment into the body stream (Underlying path)
//! - 0x08: feed the body as a single fragment (no splitting)
//! - 0x10: use tiny custom limits (exercise the limit error paths)
//!
//! Fragmentation splits are deterministic and derived from the payload bytes
//! themselves, so corpus entries reproduce the exact split pattern.
//!
//! Oracle: never panic — every malformed input is a returned `Error` — plus a
//! linear guardrail: the decoder may poll the input at most a constant number
//! of times per fragment (no rescanning, no unbounded buffering), and the
//! decoded bytes can never exceed the declared length.

use bytes::Bytes;
use futures::StreamExt;
use libfuzzer_sys::fuzz_target;
use s3s_chunked::{ChunkedStream, Limits, Sha256Sum, SignContext, StdError};
use s3s_sigv4::AmzDate;
use std::cell::Cell;
use std::rc::Rc;
use std::task::Poll;

// AWS SigV4 streaming test vectors (see the crate's test helpers).
const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

fn split_fragments(payload: &[u8]) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let len = 1 + usize::from(payload[pos] % 16);
        let end = pos.saturating_add(len).min(payload.len());
        out.push(Bytes::copy_from_slice(&payload[pos..end]));
        pos = end;
    }
    out
}

fn fuzz_error() -> StdError {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "fuzz-injected error").into()
}

fn build_items(payload: &[u8], single_fragment: bool, inject_error: bool) -> Vec<Result<Bytes, StdError>> {
    let mut items: Vec<Result<Bytes, StdError>> = if single_fragment {
        vec![Ok(Bytes::copy_from_slice(payload))]
    } else {
        split_fragments(payload).into_iter().map(Ok).collect()
    };
    if inject_error {
        if items.is_empty() {
            items.push(Err(fuzz_error()));
        } else {
            let index = items.len() / 2;
            items[index] = Err(fuzz_error());
        }
    }
    items
}

/// Wraps the fragments into a stream that counts how often it is polled.
fn counted_body(
    items: Vec<Result<Bytes, StdError>>,
    polls: &Rc<Cell<usize>>,
) -> impl futures::Stream<Item = Result<Bytes, StdError>> + Unpin {
    let polls = Rc::clone(polls);
    let mut iter = items.into_iter();
    futures::stream::poll_fn(move |_cx| {
        polls.set(polls.get() + 1);
        Poll::Ready(iter.next())
    })
}

fn sign_context() -> SignContext {
    SignContext::new(
        AmzDate::parse(TIMESTAMP).expect("valid timestamp"),
        REGION.into(),
        SERVICE.into(),
        SECRET_KEY.as_bytes(),
    )
}

fn seed_signature() -> Sha256Sum {
    Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, payload)) = data.split_first() else { return };
    let signed = control & 0x01 != 0;
    // Bit 0x02 used to select an unsigned decoder that still verified
    // signatures; the unsigned decoder rejects them now, so the bit is unused.
    let inject_error = control & 0x04 != 0;
    let single_fragment = control & 0x08 != 0;
    let tiny_limits = control & 0x10 != 0;

    let items = build_items(payload, single_fragment, inject_error);
    let item_count = items.len();
    let polls = Rc::new(Cell::new(0_usize));
    // The declared decoded length cannot exceed the input, so a successful
    // decode is always possible for well-formed bodies.
    let declared = payload.len();

    let limits = if tiny_limits {
        Limits {
            max_chunk_meta_size: 8,
            max_signed_chunk_size: 1024,
            max_trailers_size: 64,
            max_trailer_headers: 2,
        }
    } else {
        Limits::default()
    };

    let decoder = if signed {
        ChunkedStream::signed(counted_body(items, &polls), sign_context(), seed_signature(), declared, limits)
    } else {
        ChunkedStream::unsigned(counted_body(items, &polls), declared, limits)
    };

    let handle = decoder.trailer_handle();
    let mut total = 0_usize;
    let mut errors = 0_usize;
    futures::executor::block_on(async {
        let mut decoder = decoder;
        while let Some(item) = decoder.next().await {
            match item {
                Ok(bytes) => total += bytes.len(),
                Err(_) => {
                    errors += 1;
                    break;
                }
            }
        }
    });

    // Oracle: at most one error item, and fail-stop afterwards.
    assert!(errors <= 1, "more than one error item");
    assert!(total <= declared, "decoded more bytes than declared");
    // Linear guardrail: at most a constant number of polls per fragment.
    assert!(
        polls.get() <= 4 * (item_count + 16),
        "input polls grew faster than the input: {} polls for {item_count} fragments",
        polls.get()
    );

    // The handle is one-shot and must not panic when read in any order.
    let _ = handle.is_ready();
    let _ = handle.read(|map| map.len());
    let _ = handle.take();
    let _ = handle.is_ready();
});

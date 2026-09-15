// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for the standalone `s3s-multipart` parser.
//!
//! Input protocol:
//! - `data[0]` control byte (bits combine):
//!   - 0x01 inject one recognizable `Err` fragment
//!   - 0x02 feed the payload as a single fragment (the O2 reference side)
//!   - 0x04 consume the first file part through `take_data_stream`
//!   - 0x08 walk the parts through the skip path (each `Part` is dropped unread)
//!   - 0x10 use a small `max_buffer_size` (32)
//!   - 0x20 consume the first file part through `into_final`
//!   - 0x40 interleave an empty chunk before every fragment
//! - `data[1..]` payload.
//!
//! The delivery and the parsing runs live in the `s3s_fuzz` library, so the
//! committed corpus is checked against the same code (see `bin/check_seeds.rs`).
//!
//! Oracles:
//! - O1: no panic for any input and any consumption strategy.
//! - O2: the same payload parses to the same outcome however it is split.
//! - O3: `next_data`, `take_data_stream` and `into_final` yield the same bytes
//!   for the first file part.
//! - O4: the parser never polls the stream more often than it was handed
//!   fragments (plus one) and never receives more bytes than the payload
//!   holds.
//! - O5: an injected error comes back unchanged rather than rewrapped.
//! - O6: walking parts makes progress — every part is handed out exactly once
//!   and the walk ends within a bound derived from the payload.
//!
//! Two boundaries are deliberate and are not asserted across:
//! - O2 is skipped when 0x40 is set. A run of empty chunks is bounded in
//!   total (see `StreamBuffer::poll_stream`), so a stream that keeps answering
//!   with them is *meant* to fail: that is a delivery property, not a parse
//!   result. The pair is still compared for the fragmentation shapes that
//!   carry bytes only.
//! - A run that ends in an error keeps whatever parts it had already produced:
//!   inputs where one strategy fails earlier than another are compared by
//!   outcome, not re-parsed.
//!
//! Every loop here is bounded by the input and every error abandons the parser
//! instance, so a parser that re-delivers a part, or loops without progress,
//! fails an assertion instead of hanging the fuzzer.

use futures::executor::block_on;
use futures::stream;
use libfuzzer_sys::fuzz_target;
use s3s_fuzz::{INJECTED_LIMIT, OutcomeKind, make_items, max_buffer_size, run_counted, run_full, run_skip, run_taken};
use s3s_multipart::{Boundary, Error, Multipart};

/// O5: the parser hands an injected error back unchanged.
fn injected_error_comes_back_unchanged() {
    let items = vec![Err(Error::HeaderSizeExceeded { limit: INJECTED_LIMIT })];
    let mut multipart = Multipart::new(stream::iter(items), &Boundary::new(b"boundary").unwrap(), max_buffer_size(0));
    let err = block_on(async { multipart.next_part().await.err() });
    assert!(
        matches!(err, Some(Error::HeaderSizeExceeded { limit }) if limit == INJECTED_LIMIT),
        "O5 injected error identity: {err:?}"
    );
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, payload)) = data.split_first() else { return };

    let full = block_on(run_full(payload, control));

    // O2: the same bytes split differently must parse the same way. Skipped for
    // 0x40, where the empty chunks themselves are a bounded delivery property.
    if control & 0x02 == 0 && control & 0x01 == 0 && control & 0x40 == 0 {
        let single = block_on(run_full(payload, control | 0x02));
        assert_eq!(full, single, "O2 chunk equivalence");
    }

    // O3: the three consumption paths agree on the file part's bytes.
    if let Some(file_part) = full.parts.iter().find(|part| part.is_file) {
        if control & 0x04 != 0 {
            let taken = block_on(run_taken(payload, control, false));
            let Some(taken) = taken else { panic!("O3 take path did not find the file part") };
            if taken.kind == OutcomeKind::Ok {
                assert_eq!(taken.data, file_part.data, "O3 next_data/take equivalence");
            }
        }
        if control & 0x20 != 0 {
            let taken = block_on(run_taken(payload, control, true));
            let Some(taken) = taken else { panic!("O3 final path did not find the file part") };
            if taken.kind == OutcomeKind::Ok {
                assert_eq!(taken.data, file_part.data, "O3 next_data/into_final equivalence");
            }
        }
    }

    // O6: skipping parts visits each of them exactly once and terminates.
    if control & 0x08 != 0 {
        let visits = block_on(run_skip(payload, control));
        if full.kind == OutcomeKind::Ok {
            assert_eq!(visits, full.parts.len(), "O6 part walk");
        }
    }

    if control & 0x01 != 0 {
        injected_error_comes_back_unchanged();
    }

    // O4: the parser does not keep polling a finished stream, and the wrapped
    // stream cannot deliver more bytes than the payload holds.
    let (_, poll_count, byte_count) = block_on(run_counted(payload, control));
    assert!(
        poll_count <= make_items(payload, control).len().saturating_add(1),
        "O4 poll guard: {poll_count}"
    );
    assert!(byte_count <= payload.len(), "O4 byte guard: {byte_count}");
});

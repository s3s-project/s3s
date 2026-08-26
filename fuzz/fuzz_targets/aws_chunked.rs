// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for the aws-chunked stream decoder.
//!
//! Input protocol: `data[0]` is a control byte, `data[1..]` is the raw body
//! payload fed to `AwsChunkedStream` as an asynchronous byte stream.
//!
//! Control byte bits:
//! - 0x01: unsigned mode (per-chunk signatures not required)
//! - 0x02: inject one `Err` fragment into the body stream (Underlying path)
//! - 0x04: feed the body as a single fragment (no splitting)
//!
//! Fragmentation splits are deterministic and derived from the payload
//! bytes themselves, so corpus entries reproduce the exact split pattern
//! (including CRLF spans crossing fragment boundaries).
//!
//! Oracle: the decoder must never panic — every malformed input is a
//! returned `AwsChunkedStreamError`.

use bytes::Bytes;
use futures::StreamExt;
use libfuzzer_sys::fuzz_target;
use s3s::AmzDate;
use s3s::AwsChunkedStream;
use s3s::Sha256Sum;
use s3s::StdError;
use s3s::auth::SecretKey;

// AWS SigV4 streaming test vectors (see aws_chunked_stream.rs tests).
const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
// Mirrors `config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE` (pub(crate)).
const MAX_CHUNK_SIZE: usize = 256 * 1024 * 1024;

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

fn build_items(payload: &[u8], single_fragment: bool, inject_error: bool) -> Vec<Result<Bytes, StdError>> {
    let mut items: Vec<Result<Bytes, StdError>> = if single_fragment {
        vec![Ok(Bytes::copy_from_slice(payload))]
    } else {
        split_fragments(payload).into_iter().map(Ok).collect()
    };
    if inject_error && !items.is_empty() {
        let idx = items.len() / 2;
        let err: StdError = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "fuzz-injected error").into();
        items[idx] = Err(err);
    }
    items
}

fuzz_target!(|data: &[u8]| {
    let Some((&control, payload)) = data.split_first() else { return };
    let unsigned = control & 0x01 != 0;
    let inject_error = control & 0x02 != 0;
    let single_fragment = control & 0x04 != 0;

    let items = build_items(payload, single_fragment, inject_error);
    let body = futures::stream::iter(items);

    futures::executor::block_on(async {
        let mut stream = AwsChunkedStream::new(
            body,
            Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature"),
            AmzDate::parse(TIMESTAMP).expect("valid timestamp"),
            REGION.into(),
            SERVICE.into(),
            SecretKey::from(SECRET_KEY),
            0,
            unsigned,
            MAX_CHUNK_SIZE,
        );
        while let Some(item) = stream.next().await {
            let _ = item; // oracle: failures are returned errors, never panics
        }
    });
});

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Decoding of the shapes real SDK clients produce.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::common::{body, drain, items, seed, sign_context, trailer_entries};
use s3s_chunked::{ChunkedStream, Limits};

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

#[test]
fn signed_64kib_chunk() {
    let data = pattern(64 * 1024);
    let wire = body(std::slice::from_ref(&data), true, None);
    let stream = ChunkedStream::signed(
        futures::stream::iter(items(&[&wire])),
        sign_context(),
        seed(),
        data.len(),
        Limits::default(),
    );
    let (payload, error) = drain(stream);
    assert_eq!(payload, data);
    assert!(error.is_none(), "{error:?}");
}

#[test]
fn signed_1mib_chunk() {
    let data = pattern(1024 * 1024);
    let wire = body(std::slice::from_ref(&data), true, None);
    let stream = ChunkedStream::signed(
        futures::stream::iter(items(&[&wire])),
        sign_context(),
        seed(),
        data.len(),
        Limits::default(),
    );
    let (payload, error) = drain(stream);
    assert_eq!(payload, data);
    assert!(error.is_none(), "{error:?}");
}

#[test]
fn signed_small_chunks_with_signed_trailers() {
    let chunks: Vec<Vec<u8>> = (0..8).map(|i| pattern(1024 + i)).collect();
    let expected: Vec<u8> = chunks.concat();
    let wire = body(&chunks, true, Some((&[("x-amz-checksum-crc32", "AAAAAA==")], true)));
    let stream = ChunkedStream::signed(
        futures::stream::iter(items(&[&wire])),
        sign_context(),
        seed(),
        expected.len(),
        Limits::default(),
    );
    let handle = stream.trailer_handle();
    let (payload, error) = drain(stream);
    assert_eq!(payload, expected);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(trailer_entries(&handle), Some(vec!["x-amz-checksum-crc32:AAAAAA==".to_owned()]));
}

#[test]
fn unsigned_https_trailer_shape() {
    // HTTPS clients send unsigned chunks with a checksum trailer.
    let chunks: Vec<Vec<u8>> = (0..4).map(|i| pattern(16 * 1024 + i)).collect();
    let expected: Vec<u8> = chunks.concat();
    let wire = body(&chunks, false, Some((&[("x-amz-checksum-sha256", "AAAAAA==")], false)));
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), expected.len(), Limits::default());
    let handle = stream.trailer_handle();
    let (payload, error) = drain(stream);
    assert_eq!(payload, expected);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(trailer_entries(&handle), Some(vec!["x-amz-checksum-sha256:AAAAAA==".to_owned()]));
}

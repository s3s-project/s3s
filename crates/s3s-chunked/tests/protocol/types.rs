// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The documented decoder surface: accessors, forwarded limits and the digest type.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::common::{body, drain, items, seed, sign_context, trailer_entries};
use futures_core::Stream;
use s3s_chunked::{ChunkedStream, Error, Limits};

#[test]
fn accessors_report_the_documented_values() {
    let unsigned = b"5\r\nhello\r\n0\r\n\r\n";
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[unsigned])), 5, Limits::default());
    assert_eq!(stream.exact_remaining_length(), 5);
    assert_eq!(Stream::size_hint(&stream), (0, None));
    assert!(!stream.trailer_handle().is_ready());
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none());

    let wire = body(&[b"hello".to_vec()], true, Some((&[("x-amz-meta-a", "1")], true)));
    let stream = ChunkedStream::signed(futures::stream::iter(items(&[&wire])), sign_context(), seed(), 5, Limits::default());
    assert_eq!(stream.exact_remaining_length(), 5);
    let handle = stream.trailer_handle();
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none());
    assert_eq!(trailer_entries(&handle), Some(vec!["x-amz-meta-a:1".to_owned()]));
}

#[test]
fn limits_are_forwarded_to_the_unsigned_decoder() {
    let wire: &[u8] = b"5\r\nhello\r\n0\r\n\r\n";
    let stream = ChunkedStream::unsigned(
        futures::stream::iter(items(&[wire])),
        5,
        Limits {
            max_chunk_meta_size: 2,
            ..Limits::default()
        },
    );
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::ChunkMetaTooLarge(..))), "{error:?}");
}

#[test]
fn digest_accessors_are_public() {
    let digest = seed();
    assert_eq!(digest.as_bytes().len(), 32);
    assert!(digest.ct_equal(&seed()));
    assert!(!digest.ct_equal(&s3s_chunked::Sha256Sum::from_bytes([0; 32])));
}

#[test]
fn into_inner_returns_the_input() {
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"0\r\n\r\n"])), 0, Limits::default());
    let _inner = stream.into_inner();

    let wire = body(&[], true, None);
    let stream = ChunkedStream::signed(futures::stream::iter(items(&[&wire])), sign_context(), seed(), 0, Limits::default());
    let _inner = stream.into_inner();
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Framing and limit behaviour across fragments.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::common::{Step, body, drain, fragmented, items, seed, sign_context, steps, trailer_entries};
use bytes::Bytes;
use futures::StreamExt;
use s3s_chunked::{ChunkedStream, Error, Limits};

#[test]
fn framing_is_fragment_independent() {
    let unsigned = b"5\r\nhello\r\n3\r\n123\r\n0\r\n\r\n".to_vec();
    let signed = body(&[b"hello".to_vec(), b"123".to_vec()], true, None);

    for (name, wire, declared, signed_mode) in [("unsigned", unsigned, 8, false), ("signed", signed, 8, true)] {
        for size in 1..=wire.len() {
            let stream = if signed_mode {
                ChunkedStream::signed(
                    futures::stream::iter(fragmented(&wire, size)),
                    sign_context(),
                    seed(),
                    declared,
                    Limits::default(),
                )
            } else {
                ChunkedStream::unsigned(futures::stream::iter(fragmented(&wire, size)), declared, Limits::default())
            };
            let (payload, error) = drain(stream);
            assert_eq!(payload, b"hello123", "{name} fragment size {size}");
            assert!(error.is_none(), "{name} fragment size {size}: {error:?}");
        }
    }
}

#[test]
fn empty_fragments_are_skipped() {
    let wire = b"5\r\nhello\r\n0\r\n\r\n";
    let mut fragments: Vec<&[u8]> = Vec::new();
    for byte in wire {
        fragments.push(b"");
        fragments.push(std::slice::from_ref(byte));
    }
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&fragments)), 5, Limits::default());
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none());
}

#[test]
fn eof_variants_are_accepted() {
    for wire in [
        b"5\r\nhello\r\n0\r\n\r\n".as_slice(),
        b"5\r\nhello\r\n0\r\n".as_slice(),
        b"5\r\nhello\r\n0".as_slice(),
    ] {
        let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[wire])), 5, Limits::default());
        let (payload, error) = drain(stream);
        assert_eq!(payload, b"hello", "{wire:?}");
        assert!(error.is_none(), "{wire:?}: {error:?}");
    }
}

#[test]
fn limits_are_configurable() {
    let limits = Limits {
        max_chunk_meta_size: 4,
        max_trailers_size: 8,
        max_trailer_headers: 1,
        ..Limits::default()
    };

    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"0000005\r\nhello\r\n0\r\n\r\n"])), 5, limits);
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::ChunkMetaTooLarge(_, 4))), "{error:?}");

    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"0\r\nx-amz-a:1\r\n\r\n"])), 0, limits);
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::TrailersTooLarge(_, 8))), "{error:?}");

    let header_limits = Limits {
        max_trailer_headers: 1,
        ..Limits::default()
    };
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"0\r\na:1\r\nb:2\r\n\r\n"])), 0, header_limits);
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::TooManyTrailerHeaders(2, 1))), "{error:?}");
}

#[test]
fn limits_are_inclusive_at_the_boundary() {
    // A chunk metadata line of exactly `max_chunk_meta_size` bytes is accepted.
    let stream = ChunkedStream::unsigned(
        futures::stream::iter(items(&[b"5\r\nhello\r\n0\r\n\r\n"])),
        5,
        Limits {
            max_chunk_meta_size: 3,
            ..Limits::default()
        },
    );
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none(), "{error:?}");

    // A trailer block of exactly `max_trailers_size` bytes is accepted;
    // the block is `x-amz-a:1\r\n\r\n` (13 bytes including the blank line).
    let stream = ChunkedStream::unsigned(
        futures::stream::iter(items(&[b"0\r\nx-amz-a:1\r\n\r\n"])),
        0,
        Limits {
            max_trailers_size: 13,
            ..Limits::default()
        },
    );
    let (_, error) = drain(stream);
    assert!(error.is_none(), "{error:?}");

    // A signed chunk of exactly `max_signed_chunk_size` bytes is accepted.
    let wire = body(&[b"hello".to_vec()], true, None);
    let stream = ChunkedStream::signed(
        futures::stream::iter(items(&[&wire])),
        sign_context(),
        seed(),
        5,
        Limits {
            max_signed_chunk_size: 5,
            ..Limits::default()
        },
    );
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none(), "{error:?}");
}

#[test]
fn partial_meta_at_the_limit_reports_eof_not_size() {
    // `5abc` is exactly `max_chunk_meta_size` bytes without a newline: the
    // decoder must report the truncated stream, not a size overflow.
    let stream = ChunkedStream::unsigned(
        futures::stream::iter(items(&[b"5abc"])),
        5,
        Limits {
            max_chunk_meta_size: 4,
            ..Limits::default()
        },
    );
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::Incomplete)), "{error:?}");
}

#[test]
fn extra_carriage_returns_are_rejected() {
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"5\r\nhello\r\r\n"])), 5, Limits::default());
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::FormatError)), "{error:?}");
}

#[test]
fn trailer_signature_without_signature_support_is_rejected() {
    let wire = body(&[], false, Some((&[("x-amz-a", "1")], true)));
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), 0, Limits::default());
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::FormatError)), "{error:?}");
}

#[test]
fn the_stream_is_fail_stop() {
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"zz\r\n"])), 0, Limits::default());
    let produced = futures::executor::block_on(async {
        let mut stream = stream;
        let mut produced = Vec::new();
        for _ in 0..4 {
            match stream.next().await {
                Some(item) => produced.push(item),
                None => break,
            }
        }
        produced
    });
    assert!(matches!(produced.as_slice(), [Err(Error::FormatError)]), "{produced:?}");
}

#[test]
fn error_classification_is_pinned() {
    let cases: &[(&str, &[&[u8]], usize, &str)] = &[
        ("overrun", &[b"5\r\nhello\r\n0\r\n\r\n"], 3, "LengthMismatch"),
        ("short", &[b"5\r\nhello\r\n0\r\n\r\n"], 10, "Incomplete"),
        ("eof-mid-data", &[b"5\r\nhel"], 5, "Incomplete"),
        ("eof-mid-crlf", &[b"5\r\nhello\r"], 5, "Incomplete"),
        ("bad-size", &[b"zz\r\n"], 0, "FormatError"),
        ("missing-crlf", &[b"5\r\nhelloXX"], 5, "FormatError"),
        ("trailer-no-colon", &[b"0\r\nnocolon\r\n\r\n"], 0, "FormatError"),
        ("truncated-signature", &[b"5;chunk-signature=abc\r\n"], 0, "FormatError"),
    ];

    for (name, fragments, declared, expected) in cases {
        let stream = ChunkedStream::unsigned(futures::stream::iter(items(fragments)), *declared, Limits::default());
        let (_, error) = drain(stream);
        let got = error.as_ref().map(error_name);
        assert_eq!(got.as_deref(), Some(*expected), "{name}: {error:?}");
    }
}

#[test]
fn pending_fragments_are_propagated() {
    let cases: Vec<(&str, Vec<Step>, usize, &[u8])> = vec![
        (
            "meta",
            vec![Step::Pending, Step::Data(Bytes::from_static(b"5\r\nhello\r\n0\r\n\r\n"))],
            5,
            b"hello".as_slice(),
        ),
        (
            "partial-meta",
            vec![
                Step::Data(Bytes::from_static(b"5")),
                Step::Pending,
                Step::Data(Bytes::from_static(b"\r\nhello\r\n0\r\n\r\n")),
            ],
            5,
            b"hello".as_slice(),
        ),
        (
            "data",
            vec![
                Step::Data(Bytes::from_static(b"5\r\n")),
                Step::Pending,
                Step::Data(Bytes::from_static(b"hello\r\n0\r\n\r\n")),
            ],
            5,
            b"hello".as_slice(),
        ),
        (
            "crlf",
            vec![
                Step::Data(Bytes::from_static(b"5\r\nhello")),
                Step::Pending,
                Step::Data(Bytes::from_static(b"\r\n0\r\n\r\n")),
            ],
            5,
            b"hello".as_slice(),
        ),
        (
            "trailers",
            vec![
                Step::Data(Bytes::from_static(b"5\r\nhello\r\n0\r\n")),
                Step::Pending,
                Step::Data(Bytes::from_static(b"\r\n")),
            ],
            5,
            b"hello".as_slice(),
        ),
    ];

    for (name, input, declared, expected) in cases {
        let stream = ChunkedStream::unsigned(steps(input), declared, Limits::default());
        let (payload, error) = drain(stream);
        assert_eq!(payload, expected, "{name}");
        assert!(error.is_none(), "{name}: {error:?}");
    }
}

#[test]
fn underlying_errors_are_propagated_from_every_phase() {
    let cases: Vec<(&str, Vec<Step>)> = vec![
        ("meta", vec![Step::Error]),
        ("data", vec![Step::Data(Bytes::from_static(b"5\r\n")), Step::Error]),
        ("crlf", vec![Step::Data(Bytes::from_static(b"5\r\nhello")), Step::Error]),
        ("trailers", vec![Step::Data(Bytes::from_static(b"5\r\nhello\r\n0\r\n")), Step::Error]),
    ];

    for (name, input) in cases {
        let stream = ChunkedStream::unsigned(steps(input), 5, Limits::default());
        let (_, error) = drain(stream);
        assert!(matches!(error, Some(Error::Underlying(_))), "{name}: {error:?}");
    }
}

#[test]
fn eof_at_a_chunk_boundary_with_remaining_length_is_incomplete() {
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"5\r\nhello\r\n"])), 10, Limits::default());
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(matches!(error, Some(Error::Incomplete)), "{error:?}");
}

#[test]
fn oversized_meta_split_across_fragments_is_rejected() {
    let long = vec![b'a'; 600];
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[long.as_slice(), long.as_slice()])), 0, Limits::default());
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::ChunkMetaTooLarge(1200, 1024))), "{error:?}");
}

#[test]
fn meta_signature_with_trailing_garbage_is_rejected() {
    let mut wire = b"5;chunk-signature=".to_vec();
    wire.extend_from_slice(&[b'a'; 64]);
    wire.extend_from_slice(b";\r\n");
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), 0, Limits::default());
    let (_, error) = drain(stream);
    assert!(matches!(error, Some(Error::FormatError)), "{error:?}");
}

fn error_name(error: &Error) -> String {
    match error {
        Error::Underlying(_) => "Underlying".to_owned(),
        Error::SignatureMismatch => "SignatureMismatch".to_owned(),
        Error::FormatError => "FormatError".to_owned(),
        Error::Incomplete => "Incomplete".to_owned(),
        Error::LengthMismatch => "LengthMismatch".to_owned(),
        Error::ChunkMetaTooLarge(..) => "ChunkMetaTooLarge".to_owned(),
        Error::ChunkDataTooLarge(..) => "ChunkDataTooLarge".to_owned(),
        Error::TrailersTooLarge(..) => "TrailersTooLarge".to_owned(),
        Error::TooManyTrailerHeaders(..) => "TooManyTrailerHeaders".to_owned(),
        Error::TrailersMissing => "TrailersMissing".to_owned(),
        Error::TrailersEmpty => "TrailersEmpty".to_owned(),
    }
}

#[test]
fn trailer_entries_are_exposed_and_verified() {
    let wire = body(
        &[b"hello".to_vec()],
        true,
        Some((&[("x-amz-checksum-crc32", "AAAAAA=="), ("x-amz-meta-a", "1")], true)),
    );
    let stream = ChunkedStream::signed(futures::stream::iter(items(&[&wire])), sign_context(), seed(), 5, Limits::default());
    let handle = stream.trailer_handle();
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        trailer_entries(&handle),
        Some(vec!["x-amz-checksum-crc32:AAAAAA==".to_owned(), "x-amz-meta-a:1".to_owned()])
    );
    assert!(handle.take().is_none());
}

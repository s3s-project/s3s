// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Frozen differential fixtures.
//!
//! The expectations below were produced by the legacy `AwsChunkedStream`
//! oracle of the `s3s` crate.
//! They stay in the crate so the framing, trailer and length-accounting
//! behaviour remains pinned after the legacy implementation is deleted.
//!
//! One case is pinned against the intended contract instead of the oracle: a
//! signature that arrives while the request was declared unsigned is rejected,
//! because the unsigned decoder has no signing context to verify it with.

use bytes::Bytes;
use futures::StreamExt;
use s3s_chunked::{ChunkedStream, Error, Limits, StdError};

type OkCase<'a> = (&'a str, &'a [&'a [u8]], usize, &'a str, usize, Option<&'a [&'a str]>);
type ErrCase<'a> = (&'a str, &'a [&'a [u8]], usize, &'a str, usize);

struct Outcome {
    payload: Vec<u8>,
    error: Option<String>,
    remaining: usize,
    trailers: Option<Vec<String>>,
}

fn decode(fragments: &[&[u8]], declared_len: usize) -> Outcome {
    let items: Vec<Result<Bytes, StdError>> = fragments.iter().map(|f| Ok(Bytes::copy_from_slice(f))).collect();
    let mut stream = ChunkedStream::unsigned(futures::stream::iter(items), declared_len, Limits::default());
    let handle = stream.trailer_handle();
    let (payload, error) = futures::executor::block_on(async {
        let mut payload = Vec::new();
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => payload.extend_from_slice(&bytes),
                Err(err) => {
                    error = Some(error_name(&err));
                    break;
                }
            }
        }
        (payload, error)
    });
    let remaining = stream.exact_remaining_length();
    let trailers = handle.take().map(|map| {
        let mut entries: Vec<String> = map
            .iter()
            .map(|(name, value)| format!("{name}:{}", value.to_str().unwrap_or("<binary>")))
            .collect();
        entries.sort();
        entries
    });
    Outcome {
        payload,
        error,
        remaining,
        trailers,
    }
}

fn error_name(error: &Error) -> String {
    match error {
        Error::Underlying(_) => "Underlying".to_owned(),
        Error::SignatureMismatch => "SignatureMismatch".to_owned(),
        Error::FormatError => "FormatError".to_owned(),
        Error::Incomplete => "Incomplete".to_owned(),
        Error::LengthMismatch => "LengthMismatch".to_owned(),
        Error::ChunkMetaTooLarge(size, limit) => format!("ChunkMetaTooLarge({size},{limit})"),
        Error::ChunkDataTooLarge(size, limit) => format!("ChunkDataTooLarge({size},{limit})"),
        Error::TrailersTooLarge(size, limit) => format!("TrailersTooLarge({size},{limit})"),
        Error::TooManyTrailerHeaders(count, limit) => format!("TooManyTrailerHeaders({count},{limit})"),
        Error::TrailersMissing => "TrailersMissing".to_owned(),
        Error::TrailersEmpty => "TrailersEmpty".to_owned(),
    }
}

const BODY: &[u8] = b"5\r\nhello\r\n0\r\n\r\n";

#[test]
fn frozen_legacy_expectations_hold() {
    let one_byte: Vec<&[u8]> = BODY.chunks(1).collect();
    let mut meta_too_large = vec![b'a'; 1025];
    meta_too_large.push(b'\n');

    let cases: &[OkCase] = &[
        ("simple", &[BODY], 5, "hello", 0, None),
        ("fragmented", &one_byte, 5, "hello", 0, None),
        ("empty-body", &[b""], 0, "", 0, None),
        ("terminator-only", &[b"0\r\n\r\n"], 0, "", 0, None),
        ("terminator-without-crlf", &[b"5\r\nhello\r\n0\r\n"], 5, "hello", 0, None),
        (
            "trailers",
            &[b"0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n"],
            0,
            "",
            0,
            Some(&["x-amz-checksum-crc32:AAAAAA=="]),
        ),
        (
            "duplicate-trailers",
            &[b"0\r\nx-amz-meta-a:1\r\nx-amz-meta-a:2\r\n\r\n"],
            0,
            "",
            0,
            Some(&["x-amz-meta-a:1", "x-amz-meta-a:2"]),
        ),
        ("eof-mid-data", &[b"5\r\nhel"], 5, "hel", 2, None),
        ("malformed-size", &[b"zz\r\n"], 0, "", 0, None),
        ("meta-limit", &[&meta_too_large], 0, "", 0, None),
    ];

    for (name, fragments, declared_len, payload, remaining, trailers) in cases {
        let outcome = decode(fragments, *declared_len);
        assert_eq!(outcome.payload, payload.as_bytes(), "payload mismatch: {name}");
        assert_eq!(outcome.remaining, *remaining, "remaining mismatch: {name}");
        let expected: Option<Vec<String>> = trailers.as_ref().map(|list| list.iter().map(|s| (*s).to_owned()).collect());
        assert_eq!(outcome.trailers, expected, "trailers mismatch: {name}");
    }
}

/// A signature in an unsigned declaration is a declaration/body mismatch.
///
/// The legacy oracle verifies a signature whenever one is present, even in
/// unsigned mode; this decoder rejects it, so the case is pinned against the
/// intended contract rather than the frozen oracle output.
#[test]
fn unsigned_declaration_rejects_signatures() {
    let signature = "0".repeat(64);

    let chunk_signature = format!("5;chunk-signature={signature}\r\nhello\r\n0\r\n\r\n");
    let outcome = decode(&[chunk_signature.as_bytes()], 5);
    assert_eq!(outcome.payload, b"", "the chunk is rejected before its data is yielded");
    assert_eq!(outcome.error.as_deref(), Some("FormatError"));
    assert_eq!(outcome.remaining, 5);

    let trailer_signature = format!("0\r\nx-amz-meta-a:1\r\nx-amz-trailer-signature={signature}\r\n\r\n");
    let outcome = decode(&[trailer_signature.as_bytes()], 0);
    assert_eq!(outcome.error.as_deref(), Some("FormatError"));
    assert!(outcome.trailers.is_none(), "the trailing headers are not published");
}

#[test]
fn frozen_legacy_errors_hold() {
    let cases: &[ErrCase] = &[
        ("overrun", &[BODY], 3, "LengthMismatch", 3),
        ("short", &[BODY], 10, "Incomplete", 5),
        ("malformed-size", &[b"zz\r\n"], 0, "FormatError", 0),
        ("missing-crlf", &[b"5\r\nhelloXX"], 5, "FormatError", 0),
        ("trailer-without-colon", &[b"0\r\nnocolon\r\n\r\n"], 0, "FormatError", 0),
    ];

    for (name, fragments, declared_len, expected, remaining) in cases {
        let outcome = decode(fragments, *declared_len);
        assert_eq!(outcome.error.as_deref(), Some(*expected), "error mismatch: {name}");
        assert_eq!(outcome.remaining, *remaining, "remaining mismatch: {name}");
    }

    // The reported size is the projected buffer size at the check boundary, so
    // it depends on the incoming fragment: 1026 with this single fragment.
    let mut meta_too_large = vec![b'a'; 1025];
    meta_too_large.push(b'\n');
    let outcome = decode(&[&meta_too_large], 0);
    assert_eq!(outcome.error.as_deref(), Some("ChunkMetaTooLarge(1026,1024)"));
}

// Signed mode. The expectations are the ones the legacy oracle produced for the
// same bodies; the signatures are rebuilt from the documented signing key, so
// the bodies stay reproducible without the oracle.

const SIGNED_SEED: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";

fn amz_date() -> s3s_sigv4::AmzDate {
    s3s_sigv4::AmzDate::parse(TIMESTAMP).expect("valid timestamp")
}

fn seed() -> s3s_chunked::Sha256Sum {
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&SIGNED_SEED[index * 2..index * 2 + 2], 16).expect("hex seed");
    }
    s3s_chunked::Sha256Sum::from_bytes(bytes)
}

fn signature(string_to_sign: &str) -> String {
    s3s_sigv4::calculate_signature(string_to_sign, SECRET_KEY, &amz_date(), REGION, SERVICE)
}

fn chunk_signature(prev: &str, data: &[u8]) -> String {
    let string_to_sign = s3s_sigv4::create_chunk_string_to_sign(&amz_date(), REGION, SERVICE, prev, &[data]);
    signature(&string_to_sign)
}

fn trailer_signature(prev: &str, canonical: &[u8]) -> String {
    let string_to_sign = s3s_sigv4::create_trailer_string_to_sign(&amz_date(), REGION, SERVICE, prev, canonical);
    signature(&string_to_sign)
}

/// Builds a signed body: every chunk carries a chained signature, and the trailer
/// block, when present, carries one of its own.
fn signed_body(chunks: &[&[u8]], trailers: Option<&[(&str, &str)]>) -> Vec<u8> {
    let mut prev = SIGNED_SEED.to_owned();
    let mut body = Vec::new();
    for chunk in chunks {
        let signature = chunk_signature(&prev, chunk);
        body.extend_from_slice(format!("{:x};chunk-signature={signature}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(chunk);
        body.extend_from_slice(b"\r\n");
        prev = signature;
    }
    let last = chunk_signature(&prev, b"");
    body.extend_from_slice(format!("0;chunk-signature={last}\r\n").as_bytes());
    if let Some(entries) = trailers {
        let mut canonical = Vec::new();
        for (name, value) in entries {
            canonical.extend_from_slice(format!("{name}:{value}\n").as_bytes());
            body.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
        }
        let signature = trailer_signature(&last, &canonical);
        body.extend_from_slice(format!("x-amz-trailer-signature:{signature}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body
}

fn decode_signed(fragments: &[&[u8]], declared_len: usize) -> Outcome {
    let items: Vec<Result<Bytes, StdError>> = fragments.iter().map(|f| Ok(Bytes::copy_from_slice(f))).collect();
    let ctx = s3s_chunked::SignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes());
    let mut stream = ChunkedStream::signed(futures::stream::iter(items), ctx, seed(), declared_len, Limits::default());
    let handle = stream.trailer_handle();
    let (payload, error) = futures::executor::block_on(async {
        let mut payload = Vec::new();
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => payload.extend_from_slice(&bytes),
                Err(err) => {
                    error = Some(error_name(&err));
                    break;
                }
            }
        }
        (payload, error)
    });
    let remaining = stream.exact_remaining_length();
    let trailers = handle.take().map(|map| {
        let mut entries: Vec<String> = map
            .iter()
            .map(|(name, value)| format!("{name}:{}", value.to_str().unwrap_or("<binary>")))
            .collect();
        entries.sort();
        entries
    });
    Outcome {
        payload,
        error,
        remaining,
        trailers,
    }
}

#[test]
fn signed_bodies_are_verified_and_pinned() {
    let body = signed_body(&[b"hello"], None);

    let outcome = decode_signed(&[&body], 5);
    assert_eq!(outcome.payload, b"hello");
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.remaining, 0);

    // The framing must not depend on how the transport splits the body.
    let one_byte: Vec<&[u8]> = body.chunks(1).collect();
    let outcome = decode_signed(&one_byte, 5);
    assert_eq!(outcome.payload, b"hello");
    assert_eq!(outcome.error, None);

    // A tampered chunk signature is rejected before any data is yielded.
    let mut tampered = signed_body(&[b"hello"], None);
    let marker = b";chunk-signature=";
    let at = tampered
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("chunk signature")
        + marker.len();
    tampered[at] = if tampered[at] == b'a' { b'b' } else { b'a' };
    let outcome = decode_signed(&[&tampered], 5);
    assert_eq!(outcome.payload, b"");
    assert_eq!(outcome.error.as_deref(), Some("SignatureMismatch"));
}

#[test]
fn signed_trailers_are_verified_and_published() {
    let body = signed_body(&[b"hello"], Some(&[("x-amz-meta-a", "1")]));

    let outcome = decode_signed(&[&body], 5);
    assert_eq!(outcome.payload, b"hello");
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.trailers, Some(vec!["x-amz-meta-a:1".to_owned()]));

    // A trailer signature that does not match publishes no headers.
    let mut tampered = body.clone();
    let marker = b"x-amz-trailer-signature:";
    let at = tampered
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("trailer signature")
        + marker.len();
    tampered[at] = if tampered[at] == b'a' { b'b' } else { b'a' };
    let outcome = decode_signed(&[&tampered], 5);
    assert_eq!(outcome.error.as_deref(), Some("SignatureMismatch"));
    assert!(outcome.trailers.is_none(), "a failed request publishes no trailers");
}

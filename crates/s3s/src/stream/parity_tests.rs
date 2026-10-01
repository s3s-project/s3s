// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Differential tests between the legacy `AwsChunkedStream` and `s3s-chunked`.
//!
//! Both decoders are driven with identical byte fragments. The produced
//! payload, the error classification, the declared-length bookkeeping and the
//! trailing headers must agree.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use crate::auth::SecretKey;
use crate::error::StdError;
use crate::stream::aws_chunked_stream::{AwsChunkedStream, AwsChunkedStreamError};
use crate::utils::crypto::Sha256Sum;
use bytes::Bytes;
use futures::StreamExt;
use s3s_chunked::{ChunkedStream, Error as ChunkedError, Limits, SignContext as ChunkedSignContext};
use s3s_sigv4::AmzDate;

const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
const MAX_CHUNK_SIZE: usize = 256 * 1024 * 1024;

fn amz_date() -> AmzDate {
    AmzDate::parse(TIMESTAMP).expect("valid timestamp")
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

/// Deterministic PRNG (xorshift64*).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(n).unwrap()).unwrap()
    }

    fn data(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| u8::try_from(self.next_u64() & 0xff).unwrap()).collect()
    }
}

/// One input fragment.
#[derive(Clone)]
enum Frag {
    Data(Bytes),
    Fail,
}

fn materialize(frags: &[Frag]) -> Vec<Result<Bytes, StdError>> {
    frags
        .iter()
        .map(|frag| match frag {
            Frag::Data(bytes) => Ok(bytes.clone()),
            Frag::Fail => Err(std::io::Error::other("injected").into()),
        })
        .collect()
}

/// Splits a body deterministically, occasionally inserting empty fragments.
fn fragment(body: &[u8], mut rng: Rng) -> Vec<Frag> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < body.len() {
        if rng.below(8) == 0 {
            out.push(Frag::Data(Bytes::new()));
        }
        let len = 1 + rng.below(7);
        let end = (pos + len).min(body.len());
        out.push(Frag::Data(Bytes::copy_from_slice(&body[pos..end])));
        pos = end;
    }
    if out.is_empty() {
        out.push(Frag::Data(Bytes::new()));
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
struct Summary {
    decoded: Vec<u8>,
    error: Option<String>,
    remaining: usize,
    trailers: Option<Vec<String>>,
}

fn summarize(error: Option<String>, decoded: Vec<u8>, remaining: usize, trailers: Option<Vec<String>>) -> Summary {
    Summary {
        decoded,
        error,
        remaining,
        trailers,
    }
}

fn collect(map: Option<http::HeaderMap>) -> Option<Vec<String>> {
    map.map(|map| {
        let mut entries: Vec<String> = map
            .iter()
            .map(|(name, value)| format!("{name}:{}", value.to_str().unwrap_or("<binary>")))
            .collect();
        entries.sort();
        entries
    })
}

fn legacy_error(error: &AwsChunkedStreamError) -> String {
    match error {
        AwsChunkedStreamError::Underlying(_) => "Underlying".to_owned(),
        AwsChunkedStreamError::SignatureMismatch => "SignatureMismatch".to_owned(),
        AwsChunkedStreamError::FormatError => "FormatError".to_owned(),
        AwsChunkedStreamError::Incomplete => "Incomplete".to_owned(),
        AwsChunkedStreamError::LengthMismatch => "LengthMismatch".to_owned(),
        AwsChunkedStreamError::ChunkMetaTooLarge(size, limit) => format!("ChunkMetaTooLarge({size},{limit})"),
        AwsChunkedStreamError::ChunkDataTooLarge(size, limit) => format!("ChunkDataTooLarge({size},{limit})"),
        AwsChunkedStreamError::TrailersTooLarge(size, limit) => format!("TrailersTooLarge({size},{limit})"),
        AwsChunkedStreamError::TooManyTrailerHeaders(count, limit) => {
            format!("TooManyTrailerHeaders({count},{limit})")
        }
    }
}

fn chunked_error(error: &ChunkedError) -> String {
    match error {
        ChunkedError::Underlying(_) => "Underlying".to_owned(),
        ChunkedError::SignatureMismatch => "SignatureMismatch".to_owned(),
        ChunkedError::FormatError => "FormatError".to_owned(),
        ChunkedError::Incomplete => "Incomplete".to_owned(),
        ChunkedError::LengthMismatch => "LengthMismatch".to_owned(),
        ChunkedError::ChunkMetaTooLarge(size, limit) => format!("ChunkMetaTooLarge({size},{limit})"),
        ChunkedError::ChunkDataTooLarge(size, limit) => format!("ChunkDataTooLarge({size},{limit})"),
        ChunkedError::TrailersTooLarge(size, limit) => format!("TrailersTooLarge({size},{limit})"),
        ChunkedError::TooManyTrailerHeaders(count, limit) => format!("TooManyTrailerHeaders({count},{limit})"),
    }
}

fn run_legacy(frags: &[Frag], unsigned: bool, declared_len: usize) -> Summary {
    let body = futures::stream::iter(materialize(frags));
    let mut stream = AwsChunkedStream::new(
        body,
        Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed"),
        amz_date(),
        REGION.into(),
        SERVICE.into(),
        SecretKey::from(SECRET_KEY),
        declared_len,
        unsigned,
        MAX_CHUNK_SIZE,
    );
    let handle = stream.trailing_headers_handle();
    let (decoded, error) = futures::executor::block_on(async {
        let mut decoded = Vec::new();
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => decoded.extend_from_slice(&bytes),
                Err(err) => {
                    error = Some(legacy_error(&err));
                    // The legacy stream keeps re-reporting the error forever
                    // once its inner stream is exhausted; compare the first one.
                    break;
                }
            }
        }
        (decoded, error)
    });
    let remaining = stream.exact_remaining_length();
    summarize(error, decoded, remaining, collect(handle.take()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unsigned,
    Signed,
}

fn run_chunked(frags: &[Frag], mode: Mode, declared_len: usize) -> Summary {
    let body = futures::stream::iter(materialize(frags));
    let mut stream = match mode {
        // The unsigned decoder has no signing context, so the agreement matrix
        // only feeds it signature-free bodies. Signature-bearing bodies are
        // covered by the deliberate-difference test below.
        Mode::Unsigned => ChunkedStream::unsigned(body, declared_len, Limits::default()),
        Mode::Signed => {
            let ctx = ChunkedSignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes());
            let seed = s3s_chunked::Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed");
            ChunkedStream::signed(body, ctx, seed, declared_len, Limits::default())
        }
    };
    let handle = stream.trailer_handle();
    let (decoded, error) = futures::executor::block_on(async {
        let mut decoded = Vec::new();
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => decoded.extend_from_slice(&bytes),
                Err(err) => {
                    error = Some(chunked_error(&err));
                    break;
                }
            }
        }
        (decoded, error)
    });
    let remaining = stream.exact_remaining_length();
    summarize(error, decoded, remaining, collect(handle.take()))
}

struct Case {
    name: String,
    frags: Vec<Frag>,
    mode: Mode,
    declared_len: usize,
}

fn case(name: &str, body: &[u8], mode: Mode, declared_len: usize, seed: u64) -> Case {
    Case {
        name: name.to_owned(),
        frags: fragment(body, Rng::new(seed)),
        mode,
        declared_len,
    }
}

/// Builds a body from chunks, optionally signing them and adding trailers.
fn build_body(chunks: &[Vec<u8>], sign_chunks: bool, trailers: Option<(&[(&str, &str)], bool)>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut prev = SEED_SIGNATURE.to_owned();

    for chunk in chunks {
        if sign_chunks {
            let sig = chunk_signature(&prev, chunk);
            body.extend_from_slice(format!("{:x};chunk-signature={sig}\r\n", chunk.len()).as_bytes());
            prev = sig;
        } else {
            body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        }
        body.extend_from_slice(chunk);
        body.extend_from_slice(b"\r\n");
    }

    if sign_chunks {
        let sig = chunk_signature(&prev, b"");
        body.extend_from_slice(format!("0;chunk-signature={sig}\r\n").as_bytes());
        prev = sig;
    } else {
        body.extend_from_slice(b"0\r\n");
    }

    if let Some((entries, with_signature)) = trailers {
        let mut sorted: Vec<(&str, &str)> = entries.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut canonical = Vec::new();
        for (name, value) in &sorted {
            canonical.extend_from_slice(name.as_bytes());
            canonical.push(b':');
            canonical.extend_from_slice(value.as_bytes());
            canonical.push(b'\n');
        }
        for (name, value) in &sorted {
            body.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
        }
        if with_signature {
            let sig = trailer_signature(&prev, &canonical);
            body.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
    }

    body
}

fn normal_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for seed in 1..=12u64 {
        let mut rng = Rng::new(seed);
        let mode = if seed % 3 == 0 { Mode::Signed } else { Mode::Unsigned };
        // Unsigned agreement cases stay signature-free: the strict unsigned
        // decoder rejects a chunk signature instead of verifying it.
        let sign_chunks = mode == Mode::Signed;
        let count = 1 + rng.below(3);
        let sizes = [1_usize, 15, 16, 1024];
        let mut chunks = Vec::new();
        for _ in 0..count {
            let len = sizes[rng.below(sizes.len())];
            chunks.push(rng.data(len));
        }
        let declared_len: usize = chunks.iter().map(Vec::len).sum();
        let with_trailers = seed % 4 == 0;
        let trailers = if with_trailers {
            let with_signature = mode == Mode::Signed;
            Some((&[("x-amz-meta-a", "1"), ("x-amz-checksum-crc32", "AAAAAA==")][..], with_signature))
        } else {
            None
        };
        let body = build_body(&chunks, sign_chunks, trailers);
        cases.push(Case {
            name: format!("normal-{seed}"),
            frags: fragment(&body, Rng::new(seed * 31 + 7)),
            mode,
            declared_len,
        });
    }
    cases
}

fn error_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let plain = b"5\r\nhello\r\n0\r\n\r\n".to_vec();
    cases.push(case("overrun", &plain, Mode::Unsigned, 3, 11));
    cases.push(case("short", &plain, Mode::Unsigned, 6, 12));
    cases.push(case("truncated-data", b"5\r\nhel", Mode::Unsigned, 5, 13));
    cases.push(case("bad-size", b"zz\r\n", Mode::Unsigned, 0, 14));
    cases.push(case("missing-crlf", b"5\r\nhelloXX", Mode::Unsigned, 5, 15));
    cases.push(case("trailer-no-colon", b"0\r\nnocolon\r\n\r\n", Mode::Unsigned, 0, 16));
    cases.push(case("signed-missing-signature", &plain, Mode::Signed, 5, 17));

    let tampered = chunk_signature(SEED_SIGNATURE, b"HELLO");
    let tampered_body = format!("5;chunk-signature={tampered}\r\nhello\r\n0\r\n\r\n").into_bytes();
    cases.push(case("signed-tampered", &tampered_body, Mode::Signed, 5, 18));

    let mut trailers_too_large = b"0\r\n".to_vec();
    for i in 0..2000 {
        trailers_too_large.extend_from_slice(format!("x-amz-meta-{i:04}:v\r\n").as_bytes());
    }
    trailers_too_large.extend_from_slice(b"\r\n");
    cases.push(case("trailers-too-large", &trailers_too_large, Mode::Unsigned, 0, 20));

    let mut too_many = b"0\r\n".to_vec();
    for i in 0..101 {
        too_many.extend_from_slice(format!("a{i}:v\r\n").as_bytes());
    }
    too_many.extend_from_slice(b"\r\n");
    cases.push(case("too-many-headers", &too_many, Mode::Unsigned, 0, 21));

    let signed_missing_trailer_sig = build_body(&[b"hello".to_vec()], true, Some((&[("x-amz-meta-a", "1")], false)));
    cases.push(case("signed-missing-trailer-signature", &signed_missing_trailer_sig, Mode::Signed, 5, 22));

    let signed_trailer = build_body(&[b"hello".to_vec()], true, Some((&[("x-amz-meta-a", "1")], true)));
    cases.push(case("signed-trailer", &signed_trailer, Mode::Signed, 5, 23));

    let unsigned_trailer_plain = build_body(&[b"hello".to_vec()], false, Some((&[("x-amz-meta-a", "1")], false)));
    cases.push(case("unsigned-plain-trailer", &unsigned_trailer_plain, Mode::Unsigned, 5, 25));

    cases.push(Case {
        name: "underlying-error".to_owned(),
        frags: vec![Frag::Data(Bytes::from_static(b"5\r\nhe")), Frag::Fail],
        mode: Mode::Unsigned,
        declared_len: 5,
    });

    cases
}

#[test]
fn legacy_and_chunked_decoders_agree() {
    let mut checked = 0;
    for case in normal_cases().into_iter().chain(error_cases()) {
        let legacy = run_legacy(&case.frags, case.mode == Mode::Unsigned, case.declared_len);
        let chunked = run_chunked(&case.frags, case.mode, case.declared_len);
        assert_eq!(legacy, chunked, "case `{}`", case.name);
        checked += 1;
    }
    assert!(checked >= 25, "corpus too small: {checked}");
}

/// Documents a deliberate difference: the legacy stream is not fail-stop.
///
/// After a length error it keeps yielding payload and then repeats the final
/// error forever, while s3s-chunked reports the first error once and ends.
#[test]
fn error_termination_differs_deliberately() {
    // Fragments for the body below: "5\r\nh" | "ell" | "o\r\n0\r\n\r" | "\n".
    let body = b"5\r\nhello\r\n0\r\n\r\n";
    let frags = fragment(body, Rng::new(11));

    let legacy = {
        let stream = futures::stream::iter(materialize(&frags));
        AwsChunkedStream::new(
            stream,
            Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed"),
            amz_date(),
            REGION.into(),
            SERVICE.into(),
            SecretKey::from(SECRET_KEY),
            3,
            true,
            MAX_CHUNK_SIZE,
        )
    };
    assert_eq!(
        legacy_tags(legacy, 6),
        ["Ok(1)", "LengthMismatch", "Ok(1)", "Incomplete", "Incomplete", "Incomplete"]
    );

    let chunked = {
        let stream = futures::stream::iter(materialize(&frags));
        ChunkedStream::unsigned(stream, 3, Limits::default())
    };
    assert_eq!(chunked_tags(chunked, 6), ["Ok(1)", "LengthMismatch", "None", "None", "None", "None"]);
}

fn legacy_tags(mut stream: AwsChunkedStream, polls: usize) -> Vec<String> {
    let mut tags = Vec::new();
    for _ in 0..polls {
        match futures::executor::block_on(stream.next()) {
            Some(Ok(bytes)) => tags.push(format!("Ok({})", bytes.len())),
            Some(Err(error)) => tags.push(legacy_error(&error)),
            None => tags.push("None".to_owned()),
        }
    }
    tags
}

fn chunked_tags(
    mut stream: ChunkedStream<futures::stream::Iter<std::vec::IntoIter<Result<Bytes, StdError>>>>,
    polls: usize,
) -> Vec<String> {
    let mut tags = Vec::new();
    for _ in 0..polls {
        match futures::executor::block_on(stream.next()) {
            Some(Ok(bytes)) => tags.push(format!("Ok({})", bytes.len())),
            Some(Err(error)) => tags.push(chunked_error(&error)),
            None => tags.push("None".to_owned()),
        }
    }
    tags
}

/// Documents a second deliberate difference: the legacy decoder wraps an
/// oversized metadata line into `Underlying`, which maps to a 500 response,
/// while `s3s-chunked` reports `ChunkMetaTooLarge`, which maps to 400.
#[test]
fn meta_limit_classification_differs_deliberately() {
    let mut body = vec![b'a'; 1025];
    body.push(b'\n');
    let frags = fragment(&body, Rng::new(19));

    let legacy = run_legacy(&frags, true, 0);
    assert_eq!(legacy.error.as_deref(), Some("Underlying"));

    let chunked = run_chunked(&frags, Mode::Unsigned, 0);
    assert_eq!(chunked.error.as_deref(), Some("ChunkMetaTooLarge(1025,1024)"));
}

/// Documents a third deliberate difference: a signature that arrives while the
/// request was declared unsigned.
///
/// The legacy decoder verifies a chunk or trailer signature whenever one is
/// present, even in unsigned mode, because it always carries a signing context.
/// `s3s-chunked` rejects the signature instead: the unsigned decoder has no
/// context to verify it with, so a signature is a declaration/body mismatch.
/// The legacy behaviour came from an earlier refactor and is untested, so it is
/// treated as unspecified rather than preserved.
#[test]
fn unsigned_declaration_with_signatures_differs_deliberately() {
    // A chunk signature in an unsigned declaration.
    let signed_chunks = build_body(&[b"hello".to_vec()], true, None);
    let frags = fragment(&signed_chunks, Rng::new(31));

    let legacy = run_legacy(&frags, true, 5);
    assert_eq!(legacy.error, None, "the legacy decoder verifies the chunk signature");
    assert_eq!(legacy.decoded, b"hello");

    let chunked = run_chunked(&frags, Mode::Unsigned, 5);
    assert_eq!(chunked.error.as_deref(), Some("FormatError"));
    assert!(chunked.decoded.is_empty(), "the chunk is rejected before its data is yielded");

    // A trailer signature in an unsigned declaration.
    let signed_trailer = build_body(&[b"hello".to_vec()], false, Some((&[("x-amz-meta-a", "1")], true)));
    let frags = fragment(&signed_trailer, Rng::new(24));

    let legacy = run_legacy(&frags, true, 5);
    assert_eq!(legacy.error, None, "the legacy decoder verifies the trailer signature");
    assert_eq!(legacy.trailers, Some(vec!["x-amz-meta-a:1".to_owned()]));

    let chunked = run_chunked(&frags, Mode::Unsigned, 5);
    assert_eq!(chunked.error.as_deref(), Some("FormatError"));
    assert!(chunked.trailers.is_none(), "the trailing headers are not published");
}

#[test]
fn one_byte_fragmentation_matches() {
    let declared_len = 11;
    for mode in [Mode::Unsigned, Mode::Signed] {
        // The unsigned decoder only accepts signature-free bodies.
        let signed = mode == Mode::Signed;
        let trailers = Some((&[("x-amz-meta-a", "1")][..], signed));
        let body = build_body(&[b"hello".to_vec(), b"world!".to_vec()], signed, trailers);
        let frags: Vec<Frag> = body.iter().map(|b| Frag::Data(Bytes::from(vec![*b]))).collect();
        let legacy = run_legacy(&frags, mode == Mode::Unsigned, declared_len);
        let chunked = run_chunked(&frags, mode, declared_len);
        assert_eq!(legacy, chunked, "mode {mode:?}");
    }
}

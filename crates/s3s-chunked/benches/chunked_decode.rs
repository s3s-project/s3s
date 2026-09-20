// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Push-model throughput of the `s3s-chunked` decoders .
//!
//! Every cell feeds one pre-built `aws-chunked` body from an always-ready
//! stream, which is the shape of a buffered request body: the whole body is
//! already in memory and every poll is ready. The corpus (framing, chunk
//! signatures, trailers, feed split) is built outside the timed region; the
//! timed closure only constructs the decoder and drives it, and every arm is
//! validated once with SHA-256 plus an exact byte count outside the timed
//! region, so a cell cannot be fast by silently decoding nothing.
//!
//! Run with:
//! ```bash
//! cargo bench -p s3s-chunked
//! ```

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// A uniform allocator for every benchmark in this package: the system
/// allocator's arena state is sensitive to the allocation history of earlier
/// cells, which moves later measurements.
#[global_allocator]
static BENCH_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
use futures::StreamExt;
use s3s_chunked::{ChunkedStream, Error, Limits, Sha256Sum, SignContext, StdError};
use s3s_sigv4::AmzDate;
use sha2::{Digest, Sha256};

/// The AWS `SigV4` streaming test vector.
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";

/// Target decoded payload per cell.
const TARGET_PAYLOAD: usize = 16 * 1024 * 1024;
/// Cap on the chunk count so a 1 KiB chunk cell does not build 16k chunks.
const MAX_CHUNKS: usize = 4096;

fn amz_date() -> AmzDate {
    AmzDate::parse(TIMESTAMP).expect("valid timestamp")
}

fn sign_context() -> SignContext {
    SignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes())
}

fn seed_signature() -> Sha256Sum {
    Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

fn signature(string_to_sign: &str) -> String {
    s3s_sigv4::calculate_signature(string_to_sign, SECRET_KEY, &amz_date(), REGION, SERVICE)
}

fn chunk_signature(prev: &str, data: &[u8]) -> String {
    signature(&s3s_sigv4::create_chunk_string_to_sign(&amz_date(), REGION, SERVICE, prev, &[data]))
}

/// A deterministic payload pattern.
fn pattern(len: usize, salt: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from((i + salt) % 251).unwrap()).collect()
}

/// One pre-built body plus its expected decoding result.
struct Corpus {
    /// The wire body split into feed fragments.
    feed: Vec<Bytes>,
    /// Declared `x-amz-decoded-content-length`.
    decoded_len: usize,
    /// SHA-256 of the decoded payload.
    digest: [u8; 32],
    /// Whether the body carries chunk signatures.
    signed: bool,
}

impl Corpus {
    fn build(chunk_size: usize, feed_size: usize, signed: bool, trailer: bool) -> Self {
        let chunk_count = (TARGET_PAYLOAD / chunk_size).clamp(1, MAX_CHUNKS);
        let mut wire = Vec::with_capacity(chunk_count * (chunk_size + 128));
        let mut payload = Vec::with_capacity(chunk_count * chunk_size);
        let mut prev = SEED_SIGNATURE.to_owned();

        for index in 0..chunk_count {
            let data = pattern(chunk_size, index * 7);
            if signed {
                let sig = chunk_signature(&prev, &data);
                wire.extend_from_slice(format!("{:x};chunk-signature={sig}\r\n", data.len()).as_bytes());
                prev = sig;
            } else {
                wire.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
            }
            wire.extend_from_slice(&data);
            wire.extend_from_slice(b"\r\n");
            payload.extend_from_slice(&data);
        }

        if signed {
            let sig = chunk_signature(&prev, b"");
            wire.extend_from_slice(format!("0;chunk-signature={sig}\r\n").as_bytes());
            prev = sig;
        } else {
            wire.extend_from_slice(b"0\r\n");
        }

        if trailer {
            let entries: &[(&str, &str)] = &[("x-amz-checksum-crc32", "AAAAAA=="), ("x-amz-meta-a", "1")];
            let mut canonical = Vec::new();
            let mut sorted: Vec<(&str, &str)> = entries.to_vec();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (name, value) in &sorted {
                canonical.extend_from_slice(name.as_bytes());
                canonical.push(b':');
                canonical.extend_from_slice(value.as_bytes());
                canonical.push(b'\n');
            }
            for (name, value) in &sorted {
                wire.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
            }
            if signed {
                let string_to_sign = s3s_sigv4::create_trailer_string_to_sign(&amz_date(), REGION, SERVICE, &prev, &canonical);
                let sig = signature(&string_to_sign);
                wire.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
            }
            wire.extend_from_slice(b"\r\n");
        }

        let digest: [u8; 32] = Sha256::digest(&payload).into();
        let feed = wire.chunks(feed_size.max(1)).map(Bytes::copy_from_slice).collect();

        Self {
            feed,
            decoded_len: payload.len(),
            digest,
            signed,
        }
    }
}

/// An always-ready input stream over the corpus feed.
fn feed(corpus: &Corpus) -> impl futures_core::Stream<Item = Result<Bytes, StdError>> + Unpin + '_ {
    futures::stream::iter(corpus.feed.iter().cloned()).map(Ok::<Bytes, StdError>)
}

/// Drives a decoder to the end and returns the decoded byte count.
fn drive<S>(stream: S) -> usize
where
    S: futures_core::Stream<Item = Result<Bytes, Error>> + Unpin,
{
    let mut total = 0;
    futures::executor::block_on(async {
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => total += bytes.len(),
                Err(error) => panic!("decoder failed: {error:?}"),
            }
        }
    });
    total
}

fn unsigned(corpus: &Corpus) -> usize {
    let stream = feed(corpus);
    drive(ChunkedStream::unsigned(stream, corpus.decoded_len, Limits::default()))
}

fn signed(corpus: &Corpus) -> usize {
    let stream = feed(corpus);
    drive(ChunkedStream::signed(
        stream,
        sign_context(),
        seed_signature(),
        corpus.decoded_len,
        Limits::default(),
    ))
}

/// Verifies that an arm really decoded the corpus: exact length and SHA-256.
fn validate(corpus: &Corpus) -> Sha256Sum {
    let stream = feed(corpus);
    let decoder: Box<dyn futures_core::Stream<Item = Result<Bytes, Error>> + Unpin> = if corpus.signed {
        Box::new(ChunkedStream::signed(
            stream,
            sign_context(),
            seed_signature(),
            corpus.decoded_len,
            Limits::default(),
        ))
    } else {
        Box::new(ChunkedStream::unsigned(stream, corpus.decoded_len, Limits::default()))
    };
    let mut hasher = Sha256::new();
    let mut total = 0;
    futures::executor::block_on(async {
        let mut decoder = decoder;
        while let Some(item) = decoder.next().await {
            let bytes = item.expect("decoder failed");
            total += bytes.len();
            hasher.update(&bytes);
        }
    });
    assert_eq!(total, corpus.decoded_len, "decoded length mismatch");
    let digest: [u8; 32] = hasher.finalize().into();
    assert_eq!(digest, corpus.digest, "decoded payload mismatch");
    Sha256Sum::from_bytes(digest)
}

fn cell(name: &str, chunk_size: usize, feed_size: usize, signed: bool, trailer: bool) -> (String, Corpus) {
    let id = format!("{name}-chunk{}k-feed{}k", chunk_size / 1024, feed_size / 1024);
    (id, Corpus::build(chunk_size, feed_size, signed, trailer))
}

fn bench_unsigned(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunked_decode_unsigned");
    for &chunk_size in &[1024, 64 * 1024] {
        for &feed_size in &[1024, 8 * 1024, 64 * 1024, 1024 * 1024] {
            let (id, corpus) = cell("unsigned", chunk_size, feed_size, false, false);
            validate(&corpus);
            group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &corpus, |b, corpus| {
                b.iter(|| unsigned(corpus));
            });
        }
    }
    group.finish();
}

fn bench_signed(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunked_decode_signed");
    group.sample_size(10);
    for &chunk_size in &[8 * 1024, 64 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
        for &feed_size in &[8 * 1024, 64 * 1024, 1024 * 1024] {
            let (id, corpus) = cell("signed", chunk_size, feed_size, true, false);
            validate(&corpus);
            group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &corpus, |b, corpus| {
                b.iter(|| signed(corpus));
            });
        }
    }
    group.finish();
}

/// High-fragmentation diagnostic cell and the trailer path.
fn bench_diagnostic(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunked_decode_diagnostic");
    let (id, corpus) = cell("unsigned", 1024, 256, false, false);
    validate(&corpus);
    group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
    group.bench_with_input(BenchmarkId::from_parameter(&id), &corpus, |b, corpus| {
        b.iter(|| unsigned(corpus));
    });

    let (id, corpus) = cell("unsigned-trailer", 64 * 1024, 64 * 1024, false, true);
    validate(&corpus);
    group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
    group.bench_with_input(BenchmarkId::from_parameter(&id), &corpus, |b, corpus| {
        b.iter(|| {
            let stream = feed(corpus);
            let decoder = ChunkedStream::unsigned(stream, corpus.decoded_len, Limits::default());
            let handle = decoder.trailer_handle();
            let total = drive(decoder);
            assert!(handle.take().is_some(), "trailer handle is empty");
            total
        });
    });

    let (id, corpus) = cell("signed-trailer", 64 * 1024, 64 * 1024, true, true);
    validate(&corpus);
    group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
    group.bench_with_input(BenchmarkId::from_parameter(&id), &corpus, |b, corpus| {
        b.iter(|| {
            let stream = feed(corpus);
            let decoder = ChunkedStream::signed(stream, sign_context(), seed_signature(), corpus.decoded_len, Limits::default());
            let handle = decoder.trailer_handle();
            let total = drive(decoder);
            assert!(handle.read(http::HeaderMap::len).is_some(), "trailer handle is empty");
            total
        });
    });
    group.finish();
}

/// Keeps the default limits in the picture: the decoders must not allocate a
/// buffer proportional to the body.
fn bench_limits(c: &mut Criterion) {
    let (_, corpus) = cell("unsigned", 64 * 1024, 64 * 1024, false, false);
    let mut group = c.benchmark_group("chunked_decode_limits");
    group.throughput(Throughput::Bytes(corpus.decoded_len as u64));
    group.bench_with_input(BenchmarkId::from_parameter("default"), &corpus, |b, corpus| {
        b.iter(|| {
            let stream = feed(corpus);
            drive(s3s_chunked::ChunkedStream::unsigned(stream, corpus.decoded_len, Limits::default()))
        });
    });
    group.finish();
}

criterion_group!(benches, bench_unsigned, bench_signed, bench_diagnostic, bench_limits);
criterion_main!(benches);

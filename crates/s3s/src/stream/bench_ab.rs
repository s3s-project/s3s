// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! A/B benchmark harness: legacy `AwsChunkedStream` vs `s3s-chunked`
//! Temporary: it goes away together with the legacy implementation.
//!
//! This harness lives inside the crate because the legacy constructor needs
//! `crate::utils::crypto::Sha256Sum` and `s3s_sigv4::AmzDate`, which are not
//! nameable from an external bench target (they are re-exported only under
//! `cfg(fuzzing)`). Run it with:
//!
//! ```bash
//! cargo test --release -p s3s --lib stream::bench_ab -- --ignored --nocapture
//! ```
//!
//! Every arm consumes the same owned feed (same wire bytes, same feed split,
//! same declared length) in the same process, alternating A/B/A/B per round;
//! the first round is discarded. The corpus is built outside the timed region
//! and each arm is validated (decoded length + SHA-256) before timing.

#![allow(clippy::panic, clippy::unwrap_used)]
#![expect(clippy::cast_precision_loss, reason = "benchmark statistics are computed in f64")]

use crate::auth::SecretKey;
use crate::error::StdError;
use crate::stream::DynByteStream;
use crate::stream::aws_chunked_stream::AwsChunkedStream;
use crate::utils::crypto::Sha256Sum;
use bytes::Bytes;
use futures::StreamExt as _;
use s3s_chunked::{ChunkedStream, Error, Limits, SignContext};
use s3s_sigv4::AmzDate;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::time::{Duration, Instant};

/// The AWS `SigV4` streaming test vector.
const TIMESTAMP: &str = "20130524T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";

/// Pre-registered number of measured rounds (the first round is a warmup).
const ROUNDS: usize = 11;
/// Target wall time per timed sample.
const SAMPLE_TARGET: Duration = Duration::from_millis(250);
const MAX_ITERS_PER_SAMPLE: usize = 4096;

fn amz_date() -> AmzDate {
    AmzDate::parse(TIMESTAMP).expect("valid timestamp")
}

/// The seed signature for the legacy decoder (s3s' own digest type).
fn seed_signature() -> Sha256Sum {
    Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

/// The seed signature for the new crate (its own digest type).
fn chunked_seed_signature() -> s3s_chunked::Sha256Sum {
    s3s_chunked::Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

fn sign_context() -> SignContext {
    SignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes())
}

fn signature(string_to_sign: &str) -> String {
    s3s_sigv4::calculate_signature(string_to_sign, SECRET_KEY, &amz_date(), REGION, SERVICE)
}

fn pattern(len: usize, salt: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from((i + salt) % 251).unwrap()).collect()
}

/// One pre-built body shared by every arm of a scenario.
struct Corpus {
    /// The wire split into `feed_size` fragments.
    feed: Vec<Bytes>,
    /// Declared `x-amz-decoded-content-length`.
    decoded_len: usize,
    /// SHA-256 of the decoded payload.
    digest: [u8; 32],
    /// The decoded payload split per chunk (the hash-only arm's input).
    payload_chunks: Vec<Bytes>,
    signed: bool,
}

impl Corpus {
    fn build(chunk_size: usize, feed_size: usize, chunk_count: usize, signed: bool, trailer: bool) -> Self {
        let mut wire = Vec::with_capacity(chunk_count * (chunk_size + 128));
        let mut payload_chunks = Vec::with_capacity(chunk_count);
        let mut prev = SEED_SIGNATURE.to_owned();

        for index in 0..chunk_count {
            let data = pattern(chunk_size, index * 7);
            if signed {
                let string_to_sign = s3s_sigv4::create_chunk_string_to_sign(&amz_date(), REGION, SERVICE, &prev, &[&data]);
                let sig = signature(&string_to_sign);
                wire.extend_from_slice(format!("{:x};chunk-signature={sig}\r\n", data.len()).as_bytes());
                prev = sig;
            } else {
                wire.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
            }
            wire.extend_from_slice(&data);
            wire.extend_from_slice(b"\r\n");
            payload_chunks.push(Bytes::from(data));
        }

        if signed {
            let string_to_sign = s3s_sigv4::create_chunk_string_to_sign(&amz_date(), REGION, SERVICE, &prev, &[b""]);
            let sig = signature(&string_to_sign);
            wire.extend_from_slice(format!("0;chunk-signature={sig}\r\n").as_bytes());
            prev = sig;
        } else {
            wire.extend_from_slice(b"0\r\n");
        }

        if trailer {
            let entries: &[(&str, &str)] = &[("x-amz-checksum-crc32", "AAAAAA=="), ("x-amz-meta-a", "1")];
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
                wire.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
            }
            if signed {
                let string_to_sign = s3s_sigv4::create_trailer_string_to_sign(&amz_date(), REGION, SERVICE, &prev, &canonical);
                let sig = signature(&string_to_sign);
                wire.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
            }
            wire.extend_from_slice(b"\r\n");
        }

        let mut hasher = Sha256::new();
        for chunk in &payload_chunks {
            hasher.update(chunk);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        let feed = wire.chunks(feed_size.max(1)).map(Bytes::copy_from_slice).collect();
        let decoded_len: usize = payload_chunks.iter().map(Bytes::len).sum();

        Self {
            feed,
            decoded_len,
            digest,
            payload_chunks,
            signed,
        }
    }

    fn body(&self) -> impl futures::Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static {
        futures::stream::iter(self.feed.clone().into_iter().map(Ok::<Bytes, StdError>))
    }
}

/// One measured arm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// The legacy generator-based decoder driven directly.
    Legacy,
    /// The new crate driven directly.
    New,
    /// The new crate behind a bench-local stand-in for the future adapter
    /// (error mapping plus `DynByteStream` boxing).
    NewAdapted,
    /// SHA-256 over the decoded payload only: the lower bound for any decoder.
    HashOnly,
}

impl Arm {
    fn run(self, corpus: &Corpus) -> usize {
        match self {
            Arm::Legacy => {
                let stream = AwsChunkedStream::new(
                    corpus.body(),
                    seed_signature(),
                    amz_date(),
                    REGION.into(),
                    SERVICE.into(),
                    SecretKey::from(SECRET_KEY),
                    corpus.decoded_len,
                    !corpus.signed,
                    crate::config::DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
                );
                block_on(async {
                    let mut stream = stream;
                    let mut total = 0;
                    while let Some(item) = stream.next().await {
                        let bytes: Bytes = item.expect("legacy decoder failed");
                        total += bytes.len();
                    }
                    total
                })
            }
            Arm::New => {
                let decoder = if corpus.signed {
                    ChunkedStream::signed(
                        corpus.body(),
                        sign_context(),
                        chunked_seed_signature(),
                        corpus.decoded_len,
                        Limits::default(),
                    )
                } else {
                    ChunkedStream::unsigned(corpus.body(), corpus.decoded_len, Limits::default())
                };
                drive(decoder)
            }
            Arm::NewAdapted => {
                let decoder = if corpus.signed {
                    ChunkedStream::signed(
                        corpus.body(),
                        sign_context(),
                        chunked_seed_signature(),
                        corpus.decoded_len,
                        Limits::default(),
                    )
                } else {
                    ChunkedStream::unsigned(corpus.body(), corpus.decoded_len, Limits::default())
                };
                drive_dyn(adapt(decoder))
            }
            Arm::HashOnly => {
                let mut hasher = Sha256::new();
                for chunk in &corpus.payload_chunks {
                    hasher.update(chunk);
                }
                let digest: [u8; 32] = hasher.finalize().into();
                assert_eq!(digest, corpus.digest);
                corpus.decoded_len
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Arm::Legacy => "legacy",
            Arm::New => "new",
            Arm::NewAdapted => "new-adapted",
            Arm::HashOnly => "hash-only",
        }
    }
}

/// A bench-local stand-in for the future adapter: error mapping plus boxing.
fn adapt<S>(stream: S) -> DynByteStream
where
    S: futures::Stream<Item = Result<Bytes, Error>> + Unpin + Send + Sync + 'static,
{
    struct Adapter<S> {
        inner: S,
    }

    impl<S> futures::Stream for Adapter<S>
    where
        S: futures::Stream<Item = Result<Bytes, Error>> + Unpin,
    {
        type Item = Result<Bytes, StdError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<<Self as futures::Stream>::Item>> {
            let this = self.get_mut();
            match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => std::task::Poll::Ready(Some(Ok(bytes))),
                std::task::Poll::Ready(Some(Err(error))) => std::task::Poll::Ready(Some(Err(Box::new(error) as StdError))),
                std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    }

    impl<S> crate::stream::ByteStream for Adapter<S> where S: futures::Stream<Item = Result<Bytes, Error>> + Unpin {}

    impl<S> Unpin for Adapter<S> where S: Unpin {}

    Box::pin(Adapter { inner: stream })
}

fn block_on<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Drives a decoder of either shape to the end and returns the decoded bytes.
fn drive<S>(stream: S) -> usize
where
    S: futures::Stream<Item = Result<Bytes, Error>> + Unpin + Send + Sync + 'static,
{
    block_on(async {
        let mut stream = stream;
        let mut total = 0;
        while let Some(item) = stream.next().await {
            total += item.expect("decoder failed").len();
        }
        total
    })
}

/// Drives a boxed adapter stream (the adapter's shape) to the end.
fn drive_dyn(stream: DynByteStream) -> usize {
    block_on(async {
        let mut stream = stream;
        let mut total = 0;
        while let Some(item) = stream.next().await {
            total += item.expect("adapted decoder failed").len();
        }
        total
    })
}

fn validate(corpus: &Corpus) {
    let mut hasher = Sha256::new();
    let total = block_on(async {
        let mut decoder: Box<dyn futures::Stream<Item = Result<Bytes, Error>> + Unpin> = if corpus.signed {
            Box::new(ChunkedStream::signed(
                corpus.body(),
                sign_context(),
                chunked_seed_signature(),
                corpus.decoded_len,
                Limits::default(),
            ))
        } else {
            Box::new(ChunkedStream::unsigned(corpus.body(), corpus.decoded_len, Limits::default()))
        };
        let mut total = 0;
        while let Some(item) = decoder.next().await {
            let bytes = item.expect("decoder failed");
            total += bytes.len();
            hasher.update(&bytes);
        }
        total
    });
    assert_eq!(total, corpus.decoded_len, "decoded length mismatch");
    let digest: [u8; 32] = hasher.finalize().into();
    assert_eq!(digest, corpus.digest, "decoded payload mismatch");
    assert_eq!(Arm::Legacy.run(corpus), corpus.decoded_len, "legacy decoded length mismatch");
    assert_eq!(Arm::HashOnly.run(corpus), corpus.decoded_len);
}

/// Times `iters` iterations of `arm` and returns the average iteration time.
fn sample(arm: Arm, corpus: &Corpus, iters: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..iters {
        let total = arm.run(corpus);
        assert_eq!(total, corpus.decoded_len, "{} decoded length mismatch", arm.name());
    }
    start.elapsed() / u32::try_from(iters).unwrap()
}

fn calibrate(arm: Arm, corpus: &Corpus) -> usize {
    let once = sample(arm, corpus, 1).as_nanos().max(1);
    let target = SAMPLE_TARGET.as_nanos();
    usize::try_from((target / once).max(1)).unwrap_or(1).min(MAX_ITERS_PER_SAMPLE)
}

/// Per-arm per-round average iteration times.
struct Results {
    arms: Vec<Arm>,
    /// `times[arm][round]` in nanoseconds per iteration.
    times: Vec<Vec<f64>>,
    iters: Vec<usize>,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    }
}

fn dispersion(values: &[f64]) -> f64 {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut sorted = values.to_vec();
    let med = median(&mut sorted);
    if med == 0.0 { 0.0 } else { (max - min) / med * 100.0 }
}

fn run_scenario(_label: &str, corpus: &Corpus, arms: &[Arm]) -> Results {
    validate(corpus);
    let iters: Vec<usize> = arms.iter().map(|&arm| calibrate(arm, corpus)).collect();
    let mut times: Vec<Vec<f64>> = vec![Vec::with_capacity(ROUNDS); arms.len()];

    // Warmup round, discarded.
    for (index, &arm) in arms.iter().enumerate() {
        sample(arm, corpus, iters[index]);
    }

    for _ in 0..ROUNDS {
        for (index, &arm) in arms.iter().enumerate() {
            times[index].push(sample(arm, corpus, iters[index]).as_nanos() as f64);
        }
    }

    Results {
        arms: arms.to_vec(),
        times,
        iters,
    }
}

fn report(label: &str, corpus: &Corpus, results: &Results) {
    let gibps = |ns: f64| (corpus.decoded_len as f64 / (1024.0 * 1024.0 * 1024.0)) / (ns / 1e9); // GiB/s (binary, matches the sizes)
    println!("== {label} (decoded {} bytes)", corpus.decoded_len);
    for (index, &arm) in results.arms.iter().enumerate() {
        let values = &results.times[index];
        let mut sorted = values.clone();
        let med = median(&mut sorted);
        println!(
            "   {:<12} median={:>9.4} ms  {:.2} GiB/s  disp={:.2}%  iters={}",
            arm.name(),
            med / 1e6,
            gibps(med),
            dispersion(values),
            results.iters[index]
        );
    }

    let index_of = |arm: Arm| results.arms.iter().position(|&a| a == arm);
    if let (Some(new), Some(legacy)) = (index_of(Arm::New), index_of(Arm::Legacy)) {
        paired(label, "new vs legacy", &results.times[legacy], &results.times[new]);
    }
    if let (Some(new), Some(hash)) = (index_of(Arm::New), index_of(Arm::HashOnly)) {
        paired(label, "new vs hash-only", &results.times[hash], &results.times[new]);
        let mut signed_times = results.times[new].clone();
        let mut hash_times = results.times[hash].clone();
        let signed = median(&mut signed_times);
        let hash_only = median(&mut hash_times);
        println!(
            "   -> signed structural overhead = {:.3}% (target: < 1%)",
            (signed - hash_only) / hash_only * 100.0
        );
    }
    if let (Some(adapted), Some(new)) = (index_of(Arm::NewAdapted), index_of(Arm::New)) {
        paired(label, "adapted vs direct", &results.times[new], &results.times[adapted]);
    }
}

/// Paired per-round comparison of two arms (A/B/A/B interleaved).
fn paired(label: &str, what: &str, baseline: &[f64], candidate: &[f64]) {
    let diffs: Vec<f64> = baseline.iter().zip(candidate).map(|(b, c)| b - c).collect();
    let n = diffs.len() as f64;
    let mean = diffs.iter().sum::<f64>() / n;
    let variance = if diffs.len() > 1 {
        diffs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    let stderr = (variance / n).sqrt();
    let t = if stderr == 0.0 { f64::INFINITY } else { mean / stderr };
    let mut baseline_sorted = baseline.to_vec();
    let base_median = median(&mut baseline_sorted);
    let relative = mean / base_median * 100.0;
    println!("   -> {label}: {what} = {relative:+.2}%  t={t:.1}  (threshold: >=10% and >5 sigma)");
}

fn scenario_unsigned(chunk_size: usize, feed_size: usize, chunk_count: usize) -> Corpus {
    Corpus::build(chunk_size, feed_size, chunk_count, false, false)
}

fn scenario_signed(chunk_size: usize, feed_size: usize, chunk_count: usize) -> Corpus {
    Corpus::build(chunk_size, feed_size, chunk_count, true, false)
}

fn run() {
    println!("A/B harness: rounds={ROUNDS} (first discarded), sample target={SAMPLE_TARGET:?}");
    println!("cpu={}", std::env::var("BENCH_CPU").unwrap_or_else(|_| "unknown".to_owned()));

    // Threshold: unsigned 1 KiB chunk / 1 KiB feed must be >= 2x the legacy decoder.
    let corpus = scenario_unsigned(1024, 1024, 4096);
    let results = run_scenario("unsigned-1k-chunk-1k-feed", &corpus, &[Arm::Legacy, Arm::New]);
    report("unsigned-1k-chunk-1k-feed", &corpus, &results);
    let legacy = median(&mut results.times[0].clone());
    let new = median(&mut results.times[1].clone());
    println!(
        "   -> threshold (unsigned 1 KiB/1 KiB >= 2x legacy): ratio={:.2}x => {}",
        legacy / new,
        if legacy / new >= 2.0 { "PASS" } else { "FAIL" }
    );

    let corpus = scenario_unsigned(64 * 1024, 64 * 1024, 256);
    let results = run_scenario("unsigned-64k-chunk-64k-feed", &corpus, &[Arm::Legacy, Arm::New]);
    report("unsigned-64k-chunk-64k-feed", &corpus, &results);

    // The feed size a real HTTP server sees (hyper reads ~8 KiB at a time).
    let corpus = scenario_unsigned(64 * 1024, 8 * 1024, 256);
    let results = run_scenario("unsigned-64k-chunk-8k-feed", &corpus, &[Arm::Legacy, Arm::New]);
    report("unsigned-64k-chunk-8k-feed", &corpus, &results);

    let corpus = scenario_unsigned(1024, 1024 * 1024, 4096);
    let results = run_scenario("unsigned-1k-chunk-1m-feed", &corpus, &[Arm::Legacy, Arm::New]);
    report("unsigned-1k-chunk-1m-feed", &corpus, &results);

    // Threshold: signed structural overhead = (T_new - T_hash) / T_hash < 1%.
    let corpus = scenario_signed(64 * 1024, 64 * 1024, 256);
    let results = run_scenario("signed-64k-chunk-64k-feed", &corpus, &[Arm::Legacy, Arm::New, Arm::HashOnly]);
    report("signed-64k-chunk-64k-feed", &corpus, &results);
    let hash = median(&mut results.times[2].clone());
    let new = median(&mut results.times[1].clone());
    let overhead = (new - hash) / hash * 100.0;
    println!(
        "   -> threshold (signed structural overhead < 1%): {overhead:.3}% => {}",
        if overhead < 1.0 { "PASS" } else { "FAIL" }
    );

    let corpus = scenario_signed(8 * 1024 * 1024, 1024 * 1024, 2);
    let results = run_scenario("signed-8m-chunk-1m-feed", &corpus, &[Arm::Legacy, Arm::New]);
    report("signed-8m-chunk-1m-feed", &corpus, &results);

    // Adapter overhead (direct vs bench-local adapter shim).
    let corpus = scenario_unsigned(64 * 1024, 64 * 1024, 256);
    let results = run_scenario("l2-unsigned", &corpus, &[Arm::New, Arm::NewAdapted]);
    report("l2-unsigned", &corpus, &results);

    let corpus = scenario_signed(64 * 1024, 64 * 1024, 256);
    let results = run_scenario("l2-signed", &corpus, &[Arm::New, Arm::NewAdapted]);
    report("l2-signed", &corpus, &results);

    // Trailer path (TrailerHandle plumbing included in the new arm only).
    let corpus = Corpus::build(64 * 1024, 64 * 1024, 256, false, true);
    let results = run_scenario("unsigned-trailer", &corpus, &[Arm::Legacy, Arm::New]);
    report("unsigned-trailer", &corpus, &results);
}

#[test]
#[ignore = "benchmark harness; run with cargo test --release -p s3s --lib stream::bench_ab -- --ignored --nocapture"]
fn ab_bench() {
    run();
}

/// Allocation census entry point (target: O(1) allocations per request).
///
/// Run the same binary twice under `valgrind --tool=dhat` — once with
/// `CENSUS_MODE=build` (corpus only) and once with `CENSUS_MODE=decode`
/// (corpus plus `CENSUS_ITERS` decodes) — then divide the block delta by the
/// iteration count.
#[test]
#[ignore = "allocation census; run under valgrind --tool=dhat"]
fn alloc_census() {
    let signed = std::env::var("CENSUS_SIGNED").is_ok();
    let iters: usize = std::env::var("CENSUS_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let chunks: usize = std::env::var("CENSUS_CHUNKS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    let corpus = Corpus::build(64 * 1024, 64 * 1024, chunks, signed, false);
    if std::env::var("CENSUS_MODE").as_deref() == Ok("decode") {
        for _ in 0..iters {
            assert_eq!(Arm::New.run(&corpus), corpus.decoded_len);
        }
    }
    println!("census done: mode={:?} signed={signed} iters={iters}", std::env::var("CENSUS_MODE"));
}

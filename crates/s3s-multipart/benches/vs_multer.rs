// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Throughput comparison against `multer` 3.1.0, the multipart crate the wider
//! ecosystem uses (axum's default choice).
//!
//! Fairness protocol: both parsers consume the exact same body bytes, split
//! into the exact same `Bytes` sequence, built outside the timed region; both
//! run on the same single-threaded executor, registered alternately, under the
//! same allocator; both reach the same endpoint, which is enumerating every
//! part's headers and draining its data. The `reference` group additionally
//! prices aggregation on both sides, because aggregating form fields into
//! values is a consumer choice and not parser work.
//!
//! The two crates are not feature equivalent, so the comparison is only
//! meaningful on the overlap: header parsing and streamed part data. `multer`
//! has no trailer validation and no exact content length derivation, and
//! `s3s-multipart` will not aggregate fields for the caller.
//!
//! Run with:
//! ```bash
//! cargo bench -p s3s-multipart --bench vs_multer
//! ```

mod common;

use std::sync::Arc;
use std::time::Duration;

use criterion::measurement::Measurement;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use common::{
    CHUNK_SIZES, Case, MulterMode, consume_aggregate, consume_all, consume_multer, load_f, load_l, load_m, multipart,
    ready_stream, verify,
};

fn register<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, case: &Case) {
    let body_len = case.load.len() as u64;
    for &chunk_size in CHUNK_SIZES {
        let s3s = case.cache(chunk_size);
        let multer = case.cache(chunk_size);
        group.throughput(Throughput::Bytes(body_len));
        group.bench_with_input(BenchmarkId::new("s3s_multipart", case.id(chunk_size)), &(), |b, ()| {
            b.iter(|| consume_all(multipart(ready_stream(Arc::clone(&s3s)))));
        });
        group.bench_with_input(BenchmarkId::new("multer", case.id(chunk_size)), &(), |b, ()| {
            b.iter(|| consume_multer(Arc::clone(&multer), body_len, MulterMode::RawDrain));
        });
    }
}

fn bench_vs_multer(c: &mut Criterion) {
    let cases = [
        Case::new("F", load_f()),
        Case::new("L-64KiB", load_l(64 * 1024)),
        Case::new("L-1MiB", load_l(1024 * 1024)),
        Case::new("M", load_m()),
    ];
    for case in &cases {
        verify(&case.load);
    }

    let mut group = c.benchmark_group("vs_multer");
    group.warm_up_time(Duration::from_millis(1_000));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(40);
    for case in &cases {
        register(&mut group, case);
    }
    group.finish();
}

/// Prices aggregation on both sides at one chunk size, so the main matrix can
/// stay drain-only without hiding the cost of the consumer pattern that the
/// legacy s3s parser forces.
fn bench_reference(c: &mut Criterion) {
    const CHUNK: usize = 16 * 1024;

    let cases = [Case::new("L-1MiB", load_l(1024 * 1024)), Case::new("M", load_m())];
    for case in &cases {
        verify(&case.load);
    }

    let mut group = c.benchmark_group("vs_multer_reference");
    for case in &cases {
        let body_len = case.load.len() as u64;
        let s3s_drain = case.cache(CHUNK);
        let s3s_aggregate = case.cache(CHUNK);
        let multer_drain = case.cache(CHUNK);
        let multer_aggregate = case.cache(CHUNK);
        group.throughput(Throughput::Bytes(body_len));
        group.bench_with_input(BenchmarkId::new("s3s_multipart/drain", case.id(CHUNK)), &(), |b, ()| {
            b.iter(|| consume_all(multipart(ready_stream(Arc::clone(&s3s_drain)))));
        });
        group.bench_with_input(BenchmarkId::new("s3s_multipart/aggregate", case.id(CHUNK)), &(), |b, ()| {
            b.iter(|| consume_aggregate(multipart(ready_stream(Arc::clone(&s3s_aggregate)))));
        });
        group.bench_with_input(BenchmarkId::new("multer/drain", case.id(CHUNK)), &(), |b, ()| {
            b.iter(|| consume_multer(Arc::clone(&multer_drain), body_len, MulterMode::RawDrain));
        });
        group.bench_with_input(BenchmarkId::new("multer/aggregate", case.id(CHUNK)), &(), |b, ()| {
            b.iter(|| consume_multer(Arc::clone(&multer_aggregate), body_len, MulterMode::Aggregate));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_vs_multer, bench_reference);
criterion_main!(benches);

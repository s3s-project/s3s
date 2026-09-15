// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Throughput of the handoff path: the file part is turned into a self
//! contained stream with `take_data_stream` and drained on its own.
//!
//! This is the path the S3 integration uses — the storage layer receives the
//! body as a stream instead of the parser copying it out. `final` adds
//! `into_final`, which validates the strict closing trailer, so the difference
//! between the two series is the cost of that validation.
//!
//! Run with:
//! ```bash
//! cargo bench -p s3s-multipart --bench take_data_stream
//! ```

mod common;

use std::sync::Arc;
use std::time::Duration;

use criterion::measurement::Measurement;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use common::{Case, consume_take, consume_take_final, load_l, load_m, multipart, ready_stream, verify};

/// The handoff is measured at the chunk size a network body arrives in.
const CHUNK: usize = 16 * 1024;

fn register<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, case: &Case) {
    let take = case.cache(CHUNK);
    let take_final = case.cache(CHUNK);
    group.throughput(Throughput::Bytes(case.load.len() as u64));
    group.bench_with_input(BenchmarkId::new("take", case.id(CHUNK)), &(), |b, ()| {
        b.iter(|| consume_take(multipart(ready_stream(Arc::clone(&take)))));
    });
    group.bench_with_input(BenchmarkId::new("final", case.id(CHUNK)), &(), |b, ()| {
        b.iter(|| consume_take_final(multipart(ready_stream(Arc::clone(&take_final)))));
    });
}

fn bench_take(c: &mut Criterion) {
    let cases = [
        Case::new("L-64KiB", load_l(64 * 1024)),
        Case::new("L-1MiB", load_l(1024 * 1024)),
        Case::new("M", load_m()),
    ];
    for case in &cases {
        verify(&case.load);
    }

    let mut group = c.benchmark_group("take_data_stream");
    for case in &cases {
        register(&mut group, case);
    }
    group.finish();
}

fn bench_take_heavy(c: &mut Criterion) {
    let case = Case::new("L-16MiB", load_l(16 * 1024 * 1024));
    verify(&case.load);

    let mut group = c.benchmark_group("take_data_stream_heavy");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(1));
    group.sample_size(20);
    register(&mut group, &case);
    group.finish();
}

criterion_group!(benches, bench_take, bench_take_heavy);
criterion_main!(benches);

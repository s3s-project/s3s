// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Throughput of the streaming consume path: `next_part` then `next_header`
//! until the header block ends, then `next_data` until the part ends.
//!
//! Two series per cell. `ready` feeds every chunk from an always-ready stream,
//! which is what a buffered body looks like. `pending` suspends the parser
//! before every chunk, so each step pays one wakeup — that is the shape a
//! network body has, and it is the only shape in which per-poll work is
//! visible.
//!
//! Run with:
//! ```bash
//! cargo bench -p s3s-multipart --bench parse_throughput
//! ```

mod common;

use std::sync::Arc;
use std::time::Duration;

use criterion::measurement::Measurement;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use common::{
    CHUNK_SIZES, Case, consume_all, load_diag, load_f, load_l, load_m, multipart, pending_stream, ready_stream, verify,
};

fn register<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, case: &Case) {
    for &chunk_size in CHUNK_SIZES {
        let ready = case.cache(chunk_size);
        let pending = case.cache(chunk_size);
        group.throughput(Throughput::Bytes(case.load.len() as u64));
        group.bench_with_input(BenchmarkId::new("ready", case.id(chunk_size)), &(), |b, ()| {
            b.iter(|| consume_all(multipart(ready_stream(Arc::clone(&ready)))));
        });
        group.bench_with_input(BenchmarkId::new("pending", case.id(chunk_size)), &(), |b, ()| {
            b.iter(|| consume_all(multipart(pending_stream(Arc::clone(&pending)))));
        });
    }
}

fn bench_main_matrix(c: &mut Criterion) {
    let cases = [
        Case::new("F", load_f()),
        Case::new("L-64KiB", load_l(64 * 1024)),
        Case::new("L-1MiB", load_l(1024 * 1024)),
        Case::new("M", load_m()),
    ];
    for case in &cases {
        verify(&case.load);
    }

    let mut group = c.benchmark_group("parse_throughput");
    for case in &cases {
        register(&mut group, case);
    }
    group.finish();
}

fn bench_heavy(c: &mut Criterion) {
    let case = Case::new("L-16MiB", load_l(16 * 1024 * 1024));
    verify(&case.load);

    let mut group = c.benchmark_group("parse_throughput_heavy");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(1));
    group.sample_size(20);
    register(&mut group, &case);
    group.finish();
}

/// Amplifies the cost of scanning back over data that was already seen: a
/// forward-scanning parser stays flat as the field region grows, a parser that
/// re-parses its accumulated buffer does not.
fn bench_diagnostic(c: &mut Criterion) {
    let cases = [
        Case::new("D-fields-64KiB", load_diag(64, 1024)),
        Case::new("D-fields-4MiB", load_diag(64, 64 * 1024)),
    ];
    for case in &cases {
        verify(&case.load);
    }

    let mut group = c.benchmark_group("parse_throughput_diagnostic");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(1));
    group.sample_size(10);
    for case in &cases {
        for &chunk_size in &[1024, 16 * 1024] {
            let ready = case.cache(chunk_size);
            group.throughput(Throughput::Bytes(case.load.len() as u64));
            group.bench_with_input(BenchmarkId::new("ready", case.id(chunk_size)), &(), |b, ()| {
                b.iter(|| consume_all(multipart(ready_stream(Arc::clone(&ready)))));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_main_matrix, bench_heavy, bench_diagnostic);
criterion_main!(benches);

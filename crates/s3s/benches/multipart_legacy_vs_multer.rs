// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Throughput comparison between the legacy s3s multipart parser
//! (`transform_multipart`) and `multer`, the most widely used Rust multipart
//! crate (axum's official choice).
//!
//! The two implementations have different feature sets: the legacy parser is
//! S3 POST Object shaped (aggregated form fields, a single streamed `file`
//! part, exact content length derivation, strict closing trailer validation),
//! while `multer` is a generic streaming parser without trailer validation.
//! Comparisons therefore hold only for the overlapping surface: header
//! parsing, form field consumption, and large file part streaming.
//!
//! Fairness protocol:
//! - both sides consume the exact same body bytes, split into the exact same
//!   `Bytes` chunk sequence (pre-computed outside the timed region);
//! - `total_len` is always `Some` (canonical POST Object with Content-Length);
//!   the chunked (`None`) degradation path is measured only in `reference`;
//! - consumption reaches the same application endpoint: form fields are
//!   aggregated, the file part is drained as a stream on both sides;
//! - both run on the same single-threaded executor, registered alternately,
//!   under the same allocator;
//! - at startup each parser's consumed byte totals are checked against
//!   construction expectations, and the two parsers' outputs (field
//!   name/value pairs and file content) are checked for byte-for-byte
//!   equality (differential guard); the unknown-total-length path is checked
//!   to produce identical output too.
//!
//! Run with:
//! ```bash
//! RUSTFLAGS='--cfg fuzzing' cargo bench -p s3s --bench multipart_legacy_vs_multer
//! ```
//!
//! The legacy parser is exposed only through the `cfg(fuzzing)` test-support
//! surface (the same mechanism the external fuzz workspace uses to drive
//! otherwise-private internals). Under a normal build the `harness` module
//! and its `main` are cfg'd out, leaving an empty stub target.

#![allow(clippy::cast_possible_truncation)]

#[cfg(fuzzing)]
mod harness {
    // Uniform allocator for both parsers: mimalloc's arena behavior is far less
    // sensitive to prior allocation churn than the system allocator, whose
    // mmap/arena state otherwise contaminates later benchmarks with the memory
    // history of earlier ones.
    #[global_allocator]
    static BENCH_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

    use rand::rngs::StdRng;
    use rand::{Rng, RngExt, SeedableRng};
    use std::hint::black_box;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use criterion::measurement::Measurement;
    use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group};
    use futures::StreamExt;
    use futures::executor::block_on;
    use futures::pin_mut;
    use futures::stream::{self, Stream};
    use multer::{Constraints, SizeLimit};
    use s3s::{MultipartLimits, StdError, transform_multipart};

    const BOUNDARY: &[u8] = b"----s3s-bench-boundary-7f3a";

    const CHUNK_SIZES: &[usize] = &[1024, 4 * 1024, 16 * 1024, 64 * 1024];

    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    /// A prepared request body with its consumption accounting expectations.
    struct Load {
        /// Canonical multipart/form-data body (fields first, `file` part last)
        body: Vec<u8>,
        /// Total expected data bytes (field values + file content)
        expected_data_bytes: usize,
    }

    /// Deterministic ASCII value generation from a fixed-seed `StdRng`.
    fn ascii(rng: &mut StdRng, len: usize) -> String {
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..len)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
            .collect()
    }

    fn build_load(fields: &[(String, String)], file_len: usize, file_rng: &mut StdRng) -> Load {
        let mut body = Vec::new();
        let mut data_bytes = 0usize;

        for (name, value) in fields {
            data_bytes += value.len();
            body.extend_from_slice(b"--");
            body.extend_from_slice(BOUNDARY);
            body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(b"\"\r\n\r\n");
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }

        body.extend_from_slice(b"--");
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"bench.bin\"\r\n");
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");

        // Full-range pseudo-random content: matches the ~1/256 CR density of real
        // binary uploads. (A repeating-value ramp would concentrate runs of b'\r'
        // -- the byte the legacy parser's boundary scan probes on -- and skew the
        // comparison.)
        let file_start = body.len();
        body.resize(file_start + file_len, 0);
        file_rng.fill_bytes(&mut body[file_start..]);
        data_bytes += file_len;

        body.extend_from_slice(b"\r\n--");
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(b"--\r\n");

        Load {
            body,
            expected_data_bytes: data_bytes,
        }
    }

    fn load_f() -> Load {
        let mut rng = StdRng::seed_from_u64(0xfeed_beef);
        let fields: Vec<(String, String)> = (0..16).map(|i| (format!("field_{i:02}"), ascii(&mut rng, 1024))).collect();
        build_load(&fields, 64, &mut rng)
    }

    fn load_l(file_len: usize) -> Load {
        let mut rng = StdRng::seed_from_u64(0x1111_2222);
        build_load(&[("key".to_owned(), "bench-key".to_owned())], file_len, &mut rng)
    }

    fn load_m() -> Load {
        let mut rng = StdRng::seed_from_u64(0x5eed_5eed);
        let fields = vec![
            ("key".to_owned(), "uploads/2026/09/bench.bin".to_owned()),
            ("policy".to_owned(), ascii(&mut rng, 1024)),
            ("x-amz-signature".to_owned(), ascii(&mut rng, 64)),
            ("content-type".to_owned(), "application/octet-stream".to_owned()),
        ];
        build_load(&fields, 1024 * 1024, &mut rng)
    }

    /// Diagnostic load: large fields region + tiny file, exposes the legacy
    /// parser's re-parse of the accumulated buffer while hunting for the file
    /// part (the signal-gated fix is not merged to main yet).
    fn load_diag(field_count: usize, field_len: usize) -> Load {
        let mut rng = StdRng::seed_from_u64(0xd1a9_0001);
        let fields: Vec<(String, String)> = (0..field_count)
            .map(|i| (format!("field_{i:04}"), ascii(&mut rng, field_len)))
            .collect();
        build_load(&fields, 64, &mut rng)
    }

    /// Pre-split the body into shared `Bytes` chunks (outside the timed region).
    fn chunk_cache(body: &[u8], chunk_size: usize) -> Arc<Vec<Bytes>> {
        Arc::new(body.chunks(chunk_size.max(1)).map(Bytes::copy_from_slice).collect())
    }

    /// Owning iterator over the pre-split chunks; sharing `Bytes` clones keeps
    /// per-iteration setup at refcount bumps instead of body copies.
    struct ChunkStream {
        chunks: Arc<Vec<Bytes>>,
        index: usize,
    }

    impl Iterator for ChunkStream {
        type Item = Result<Bytes, StdError>;

        fn next(&mut self) -> Option<Self::Item> {
            let item = self.chunks.get(self.index).cloned();
            self.index += 1;
            item.map(Ok)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.chunks.len().saturating_sub(self.index);
            (remaining, Some(remaining))
        }
    }

    /// Build the input stream over shared chunks; the `Arc` is owned by the
    /// stream, satisfying the `'static` bound of both parsers.
    fn body_stream(chunks: Arc<Vec<Bytes>>) -> impl Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static {
        stream::iter(ChunkStream { chunks, index: 0 })
    }

    fn fold(hash: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(hash, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME))
    }

    fn fold_len(hash: u64, len: usize) -> u64 {
        fold(hash, &(len as u64).to_le_bytes())
    }

    /// Small data-dependent fingerprint: first/last bytes of a consumed value.
    fn fold_ends(hash: u64, data: &[u8]) -> u64 {
        let mut h = hash;
        if let Some(&first) = data.first() {
            h = fold(h, &[first]);
        }
        if let Some(&last) = data.last() {
            h = fold(h, &[last]);
        }
        h
    }

    fn chunk_fingerprint(hash: u64, chunk: &Bytes) -> u64 {
        fold_len(fold(hash, &chunk[..chunk.len().min(8)]), chunk.len())
    }

    /// Drive the legacy parser to the application endpoint: fields aggregated,
    /// file part drained as a stream. Returns consumed data bytes.
    fn drive_legacy(chunks: &Arc<Vec<Bytes>>, total_len: Option<u64>) -> u64 {
        block_on(async {
            let mut multipart =
                transform_multipart(body_stream(Arc::clone(chunks)), BOUNDARY, MultipartLimits::default(), total_len)
                    .await
                    .expect("legacy: parse error");

            let mut hash = FNV_OFFSET;
            let mut total = 0usize;
            for (name, value) in multipart.fields() {
                total += value.len();
                hash = fold_ends(fold(hash, name.as_bytes()), value.as_bytes());
                hash = fold_len(hash, value.len());
            }
            black_box(multipart.fields());

            let file = multipart.take_file_stream().expect("legacy: missing file stream");
            pin_mut!(file);
            while let Some(item) = file.next().await {
                let chunk = item.expect("legacy: file stream error");
                total += chunk.len();
                hash = chunk_fingerprint(hash, &chunk);
                black_box(&chunk);
            }
            black_box(hash);
            black_box(total as u64)
        })
    }

    /// Drive `multer` to the same application endpoint.
    #[derive(Clone, Copy)]
    enum MulterMode {
        /// Aggregate every non-file field (`Field::bytes`, canonical consumer)
        Aggregate,
        /// Drain every field chunk by chunk without aggregation (raw parser cost)
        RawDrain,
    }

    fn drive_multer(chunks: &Arc<Vec<Bytes>>, body_len: u64, mode: MulterMode) -> u64 {
        block_on(async {
            let boundary = std::str::from_utf8(BOUNDARY).expect("boundary is ASCII");
            let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(body_len).per_field(body_len));
            let mut multipart = multer::Multipart::with_constraints(body_stream(Arc::clone(chunks)), boundary, constraints);

            let mut hash = FNV_OFFSET;
            let mut total = 0usize;
            while let Some(mut field) = multipart.next_field().await.expect("multer: stream error") {
                hash = fold(hash, field.name().unwrap_or_default().as_bytes());
                if field.file_name().is_some() || matches!(mode, MulterMode::RawDrain) {
                    while let Some(chunk) = field.chunk().await.expect("multer: field error") {
                        total += chunk.len();
                        hash = chunk_fingerprint(hash, &chunk);
                        black_box(&chunk);
                    }
                } else {
                    let data = field.bytes().await.expect("multer: field error");
                    total += data.len();
                    hash = fold_ends(hash, &data);
                    hash = fold_len(hash, data.len());
                    black_box(&data);
                }
            }
            black_box(hash);
            black_box(total as u64)
        })
    }

    /// One (load, chunk) configuration with pre-computed chunk caches.
    struct Case {
        label: String,
        load: Load,
        caches: Vec<(usize, Arc<Vec<Bytes>>)>,
    }

    fn build_case(label: &str, load: Load) -> Case {
        let caches = CHUNK_SIZES.iter().map(|&c| (c, chunk_cache(&load.body, c))).collect();
        Case {
            label: label.to_owned(),
            load,
            caches,
        }
    }

    /// Parse a body with the legacy parser and return the aggregated fields
    /// and the full file content (startup verification only, never timed).
    fn collect_legacy(chunks: &Arc<Vec<Bytes>>, total_len: Option<u64>) -> (Vec<(String, String)>, Vec<u8>) {
        block_on(async {
            let mut multipart =
                transform_multipart(body_stream(Arc::clone(chunks)), BOUNDARY, MultipartLimits::default(), total_len)
                    .await
                    .expect("legacy: parse error");

            let file = multipart.take_file_stream().expect("legacy: missing file stream");
            pin_mut!(file);
            let mut content = Vec::new();
            while let Some(item) = file.next().await {
                content.extend_from_slice(&item.expect("legacy: file stream error"));
            }
            (multipart.fields().to_vec(), content)
        })
    }

    /// Parse a body with `multer` and return the aggregated fields and the
    /// full file content (startup verification only, never timed).
    fn collect_multer(chunks: &Arc<Vec<Bytes>>, body_len: u64) -> (Vec<(String, String)>, Vec<u8>) {
        block_on(async {
            let boundary = std::str::from_utf8(BOUNDARY).expect("boundary is ASCII");
            let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(body_len).per_field(body_len));
            let mut multipart = multer::Multipart::with_constraints(body_stream(Arc::clone(chunks)), boundary, constraints);

            let mut fields = Vec::new();
            let mut content = Vec::new();
            while let Some(mut field) = multipart.next_field().await.expect("multer: stream error") {
                let name = field.name().unwrap_or_default().to_owned();
                if field.file_name().is_some() {
                    while let Some(chunk) = field.chunk().await.expect("multer: field error") {
                        content.extend_from_slice(&chunk);
                    }
                } else {
                    let data = field.bytes().await.expect("multer: field error");
                    fields.push((name, String::from_utf8(data.to_vec()).expect("field value is UTF-8")));
                }
            }
            (fields, content)
        })
    }

    /// The legacy parser lowercases field names and sorts them by name; both
    /// sides are normalized the same way before comparison.
    fn normalize_fields(mut fields: Vec<(String, String)>) -> Vec<(String, String)> {
        for (name, _) in &mut fields {
            *name = name.to_lowercase();
        }
        fields.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
        fields
    }

    /// Startup differential guard: byte totals against construction
    /// expectations, plus byte-for-byte equality of the two parsers' outputs
    /// (field name/value pairs and file content).
    fn verify(case: &Case) {
        for (chunk, cache) in &case.caches {
            let body_len = case.load.body.len() as u64;
            let (legacy_fields, legacy_file) = collect_legacy(cache, Some(body_len));
            let (multer_fields, multer_file) = collect_multer(cache, body_len);

            let legacy_total: usize = legacy_fields.iter().map(|(_, v)| v.len()).sum::<usize>() + legacy_file.len();
            let multer_total: usize = multer_fields.iter().map(|(_, v)| v.len()).sum::<usize>() + multer_file.len();
            let expected = case.load.expected_data_bytes as usize;
            assert_eq!(legacy_total, expected, "legacy byte accounting mismatch: {} chunk {chunk}", case.label);
            assert_eq!(multer_total, expected, "multer byte accounting mismatch: {} chunk {chunk}", case.label);

            assert_eq!(
                normalize_fields(legacy_fields),
                normalize_fields(multer_fields),
                "field name/value mismatch between parsers: {} chunk {chunk}",
                case.label
            );
            assert_eq!(
                legacy_file, multer_file,
                "file content mismatch between parsers: {} chunk {chunk}",
                case.label
            );
        }
    }

    /// The unknown-total-length path (`total_len = None`) must yield the same
    /// output as the exact-length path; it only skips strict validation.
    fn verify_none_degradation(case: &Case, cache: &Arc<Vec<Bytes>>) {
        let body_len = case.load.body.len() as u64;
        let (some_fields, some_file) = collect_legacy(cache, Some(body_len));
        let (none_fields, none_file) = collect_legacy(cache, None);
        assert_eq!(
            normalize_fields(some_fields),
            normalize_fields(none_fields),
            "fields changed under unknown total length: {}",
            case.label
        );
        assert_eq!(some_file, none_file, "file content changed under unknown total length: {}", case.label);
    }

    #[derive(Clone, Copy)]
    enum Settings {
        Default,
        Heavy,
        Diagnostic,
    }

    impl Settings {
        const fn warm_up(self) -> Duration {
            match self {
                Self::Default => Duration::from_millis(1_000),
                Self::Heavy | Self::Diagnostic => Duration::from_millis(500),
            }
        }

        const fn measurement(self) -> Duration {
            match self {
                Self::Default => Duration::from_secs(2),
                Self::Heavy | Self::Diagnostic => Duration::from_secs(1),
            }
        }

        const fn samples(self) -> usize {
            match self {
                Self::Default => 40,
                Self::Heavy => 20,
                Self::Diagnostic => 10,
            }
        }
    }

    fn apply_settings<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, level: Settings) {
        group.warm_up_time(level.warm_up());
        group.measurement_time(level.measurement());
        group.sample_size(level.samples());
    }

    fn register_pair<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, case: &Case, chunk: usize) {
        let cache = case
            .caches
            .iter()
            .find(|(c, _)| *c == chunk)
            .map(|(_, cache)| Arc::clone(cache))
            .expect("chunk cache missing");
        let name = format!("{}/{chunk}", case.label);
        let body_len = case.load.body.len() as u64;

        group.throughput(Throughput::Bytes(body_len));
        group.bench_with_input(BenchmarkId::new("legacy", &name), &(), |b, &()| {
            b.iter(|| drive_legacy(&cache, Some(body_len)))
        });
        group.bench_with_input(BenchmarkId::new("multer", &name), &(), |b, &()| {
            b.iter(|| drive_multer(&cache, body_len, MulterMode::Aggregate))
        });
    }

    fn bench_main_matrix(c: &mut Criterion) {
        let cases = [
            build_case("F", load_f()),
            build_case("L-64KiB", load_l(64 * 1024)),
            build_case("L-1MiB", load_l(1024 * 1024)),
            build_case("M", load_m()),
        ];
        for case in &cases {
            verify(case);
        }

        let mut group = c.benchmark_group("multipart/legacy_vs_multer");
        apply_settings(&mut group, Settings::Default);
        for case in &cases {
            for &chunk in CHUNK_SIZES {
                register_pair(&mut group, case, chunk);
            }
        }
        group.finish();
    }

    fn bench_heavy_matrix(c: &mut Criterion) {
        let case = build_case("L-16MiB", load_l(16 * 1024 * 1024));
        verify(&case);

        let mut group = c.benchmark_group("multipart/legacy_vs_multer_heavy");
        apply_settings(&mut group, Settings::Heavy);
        for &chunk in CHUNK_SIZES {
            register_pair(&mut group, &case, chunk);
        }
        group.finish();
    }

    /// Reference group: legacy behavior with unknown total length (the parser
    /// streams without strict validation; whole-file aggregation happens in
    /// the ops layer, not here) and multer's raw drain mode.
    fn bench_reference(c: &mut Criterion) {
        let case = build_case("L-1MiB", load_l(1024 * 1024));
        verify(&case);
        let chunk = 16 * 1024;
        let cache = case
            .caches
            .iter()
            .find(|(c, _)| *c == chunk)
            .map(|(_, cache)| Arc::clone(cache))
            .expect("chunk cache missing");
        verify_none_degradation(&case, &cache);
        let body_len = case.load.body.len() as u64;

        let mut group = c.benchmark_group("multipart/reference");
        apply_settings(&mut group, Settings::Default);
        group.throughput(Throughput::Bytes(body_len));

        group.bench_function("legacy/some_total_len", |b| b.iter(|| drive_legacy(&cache, Some(body_len))));
        group.bench_function("legacy/none_total_len", |b| b.iter(|| drive_legacy(&cache, None)));
        group.bench_function("multer/raw_drain", |b| b.iter(|| drive_multer(&cache, body_len, MulterMode::RawDrain)));
        group.bench_function("multer/aggregate", |b| b.iter(|| drive_multer(&cache, body_len, MulterMode::Aggregate)));
        group.finish();
    }

    /// Diagnostic group: large fields region amplifies the legacy parser's
    /// per-chunk re-parse of the accumulated buffer (O(n²/c) until the file part
    /// signal arrives); multer scans forward.
    fn bench_diagnostic(c: &mut Criterion) {
        let cases = [
            build_case("D-fields-64KiB", load_diag(64, 1024)),
            build_case("D-fields-4MiB", load_diag(64, 64 * 1024)),
        ];
        for case in &cases {
            verify(case);
        }

        let mut group = c.benchmark_group("multipart/diagnostic_fields_rescan");
        apply_settings(&mut group, Settings::Diagnostic);
        for case in &cases {
            for &chunk in &[1024, 16 * 1024] {
                register_pair(&mut group, case, chunk);
            }
        }
        group.finish();
    }

    criterion_group!(benches, bench_main_matrix, bench_heavy_matrix, bench_reference, bench_diagnostic);
}

#[cfg(fuzzing)]
criterion::criterion_main!(harness::benches);

#[cfg(not(fuzzing))]
fn main() {}

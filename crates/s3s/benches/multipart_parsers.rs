// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Throughput comparison across the three multipart parsers this repository has
//! a stake in: the legacy s3s parser (`transform_multipart`, still the
//! production parser on `main`), the `s3s-multipart` crate that replaces it,
//! and `multer`, the most widely used Rust multipart crate (axum's official
//! choice).
//!
//! The implementations have different feature sets, so a comparison only holds
//! for the overlapping surface: header parsing, form field consumption and
//! streamed file parts. The legacy parser is S3 POST Object shaped (aggregated
//! form fields, a single streamed `file` part, exact content length derivation,
//! strict closing trailer validation); `multer` is a generic streaming parser
//! without trailer validation; `s3s-multipart` streams parts and never
//! aggregates for the caller.
//!
//! Fairness protocol:
//! - all three consume the exact same body bytes, split into the exact same
//!   `Bytes` chunk sequence (pre-computed outside the timed region);
//! - `total_len` is always `Some` for the legacy parser (canonical POST Object
//!   with Content-Length); the chunked (`None`) degradation path is measured
//!   only in `reference`;
//! - consumption reaches the same application endpoint: form fields are
//!   aggregated into values, the file part is drained as a stream;
//! - all three run on the same single-threaded executor, registered
//!   alternately, under the same allocator;
//! - at startup each parser's consumed byte totals are checked against
//!   construction expectations, and the three parsers' outputs (field
//!   name/value pairs and file content) are checked for byte-for-byte equality
//!   (differential guard); the unknown-total-length path is checked to produce
//!   identical output too.
//!
//! The `pending` group feeds the chunks from a stream that suspends between
//! them: the first chunk arrives on the first poll and every later one costs a
//! wakeup. An always-ready stream lets a parser run from a single poll loop,
//! which hides whatever it does per poll behind the previous chunk's
//! processing.
//!
//! The two shapes are not equivalent for every parser, and the difference is
//! not noise. `multer`'s `StreamBuffer::poll_stream` loops until the stream
//! returns `Pending`, so an always-ready stream makes it pull the whole
//! remaining body into one `BytesMut` on the first poll, while a suspending
//! stream bounds what it takes to one chunk. Its measured throughput therefore
//! depends on how the body is delivered: at 16 KiB chunks it goes from
//! 11.3 GiB/s ready and 13.2 GiB/s suspending on a 64 KiB body to 7.5 and
//! 18.4 GiB/s on a 1 MiB body, while the legacy parser moves 11.2/12.1 and
//! `s3s-multipart` 33.8/34.6 GiB/s across the same two shapes. Read the two
//! groups together: an always-ready stream measures a buffered body, which is
//! `multer`'s worst case rather than a neutral one.
//!
//! Run with:
//! ```bash
//! RUSTFLAGS='--cfg fuzzing' cargo bench -p s3s --bench multipart_parsers
//! ```
//!
//! The legacy parser is exposed only through the `cfg(fuzzing)` test-support
//! surface (the same mechanism the external fuzz workspace uses to drive
//! otherwise-private internals). Under a normal build the `harness` module
//! and its `main` are cfg'd out, leaving an empty stub target.

#![allow(clippy::cast_possible_truncation)]

#[cfg(fuzzing)]
mod harness {
    // Uniform allocator for all three parsers: mimalloc's arena behavior is far
    // less sensitive to prior allocation churn than the system allocator, whose
    // mmap/arena state otherwise contaminates later benchmarks with the memory
    // history of earlier ones.
    #[global_allocator]
    static BENCH_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

    use rand::rngs::StdRng;
    use rand::{Rng, RngExt, SeedableRng};
    use std::hint::black_box;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use criterion::measurement::Measurement;
    use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group};
    use futures::StreamExt;
    use futures::executor::block_on;
    use futures::pin_mut;
    use futures::stream::Stream;
    use multer::{Constraints, SizeLimit};
    use s3s::{MultipartLimits, StdError, transform_multipart};
    use s3s_multipart::{Boundary as MpBoundary, Error as MpError, Multipart as MpMultipart};

    const BOUNDARY: &[u8] = b"----s3s-bench-boundary-7f3a";

    const CHUNK_SIZES: &[usize] = &[1024, 4 * 1024, 16 * 1024, 64 * 1024];

    /// Internal buffer limit for the `s3s-multipart` side: it bounds the header
    /// block and the boundary residue only. Part data is passed through and is
    /// never charged to it.
    const MP_MAX_BUFFER_SIZE: usize = 64 * 1024;

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
        // -- the byte a boundary scan probes on -- and skew the comparison.)
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

    /// How the parser is fed.
    ///
    /// Both shapes hand out the same chunks with the same `size_hint`; they
    /// differ only in whether a poll may return `Pending` first. Keeping them
    /// one type means the two series cannot accidentally differ in anything
    /// else, and it lets the startup guard cover the suspending shape too.
    #[derive(Clone, Copy, Debug)]
    enum Shape {
        /// Chunks are always ready: what a fully buffered body looks like.
        Ready,
        /// Suspends between chunks: the first is ready, every later one costs
        /// a wakeup. A network body.
        Suspending,
    }

    /// Owning stream over the pre-split chunks; sharing `Bytes` clones keeps
    /// per-iteration setup at refcount bumps instead of body copies.
    struct BodyStream {
        chunks: Arc<Vec<Bytes>>,
        index: usize,
        suspend: bool,
        resume: bool,
    }

    impl BodyStream {
        fn new(chunks: Arc<Vec<Bytes>>, shape: Shape) -> Self {
            Self {
                chunks,
                index: 0,
                suspend: matches!(shape, Shape::Suspending),
                resume: true,
            }
        }
    }

    impl Stream for BodyStream {
        type Item = Result<Bytes, StdError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.suspend && !this.resume {
                this.resume = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.resume = false;
            let item = this.chunks.get(this.index).cloned();
            this.index += 1;
            Poll::Ready(item.map(Ok))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.chunks.len().saturating_sub(self.index);
            (remaining, Some(remaining))
        }
    }

    /// The same stream for the `s3s-multipart` side, whose item error is the
    /// crate's own type rather than `StdError`.
    struct MpBodyStream {
        chunks: Arc<Vec<Bytes>>,
        index: usize,
        suspend: bool,
        resume: bool,
    }

    impl MpBodyStream {
        fn new(chunks: Arc<Vec<Bytes>>, shape: Shape) -> Self {
            Self {
                chunks,
                index: 0,
                suspend: matches!(shape, Shape::Suspending),
                resume: true,
            }
        }
    }

    impl Stream for MpBodyStream {
        type Item = Result<Bytes, MpError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.suspend && !this.resume {
                this.resume = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.resume = false;
            let item = this.chunks.get(this.index).cloned();
            this.index += 1;
            Poll::Ready(item.map(Ok))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.chunks.len().saturating_sub(self.index);
            (remaining, Some(remaining))
        }
    }

    /// Build the input stream over shared chunks; the `Arc` is owned by the
    /// stream, satisfying the `'static` bound of every parser.
    fn body_stream(chunks: Arc<Vec<Bytes>>, shape: Shape) -> BodyStream {
        BodyStream::new(chunks, shape)
    }

    /// The `s3s-multipart` input stream over the same chunks.
    fn mp_body_stream(chunks: Arc<Vec<Bytes>>, shape: Shape) -> MpBodyStream {
        MpBodyStream::new(chunks, shape)
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

    fn mp_boundary() -> MpBoundary {
        MpBoundary::new(BOUNDARY).expect("the benchmark boundary is valid")
    }

    /// Drive the legacy parser to the application endpoint: fields aggregated,
    /// file part drained as a stream. Returns consumed data bytes.
    fn drive_legacy<S>(stream: S, total_len: Option<u64>) -> u64
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        block_on(async {
            let mut multipart = transform_multipart(stream, BOUNDARY, MultipartLimits::default(), total_len)
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

    /// Drive `s3s-multipart` to the same endpoint: field parts aggregated into
    /// values, the file part drained as a stream.
    fn drive_s3s<S>(stream: S) -> u64
    where
        S: Stream<Item = Result<Bytes, MpError>> + Send + Unpin,
    {
        block_on(async {
            let mut multipart = MpMultipart::new(stream, &mp_boundary(), MP_MAX_BUFFER_SIZE);

            let mut hash = FNV_OFFSET;
            let mut total = 0usize;
            while let Some(mut part) = multipart.next_part().await.expect("s3s-multipart: parse error") {
                let mut is_file = false;
                while let Some(header) = part.next_header().await.expect("s3s-multipart: header error") {
                    if header.name.eq_ignore_ascii_case("content-disposition") {
                        let parsed = s3s_multipart::parse_content_disposition(header.value);
                        if let Some(cd) = parsed {
                            is_file = cd.file_name.is_some();
                            hash = fold(hash, cd.name.unwrap_or_default());
                        }
                    }
                }
                if is_file {
                    while let Some(chunk) = part.next_data().await.expect("s3s-multipart: data error") {
                        total += chunk.len();
                        hash = chunk_fingerprint(hash, &chunk);
                        black_box(&chunk);
                    }
                } else {
                    let mut value = Vec::new();
                    while let Some(chunk) = part.next_data().await.expect("s3s-multipart: data error") {
                        value.extend_from_slice(&chunk);
                    }
                    total += value.len();
                    hash = fold_ends(hash, &value);
                    hash = fold_len(hash, value.len());
                    black_box(&value);
                }
            }
            black_box(hash);
            black_box(total as u64)
        })
    }

    /// Drive the `s3s-multipart` handoff: the file part is taken with
    /// `take_data_stream` and drained through `into_final`, which validates the
    /// strict closing trailer. This is the path the S3 integration uses.
    fn drive_s3s_take<S>(stream: S) -> u64
    where
        S: Stream<Item = Result<Bytes, MpError>> + Send + Unpin,
    {
        block_on(async {
            let mut multipart = MpMultipart::new(stream, &mp_boundary(), MP_MAX_BUFFER_SIZE);

            let mut hash = FNV_OFFSET;
            let mut total = 0usize;
            while let Some(mut part) = multipart.next_part().await.expect("s3s-multipart: parse error") {
                let mut is_file = false;
                while let Some(header) = part.next_header().await.expect("s3s-multipart: header error") {
                    if header.name.eq_ignore_ascii_case("content-disposition") {
                        let parsed = s3s_multipart::parse_content_disposition(header.value);
                        if let Some(cd) = parsed {
                            is_file = cd.file_name.is_some();
                            hash = fold(hash, cd.name.unwrap_or_default());
                        }
                    }
                }
                if is_file {
                    let taken = part.take_data_stream().expect("s3s-multipart: take_data_stream");
                    let mut taken = Box::pin(taken.into_final());
                    while let Some(chunk) = taken.next().await {
                        let chunk = chunk.expect("s3s-multipart: file stream error");
                        total += chunk.len();
                        hash = chunk_fingerprint(hash, &chunk);
                        black_box(&chunk);
                    }
                    break;
                }
                while let Some(chunk) = part.next_data().await.expect("s3s-multipart: data error") {
                    total += chunk.len();
                    hash = chunk_fingerprint(hash, &chunk);
                    black_box(&chunk);
                }
            }
            black_box(hash);
            black_box(total as u64)
        })
    }

    /// Drive the legacy parser's handoff (`take_file_stream`) to the same
    /// endpoint as [`drive_s3s_take`].
    fn drive_legacy_take<S>(stream: S, total_len: Option<u64>) -> u64
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        block_on(async {
            let mut multipart = transform_multipart(stream, BOUNDARY, MultipartLimits::default(), total_len)
                .await
                .expect("legacy: parse error");

            let mut hash = FNV_OFFSET;
            let mut total = 0usize;
            for (name, value) in multipart.fields() {
                total += value.len();
                hash = fold_ends(fold(hash, name.as_bytes()), value.as_bytes());
                hash = fold_len(hash, value.len());
            }

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

    /// How the `multer` side consumes a field.
    #[derive(Clone, Copy)]
    enum MulterMode {
        /// Aggregate every non-file field (`Field::bytes`, canonical consumer)
        Aggregate,
        /// Drain every field chunk by chunk without aggregation (raw parser cost)
        RawDrain,
    }

    fn drive_multer<S>(stream: S, body_len: u64, mode: MulterMode) -> u64
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + 'static,
    {
        block_on(async {
            let boundary = std::str::from_utf8(BOUNDARY).expect("boundary is ASCII");
            let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(body_len).per_field(body_len));
            let mut multipart = multer::Multipart::with_constraints(stream, boundary, constraints);

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

    impl Case {
        fn cache(&self, chunk: usize) -> Arc<Vec<Bytes>> {
            self.caches
                .iter()
                .find(|(c, _)| *c == chunk)
                .map(|(_, cache)| Arc::clone(cache))
                .expect("chunk cache missing")
        }

        fn name(&self, chunk: usize) -> String {
            format!("{}/{chunk}", self.label)
        }
    }

    /// Parse a body with the legacy parser and return the aggregated fields
    /// and the full file content (startup verification only, never timed).
    fn collect_legacy<S>(stream: S, total_len: Option<u64>) -> (Vec<(String, String)>, Vec<u8>)
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        block_on(async {
            let mut multipart = transform_multipart(stream, BOUNDARY, MultipartLimits::default(), total_len)
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

    /// Parse a body with `s3s-multipart` and return the fields and the file
    /// content in the same shape as [`collect_legacy`].
    fn collect_s3s<S>(stream: S) -> (Vec<(String, String)>, Vec<u8>)
    where
        S: Stream<Item = Result<Bytes, MpError>> + Send + Unpin,
    {
        block_on(async {
            let mut multipart = MpMultipart::new(stream, &mp_boundary(), MP_MAX_BUFFER_SIZE);

            let mut fields = Vec::new();
            let mut content = Vec::new();
            while let Some(mut part) = multipart.next_part().await.expect("s3s-multipart: parse error") {
                let mut name = Vec::new();
                let mut is_file = false;
                while let Some(header) = part.next_header().await.expect("s3s-multipart: header error") {
                    if header.name.eq_ignore_ascii_case("content-disposition") {
                        let parsed = s3s_multipart::parse_content_disposition(header.value).expect("content-disposition parses");
                        name = parsed.name.unwrap_or_default().to_vec();
                        is_file = parsed.file_name.is_some();
                    }
                }
                let mut data = Vec::new();
                while let Some(chunk) = part.next_data().await.expect("s3s-multipart: data error") {
                    data.extend_from_slice(&chunk);
                }
                if is_file {
                    content = data;
                } else {
                    let name = String::from_utf8(name).expect("field name is UTF-8");
                    let value = String::from_utf8(data).expect("field value is UTF-8");
                    fields.push((name, value));
                }
            }
            (fields, content)
        })
    }

    /// Parse a body with `multer` and return the aggregated fields and the
    /// full file content (startup verification only, never timed).
    fn collect_multer<S>(stream: S, body_len: u64) -> (Vec<(String, String)>, Vec<u8>)
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + 'static,
    {
        block_on(async {
            let boundary = std::str::from_utf8(BOUNDARY).expect("boundary is ASCII");
            let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(body_len).per_field(body_len));
            let mut multipart = multer::Multipart::with_constraints(stream, boundary, constraints);

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

    /// The legacy parser lowercases field names and sorts them by name; all
    /// three sides are normalized the same way before comparison.
    fn normalize_fields(mut fields: Vec<(String, String)>) -> Vec<(String, String)> {
        for (name, _) in &mut fields {
            *name = name.to_lowercase();
        }
        fields.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
        fields
    }

    fn data_total(fields: &[(String, String)], file: &[u8]) -> usize {
        fields.iter().map(|(_, value)| value.len()).sum::<usize>() + file.len()
    }

    /// Startup differential guard: byte totals against construction
    /// expectations, plus byte-for-byte equality of the three parsers' outputs
    /// (field name/value pairs and file content).
    fn verify_shape(case: &Case, shape: Shape) {
        for (chunk, cache) in &case.caches {
            let body_len = case.load.body.len() as u64;
            let expected = case.load.expected_data_bytes;

            let (legacy_fields, legacy_file) = collect_legacy(body_stream(Arc::clone(cache), shape), Some(body_len));
            let (multer_fields, multer_file) = collect_multer(body_stream(Arc::clone(cache), shape), body_len);
            let (s3s_fields, s3s_file) = collect_s3s(mp_body_stream(Arc::clone(cache), shape));

            assert_eq!(
                data_total(&legacy_fields, &legacy_file),
                expected,
                "legacy byte accounting mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
            assert_eq!(
                data_total(&multer_fields, &multer_file),
                expected,
                "multer byte accounting mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
            assert_eq!(
                data_total(&s3s_fields, &s3s_file),
                expected,
                "s3s-multipart byte accounting mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );

            let legacy_fields = normalize_fields(legacy_fields);
            assert_eq!(
                legacy_fields,
                normalize_fields(multer_fields),
                "legacy/multer field mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
            assert_eq!(
                legacy_fields,
                normalize_fields(s3s_fields),
                "legacy/s3s-multipart field mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
            assert_eq!(
                legacy_file, multer_file,
                "legacy/multer file content mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
            assert_eq!(
                legacy_file, s3s_file,
                "legacy/s3s-multipart file content mismatch: {} chunk {chunk} shape {shape:?}",
                case.label
            );
        }
    }

    /// The differential guard runs for every measured stream shape, so a shape
    /// that silently consumed less than the body cannot be reported as a result.
    fn verify(case: &Case) {
        verify_shape(case, Shape::Ready);
        verify_shape(case, Shape::Suspending);
    }

    /// The unknown-total-length path (`total_len = None`) must yield the same
    /// output as the exact-length path; it only skips strict validation.
    fn verify_none_degradation(case: &Case, cache: &Arc<Vec<Bytes>>) {
        let body_len = case.load.body.len() as u64;
        let (some_fields, some_file) = collect_legacy(body_stream(Arc::clone(cache), Shape::Ready), Some(body_len));
        let (none_fields, none_file) = collect_legacy(body_stream(Arc::clone(cache), Shape::Ready), None);
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

    fn register_triple<M: Measurement>(group: &mut BenchmarkGroup<'_, M>, case: &Case, chunk: usize) {
        let legacy_cache = case.cache(chunk);
        let s3s_cache = case.cache(chunk);
        let multer_cache = case.cache(chunk);
        let name = case.name(chunk);
        let body_len = case.load.body.len() as u64;

        group.throughput(Throughput::Bytes(body_len));
        group.bench_with_input(BenchmarkId::new("legacy", &name), &(), |b, &()| {
            b.iter(|| drive_legacy(body_stream(Arc::clone(&legacy_cache), Shape::Ready), Some(body_len)));
        });
        group.bench_with_input(BenchmarkId::new("s3s_multipart", &name), &(), |b, &()| {
            b.iter(|| drive_s3s(mp_body_stream(Arc::clone(&s3s_cache), Shape::Ready)));
        });
        group.bench_with_input(BenchmarkId::new("multer", &name), &(), |b, &()| {
            b.iter(|| drive_multer(body_stream(Arc::clone(&multer_cache), Shape::Ready), body_len, MulterMode::Aggregate));
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

        let mut group = c.benchmark_group("multipart/parsers");
        apply_settings(&mut group, Settings::Default);
        for case in &cases {
            for &chunk in CHUNK_SIZES {
                register_triple(&mut group, case, chunk);
            }
        }
        group.finish();
    }

    fn bench_heavy_matrix(c: &mut Criterion) {
        let case = build_case("L-16MiB", load_l(16 * 1024 * 1024));
        verify(&case);

        let mut group = c.benchmark_group("multipart/parsers_heavy");
        apply_settings(&mut group, Settings::Heavy);
        for &chunk in CHUNK_SIZES {
            register_triple(&mut group, &case, chunk);
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
        let cache = case.cache(chunk);
        verify_none_degradation(&case, &cache);
        let body_len = case.load.body.len() as u64;

        let mut group = c.benchmark_group("multipart/reference");
        apply_settings(&mut group, Settings::Default);
        group.throughput(Throughput::Bytes(body_len));

        group.bench_function("legacy/some_total_len", |b| {
            b.iter(|| drive_legacy(body_stream(Arc::clone(&cache), Shape::Ready), Some(body_len)));
        });
        group.bench_function("legacy/none_total_len", |b| {
            b.iter(|| drive_legacy(body_stream(Arc::clone(&cache), Shape::Ready), None));
        });
        group.bench_function("s3s_multipart/aggregate", |b| {
            b.iter(|| drive_s3s(mp_body_stream(Arc::clone(&cache), Shape::Ready)));
        });
        group.bench_function("multer/raw_drain", |b| {
            b.iter(|| drive_multer(body_stream(Arc::clone(&cache), Shape::Ready), body_len, MulterMode::RawDrain));
        });
        group.bench_function("multer/aggregate", |b| {
            b.iter(|| drive_multer(body_stream(Arc::clone(&cache), Shape::Ready), body_len, MulterMode::Aggregate));
        });
        group.finish();
    }

    /// Diagnostic group: large fields region amplifies the legacy parser's
    /// per-chunk re-parse of the accumulated buffer (O(n^2/c) until the file
    /// part signal arrives); the other two scan forward.
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
                register_triple(&mut group, case, chunk);
            }
        }
        group.finish();
    }

    /// The handoff path: legacy `take_file_stream` against the
    /// `s3s-multipart` `take_data_stream` + `into_final`, which is what the S3
    /// integration uses to give the body to the storage layer. `multer` has no
    /// equivalent, so this group is a pair.
    fn bench_take(c: &mut Criterion) {
        let cases = [build_case("L-1MiB", load_l(1024 * 1024)), build_case("M", load_m())];
        for case in &cases {
            verify(case);
        }

        let chunk = 16 * 1024;
        let mut group = c.benchmark_group("multipart/take");
        apply_settings(&mut group, Settings::Default);
        for case in &cases {
            let legacy_cache = case.cache(chunk);
            let s3s_cache = case.cache(chunk);
            let name = case.name(chunk);
            let body_len = case.load.body.len() as u64;
            group.throughput(Throughput::Bytes(body_len));
            group.bench_with_input(BenchmarkId::new("legacy", &name), &(), |b, &()| {
                b.iter(|| drive_legacy_take(body_stream(Arc::clone(&legacy_cache), Shape::Ready), Some(body_len)));
            });
            group.bench_with_input(BenchmarkId::new("s3s_multipart", &name), &(), |b, &()| {
                b.iter(|| drive_s3s_take(mp_body_stream(Arc::clone(&s3s_cache), Shape::Ready)));
            });
        }
        group.finish();
    }

    /// The suspend/resume path: every chunk is preceded by a `Pending`, so each
    /// parser step pays one wakeup. An always-ready stream cannot show this.
    ///
    /// The two shapes are not interchangeable for every parser: a stream that
    /// suspends bounds how much a parser can pull into its internal buffer in
    /// one poll, so a parser that accumulates whatever is ready measures
    /// differently under the two shapes. Two body sizes are measured here so
    /// that dependence is visible instead of being read as noise.
    fn bench_pending(c: &mut Criterion) {
        let cases = [
            build_case("L-64KiB", load_l(64 * 1024)),
            build_case("L-1MiB", load_l(1024 * 1024)),
        ];
        for case in &cases {
            verify(case);
        }

        let chunk = 16 * 1024;
        let mut group = c.benchmark_group("multipart/pending");
        apply_settings(&mut group, Settings::Default);
        for case in &cases {
            let legacy_cache = case.cache(chunk);
            let s3s_cache = case.cache(chunk);
            let multer_cache = case.cache(chunk);
            let name = case.name(chunk);
            let body_len = case.load.body.len() as u64;
            group.throughput(Throughput::Bytes(body_len));
            group.bench_with_input(BenchmarkId::new("legacy", &name), &(), |b, &()| {
                b.iter(|| drive_legacy(body_stream(Arc::clone(&legacy_cache), Shape::Suspending), Some(body_len)));
            });
            group.bench_with_input(BenchmarkId::new("s3s_multipart", &name), &(), |b, &()| {
                b.iter(|| drive_s3s(mp_body_stream(Arc::clone(&s3s_cache), Shape::Suspending)));
            });
            group.bench_with_input(BenchmarkId::new("multer", &name), &(), |b, &()| {
                b.iter(|| {
                    drive_multer(body_stream(Arc::clone(&multer_cache), Shape::Suspending), body_len, MulterMode::Aggregate)
                });
            });
        }
        group.finish();
    }

    criterion_group!(
        benches,
        bench_main_matrix,
        bench_heavy_matrix,
        bench_reference,
        bench_diagnostic,
        bench_take,
        bench_pending
    );
}

#[cfg(fuzzing)]
criterion::criterion_main!(harness::benches);

#[cfg(not(fuzzing))]
fn main() {}

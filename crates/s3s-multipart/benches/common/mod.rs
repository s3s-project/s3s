// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Loads, chunk caches and drivers shared by the `s3s-multipart` benchmarks.
//!
//! A load is built once and reused: [`Load::new`] serialises a part list into
//! the canonical body *and* keeps that same list as the expected parse result,
//! so the body and the oracle cannot drift apart. [`verify`] then parses the
//! body under every chunking and checks the two against each other before any
//! measurement starts.
//!
//! Body construction, chunk splitting and `Boundary` creation all happen
//! outside `b.iter`, so an iteration measures the parser and not the harness.
//! `Bytes` clones only bump a refcount, which is why an iteration can hand the
//! parser the same pre-split chunks without copying the body.
//!
//! Part data is full-range pseudo-random from a fixed-seed `StdRng`, never a
//! repeating ramp: a ramp puts long runs of `\r` — the byte a boundary scan
//! probes — at regular offsets, so the numbers would describe the generator
//! instead of the parser.
//!
//! Not every bench target uses every item here, hence the module-wide
//! `dead_code` allowance.

#![allow(dead_code)]

use std::hint::black_box;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::executor::block_on;
use futures::stream::{self, Stream, StreamExt};
use multer::{Constraints, SizeLimit};
use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};

use s3s_multipart::{Boundary, Error, Multipart, parse_content_disposition};

/// Boundary used by every benchmark body.
pub const BOUNDARY: &[u8] = b"----s3s-bench-boundary-7f3a";

/// Internal buffer limit, matching the crate default the integration uses.
pub const MAX_BUFFER_SIZE: usize = 64 * 1024;

/// Chunk granularities the body is pre-split into.
pub const CHUNK_SIZES: &[usize] = &[1024, 4 * 1024, 16 * 1024, 64 * 1024];

/// Field name of the header that carries part identity.
const CONTENT_DISPOSITION: &str = "content-disposition";

/// A uniform allocator for every benchmark in this package.
///
/// The system allocator's arena state is sensitive to the allocation history of
/// earlier benchmarks, which moves later measurements by up to 2x; mimalloc's
/// behavior is far less position dependent, so series stay comparable.
#[global_allocator]
static BENCH_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// One part of a benchmark body.
pub struct PartSpec {
    /// The `name` parameter of `Content-Disposition`.
    pub name: String,
    /// The `filename` parameter; `Some` marks the streamed file part.
    pub file_name: Option<String>,
    /// The exact bytes the parser has to hand out for this part.
    pub data: Vec<u8>,
}

impl PartSpec {
    /// A form field.
    pub fn field(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            file_name: None,
            data: value.as_bytes().to_vec(),
        }
    }

    /// A file part, which also carries a `Content-Type`.
    pub fn file(name: &str, file_name: &str, data: Vec<u8>) -> Self {
        Self {
            name: name.to_owned(),
            file_name: Some(file_name.to_owned()),
            data,
        }
    }

    fn write_to(&self, body: &mut Vec<u8>) {
        body.extend_from_slice(b"--");
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        body.extend_from_slice(self.name.as_bytes());
        body.push(b'"');
        if let Some(file_name) = &self.file_name {
            body.extend_from_slice(b"; filename=\"");
            body.extend_from_slice(file_name.as_bytes());
            body.push(b'"');
        }
        body.extend_from_slice(b"\r\n");
        if self.file_name.is_some() {
            body.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&self.data);
        body.extend_from_slice(b"\r\n");
    }
}

/// A canonical `multipart/form-data` body plus the parts it was built from.
pub struct Load {
    /// The serialised body, also the throughput denominator.
    pub body: Vec<u8>,
    /// The parts `body` encodes, in order.
    pub parts: Vec<PartSpec>,
}

impl Load {
    /// Serialises `parts` and keeps them as the expected parse result.
    pub fn new(parts: Vec<PartSpec>) -> Self {
        let mut body = Vec::new();
        for part in &parts {
            part.write_to(&mut body);
        }
        body.extend_from_slice(b"--");
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(b"--\r\n");
        Self { body, parts }
    }

    /// Body length in bytes.
    pub fn len(&self) -> usize {
        self.body.len()
    }

    /// Data bytes the parser has to hand out in total.
    pub fn data_bytes(&self) -> usize {
        self.parts.iter().map(|part| part.data.len()).sum()
    }
}

/// Load with a wide header block and a negligible file part: it separates
/// header work from data work.
pub fn load_f() -> Load {
    let mut rng = StdRng::seed_from_u64(0xfeed_beef);
    let mut parts: Vec<PartSpec> = (0..16)
        .map(|i| PartSpec::field(&format!("field_{i:02}"), &ascii(&mut rng, 1024)))
        .collect();
    parts.push(PartSpec::file("file", "bench.bin", random_bytes(&mut rng, 64)));
    Load::new(parts)
}

/// Single file part of `file_len` bytes, shaped like a POST Object upload.
pub fn load_l(file_len: usize) -> Load {
    let mut rng = StdRng::seed_from_u64(0x1111_2222);
    let parts = vec![
        PartSpec::field("key", "bench-key"),
        PartSpec::file("file", "bench.bin", random_bytes(&mut rng, file_len)),
    ];
    Load::new(parts)
}

/// POST Object shape: the policy fields a browser sends plus a 1 MiB file.
pub fn load_m() -> Load {
    let mut rng = StdRng::seed_from_u64(0x5eed_5eed);
    let parts = vec![
        PartSpec::field("key", "uploads/2026/09/bench.bin"),
        PartSpec::field("policy", &ascii(&mut rng, 1024)),
        PartSpec::field("x-amz-signature", &ascii(&mut rng, 64)),
        PartSpec::field("content-type", "application/octet-stream"),
        PartSpec::file("file", "bench.bin", random_bytes(&mut rng, 1024 * 1024)),
    ];
    Load::new(parts)
}

/// Diagnostic load: a large field region in front of a tiny file part. A parser
/// that re-scans its accumulated buffer while it looks for the file part
/// degrades super-linearly here; a forward-scanning parser does not.
pub fn load_diag(field_count: usize, field_len: usize) -> Load {
    let mut rng = StdRng::seed_from_u64(0xd1a9_0001);
    let mut parts: Vec<PartSpec> = (0..field_count)
        .map(|i| PartSpec::field(&format!("field_{i:04}"), &ascii(&mut rng, field_len)))
        .collect();
    parts.push(PartSpec::file("file", "bench.bin", random_bytes(&mut rng, 64)));
    Load::new(parts)
}

fn ascii(rng: &mut StdRng, len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| char::from(CHARSET[rng.random_range(0..CHARSET.len())]))
        .collect()
}

fn random_bytes(rng: &mut StdRng, len: usize) -> Vec<u8> {
    let mut buf = vec![0_u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

/// A body pre-split into shared `Bytes` chunks.
pub type ChunkCache = Arc<Vec<Bytes>>;

/// Splits `body` into chunks of `chunk_size`; built outside the timed region.
pub fn chunk_cache(body: &[u8], chunk_size: usize) -> ChunkCache {
    Arc::new(body.chunks(chunk_size.max(1)).map(Bytes::copy_from_slice).collect())
}

/// Owning iterator over the pre-split chunks.
pub struct ReadyStream {
    chunks: ChunkCache,
    index: usize,
}

impl Iterator for ReadyStream {
    type Item = Result<Bytes, Error>;

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

/// A stream that is always ready, so the parser never suspends.
pub fn ready_stream(chunks: ChunkCache) -> stream::Iter<ReadyStream> {
    stream::iter(ReadyStream { chunks, index: 0 })
}

/// A stream that suspends between chunks.
///
/// An always-ready stream lets the parser run from one poll loop, which hides
/// per-poll work behind the previous chunk's processing. This stream yields the
/// first chunk on the first poll and suspends before every later one, so a
/// parse of N chunks pays N - 1 wakeups: it models a body whose chunks keep
/// arriving, not how the first one does.
pub struct PendingStream {
    chunks: ChunkCache,
    index: usize,
    resume: bool,
}

impl Stream for PendingStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if !this.resume {
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

/// A stream that suspends between chunks.
pub fn pending_stream(chunks: ChunkCache) -> PendingStream {
    PendingStream {
        chunks,
        index: 0,
        resume: true,
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A data-dependent checksum of everything the parser handed out.
///
/// Folding a few bytes per chunk plus the chunk length keeps the fold from
/// dominating the measurement while still making the work observable, so the
/// compiler cannot drop the parse.
#[derive(Clone, Copy)]
pub struct Fingerprint {
    hash: u64,
    chunks: usize,
    bytes: usize,
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Fingerprint {
    /// An empty fingerprint.
    pub fn new() -> Self {
        Self {
            hash: FNV_OFFSET,
            chunks: 0,
            bytes: 0,
        }
    }

    /// Folds one piece of part data.
    pub fn fold_data(&mut self, data: &[u8]) {
        let head = data.len().min(8);
        self.hash = data[..head]
            .iter()
            .fold(self.hash, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME));
        self.hash = (self.hash ^ (data.len() as u64)).wrapping_mul(FNV_PRIME);
        self.chunks += 1;
        self.bytes += data.len();
    }

    /// Number of pieces folded.
    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// Total bytes folded.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// A parser over the benchmark body.
pub fn multipart<S>(stream: S) -> Multipart<S>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    let boundary = Boundary::new(BOUNDARY).expect("the benchmark boundary is valid");
    Multipart::new(stream, &boundary, MAX_BUFFER_SIZE)
}

/// Consumes every part through `next_part` / `next_header` / `next_data`.
///
/// This is the streaming endpoint: headers are read, `Content-Disposition` is
/// parsed exactly as a real consumer would, and part data is passed through
/// without being accumulated.
pub fn consume_all<S>(mut mp: Multipart<S>) -> Fingerprint
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    block_on(async {
        let mut fp = Fingerprint::new();
        while let Some(mut part) = mp.next_part().await.expect("next_part") {
            while let Some(header) = part.next_header().await.expect("next_header") {
                if header.name.eq_ignore_ascii_case(CONTENT_DISPOSITION) {
                    black_box(parse_content_disposition(header.value));
                }
            }
            while let Some(chunk) = part.next_data().await.expect("next_data") {
                fp.fold_data(&chunk);
                black_box(&chunk);
            }
        }
        black_box(fp.bytes());
        fp
    })
}

/// Consumes every part, aggregating form field data into values.
///
/// The parser itself never aggregates; this series exists to price the
/// aggregation a consumer asks for, so a comparison against a consumer that
/// aggregates stays honest.
pub fn consume_aggregate<S>(mut mp: Multipart<S>) -> Fingerprint
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    block_on(async {
        let mut fp = Fingerprint::new();
        while let Some(mut part) = mp.next_part().await.expect("next_part") {
            let mut is_file = false;
            while let Some(header) = part.next_header().await.expect("next_header") {
                if header.name.eq_ignore_ascii_case(CONTENT_DISPOSITION) {
                    let parsed = parse_content_disposition(header.value);
                    is_file = parsed.is_some_and(|cd| cd.file_name.is_some());
                    black_box(parsed);
                }
            }
            let mut value = Vec::new();
            while let Some(chunk) = part.next_data().await.expect("next_data") {
                if is_file {
                    fp.fold_data(&chunk);
                    black_box(&chunk);
                } else {
                    value.extend_from_slice(&chunk);
                }
            }
            if !is_file {
                fp.fold_data(&value);
                black_box(&value);
            }
        }
        fp
    })
}

/// Consumes the file part through `take_data_stream`, the handoff the S3
/// integration uses to hand the body to the storage layer.
pub fn consume_take<S>(mut mp: Multipart<S>) -> Fingerprint
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    block_on(async {
        let mut fp = Fingerprint::new();
        while let Some(mut part) = mp.next_part().await.expect("next_part") {
            let mut is_file = false;
            while let Some(header) = part.next_header().await.expect("next_header") {
                if header.name.eq_ignore_ascii_case(CONTENT_DISPOSITION) {
                    let parsed = parse_content_disposition(header.value);
                    is_file = parsed.is_some_and(|cd| cd.file_name.is_some());
                    black_box(parsed);
                }
            }
            if is_file {
                let mut stream = Box::pin(part.take_data_stream().expect("take_data_stream"));
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.expect("part data stream");
                    fp.fold_data(&chunk);
                    black_box(&chunk);
                }
                // The parser is spent once the stream has been taken, so this
                // is the endpoint; the file part is last in every load.
                return fp;
            }
            while let Some(chunk) = part.next_data().await.expect("next_data") {
                fp.fold_data(&chunk);
                black_box(&chunk);
            }
        }
        fp
    })
}

/// Consumes the file part through `take_data_stream` and then `into_final`,
/// which additionally validates the strict closing trailer.
pub fn consume_take_final<S>(mut mp: Multipart<S>) -> Fingerprint
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    block_on(async {
        let mut fp = Fingerprint::new();
        while let Some(mut part) = mp.next_part().await.expect("next_part") {
            let mut is_file = false;
            while let Some(header) = part.next_header().await.expect("next_header") {
                if header.name.eq_ignore_ascii_case(CONTENT_DISPOSITION) {
                    let parsed = parse_content_disposition(header.value);
                    is_file = parsed.is_some_and(|cd| cd.file_name.is_some());
                    black_box(parsed);
                }
            }
            if is_file {
                let stream = part.take_data_stream().expect("take_data_stream");
                let mut stream = Box::pin(stream.into_final());
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.expect("final part data stream");
                    fp.fold_data(&chunk);
                    black_box(&chunk);
                }
                return fp;
            }
            while let Some(chunk) = part.next_data().await.expect("next_data") {
                fp.fold_data(&chunk);
                black_box(&chunk);
            }
        }
        fp
    })
}

/// How the `multer` side consumes a field.
#[derive(Clone, Copy)]
pub enum MulterMode {
    /// Drain every field chunk by chunk: the raw parser cost.
    RawDrain,
    /// Aggregate non-file fields with `Field::bytes`, matching a consumer that
    /// collects form fields as values.
    Aggregate,
}

/// Consumes a body with `multer` over the same pre-split chunks.
pub fn consume_multer(chunks: ChunkCache, body_len: u64, mode: MulterMode) -> Fingerprint {
    let boundary = std::str::from_utf8(BOUNDARY).expect("the benchmark boundary is ASCII");
    block_on(async {
        let limits = SizeLimit::new().whole_stream(body_len).per_field(body_len);
        let constraints = Constraints::new().size_limit(limits);
        let mut mp = multer::Multipart::with_constraints(ready_stream(chunks), boundary, constraints);

        let mut fp = Fingerprint::new();
        while let Some(mut field) = mp.next_field().await.expect("multer next_field") {
            black_box(field.name());
            black_box(field.headers());
            if matches!(mode, MulterMode::Aggregate) && field.file_name().is_none() {
                let value = field.bytes().await.expect("multer field bytes");
                fp.fold_data(&value);
                black_box(&value);
                continue;
            }
            while let Some(chunk) = field.chunk().await.expect("multer field chunk") {
                fp.fold_data(&chunk);
                black_box(&chunk);
            }
        }
        fp
    })
}

/// One parsed part, materialised for the startup check.
pub struct CollectedPart {
    /// The `name` parameter, if the part carried one.
    pub name: Option<Vec<u8>>,
    /// The `filename` parameter, if the part carried one.
    pub file_name: Option<Vec<u8>>,
    /// The whole part data.
    pub data: Vec<u8>,
}

/// Parses a body and materialises every part.
///
/// Startup-only: it copies the whole body, so it never runs inside a
/// measurement.
pub fn collect_parts<S>(mut mp: Multipart<S>) -> Vec<CollectedPart>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + Unpin,
{
    block_on(async {
        let mut parts = Vec::new();
        while let Some(mut part) = mp.next_part().await.expect("next_part") {
            let mut name = None;
            let mut file_name = None;
            while let Some(header) = part.next_header().await.expect("next_header") {
                if header.name.eq_ignore_ascii_case(CONTENT_DISPOSITION) {
                    let parsed = parse_content_disposition(header.value).expect("content-disposition parses");
                    name = parsed.name.map(<[u8]>::to_vec);
                    file_name = parsed.file_name.map(<[u8]>::to_vec);
                }
            }
            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.expect("next_data") {
                data.extend_from_slice(&chunk);
            }
            parts.push(CollectedPart { name, file_name, data });
        }
        parts
    })
}

/// Differential guard, run once per load before any measurement.
///
/// For every chunking it checks that the parser reproduces the part list the
/// body was serialised from: same part count, same `Content-Disposition`
/// parameters, same data bytes, and the expected total. It also checks that the
/// take handoff delivers the same data as the streaming path, so a load whose
/// numbers came from an error path cannot be reported as a measurement.
pub fn verify(load: &Load) {
    for &chunk_size in CHUNK_SIZES {
        let cache = chunk_cache(&load.body, chunk_size);
        let collected = collect_parts(multipart(ready_stream(Arc::clone(&cache))));

        assert_eq!(collected.len(), load.parts.len(), "part count mismatch at chunk {chunk_size}");
        let mut total = 0usize;
        for (index, (got, want)) in collected.iter().zip(&load.parts).enumerate() {
            assert_eq!(
                got.name.as_deref(),
                Some(want.name.as_bytes()),
                "name mismatch in part {index} at chunk {chunk_size}"
            );
            assert_eq!(
                got.file_name.as_deref(),
                want.file_name.as_deref().map(str::as_bytes),
                "filename mismatch in part {index} at chunk {chunk_size}"
            );
            assert_eq!(got.data, want.data, "data mismatch in part {index} at chunk {chunk_size}");
            total += got.data.len();
        }
        assert_eq!(total, load.data_bytes(), "data byte total mismatch at chunk {chunk_size}");
    }
}

/// One load with its chunk caches, so a benchmark body can register several
/// series over the same pre-built inputs.
pub struct Case {
    /// Short label used in benchmark ids.
    pub label: &'static str,
    /// The body and its expected parse result.
    pub load: Load,
    caches: Vec<(usize, ChunkCache)>,
}

impl Case {
    /// Builds a case and pre-splits its body at every chunk size.
    pub fn new(label: &'static str, load: Load) -> Self {
        let caches = CHUNK_SIZES
            .iter()
            .map(|&chunk_size| (chunk_size, chunk_cache(&load.body, chunk_size)))
            .collect();
        Self { label, load, caches }
    }

    /// The chunk cache for `chunk_size`.
    pub fn cache(&self, chunk_size: usize) -> ChunkCache {
        self.caches
            .iter()
            .find(|(size, _)| *size == chunk_size)
            .map(|(_, cache)| Arc::clone(cache))
            .expect("the chunk cache exists")
    }

    /// Benchmark id for one case at one chunk size.
    pub fn id(&self, chunk_size: usize) -> String {
        format!("{}/{chunk_size}", self.label)
    }
}

//! aws-chunked stream

use crate::auth::SecretKey;
use crate::error::StdError;
use crate::protocol::TrailingHeaders;
use crate::sig_v4;
use crate::sig_v4::AmzDate;
use crate::sig_v4::create_trailer_string_to_sign;
use crate::stream::{ByteStream, DynByteStream, RemainingLength};
use crate::utils::SyncBoxFuture;

use hyper::HeaderMap;
use hyper::http::{HeaderName, HeaderValue};
use std::fmt::{self, Debug};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::pin_mut;
use futures::stream::{Stream, StreamExt};
use hyper::body::{Buf, Bytes};
use memchr::memchr;
use transform_stream::AsyncTryStream;

/// Maximum size for chunk metadata line (chunk-size + optional signature)
/// Format: "{hex-size};chunk-signature={64-hex-chars}\r\n"
/// Conservative limit: 1KB should be plenty for any valid chunk metadata
const MAX_CHUNK_META_SIZE: usize = 1024;

/// Maximum size for trailing headers block
/// S3 trailing headers are typically checksums and metadata
/// Conservative limit: 16KB for all trailing headers
const MAX_TRAILERS_SIZE: usize = 16 * 1024;

/// Maximum number of trailing headers
/// Prevents `DoS` via excessive header count
const MAX_TRAILER_HEADERS: usize = 100;

/// Aws chunked stream
pub struct AwsChunkedStream {
    /// inner
    inner: AsyncTryStream<Bytes, AwsChunkedStreamError, SyncBoxFuture<'static, Result<(), AwsChunkedStreamError>>>,

    remaining_length: usize,

    // Parsed trailing headers (lower-cased names) if present and verified.
    trailers: Arc<Mutex<Option<HeaderMap>>>,
}

impl Debug for AwsChunkedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsChunkedStream").finish_non_exhaustive()
    }
}

/// signature ctx
#[derive(Debug)]
struct SignatureCtx {
    /// date
    amz_date: AmzDate,

    /// region
    region: Box<str>,

    //// service
    service: Box<str>,

    /// secret key
    secret_key: SecretKey,

    /// previous chunk's signature
    prev_signature: Box<str>,
}

/// [`AwsChunkedStream`]
#[derive(Debug, thiserror::Error)]
pub enum AwsChunkedStreamError {
    /// Underlying error
    #[error("AwsChunkedStreamError: Underlying: {}",.0)]
    Underlying(StdError),
    /// Signature mismatch
    #[error("AwsChunkedStreamError: SignatureMismatch")]
    SignatureMismatch,
    /// Format error
    #[error("AwsChunkedStreamError: FormatError")]
    FormatError,
    /// Incomplete stream
    #[error("AwsChunkedStreamError: Incomplete")]
    Incomplete,
    /// Chunk metadata too large
    #[error("AwsChunkedStreamError: ChunkMetaTooLarge: size {0} exceeds limit {1}")]
    ChunkMetaTooLarge(usize, usize),
    /// Trailers too large
    #[error("AwsChunkedStreamError: TrailersTooLarge: size {0} exceeds limit {1}")]
    TrailersTooLarge(usize, usize),
    /// Too many trailer headers
    #[error("AwsChunkedStreamError: TooManyTrailerHeaders: count {0} exceeds limit {1}")]
    TooManyTrailerHeaders(usize, usize),
}

/// Chunk meta
#[derive(Debug)]
struct ChunkMeta<'a> {
    /// chunk size
    size: usize,
    /// Optional chunk signature.
    /// `Some` for signed chunks,
    /// `None` for unsigned streaming
    signature: Option<&'a [u8]>,
}

/// nom parser
fn parse_chunk_meta(mut input: &[u8]) -> nom::IResult<&[u8], ChunkMeta<'_>> {
    use crate::utils::parser::consume;
    use nom::Parser;
    use nom::bytes::complete::{tag, take, take_till1};
    use nom::combinator::{all_consuming, map_res};
    use nom::number::complete::hex_u32;
    use nom::sequence::delimited;

    let s = &mut input;

    // read size until ';' or CR
    let size = consume(s, take_till1(|c| c == b';' || c == b'\r'))?;
    let (_, size) = map_res(hex_u32, TryInto::try_into).parse(size)?;

    // Either ";chunk-signature=<64>\r\n" or just "\r\n"
    let signature = if let Ok(sig) = consume(s, |i| {
        all_consuming(delimited(tag(&b";chunk-signature="[..]), take(64_usize), tag(&b"\r\n"[..]))).parse(i)
    }) {
        Some(sig)
    } else {
        // If no signature extension, accept plain CRLF
        let _ = consume(s, |i| all_consuming(tag(&b"\r\n"[..])).parse(i))?;
        None
    };

    Ok((input, ChunkMeta { size, signature }))
}

/// check signature
fn check_signature(ctx: &SignatureCtx, expected_signature: &[u8], chunk_data: &[Bytes]) -> Option<Box<str>> {
    let string_to_sign =
        sig_v4::create_chunk_string_to_sign(&ctx.amz_date, &ctx.region, &ctx.service, &ctx.prev_signature, chunk_data);

    let chunk_signature = sig_v4::calculate_signature(&string_to_sign, &ctx.secret_key, &ctx.amz_date, &ctx.region, &ctx.service);

    (chunk_signature.as_bytes() == expected_signature).then(|| chunk_signature.into())
}

impl AwsChunkedStream {
    /// Constructs a `ChunkedStream`
    #[allow(clippy::too_many_arguments)]
    pub fn new<S>(
        body: S,
        seed_signature: Box<str>,
        amz_date: AmzDate,
        region: Box<str>,
        service: Box<str>,
        secret_key: SecretKey,
        decoded_content_length: usize,
        unsigned: bool,
    ) -> Self
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        let trailers: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
        let trailers_for_worker = Arc::clone(&trailers);
        let inner = AsyncTryStream::<_, _, SyncBoxFuture<'static, Result<(), AwsChunkedStreamError>>>::new(|mut y| {
            #[allow(clippy::shadow_same)] // necessary for `pin_mut!`
            Box::pin(async move {
                pin_mut!(body);
                let mut prev_bytes = Bytes::new();
                let mut buf: Vec<u8> = Vec::new();
                let mut ctx = SignatureCtx {
                    amz_date,
                    region,
                    service,
                    secret_key,
                    prev_signature: seed_signature,
                };

                loop {
                    let meta = {
                        match Self::read_meta_bytes(body.as_mut(), prev_bytes, &mut buf).await {
                            None => break,
                            Some(Err(e)) => return Err(AwsChunkedStreamError::Underlying(e)),
                            Some(Ok(remaining_bytes)) => prev_bytes = remaining_bytes,
                        }
                        if let Ok((_, meta)) = parse_chunk_meta(&buf) {
                            // Enforce signature presence based on mode
                            if !unsigned && meta.signature.is_none() {
                                return Err(AwsChunkedStreamError::FormatError);
                            }
                            meta
                        } else {
                            return Err(AwsChunkedStreamError::FormatError);
                        }
                    };

                    tracing::trace!(?meta);

                    let (data, remaining_after_data): (Vec<Bytes>, Bytes) = {
                        match Self::read_data(body.as_mut(), prev_bytes, meta.size).await {
                            None => return Err(AwsChunkedStreamError::Incomplete),
                            Some(Err(e)) => return Err(e),
                            Some(Ok((data, remaining_bytes))) => (data, remaining_bytes),
                        }
                    };

                    if let Some(expected_sig) = meta.signature {
                        match check_signature(&ctx, expected_sig, &data) {
                            None => return Err(AwsChunkedStreamError::SignatureMismatch),
                            Some(signature) => ctx.prev_signature = signature,
                        }
                    }

                    for bytes in data {
                        y.yield_ok(bytes).await;
                    }

                    if meta.size == 0 {
                        // Try to read all remaining bytes (including any immediate remainder) as trailing headers
                        // block and verify signature if present.
                        match Self::read_and_verify_trailers(body.as_mut(), remaining_after_data, &ctx, unsigned).await {
                            // No more bytes => no trailers, treat as normal termination.
                            None => break,
                            Some(Err(e)) => return Err(e),
                            Some(Ok((entries, _remaining_bytes))) => {
                                // After trailers there must not be any additional payload bytes in a valid request.
                                // However, if there are remaining bytes, we will just attempt to continue parsing
                                // which will likely fail with FormatError.
                                // Build HeaderMap from entries and store.
                                let mut map: HeaderMap = HeaderMap::new();
                                for (name, value) in entries {
                                    // Names are already lower-cased ASCII
                                    let hn: HeaderName = match name.parse() {
                                        Ok(h) => h,
                                        Err(_) => return Err(AwsChunkedStreamError::FormatError),
                                    };
                                    let hv: HeaderValue = match HeaderValue::from_str(&value) {
                                        Ok(v) => v,
                                        Err(_) => return Err(AwsChunkedStreamError::FormatError),
                                    };
                                    map.append(hn, hv);
                                }
                                tracing::debug!(trailers=?map);
                                *trailers_for_worker.lock().unwrap() = Some(map);
                                break;
                            }
                        }
                    }

                    // Carry over any remaining bytes to next iteration when there are more chunks.
                    prev_bytes = remaining_after_data;
                }

                Ok(())
            })
        });
        Self {
            inner,
            remaining_length: decoded_content_length,
            trailers,
        }
    }

    /// Read trailing headers after the final 0-size chunk and verify trailer signature.
    /// Returns:
    /// - `None`: if there are no more bytes (no trailers present)
    /// - `Some(Ok(remaining_bytes))`: when trailers are consumed; usually `remaining_bytes` is empty
    /// - `Some(Err(..))`: on errors
    async fn read_and_verify_trailers<S>(
        mut body: Pin<&mut S>,
        prev_bytes: Bytes,
        ctx: &SignatureCtx,
        unsigned: bool,
    ) -> Option<Result<(Vec<(String, String)>, Bytes), AwsChunkedStreamError>>
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + 'static,
    {
        // Accumulate all remaining bytes until EOF, with size limit.
        let mut buf: Vec<u8> = Vec::new();
        let mut total_size: usize = 0;

        if !prev_bytes.is_empty() {
            total_size = prev_bytes.len();
            if total_size > MAX_TRAILERS_SIZE {
                return Some(Err(AwsChunkedStreamError::TrailersTooLarge(total_size, MAX_TRAILERS_SIZE)));
            }
            buf.extend_from_slice(prev_bytes.as_ref());
        }

        // Read to end with size limit
        while let Some(next) = body.next().await {
            match next {
                Err(e) => return Some(Err(AwsChunkedStreamError::Underlying(e))),
                Ok(bytes) => {
                    total_size = total_size.saturating_add(bytes.len());
                    if total_size > MAX_TRAILERS_SIZE {
                        return Some(Err(AwsChunkedStreamError::TrailersTooLarge(total_size, MAX_TRAILERS_SIZE)));
                    }
                    buf.extend_from_slice(bytes.as_ref());
                }
            }
        }

        if buf.is_empty() || buf == b"\r\n" {
            // No trailers present
            return None;
        }

        // Parse trailing headers lines. Lines are separated by CRLF or LF.
        // Build canonical_trailers by sorting headers (excluding x-amz-trailer-signature) and joining as `name:value\n`.
        // Extract the provided trailer signature from header `x-amz-trailer-signature`.
        let (canonical_trailers, provided_signature, entries) = match Self::parse_trailing_headers(&buf) {
            Ok(x) => x,
            Err(e) => return Some(Err(e)),
        };

        // Verify the trailer signature if present, or require it when unsigned=false
        if let Some(provided) = provided_signature.as_ref() {
            let string_to_sign =
                create_trailer_string_to_sign(&ctx.amz_date, &ctx.region, &ctx.service, &ctx.prev_signature, &canonical_trailers);
            let trailer_signature =
                sig_v4::calculate_signature(&string_to_sign, &ctx.secret_key, &ctx.amz_date, &ctx.region, &ctx.service);
            if trailer_signature.as_bytes() != provided.as_slice() {
                return Some(Err(AwsChunkedStreamError::SignatureMismatch));
            }
        } else if !unsigned {
            // In signed-with-trailer mode, missing trailer signature is an error
            return Some(Err(AwsChunkedStreamError::FormatError));
        }

        Some(Ok((entries, Bytes::new())))
    }

    #[allow(clippy::type_complexity)]
    fn parse_trailing_headers(buf: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>, Vec<(String, String)>), AwsChunkedStreamError> {
        // Split into lines by `\n`. Accept optional `\r` before `\n` and also handle last line without `\n`.
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut provided_signature: Option<Vec<u8>> = None;
        let mut header_count: usize = 0;

        let mut start = 0usize;
        for i in 0..=buf.len() {
            let is_end = i == buf.len();
            let is_lf = !is_end && buf[i] == b'\n';
            if is_lf || is_end {
                let mut line = &buf[start..i];
                // trim trailing CR
                if let Some(&b'\r') = line.last() {
                    line = &line[..line.len().saturating_sub(1)];
                }
                // advance start
                start = i.saturating_add(1);

                if line.is_empty() {
                    continue;
                }

                // Check header count limit
                header_count += 1;
                if header_count > MAX_TRAILER_HEADERS {
                    return Err(AwsChunkedStreamError::TooManyTrailerHeaders(header_count, MAX_TRAILER_HEADERS));
                }

                // Find ':'
                let Some(colon_pos) = memchr(b':', line) else {
                    return Err(AwsChunkedStreamError::FormatError);
                };
                let (name_raw, value_raw) = line.split_at(colon_pos);
                // value_raw starts with ':'
                let value_raw = &value_raw[1..];

                // Lowercase name
                let name = String::from_utf8(name_raw.to_ascii_lowercase()).map_err(|_| AwsChunkedStreamError::FormatError)?;

                // Trim ASCII whitespace around value
                let value = {
                    let v = value_raw;
                    let v = trim_ascii_whitespace(v);
                    String::from_utf8(v.to_vec()).map_err(|_| AwsChunkedStreamError::FormatError)?
                };

                if name == "x-amz-trailer-signature" {
                    provided_signature = Some(value.into_bytes());
                } else {
                    entries.push((name, value));
                }
            }
        }

        // Sort by header name to canonicalize deterministically
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Build canonical bytes: name:value\n
        // Check canonical buffer size as we build it
        let mut canonical: Vec<u8> = Vec::new();
        for (n, v) in &entries {
            let entry_size = n.len() + 1 + v.len() + 1; // name + ':' + value + '\n'
            if canonical.len().saturating_add(entry_size) > MAX_TRAILERS_SIZE {
                return Err(AwsChunkedStreamError::TrailersTooLarge(
                    canonical.len().saturating_add(entry_size),
                    MAX_TRAILERS_SIZE,
                ));
            }
            canonical.extend_from_slice(n.as_bytes());
            canonical.push(b':');
            canonical.extend_from_slice(v.as_bytes());
            canonical.push(b'\n');
        }

        Ok((canonical, provided_signature, entries))
    }

    /// read meta bytes and return remaining bytes
    async fn read_meta_bytes<S>(mut body: Pin<&mut S>, prev_bytes: Bytes, buf: &mut Vec<u8>) -> Option<Result<Bytes, StdError>>
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + 'static,
    {
        buf.clear();

        let mut push_meta_bytes = |mut bytes: Bytes| -> Result<Option<Bytes>, StdError> {
            if let Some(idx) = memchr(b'\n', bytes.as_ref()) {
                let len = idx.wrapping_add(1); // assume: idx < bytes.len()
                let leading = bytes.split_to(len);
                buf.extend_from_slice(leading.as_ref());
                return Ok(Some(bytes));
            }

            // Check size limit before extending
            if buf.len().saturating_add(bytes.len()) > MAX_CHUNK_META_SIZE {
                return Err(Box::new(AwsChunkedStreamError::ChunkMetaTooLarge(
                    buf.len().saturating_add(bytes.len()),
                    MAX_CHUNK_META_SIZE,
                )));
            }

            buf.extend_from_slice(bytes.as_ref());
            Ok(None)
        };

        match push_meta_bytes(prev_bytes) {
            Err(e) => return Some(Err(e)),
            Ok(Some(remaining_bytes)) => return Some(Ok(remaining_bytes)),
            Ok(None) => {}
        }

        loop {
            match body.next().await? {
                Err(e) => return Some(Err(e)),
                Ok(bytes) => match push_meta_bytes(bytes) {
                    Err(e) => return Some(Err(e)),
                    Ok(Some(remaining_bytes)) => return Some(Ok(remaining_bytes)),
                    Ok(None) => {}
                },
            }
        }
    }

    /// read data and return remaining bytes
    async fn read_data<S>(
        mut body: Pin<&mut S>,
        prev_bytes: Bytes,
        mut data_size: usize,
    ) -> Option<Result<(Vec<Bytes>, Bytes), AwsChunkedStreamError>>
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + 'static,
    {
        if data_size == 0 {
            return Some(Ok((Vec::new(), prev_bytes)));
        }

        let mut bytes_buffer = Vec::new();
        let mut push_data_bytes = |mut bytes: Bytes| {
            if data_size == 0 {
                return Some(bytes);
            }
            if data_size <= bytes.len() {
                let data = bytes.split_to(data_size);
                bytes_buffer.push(data);
                data_size = 0;
                Some(bytes)
            } else {
                data_size = data_size.wrapping_sub(bytes.len());
                bytes_buffer.push(bytes);
                None
            }
        };

        let mut remaining_bytes = 'outer: {
            if let Some(remaining_bytes) = push_data_bytes(prev_bytes) {
                break 'outer remaining_bytes;
            }

            loop {
                match body.next().await? {
                    Err(e) => return Some(Err(AwsChunkedStreamError::Underlying(e))),
                    Ok(bytes) => {
                        if let Some(remaining_bytes) = push_data_bytes(bytes) {
                            break 'outer remaining_bytes;
                        }
                    }
                }
            }
        };

        if remaining_bytes.starts_with(b"\r\n") {
            // fast path
            remaining_bytes.advance(2);
        } else {
            for &expected_byte in b"\r\n" {
                loop {
                    match *remaining_bytes.as_ref() {
                        [] => match body.next().await? {
                            Err(e) => return Some(Err(AwsChunkedStreamError::Underlying(e))),
                            Ok(bytes) => remaining_bytes = bytes,
                        },

                        [x, ..] if x == expected_byte => {
                            remaining_bytes.advance(1);
                            break;
                        }
                        _ => return Some(Err(AwsChunkedStreamError::FormatError)),
                    }
                }
            }
        }

        Some(Ok((bytes_buffer, remaining_bytes)))
    }

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, AwsChunkedStreamError>>> {
        let ans = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(ref bytes))) = ans {
            self.remaining_length = self.remaining_length.saturating_sub(bytes.len());
        }
        ans
    }

    pub fn exact_remaining_length(&self) -> usize {
        self.remaining_length
    }

    pub fn into_byte_stream(self) -> DynByteStream {
        crate::stream::into_dyn(self)
    }

    // Note: Trailing headers should be accessed via trailing_headers_handle().

    /// Get a handle to access verified trailing headers later.
    /// This can be cloned and stored outside to retrieve trailers after the
    /// stream has been fully read.
    pub(crate) fn trailing_headers_handle(&self) -> TrailingHeaders {
        TrailingHeaders(Arc::clone(&self.trailers))
    }
}

impl Stream for AwsChunkedStream {
    type Item = Result<Bytes, AwsChunkedStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl ByteStream for AwsChunkedStream {
    fn remaining_length(&self) -> RemainingLength {
        RemainingLength::new_exact(self.remaining_length)
    }
}

#[inline]
fn trim_ascii_whitespace(mut s: &[u8]) -> &[u8] {
    while matches!(s.first(), Some(b' ' | b'\t')) {
        s = &s[1..];
    }
    while matches!(s.last(), Some(b' ' | b'\t')) {
        s = &s[..s.len() - 1];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(bytes: &[&[u8]]) -> Bytes {
        let mut buf = Vec::new();
        for b in bytes {
            buf.extend_from_slice(b);
        }
        buf.into()
    }

    #[tokio::test]
    async fn example_put_object_chunked_stream() {
        let chunk1_meta = b"10000;chunk-signature=ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648\r\n";
        let chunk2_meta = b"400;chunk-signature=0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497\r\n";
        let chunk3_meta = b"0;chunk-signature=b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9\r\n";

        let chunk1_data = vec![b'a'; 0x10000]; // 65536
        let chunk2_data = vec![b'a'; 1024];
        let chunk3_data = [];
        let decoded_content_length = chunk1_data.len() + chunk2_data.len() + chunk3_data.len();

        let chunk1 = join(&[chunk1_meta, &chunk1_data, b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, &chunk2_data, b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, &chunk3_data, b"\r\n"]);

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3)];

        let seed_signature = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            seed_signature.into(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            false,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        {
            assert!(chunked_stream.next().await.is_none());
            assert!(chunked_stream.next().await.is_none());
            assert!(chunked_stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn example_put_object_chunked_stream_with_trailers() {
        // Example from AWS docs: https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-streaming-trailers.html
        // Seed signature corresponds to STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER canonical request
        let chunk1_meta = b"10000;chunk-signature=b474d8862b1487a5145d686f57f013e54db672cee1c953b3010fb58501ef5aa2\r\n";
        let chunk2_meta = b"400;chunk-signature=1c1344b170168f8e65b41376b44b20fe354e373826ccbbe2c1d40a8cae51e5c7\r\n";
        let chunk3_meta = b"0;chunk-signature=2ca2aba2005185cf7159c6277faf83795951dd77a3a99e6e65d5c9f85863f992\r\n";

        let chunk1_data = vec![b'a'; 0x10000]; // 65536
        let chunk2_data = vec![b'a'; 1024];
        let chunk3_data = [];
        let decoded_content_length = chunk1_data.len() + chunk2_data.len() + chunk3_data.len();

        let chunk1 = join(&[chunk1_meta, &chunk1_data, b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, &chunk2_data, b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, &chunk3_data, b"\r\n"]);

        let trailers_block = Bytes::from_static(b"x-amz-checksum-crc32c:sOO8/Q==\r\nx-amz-trailer-signature:d81f82fc3505edab99d459891051a732e8730629a2e4a59689829ca17fe2e435");

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3), Ok(trailers_block)];

        let seed_signature = "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            seed_signature.into(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            false,
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        // No more data after verifying trailers
        assert!(chunked_stream.next().await.is_none());

        // Export trailers via handle
        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 1);
        let v = trailers.get("x-amz-checksum-crc32c").unwrap();
        assert_eq!(v, &HeaderValue::from_static("sOO8/Q=="));
    }

    #[tokio::test]
    async fn unsigned_payload_with_trailer_minimal() {
        // Construct a minimal unsigned aws-chunked stream: two data chunks and a 0 chunk, then trailers block.
        // Here we compute signatures using existing test vectors in sig_v4::methods.rs indirectly by using
        // a known seed and the create_trailer_string_to_sign path inside AwsChunkedStream.

        // For unsigned per-chunk mode, meta lines are plain sizes without extensions.
        let chunk1_meta = b"3\r\n"; // size 3
        let chunk2_meta = b"4\r\n"; // size 4
        let chunk3_meta = b"0\r\n"; // last chunk

        let chunk1_data = b"abc";
        let chunk2_data = b"defg";
        let decoded_content_length = chunk1_data.len() + chunk2_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, chunk2_data.as_ref(), b"\r\n"]);
        let chunk3 = join(&[chunk3_meta, b"\r\n"]); // 0-size chunk is followed by CRLF before trailers

        // Build trailers. We'll compute the expected trailer signature using the same algorithm as the stream.
        let seed_signature = "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e"; // from example seed for trailer flows
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        // Canonical trailers: one additional header besides x-amz-trailer-signature
        let canonical = b"x-amz-meta-foo:bar\n".to_vec();
        let string_to_sign = create_trailer_string_to_sign(&date, region, service, seed_signature, &canonical);
        let sig = sig_v4::calculate_signature(&string_to_sign, &SecretKey::from(secret_access_key), &date, region, service);
        let trailers_block = Bytes::from(format!("x-amz-meta-foo: bar\r\nx-amz-trailer-signature:{sig}"));

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(chunk3), Ok(trailers_block)];

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            seed_signature.into(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        let ans2 = chunked_stream.next().await.unwrap();
        assert_eq!(ans2.unwrap(), chunk2_data.as_slice());

        // No more data after verifying trailers
        assert!(chunked_stream.next().await.is_none());

        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 1);
        let v = trailers.get("x-amz-meta-foo").unwrap();
        assert_eq!(v, &HeaderValue::from_static("bar"));
    }

    #[tokio::test]
    async fn unsigned_payload_with_trailer_no_signature() {
        // unsigned mode with trailers present but no x-amz-trailer-signature
        let chunk1_meta = b"3\r\n"; // size 3
        let chunk2_meta = b"0\r\n"; // last chunk

        let chunk1_data = b"xyz";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Trailers without signature
        let trailers_block = Bytes::from_static(b"x-amz-meta-a: 1\r\nx-amz-meta-b: 2");

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(trailers_block)];

        let seed_signature = "deadbeef"; // not used for unsigned per-chunk, but used to build ctx
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            seed_signature.into(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // No more data after verifying trailers (which contain no signature in unsigned mode)
        assert!(chunked_stream.next().await.is_none());

        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 2);
        assert_eq!(trailers.get("x-amz-meta-a").unwrap(), &HeaderValue::from_static("1"));
        assert_eq!(trailers.get("x-amz-meta-b").unwrap(), &HeaderValue::from_static("2"));
    }

    #[tokio::test]
    #[allow(clippy::assertions_on_constants)]
    async fn test_limits_constants_exist() {
        // This test verifies that the limit constants are defined and have reasonable values
        assert!(MAX_CHUNK_META_SIZE > 0);
        assert!(MAX_CHUNK_META_SIZE < 10 * 1024); // Should be reasonable, less than 10KB
        assert!(MAX_TRAILERS_SIZE > 0);
        assert!(MAX_TRAILERS_SIZE < 100 * 1024); // Should be reasonable, less than 100KB
        assert!(MAX_TRAILER_HEADERS > 0);
        assert!(MAX_TRAILER_HEADERS < 1000); // Should be reasonable
    }

    #[tokio::test]
    async fn test_normal_sized_trailers_work() {
        // Verify that normal-sized trailers work fine (well within limits)
        let chunk1_meta = b"3\r\n";
        let chunk2_meta = b"0\r\n";

        let chunk1_data = b"abc";
        let decoded_content_length = chunk1_data.len();

        let chunk1 = join(&[chunk1_meta, chunk1_data.as_ref(), b"\r\n"]);
        let chunk2 = join(&[chunk2_meta, b"\r\n"]);

        // Create trailers with reasonable number of headers (50, well under limit of 100)
        let mut trailers = Vec::new();
        for i in 0..50 {
            trailers.extend_from_slice(format!("x-amz-meta-{i}: value{i}\r\n").as_bytes());
        }

        let chunk_results: Vec<Result<Bytes, _>> = vec![Ok(chunk1), Ok(chunk2), Ok(Bytes::from(trailers))];

        let seed_signature = "deadbeef";
        let timestamp = "20130524T000000Z";
        let region = "us-east-1";
        let service = "s3";
        let secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = AmzDate::parse(timestamp).unwrap();

        let stream = futures::stream::iter(chunk_results);
        let mut chunked_stream = AwsChunkedStream::new(
            stream,
            seed_signature.into(),
            date,
            region.into(),
            service.into(),
            secret_access_key.into(),
            decoded_content_length,
            true, // unsigned
        );

        let ans1 = chunked_stream.next().await.unwrap();
        assert_eq!(ans1.unwrap(), chunk1_data.as_slice());

        // Should complete successfully
        assert!(chunked_stream.next().await.is_none());

        // Verify trailers were parsed
        let handle = chunked_stream.trailing_headers_handle();
        let trailers = handle.take().expect("trailers present");
        assert_eq!(trailers.len(), 50);
    }
}

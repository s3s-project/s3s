// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! multipart/form-data encoding for POST Object
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>
//!

use crate::error::StdError;
use crate::stream::ByteStream;
use crate::utils::SyncBoxFuture;

use std::fmt::{self, Debug};
use std::mem;
use std::pin::Pin;
use std::task::Poll;

use futures::stream::{Stream, StreamExt};
use hyper::body::Bytes;
use memchr::memchr_iter;
use transform_stream::{AsyncTryStream, Yielder};

/// Maximum size for boundary matching buffer in `FileStream`
/// This buffer accumulates bytes when looking for a boundary pattern that spans chunks
/// Conservative limit: 64KB should be more than enough for any reasonable boundary pattern
const MAX_BOUNDARY_BUFFER_SIZE: usize = 64 * 1024;

/// Limits for multipart form parsing
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
pub struct MultipartLimits {
    /// Maximum size per form field in bytes
    pub max_field_size: usize,
    /// Maximum total size for all form fields combined in bytes
    pub max_fields_size: usize,
    /// Maximum number of parts in multipart form
    pub max_parts: usize,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        Self {
            max_field_size: 1024 * 1024,       // 1 MB
            max_fields_size: 20 * 1024 * 1024, // 20 MB
            max_parts: 1000,
        }
    }
}

/// Form file
#[derive(Debug)]
pub struct File {
    /// name
    #[allow(dead_code)] // FIXME: discard this field?
    pub name: String,
    /// content type
    #[allow(dead_code)] // FIXME: discard this field?
    pub content_type: Option<String>,
    /// stream
    pub stream: Option<FileStream>,
}

/// multipart/form-data for POST Object
#[derive(Debug)]
pub struct Multipart {
    /// fields
    fields: Vec<(String, String)>,
    /// file
    pub file: File,
}

impl Multipart {
    pub fn fields(&self) -> &[(String, String)] {
        &self.fields
    }

    pub fn take_file_stream(&mut self) -> Option<FileStream> {
        self.file.stream.take()
    }

    /// Finds field value
    #[must_use]
    pub fn find_field_value<'a>(&'a self, name: &str) -> Option<&'a str> {
        let upper_bound = self.fields.partition_point(|x| x.0.as_str() <= name);
        if upper_bound == 0 {
            return None;
        }
        let pair = &self.fields[upper_bound - 1];
        if pair.0.as_str() != name {
            return None;
        }
        Some(pair.1.as_str())
    }

    /// Create a Multipart for testing purposes
    ///
    /// This mirrors the normalization performed in `try_parse` by:
    /// - lowercasing field names
    /// - sorting fields by name
    #[cfg(test)]
    pub(crate) fn new_for_test(mut fields: Vec<(String, String)>, file: File) -> Self {
        // Normalize field names to lowercase to match production behavior.
        for (name, _) in &mut fields {
            *name = name.to_ascii_lowercase();
        }

        // Sort fields by name so that `find_field_value`'s binary search works correctly.
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Self { fields, file }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MultipartError {
    #[error("MultipartError: Underlying: {0}")]
    Underlying(#[source] StdError),
    #[error("MultipartError: InvalidFormat")]
    InvalidFormat,
    #[error("MultipartError: FieldTooLarge: field size {0} bytes exceeds limit of {1} bytes")]
    FieldTooLarge(usize, usize),
    #[error("MultipartError: TotalSizeTooLarge: total form fields size {0} bytes exceeds limit of {1} bytes")]
    TotalSizeTooLarge(usize, usize),
    #[error("MultipartError: TooManyParts: part count {0} exceeds limit of {1}")]
    TooManyParts(usize, usize),
    #[error("MultipartError: FileTooLarge: file size {0} bytes exceeds limit of {1} bytes")]
    FileTooLarge(u64, u64),
}

/// Aggregates a file stream into a Vec<Bytes> with a size limit.
/// Returns error if the total size exceeds the limit.
pub async fn aggregate_file_stream_limited(mut stream: FileStream, max_size: u64) -> Result<Vec<Bytes>, MultipartError> {
    use futures::stream::StreamExt;
    let mut vec = Vec::new();
    let mut total_size: u64 = 0;

    while let Some(result) = stream.next().await {
        let bytes = result.map_err(|e| MultipartError::Underlying(Box::new(e)))?;
        total_size = total_size.saturating_add(bytes.len() as u64);
        if total_size > max_size {
            return Err(MultipartError::FileTooLarge(total_size, max_size));
        }
        vec.push(bytes);
    }
    Ok(vec)
}

/// transform multipart
///
/// When `total_len` is known (the request's `Content-Length`), the parser
/// derives the exact file content length and the file stream validates the
/// canonical `--\r\n` closing trailer. See [`derive_file_len`].
///
/// # Errors
/// Returns an `Err` if the format is invalid
pub async fn transform_multipart<S>(
    body_stream: S,
    boundary: &'_ [u8],
    limits: MultipartLimits,
    total_len: Option<u64>,
) -> Result<Multipart, MultipartError>
where
    S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
{
    let mut buf = Vec::new();

    let mut body = Box::pin(body_stream);

    let mut pat: Box<[u8]> = {
        let mut v = Vec::with_capacity(boundary.len().saturating_add(4));
        v.extend_from_slice(b"--");
        v.extend_from_slice(boundary);
        v.extend_from_slice(b"\r\n");
        v.into()
    };

    let mut fields = Vec::new();
    let mut total_fields_size: usize = 0;
    let mut parts_count: usize = 0;

    loop {
        // copy bytes to buf
        match body.as_mut().next().await {
            None => return Err(MultipartError::InvalidFormat),
            Some(Err(e)) => return Err(MultipartError::Underlying(e)),
            Some(Ok(bytes)) => {
                // Check if adding these bytes would exceed reasonable buffer size
                if buf.len().saturating_add(bytes.len()) > limits.max_fields_size {
                    return Err(MultipartError::TotalSizeTooLarge(
                        buf.len().saturating_add(bytes.len()),
                        limits.max_fields_size,
                    ));
                }
                buf.extend_from_slice(&bytes);
            }
        }

        // try to parse
        match try_parse(
            body,
            pat,
            &buf,
            &mut fields,
            boundary,
            &mut total_fields_size,
            &mut parts_count,
            limits,
            total_len,
        ) {
            Err((b, p)) => {
                body = b;
                pat = p;
            }
            Ok(ans) => return ans,
        }
    }
}

/// Derives the exact file content length from the request's total body length.
///
/// The canonical multipart layout guarantees:
/// `total = pre_file + file + CRLF + "--" + boundary + "--" + CRLF`
/// where `pre_file` is everything before the file content. At the moment the
/// file part is detected, the parser's accumulated buffer contains every byte
/// polled so far: `buf_len = pre_file + prev_len` (the `prev_len` content
/// bytes of the final chunk).
///
/// Returns `None` when the total length is unknown (chunked request) or any
/// arithmetic overflows; the caller then falls back to aggregating the file.
fn derive_file_len(total_len: Option<u64>, boundary: &[u8], buf_len: u64, prev_len: usize) -> Option<u64> {
    let trailer_len = u64::try_from(boundary.len()).ok()?.checked_add(8)?;
    total_len?
        .checked_sub(buf_len)?
        .checked_add(u64::try_from(prev_len).ok()?)?
        .checked_sub(trailer_len)
}

/// try to parse data buffer, pat: b"--{boundary}\r\n"
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn try_parse<S>(
    body: Pin<Box<S>>,
    pat: Box<[u8]>,
    buf: &'_ [u8],
    fields: &'_ mut Vec<(String, String)>,
    boundary: &'_ [u8],
    total_fields_size: &'_ mut usize,
    parts_count: &'_ mut usize,
    limits: MultipartLimits,
    total_len: Option<u64>,
) -> Result<Result<Multipart, MultipartError>, (Pin<Box<S>>, Box<[u8]>)>
where
    S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
{
    #[allow(clippy::indexing_slicing)]
    let pat_without_crlf = &pat[..pat.len().wrapping_sub(2)];

    fields.clear();
    // Reset counters since we're re-parsing from scratch
    *total_fields_size = 0;
    *parts_count = 0;

    let mut lines = CrlfLines { slice: buf };

    // first line
    match lines.next_line() {
        None => return Err((body, pat)),
        Some(&[]) => {
            // first boundary
            match lines.next_line() {
                None => return Err((body, pat)),
                Some(line) => {
                    if line != pat_without_crlf {
                        return Ok(Err(MultipartError::InvalidFormat));
                    }
                }
            }
        }
        Some(line) => {
            if line != pat_without_crlf {
                return Ok(Err(MultipartError::InvalidFormat));
            }
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 2];
    loop {
        // Check parts count limit
        *parts_count += 1;
        if *parts_count > limits.max_parts {
            return Ok(Err(MultipartError::TooManyParts(*parts_count, limits.max_parts)));
        }

        let (idx, parsed_headers) = match httparse::parse_headers(lines.slice, &mut headers) {
            Ok(httparse::Status::Complete(ans)) => ans,
            Ok(_) => return Err((body, pat)),
            Err(_) => return Ok(Err(MultipartError::InvalidFormat)),
        };
        lines.slice = lines.slice.split_at(idx).1;

        let mut content_disposition_bytes = None;
        let mut content_type_bytes = None;
        for header in parsed_headers {
            if header.name.eq_ignore_ascii_case("Content-Disposition") {
                content_disposition_bytes = Some(header.value);
            } else if header.name.eq_ignore_ascii_case("Content-Type") {
                content_type_bytes = Some(header.value);
            } else {
                continue;
            }
        }

        let content_disposition = match content_disposition_bytes.map(parse_content_disposition) {
            None => return Err((body, pat)),
            Some(Err(_)) => return Ok(Err(MultipartError::InvalidFormat)),
            Some(Ok((_, c))) => c,
        };
        if content_disposition.name.eq_ignore_ascii_case("file") {
            let content_type = match content_type_bytes.map(std::str::from_utf8) {
                None => None,
                Some(Err(_)) => return Ok(Err(MultipartError::InvalidFormat)),
                Some(Ok(s)) => Some(s),
            };
            let remaining_bytes = if lines.slice.is_empty() {
                None
            } else {
                Some(Bytes::copy_from_slice(lines.slice))
            };
            let prev_len = remaining_bytes.as_ref().map_or(0, Bytes::len);
            let content_len = u64::try_from(buf.len())
                .ok()
                .and_then(|buf_len| derive_file_len(total_len, boundary, buf_len, prev_len));
            let file_stream = FileStream::new(body, boundary, remaining_bytes, content_len);
            let file_name = content_disposition.filename.unwrap_or(content_disposition.name);
            let file = File {
                name: file_name.to_owned(),
                content_type: content_type.map(str::to_owned),
                stream: Some(file_stream),
            };

            let mut fields = mem::take(fields);
            for x in &mut fields {
                x.0.make_ascii_lowercase();
            }
            fields.sort_by(|lhs, rhs| lhs.0.as_str().cmp(rhs.0.as_str()));

            return Ok(Ok(Multipart { fields, file }));
        }

        let value = match lines.split_to(pat_without_crlf) {
            None => return Err((body, pat)),
            Some(b) => {
                #[allow(clippy::indexing_slicing)]
                let b = &b[..b.len().saturating_sub(2)];

                // Check per-field size limit
                if b.len() > limits.max_field_size {
                    return Ok(Err(MultipartError::FieldTooLarge(b.len(), limits.max_field_size)));
                }

                // Check total fields size limit
                *total_fields_size = total_fields_size.saturating_add(b.len());
                if *total_fields_size > limits.max_fields_size {
                    return Ok(Err(MultipartError::TotalSizeTooLarge(*total_fields_size, limits.max_fields_size)));
                }

                match std::str::from_utf8(b) {
                    Err(_) => return Ok(Err(MultipartError::InvalidFormat)),
                    Ok(s) => s,
                }
            }
        };

        fields.push((content_disposition.name.to_owned(), value.to_owned()));
    }
}

/// File stream error
#[derive(Debug, thiserror::Error)]
pub enum FileStreamError {
    /// Incomplete error
    #[error("FileStreamError: Incomplete")]
    Incomplete,
    /// IO error
    #[error("FileStreamError: Underlying: {0}")]
    Underlying(#[source] StdError),
    /// Boundary buffer too large
    #[error("FileStreamError: BoundaryBufferTooLarge: size {0} exceeds limit {1}")]
    BoundaryBufferTooLarge(usize, usize),
    /// Bytes after the file do not match the canonical `--\r\n` closing
    /// delimiter (e.g., the file is not the last part or an epilogue exists)
    #[error("FileStreamError: InvalidTrailer")]
    InvalidTrailer,
    /// The yielded byte count disagrees with the declared content length
    #[error("FileStreamError: LengthMismatch: {remaining} bytes remaining")]
    LengthMismatch { remaining: u64 },
}

/// File stream
pub struct FileStream {
    /// inner stream
    inner: AsyncTryStream<Bytes, FileStreamError, SyncBoxFuture<'static, Result<(), FileStreamError>>>,
    /// exact content length derived from the request's `Content-Length`, if known
    content_len: Option<u64>,
    /// remaining content bytes; counts down as bytes are yielded
    remaining: u64,
    /// set once a length mismatch has been reported, so the terminal error is
    /// yielded at most once
    ended: bool,
}

impl Debug for FileStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileStream")
            .field("content_len", &self.content_len)
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl FileStream {
    /// Constructs a `FileStream`
    #[allow(clippy::too_many_lines)]
    fn new<S>(body: Pin<Box<S>>, boundary: &'_ [u8], prev_bytes: Option<Bytes>, content_len: Option<u64>) -> Self
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        /// internal async generator
        #[allow(clippy::too_many_lines)]
        async fn generate<S>(
            mut y: Yielder<Result<Bytes, FileStreamError>>,
            mut body: Pin<Box<S>>,
            crlf_pat: Box<[u8]>,
            prev_bytes: Option<Bytes>,
            strict: bool,
        ) -> Result<(), FileStreamError>
        where
            S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
        {
            let mut state: u8;

            let mut bytes;
            let mut buf: Vec<u8> = Vec::new();

            if let Some(b) = prev_bytes {
                state = 2;
                bytes = b;
            } else {
                state = 1;
                bytes = Bytes::new();
            }

            'dfa: loop {
                match state {
                    1 => {
                        match body.as_mut().next().await {
                            None => return Err(FileStreamError::Incomplete),
                            Some(Err(e)) => return Err(FileStreamError::Underlying(e)),
                            Some(Ok(b)) => bytes = b,
                        }
                        state = 2;
                        continue 'dfa;
                    }
                    2 => {
                        for idx in memchr_iter(b'\r', bytes.as_ref()) {
                            #[allow(clippy::indexing_slicing)]
                            let remaining = &bytes[idx..];

                            if remaining.len() >= crlf_pat.len() {
                                if remaining.starts_with(&crlf_pat) {
                                    // File content ends here. Keep whatever follows the
                                    // boundary pattern for trailer validation.
                                    let mut rest = bytes.split_off(idx);
                                    y.yield_ok(bytes).await;
                                    if strict {
                                        rest = rest.slice(crlf_pat.len()..);
                                        bytes = rest;
                                        state = 4;
                                    } else {
                                        return Ok(());
                                    }
                                    continue 'dfa;
                                }
                                continue;
                            }

                            if crlf_pat.starts_with(remaining) {
                                y.yield_ok(bytes.split_to(idx)).await;
                                buf.extend_from_slice(&bytes);
                                bytes.clear();
                                state = 3;
                                continue 'dfa;
                            }

                            continue;
                        }

                        y.yield_ok(mem::take(&mut bytes)).await;
                        state = 1;
                        continue 'dfa;
                    }
                    3 => {
                        match body.as_mut().next().await {
                            None => return Err(FileStreamError::Incomplete),
                            Some(Err(e)) => return Err(FileStreamError::Underlying(e)),
                            Some(Ok(b)) => {
                                // Check buffer size limit before extending
                                if buf.len().saturating_add(b.len()) > MAX_BOUNDARY_BUFFER_SIZE {
                                    return Err(FileStreamError::BoundaryBufferTooLarge(
                                        buf.len().saturating_add(b.len()),
                                        MAX_BOUNDARY_BUFFER_SIZE,
                                    ));
                                }
                                buf.extend_from_slice(&b);
                            }
                        }
                        bytes = Bytes::from(mem::take(&mut buf));
                        state = 2;
                        continue 'dfa;
                    }
                    4 => {
                        // expect the closing "--"
                        while bytes.len() < 2 {
                            let chunk = match body.as_mut().next().await {
                                None => return Err(FileStreamError::Incomplete),
                                Some(Err(e)) => return Err(FileStreamError::Underlying(e)),
                                Some(Ok(b)) => b,
                            };
                            buf.extend_from_slice(&bytes);
                            buf.extend_from_slice(&chunk);
                            if buf.len() > MAX_BOUNDARY_BUFFER_SIZE {
                                return Err(FileStreamError::BoundaryBufferTooLarge(buf.len(), MAX_BOUNDARY_BUFFER_SIZE));
                            }
                            bytes = Bytes::from(mem::take(&mut buf));
                        }
                        if !bytes.starts_with(b"--") {
                            return Err(FileStreamError::InvalidTrailer);
                        }
                        bytes = bytes.slice(2..);
                        state = 5;
                        continue 'dfa;
                    }
                    5 => {
                        // expect the final CRLF
                        while bytes.len() < 2 {
                            let chunk = match body.as_mut().next().await {
                                None => return Err(FileStreamError::Incomplete),
                                Some(Err(e)) => return Err(FileStreamError::Underlying(e)),
                                Some(Ok(b)) => b,
                            };
                            buf.extend_from_slice(&bytes);
                            buf.extend_from_slice(&chunk);
                            if buf.len() > MAX_BOUNDARY_BUFFER_SIZE {
                                return Err(FileStreamError::BoundaryBufferTooLarge(buf.len(), MAX_BOUNDARY_BUFFER_SIZE));
                            }
                            bytes = Bytes::from(mem::take(&mut buf));
                        }
                        if !bytes.starts_with(b"\r\n") {
                            return Err(FileStreamError::InvalidTrailer);
                        }
                        bytes = bytes.slice(2..);
                        state = 6;
                        continue 'dfa;
                    }
                    6 => {
                        // nothing may follow the closing delimiter
                        if !bytes.is_empty() {
                            return Err(FileStreamError::InvalidTrailer);
                        }
                        match body.as_mut().next().await {
                            None => return Ok(()),
                            Some(Err(e)) => return Err(FileStreamError::Underlying(e)),
                            Some(Ok(_)) => return Err(FileStreamError::InvalidTrailer),
                        }
                    }
                    #[allow(clippy::unreachable)]
                    _ => unreachable!(),
                }
            }
        }

        // `\r\n--{boundary}`
        let crlf_pat: Box<[u8]> = {
            let mut v = Vec::with_capacity(boundary.len().saturating_add(4));
            v.extend_from_slice(b"\r\n--");
            v.extend_from_slice(boundary);
            v.into()
        };

        let strict = content_len.is_some();

        Self {
            inner: AsyncTryStream::new(|y| -> SyncBoxFuture<'static, Result<(), FileStreamError>> {
                Box::pin(generate(y, body, crlf_pat, prev_bytes, strict))
            }),
            content_len,
            remaining: content_len.unwrap_or(0),
            ended: false,
        }
    }

    /// Returns the exact content length derived from the request's
    /// `Content-Length` header, if it is known.
    pub fn content_len(&self) -> Option<u64> {
        self.content_len
    }
}

impl Stream for FileStream {
    type Item = Result<Bytes, FileStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Enforce the declared length: never deliver content beyond
                // the claim. The canonical trailer validation in the generator
                // guarantees the counter reaches zero on clean completion.
                if this.content_len.is_some() {
                    let len = bytes.len() as u64;
                    if len > this.remaining {
                        this.ended = true;
                        return Poll::Ready(Some(Err(FileStreamError::LengthMismatch {
                            remaining: this.remaining,
                        })));
                    }
                    this.remaining -= len;
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            // The stream ended cleanly but delivered fewer bytes than claimed.
            // Hyper guarantees consistency between `Content-Length` and the
            // body in production, so this should be unreachable there; surface
            // it as an error instead of a silent truncation regardless.
            Poll::Ready(None) if this.content_len.is_some() && this.remaining > 0 => {
                this.ended = true;
                Poll::Ready(Some(Err(FileStreamError::LengthMismatch {
                    remaining: this.remaining,
                })))
            }
            // An error surfaced by the generator is terminal for this stream:
            // mark it ended so the terminal length check does not emit a
            // second, redundant error.
            Poll::Ready(Some(Err(e))) => {
                this.ended = true;
                Poll::Ready(Some(Err(e)))
            }
            other => other,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // `Stream::size_hint` bounds the number of remaining *chunks*, which
        // is unknowable here; byte-level length information is exposed via
        // `ByteStream::remaining_length` instead.
        (0, None)
    }
}

impl ByteStream for FileStream {
    fn remaining_length(&self) -> crate::stream::RemainingLength {
        match usize::try_from(self.remaining) {
            Ok(remaining) if self.content_len.is_some() => crate::stream::RemainingLength::new_exact(remaining),
            _ => crate::stream::RemainingLength::unknown(),
        }
    }
}

/// CRLF lines
struct CrlfLines<'a> {
    /// slice
    slice: &'a [u8],
}

impl<'a> CrlfLines<'a> {
    /// poll next line
    fn next_line(&mut self) -> Option<&'a [u8]> {
        for idx in memchr_iter(b'\n', self.slice) {
            if idx == 0 {
                continue;
            }

            #[allow(clippy::indexing_slicing)]
            let byte = self.slice[idx.wrapping_sub(1)];

            if byte == b'\r' {
                #[allow(clippy::indexing_slicing)]
                let left = &self.slice[..idx.wrapping_sub(1)];

                #[allow(clippy::indexing_slicing)]
                let right = &self.slice[idx.wrapping_add(1)..];

                self.slice = right;
                return Some(left);
            }
        }
        if self.slice.is_empty() {
            None
        } else {
            Some(mem::take(&mut self.slice))
        }
    }

    /// split by pattern and return previous bytes
    fn split_to(&mut self, line_pat: &'_ [u8]) -> Option<&'a [u8]> {
        let mut len: usize = 0;
        let mut lines = Self { slice: self.slice };
        loop {
            let line = lines.next_line()?;
            if line == line_pat {
                len = len.min(self.slice.len());

                #[allow(clippy::indexing_slicing)]
                let ans = &self.slice[..len];

                self.slice = lines.slice;
                return Some(ans);
            }
            len = len.wrapping_add(line.len()).saturating_add(2);
        }
    }
}

/// Content-Disposition
#[derive(Debug)]
struct ContentDisposition<'a> {
    /// name
    name: &'a str,
    /// filename
    filename: Option<&'a str>,
}

/// parse content disposition value
fn parse_content_disposition(input: &[u8]) -> nom::IResult<&[u8], ContentDisposition<'_>> {
    use nom::Parser;
    use nom::bytes::complete::{tag, take, take_till1};
    use nom::combinator::{all_consuming, map_res, opt};
    use nom::sequence::{delimited, preceded};

    // TODO: escape?

    let parse_name = delimited(
        tag(&b"name=\""[..]),
        map_res(take_till1(|c| c == b'"'), std::str::from_utf8),
        take(1_usize),
    );

    let parse_filename = delimited(
        tag(&b"filename=\""[..]),
        map_res(take_till1(|c| c == b'"'), std::str::from_utf8),
        take(1_usize),
    );

    let mut parse = all_consuming((
        preceded(tag(&b"form-data; "[..]), parse_name),
        opt(preceded(tag(&b"; "[..]), parse_filename)),
    ));

    let (remaining, (name, filename)) = parse.parse(input)?;

    Ok((remaining, ContentDisposition { name, filename }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;
    use std::slice;

    async fn aggregate_file_stream(mut file_stream: FileStream) -> Result<Bytes, FileStreamError> {
        let mut buf = Vec::new();

        while let Some(bytes) = file_stream.next().await {
            buf.extend(bytes?);
        }

        Ok(buf.into())
    }

    #[test]
    fn content_disposition() {
        {
            let text = b"form-data; name=\"Signature\"";
            let (_, ans) = parse_content_disposition(text).unwrap();
            assert_eq!(ans.name, "Signature");
            assert_eq!(ans.filename, None);
        }
        {
            let text = b"form-data; name=\"file\"; filename=\"MyFilename.jpg\"";
            let (_, ans) = parse_content_disposition(text).unwrap();
            assert_eq!(ans.name, "file");
            assert_eq!(ans.filename, Some("MyFilename.jpg"));
        }
    }

    #[test]
    fn split_to() {
        let bytes = b"\r\n----\r\nasd\r\nqwe";
        {
            let mut lines = CrlfLines { slice: bytes };
            assert_eq!(lines.split_to(b"----"), Some(b"\r\n".as_ref()));
            assert_eq!(lines.slice, b"asd\r\nqwe");
        }
        {
            let mut lines = CrlfLines { slice: bytes };
            assert_eq!(lines.split_to(b"xxx"), None);
        }
    }

    #[tokio::test]
    async fn file_stream() {
        let file_content = "\r\n too much crlf \r\n--\r\n\r\n\r\n";

        let body = b"\n too much crlf \r\n--\r\n\r\n\r\n\r\n----an-invalid-\r\n--boundary--droped-data";

        let body_bytes = body
            .iter()
            .map(|b| Ok(Bytes::from(slice::from_ref(b))))
            .collect::<Vec<Result<Bytes, StdError>>>();

        let body_stream = futures::stream::iter(body_bytes);

        let boundary = b"--an-invalid-\r\n--boundary--";

        let file_stream = FileStream::new(Box::pin(body_stream), boundary, Some(Bytes::copy_from_slice(b"\r")), None);

        {
            let file_bytes = aggregate_file_stream(file_stream).await.unwrap();
            assert_eq!(file_bytes, file_content);
        }
    }

    /// A yielded chunk crossing the declared length must surface a
    /// `LengthMismatch` error exactly once instead of delivering extra bytes.
    #[tokio::test]
    async fn file_stream_length_mismatch_over() {
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hello world\r\n--boundary--\r\n"))]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(5));

        {
            let err = file_stream.next().await.unwrap().unwrap_err();
            assert!(matches!(err, FileStreamError::LengthMismatch { remaining: 5 }));
        }
        assert!(file_stream.next().await.is_none());
    }

    /// A stream ending cleanly with fewer bytes than claimed must surface a
    /// `LengthMismatch` error instead of ending like a successful EOF.
    #[tokio::test]
    async fn file_stream_length_mismatch_under() {
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--\r\n"))]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(10));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        {
            let err = file_stream.next().await.unwrap().unwrap_err();
            assert!(matches!(err, FileStreamError::LengthMismatch { remaining: 8 }));
        }
        assert!(file_stream.next().await.is_none());
    }

    /// Delivering the canonical trailer one byte per poll forces the strict
    /// states to buffer across chunks instead of validating in a single step.
    #[tokio::test]
    async fn file_stream_strict_handles_split_chunks() {
        let data = b"hello\r\n--boundary--\r\n";
        let body = futures::stream::iter(
            data.iter()
                .map(|b| Ok::<Bytes, StdError>(Bytes::copy_from_slice(slice::from_ref(b)))),
        );

        let file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(5));

        let collected = aggregate_file_stream(file_stream).await.unwrap();
        assert_eq!(collected, "hello");
    }

    /// A body ending mid-trailer (closing delimiter without the final CRLF)
    /// must surface `Incomplete` rather than a clean end of stream.
    #[tokio::test]
    async fn file_stream_truncated_trailer_reports_incomplete() {
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--"))]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::Incomplete))));
    }

    /// An underlying transport error while buffering the closing `--` is
    /// surfaced as `Underlying`.
    #[tokio::test]
    async fn file_stream_underlying_error_in_state4_pull() {
        let body = futures::stream::iter(vec![
            Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary")),
            Err(io::Error::other("boom").into()),
        ]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    /// Same as above, but the failure happens while waiting for the final
    /// CRLF after the closing delimiter.
    #[tokio::test]
    async fn file_stream_underlying_error_in_state5_pull() {
        let body = futures::stream::iter(vec![
            Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--x")),
            Err(io::Error::other("boom").into()),
        ]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    #[test]
    fn file_stream_debug_contains_fields() {
        let body = futures::stream::iter(Vec::<Result<Bytes, StdError>>::new());
        let file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(7));

        let text = format!("{file_stream:?}");
        assert!(text.contains("content_len"), "{text}");
        assert!(text.contains("remaining"), "{text}");
    }

    /// A body ending right after the closing boundary pattern (no `--` at
    /// all) leaves state 4 waiting for bytes that never arrive.
    #[tokio::test]
    async fn file_stream_truncated_after_boundary_reports_incomplete() {
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary"))]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::Incomplete))));
    }

    /// A single trailing chunk larger than the boundary buffer trips the
    /// buffer-size guard while state 4 waits for the closing `--`.
    #[tokio::test]
    async fn file_stream_buffer_overflow_in_state4_pull() {
        let huge = Bytes::from(vec![b'q'; MAX_BOUNDARY_BUFFER_SIZE + 1]);
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundaryx")), Ok(huge)]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(
            file_stream.next().await,
            Some(Err(FileStreamError::BoundaryBufferTooLarge(_, _)))
        ));
    }

    /// Same as above, but the overflow happens while waiting for the final
    /// CRLF in state 5.
    #[tokio::test]
    async fn file_stream_buffer_overflow_in_state5_pull() {
        let huge = Bytes::from(vec![b'q'; MAX_BOUNDARY_BUFFER_SIZE + 1]);
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--x")), Ok(huge)]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(
            file_stream.next().await,
            Some(Err(FileStreamError::BoundaryBufferTooLarge(_, _)))
        ));
    }

    /// Trailing garbage that does not start with CRLF right after the closing
    /// `--` is rejected while buffering in state 5.
    #[tokio::test]
    async fn file_stream_rejects_garbage_in_state5_after_close() {
        let body = futures::stream::iter(vec![Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--zz\r\n"))]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// An underlying transport error while checking for an epilogue in
    /// state 6 is surfaced as `Underlying`.
    #[tokio::test]
    async fn file_stream_underlying_error_at_state6() {
        let body = futures::stream::iter(vec![
            Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--\r\n")),
            Err(io::Error::other("boom").into()),
        ]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    /// An epilogue delivered as a separate poll after the canonical close is
    /// rejected instead of being ignored.
    #[tokio::test]
    async fn file_stream_rejects_epilogue_in_separate_chunk() {
        let body = futures::stream::iter(vec![
            Ok::<Bytes, StdError>(Bytes::from_static(b"hi\r\n--boundary--\r\n")),
            Ok(Bytes::from_static(b"epilogue")),
        ]);

        let mut file_stream = FileStream::new(Box::pin(body), b"boundary", None, Some(2));

        {
            let bytes = file_stream.next().await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"hi");
        }
        assert!(matches!(file_stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// Without a derived length claim the byte stream reports an unknown
    /// remaining length.
    #[test]
    fn file_stream_remaining_length_unknown_without_claim() {
        let body = futures::stream::iter(Vec::<Result<Bytes, StdError>>::new());
        let file_stream = FileStream::new(Box::pin(body), b"boundary", None, None);

        assert!(file_stream.remaining_length().exact().is_none());
    }

    #[tokio::test]
    async fn multipart() {
        let fields = [
            ("key", "acl"),
            (
                "tagging",
                "<Tagging><TagSet><Tag><Key>Tag Name</Key><Value>Tag Value</Value></Tag></TagSet></Tagging>",
            ),
            ("success_action_redirect", "success_redirect"),
            ("Content-Type", "content_type"),
            ("x-amz-meta-uuid", "uuid"),
            ("x-amz-meta-tag", "metadata"),
            ("AWSAccessKeyId", "access-key-id"),
            ("Policy", "encoded_policy"),
            ("Signature", "signature="),
        ];

        let other_fields = [("submit", "Upload to Amazon S3")];

        let filename = "MyFilename.jpg";
        let content_type = "image/jpg";
        let boundary = "9431149156168";
        let file_content = "file_content";

        let body_bytes = {
            let mut ss = vec![format!("\r\n--{boundary}\r\n")];
            for &(n, v) in &fields {
                ss.push(format!(
                    concat!("Content-Disposition: form-data; name=\"{}\"\r\n", "\r\n", "{}\r\n", "--{}\r\n",),
                    n, v, boundary
                ));
            }
            ss.push(format!(
                concat!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                    "Content-Type: {}\r\n",
                    "\r\n",
                    "{}\r\n",
                    "--{}\r\n",
                ),
                "file", filename, content_type, file_content, boundary
            ));
            ss.push(format!(
                concat!("Content-Disposition: form-data; name=\"{}\"\r\n", "\r\n", "{}\r\n", "--{}--\r\n",),
                other_fields[0].0, other_fields[0].1, boundary
            ));

            ss.into_iter()
                .map(|s| Ok(Bytes::from(s.into_bytes())))
                .collect::<Vec<Result<Bytes, StdError>>>()
        };

        let body_stream = futures::stream::iter(body_bytes);

        let ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();

        for &(name, value) in &fields {
            let name = name.to_ascii_lowercase();
            assert_eq!(ans.find_field_value(&name).unwrap(), value);
        }

        assert_eq!(ans.file.name, filename);
        assert_eq!(ans.file.content_type.unwrap(), content_type);

        let file_bytes = aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap();

        {
            assert_eq!(file_bytes, file_content);
        }
    }

    #[tokio::test]
    async fn post_object() {
        let bytes: &[&[u8]] = &[
            b"--------------------------c634190ccaebbc34\r\nContent-Disposition: form-data; name=\"x-amz-sig",
            b"nature\"\r\n\r\na71d6dfaaa5aa018dc8e3945f2cec30ea1939ff7ed2f2dd65a6d49320c8fa1e6\r\n----------",
            b"----------------c634190ccaebbc34\r\nContent-Disposition: form-data; name=\"bucket\"\r\n\r\nmc-te",
            b"st-bucket-32569\r\n--------------------------c634190ccaebbc34\r\nContent-Disposition: form-data; na",
            b"me=\"policy\"\r\n\r\neyJleHBpcmF0aW9uIjoiMjAyMC0xMC0wM1QxMzoyNTo0Ny4yMThaIiwiY29uZGl0aW9ucyI6W1siZ",
            b"XEiLCIkYnVja2V0IiwibWMtdGVzdC1idWNrZXQtMzI1NjkiXSxbImVxIiwiJGtleSIsIm1jLXRlc3Qtb2JqZWN0LTc2NTgiXSxb",
            b"ImVxIiwiJHgtYW16LWRhdGUiLCIyMDIwMDkyNlQxMzI1NDdaIl0sWyJlcSIsIiR4LWFtei1hbGdvcml0aG0iLCJBV1M0LUhNQUMt",
            b"U0hBMjU2Il0sWyJlcSIsIiR4LWFtei1jcmVkZW50aWFsIiwiQUtJQUlPU0ZPRE5ON0VYQU1QTEUvMjAyMDA5MjYvdXMtZWFzdC0x",
            b"L3MzL2F3czRfcmVxdWVzdCJdXX0=\r\n--------------------------c634190ccaebbc34\r\nContent-Disposition: form-",
            b"data; name=\"x-amz-algorithm\"\r\n\r\nAWS4-HMAC-SHA256\r\n--------------------------c634190ccaebbc34\r",
            b"\nContent-Disposition: form-data; name=\"x-amz-credential\"\r\n\r\nAKIAIOSFODNN7EXAMPLE/20200926/us-east-1/",
            b"s3/aws4_request\r\n--------------------------c634190ccaebbc34\r\nContent-Disposition: form-data; nam",
            b"e=\"x-amz-date\"\r\n\r\n20200926T132547Z\r\n--------------------------c634190ccaebbc34\r\nContent-Dispos",
            b"ition: form-data; name=\"key\"\r\n\r\nmc-test-object-7658\r\n--------------------------c634190ccae",
            b"bbc34\r\nContent-Disposition: form-data; name=\"file\"; filename=\"datafile-1-MB\"\r\nContent-Type: app",
            b"lication/octet-stream\r\n\r\nNxjFYaL4HJsJsSy/d3V7F+s1DfU+AdMw9Ze0GbhIXYn9OCvtkz4/mRdf0/V2gdgc4vuXzWUlVHag",
            b"\npSI7q6mw4aXom0gunpMMUS0cEJgSoqB/yt4roLl2icdCnUPHhiO0SBh1VkBxSz5CwWlN/mmLfu5l\nAkD8fVoMTT/+kVSJzw7ykO48",
            b"7xLh6JOEfPaceUV30ASxGvkZkM0QEW5pWR1Lpwst6adXwxQiP2P8Pp0fpe\niA6bh6mXxH3BPeQhL9Ub44HdS2LlcUwpVjvcbvzGC31t",
            b"VIIABAshhx2VAcB1+QrvgCeT75IJGOWa\n3gNDHTPOEp/TBls2d7axY+zvCW9x4NBboKX25D1kBfAb90GaePbg/S5k5LvxJsr7vkCnU",
            b"4Iq85RV\n4uskvQ5CLZTtWQKJq6WDlZJWnVuA1qQqFVFWs/p02teDX/XOQpgW1I9trzHjOF8+AjI\r\n---------------------",
            b"-----c634190ccaebbc34--\r\n",
        ];

        let body_bytes: Vec<Result<Bytes, StdError>> = { bytes.iter().copied().map(Bytes::copy_from_slice).map(Ok).collect() };
        let body_stream = futures::stream::iter(body_bytes);
        let boundary = "------------------------c634190ccaebbc34";

        let ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();

        let fields = [
            ("x-amz-signature", "a71d6dfaaa5aa018dc8e3945f2cec30ea1939ff7ed2f2dd65a6d49320c8fa1e6"),
            ("bucket", "mc-test-bucket-32569"),
            (
                "policy",
                "eyJleHBpcmF0aW9uIjoiMjAyMC0xMC0wM1QxMzoyNTo0Ny4yMThaIiwiY29uZGl0aW9ucyI6W1siZXEiLCIkYnVja2V0IiwibWMtdGVzdC1idWNrZXQtMzI1NjkiXSxbImVxIiwiJGtleSIsIm1jLXRlc3Qtb2JqZWN0LTc2NTgiXSxbImVxIiwiJHgtYW16LWRhdGUiLCIyMDIwMDkyNlQxMzI1NDdaIl0sWyJlcSIsIiR4LWFtei1hbGdvcml0aG0iLCJBV1M0LUhNQUMtU0hBMjU2Il0sWyJlcSIsIiR4LWFtei1jcmVkZW50aWFsIiwiQUtJQUlPU0ZPRE5ON0VYQU1QTEUvMjAyMDA5MjYvdXMtZWFzdC0xL3MzL2F3czRfcmVxdWVzdCJdXX0=",
            ),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", "AKIAIOSFODNN7EXAMPLE/20200926/us-east-1/s3/aws4_request"),
            ("x-amz-date", "20200926T132547Z"),
            ("key", "mc-test-object-7658"),
        ];
        let file_name = "datafile-1-MB";
        let content_type = "application/octet-stream";

        for &(name, value) in &fields {
            let name = name.to_ascii_lowercase();
            assert_eq!(ans.find_field_value(&name).unwrap(), value);
        }

        assert_eq!(ans.file.name, file_name);
        assert_eq!(ans.file.content_type.unwrap(), content_type);

        let file_content = concat!(
            "NxjFYaL4HJsJsSy/d3V7F+s1DfU+AdMw9Ze0GbhIXYn9OCvtkz4/mRdf0/V2gdgc4vuXzWUlVHag",
            "\npSI7q6mw4aXom0gunpMMUS0cEJgSoqB/yt4roLl2icdCnUPHhiO0SBh1VkBxSz5CwWlN/mmLfu5l\nAkD8fVoMTT/+kVSJzw7ykO48",
            "7xLh6JOEfPaceUV30ASxGvkZkM0QEW5pWR1Lpwst6adXwxQiP2P8Pp0fpe\niA6bh6mXxH3BPeQhL9Ub44HdS2LlcUwpVjvcbvzGC31t",
            "VIIABAshhx2VAcB1+QrvgCeT75IJGOWa\n3gNDHTPOEp/TBls2d7axY+zvCW9x4NBboKX25D1kBfAb90GaePbg/S5k5LvxJsr7vkCnU",
            "4Iq85RV\n4uskvQ5CLZTtWQKJq6WDlZJWnVuA1qQqFVFWs/p02teDX/XOQpgW1I9trzHjOF8+AjI",
        );

        {
            let file_bytes = aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap();
            assert_eq!(file_bytes, file_content);
        }
    }

    /// With a known total length, the parser derives the exact file length so
    /// that the file can be forwarded as a stream without aggregation.
    #[tokio::test]
    async fn multipart_derives_file_len() {
        use std::fmt::Write as _;

        let boundary = "boundary123";
        let file_content = "file content";
        let fields = [("key", "test-key"), ("policy", "policy-data")];

        let mut body = String::new();
        for (name, value) in fields {
            let _ = write!(body, "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
        }
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{boundary}--\r\n"
        );

        let total_len = body.len() as u64;
        let body_stream = futures::stream::iter(vec![Ok::<_, StdError>(Bytes::from(body))]);
        let ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();

        let file_stream = ans.file.stream.unwrap();
        assert_eq!(file_stream.content_len(), Some(file_content.len() as u64));
        assert_eq!(file_stream.remaining_length().exact(), Some(file_content.len()));

        let file_bytes = aggregate_file_stream(file_stream).await.unwrap();
        assert_eq!(file_bytes, file_content);
    }

    /// When the file is not the last part, the derived length cannot be
    /// trusted; the stream must reject the request instead of silently
    /// dropping the trailing fields.
    #[tokio::test]
    async fn multipart_rejects_trailing_part_with_file_len() {
        let boundary = "boundary123";
        let file_content = "file content";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"late\"\r\n\r\nvalue\r\n--{boundary}--\r\n"
        );

        let total_len = body.len() as u64;
        let body_stream = futures::stream::iter(vec![Ok::<_, StdError>(Bytes::from(body))]);
        let mut ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();

        let mut file_stream = ans.take_file_stream().unwrap();

        let mut chunks = Vec::new();
        while let Some(chunk) = file_stream.next().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "content chunk followed by trailer error");
        assert_eq!(chunks[0].as_ref().unwrap(), file_content);
        assert!(matches!(chunks[1], Err(FileStreamError::InvalidTrailer)));
    }

    /// Extra bytes after the canonical closing delimiter (an epilogue) make
    /// the derived length incorrect; the stream must reject them.
    #[tokio::test]
    async fn multipart_rejects_epilogue_with_file_len() {
        let boundary = "boundary123";
        let file_content = "file content";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{boundary}--\r\nepilogue"
        );

        let total_len = body.len() as u64;
        let body_stream = futures::stream::iter(vec![Ok::<_, StdError>(Bytes::from(body))]);
        let mut ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();

        let mut file_stream = ans.take_file_stream().unwrap();

        let mut chunks = Vec::new();
        while let Some(chunk) = file_stream.next().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "content chunk followed by trailer error");
        assert_eq!(chunks[0].as_ref().unwrap(), file_content);
        assert!(matches!(chunks[1], Err(FileStreamError::InvalidTrailer)));
    }

    /// A missing final CRLF after the closing delimiter makes the content
    /// longer than the derived length; the built-in length check must reject
    /// it before any content is emitted.
    #[tokio::test]
    async fn multipart_rejects_missing_final_crlf_with_file_len() {
        let boundary = "boundary123";
        let file_content = "file content";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{boundary}--"
        );

        let total_len = body.len() as u64;
        let body_stream = futures::stream::iter(vec![Ok::<_, StdError>(Bytes::from(body))]);
        let mut ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();

        let mut file_stream = ans.take_file_stream().unwrap();

        let mut chunks = Vec::new();
        while let Some(chunk) = file_stream.next().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "length check rejects before emitting content");
        assert!(matches!(chunks[0], Err(FileStreamError::LengthMismatch { remaining: 10 })));
    }

    #[tokio::test]
    async fn multipart_field_with_filename() {
        let boundary = "boundary123";
        let file_content = "file content";
        let body_bytes = vec![
            format!("\r\n--{boundary}\r\n"),
            "Content-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\n".to_string(),
            "foo.txt\r\n".to_string(),
            format!("--{boundary}\r\n"),
            "Content-Disposition: form-data; name=\"policy\"\r\n\r\n".to_string(),
            "policy-data\r\n".to_string(),
            format!("--{boundary}\r\n"),
            "Content-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n".to_string(),
            "Content-Type: text/plain\r\n\r\n".to_string(),
            format!("{file_content}\r\n"),
            format!("--{boundary}--\r\n"),
        ];

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(|s| Ok::<_, StdError>(Bytes::from(s))));

        let ans = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();

        assert_eq!(ans.find_field_value("key"), Some("foo.txt"));
        assert_eq!(ans.find_field_value("policy"), Some("policy-data"));
        assert_eq!(ans.file.name, "file.txt");

        let file_bytes = aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap();
        assert_eq!(file_bytes, file_content);
    }

    #[tokio::test]
    async fn test_field_too_large() {
        let boundary = "boundary123";
        let limits = MultipartLimits::default();

        // Create a field value that exceeds max_field_size (1 MB)
        let field_size = limits.max_field_size + 1000; // Just over 1 MB
        let large_value = "x".repeat(field_size);

        let body_bytes = vec![
            Bytes::from(format!("--{boundary}\r\n")),
            Bytes::from("Content-Disposition: form-data; name=\"large_field\"\r\n\r\n"),
            Bytes::from(large_value),
            Bytes::from(format!("\r\n--{boundary}--\r\n")),
        ];

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(Ok::<_, StdError>));

        let result = transform_multipart(body_stream, boundary.as_bytes(), limits, None).await;
        // Either error is acceptable - both indicate the field/buffer is too large
        assert!(result.is_err(), "Should fail when field exceeds limits");
    }

    #[tokio::test]
    async fn test_total_size_too_large() {
        let boundary = "boundary123";
        let limits = MultipartLimits::default();

        // Create multiple fields that together exceed max_fields_size (20 MB)
        let field_size = limits.max_field_size; // 1 MB per field
        let num_fields = 21; // 21 fields = 21 MB > 20 MB limit

        let mut body_bytes = Vec::new();

        for i in 0..num_fields {
            body_bytes.push(format!("--{boundary}\r\n"));
            body_bytes.push(format!("Content-Disposition: form-data; name=\"field{i}\"\r\n\r\n"));
            body_bytes.push("x".repeat(field_size));
            body_bytes.push("\r\n".to_string());
        }
        body_bytes.push(format!("--{boundary}--\r\n"));

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(|s| Ok::<_, StdError>(Bytes::from(s))));

        let result = transform_multipart(body_stream, boundary.as_bytes(), limits, None).await;
        match result {
            Err(MultipartError::TotalSizeTooLarge(size, limit)) => {
                assert_eq!(limit, limits.max_fields_size);
                assert!(size > limits.max_fields_size);
            }
            _ => panic!("Expected TotalSizeTooLarge error"),
        }
    }

    #[tokio::test]
    async fn test_too_many_parts() {
        let boundary = "boundary123";
        let limits = MultipartLimits::default();

        // Create more parts than max_parts (1000)
        let num_parts = limits.max_parts + 1;

        let mut body_bytes = Vec::new();

        for i in 0..num_parts {
            body_bytes.push(format!("--{boundary}\r\n"));
            body_bytes.push(format!("Content-Disposition: form-data; name=\"field{i}\"\r\n\r\n"));
            body_bytes.push("value".to_string());
            body_bytes.push("\r\n".to_string());
        }
        body_bytes.push(format!("--{boundary}--\r\n"));

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(|s| Ok::<_, StdError>(Bytes::from(s))));

        let result = transform_multipart(body_stream, boundary.as_bytes(), limits, None).await;
        match result {
            Err(MultipartError::TooManyParts(count, limit)) => {
                assert_eq!(limit, limits.max_parts);
                assert!(count > limits.max_parts);
            }
            _ => panic!("Expected TooManyParts error"),
        }
    }

    #[tokio::test]
    async fn test_limits_within_bounds() {
        let boundary = "boundary123";

        // Create fields within limits
        let field_count = 10;
        let field_size = 100; // Small fields

        let mut body_bytes = Vec::new();

        for i in 0..field_count {
            body_bytes.push(format!("--{boundary}\r\n"));
            body_bytes.push(format!("Content-Disposition: form-data; name=\"field{i}\"\r\n\r\n"));
            body_bytes.push("x".repeat(field_size));
            body_bytes.push("\r\n".to_string());
        }

        // Add a file
        body_bytes.push(format!("--{boundary}\r\n"));
        body_bytes.push("Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n".to_string());
        body_bytes.push("Content-Type: text/plain\r\n\r\n".to_string());
        body_bytes.push("file content".to_string());
        body_bytes.push(format!("\r\n--{boundary}--\r\n"));

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(|s| Ok::<_, StdError>(Bytes::from(s))));

        let result = transform_multipart(body_stream, boundary.as_bytes(), MultipartLimits::default(), None).await;
        assert!(result.is_ok(), "Should succeed when within limits");

        let multipart = result.unwrap();
        assert_eq!(multipart.fields().len(), field_count);
        assert!(multipart.file.stream.is_some());
    }

    #[tokio::test]
    async fn test_boundary_buffer_too_large() {
        // Create a scenario where the boundary pattern spans many chunks, causing
        // the buffer in state 3 to accumulate more than MAX_BOUNDARY_BUFFER_SIZE
        let boundary = b"boundary123";

        // Create file content that will trigger state 3 (boundary matching across chunks)
        // by having a partial boundary pattern that keeps accumulating
        let mut file_content = Vec::new();

        // Add some normal content first
        file_content.extend_from_slice(b"normal content here\r");

        // Create a long sequence that starts like the boundary pattern "\r\n--boundary123"
        // but never completes, forcing the buffer to keep accumulating in state 3
        // The pattern is "\r\n--" which matches the start of the boundary pattern
        file_content.extend_from_slice(b"\n-"); // This will trigger state 3

        // Now send many chunks that continue to look like they might be the boundary
        // but never complete it, causing the buffer to grow beyond MAX_BOUNDARY_BUFFER_SIZE
        let large_chunk = vec![b'-'; MAX_BOUNDARY_BUFFER_SIZE + 1000];

        let body_bytes = vec![
            Bytes::from(b"--boundary123\r\n".to_vec()),
            Bytes::from(b"Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\n".to_vec()),
            Bytes::from(file_content),
            // Send the large chunk that will exceed the buffer limit
            Bytes::from(large_chunk),
        ];

        let body_stream = futures::stream::iter(body_bytes.into_iter().map(Ok::<_, StdError>));

        let result = transform_multipart(body_stream, boundary, MultipartLimits::default(), None).await;

        // The multipart parsing will succeed, but when we try to read the file stream,
        // it should error with BoundaryBufferTooLarge
        assert!(result.is_ok(), "Multipart parsing should succeed");

        let mut multipart = result.unwrap();
        let mut file_stream = multipart.take_file_stream().expect("File stream should exist");

        // Try to read from the file stream, which should trigger the boundary buffer error
        let mut errored = false;
        while let Some(chunk_result) = file_stream.next().await {
            if let Err(FileStreamError::BoundaryBufferTooLarge(size, limit)) = chunk_result {
                assert_eq!(limit, MAX_BOUNDARY_BUFFER_SIZE);
                assert!(size > MAX_BOUNDARY_BUFFER_SIZE);
                errored = true;
                break;
            }
        }

        assert!(errored, "Should have received BoundaryBufferTooLarge error");
    }

    #[test]
    fn multipart_underlying_error_exposes_source() {
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "boom");
        let err = MultipartError::Underlying(Box::new(cause));

        assert_eq!(err.to_string(), "MultipartError: Underlying: boom");

        let err: &dyn std::error::Error = &err;
        let source = err.source().expect("the underlying error should be exposed as the source");
        let kind = source.downcast_ref::<io::Error>().map(io::Error::kind);
        assert_eq!(kind, Some(io::ErrorKind::ConnectionReset));
    }

    #[test]
    fn file_stream_underlying_error_exposes_source() {
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "boom");
        let err = FileStreamError::Underlying(Box::new(cause));

        assert_eq!(err.to_string(), "FileStreamError: Underlying: boom");

        let err: &dyn std::error::Error = &err;
        let source = err.source().expect("the underlying error should be exposed as the source");
        let kind = source.downcast_ref::<io::Error>().map(io::Error::kind);
        assert_eq!(kind, Some(io::ErrorKind::ConnectionReset));
    }
}

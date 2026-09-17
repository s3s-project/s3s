// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
//! multipart/form-data encoding for POST Object
//!
//! Parsing is delegated to [`s3s_multipart`]; this module keeps the S3 POST
//! Object contract on top of it:
//!
//! - form fields are aggregated into owned `(name, value)` pairs, with names
//!   lowercased and sorted so that [`Multipart::find_field_value`] can binary
//!   search them;
//! - the `file` part is handed over as a [`FileStream`]; when the request
//!   carries a `Content-Length`, the stream derives the exact file length from
//!   it, so the consumer can forward the file without buffering;
//! - the `file` part must be the last one: the closing trailer is validated
//!   while the stream is consumed, so an epilogue or another part is rejected;
//! - the form limits ([`MultipartLimits`]) are enforced here, on top of the
//!   parser's own part-header block limit.
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>

use crate::error::StdError;
use crate::stream::ByteStream;

use std::fmt::{self, Debug};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use hyper::body::Bytes;
use s3s_multipart::Boundary;
use s3s_multipart::Error as ParserError;
use s3s_multipart::FinalPartDataStream;
use s3s_multipart::Multipart as Parser;
use s3s_multipart::parse_content_disposition;

/// The boxed stream handed to the multipart parser.
///
/// [`s3s_multipart::Multipart`] fixes the item error type and requires the
/// stream to be `Unpin`; boxing satisfies both and keeps [`FileStream`]
/// non-generic.
type ParserStream = Pin<Box<dyn Stream<Item = Result<Bytes, ParserError>> + Send + Sync + 'static>>;

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
            max_field_size: 1024 * 1024,       // 1 MiB
            max_fields_size: 20 * 1024 * 1024, // 20 MiB
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
        let idx = Self::find_field_index(&self.fields, name)?;
        Some(self.fields.get(idx)?.1.as_str())
    }

    fn find_field_value_mut<'a>(fields: &'a mut [(String, String)], name: &str) -> Option<&'a mut String> {
        let idx = Self::find_field_index(fields, name)?;
        Some(&mut fields.get_mut(idx)?.1)
    }

    fn find_field_index(fields: &[(String, String)], name: &str) -> Option<usize> {
        let upper_bound = fields.partition_point(|x| x.0.as_str() <= name);
        let idx = upper_bound.checked_sub(1)?;
        let pair = fields.get(idx)?;
        if pair.0.as_str() != name {
            return None;
        }
        Some(idx)
    }

    /// Substitutes the `${filename}` variable in the `key` field with the
    /// filename supplied by the file part, per the AWS POST upload contract:
    /// <https://docs.aws.amazon.com/AmazonS3/latest/dev/sigv4-HTTPPOSTConstructPolicy.html>
    ///
    /// Amazon S3 replaces the variable verbatim and defines no escaping
    /// mechanism, so a key containing a literal `${filename}` cannot be
    /// uploaded through POST — this matches AWS behavior. Callers must invoke
    /// this before evaluating POST policy conditions so that `eq`/`starts-with`
    /// constraints on `$key` apply to the final substituted key.
    pub(crate) fn substitute_key_filename(&mut self) {
        const FILENAME_VARIABLE: &str = "${filename}";
        let file_name = &self.file.name;
        let Some(key) = Self::find_field_value_mut(&mut self.fields, "key") else {
            return;
        };
        if key.contains(FILENAME_VARIABLE) {
            *key = key.replace(FILENAME_VARIABLE, file_name);
        }
    }

    /// Create a Multipart for testing purposes
    ///
    /// This mirrors the normalization performed by `transform_multipart` by:
    /// - lowercasing field names
    /// - sorting fields by name
    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unreachable,
        clippy::unwrap_used
    )]
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
/// canonical closing trailer. See [`FileStream::content_len`].
///
/// # Errors
/// Returns an `Err` if the format is invalid
pub async fn transform_multipart<S>(
    body_stream: S,
    boundary: &[u8],
    limits: MultipartLimits,
    total_len: Option<u64>,
) -> Result<Multipart, MultipartError>
where
    S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
{
    let parser_boundary = Boundary::new(boundary).map_err(|_| MultipartError::InvalidFormat)?;
    // The crate fixes the stream item's error type, so the transport error is
    // wrapped here instead of surfacing as a crate-generic error.
    let stream: ParserStream = Box::pin(body_stream.map(|item| item.map_err(ParserError::stream_read_failed)));
    let mut parser = Parser::new(stream, &parser_boundary, limits.max_field_size);

    let mut fields: Vec<(String, String)> = Vec::new();
    let mut total_fields_size: usize = 0;
    let mut parts_count: usize = 0;

    loop {
        let Some(mut part) = parser.next_part().await.map_err(map_parser_error)? else {
            // The closing delimiter was reached without a file part.
            return Err(MultipartError::InvalidFormat);
        };

        parts_count = parts_count.saturating_add(1);
        if parts_count > limits.max_parts {
            return Err(MultipartError::TooManyParts(parts_count, limits.max_parts));
        }

        // The headers borrow the parser's buffer, so the part's metadata is
        // converted to owned values here; the borrow cannot be carried across
        // loop iterations. The last header of each kind wins, matching the
        // previous parser.
        let mut cd_name = None;
        let mut cd_file_name = None;
        let mut content_type = None;

        while let Some(header) = part.next_header().await.map_err(map_parser_error)? {
            if header.name.eq_ignore_ascii_case("content-disposition") {
                let cd = parse_content_disposition(header.value);
                cd_name = cd.and_then(|cd| cd.name).map(bytes_to_string).transpose()?;
                cd_file_name = cd.and_then(|cd| cd.file_name).map(bytes_to_string).transpose()?;
            } else if header.name.eq_ignore_ascii_case("content-type") {
                content_type = Some(bytes_to_string(header.value)?);
            }
        }

        let Some(name) = cd_name else {
            // A part without a usable `Content-Disposition` name cannot be
            // represented as a form field, so the whole form is malformed.
            return Err(MultipartError::InvalidFormat);
        };

        if name.eq_ignore_ascii_case("file") {
            let owned = part.take_data_stream().map_err(map_parser_error)?.into_final();
            let content_len = total_len.and_then(|total| {
                total
                    .checked_sub(owned.multipart_consumed())?
                    .checked_sub(u64::try_from(boundary.len()).ok()?.checked_add(8)?)
            });

            for (field_name, _) in &mut fields {
                field_name.make_ascii_lowercase();
            }
            fields.sort_by(|a, b| a.0.cmp(&b.0));

            return Ok(Multipart {
                fields,
                file: File {
                    name: cd_file_name.unwrap_or(name),
                    content_type,
                    stream: Some(FileStream::from_owned(owned, content_len)),
                },
            });
        }

        let mut value: Vec<u8> = Vec::new();
        while let Some(chunk) = part.next_data().await.map_err(map_parser_error)? {
            value.extend_from_slice(&chunk);
            if value.len() > limits.max_field_size {
                return Err(MultipartError::FieldTooLarge(value.len(), limits.max_field_size));
            }
            total_fields_size = total_fields_size.saturating_add(chunk.len());
            if total_fields_size > limits.max_fields_size {
                return Err(MultipartError::TotalSizeTooLarge(total_fields_size, limits.max_fields_size));
            }
        }

        fields.push((name, bytes_to_string(&value)?));
    }
}

/// Converts a parsed name, filename or header value to a `String`.
///
/// The previous parser converted these values with `str`, so a non-UTF-8
/// name, filename or `Content-Type` is still a format error rather than being
/// silently replaced.
fn bytes_to_string(bytes: &[u8]) -> Result<String, MultipartError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| MultipartError::InvalidFormat)
}

fn map_parser_error(err: ParserError) -> MultipartError {
    match err {
        ParserError::StreamReadFailed(err) => MultipartError::Underlying(err),
        ParserError::HeaderSizeExceeded { limit } => MultipartError::FieldTooLarge(limit, limit),
        // `Error` is #[non_exhaustive]; every other variant describes a
        // malformed or prematurely ended body, or a misuse the sequential
        // adapter cannot reach, and keeps the "invalid format" mapping.
        _ => MultipartError::InvalidFormat,
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
    /// Bytes after the file do not match the canonical closing delimiter
    /// (e.g., the file is not the last part or an epilogue exists)
    #[error("FileStreamError: InvalidTrailer")]
    InvalidTrailer,
    /// The yielded byte count disagrees with the declared content length
    #[error("FileStreamError: LengthMismatch: {remaining} bytes remaining")]
    LengthMismatch { remaining: u64 },
}

/// File stream
pub struct FileStream {
    /// Inner stream.
    ///
    /// Boxed: the parser state is large (it owns the byte buffer and the
    /// delimiter finder), and the stream is moved through the dispatch
    /// futures, so keeping it on the heap bounds their size (see the
    /// future-size budget test).
    inner: Box<FinalPartDataStream<ParserStream>>,
    /// exact content length derived from the request's `Content-Length`, if known
    content_len: Option<u64>,
    /// remaining content bytes; counts down as bytes are yielded
    remaining: u64,
    /// set once a terminal error has been reported
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
    /// Constructs a `FileStream` from the parser's taken stream.
    fn from_owned(inner: FinalPartDataStream<ParserStream>, content_len: Option<u64>) -> Self {
        Self {
            inner: Box::new(inner),
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

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut *this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Enforce the declared length: never deliver content beyond
                // the claim. The trailer validation in the inner stream
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
            Poll::Ready(None) if this.content_len.is_some() && this.remaining > 0 => {
                this.ended = true;
                Poll::Ready(Some(Err(FileStreamError::LengthMismatch {
                    remaining: this.remaining,
                })))
            }
            // An error surfaced by the inner stream is terminal for this
            // stream: mark it ended so the terminal length check does not emit
            // a second, redundant error.
            Poll::Ready(Some(Err(err))) => {
                this.ended = true;
                Poll::Ready(Some(Err(map_file_stream_error(err))))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
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

fn map_file_stream_error(err: ParserError) -> FileStreamError {
    match err {
        ParserError::StreamReadFailed(err) => FileStreamError::Underlying(err),
        ParserError::IncompleteStreamPart => FileStreamError::Incomplete,
        // `StreamPartNotLast` covers an epilogue or another part after the
        // closing delimiter; every other variant means the closing trailer
        // could not be validated.
        _ => FileStreamError::InvalidTrailer,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    use std::fmt::Write as _;
    use std::io;

    const BOUNDARY: &str = "boundary123";

    fn chunks(items: Vec<Bytes>) -> impl Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static {
        futures::stream::iter(items.into_iter().map(Ok::<_, StdError>))
    }

    fn body_stream(body: impl Into<Vec<u8>>) -> impl Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static {
        chunks(vec![Bytes::from(body.into())])
    }

    fn byte_chunks(body: &str) -> Vec<Bytes> {
        body.as_bytes().iter().map(|byte| Bytes::copy_from_slice(&[*byte])).collect()
    }

    /// A canonical one-file form with the given field parts first.
    fn file_form(fields: &[(&str, &str)], file_content: &str) -> String {
        let mut body = String::new();
        for (name, value) in fields {
            let _ = write!(body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
        }
        let _ = write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{BOUNDARY}--\r\n"
        );
        body
    }

    async fn parse_file_stream(body: &str, total_len: Option<u64>) -> FileStream {
        let mut multipart =
            transform_multipart(body_stream(body.to_owned()), BOUNDARY.as_bytes(), MultipartLimits::default(), total_len)
                .await
                .unwrap();
        multipart.take_file_stream().unwrap()
    }

    async fn aggregate_file_stream(mut file_stream: FileStream) -> Result<Bytes, FileStreamError> {
        let mut buf = Vec::new();
        while let Some(bytes) = file_stream.next().await {
            buf.extend(bytes?);
        }
        Ok(buf.into())
    }

    fn test_file(name: &str) -> File {
        File {
            name: name.to_owned(),
            content_type: None,
            stream: None,
        }
    }

    #[test]
    fn substitute_key_filename_replaces_variable_before_policy_checks() {
        let mut m = Multipart::new_for_test(
            vec![
                ("key".to_owned(), "user/betty/${filename}".to_owned()),
                ("policy".to_owned(), "policy-data".to_owned()),
            ],
            test_file("photo1.jpg"),
        );
        m.substitute_key_filename();
        assert_eq!(m.find_field_value("key"), Some("user/betty/photo1.jpg"));

        let mut m =
            Multipart::new_for_test(vec![("key".to_owned(), "${filename}/copies/${filename}".to_owned())], test_file("a.txt"));
        m.substitute_key_filename();
        assert_eq!(m.find_field_value("key"), Some("a.txt/copies/a.txt"));
    }

    #[test]
    fn substitute_key_filename_leaves_plain_keys_and_other_fields_alone() {
        let mut m = Multipart::new_for_test(
            vec![
                ("key".to_owned(), "plain-key.txt".to_owned()),
                ("success_action_redirect".to_owned(), "https://example.com/${filename}".to_owned()),
            ],
            test_file("photo1.jpg"),
        );
        m.substitute_key_filename();
        assert_eq!(m.find_field_value("key"), Some("plain-key.txt"));
        assert_eq!(m.find_field_value("success_action_redirect"), Some("https://example.com/${filename}"));

        let mut m = Multipart::new_for_test(vec![("policy".to_owned(), "policy-data".to_owned())], test_file("photo1.jpg"));
        m.substitute_key_filename();
        assert_eq!(m.find_field_value("key"), None);
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
        let filename = "MyFilename.jpg";
        let content_type = "image/jpg";
        let file_content = "file_content";

        // The leading CRLF is part of the preamble the parser accepts.
        let mut body = String::from("\r\n");
        for (name, value) in fields {
            let _ = write!(body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
        }
        let _ = write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n{file_content}\r\n--{BOUNDARY}--\r\n"
        );

        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        for &(name, value) in &fields {
            assert_eq!(ans.find_field_value(&name.to_ascii_lowercase()).unwrap(), value);
        }
        assert_eq!(ans.file.name, filename);
        assert_eq!(ans.file.content_type.clone().unwrap(), content_type);
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), file_content);
    }

    #[tokio::test]
    async fn post_object() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"x-amz-signature\"\r\n\r\nsig\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"bucket\"\r\n\r\nbucket\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"policy\"\r\n\r\npolicy\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nkey\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"datafile\"\r\nContent-Type: application/octet-stream\r\n\r\nfile-data\r\n--{BOUNDARY}--\r\n"
        );
        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.find_field_value("key"), Some("key"));
        assert_eq!(ans.find_field_value("policy"), Some("policy"));
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), "file-data");
    }

    #[tokio::test]
    async fn multipart_derives_file_len() {
        let file_content = "file content";
        let body = file_form(&[], file_content);
        let total_len = body.len() as u64;

        let mut ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();
        let file_stream = ans.take_file_stream().unwrap();
        assert_eq!(file_stream.content_len(), Some(file_content.len() as u64));
        assert_eq!(aggregate_file_stream(file_stream).await.unwrap(), file_content);
    }

    #[tokio::test]
    async fn multipart_rejects_trailing_part_with_file_len() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nfile content\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"late\"\r\n\r\nvalue\r\n--{BOUNDARY}--\r\n"
        );
        let total_len = body.len() as u64;
        let mut ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();
        let mut stream = ans.take_file_stream().unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"file content");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    #[tokio::test]
    async fn multipart_rejects_epilogue_with_file_len() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nfile content\r\n--{BOUNDARY}--\r\nepilogue"
        );
        let total_len = body.len() as u64;
        let mut ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), Some(total_len))
            .await
            .unwrap();
        let mut stream = ans.take_file_stream().unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"file content");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// A missing final CRLF after the closing delimiter makes the content
    /// longer than the derived length; the built-in length check must reject
    /// it before any content is emitted.
    #[tokio::test]
    async fn multipart_rejects_missing_final_crlf_with_file_len() {
        let file_content = "file content";
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{file_content}\r\n--{BOUNDARY}--"
        );
        let total_len = body.len() as u64;

        let mut ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), Some(total_len))
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

    /// A part with a filename is still a form field, not the file part.
    #[tokio::test]
    async fn multipart_field_with_filename() {
        let file_content = "file content";
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\nfoo.txt\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n\r\n{file_content}\r\n--{BOUNDARY}--\r\n"
        );
        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.find_field_value("key"), Some("foo.txt"));
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), file_content);
    }

    #[tokio::test]
    async fn test_field_too_large() {
        let limits = MultipartLimits::default();
        let large_value = "x".repeat(limits.max_field_size + 1000);
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"large_field\"\r\n\r\n{large_value}\r\n--{BOUNDARY}--\r\n"
        );
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), limits, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_total_size_too_large() {
        let limits = MultipartLimits::default();
        let field_size = limits.max_field_size;
        let num_fields = 21;
        let mut body = String::new();
        for i in 0..num_fields {
            let _ = write!(body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"field{i}\"\r\n\r\n");
            body.push_str(&"x".repeat(field_size));
            body.push_str("\r\n");
        }
        let _ = write!(body, "--{BOUNDARY}--\r\n");
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), limits, None).await;
        assert!(matches!(result, Err(MultipartError::TotalSizeTooLarge(_, _))), "{result:?}");
    }

    #[tokio::test]
    async fn test_too_many_parts() {
        let limits = MultipartLimits {
            max_field_size: 1024 * 1024,
            max_fields_size: 20 * 1024 * 1024,
            max_parts: 4,
        };
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\n2\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"c\"\r\n\r\n3\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"d\"\r\n\r\n4\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"e\"\r\n\r\n5\r\n--{BOUNDARY}--\r\n"
        );
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), limits, None).await;
        assert!(matches!(result, Err(MultipartError::TooManyParts(5, 4))));
    }

    #[tokio::test]
    async fn form_without_a_file_part_is_invalid() {
        let body = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk\r\n--{BOUNDARY}--\r\n");
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None).await;
        assert!(matches!(result, Err(MultipartError::InvalidFormat)), "{result:?}");
    }

    /// Parts carrying more headers than the parser reads are accepted: the
    /// surplus headers are ignored instead of rejecting the whole form.
    #[tokio::test]
    async fn part_with_extra_headers_is_accepted() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"\r\nX-Extra: 1\r\nContent-Type: text/plain\r\n\r\nk\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\nX-Extra: 2\r\n\r\nfile content\r\n--{BOUNDARY}--\r\n"
        );
        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.find_field_value("key"), Some("k"));
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), "file content");
    }

    /// The filename may precede the name, parameter names are case-insensitive
    /// and unknown parameters are ignored.
    #[tokio::test]
    async fn content_disposition_is_parsed_tolerantly() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; filename=\"f.txt\"; NAME=file; size=12\r\n\r\nfile content\r\n--{BOUNDARY}--\r\n"
        );
        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.file.name, "f.txt");
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), "file content");
    }

    /// RFC 2046 allows a preamble before the first boundary.
    #[tokio::test]
    async fn preamble_before_the_first_boundary_is_accepted() {
        let body = format!(
            "this is a preamble\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nfile content\r\n--{BOUNDARY}--\r\n"
        );
        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.find_field_value("key"), Some("k"));
        assert_eq!(aggregate_file_stream(ans.file.stream.unwrap()).await.unwrap(), "file content");
    }

    /// Without a derived length the strict closing trailer is still validated:
    /// an epilogue after the closing delimiter is rejected instead of being
    /// silently dropped.
    #[tokio::test]
    async fn chunked_form_rejects_content_after_the_file() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nfile content\r\n--{BOUNDARY}--\r\nepilogue"
        );
        let mut ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        let mut stream = ans.take_file_stream().unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"file content");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// A part without a usable Content-Disposition name is rejected right
    /// away, without buffering the rest of the form.
    #[tokio::test]
    async fn part_without_content_disposition_is_rejected() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Type: text/plain\r\n\r\nnameless\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nfile content\r\n--{BOUNDARY}--\r\n"
        );
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None).await;
        assert!(matches!(result, Err(MultipartError::InvalidFormat)), "{result:?}");
    }

    /// The boundary is validated when the parser is constructed (RFC 2046
    /// allows at most 70 characters).
    #[tokio::test]
    async fn boundary_longer_than_allowed_is_rejected() {
        let boundary = "a".repeat(71);
        let body = format!("--{boundary}\r\n\r\nx\r\n--{boundary}--\r\n");
        let result = transform_multipart(body_stream(body), boundary.as_bytes(), MultipartLimits::default(), None).await;
        assert!(matches!(result, Err(MultipartError::InvalidFormat)), "{result:?}");
    }

    /// The part-header block, not the arriving chunk, is bounded by the field
    /// limit; the parser reports the limit it exceeded.
    #[tokio::test]
    async fn part_header_block_over_the_buffer_limit_is_rejected() {
        let limits = MultipartLimits {
            max_field_size: 64,
            max_fields_size: 1024,
            max_parts: 8,
        };
        let long_name = "a".repeat(200);
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{long_name}\"\r\n\r\nx\r\n--{BOUNDARY}--\r\n"
        );
        let result = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), limits, None).await;
        assert!(matches!(result, Err(MultipartError::FieldTooLarge(64, 64))), "{result:?}");
    }

    /// A transport error before the file part is reported as Underlying.
    #[tokio::test]
    async fn stream_error_before_the_file_is_underlying() {
        let items: Vec<Result<Bytes, StdError>> = vec![
            Ok(Bytes::from(format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk"
            ))),
            Err(Box::new(io::Error::new(io::ErrorKind::ConnectionReset, "boom"))),
        ];
        let result =
            transform_multipart(futures::stream::iter(items), BOUNDARY.as_bytes(), MultipartLimits::default(), None).await;
        let MultipartError::Underlying(source) = result.unwrap_err() else {
            panic!("expected an underlying error");
        };
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::ConnectionReset)
        );
    }

    #[tokio::test]
    async fn file_stream_length_mismatch_over() {
        let body = file_form(&[], "hello world");
        // Shrinking the claim by six bytes makes the first data chunk exceed it.
        let total_len = body.len() as u64 - 6;
        let mut stream = parse_file_stream(&body, Some(total_len)).await;
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(err, FileStreamError::LengthMismatch { remaining: 5 }));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn file_stream_length_mismatch_under() {
        let body = file_form(&[], "hi");
        // Growing the claim by eight bytes leaves it short by that much.
        let total_len = body.len() as u64 + 8;
        let mut stream = parse_file_stream(&body, Some(total_len)).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(err, FileStreamError::LengthMismatch { remaining: 8 }));
    }

    /// Delivering the canonical trailer one byte per poll forces the closing
    /// validation to work across chunks.
    #[tokio::test]
    async fn file_stream_strict_handles_split_chunks() {
        let body = file_form(&[], "hello");
        let total_len = body.len() as u64;
        let mut multipart = transform_multipart(
            chunks(byte_chunks(&body)),
            BOUNDARY.as_bytes(),
            MultipartLimits::default(),
            Some(total_len),
        )
        .await
        .unwrap();
        let stream = multipart.take_file_stream().unwrap();
        assert_eq!(stream.content_len(), Some(5));
        assert_eq!(aggregate_file_stream(stream).await.unwrap(), "hello");
    }

    /// File content made of CRLF runs and near-miss dash patterns is streamed
    /// back byte for byte, even when every chunk carries a single byte.
    #[tokio::test]
    async fn file_content_with_crlf_runs_is_streamed_exactly() {
        let file_content = "\r\n too much crlf \r\n--\r\n\r\n\r\n";
        let body = file_form(&[], file_content);
        let total_len = body.len() as u64;
        let mut multipart = transform_multipart(
            chunks(byte_chunks(&body)),
            BOUNDARY.as_bytes(),
            MultipartLimits::default(),
            Some(total_len),
        )
        .await
        .unwrap();
        let stream = multipart.take_file_stream().unwrap();
        assert_eq!(aggregate_file_stream(stream).await.unwrap(), file_content);
    }

    /// A body ending mid-trailer (closing delimiter without the final CRLF)
    /// must surface Incomplete rather than a clean end of stream.
    #[tokio::test]
    async fn file_stream_truncated_trailer_reports_incomplete() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nhi\r\n--{BOUNDARY}--"
        );
        let mut stream = parse_file_stream(&body, None).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::Incomplete))));
    }

    /// A body ending right after the boundary pattern (no closing dashes) is
    /// incomplete too.
    #[tokio::test]
    async fn file_stream_truncated_after_boundary_reports_incomplete() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nhi\r\n--{BOUNDARY}"
        );
        let mut stream = parse_file_stream(&body, None).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::Incomplete))));
    }

    /// An epilogue delivered after the canonical close is rejected instead of
    /// being ignored.
    #[tokio::test]
    async fn file_stream_rejects_epilogue_in_separate_chunk() {
        let items = vec![
            Bytes::from(format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nhi\r\n--{BOUNDARY}--\r\n"
            )),
            Bytes::from_static(b"epilogue"),
        ];
        let mut multipart = transform_multipart(chunks(items), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        let mut stream = multipart.take_file_stream().unwrap();
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// A form whose fields and file part are all within the limits parses, and
    /// every part is reported.
    #[tokio::test]
    async fn limits_within_bounds() {
        let field_count = 10;
        let field_value = "x".repeat(100);
        let mut body = String::new();
        for i in 0..field_count {
            let _ = write!(
                body,
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"field{i}\"\r\n\r\n{field_value}\r\n"
            );
        }
        let _ = write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\nfile content\r\n--{BOUNDARY}--\r\n"
        );

        let ans = transform_multipart(body_stream(body), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
            .await
            .unwrap();
        assert_eq!(ans.fields().len(), field_count);
        assert!(ans.file.stream.is_some());
    }

    /// A file part holding "hi" followed by the given tail, with the stream
    /// failing after the first chunk.
    async fn file_stream_then_error(tail: &str) -> FileStream {
        let items: Vec<Result<Bytes, StdError>> = vec![
            Ok(Bytes::from(format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nhi{tail}"
            ))),
            Err(Box::new(io::Error::new(io::ErrorKind::ConnectionReset, "boom"))),
        ];
        let mut multipart =
            transform_multipart(futures::stream::iter(items), BOUNDARY.as_bytes(), MultipartLimits::default(), None)
                .await
                .unwrap();
        multipart.take_file_stream().unwrap()
    }

    /// A transport error while the closing dashes are read surfaces as an
    /// underlying error.
    #[tokio::test]
    async fn file_stream_underlying_error_while_reading_the_closing_dashes() {
        let mut stream = file_stream_then_error(&format!("\r\n--{BOUNDARY}")).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    /// The same while the final CRLF after the closing dashes is read.
    #[tokio::test]
    async fn file_stream_underlying_error_while_reading_the_final_crlf() {
        let mut stream = file_stream_then_error(&format!("\r\n--{BOUNDARY}--")).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    /// The same while the end of the multipart stream is checked after a
    /// complete closing trailer.
    #[tokio::test]
    async fn file_stream_underlying_error_while_checking_the_epilogue() {
        let mut stream = file_stream_then_error(&format!("\r\n--{BOUNDARY}--\r\n")).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::Underlying(_)))));
    }

    /// Bytes between the closing dashes and the final CRLF are a trailer
    /// mismatch, not a transport error.
    #[tokio::test]
    async fn file_stream_rejects_garbage_after_the_closing_dashes() {
        let mut stream = file_stream_then_error(&format!("\r\n--{BOUNDARY}--zz\r\n")).await;
        let bytes = stream.next().await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"hi");
        assert!(matches!(stream.next().await, Some(Err(FileStreamError::InvalidTrailer))));
    }

    /// Without a derived length claim the byte stream reports an unknown
    /// remaining length.
    #[tokio::test]
    async fn file_stream_remaining_length_unknown_without_claim() {
        let body = file_form(&[], "hi");
        let stream = parse_file_stream(&body, None).await;
        assert!(stream.remaining_length().exact().is_none());

        let total_len = body.len() as u64;
        let stream = parse_file_stream(&body, Some(total_len)).await;
        assert_eq!(stream.remaining_length().exact(), Some(2));
    }

    #[tokio::test]
    async fn file_stream_debug_contains_fields() {
        let body = file_form(&[], "hello");
        let stream = parse_file_stream(&body, None).await;
        let text = format!("{stream:?}");
        assert!(text.contains("content_len"), "{text}");
        assert!(text.contains("remaining"), "{text}");
    }

    #[test]
    fn multipart_underlying_error_exposes_source() {
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "boom");
        let err = MultipartError::Underlying(Box::new(cause));
        assert_eq!(err.to_string(), "MultipartError: Underlying: boom");
        let source = std::error::Error::source(&err).unwrap();
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::ConnectionReset)
        );
    }

    #[test]
    fn file_stream_underlying_error_exposes_source() {
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "boom");
        let err = FileStreamError::Underlying(Box::new(cause));
        assert_eq!(err.to_string(), "FileStreamError: Underlying: boom");
        let source = std::error::Error::source(&err).unwrap();
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::ConnectionReset)
        );
    }
}

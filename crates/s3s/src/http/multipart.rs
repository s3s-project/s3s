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

use futures::stream::{Stream, StreamExt};
use hyper::body::Bytes;
use memchr::memchr_iter;
use transform_stream::{AsyncTryStream, Yielder};

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
}

#[derive(Debug, thiserror::Error)]
pub enum MultipartError {
    #[error("MultipartError: Underlying: {0}")]
    Underlying(StdError),
    #[error("MultipartError: InvalidFormat")]
    InvalidFormat,
}

/// transform multipart
/// # Errors
/// Returns an `Err` if the format is invalid
pub async fn transform_multipart<S>(body_stream: S, boundary: &'_ [u8]) -> Result<Multipart, MultipartError>
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

    loop {
        // copy bytes to buf
        match body.as_mut().next().await {
            None => return Err(MultipartError::InvalidFormat),
            Some(Err(e)) => return Err(MultipartError::Underlying(e)),
            Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
        }

        // try to parse
        match try_parse(body, pat, &buf, &mut fields, boundary) {
            Err((b, p)) => {
                body = b;
                pat = p;
            }
            Ok(ans) => return ans,
        }
    }
}

/// try to parse data buffer, pat: b"--{boundary}\r\n"
#[allow(clippy::type_complexity)]
fn try_parse<S>(
    body: Pin<Box<S>>,
    pat: Box<[u8]>,
    buf: &'_ [u8],
    fields: &'_ mut Vec<(String, String)>,
    boundary: &'_ [u8],
) -> Result<Result<Multipart, MultipartError>, (Pin<Box<S>>, Box<[u8]>)>
where
    S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
{
    #[allow(clippy::indexing_slicing)]
    let pat_without_crlf = &pat[..pat.len().wrapping_sub(2)];

    fields.clear();

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
        match content_disposition.filename {
            None => {
                let value = match lines.split_to(pat_without_crlf) {
                    None => return Err((body, pat)),
                    Some(b) => {
                        #[allow(clippy::indexing_slicing)]
                        let b = &b[..b.len().saturating_sub(2)];

                        match std::str::from_utf8(b) {
                            Err(_) => return Ok(Err(MultipartError::InvalidFormat)),
                            Ok(s) => s,
                        }
                    }
                };

                fields.push((content_disposition.name.to_owned(), value.to_owned()));
            }
            Some(filename) => {
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
                let file_stream = FileStream::new(body, boundary, remaining_bytes);
                let file = File {
                    name: filename.to_owned(),
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
        }
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
    Underlying(StdError),
}

/// File stream
pub struct FileStream {
    /// inner stream
    inner: AsyncTryStream<Bytes, FileStreamError, SyncBoxFuture<'static, Result<(), FileStreamError>>>,
}

impl Debug for FileStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileStream {{...}}")
    }
}

impl FileStream {
    /// Constructs a `FileStream`
    fn new<S>(body: Pin<Box<S>>, boundary: &'_ [u8], prev_bytes: Option<Bytes>) -> Self
    where
        S: Stream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        /// internal async generator
        async fn generate<S>(
            mut y: Yielder<Result<Bytes, FileStreamError>>,
            mut body: Pin<Box<S>>,
            crlf_pat: Box<[u8]>,
            prev_bytes: Option<Bytes>,
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
                                    bytes.truncate(idx);
                                    y.yield_ok(bytes).await;
                                    return Ok(());
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
                            Some(Ok(b)) => buf.extend_from_slice(&b),
                        }
                        bytes = Bytes::from(mem::take(&mut buf));
                        state = 2;
                        continue 'dfa;
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

        Self {
            inner: AsyncTryStream::new(|y| -> SyncBoxFuture<'static, Result<(), FileStreamError>> {
                Box::pin(generate(y, body, crlf_pat, prev_bytes))
            }),
        }
    }
}

impl Stream for FileStream {
    type Item = Result<Bytes, FileStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl ByteStream for FileStream {
    fn remaining_length(&self) -> crate::stream::RemainingLength {
        crate::stream::RemainingLength::unknown()
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

        let file_stream = FileStream::new(Box::pin(body_stream), boundary, Some(Bytes::copy_from_slice(b"\r")));

        {
            let file_bytes = aggregate_file_stream(file_stream).await.unwrap();
            assert_eq!(file_bytes, file_content);
        }
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

        let ans = transform_multipart(body_stream, boundary.as_bytes()).await.unwrap();

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

        let ans = transform_multipart(body_stream, boundary.as_bytes()).await.unwrap();

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
}

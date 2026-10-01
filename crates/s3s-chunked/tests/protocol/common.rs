// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Shared helpers for the protocol tests.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use bytes::Bytes;
use futures::StreamExt;
use futures_core::Stream;
use s3s_chunked::{Error, Sha256Sum, SignContext, StdError};
use s3s_sigv4::AmzDate;
use std::task::Poll;

/// The AWS `SigV4` streaming test vector.
pub const TIMESTAMP: &str = "20130524T000000Z";
/// The AWS `SigV4` streaming test vector.
pub const REGION: &str = "us-east-1";
/// The AWS `SigV4` streaming test vector.
pub const SERVICE: &str = "s3";
/// The AWS `SigV4` streaming test vector.
pub const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
/// The AWS `SigV4` streaming test vector (the request signature).
pub const SEED_SIGNATURE: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";

pub fn amz_date() -> AmzDate {
    AmzDate::parse(TIMESTAMP).expect("valid timestamp")
}

pub fn seed() -> Sha256Sum {
    Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

pub fn sign_context() -> SignContext {
    SignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes())
}

fn signature(string_to_sign: &str) -> String {
    s3s_sigv4::calculate_signature(string_to_sign, SECRET_KEY, &amz_date(), REGION, SERVICE)
}

pub fn chunk_signature(prev: &str, data: &[u8]) -> String {
    signature(&s3s_sigv4::create_chunk_string_to_sign(&amz_date(), REGION, SERVICE, prev, &[data]))
}

pub fn trailer_signature(prev: &str, canonical: &[u8]) -> String {
    signature(&s3s_sigv4::create_trailer_string_to_sign(&amz_date(), REGION, SERVICE, prev, canonical))
}

/// Builds an `aws-chunked` body.
///
/// `trailers` carries the entries and whether a trailer signature is added.
pub fn body(chunks: &[Vec<u8>], sign_chunks: bool, trailers: Option<(&[(&str, &str)], bool)>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut prev = SEED_SIGNATURE.to_owned();

    for chunk in chunks {
        if sign_chunks {
            let sig = chunk_signature(&prev, chunk);
            body.extend_from_slice(format!("{:x};chunk-signature={sig}\r\n", chunk.len()).as_bytes());
            prev = sig;
        } else {
            body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        }
        body.extend_from_slice(chunk);
        body.extend_from_slice(b"\r\n");
    }

    if sign_chunks {
        let sig = chunk_signature(&prev, b"");
        body.extend_from_slice(format!("0;chunk-signature={sig}\r\n").as_bytes());
        prev = sig;
    } else {
        body.extend_from_slice(b"0\r\n");
    }

    if let Some((entries, with_signature)) = trailers {
        let mut sorted: Vec<(&str, &str)> = entries.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut canonical = Vec::new();
        for (name, value) in &sorted {
            canonical.extend_from_slice(name.as_bytes());
            canonical.push(b':');
            canonical.extend_from_slice(value.as_bytes());
            canonical.push(b'\n');
        }
        for (name, value) in &sorted {
            body.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
        }
        if with_signature {
            let sig = trailer_signature(&prev, &canonical);
            body.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
    }

    body
}

pub fn items(fragments: &[&[u8]]) -> Vec<Result<Bytes, StdError>> {
    fragments.iter().map(|f| Ok(Bytes::copy_from_slice(f))).collect()
}

/// One input step for tests that need `Pending` or an underlying error.
pub enum Step {
    /// Yields one fragment.
    Data(Bytes),
    /// Returns `Pending` once, waking the task, before moving on.
    Pending,
    /// Yields one underlying stream error.
    Error,
}

/// Builds a stream from explicit steps.
pub fn steps(steps: Vec<Step>) -> impl Stream<Item = Result<Bytes, StdError>> + Unpin {
    let mut index = 0;
    futures::stream::poll_fn(move |cx| {
        let Some(step) = steps.get(index) else {
            return Poll::Ready(None);
        };
        index += 1;
        match step {
            Step::Data(bytes) => Poll::Ready(Some(Ok(bytes.clone()))),
            Step::Pending => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Step::Error => Poll::Ready(Some(Err(std::io::Error::other("synthetic input failure").into()))),
        }
    })
}

/// Splits the payload into fragments of at most `size` bytes.
pub fn fragmented(body: &[u8], size: usize) -> Vec<Result<Bytes, StdError>> {
    body.chunks(size.max(1)).map(|c| Ok(Bytes::copy_from_slice(c))).collect()
}

/// Drains a decoder, stopping at the first error.
pub fn drain<S>(stream: S) -> (Vec<u8>, Option<Error>)
where
    S: Stream<Item = Result<Bytes, Error>> + Unpin,
{
    let mut payload = Vec::new();
    let mut error = None;
    futures::executor::block_on(async {
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => payload.extend_from_slice(&bytes),
                Err(err) => {
                    error = Some(err);
                    break;
                }
            }
        }
    });
    (payload, error)
}

/// Collects the trailer entries as sorted `name:value` strings.
pub fn trailer_entries(handle: &s3s_chunked::TrailerHandle) -> Option<Vec<String>> {
    handle.take().map(|map| {
        let mut entries: Vec<String> = map
            .iter()
            .map(|(name, value)| format!("{name}:{}", value.to_str().unwrap_or("<binary>")))
            .collect();
        entries.sort();
        entries
    })
}

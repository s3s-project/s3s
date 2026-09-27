// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Test helpers shared by the decoder unit tests.

use crate::SignContext;
use crate::utils::{Sha256Sum, StdError};

use s3s_sigv4::AmzDate;

use bytes::Bytes;
use futures::StreamExt;
use futures_core::Stream;

/// One input fragment.
pub type Item = Result<Bytes, StdError>;

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

pub fn sign_context() -> SignContext {
    SignContext::new(amz_date(), REGION.into(), SERVICE.into(), SECRET_KEY.as_bytes())
}

pub fn seed_signature() -> Sha256Sum {
    Sha256Sum::from_hex(SEED_SIGNATURE).expect("valid seed signature")
}

/// Puts the whole payload into one fragment.
pub fn single(payload: &[u8]) -> Vec<Item> {
    vec![Ok(Bytes::copy_from_slice(payload))]
}

/// Splits the payload into fragments of at most `size` bytes.
pub fn fragmented(payload: &[u8], size: usize) -> Vec<Item> {
    payload
        .chunks(size.max(1))
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect()
}

/// Drains a decoder, returning the produced payload and the first error.
pub fn drain<S>(stream: S) -> (Vec<u8>, Option<crate::Error>)
where
    S: Stream<Item = Result<Bytes, crate::Error>> + Unpin,
{
    let mut payload = Vec::new();
    let mut error = None;
    futures::executor::block_on(async {
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => payload.extend_from_slice(&bytes),
                Err(err) => error = Some(err),
            }
        }
    });
    (payload, error)
}

/// Computes the signature of one chunk in the chain.
pub fn chunk_signature(prev: &Sha256Sum, data: &[u8]) -> Sha256Sum {
    let ctx = sign_context();
    let mut prev_buf = [0_u8; 64];
    let prev_hex = prev.to_hex(&mut prev_buf);
    let digest = Sha256Sum::from_bytes(sha256(data));
    let mut digest_buf = [0_u8; 64];
    let digest_hex = digest.to_hex(&mut digest_buf);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}/{}/{}/aws4_request\n{prev_hex}\n{}\n{digest_hex}",
        ctx_amz_date().fmt_iso8601(),
        ctx_amz_date().fmt_date(),
        REGION,
        SERVICE,
        s3s_sigv4::EMPTY_STRING_SHA256_HASH,
    );
    ctx.sign(&string_to_sign)
}

fn ctx_amz_date() -> AmzDate {
    amz_date()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

/// Computes the trailer block signature.
pub fn trailer_signature(prev: &Sha256Sum, canonical: &[u8]) -> Sha256Sum {
    let ctx = sign_context();
    let mut prev_buf = [0_u8; 64];
    let prev_hex = prev.to_hex(&mut prev_buf);
    let digest = Sha256Sum::from_bytes(sha256(canonical));
    let mut digest_buf = [0_u8; 64];
    let digest_hex = digest.to_hex(&mut digest_buf);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256-TRAILER\n{}\n{}/{}/{}/aws4_request\n{prev_hex}\n{digest_hex}",
        ctx_amz_date().fmt_iso8601(),
        ctx_amz_date().fmt_date(),
        REGION,
        SERVICE,
    );
    ctx.sign(&string_to_sign)
}

/// Builds a signed chunk line.
pub fn signed_chunk_line(size: usize, signature: &Sha256Sum) -> Vec<u8> {
    let mut buf = [0_u8; 64];
    let hex = signature.to_hex(&mut buf);
    format!("{size:x};chunk-signature={hex}\r\n").into_bytes()
}

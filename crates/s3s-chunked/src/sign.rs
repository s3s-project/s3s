// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Signing context for signed `aws-chunked` streams.

use crate::error::Error;

use s3s_sigv4::AmzDate;

use std::fmt::{self, Debug, Formatter};

use bytes::Bytes;
use zeroize::Zeroizing;

/// Signing context shared by the chunks of a signed `aws-chunked` stream.
pub struct SignContext {
    amz_date: AmzDate,
    region: Box<str>,
    service: Box<str>,
    signing_key: Zeroizing<[u8; 32]>,
}

impl SignContext {
    /// Derives the signing key from the request credentials.
    #[must_use]
    pub fn new(amz_date: AmzDate, region: Box<str>, service: Box<str>, secret_key: &[u8]) -> Self {
        let signing_key = Zeroizing::new(s3s_sigv4::derive_signing_key(secret_key, &amz_date, &region, &service));
        Self {
            amz_date,
            region,
            service,
            signing_key,
        }
    }

    /// Computes the `SigV4` signature of `string_to_sign` with the derived signing key.
    #[must_use]
    pub fn sign(&self, string_to_sign: &str) -> crate::Sha256Sum {
        crate::Sha256Sum::from_bytes(s3s_sigv4::calculate_signature_with_key(string_to_sign, &self.signing_key))
    }
}

/// Per-stream signing state: the chunk chain and reusable buffers.
pub struct SignState {
    ctx: SignContext,
    prev_signature: crate::Sha256Sum,
    data: Vec<Bytes>,
    string_to_sign: String,
}

impl SignState {
    pub fn new(ctx: SignContext, seed_signature: crate::Sha256Sum) -> Self {
        Self {
            ctx,
            prev_signature: seed_signature,
            data: Vec::new(),
            string_to_sign: String::with_capacity(256),
        }
    }

    /// Drops the buffered fragments of the previous chunk.
    pub fn clear_chunk(&mut self) {
        self.data.clear();
    }

    /// Buffers one fragment of the current signed chunk.
    pub fn push(&mut self, bytes: Bytes) {
        self.data.push(bytes);
    }

    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    pub fn fragment(&self, index: usize) -> Bytes {
        self.data[index].clone()
    }

    /// Verifies the signature of the current chunk and advances the chain.
    ///
    /// The buffered fragments are only yielded after this succeeds.
    pub fn verify_chunk(&mut self, expected: &[u8; 64]) -> Result<(), Error> {
        let expected = parse_signature(expected)?;
        let digest = sha256_chunks(&self.data);
        let signature = self.sign_digest(&digest);
        if !expected.ct_equal(&signature) {
            return Err(Error::SignatureMismatch);
        }
        self.prev_signature = signature;
        Ok(())
    }

    /// Verifies the trailer signature, or reports its absence when required.
    pub fn verify_trailers(&mut self, canonical: &[u8], provided: Option<&[u8]>, required: bool) -> Result<(), Error> {
        let Some(provided) = provided else {
            return if required { Err(Error::FormatError) } else { Ok(()) };
        };

        let expected = parse_signature(provided)?;
        let mut prev_buf = [0_u8; 64];
        let prev_hex = self.prev_signature.to_hex(&mut prev_buf);
        self.string_to_sign.clear();
        let _ = s3s_sigv4::write_trailer_string_to_sign(
            &mut self.string_to_sign,
            &self.ctx.amz_date,
            &self.ctx.region,
            &self.ctx.service,
            prev_hex,
            canonical,
        );
        let signature = self.ctx.sign(&self.string_to_sign);
        if !expected.ct_equal(&signature) {
            return Err(Error::SignatureMismatch);
        }
        Ok(())
    }

    /// Writes the chunk string-to-sign into the reusable buffer and signs it.
    fn sign_digest(&mut self, digest: &crate::Sha256Sum) -> crate::Sha256Sum {
        let mut prev_buf = [0_u8; 64];
        let mut digest_buf = [0_u8; 64];
        let prev_hex = self.prev_signature.to_hex(&mut prev_buf);
        let digest_hex = digest.to_hex(&mut digest_buf);
        self.string_to_sign.clear();
        let _ = s3s_sigv4::write_chunk_string_to_sign(
            &mut self.string_to_sign,
            &self.ctx.amz_date,
            &self.ctx.region,
            &self.ctx.service,
            prev_hex,
            digest_hex,
        );
        self.ctx.sign(&self.string_to_sign)
    }
}

fn parse_signature(bytes: &[u8]) -> Result<crate::Sha256Sum, Error> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(crate::Sha256Sum::from_hex)
        .ok_or(Error::SignatureMismatch)
}

fn sha256_chunks(chunks: &[Bytes]) -> crate::Sha256Sum {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for chunk in chunks {
        hasher.update(chunk.as_ref());
    }
    crate::Sha256Sum::from_bytes(hasher.finalize().into())
}

impl Debug for SignContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignContext")
            .field("amz_date", &self.amz_date)
            .field("region", &self.region)
            .field("service", &self.service)
            .field("signing_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_matches_a_manually_derived_key() {
        let secret = b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let amz_date = AmzDate::parse("20130524T000000Z").expect("valid timestamp");
        let ctx = SignContext::new(amz_date.clone(), "us-east-1".into(), "s3".into(), secret);
        let expected = s3s_sigv4::calculate_signature_with_key(
            "string-to-sign",
            &s3s_sigv4::derive_signing_key(secret, &amz_date, "us-east-1", "s3"),
        );
        assert_eq!(ctx.sign("string-to-sign"), crate::Sha256Sum::from_bytes(expected));
    }

    #[test]
    fn debug_redacts_the_signing_key() {
        let ctx = SignContext::new(
            AmzDate::parse("20130524T000000Z").expect("valid timestamp"),
            "us-east-1".into(),
            "s3".into(),
            b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        let text = format!("{ctx:?}");
        assert!(text.contains("SignContext"));
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("wJalrXUtnFEMI"));
    }
}

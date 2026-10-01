// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Signing context for signed `aws-chunked` streams.

use crate::error::Error;

use s3s_sigv4::AmzDate;

use std::fmt::{self, Debug, Formatter};

use bytes::{Bytes, BytesMut};
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

/// Buffered payload of the current signed chunk.
///
/// Fragments are normally kept exactly as the transport delivered them, so a
/// verified chunk reaches its consumer without the payload being copied.
/// Framing is peer controlled though: a stream of tiny fragments would pin one
/// read buffer per fragment and grow the fragment list without bound. Once the
/// list gets long the buffered bytes are coalesced into a single allocation and
/// later fragments are appended there, which releases those read buffers. The
/// switch happens once, so the extra work stays linear, and a chunk arrives in a
/// handful of fragments as usual stays copy free.
enum ChunkBuffer {
    /// Fragments kept as delivered.
    Fragments(Vec<Bytes>),
    /// Fragments coalesced into one growing allocation.
    Coalesced(BytesMut),
    /// Coalesced bytes frozen after the signature was verified.
    Sealed(Bytes),
}

impl ChunkBuffer {
    /// Upper bound on the fragment list, which bounds the metadata and the
    /// number of read buffers a single chunk can pin.
    const MAX_FRAGMENTS: usize = 64;

    fn new() -> Self {
        Self::Fragments(Vec::new())
    }

    /// Buffers one fragment of the current chunk.
    fn push(&mut self, bytes: Bytes) {
        match self {
            Self::Fragments(fragments) => {
                if fragments.len() >= Self::MAX_FRAGMENTS {
                    let capacity = fragments.iter().map(Bytes::len).sum::<usize>() + bytes.len();
                    let mut buffer = BytesMut::with_capacity(capacity);
                    for fragment in fragments.drain(..) {
                        buffer.extend_from_slice(&fragment);
                    }
                    buffer.extend_from_slice(&bytes);
                    *self = Self::Coalesced(buffer);
                } else {
                    fragments.push(bytes);
                }
            }
            Self::Coalesced(buffer) => buffer.extend_from_slice(&bytes),
            Self::Sealed(_) => unreachable!("a sealed buffer is not pushed into"),
        }
    }

    /// Drops the buffered fragments of the previous chunk.
    fn clear(&mut self) {
        match self {
            Self::Fragments(fragments) => fragments.clear(),
            Self::Coalesced(buffer) => buffer.clear(),
            Self::Sealed(_) => *self = Self::new(),
        }
    }

    /// Number of fragments the chunk is emitted as.
    fn fragment_count(&self) -> usize {
        match self {
            Self::Fragments(fragments) => fragments.len(),
            Self::Coalesced(buffer) => usize::from(!buffer.is_empty()),
            Self::Sealed(bytes) => usize::from(!bytes.is_empty()),
        }
    }

    /// One fragment of the chunk; callers only ask for indices below
    /// `fragment_count`, and only after the signature was verified.
    fn fragment(&self, index: usize) -> Bytes {
        match self {
            Self::Fragments(fragments) => fragments[index].clone(),
            Self::Sealed(bytes) => bytes.clone(),
            Self::Coalesced(buffer) => Bytes::copy_from_slice(&buffer[index.min(buffer.len())..]),
        }
    }

    /// Hashes the buffered payload without copying it.
    fn sha256(&self) -> crate::Sha256Sum {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        match self {
            Self::Fragments(fragments) => {
                for fragment in fragments {
                    hasher.update(fragment.as_ref());
                }
            }
            Self::Coalesced(buffer) => hasher.update(&buffer[..]),
            Self::Sealed(bytes) => hasher.update(bytes.as_ref()),
        }
        crate::Sha256Sum::from_bytes(hasher.finalize().into())
    }

    /// Freezes a coalesced buffer, so a verified chunk is handed out without
    /// another copy.
    fn seal(&mut self) {
        if let Self::Coalesced(buffer) = self {
            *self = Self::Sealed(buffer.split().freeze());
        }
    }
}

/// Per-stream signing state: the chunk chain and reusable buffers.
pub struct SignState {
    ctx: SignContext,
    prev_signature: crate::Sha256Sum,
    data: ChunkBuffer,
    string_to_sign: String,
}

impl SignState {
    pub fn new(ctx: SignContext, seed_signature: crate::Sha256Sum) -> Self {
        Self {
            ctx,
            prev_signature: seed_signature,
            data: ChunkBuffer::new(),
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

    pub fn fragment_count(&self) -> usize {
        self.data.fragment_count()
    }

    pub fn fragment(&self, index: usize) -> Bytes {
        self.data.fragment(index)
    }

    /// Verifies the signature of the current chunk and advances the chain.
    ///
    /// The buffered fragments are only yielded after this succeeds.
    pub fn verify_chunk(&mut self, expected: &[u8; 64]) -> Result<(), Error> {
        let expected = parse_signature(expected)?;
        let digest = self.data.sha256();
        let signature = self.sign_digest(&digest);
        if !expected.ct_equal(&signature) {
            return Err(Error::SignatureMismatch);
        }
        self.data.seal();
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

#[cfg(test)]
mod buffer_tests {
    use super::*;

    fn state() -> SignState {
        let ctx = SignContext::new(
            AmzDate::parse("20130524T000000Z").unwrap(),
            "us-east-1".into(),
            "s3".into(),
            b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        SignState::new(ctx, crate::Sha256Sum::from_bytes([0; 32]))
    }

    #[test]
    fn keeps_whole_fragments_as_delivered() {
        let mut sign = state();
        for _ in 0..4 {
            sign.push(Bytes::from(vec![0_u8; 8 * 1024]));
        }
        assert_eq!(sign.fragment_count(), 4, "whole buffers stay zero copy");
        assert_eq!(sign.fragment(0).len(), 8 * 1024);
    }

    #[test]
    fn coalesces_tiny_fragments() {
        let mut sign = state();
        for _ in 0..1024 {
            sign.push(Bytes::from_static(&[7]));
        }
        assert_eq!(sign.fragment_count(), 1, "tiny fragments are coalesced");
        assert_eq!(sign.fragment(0).len(), 1024);
    }

    #[test]
    fn coalesces_beyond_the_fragment_cap() {
        let mut sign = state();
        for _ in 0..(ChunkBuffer::MAX_FRAGMENTS + 8) {
            sign.push(Bytes::from(vec![0_u8; 8 * 1024]));
        }
        assert_eq!(sign.fragment_count(), 1, "the fragment list is bounded");
    }

    #[test]
    fn clears_between_chunks() {
        let mut sign = state();
        sign.push(Bytes::from_static(b"tiny"));
        sign.clear_chunk();
        assert_eq!(sign.fragment_count(), 0);
    }
}

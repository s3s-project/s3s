// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Async streaming decoder for `aws-chunked` request bodies.
//!
//! The decoder consumes the wire framing of an `aws-chunked` request body and
//! produces the decoded payload. Chunk signatures are verified before the data
//! of the corresponding chunk is yielded, and trailer signatures are verified
//! before trailing headers are exposed.
//!
//! The framing follows the `SigV4` streaming form of the S3 API, where the
//! payload is transferred in multiple chunks: a chunk-size line carrying an
//! optional chunk signature, the `CRLF` that terminates each chunk, and the
//! trailer block at the end.
//!
//! # Modes
//!
//! The consumer selects the mode from the `x-amz-content-sha256` request
//! header:
//!
//! - `STREAMING-UNSIGNED-PAYLOAD-TRAILER` — unsigned payload with trailing
//!   headers, decoded by [`ChunkedStream::unsigned`]. A chunk signature or a
//!   trailer signature in the body is rejected, because a decoder without a
//!   signing context cannot verify it.
//! - `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` — signed payload without trailing
//!   headers, decoded by [`ChunkedStream::signed`].
//! - `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER` — signed payload with
//!   trailing headers, decoded by [`ChunkedStream::signed`]; chunk and trailer
//!   signatures are mandatory.
//!
//! Whether the body carries a trailer block is discovered while decoding: the
//! signed and unsigned modes both accept a body that ends right after the final
//! zero chunk. A request that announced trailing headers can require the block
//! with [`ChunkedStream::with_required_trailers`]; without it an empty block is
//! accepted, and a run of empty body fragments is a format error either way.
//!
//! # Guarantees
//!
//! - A chunk is emitted as the fragments it arrived in, so a signed chunk reaches
//!   its consumer without being copied; a chunk that arrives in a very large number
//!   of fragments is coalesced into one allocation first, which bounds the read
//!   buffers a malformed request can pin.
//! - A verified chunk is emitted only after its signature, and trailer headers only
//!   after the trailer block was accepted. A request that fails exposes nothing: it
//!   yields its error and then `None` for ever.
//!
//! The `SigV4a` values (`STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD` and
//! `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER`) are not implemented
//! yet: the request layer rejects them with `NotImplemented`, and the signing
//! state of this crate is `HMAC-SHA256` only. The non-streaming values
//! (`UNSIGNED-PAYLOAD` and a raw SHA-256 digest) do not use this framing.

#![deny(missing_docs)]

mod decoder;
mod error;
mod limits;
mod meta;
mod sign;
mod stream;
mod trailer;
mod utils;

#[cfg(test)]
mod test_utils;

pub use self::error::Error;
pub use self::limits::Limits;
pub use self::sign::SignContext;
pub use self::stream::ChunkedStream;
pub use self::trailer::TrailerHandle;
pub use self::utils::{Sha256Sum, StdError};

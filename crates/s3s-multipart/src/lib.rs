// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Asynchronous streaming parser for `multipart/form-data`.
//!
//! The parser consumes any [`futures_core::Stream`] of [`bytes::Bytes`]
//! chunks and yields individual parts together with their headers and data.
//!
//! # Compatibility notes
//!
//! This crate follows RFC 2046 section 5.1 and RFC 7578, with a small set
//! of deliberate differences. Every item in this list is documented at the
//! corresponding API item and has a regression test.
//!
//! - Parts carrying more header fields than the parser reads are accepted:
//!   the surplus fields are ignored. RFC 7578 section 4.8 defines exactly
//!   `Content-Disposition`, `Content-Type` and the deprecated
//!   `Content-Transfer-Encoding`, requires any other header field to be
//!   ignored, and a conforming part carries at most those three — so the
//!   limit is the conforming set itself and can only ever drop fields that a
//!   conforming part cannot have.
//! - A preamble before the first boundary and transport padding after a
//!   boundary line are accepted.
//! - Field lines are parsed with the strict header grammar: a folded field
//!   (a continuation line starting with optional whitespace) and a field name
//!   with leading optional whitespace are both rejected with
//!   [`Error::InvalidFormat`], because the block is read by `httparse` rather
//!   than by the lenient line-folding rules of RFC 7230 section 3.2.4, which
//!   let a server reject a folded field anyway.
//! - Empty part headers and empty part bodies are valid.
//! - `Content-Disposition` parsing is tolerant: parameter names and the
//!   `form-data` token are case-insensitive, parameter order is free, and
//!   both quoted and token values are accepted.
//! - `Content-Disposition` `name` and `filename` are returned as raw bytes:
//!   no UTF-8 validation is performed, and callers decide how to convert or
//!   reject invalid sequences. Parsing is best-effort, so a malformed
//!   parameter tail is ignored instead of failing the parse.
//! - Quoted `Content-Disposition` parameter values keep their backslash
//!   escape sequences: `quoted-pair` is not decoded (`\"` stays `\"`), because
//!   returned values are always sub-slices of the header value. Callers that
//!   need the decoded value have to decode it themselves.
//! - The boundary is validated when it is constructed, according to RFC 2046
//!   section 5.1.1 (1 to 70 characters, only `bchars`, and no trailing
//!   whitespace). A boundary that other parsers accept but RFC 2046 forbids is
//!   rejected, and there is no unchecked constructor.
//! - A part data stream taken with `take_data_stream` ends at the closing
//!   delimiter without interpreting what follows. Converting it with
//!   `into_final` yields a `FinalPartDataStream` that enforces the strict
//!   closing delimiter and rejects an epilogue, because callers need an
//!   exact content length.
//! - The internal buffer limit applies to the bytes accumulated in the
//!   internal buffer. While a part header block is still being read it also
//!   bounds the arriving chunk: a chunk that would push the buffer over the
//!   limit is rejected even when it contains the header terminator, because
//!   the bytes that follow the terminator are retained until the header block
//!   is consumed. Data chunks yielded while streaming part data are never
//!   retained and are not charged to the limit.

#![deny(missing_docs)]
#![deny(clippy::expect_used, clippy::panic, clippy::unreachable, clippy::unwrap_used)]
#![deny(clippy::missing_panics_doc)]

// Internal visibility: every module below is private and none of them is
// re-exported wholesale, so items that live in them are declared `pub` — for a
// crate-root child module that is the same reach as `pub(super)`, just shorter.
// Members of types that ARE re-exported must stay `pub(super)`: an inherent
// method or a field of a public type is public API even when its impl block
// lives in a private module, which rustc reports as `missing_docs` (the crate
// denies it) and as `private_interfaces` when the member type is internal.
mod error;
pub use self::error::Error;

mod boundary;
pub use self::boundary::Boundary;

mod content_disposition;
pub use self::content_disposition::{ContentDisposition, parse_content_disposition};

mod buffer;

mod delimiter;
mod header;
mod utils;

mod multipart;
pub use self::multipart::Multipart;

mod part;
pub use self::part::Part;

mod part_data_stream;
pub use self::part_data_stream::PartDataStream;

mod final_part_data_stream;
pub use self::final_part_data_stream::FinalPartDataStream;

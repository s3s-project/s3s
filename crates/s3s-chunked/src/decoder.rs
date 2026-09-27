// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Shared polling core for the `aws-chunked` decoders.
//!
//! The core is an explicit state machine driven by [`Stream::poll_next`]: each
//! step runs one [`Phase`] transition and either makes progress or reports
//! `Pending`, so a decoder never spins and never parks without a wakeup.
//!
//! # Phase transitions
//!
//! ```text
//!                     size > 0                data done            CRLF
//!   Meta ───────────────────────▶ Data{remaining} ──────▶ Crlf{state} ──────┐
//!     ▲                                                                     │
//!     │                        ┌────────── verified ──────────┐             │
//!     └──── Emitting{index} ◀──┤ Verify{trailers: false}      │◀────────────┘ (signed chunk)
//!     │                        └──────────────────────────────┘
//!     │
//!     └──── (unsigned chunk)
//!
//!                     size == 0
//!   Meta ──────────────────────▶ [Verify{trailers: true}] ──▶ Trailers ──EOF──▶ Done
//!
//!   any failure ──▶ Done
//! ```
//!
//! - `Meta` accumulates one metadata line: the chunk size plus an optional
//!   chunk signature. A size above zero enters `Data`; a size of zero ends the
//!   payload and enters `Verify` with `trailers`, or `Trailers` directly when
//!   the chunk carried no signature.
//! - `Data` hands out payload bytes straight from the input buffer. When the
//!   chunk carries a signature the bytes are buffered in the signing state
//!   instead, because they are only released after `Verify`.
//! - `Crlf` consumes the two bytes that terminate the chunk, with a fast path
//!   when both are already buffered, then selects `Verify` for a signed chunk
//!   and `Meta` for an unsigned one.
//! - `Verify` checks the buffered chunk signature; a mismatch fails the stream
//!   before any of the buffered fragments is produced.
//! - `Emitting` yields the verified fragments of one chunk in order and then
//!   returns to `Meta`.
//! - `Trailers` reads the rest of the body as the trailer block (bounded by
//!   [`Limits`]), verifies the trailer signature when the mode requires it,
//!   publishes the trailing headers and ends the stream.
//! - `Done` is terminal: later polls return `None`.
//!
//! # Invariants
//!
//! - Every loop in a step either consumes input or returns `Pending`; progress
//!   is never assumed.
//! - Chunk data is yielded only after its signature verified; trailer headers
//!   are published only after the trailer block verified.
//! - Every failure sets [`Phase::Done`] before the error item is returned, so a
//!   failed stream never resumes.
//! - Produced bytes are accounted against the declared decoded length before
//!   they are yielded.
//! - [`Signing`] decides whether signatures are mandatory; whether the body
//!   carries a signature or a trailer block is discovered from the body itself.

use crate::error::Error;
use crate::limits::Limits;
use crate::meta::{ChunkMeta, parse_chunk_meta};
use crate::sign::SignState;
use crate::trailer::{TrailerHandle, parse_trailers};
use crate::utils::StdError;

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures_core::Stream;
use pin_project_lite::pin_project;

pin_project! {
    /// Polling core shared by the public decoder types.
    pub struct Decoder<S> {
        #[pin]
        inner: S,
        state: State,
    }
}

/// Decoding state, independent of the wrapped stream.
struct State {
    limits: Limits,
    remaining: usize,
    phase: Phase,
    carry: Bytes,
    meta_buf: Vec<u8>,
    trailer_buf: Vec<u8>,
    chunk_signature: Option<[u8; 64]>,
    signing: Signing,
    handle: TrailerHandle,
}

/// Signing behaviour of a decoder, fixed when it is constructed.
pub enum Signing {
    /// No signing context: a signature in the body is a format error.
    None,
    /// Chunk and trailer signatures are mandatory.
    Required(SignState),
}

impl Signing {
    /// Returns the signing state, when the decoder has one.
    fn state(&mut self) -> Option<&mut SignState> {
        match self {
            Self::None => None,
            Self::Required(state) => Some(state),
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// Accumulating the next chunk metadata line.
    Meta,
    /// Reading chunk data.
    Data { remaining: usize },
    /// Consuming the CRLF that terminates chunk data.
    Crlf { state: u8 },
    /// Verifying the buffered signed chunk.
    Verify { trailers: bool },
    /// Yielding the verified fragments of the current chunk.
    Emitting { index: usize },
    /// Reading the trailer block until the end of the stream.
    Trailers,
    /// The stream has finished.
    Done,
}

/// Outcome of a metadata read.
enum MetaOutcome {
    /// A complete line is buffered.
    Line,
    /// The stream ended before a complete line was read.
    End,
}

/// What the poll loop does after one phase step.
enum Step {
    /// The wrapped stream is not ready.
    Pending,
    /// Run another step with the updated state.
    Again,
    /// A decoded fragment is ready to be yielded.
    Yield(Bytes),
    /// The stream finished successfully.
    Done,
    /// The stream failed.
    Fail(Error),
}

impl<S> Decoder<S> {
    pub fn new(inner: S, decoded_content_length: usize, limits: Limits, signing: Signing) -> Self {
        Self {
            inner,
            state: State {
                limits,
                remaining: decoded_content_length,
                phase: Phase::Meta,
                carry: Bytes::new(),
                meta_buf: Vec::new(),
                trailer_buf: Vec::new(),
                chunk_signature: None,
                signing,
                handle: TrailerHandle::empty(),
            },
        }
    }

    #[must_use]
    pub fn trailer_handle(&self) -> TrailerHandle {
        self.state.handle.clone()
    }

    #[must_use]
    pub fn exact_remaining_length(&self) -> usize {
        self.state.remaining
    }

    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S> Stream for Decoder<S>
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let state = &mut *this.state;

        loop {
            match step_phase(this.inner.as_mut(), state, cx) {
                Step::Pending => return Poll::Pending,
                Step::Again => {}
                Step::Yield(bytes) => return emit(bytes, &mut state.remaining, &mut state.phase),
                Step::Done => {
                    state.phase = Phase::Done;
                    return Poll::Ready(None);
                }
                Step::Fail(error) => return fail(&mut state.phase, error),
            }
        }
    }
}

/// Advances the state machine by one step.
fn step_phase<S>(mut inner: Pin<&mut S>, state: &mut State, cx: &mut Context<'_>) -> Step
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    match state.phase {
        Phase::Meta => step_meta(inner.as_mut(), state, cx),
        Phase::Data { remaining } => step_data(inner.as_mut(), state, remaining, cx),
        Phase::Crlf { state: crlf } => step_crlf(inner.as_mut(), state, crlf, cx),
        Phase::Verify { trailers } => step_verify(state, trailers),
        Phase::Emitting { index } => step_emitting(state, index),
        Phase::Trailers => step_trailers(inner.as_mut(), state, cx),
        Phase::Done => Step::Done,
    }
}

/// Reads one chunk metadata line and enters the phase it selects.
fn step_meta<S>(mut inner: Pin<&mut S>, state: &mut State, cx: &mut Context<'_>) -> Step
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    let limit = state.limits.max_chunk_meta_size;
    match poll_meta(inner.as_mut(), &mut state.carry, &mut state.meta_buf, limit, cx) {
        Poll::Pending => return Step::Pending,
        Poll::Ready(Err(error)) => return Step::Fail(error),
        Poll::Ready(Ok(MetaOutcome::End)) => {
            if state.remaining == 0 {
                return Step::Done;
            }
            return Step::Fail(Error::Incomplete);
        }
        Poll::Ready(Ok(MetaOutcome::Line)) => {}
    }

    let Some(meta) = parse_chunk_meta(&state.meta_buf) else {
        return Step::Fail(Error::FormatError);
    };
    state.meta_buf.clear();

    let ChunkMeta { size, signature } = meta;
    if let Err(error) = accept_chunk_meta(state, size, signature) {
        return Step::Fail(error);
    }

    state.phase = if size == 0 {
        if state.chunk_signature.is_some() {
            Phase::Verify { trailers: true }
        } else {
            Phase::Trailers
        }
    } else {
        Phase::Data { remaining: size }
    };

    Step::Again
}

/// Validates one chunk metadata line and records its signature.
fn accept_chunk_meta(state: &mut State, size: usize, signature: Option<[u8; 64]>) -> Result<(), Error> {
    let Some(sign) = state.signing.state() else {
        // No signing context: a signature that cannot be verified is rejected.
        return if signature.is_some() {
            Err(Error::FormatError)
        } else {
            Ok(())
        };
    };

    let Some(signature) = signature else {
        // Signed streams require a signature on every chunk.
        return Err(Error::FormatError);
    };

    sign.clear_chunk();
    if size > state.limits.max_signed_chunk_size {
        return Err(Error::ChunkDataTooLarge(size, state.limits.max_signed_chunk_size));
    }

    state.chunk_signature = Some(signature);

    Ok(())
}

/// Consumes chunk data, buffering signed chunks and yielding unsigned ones.
fn step_data<S>(mut inner: Pin<&mut S>, state: &mut State, remaining: usize, cx: &mut Context<'_>) -> Step
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    if remaining == 0 {
        state.phase = Phase::Crlf { state: 0 };
        return Step::Again;
    }

    if state.carry.is_empty() {
        match poll_fragment(inner.as_mut(), cx) {
            Poll::Pending => return Step::Pending,
            Poll::Ready(None) => return Step::Fail(Error::Incomplete),
            Poll::Ready(Some(Err(error))) => return Step::Fail(error),
            Poll::Ready(Some(Ok(bytes))) => state.carry = bytes,
        }
        return Step::Again;
    }

    let take = remaining.min(state.carry.len());
    let data = state.carry.split_to(take);

    if state.chunk_signature.is_some() {
        if let Some(sign) = state.signing.state() {
            sign.push(data);
        }
        state.phase = Phase::Data {
            remaining: remaining - take,
        };
        return Step::Again;
    }

    state.phase = if take == remaining {
        Phase::Crlf { state: 0 }
    } else {
        Phase::Data {
            remaining: remaining - take,
        }
    };

    Step::Yield(data)
}

/// Consumes the CRLF that terminates chunk data.
fn step_crlf<S>(mut inner: Pin<&mut S>, state: &mut State, crlf: u8, cx: &mut Context<'_>) -> Step
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    if crlf == 0 && state.carry.starts_with(b"\r\n") {
        state.carry.advance(2);
        let signed = state.chunk_signature.is_some();
        finish_chunk(&mut state.phase, signed);
        return Step::Again;
    }

    let expected = if crlf == 0 { b'\r' } else { b'\n' };
    loop {
        match state.carry.first().copied() {
            Some(byte) if byte == expected => {
                state.carry.advance(1);
                break;
            }
            Some(_) => return Step::Fail(Error::FormatError),
            None => match poll_fragment(inner.as_mut(), cx) {
                Poll::Pending => return Step::Pending,
                Poll::Ready(None) => return Step::Fail(Error::Incomplete),
                Poll::Ready(Some(Err(error))) => return Step::Fail(error),
                Poll::Ready(Some(Ok(bytes))) => state.carry = bytes,
            },
        }
    }

    if crlf == 0 {
        state.phase = Phase::Crlf { state: 1 };
        return Step::Again;
    }

    let signed = state.chunk_signature.is_some();
    finish_chunk(&mut state.phase, signed);
    Step::Again
}

/// Verifies the buffered signed chunk.
fn step_verify(state: &mut State, trailers: bool) -> Step {
    let Some(signature) = state.chunk_signature.take() else {
        return Step::Fail(Error::FormatError);
    };
    let Some(sign) = state.signing.state() else {
        return Step::Fail(Error::FormatError);
    };
    if let Err(error) = sign.verify_chunk(&signature) {
        return Step::Fail(error);
    }

    state.phase = if trailers {
        Phase::Trailers
    } else {
        Phase::Emitting { index: 0 }
    };

    Step::Again
}

/// Yields the fragments of a verified chunk.
fn step_emitting(state: &mut State, index: usize) -> Step {
    let Some(sign) = state.signing.state() else {
        return Step::Fail(Error::FormatError);
    };

    if index < sign.data_len() {
        let data = sign.fragment(index);
        state.phase = Phase::Emitting { index: index + 1 };
        return Step::Yield(data);
    }

    sign.clear_chunk();
    state.phase = Phase::Meta;

    Step::Again
}

/// Reads the trailer block until the end of the stream, then verifies it.
fn step_trailers<S>(mut inner: Pin<&mut S>, state: &mut State, cx: &mut Context<'_>) -> Step
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    loop {
        if !state.carry.is_empty() {
            let total = state.trailer_buf.len().saturating_add(state.carry.len());
            if total > state.limits.max_trailers_size {
                return Step::Fail(Error::TrailersTooLarge(total, state.limits.max_trailers_size));
            }
            state.trailer_buf.extend_from_slice(&state.carry);
            state.carry = Bytes::new();
        }

        match poll_fragment(inner.as_mut(), cx) {
            Poll::Pending => return Step::Pending,
            Poll::Ready(None) => break,
            Poll::Ready(Some(Err(error))) => return Step::Fail(error),
            Poll::Ready(Some(Ok(bytes))) => state.carry = bytes,
        }
    }

    if let Err(error) = verify_trailers(state) {
        return Step::Fail(error);
    }

    if state.remaining == 0 {
        return Step::Done;
    }

    Step::Fail(Error::Incomplete)
}

/// Parses and verifies the buffered trailer block, publishing the headers.
fn verify_trailers(state: &mut State) -> Result<(), Error> {
    if state.trailer_buf.is_empty() || state.trailer_buf.as_slice() == b"\r\n" {
        return Ok(());
    }

    let parsed = parse_trailers(&state.trailer_buf, &state.limits)?;
    match state.signing.state() {
        Some(sign) => sign.verify_trailers(&parsed.canonical, parsed.signature.as_deref(), true)?,
        None if parsed.signature.is_some() => return Err(Error::FormatError),
        None => {}
    }
    state.handle.set(parsed.headers);

    Ok(())
}

/// Marks the stream as finished and reports one error item.
fn fail(phase: &mut Phase, error: Error) -> Poll<Option<Result<Bytes, Error>>> {
    *phase = Phase::Done;
    Poll::Ready(Some(Err(error)))
}

/// Applies the declared decoded length accounting to one produced fragment.
fn emit(bytes: Bytes, remaining: &mut usize, phase: &mut Phase) -> Poll<Option<Result<Bytes, Error>>> {
    if bytes.len() > *remaining {
        return fail(phase, Error::LengthMismatch);
    }
    *remaining -= bytes.len();
    Poll::Ready(Some(Ok(bytes)))
}

/// Selects the phase that follows a completed chunk.
fn finish_chunk(phase: &mut Phase, signed: bool) {
    *phase = if signed {
        Phase::Verify { trailers: false }
    } else {
        Phase::Meta
    };
}

fn poll_meta<S>(
    mut inner: Pin<&mut S>,
    carry: &mut Bytes,
    buf: &mut Vec<u8>,
    limit: usize,
    cx: &mut Context<'_>,
) -> Poll<Result<MetaOutcome, Error>>
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    loop {
        if let Some(index) = memchr::memchr(b'\n', carry.as_ref()) {
            let taken = carry.split_to(index + 1);
            let total = buf.len().saturating_add(taken.len());
            if total > limit {
                return Poll::Ready(Err(Error::ChunkMetaTooLarge(total, limit)));
            }
            buf.extend_from_slice(&taken);
            return Poll::Ready(Ok(MetaOutcome::Line));
        }

        if !carry.is_empty() {
            let total = buf.len().saturating_add(carry.len());
            if total > limit {
                return Poll::Ready(Err(Error::ChunkMetaTooLarge(total, limit)));
            }
            buf.extend_from_slice(carry.as_ref());
            *carry = Bytes::new();
        }

        match poll_fragment(inner.as_mut(), cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(Ok(MetaOutcome::End)),
            Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
            Poll::Ready(Some(Ok(bytes))) => *carry = bytes,
        }
    }
}

/// Polls one input fragment, mapping the stream error type.
fn poll_fragment<S>(mut inner: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>>
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    match inner.as_mut().poll_next(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(None) => Poll::Ready(None),
        Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
        Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(Error::Underlying(error)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::Item;

    use futures::StreamExt;

    fn idle_decoder() -> Pin<Box<Decoder<futures::stream::Empty<Item>>>> {
        Box::pin(Decoder::new(futures::stream::empty(), 0, Limits::default(), Signing::None))
    }

    #[test]
    fn verify_without_buffered_state_fails_closed() {
        let mut decoder = idle_decoder();
        decoder.state.phase = Phase::Verify { trailers: false };
        assert!(matches!(futures::executor::block_on(decoder.next()), Some(Err(Error::FormatError))));

        let mut decoder = idle_decoder();
        decoder.state.phase = Phase::Verify { trailers: false };
        decoder.state.chunk_signature = Some([0; 64]);
        assert!(matches!(futures::executor::block_on(decoder.next()), Some(Err(Error::FormatError))));
    }

    #[test]
    fn emitting_without_a_sign_state_fails_closed() {
        let mut decoder = idle_decoder();
        decoder.state.phase = Phase::Emitting { index: 0 };
        assert!(matches!(futures::executor::block_on(decoder.next()), Some(Err(Error::FormatError))));
    }
}

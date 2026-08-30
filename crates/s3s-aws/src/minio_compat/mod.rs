// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Compatibility shims for `MinIO`-specific response quirks.
//!
//! `MinIO` serializes some XML response bodies with Go's `encoding/xml`, which
//! renders booleans as uppercase `TRUE`/`FALSE` instead of the lowercase
//! `true`/`false` expected by the Rust AWS SDK (`aws-smithy-xml` only accepts
//! the lowercase forms). `GetBucketPolicyStatus` is the known offender:
//! `PolicyStatus.IsPublic` is emitted as `<IsPublic>FALSE</IsPublic>`, which
//! makes `aws-sdk-s3` fail deserialization and turns a perfectly fine 200
//! response into an error at the proxy layer.
//!
//! [`MinioBoolCompatInterceptor`] normalizes these elements back to lowercase
//! before the SDK parses the response body. The scan is cheap and only fires
//! on an exact element match, so non-MinIO backends (which already emit
//! lowercase) are unaffected. Other operations are skipped by input type, so
//! streaming payloads (e.g. `GetObject`) are never touched.

use aws_sdk_s3::operation::get_bucket_policy_status::GetBucketPolicyStatusInput;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::Intercept;
use aws_smithy_runtime_api::client::interceptors::context::{
    BeforeDeserializationInterceptorContextMut, BeforeSerializationInterceptorContextRef,
};
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::{ConfigBag, Storable, StoreReplace};
use http_body::Body;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[cfg(test)]
mod tests;

/// An `aws-sdk-s3` interceptor that normalizes `MinIO`'s uppercase boolean XML
/// elements to lowercase before response deserialization.
///
/// Mount it on the client configuration, e.g.:
///
/// ```ignore
/// aws_sdk_s3::config::Builder::from(&sdk_conf)
///     .interceptor(MinioBoolCompatInterceptor::new())
///     .build()
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct MinioBoolCompatInterceptor;

impl MinioBoolCompatInterceptor {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Marks the current operation as `GetBucketPolicyStatus` in the interceptor
/// state so that the deserialization hook can recognize it (the operation
/// input is consumed before that point).
#[derive(Debug, Clone, Copy)]
struct GetBucketPolicyStatusRequest;

impl Storable for GetBucketPolicyStatusRequest {
    type Storer = StoreReplace<Self>;
}

impl Intercept for MinioBoolCompatInterceptor {
    fn name(&self) -> &'static str {
        "minio_bool_compat"
    }

    fn read_before_serialization(
        &self,
        context: &BeforeSerializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        if context.input().downcast_ref::<GetBucketPolicyStatusInput>().is_some() {
            cfg.interceptor_state().store_put(GetBucketPolicyStatusRequest);
        }
        Ok(())
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        // Only `GetBucketPolicyStatus` is affected; every other operation
        // (including streaming payloads) is skipped without touching the
        // response body.
        if cfg.load::<GetBucketPolicyStatusRequest>().is_none() {
            return Ok(());
        }

        let response = context.response_mut();
        // Buffered body (e.g. on retry): normalize in place.
        if let Some(new_body) = normalize_minio_bools(response.body().bytes()) {
            *response.body_mut() = SdkBody::from(new_body);
            return Ok(());
        }
        // Streaming body: the orchestrator has not collected it yet, and all
        // interceptor hooks are synchronous. `GetBucketPolicyStatus` responses
        // are tiny XML documents (a few hundred bytes) that hyper has already
        // buffered in memory, so polling the stream synchronously with a
        // no-op waker completes without actually blocking. If the first frame
        // is not ready, we leave the body untouched and let the SDK handle it
        // (it would fail deserialization exactly as before this interceptor).
        let mut old_body = std::mem::replace(response.body_mut(), SdkBody::empty());
        match collect_body_sync(&mut old_body) {
            Ok(Some(bytes)) => {
                let body = normalize_minio_bools(Some(&bytes)).unwrap_or(bytes);
                *response.body_mut() = SdkBody::from(body);
            }
            Ok(None) => {
                // Not ready; put the (possibly partially consumed) body back.
                *response.body_mut() = old_body;
            }
            Err(err) => return Err(err),
        }
        Ok(())
    }
}

/// Collects a [`SdkBody`] into memory using synchronous polling.
/// Returns `Ok(Some(bytes))` when the stream has been fully read, `Ok(None)`
/// when the first frame was not ready (data not yet received), and `Err` on a
/// stream error. Only intended for tiny response bodies that are already
/// buffered by the HTTP client in memory.
fn collect_body_sync(body: &mut SdkBody) -> Result<Option<Vec<u8>>, BoxError> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut pinned = Pin::new(body);
    let mut out = Vec::new();
    loop {
        match pinned.as_mut().poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    out.extend_from_slice(&data);
                }
            }
            Poll::Ready(Some(Err(err))) => return Err(err),
            Poll::Ready(None) => return Ok(Some(out)),
            Poll::Pending => return Ok(None),
        }
    }
}

/// `<Name>TRUE<` / `<Name>FALSE<` → lowercase pairs for each XML element in
/// `MINIO_BOOL_ELEMENTS`. The trailing `<` anchors the match to the element
/// content only (a value like `<IsPublic>TRUEISH<` is left alone).
///
/// The element list is derived from the Smithy model's XML-body boolean
/// members, verified against a live `MinIO` server: currently only `IsPublic`
/// is affected (`IsTruncated` and friends are hand-written lowercase by
/// `MinIO`).
const BOOL_REPLACEMENTS: &[(&[u8], &[u8])] = &[
    (b"<IsPublic>TRUE<", b"<IsPublic>true<"),
    (b"<IsPublic>FALSE<", b"<IsPublic>false<"),
];

/// Replaces `MinIO`'s uppercase boolean element content with lowercase.
///
/// Returns `None` when nothing needs to change. A `None` body (streaming
/// response) is also `None`.
#[must_use]
pub fn normalize_minio_bools(body: Option<&[u8]>) -> Option<Vec<u8>> {
    let body = body?;
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    let mut changed = false;
    while !rest.is_empty() {
        // Pick the earliest match among all replacement pairs.
        let mut best: Option<(usize, &[u8], &[u8])> = None;
        for (needle, replacement) in BOOL_REPLACEMENTS {
            if let Some(offset) = find_subslice(rest, needle)
                && best.is_none_or(|(best_offset, _, _)| offset < best_offset)
            {
                best = Some((offset, needle, replacement));
            }
        }
        let Some((offset, needle, replacement)) = best else {
            out.extend_from_slice(rest);
            break;
        };
        out.extend_from_slice(&rest[..offset]);
        out.extend_from_slice(replacement);
        rest = &rest[offset + needle.len()..];
        changed = true;
    }
    changed.then_some(out)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

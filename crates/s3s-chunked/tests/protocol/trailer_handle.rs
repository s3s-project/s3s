// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `TrailerHandle` semantics observed through the public decoders.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::common::{body, drain, items, seed, sign_context};
use s3s_chunked::{ChunkedStream, Limits};

#[test]
fn handle_becomes_ready_after_consumption() {
    let wire = body(&[], false, Some((&[("x-amz-meta-a", "1")], false)));
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), 0, Limits::default());
    let handle = stream.trailer_handle();
    assert!(!handle.is_ready());
    let (payload, error) = drain(stream);
    assert_eq!(payload.len(), 0);
    assert!(error.is_none(), "{error:?}");
    assert!(handle.is_ready());
    assert_eq!(handle.read(http::HeaderMap::len), Some(1));
    assert_eq!(
        handle.read(|map| map.get("x-amz-meta-a").unwrap().to_str().unwrap().to_owned()),
        Some("1".to_owned())
    );
    assert!(handle.take().is_some());
    assert!(handle.take().is_none());
}

#[test]
fn handle_stays_empty_without_trailers() {
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[b"0\r\n\r\n"])), 0, Limits::default());
    let handle = stream.trailer_handle();
    let (_, error) = drain(stream);
    assert!(error.is_none());
    assert!(!handle.is_ready());
    assert!(handle.take().is_none());
    assert!(handle.read(|_| ()).is_none());
}

#[test]
fn duplicate_trailer_names_are_preserved() {
    let wire = body(&[], false, Some((&[("x-amz-meta-a", "1"), ("x-amz-meta-a", "2")], false)));
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), 0, Limits::default());
    let handle = stream.trailer_handle();
    let (_, error) = drain(stream);
    assert!(error.is_none(), "{error:?}");
    let values = handle
        .read(|map| {
            map.get_all("x-amz-meta-a")
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(values, ["1", "2"]);
}

#[test]
fn trailer_values_are_trimmed() {
    let wire = body(&[], false, Some((&[("x-amz-meta-a", "  hello \t")], false)));
    let stream = ChunkedStream::unsigned(futures::stream::iter(items(&[&wire])), 0, Limits::default());
    let handle = stream.trailer_handle();
    let (_, error) = drain(stream);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        handle.read(|map| map.get("x-amz-meta-a").unwrap().to_str().unwrap().to_owned()),
        Some("hello".to_owned())
    );
}

#[test]
fn signed_trailer_handle_is_exposed_after_verification() {
    let wire = body(&[b"hello".to_vec()], true, Some((&[("x-amz-meta-a", "1")], true)));
    let stream = ChunkedStream::signed(futures::stream::iter(items(&[&wire])), sign_context(), seed(), 5, Limits::default());
    let handle = stream.trailer_handle();
    let (payload, error) = drain(stream);
    assert_eq!(payload, b"hello");
    assert!(error.is_none(), "{error:?}");
    assert_eq!(handle.read(http::HeaderMap::len), Some(1));
}

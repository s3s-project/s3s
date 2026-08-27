// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for the synchronous syntax parsers and multipart form bodies.
//!
//! Input protocol: `data[0] % 4` selects the parser, `data[1..]` is the input:
//! - 0: SigV4 `Authorization` header (`AuthorizationV4::parse`)
//! - 1: SigV2 `Authorization` header (`AuthorizationV2::parse`)
//! - 2: ordered query string (`OrderedQs::parse`)
//! - 3: multipart/form-data body (`transform_multipart`)
//!
//! For selector 3, `data[1]` picks the boundary (low 2 bits) and optionally
//! tight limits (bit 2), `data[2..]` is the form body, fragmented
//! deterministically like the aws_chunked target.
//!
//! Oracle: parsing must never panic — every malformed input is a returned
//! error.

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use s3s::AuthorizationV2;
use s3s::AuthorizationV4;
use s3s::MultipartLimits;
use s3s::OrderedQs;
use s3s::StdError;
use s3s::transform_multipart;

fn split_fragments(payload: &[u8]) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let len = 1 + usize::from(payload[pos] % 16);
        let end = pos.saturating_add(len).min(payload.len());
        out.push(Bytes::copy_from_slice(&payload[pos..end]));
        pos = end;
    }
    out
}

fn fuzz_multipart(data: &[u8]) {
    let Some((&control, body)) = data.split_first() else { return };
    let boundary: &[u8] = match control & 0x03 {
        0 => b"BOUNDARY",
        1 => b"----WebKitFormBoundary7MA4YWxkTrZu0gW",
        2 => b"a",
        _ => b"0123456789",
    };
    let limits = if control & 0x04 != 0 {
        MultipartLimits {
            max_field_size: 64,
            max_fields_size: 256,
            max_parts: 8,
        }
    } else {
        MultipartLimits::default()
    };
    let items: Vec<Result<Bytes, StdError>> = split_fragments(body).into_iter().map(Ok).collect();
    futures::executor::block_on(async {
        let _ = transform_multipart(futures::stream::iter(items), boundary, limits, None).await;
    });
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else { return };
    match selector % 4 {
        0 => {
            if let Ok(s) = std::str::from_utf8(input) {
                let _ = AuthorizationV4::parse(s);
            }
        }
        1 => {
            if let Ok(s) = std::str::from_utf8(input) {
                let _ = AuthorizationV2::parse(s);
            }
        }
        2 => {
            if let Ok(s) = std::str::from_utf8(input) {
                let _ = OrderedQs::parse(s);
            }
        }
        _ => fuzz_multipart(input),
    }
});

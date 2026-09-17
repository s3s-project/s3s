// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Future-size budget for the dispatch path.
//!
//! The caps are the sizes measured when the budgets were last tightened,
//! rounded up to the next multiple of 100, so the guard keeps a margin instead
//! of matching exactly.

use super::*;
use crate::service::S3Service;
use stdx::mem::output_size;

#[test]
fn future_size() {
    // Guards against accidental future-size bloat in the dispatch path.
    macro_rules! future_size {
        ($f:path, $cap:expr) => {
            (stringify!($f), output_size(&$f), $cap)
        };
    }

    #[rustfmt::skip]
    let sizes = [
        future_size!(S3Service::call,                           3200),
        future_size!(call,                                      1800),
        future_size!(prepare,                                   1700),
        future_size!(SignatureContext::check,                    800),
        future_size!(SignatureContext::v2_check,                 300),
        future_size!(SignatureContext::v2_check_presigned_url,   200),
        future_size!(SignatureContext::v2_check_header_auth,     200),
        future_size!(SignatureContext::v4_check,                 800),
        future_size!(SignatureContext::v4_check_post_signature,  500),
        future_size!(SignatureContext::v4_check_presigned_url,   600),
        future_size!(SignatureContext::v4_check_header_auth,     700),
    ];

    println!("{sizes:#?}");
    for (name, size, cap) in sizes {
        assert!(size <= cap, "{name:?} size changed: cap {cap}, now {size}");
    }
}

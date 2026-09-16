// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Future-size budget for the dispatch path.

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
        future_size!(S3Service::call,                           3320),
        future_size!(call,                                      1920),
        future_size!(prepare,                                   1870),
        future_size!(SignatureContext::check,                    900),
        future_size!(SignatureContext::v2_check,                 290),
        future_size!(SignatureContext::v2_check_presigned_url,   140),
        future_size!(SignatureContext::v2_check_header_auth,     170),
        future_size!(SignatureContext::v4_check,                 780),
        future_size!(SignatureContext::v4_check_post_signature,  600),
        future_size!(SignatureContext::v4_check_presigned_url,   555),
        future_size!(SignatureContext::v4_check_header_auth,     665),
    ];

    println!("{sizes:#?}");
    for (name, size, cap) in sizes {
        assert!(size <= cap, "{name:?} size changed: cap {cap}, now {size}");
    }
}

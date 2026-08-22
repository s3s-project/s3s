// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use aws_smithy_types::body::SdkBody;

pub fn s3s_body_into_sdk_body(body: s3s::Body) -> SdkBody {
    SdkBody::from_body_1_x(body)
}

pub fn sdk_body_into_s3s_body(body: SdkBody) -> s3s::Body {
    s3s::Body::http_body(body)
}

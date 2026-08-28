// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#[cfg(feature = "minio")]
mod generated_minio;

#[cfg(not(feature = "minio"))]
mod generated;

#[cfg(feature = "minio")]
mod listen;

mod meta;

#[cfg(feature = "minio")]
use minio::s3::MinioClient;

pub struct Proxy(aws_sdk_s3::Client, #[cfg(feature = "minio")] MinioClient);

#[cfg(not(feature = "minio"))]
impl From<aws_sdk_s3::Client> for Proxy {
    fn from(value: aws_sdk_s3::Client) -> Self {
        Self(value)
    }
}

#[cfg(feature = "minio")]
impl Proxy {
    /// Create a proxy with an extra `MinIO` client used for `MinIO`-only extensions.
    #[must_use]
    pub fn new(client: aws_sdk_s3::Client, minio: MinioClient) -> Self {
        Self(client, minio)
    }
}

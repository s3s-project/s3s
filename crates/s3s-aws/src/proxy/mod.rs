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

/// An S3 service adapter that forwards requests through the AWS SDK for S3.
///
/// Build one via [`ProxyBuilder`].
pub struct Proxy {
    client: aws_sdk_s3::Client,
    #[cfg(feature = "minio")]
    minio: MinioClient,
}

impl Proxy {
    /// Returns a builder for a [`Proxy`].
    #[must_use]
    pub fn builder(client: aws_sdk_s3::Client) -> ProxyBuilder {
        ProxyBuilder {
            client,
            #[cfg(feature = "minio")]
            minio: None,
        }
    }
}

/// Builder for [`Proxy`].
pub struct ProxyBuilder {
    client: aws_sdk_s3::Client,
    #[cfg(feature = "minio")]
    minio: Option<MinioClient>,
}

impl ProxyBuilder {
    /// Set the `MinIO` client used for `MinIO`-only extensions.
    ///
    /// This method is only available with the `minio` feature.
    #[cfg(feature = "minio")]
    #[must_use]
    pub fn minio_client(mut self, minio: MinioClient) -> Self {
        self.minio = Some(minio);
        self
    }

    /// Build the [`Proxy`].
    ///
    /// With the `minio` feature, a `MinIO` client must have been set via
    /// [`Self::minio_client`]; a missing one is a programming error.
    #[must_use]
    pub fn build(self) -> Proxy {
        Proxy {
            client: self.client,
            #[cfg(feature = "minio")]
            minio: self.minio.expect("minio client is required with the minio feature"),
        }
    }
}

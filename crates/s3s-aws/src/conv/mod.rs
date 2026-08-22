// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod builtin;

#[cfg(feature = "minio")]
mod generated_minio;

#[cfg(not(feature = "minio"))]
mod generated;

use s3s::s3_error;
use s3s::{S3Error, S3Result};

pub trait AwsConversion: Sized {
    type Target;
    type Error;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error>;

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error>;
}

pub fn try_from_aws<T: AwsConversion>(x: T::Target) -> Result<T, T::Error> {
    T::try_from_aws(x)
}

pub fn try_into_aws<T: AwsConversion>(x: T) -> S3Result<T::Target, T::Error> {
    T::try_into_aws(x)
}

fn unwrap_from_aws<T: AwsConversion>(opt: Option<T::Target>, field_name: &str) -> S3Result<T>
where
    S3Error: From<T::Error>,
{
    match opt {
        Some(x) => T::try_from_aws(x).map_err(Into::into),
        None => Err(s3_error!(InternalError, "missing field: {}", field_name)),
    }
}

#[must_use]
pub fn string_from_integer(x: i32) -> String {
    x.to_string()
}

pub fn integer_from_string(x: &str) -> S3Result<i32> {
    x.parse::<i32>().map_err(S3Error::internal_error)
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod builtin;

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

/// Converts the SDK's `DateTime` (HTTP-date) back to the raw string kept by s3s.
pub fn expires_from_aws(x: Option<aws_sdk_s3::primitives::DateTime>) -> S3Result<Option<String>> {
    use aws_smithy_types::date_time::Format;
    match x {
        Some(v) => Ok(Some(v.fmt(Format::HttpDate).map_err(S3Error::internal_error)?)),
        None => Ok(None),
    }
}

/// Parses the raw s3s string into the SDK's `DateTime` (HTTP-date or RFC 3339).
pub fn expires_into_aws(x: Option<String>) -> S3Result<Option<aws_sdk_s3::primitives::DateTime>> {
    use aws_smithy_types::date_time::Format;
    match x {
        Some(s) => {
            let v = aws_sdk_s3::primitives::DateTime::from_str(&s, Format::HttpDate)
                .or_else(|_| aws_sdk_s3::primitives::DateTime::from_str(&s, Format::DateTime))
                .map_err(S3Error::internal_error)?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

use super::*;

use crate::body::{s3s_body_into_sdk_body, sdk_body_into_s3s_body};

use std::collections::HashMap;
use std::convert::Infallible;

macro_rules! identity_impl {
    ($($ty:ty),+) => {
        $(
            impl AwsConversion for $ty {
                type Target = $ty;
                type Error = Infallible;

                #[inline]
                fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
                    Ok(x)
                }

                #[inline]
                fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
                    Ok(x)
                }
            }
        )+
    };
}

identity_impl!(bool, i32, i64, String, HashMap<String, String>);

impl<T: AwsConversion> AwsConversion for Option<T> {
    type Target = Option<T::Target>;
    type Error = T::Error;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        x.map(try_from_aws).transpose()
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        x.map(try_into_aws).transpose()
    }
}

impl<T: AwsConversion> AwsConversion for Vec<T> {
    type Target = Vec<T::Target>;
    type Error = T::Error;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        x.into_iter().map(try_from_aws).collect()
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        x.into_iter().map(try_into_aws).collect()
    }
}

impl AwsConversion for s3s::dto::Timestamp {
    type Target = aws_sdk_s3::primitives::DateTime;
    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        use aws_smithy_types_convert::date_time::DateTimeExt;
        Ok(Self::from(x.to_time().map_err(S3Error::internal_error)?))
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        use aws_smithy_types_convert::date_time::DateTimeExt;
        Ok(aws_sdk_s3::primitives::DateTime::from_time(x.into()))
    }
}

impl AwsConversion for s3s::dto::CopySource {
    type Target = String;
    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        Self::parse(x.as_str()).map_err(S3Error::internal_error)
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        Ok(x.format_to_string())
    }
}

impl AwsConversion for s3s::dto::Range {
    type Target = String;
    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        Self::parse(x.as_str()).map_err(S3Error::internal_error)
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        Ok(x.to_header_string())
    }
}

impl AwsConversion for s3s::dto::Event {
    type Target = aws_sdk_s3::types::Event;
    type Error = Infallible;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        Ok(Self::from(x.as_str().to_owned()))
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        x.as_ref().parse()
    }
}

impl AwsConversion for s3s::dto::StreamingBlob {
    type Target = aws_sdk_s3::primitives::ByteStream;
    type Error = Infallible;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        // ByteStream -> SdkBody -> s3s::Body -> StreamingBlob
        Ok(sdk_body_into_s3s_body(x.into_inner()).into())
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        // StreamingBlob -> s3s::Body -> SdkBody -> ByteStream
        Ok(s3s_body_into_sdk_body(x.into()).into())
    }
}

impl AwsConversion for s3s::dto::Body {
    type Target = aws_sdk_s3::primitives::Blob;
    type Error = Infallible;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        Ok(x.into_inner().into())
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        Ok(Self::Target::new(x))
    }
}

impl AwsConversion for s3s::dto::SelectObjectContentInput {
    type Target = aws_sdk_s3::operation::select_object_content::SelectObjectContentInput;

    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> Result<Self, Self::Error> {
        Ok(Self {
            bucket: unwrap_from_aws(x.bucket, "bucket")?,
            expected_bucket_owner: x.expected_bucket_owner,
            key: unwrap_from_aws(x.key, "key")?,
            sse_customer_algorithm: x.sse_customer_algorithm,
            sse_customer_key: x.sse_customer_key,
            sse_customer_key_md5: x.sse_customer_key_md5,
            request: s3s::dto::SelectObjectContentRequest {
                expression: unwrap_from_aws(x.expression, "expression")?,
                expression_type: unwrap_from_aws(x.expression_type, "expression_type")?,
                input_serialization: unwrap_from_aws(x.input_serialization, "input_serialization")?,
                output_serialization: unwrap_from_aws(x.output_serialization, "output_serialization")?,
                request_progress: try_from_aws(x.request_progress)?,
                scan_range: try_from_aws(x.scan_range)?,
            },
        })
    }

    fn try_into_aws(x: Self) -> Result<Self::Target, Self::Error> {
        aws_sdk_s3::operation::select_object_content::SelectObjectContentInput::builder()
            .set_bucket(Some(x.bucket))
            .set_expected_bucket_owner(x.expected_bucket_owner)
            .set_key(Some(x.key))
            .set_sse_customer_algorithm(x.sse_customer_algorithm)
            .set_sse_customer_key(x.sse_customer_key)
            .set_sse_customer_key_md5(x.sse_customer_key_md5)
            .set_expression(Some(x.request.expression))
            .set_expression_type(Some(try_into_aws(x.request.expression_type)?))
            .set_input_serialization(Some(try_into_aws(x.request.input_serialization)?))
            .set_output_serialization(Some(try_into_aws(x.request.output_serialization)?))
            .set_request_progress(try_into_aws(x.request.request_progress)?)
            .set_scan_range(try_into_aws(x.request.scan_range)?)
            .build()
            .map_err(S3Error::internal_error)
    }
}

impl AwsConversion for s3s::dto::Tag {
    type Target = aws_sdk_s3::types::Tag;

    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        Ok(Self {
            key: Some(try_from_aws(x.key)?),
            value: Some(try_from_aws(x.value)?),
        })
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        let mut y = Self::Target::builder();
        y = y.set_key(try_into_aws(x.key)?);
        y = y.set_value(try_into_aws(x.value)?);
        y.build().map_err(S3Error::internal_error)
    }
}

impl AwsConversion for s3s::dto::ETag {
    type Target = String;

    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        Self::parse_http_header(x.as_bytes()).map_err(S3Error::internal_error)
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        Ok(format!("\"{}\"", x.value()))
    }
}

impl AwsConversion for s3s::dto::ETagCondition {
    type Target = String;

    type Error = S3Error;

    fn try_from_aws(x: Self::Target) -> S3Result<Self> {
        Self::parse_http_header(x.as_bytes()).map_err(S3Error::internal_error)
    }

    fn try_into_aws(x: Self) -> S3Result<Self::Target> {
        match x {
            s3s::dto::ETagCondition::ETag(etag) => Ok(format!("\"{}\"", etag.value())),
            s3s::dto::ETagCondition::Any => Ok("*".to_string()),
        }
    }
}

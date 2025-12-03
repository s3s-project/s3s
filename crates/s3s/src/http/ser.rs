use super::Body;
use super::Response;

use crate::StdError;
use crate::dto::SelectObjectContentEventStream;
use crate::dto::{Metadata, StreamingBlob, Timestamp, TimestampFormat};
use crate::error::{S3Error, S3Result};
use crate::http::KeepAliveBody;
use crate::http::{HeaderName, HeaderValue};
use crate::utils::format::fmt_timestamp;
use crate::xml;

use std::convert::Infallible;
use std::fmt::Write as _;

use hyper::header::{IntoHeaderName, InvalidHeaderValue};

// pub fn add_header<N, V>(res: &mut Response, name: N, value: V) -> S3Result
// where
//     N: IntoHeaderName,
//     V: TryIntoHeaderValue,
//     V::Error: std::error::Error + Send + Sync + 'static,
// {
//     let val = value.try_into_header_value().map_err(S3Error::internal_error)?;
//     res.headers.insert(name, val);
//     Ok(())
// }

pub fn add_opt_header<N, V>(res: &mut Response, name: N, value: Option<V>) -> S3Result
where
    N: IntoHeaderName,
    V: TryIntoHeaderValue,
    V::Error: std::error::Error + Send + Sync + 'static,
{
    if let Some(value) = value {
        let val = value.try_into_header_value().map_err(S3Error::internal_error)?;
        res.headers.insert(name, val);
    }
    Ok(())
}

pub fn add_opt_header_timestamp<N>(res: &mut Response, name: N, value: Option<Timestamp>, fmt: TimestampFormat) -> S3Result
where
    N: IntoHeaderName,
{
    if let Some(value) = value {
        let val = fmt_timestamp(&value, fmt, HeaderValue::from_bytes).map_err(S3Error::internal_error)?;
        res.headers.insert(name, val);
    }
    Ok(())
}

pub trait TryIntoHeaderValue {
    type Error;
    fn try_into_header_value(self) -> Result<HeaderValue, Self::Error>;
}

impl TryIntoHeaderValue for bool {
    type Error = Infallible;

    #[allow(clippy::declare_interior_mutable_const)]
    fn try_into_header_value(self) -> Result<HeaderValue, Self::Error> {
        const TRUE: HeaderValue = HeaderValue::from_static("true");
        const FALSE: HeaderValue = HeaderValue::from_static("false");
        Ok(if self { TRUE } else { FALSE })
    }
}

impl TryIntoHeaderValue for i32 {
    type Error = Infallible;

    fn try_into_header_value(self) -> Result<HeaderValue, Self::Error> {
        Ok(HeaderValue::from(self))
    }
}

impl TryIntoHeaderValue for i64 {
    type Error = Infallible;

    fn try_into_header_value(self) -> Result<HeaderValue, Self::Error> {
        Ok(HeaderValue::from(self))
    }
}

impl TryIntoHeaderValue for String {
    type Error = InvalidHeaderValue;

    fn try_into_header_value(self) -> Result<HeaderValue, Self::Error> {
        HeaderValue::try_from(self)
    }
}

/// See <https://github.com/hyperium/mime/issues/144>
#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_XML: HeaderValue = HeaderValue::from_static("application/xml");

pub fn set_xml_body<T: xml::Serialize>(res: &mut Response, val: &T) -> S3Result {
    let mut buf = Vec::with_capacity(256);
    {
        let mut ser = xml::Serializer::new(&mut buf);
        ser.decl()
            .and_then(|()| val.serialize(&mut ser))
            .map_err(S3Error::internal_error)?;
    }
    res.body = Body::from(buf);
    res.headers.insert(hyper::header::CONTENT_TYPE, APPLICATION_XML);
    Ok(())
}

#[allow(clippy::declare_interior_mutable_const)]
const TRANSFER_ENCODING_CHUNKED: HeaderValue = HeaderValue::from_static("chunked");

pub fn set_keep_alive_xml_body(
    res: &mut Response,
    fut: impl std::future::Future<Output = Result<Response, StdError>> + Send + Sync + 'static,
    duration: std::time::Duration,
) -> S3Result {
    let mut buf = Vec::with_capacity(40);
    let mut ser = xml::Serializer::new(&mut buf);
    ser.decl().map_err(S3Error::internal_error)?;

    res.body = Body::http_body(KeepAliveBody::new(fut, duration, Some(buf.into()), false));
    res.headers.insert(hyper::header::CONTENT_TYPE, APPLICATION_XML);
    res.headers
        .insert(hyper::header::TRANSFER_ENCODING, TRANSFER_ENCODING_CHUNKED);
    Ok(())
}

pub fn set_xml_body_no_decl<T: xml::Serialize>(res: &mut Response, val: &T) -> S3Result {
    let mut buf = Vec::with_capacity(256);
    let mut ser = xml::Serializer::new(&mut buf);
    val.serialize(&mut ser).map_err(S3Error::internal_error)?;
    res.body = Body::from(buf);
    res.headers.insert(hyper::header::CONTENT_TYPE, APPLICATION_XML);
    Ok(())
}

pub fn set_stream_body(res: &mut Response, stream: StreamingBlob) {
    res.body = Body::from(stream);
}

pub fn set_event_stream_body(res: &mut Response, stream: SelectObjectContentEventStream) {
    res.body = Body::from(stream.into_byte_stream());
    res.headers
        .insert(hyper::header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
}

pub fn add_opt_metadata(res: &mut Response, metadata: Option<Metadata>) -> S3Result {
    if let Some(map) = metadata {
        let mut buf = String::new();
        for (key, val) in map {
            write!(&mut buf, "x-amz-meta-{key}").unwrap();
            let name = HeaderName::from_bytes(buf.as_bytes()).map_err(S3Error::internal_error)?;
            let value = HeaderValue::try_from(val).map_err(S3Error::internal_error)?;
            res.headers.insert(name, value);
            buf.clear();
        }
    }
    Ok(())
}

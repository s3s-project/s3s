use super::{Multipart, Request};

use crate::dto::{List, Metadata, StreamingBlob, Timestamp, TimestampFormat};
use crate::error::*;
use crate::http::{HeaderName, HeaderValue};
use crate::path::S3Path;
use crate::xml;

use std::fmt;
use std::str::FromStr;

use stdx::string::StringExt;
use tracing::{debug, error};

fn missing_header(name: &HeaderName) -> S3Error {
    invalid_request!("missing header: {}", name.as_str())
}

fn duplicate_header(name: &HeaderName) -> S3Error {
    invalid_request!("duplicate header: {}", name.as_str())
}

fn invalid_header<E>(source: E, name: &HeaderName, val: impl fmt::Debug) -> S3Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    s3_error!(source, InvalidArgument, "invalid header: {}: {:?}", name.as_str(), val)
}

fn get_required_header<'r>(req: &'r Request, name: &HeaderName) -> S3Result<&'r HeaderValue> {
    let mut iter = req.headers.get_all(name).into_iter();
    let Some(val) = iter.next() else { return Err(missing_header(name)) };
    let None = iter.next() else { return Err(duplicate_header(name)) };

    if val.is_empty() {
        return Err(missing_header(name));
    }

    Ok(val)
}

fn get_optional_header<'r>(req: &'r Request, name: &HeaderName) -> S3Result<Option<&'r HeaderValue>> {
    let mut iter = req.headers.get_all(name).into_iter();
    let Some(val) = iter.next() else { return Ok(None) };
    let None = iter.next() else { return Err(duplicate_header(name)) };

    if val.is_empty() {
        return Ok(None);
    }

    Ok(Some(val))
}

pub fn parse_header<T>(req: &Request, name: &HeaderName) -> S3Result<T>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let val = get_required_header(req, name)?;

    T::try_from_header_value(val).map_err(|err| invalid_header(err, name, val))
}

pub fn parse_opt_header<T>(req: &Request, name: &HeaderName) -> S3Result<Option<T>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let Some(val) = get_optional_header(req, name)? else { return Ok(None) };

    match T::try_from_header_value(val) {
        Ok(ans) => Ok(Some(ans)),
        Err(err) => Err(invalid_header(err, name, val)),
    }
}

pub fn parse_checksum_algorithm_header(req: &Request) -> S3Result<Option<crate::dto::ChecksumAlgorithm>> {
    let ans: Option<crate::dto::ChecksumAlgorithm> = parse_opt_header(req, &crate::header::X_AMZ_CHECKSUM_ALGORITHM)?;

    if ans.is_some() {
        return Ok(ans);
    }

    let Some(trailer) = req.headers.get("x-amz-trailer") else {
        return Ok(None);
    };

    let mapping = &const {
        [
            (crate::header::X_AMZ_CHECKSUM_CRC32, crate::dto::ChecksumAlgorithm::CRC32),
            (crate::header::X_AMZ_CHECKSUM_CRC32C, crate::dto::ChecksumAlgorithm::CRC32C),
            (crate::header::X_AMZ_CHECKSUM_SHA1, crate::dto::ChecksumAlgorithm::SHA1),
            (crate::header::X_AMZ_CHECKSUM_SHA256, crate::dto::ChecksumAlgorithm::SHA256),
            (crate::header::X_AMZ_CHECKSUM_CRC64NVME, crate::dto::ChecksumAlgorithm::CRC64NVME),
        ]
    };

    for (h, v) in mapping {
        if trailer.as_bytes() == h.as_str().as_bytes() {
            return Ok(Some(crate::dto::ChecksumAlgorithm::from_static(v)));
        }
    }

    Ok(None)
}

pub fn parse_opt_header_timestamp(req: &Request, name: &HeaderName, fmt: TimestampFormat) -> S3Result<Option<Timestamp>> {
    let Some(val) = get_optional_header(req, name)? else { return Ok(None) };

    let s = val.to_str().map_err(|err| invalid_header(err, name, val))?;
    match Timestamp::parse(fmt, s) {
        Ok(ans) => Ok(Some(ans)),
        Err(err) => Err(invalid_header(err, name, val)),
    }
}

pub fn parse_list_header<T>(req: &Request, name: &HeaderName) -> S3Result<List<T>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let mut list = List::new();
    for val in req.headers.get_all(name) {
        let ans = T::try_from_header_value(val).map_err(|err| invalid_header(err, name, val))?;
        list.push(ans);
    }
    if list.is_empty() {
        return Err(missing_header(name));
    }
    Ok(list)
}

pub fn parse_opt_list_header<T>(req: &Request, name: &HeaderName) -> S3Result<Option<List<T>>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let mut list = List::new();
    for val in req.headers.get_all(name) {
        let ans = T::try_from_header_value(val).map_err(|err| invalid_header(err, name, val))?;
        list.push(ans);
    }
    if list.is_empty() {
        return Ok(None);
    }
    Ok(Some(list))
}

fn missing_query(name: &str) -> S3Error {
    invalid_request!("missing query: {}", name)
}

fn duplicate_query(name: &str) -> S3Error {
    invalid_request!("duplicate query: {}", name)
}

fn invalid_query<E>(source: E, name: &str, val: &str) -> S3Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    s3_error!(source, InvalidArgument, "invalid query: {}: {}", name, val)
}

pub fn parse_query<T: FromStr>(req: &Request, name: &str) -> S3Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let Some(qs) = req.s3ext.qs.as_ref() else { return Err(missing_query(name)) };

    let mut iter = qs.get_all(name);
    let Some(val) = iter.next() else { return Err(missing_query(name)) };
    let None = iter.next() else { return Err(duplicate_query(name)) };

    val.parse::<T>().map_err(|err| invalid_query(err, name, val))
}

pub fn parse_opt_query<T: FromStr>(req: &Request, name: &str) -> S3Result<Option<T>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let Some(qs) = req.s3ext.qs.as_ref() else { return Ok(None) };

    let mut iter = qs.get_all(name);
    let Some(val) = iter.next() else { return Ok(None) };
    let None = iter.next() else { return Err(duplicate_query(name)) };

    Ok(Some(val.parse::<T>().map_err(|err| invalid_query(err, name, val))?))
}

pub fn parse_opt_query_timestamp(req: &Request, name: &str, fmt: TimestampFormat) -> S3Result<Option<Timestamp>> {
    let Some(qs) = req.s3ext.qs.as_ref() else { return Ok(None) };

    let mut iter = qs.get_all(name);
    let Some(val) = iter.next() else { return Ok(None) };
    let None = iter.next() else { return Err(duplicate_query(name)) };

    Ok(Some(Timestamp::parse(fmt, val).map_err(|err| invalid_query(err, name, val))?))
}

#[track_caller]
pub fn unwrap_bucket(req: &mut Request) -> String {
    match req.s3ext.s3_path.take() {
        Some(S3Path::Bucket { bucket }) => bucket.into(),
        _ => panic!("s3 path not found, expected bucket"),
    }
}

#[track_caller]
pub fn unwrap_object(req: &mut Request) -> (String, String) {
    match req.s3ext.s3_path.take() {
        Some(S3Path::Object { bucket, key }) => (bucket.into(), key.into()),
        _ => panic!("s3 path not found, expected object"),
    }
}

fn malformed_xml(source: xml::DeError) -> S3Error {
    S3Error::with_source(S3ErrorCode::MalformedXML, Box::new(source))
}

fn deserialize_xml<T>(bytes: &[u8]) -> S3Result<T>
where
    T: for<'xml> xml::Deserialize<'xml>,
{
    let mut d = xml::Deserializer::new(bytes);
    let ans = T::deserialize(&mut d).map_err(malformed_xml)?;
    d.expect_eof().map_err(malformed_xml)?;
    Ok(ans)
}

pub fn take_xml_body<T>(req: &mut Request) -> S3Result<T>
where
    T: for<'xml> xml::Deserialize<'xml>,
{
    let bytes = req.body.take_bytes().expect("full body not found");
    if bytes.is_empty() {
        return Err(S3ErrorCode::MissingRequestBodyError.into());
    }
    let result = deserialize_xml(&bytes);
    if result.is_err() {
        error!(?bytes, "malformed xml body");
    }
    result
}

pub fn take_opt_xml_body<T>(req: &mut Request) -> S3Result<Option<T>>
where
    T: for<'xml> xml::Deserialize<'xml>,
{
    let bytes = req.body.take_bytes().expect("full body not found");
    if bytes.is_empty() {
        return Ok(None);
    }
    let result = deserialize_xml(&bytes).map(Some);
    if result.is_err() {
        error!(?bytes, "malformed xml body");
    }
    result
}

pub fn take_string_body(req: &mut Request) -> S3Result<String> {
    let bytes = req.body.take_bytes().expect("full body not found");
    match String::from_utf8_simd(bytes.into()) {
        Ok(s) => Ok(s),
        Err(_) => Err(invalid_request!("expected UTF-8 body")),
    }
}

pub fn take_stream_body(req: &mut Request) -> StreamingBlob {
    let body = std::mem::take(&mut req.body);
    let size_hint = http_body::Body::size_hint(&body);
    debug!(?size_hint, "taking streaming blob");
    StreamingBlob::from(body)
}

pub fn parse_opt_metadata(req: &Request) -> S3Result<Option<Metadata>> {
    let mut metadata = Metadata::default();
    let map = &req.headers;
    for name in map.keys() {
        let Some(key) = name.as_str().strip_prefix("x-amz-meta-") else { continue };
        if key.is_empty() {
            continue;
        }
        let mut iter = map.get_all(name).into_iter();
        let val = iter.next().unwrap();
        let None = iter.next() else { return Err(duplicate_header(name)) };

        let val = val.to_str().map_err(|err| invalid_header(err, name, val))?;
        metadata.insert(key.into(), val.into());
    }
    if metadata.is_empty() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

pub trait TryFromHeaderValue: Sized {
    type Error;
    fn try_from_header_value(val: &HeaderValue) -> Result<Self, Self::Error>;
}

#[derive(Debug, thiserror::Error)]
pub enum ParseHeaderError {
    #[error("Invalid boolean value")]
    Boolean,

    #[error("Invalid integer value")]
    Integer,

    #[error("Invalid long value")]
    Long,

    #[error("Invalid enum value")]
    Enum,

    #[error("Invalid string value")]
    String,
}

impl TryFromHeaderValue for bool {
    type Error = ParseHeaderError;

    fn try_from_header_value(val: &HeaderValue) -> Result<Self, Self::Error> {
        match val.as_bytes() {
            b"true" | b"True" => Ok(true),
            b"false" | b"False" => Ok(false),
            _ => Err(ParseHeaderError::Boolean),
        }
    }
}

impl TryFromHeaderValue for i32 {
    type Error = ParseHeaderError;

    fn try_from_header_value(val: &HeaderValue) -> Result<Self, Self::Error> {
        atoi::atoi(val.as_bytes()).ok_or(ParseHeaderError::Integer)
    }
}

impl TryFromHeaderValue for i64 {
    type Error = ParseHeaderError;

    fn try_from_header_value(val: &HeaderValue) -> Result<Self, Self::Error> {
        atoi::atoi(val.as_bytes()).ok_or(ParseHeaderError::Long)
    }
}

impl TryFromHeaderValue for String {
    type Error = ParseHeaderError;

    fn try_from_header_value(val: &HeaderValue) -> Result<Self, Self::Error> {
        match val.to_str() {
            Ok(s) => Ok(s.to_owned()),
            Err(_) => Err(ParseHeaderError::String),
        }
    }
}

pub fn parse_field_value<T>(m: &Multipart, name: &str) -> S3Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let Some(val) = m.find_field_value(name) else { return Ok(None) };
    match val.parse() {
        Ok(ans) => Ok(Some(ans)),
        Err(source) => Err(s3_error!(source, InvalidArgument, "invalid field value: {}: {:?}", name, val)),
    }
}

pub fn parse_field_value_timestamp(m: &Multipart, name: &str, fmt: TimestampFormat) -> S3Result<Option<Timestamp>> {
    let Some(val) = m.find_field_value(name) else { return Ok(None) };
    match Timestamp::parse(fmt, val) {
        Ok(ans) => Ok(Some(ans)),
        Err(source) => Err(s3_error!(source, InvalidArgument, "invalid field value: {}: {:?}", name, val)),
    }
}

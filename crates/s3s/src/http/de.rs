// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

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

/// Parses the `Range` header for `GetObject` and `HeadObject`.
///
/// A `Range` field that cannot be served as a single byte range is ignored
/// instead of rejected, matching Amazon S3 (`200 OK` with the whole object).
/// Only the first field line is used, as S3 and `MinIO` do; an empty value
/// counts as absent and a value that is not UTF-8 is ignored.
pub(crate) fn parse_opt_range_header(req: &Request) -> Option<crate::dto::Range> {
    let val = req.headers.get_all(crate::header::RANGE).iter().next()?;

    if val.is_empty() {
        return None;
    }

    let header = val.to_str().ok()?;

    crate::dto::parse_lenient(header)
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

    let trailer_name = HeaderName::from_static("x-amz-trailer");
    let Some(trailer_value) = get_optional_header(req, &trailer_name)? else {
        return Ok(None);
    };
    let trailer = trailer_value
        .to_str()
        .map_err(|err| invalid_header(err, &trailer_name, trailer_value))?;

    let mapping = &const {
        [
            (crate::header::X_AMZ_CHECKSUM_CRC32, crate::dto::ChecksumAlgorithm::CRC32),
            (crate::header::X_AMZ_CHECKSUM_CRC32C, crate::dto::ChecksumAlgorithm::CRC32C),
            (crate::header::X_AMZ_CHECKSUM_SHA1, crate::dto::ChecksumAlgorithm::SHA1),
            (crate::header::X_AMZ_CHECKSUM_SHA256, crate::dto::ChecksumAlgorithm::SHA256),
            (crate::header::X_AMZ_CHECKSUM_CRC64NVME, crate::dto::ChecksumAlgorithm::CRC64NVME),
            (crate::header::X_AMZ_CHECKSUM_SHA512, crate::dto::ChecksumAlgorithm::SHA512),
            (crate::header::X_AMZ_CHECKSUM_MD5, crate::dto::ChecksumAlgorithm::MD5),
            (crate::header::X_AMZ_CHECKSUM_XXHASH64, crate::dto::ChecksumAlgorithm::XXHASH64),
            (crate::header::X_AMZ_CHECKSUM_XXHASH3, crate::dto::ChecksumAlgorithm::XXHASH3),
            (crate::header::X_AMZ_CHECKSUM_XXHASH128, crate::dto::ChecksumAlgorithm::XXHASH128),
        ]
    };

    let mut ans = None;
    for trailer_field in trailer.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        for (h, v) in mapping {
            if trailer_field.eq_ignore_ascii_case(h.as_str()) {
                if ans.replace(*v).is_some() {
                    return Err(invalid_header(ParseHeaderError::Enum, &trailer_name, trailer));
                }
                break;
            }
        }
    }

    Ok(ans.map(crate::dto::ChecksumAlgorithm::from_static))
}

/// Whether `name` is one of the auxiliary `x-amz-checksum-*` headers, which do
/// not name a checksum algorithm.
///
/// The names are the generated header constants from [`crate::header`].
fn is_checksum_aux_header(name: &HeaderName) -> bool {
    [
        crate::header::X_AMZ_CHECKSUM_ALGORITHM,
        crate::header::X_AMZ_CHECKSUM_MODE,
        crate::header::X_AMZ_CHECKSUM_TYPE,
    ]
    .contains(name)
}

/// Whether `name` is `x-amz-checksum-<algorithm>` for a modelled algorithm, one per
/// variant of [`crate::dto::ChecksumAlgorithm`].
///
/// The names are the generated header constants from [`crate::header`]: a new checksum
/// algorithm adds a constant there and has to be listed here as well, because the model
/// type is a string newtype, so the list cannot be derived from it and nothing else
/// would notice the omission.
fn is_checksum_algorithm_header(name: &HeaderName) -> bool {
    [
        crate::header::X_AMZ_CHECKSUM_CRC32,
        crate::header::X_AMZ_CHECKSUM_CRC32C,
        crate::header::X_AMZ_CHECKSUM_CRC64NVME,
        crate::header::X_AMZ_CHECKSUM_MD5,
        crate::header::X_AMZ_CHECKSUM_SHA1,
        crate::header::X_AMZ_CHECKSUM_SHA256,
        crate::header::X_AMZ_CHECKSUM_SHA512,
        crate::header::X_AMZ_CHECKSUM_XXHASH3,
        crate::header::X_AMZ_CHECKSUM_XXHASH64,
        crate::header::X_AMZ_CHECKSUM_XXHASH128,
    ]
    .contains(name)
}

/// Rejects the `x-amz-checksum-*` headers the service refuses.
///
/// The order is the one the service uses: a header name that carries several
/// values is rejected first, then a request that names more than one checksum
/// algorithm, then one that names an algorithm outside the model's
/// [`crate::dto::ChecksumAlgorithm`] enumeration. The auxiliary
/// `x-amz-checksum-algorithm` / `-mode` / `-type` headers are not algorithm
/// headers.
pub fn validate_checksum_headers(req: &Request) -> S3Result<()> {
    let mut algorithm_headers = 0_usize;
    let mut unknown_algorithm = false;

    for name in req.headers.keys() {
        if !name.as_str().starts_with("x-amz-checksum-") {
            continue;
        }

        if req.headers.get_all(name).iter().count() > 1 {
            return Err(s3_error!(InvalidArgument, "Only one value may be specified."));
        }

        if is_checksum_aux_header(name) {
            continue;
        }

        algorithm_headers += 1;
        unknown_algorithm |= !is_checksum_algorithm_header(name);
    }

    if algorithm_headers > 1 {
        return Err(s3_error!(
            InvalidRequest,
            "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed."
        ));
    }

    if unknown_algorithm {
        return Err(s3_error!(
            InvalidRequest,
            "The algorithm type you specified in x-amz-checksum- header is invalid."
        ));
    }

    Ok(())
}

/// Rejects an empty `x-amz-expected-bucket-owner` header.
///
/// The header is optional, but a present header must carry an account ID: an
/// empty value is invalid rather than absent. The message is the one the service
/// returns, including the trailing `... []` it appends.
pub fn validate_expected_bucket_owner(req: &Request) -> S3Result<()> {
    let mut values = req.headers.get_all(&crate::header::X_AMZ_EXPECTED_BUCKET_OWNER).iter();
    if values.next().is_some_and(HeaderValue::is_empty) {
        return Err(s3_error!(
            InvalidBucketOwnerAWSAccountID,
            "The value of the expected bucket owner parameter must be an AWS Account ID... []"
        ));
    }

    Ok(())
}

pub fn parse_opt_header_timestamp(req: &Request, name: &HeaderName, fmt: TimestampFormat) -> S3Result<Option<Timestamp>> {
    let Some(val) = get_optional_header(req, name)? else { return Ok(None) };

    let s = val.to_str().map_err(|err| invalid_header(err, name, val))?;
    match Timestamp::parse(fmt, s) {
        Ok(ans) => Ok(Some(ans)),
        Err(err) => Err(invalid_header(err, name, val)),
    }
}

/// Returns the first value of an optional header, ignoring any further values.
///
/// The conditional date fields read a repeated field as its first value: the service evaluates the
/// condition carried by the first field line (measured: a repeated `If-Unmodified-Since` whose
/// first value is older than the object answers 412), so a repeated header is not an error here.
fn get_optional_header_first_value<'r>(req: &'r Request, name: &HeaderName) -> Option<&'r HeaderValue> {
    let val = req.headers.get(name)?;

    if val.is_empty() {
        return None;
    }

    Some(val)
}

/// Parses an optional header timestamp, ignoring a value that cannot be used as a date in `fmt`.
///
/// The conditional date fields use this parser instead of [`parse_opt_header_timestamp`]: a value
/// that is not a valid HTTP-date is ignored (RFC 9110 §13.1.3, §13.1.4), and so is a value that
/// cannot be decoded as UTF-8, because neither can carry a valid HTTP-date. A repeated field is
/// read as its first value, the way the service reads it. The same rules apply to the copy-source
/// and rename-source conditions.
///
/// Nothing here can fail, so the generated caller takes the value without a `?`, the way it takes
/// [`parse_opt_range_header`].
pub fn parse_opt_header_timestamp_ignoring_invalid(req: &Request, name: &HeaderName, fmt: TimestampFormat) -> Option<Timestamp> {
    let val = get_optional_header_first_value(req, name)?;
    let s = val.to_str().ok()?;

    Timestamp::parse(fmt, s).ok()
}

/// Parses a required list header: one whose values are elements separated by commas.
///
/// A list header may be split over several field lines, and one field line may carry several
/// elements; the two forms mean the same list (RFC 9110 §5.6.1), so both are read. The elements
/// are split on commas with the optional whitespace around them removed, and the empty ones are
/// skipped, so a field that carries no element counts as a missing header.
pub fn parse_list_header<T>(req: &Request, name: &HeaderName) -> S3Result<List<T>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let list = parse_list_header_elements(req, name)?;

    if list.is_empty() {
        return Err(missing_header(name));
    }

    Ok(list)
}

/// Parses an optional list header; see [`parse_list_header`] for the element syntax.
///
/// A header whose values carry no element counts as absent, the way [`parse_opt_header`] reads a
/// present but empty single-valued header.
pub fn parse_opt_list_header<T>(req: &Request, name: &HeaderName) -> S3Result<Option<List<T>>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let list = parse_list_header_elements(req, name)?;

    if list.is_empty() {
        return Ok(None);
    }

    Ok(Some(list))
}

/// Reads every element of a list header, in field order.
///
/// Each field line is decoded as UTF-8 and split on commas. A field line that cannot be decoded,
/// or an element the element type refuses, is rejected as an invalid header carrying the name of
/// the header it came from.
fn parse_list_header_elements<T>(req: &Request, name: &HeaderName) -> S3Result<List<T>>
where
    T: TryFromHeaderValue,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let mut list = List::new();

    for val in req.headers.get_all(name) {
        let value = val.to_str().map_err(|err| invalid_header(err, name, val))?;

        let elements = value
            .split(',')
            .map(|element| element.trim_matches([' ', '\t']))
            .filter(|element| !element.is_empty());

        for element in elements {
            let element = HeaderValue::from_bytes(element.as_bytes()).map_err(|err| invalid_header(err, name, val))?;
            let ans = T::try_from_header_value(&element).map_err(|err| invalid_header(err, name, val))?;
            list.push(ans);
        }
    }

    Ok(list)
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

/// Like [`parse_opt_query`], but joins repeated query values with
/// `separator` instead of rejecting them.
pub fn parse_opt_query_joined(req: &Request, name: &str, separator: &str) -> Option<String> {
    let qs = req.s3ext.qs.as_ref()?;

    let mut iter = qs.get_all(name);
    let first = iter.next()?;

    let mut joined = String::new();
    joined.push_str(first);
    for value in iter {
        joined.push_str(separator);
        joined.push_str(value);
    }
    Some(joined)
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

/// `MinIO` compatibility: types that can be constructed from a bare body literal.
///
/// `MinIO` reference:
/// - <https://github.com/minio/minio/blob/7aac2a2c5b7c882e68c1ce017d8256be2feea27f/internal/bucket/object/lock/lock.go#L232-L319>
/// - <https://github.com/minio/minio/blob/7aac2a2c5b7c882e68c1ce017d8256be2feea27f/internal/bucket/versioning/versioning.go#L49-L84>
#[cfg(feature = "minio")]
pub(crate) trait BodyLiteral: Sized + for<'xml> crate::xml::Deserialize<'xml> {
    /// Construct this type from the matched body literal value.
    fn from_body_literal(literal: &str) -> Self;
}

#[cfg(feature = "minio")]
impl BodyLiteral for crate::dto::ObjectLockConfiguration {
    fn from_body_literal(literal: &str) -> Self {
        use crate::dto::ObjectLockEnabled;
        crate::dto::ObjectLockConfiguration {
            object_lock_enabled: Some(ObjectLockEnabled::from(literal.to_owned())),
            rule: None,
        }
    }
}

#[cfg(feature = "minio")]
impl BodyLiteral for crate::dto::VersioningConfiguration {
    fn from_body_literal(literal: &str) -> Self {
        use crate::dto::BucketVersioningStatus;
        crate::dto::VersioningConfiguration {
            status: Some(BucketVersioningStatus::from(literal.to_owned())),
            ..Default::default()
        }
    }
}

/// `MinIO` compatibility: accept a bare body literal or full XML (optional field).
#[cfg(feature = "minio")]
pub(crate) fn take_opt_body_literal<T: BodyLiteral>(req: &mut Request, literal: &str) -> S3Result<Option<T>> {
    debug_assert!(!literal.is_empty(), "bodyLiteral value must not be empty");
    let bytes = req.body.take_bytes().expect("full body not found");
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.trim_ascii() == literal.as_bytes() {
        return Ok(Some(T::from_body_literal(literal)));
    }
    let result = deserialize_xml::<T>(&bytes).map(Some);
    if result.is_err() {
        error!(?bytes, "malformed xml body");
    }
    result
}

/// `MinIO` compatibility: accept a bare body literal or full XML (required field).
#[cfg(feature = "minio")]
pub(crate) fn take_body_literal<T: BodyLiteral>(req: &mut Request, literal: &str) -> S3Result<T> {
    debug_assert!(!literal.is_empty(), "bodyLiteral value must not be empty");
    let bytes = req.body.take_bytes().expect("full body not found");
    if bytes.is_empty() {
        return Err(S3ErrorCode::MissingRequestBodyError.into());
    }
    if bytes.trim_ascii() == literal.as_bytes() {
        return Ok(T::from_body_literal(literal));
    }
    let result = deserialize_xml::<T>(&bytes);
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

        let raw = std::str::from_utf8(val.as_bytes()).map_err(|err| invalid_header(err, name, val))?;
        let val = s3s_rfc2047::decode(raw).map_err(|err| invalid_header(err, name, val))?;
        metadata.insert(key.into(), val.into_owned());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::TimestampFormat;
    use crate::http::multipart::File;
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;
    use crate::stream::ByteStream;
    use bytes::Bytes;

    fn make_request() -> Request {
        Request {
            version: http::Version::HTTP_11,
            method: hyper::Method::GET,
            uri: hyper::Uri::from_static("http://example.com"),
            headers: hyper::HeaderMap::new(),
            extensions: hyper::http::Extensions::default(),
            body: Body::empty(),
            s3ext: super::super::request::S3Extensions::default(),
        }
    }

    // --- TryFromHeaderValue tests ---

    #[test]
    fn try_from_header_value_bool() {
        use crate::http::HeaderValue;
        assert!(bool::try_from_header_value(&HeaderValue::from_static("true")).unwrap());
        assert!(bool::try_from_header_value(&HeaderValue::from_static("True")).unwrap());
        assert!(!bool::try_from_header_value(&HeaderValue::from_static("false")).unwrap());
        assert!(!bool::try_from_header_value(&HeaderValue::from_static("False")).unwrap());
        assert!(bool::try_from_header_value(&HeaderValue::from_static("invalid")).is_err());
    }

    #[test]
    fn try_from_header_value_i32() {
        use crate::http::HeaderValue;
        assert_eq!(i32::try_from_header_value(&HeaderValue::from_static("42")).unwrap(), 42);
        assert_eq!(i32::try_from_header_value(&HeaderValue::from_static("-1")).unwrap(), -1);
        assert!(i32::try_from_header_value(&HeaderValue::from_static("abc")).is_err());
    }

    #[test]
    fn try_from_header_value_i64() {
        use crate::http::HeaderValue;
        assert_eq!(
            i64::try_from_header_value(&HeaderValue::from_static("9876543210")).unwrap(),
            9_876_543_210_i64
        );
        assert!(i64::try_from_header_value(&HeaderValue::from_static("abc")).is_err());
    }

    #[test]
    fn try_from_header_value_string() {
        use crate::http::HeaderValue;
        assert_eq!(String::try_from_header_value(&HeaderValue::from_static("hello")).unwrap(), "hello");
    }

    // --- parse_header / parse_opt_header tests ---

    #[test]
    fn parse_header_present() {
        let mut req = make_request();
        req.headers.insert("x-custom", "42".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let val: i32 = parse_header(&req, &name).unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn parse_header_missing() {
        let req = make_request();
        let name = HeaderName::from_static("x-custom");
        let result: S3Result<i32> = parse_header(&req, &name);
        assert!(result.is_err());
    }

    #[test]
    fn parse_header_empty_value() {
        let mut req = make_request();
        req.headers.insert("x-custom", "".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let result: S3Result<i32> = parse_header(&req, &name);
        assert!(result.is_err());
    }

    #[test]
    fn parse_header_duplicate() {
        let mut req = make_request();
        req.headers.append("x-custom", "1".parse().unwrap());
        req.headers.append("x-custom", "2".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let result: S3Result<i32> = parse_header(&req, &name);
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_header_present() {
        let mut req = make_request();
        req.headers.insert("x-custom", "hello".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let val: Option<String> = parse_opt_header(&req, &name).unwrap();
        assert_eq!(val.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_opt_header_missing() {
        let req = make_request();
        let name = HeaderName::from_static("x-custom");
        let val: Option<String> = parse_opt_header(&req, &name).unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn parse_opt_header_empty() {
        let mut req = make_request();
        req.headers.insert("x-custom", "".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let val: Option<String> = parse_opt_header(&req, &name).unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn parse_opt_header_duplicate() {
        let mut req = make_request();
        req.headers.append("x-custom", "a".parse().unwrap());
        req.headers.append("x-custom", "b".parse().unwrap());
        let name = HeaderName::from_static("x-custom");
        let result: S3Result<Option<String>> = parse_opt_header(&req, &name);
        assert!(result.is_err());
    }

    fn parse_checksum_algorithm(headers: &[(&'static str, &'static str)]) -> S3Result<Option<crate::dto::ChecksumAlgorithm>> {
        let mut req = make_request();
        for (name, value) in headers {
            req.headers.insert(*name, value.parse().unwrap());
        }
        parse_checksum_algorithm_header(&req)
    }

    #[test]
    fn parse_checksum_algorithm_header_single_trailer() {
        let ans = parse_checksum_algorithm(&[("x-amz-trailer", "x-amz-checksum-xxhash64")]).unwrap();
        assert_eq!(
            ans.as_ref().map(crate::dto::ChecksumAlgorithm::as_str),
            Some(crate::dto::ChecksumAlgorithm::XXHASH64)
        );
    }

    #[test]
    fn parse_checksum_algorithm_header_trailer_list_with_checksum_first() {
        let ans = parse_checksum_algorithm(&[("x-amz-trailer", "x-amz-checksum-xxhash3, x-amz-meta-foo")]).unwrap();
        assert_eq!(
            ans.as_ref().map(crate::dto::ChecksumAlgorithm::as_str),
            Some(crate::dto::ChecksumAlgorithm::XXHASH3)
        );
    }

    #[test]
    fn parse_checksum_algorithm_header_trailer_list_with_checksum_last() {
        let ans = parse_checksum_algorithm(&[("x-amz-trailer", "x-amz-meta-foo, x-amz-checksum-sha512")]).unwrap();
        assert_eq!(
            ans.as_ref().map(crate::dto::ChecksumAlgorithm::as_str),
            Some(crate::dto::ChecksumAlgorithm::SHA512)
        );
    }

    #[test]
    fn parse_checksum_algorithm_header_trailer_is_case_insensitive() {
        let ans = parse_checksum_algorithm(&[("x-amz-trailer", " X-Amz-Checksum-Xxhash128 ")]).unwrap();
        assert_eq!(
            ans.as_ref().map(crate::dto::ChecksumAlgorithm::as_str),
            Some(crate::dto::ChecksumAlgorithm::XXHASH128)
        );
    }

    #[test]
    fn parse_checksum_algorithm_header_trailer_without_checksum() {
        let ans = parse_checksum_algorithm(&[("x-amz-trailer", "x-amz-meta-foo, x-amz-meta-bar")]).unwrap();
        assert!(ans.is_none());
    }

    #[test]
    fn parse_checksum_algorithm_header_rejects_multiple_checksum_trailers() {
        let err = parse_checksum_algorithm(&[("x-amz-trailer", "x-amz-checksum-crc32, x-amz-checksum-xxhash64")]);
        assert!(err.is_err());
    }

    #[test]
    fn parse_checksum_algorithm_header_prefers_explicit_algorithm() {
        let ans = parse_checksum_algorithm(&[
            ("x-amz-checksum-algorithm", "SHA256"),
            ("x-amz-trailer", "x-amz-checksum-xxhash64"),
        ])
        .unwrap();
        assert_eq!(
            ans.as_ref().map(crate::dto::ChecksumAlgorithm::as_str),
            Some(crate::dto::ChecksumAlgorithm::SHA256)
        );
    }

    #[test]
    fn parse_opt_header_timestamp_present() {
        let mut req = make_request();
        req.headers
            .insert("x-amz-date", "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        let name = HeaderName::from_static("x-amz-date");
        let ts = parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_some());
    }

    #[test]
    fn parse_opt_header_timestamp_missing() {
        let req = make_request();
        let name = HeaderName::from_static("x-amz-date");
        let ts = parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_none());
    }

    #[test]
    fn parse_opt_header_timestamp_invalid() {
        let mut req = make_request();
        req.headers.insert("x-amz-date", "not-a-date".parse().unwrap());
        let name = HeaderName::from_static("x-amz-date");
        let result = parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate);
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_header_timestamp_ignoring_invalid_ignores_bad_values() {
        let mut req = make_request();
        let name = HeaderName::from_static("if-modified-since");
        req.headers.insert("if-modified-since", "not-a-date".parse().unwrap());
        let ts = parse_opt_header_timestamp_ignoring_invalid(&req, &name, TimestampFormat::HttpDate);
        assert!(ts.is_none());

        req.headers
            .insert("if-modified-since", "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        let ts = parse_opt_header_timestamp_ignoring_invalid(&req, &name, TimestampFormat::HttpDate);
        assert!(ts.is_some());
    }

    /// A field that is present but empty cannot carry a date either, so both parsers read it as
    /// absent; the lenient one does not need to reject anything here.
    #[test]
    fn parse_opt_header_timestamp_ignoring_invalid_ignores_an_empty_value() {
        let mut req = make_request();
        let name = HeaderName::from_static("if-modified-since");
        req.headers.insert("if-modified-since", HeaderValue::from_static(""));

        let ts = parse_opt_header_timestamp_ignoring_invalid(&req, &name, TimestampFormat::HttpDate);
        assert!(ts.is_none());
        let strict = parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate).expect("an empty value is absent");
        assert!(strict.is_none());
    }

    /// A value that cannot be decoded as UTF-8 cannot be a valid HTTP-date either, so the lenient
    /// parser ignores it while the strict one still reports it.
    #[test]
    fn parse_opt_header_timestamp_ignoring_invalid_ignores_undecodable_values() {
        let mut req = make_request();
        let name = HeaderName::from_static("if-modified-since");
        req.headers
            .insert("if-modified-since", HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());

        let ts = parse_opt_header_timestamp_ignoring_invalid(&req, &name, TimestampFormat::HttpDate);
        assert!(ts.is_none());
        assert!(parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate).is_err());
    }

    /// A repeated field is read as its first value (the service evaluates the first field line);
    /// the strict parser keeps answering a repeated header with an error.
    #[test]
    fn parse_opt_header_timestamp_ignoring_invalid_reads_the_first_of_repeated_fields() {
        let mut req = make_request();
        let name = HeaderName::from_static("if-modified-since");
        req.headers
            .append("if-modified-since", HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        req.headers
            .append("if-modified-since", HeaderValue::from_static("not-a-date"));

        let ts = parse_opt_header_timestamp_ignoring_invalid(&req, &name, TimestampFormat::HttpDate);
        assert!(ts.is_some(), "the first value carries the condition");
        assert!(parse_opt_header_timestamp(&req, &name, TimestampFormat::HttpDate).is_err());
    }

    // --- parse_query / parse_opt_query tests ---

    #[test]
    fn parse_query_present() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![("max-keys".into(), "100".into())]));
        let val: i32 = parse_query(&req, "max-keys").unwrap();
        assert_eq!(val, 100);
    }

    #[test]
    fn parse_query_missing_qs() {
        let req = make_request();
        let result: S3Result<i32> = parse_query(&req, "max-keys");
        assert!(result.is_err());
    }

    #[test]
    fn parse_query_missing_key() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![("other".into(), "1".into())]));
        let result: S3Result<i32> = parse_query(&req, "max-keys");
        assert!(result.is_err());
    }

    #[test]
    fn parse_query_duplicate() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![
            ("key".into(), "1".into()),
            ("key".into(), "2".into()),
        ]));
        let result: S3Result<i32> = parse_query(&req, "key");
        assert!(result.is_err());
    }

    #[test]
    fn parse_query_invalid_value() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![("max-keys".into(), "abc".into())]));
        let result: S3Result<i32> = parse_query(&req, "max-keys");
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_query_present() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![("prefix".into(), "foo".into())]));
        let val: Option<String> = parse_opt_query(&req, "prefix").unwrap();
        assert_eq!(val.as_deref(), Some("foo"));
    }

    #[test]
    fn parse_opt_query_missing() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![]));
        let val: Option<String> = parse_opt_query(&req, "prefix").unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn parse_opt_query_no_qs() {
        let req = make_request();
        let val: Option<String> = parse_opt_query(&req, "prefix").unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn parse_opt_query_duplicate() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![
            ("key".into(), "a".into()),
            ("key".into(), "b".into()),
        ]));
        let result: S3Result<Option<String>> = parse_opt_query(&req, "key");
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_query_timestamp_present() {
        let mut req = make_request();
        req.s3ext.qs = Some(OrderedQs::from_vec_unchecked(vec![(
            "date".into(),
            "Wed, 21 Oct 2015 07:28:00 GMT".into(),
        )]));
        let ts = parse_opt_query_timestamp(&req, "date", TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_some());
    }

    #[test]
    fn parse_opt_query_timestamp_missing() {
        let req = make_request();
        let ts = parse_opt_query_timestamp(&req, "date", TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_none());
    }

    // --- unwrap_bucket / unwrap_object tests ---

    #[test]
    fn unwrap_bucket_test() {
        let mut req = make_request();
        req.s3ext.s3_path = Some(S3Path::Bucket {
            bucket: "my-bucket".into(),
        });
        let bucket = unwrap_bucket(&mut req);
        assert_eq!(bucket, "my-bucket");
    }

    #[test]
    #[should_panic(expected = "s3 path not found")]
    fn unwrap_bucket_missing() {
        let mut req = make_request();
        let _ = unwrap_bucket(&mut req);
    }

    #[test]
    fn unwrap_object_test() {
        let mut req = make_request();
        req.s3ext.s3_path = Some(S3Path::Object {
            bucket: "my-bucket".into(),
            key: "my-key".into(),
        });
        let (bucket, key) = unwrap_object(&mut req);
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "my-key");
    }

    #[test]
    #[should_panic(expected = "s3 path not found")]
    fn unwrap_object_missing() {
        let mut req = make_request();
        let _ = unwrap_object(&mut req);
    }

    // --- take_string_body / take_xml_body / take_opt_xml_body tests ---

    #[test]
    fn take_string_body_ok() {
        let mut req = make_request();
        req.body = Body::from(Bytes::from_static(b"hello world"));
        let s = take_string_body(&mut req).unwrap();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn take_string_body_empty() {
        let mut req = make_request();
        req.body = Body::from(Bytes::new());
        let s = take_string_body(&mut req).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn take_string_body_invalid_utf8() {
        let mut req = make_request();
        req.body = Body::from(vec![0xff, 0xfe]);
        let result = take_string_body(&mut req);
        assert!(result.is_err());
    }

    #[test]
    fn take_stream_body_test() {
        let mut req = make_request();
        req.body = Body::from(Bytes::from_static(b"stream data"));
        let blob = take_stream_body(&mut req);
        let rl = blob.remaining_length();
        // After taking, body should be default (empty)
        assert!(http_body::Body::is_end_stream(&req.body));
        let _ = rl;
    }

    #[test]
    fn take_xml_body_empty_returns_error() {
        let mut req = make_request();
        req.body = Body::from(Bytes::new());
        let result = take_xml_body::<crate::dto::Tagging>(&mut req);
        assert!(result.is_err());
    }

    #[test]
    fn take_opt_xml_body_empty_returns_none() {
        let mut req = make_request();
        req.body = Body::from(Bytes::new());
        let result = take_opt_xml_body::<crate::dto::Tagging>(&mut req).unwrap();
        assert!(result.is_none());
    }

    // --- parse_opt_metadata ---

    #[test]
    fn parse_opt_metadata_none() {
        let req = make_request();
        let result = parse_opt_metadata(&req).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_opt_metadata_some() {
        let mut req = make_request();
        req.headers.insert("x-amz-meta-mykey", "myvalue".parse().unwrap());
        let result = parse_opt_metadata(&req).unwrap();
        let metadata = result.unwrap();
        assert_eq!(metadata.get("mykey").map(String::as_str), Some("myvalue"));
    }

    #[test]
    fn parse_opt_metadata_empty_key_ignored() {
        let mut req = make_request();
        // "x-amz-meta-" with empty suffix should be ignored
        req.headers.insert("x-amz-meta-", "value".parse().unwrap());
        let result = parse_opt_metadata(&req).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_opt_metadata_duplicate_header() {
        let mut req = make_request();
        req.headers.append("x-amz-meta-key", "val1".parse().unwrap());
        req.headers.append("x-amz-meta-key", "val2".parse().unwrap());
        let result = parse_opt_metadata(&req);
        assert!(result.is_err());
    }

    // --- parse_list_header / parse_opt_list_header ---

    #[test]
    fn parse_list_header_single() {
        let mut req = make_request();
        req.headers.insert("x-list", "hello".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list, vec!["hello".to_string()]);
    }

    #[test]
    fn parse_list_header_multiple() {
        let mut req = make_request();
        req.headers.append("x-list", "a".parse().unwrap());
        req.headers.append("x-list", "b".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn parse_list_header_missing() {
        let req = make_request();
        let name = HeaderName::from_static("x-list");
        let result: S3Result<List<String>> = parse_list_header(&req, &name);
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_list_header_present() {
        let mut req = make_request();
        req.headers.insert("x-list", "item".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: Option<List<String>> = parse_opt_list_header(&req, &name).unwrap();
        assert!(list.is_some());
    }

    #[test]
    fn parse_opt_list_header_missing() {
        let req = make_request();
        let name = HeaderName::from_static("x-list");
        let list: Option<List<String>> = parse_opt_list_header(&req, &name).unwrap();
        assert!(list.is_none());
    }

    #[test]
    fn parse_list_header_splits_commas() {
        let mut req = make_request();
        req.headers.insert("x-list", "a,b".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_list_header_splits_every_field_line() {
        let mut req = make_request();
        req.headers.append("x-list", "a,b".parse().unwrap());
        req.headers.append("x-list", "c".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn parse_list_header_trims_optional_whitespace() {
        let mut req = make_request();
        req.headers.insert("x-list", "a , b".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_list_header_ignores_empty_elements() {
        let mut req = make_request();
        req.headers.insert("x-list", "a,,b,".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: List<String> = parse_list_header(&req, &name).unwrap();
        assert_eq!(list, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_list_header_empty_value_is_missing() {
        let mut req = make_request();
        req.headers.insert("x-list", "".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let result: S3Result<List<String>> = parse_list_header(&req, &name);
        assert!(result.is_err());
    }

    #[test]
    fn parse_opt_list_header_splits_commas() {
        let mut req = make_request();
        req.headers.insert("x-list", "a,b".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: Option<List<String>> = parse_opt_list_header(&req, &name).unwrap();
        assert_eq!(list, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn parse_opt_list_header_empty_value_is_none() {
        let mut req = make_request();
        req.headers.insert("x-list", "".parse().unwrap());
        let name = HeaderName::from_static("x-list");
        let list: Option<List<String>> = parse_opt_list_header(&req, &name).unwrap();
        assert!(list.is_none());
    }

    #[test]
    fn parse_list_header_splits_object_attributes() {
        // Guards the GetObjectAttributes regression: a comma-joined value such as
        // ETag,ObjectSize used to become one element, which the proxy forwarded as
        // a quoted single attribute name and the backend rejected.
        use crate::dto::ObjectAttributes;

        let mut req = make_request();
        req.headers
            .insert(crate::header::X_AMZ_OBJECT_ATTRIBUTES, "ETag,ObjectSize".parse().unwrap());
        let list: List<ObjectAttributes> = parse_list_header(&req, &crate::header::X_AMZ_OBJECT_ATTRIBUTES).unwrap();
        assert_eq!(
            list,
            vec![
                ObjectAttributes::from_static(ObjectAttributes::ETAG),
                ObjectAttributes::from_static(ObjectAttributes::OBJECT_SIZE),
            ]
        );
    }

    // --- parse_field_value / parse_field_value_timestamp ---

    #[test]
    fn parse_field_value_found() {
        let m = Multipart::new_for_test(
            vec![("key".into(), "42".into())],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let val: Option<i32> = parse_field_value(&m, "key").unwrap();
        assert_eq!(val, Some(42));
    }

    #[test]
    fn parse_field_value_not_found() {
        let m = Multipart::new_for_test(
            vec![],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let val: Option<i32> = parse_field_value(&m, "key").unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn parse_field_value_invalid() {
        let m = Multipart::new_for_test(
            vec![("key".into(), "abc".into())],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let result: S3Result<Option<i32>> = parse_field_value(&m, "key");
        assert!(result.is_err());
    }

    #[test]
    fn parse_field_value_timestamp_found() {
        let m = Multipart::new_for_test(
            vec![("date".into(), "Wed, 21 Oct 2015 07:28:00 GMT".into())],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let ts = parse_field_value_timestamp(&m, "date", TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_some());
    }

    #[test]
    fn parse_field_value_timestamp_not_found() {
        let m = Multipart::new_for_test(
            vec![],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let ts = parse_field_value_timestamp(&m, "date", TimestampFormat::HttpDate).unwrap();
        assert!(ts.is_none());
    }

    #[test]
    fn parse_field_value_timestamp_invalid() {
        let m = Multipart::new_for_test(
            vec![("date".into(), "not-a-date".into())],
            File {
                name: "file".into(),
                content_type: None,
                stream: None,
            },
        );
        let result = parse_field_value_timestamp(&m, "date", TimestampFormat::HttpDate);
        assert!(result.is_err());
    }

    // --- ParseHeaderError Display ---

    #[test]
    fn parse_header_error_display() {
        assert!(format!("{}", ParseHeaderError::Boolean).contains("boolean"));
        assert!(format!("{}", ParseHeaderError::Integer).contains("integer"));
        assert!(format!("{}", ParseHeaderError::Long).contains("long"));
        assert!(format!("{}", ParseHeaderError::Enum).contains("enum"));
        assert!(format!("{}", ParseHeaderError::String).contains("string"));
    }

    // --- BodyLiteral tests ---

    #[cfg(feature = "minio")]
    mod body_literal_tests {
        use super::*;
        use crate::dto::{BucketVersioningStatus, ObjectLockEnabled, VersioningConfiguration};
        use crate::http::request::S3Extensions;

        fn make_xml_body(xml: &str) -> Body {
            Body::from(xml.as_bytes().to_vec())
        }

        fn make_request_with_body(body: Body) -> Request {
            Request {
                version: http::Version::HTTP_11,
                method: hyper::Method::PUT,
                uri: hyper::Uri::from_static("http://example.com"),
                headers: hyper::HeaderMap::new(),
                extensions: hyper::http::Extensions::default(),
                body,
                s3ext: S3Extensions::default(),
            }
        }

        #[test]
        fn take_body_literal_matches_enabled() {
            let body = Body::from(b" Enabled ".to_vec());
            let mut req = make_request_with_body(body);
            let result: VersioningConfiguration = take_body_literal(&mut req, "Enabled").unwrap();
            assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
        }

        #[test]
        fn take_body_literal_falls_through_to_xml() {
            let xml = "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
            let body = make_xml_body(xml);
            let mut req = make_request_with_body(body);
            let result: VersioningConfiguration = take_body_literal(&mut req, "Enabled").unwrap();
            assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
        }

        #[test]
        fn take_body_literal_empty_body_returns_error() {
            let body = Body::empty();
            let mut req = make_request_with_body(body);
            let result = take_body_literal::<VersioningConfiguration>(&mut req, "Enabled");
            assert!(result.is_err());
        }

        #[test]
        fn take_opt_body_literal_empty_body_returns_none() {
            let body = Body::empty();
            let mut req = make_request_with_body(body);
            let result: Option<crate::dto::ObjectLockConfiguration> = take_opt_body_literal(&mut req, "Enabled").unwrap();
            assert!(result.is_none());
        }

        #[test]
        fn take_opt_body_literal_matches_enabled() {
            let body = Body::from(b"Enabled".to_vec());
            let mut req = make_request_with_body(body);
            let result: Option<crate::dto::ObjectLockConfiguration> = take_opt_body_literal(&mut req, "Enabled").unwrap();
            let config = result.unwrap();
            assert_eq!(config.object_lock_enabled.as_ref().map(ObjectLockEnabled::as_str), Some("Enabled"));
            assert!(config.rule.is_none());
        }
    }
}

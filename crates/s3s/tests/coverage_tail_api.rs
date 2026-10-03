// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Coverage tests for hand-written tail modules that are reachable through the
//! public API: DTO helpers, ETag/Range/Region accessors, XML deserializer and
//! serializer edge cases, and the stream wrappers.

#![allow(clippy::too_many_lines)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use s3s::dto::{
    BucketLocationConstraint, CopySource, ETag, ETagCondition, EncodingType, GetBucketLocationOutput, ListObjectsInput,
    ListObjectsV2Input, Range, RequestPayer, StreamingBlob,
};
use s3s::stream::ByteStream;
use s3s::stream::upload_stream::UploadStream;
use s3s::xml::{DeError, Deserialize, Deserializer, Serialize, Serializer};
use s3s::{S3Error, S3ErrorCode, StdError};
use s3s_sigv4::Sha256Sum;

fn sha256_hex(hex: &str) -> Sha256Sum {
    Sha256Sum::from_hex(hex).expect("valid sha256 hex")
}

const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const SHA256_ABCD: &str = "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589";

// ---------------------------------------------------------------------------
// DTO conversions
// ---------------------------------------------------------------------------

#[test]
fn list_objects_input_converts_to_v2() {
    let v1 = ListObjectsInput {
        bucket: "bucket".to_owned(),
        delimiter: Some("delimiter".to_owned()),
        encoding_type: Some(EncodingType::from("encoding-type".to_owned())),
        expected_bucket_owner: Some("owner".to_owned()),
        marker: Some("marker".to_owned()),
        max_keys: Some(7),
        prefix: Some("prefix".to_owned()),
        request_payer: Some(RequestPayer::from("requester".to_owned())),
        optional_object_attributes: None,
    };

    let v2 = ListObjectsV2Input::from(v1);

    assert_eq!(v2.bucket.as_str(), "bucket");
    assert_eq!(v2.delimiter.as_deref(), Some("delimiter"));
    assert_eq!(v2.encoding_type.as_ref().map(EncodingType::as_str), Some("encoding-type"));
    assert_eq!(v2.expected_bucket_owner.as_deref(), Some("owner"));
    assert_eq!(v2.max_keys, Some(7));
    assert_eq!(v2.prefix.as_deref(), Some("prefix"));
    assert_eq!(v2.request_payer.as_ref().map(RequestPayer::as_str), Some("requester"));
    // The v1 marker becomes the v2 start_after; v2-only fields stay empty.
    assert_eq!(v2.start_after.as_deref(), Some("marker"));
    assert_eq!(v2.continuation_token, None);
    assert_eq!(v2.fetch_owner, None);
    assert_eq!(v2.optional_object_attributes, None);
}

// ---------------------------------------------------------------------------
// S3Error accessors
// ---------------------------------------------------------------------------

#[test]
fn s3_error_setters_and_getters() {
    let mut err = S3Error::new(S3ErrorCode::AccessDenied);
    assert_eq!(err.code().as_str(), "AccessDenied");
    assert_eq!(err.request_id(), None);
    assert_eq!(err.headers(), None);

    err.set_code(S3ErrorCode::NoSuchBucket);
    err.set_message("no such bucket");
    err.set_request_id("request-id-1");
    err.set_headers({
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-amz-test", hyper::header::HeaderValue::from_static("value"));
        headers
    });

    assert_eq!(err.code().as_str(), "NoSuchBucket");
    assert_eq!(err.message(), Some("no such bucket"));
    assert_eq!(err.request_id(), Some("request-id-1"));
    assert_eq!(
        err.headers()
            .and_then(|h| h.get("x-amz-test"))
            .map(|v| v.to_str().expect("ascii")),
        Some("value")
    );
}

#[test]
fn s3_error_internal_error_keeps_source() {
    let source = std::io::Error::other("disk on fire");
    let err = S3Error::internal_error(source);

    assert_eq!(err.code().as_str(), "InternalError");
    let source = err.source().expect("source is set");
    assert_eq!(source.to_string(), "disk on fire");
}

#[test]
fn s3_error_display_includes_optional_fields() {
    let mut err = S3Error::new(S3ErrorCode::AccessDenied);
    assert_eq!(format!("{err}"), "S3Error { code: AccessDenied, message: \"Access Denied\", .. }");

    err.set_request_id("request-id-2");
    err.set_status_code(hyper::StatusCode::IM_A_TEAPOT);
    let text = format!("{err}");
    assert!(text.contains("request_id: \"request-id-2\""), "{text}");
    assert!(text.contains("status_code: 418"), "{text}");
}

#[test]
fn s3_error_to_http_response_writes_request_id() {
    let mut err = S3Error::new(S3ErrorCode::AccessDenied);
    err.set_request_id("request-id-3");

    let resp = err.to_http_response().expect("serializes");
    assert_eq!(resp.status(), hyper::StatusCode::FORBIDDEN);
    let body = futures::executor::block_on(http_body_util::BodyExt::collect(resp.into_body()))
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf-8");
    assert!(body.contains("<Code>AccessDenied</Code>"), "{body}");
    assert!(body.contains("<RequestId>request-id-3</RequestId>"), "{body}");
}

#[test]
fn s3_error_code_from_str_round_trips() {
    assert_eq!(S3ErrorCode::from_str("NoSuchKey").expect("infallible"), S3ErrorCode::NoSuchKey);

    let custom = S3ErrorCode::from_str("TotallyMadeUpCode").expect("infallible");
    assert_eq!(custom.as_str(), "TotallyMadeUpCode");
    assert_eq!(custom, S3ErrorCode::Custom("TotallyMadeUpCode".into()));
    assert_eq!(custom.status_code(), None);
}

// ---------------------------------------------------------------------------
// ETag and Range accessors
// ---------------------------------------------------------------------------

#[test]
fn etag_accessors_cover_every_variant() {
    let strong = ETag::Strong("strong-value".to_owned());
    let weak = ETag::Weak("weak-value".to_owned());

    assert_eq!(strong.as_strong(), Some("strong-value"));
    assert_eq!(strong.as_weak(), None);
    assert_eq!(weak.as_weak(), Some("weak-value"));
    assert_eq!(weak.as_strong(), None);

    assert_eq!(strong.clone().into_strong(), Some("strong-value".to_owned()));
    assert_eq!(weak.clone().into_strong(), None);
    assert_eq!(strong.clone().into_weak(), None);
    assert_eq!(weak.clone().into_weak(), Some("weak-value".to_owned()));

    assert_eq!(strong.into_value(), "strong-value");
    assert_eq!(weak.into_value(), "weak-value");
}

#[test]
fn etag_condition_into_etag_covers_both_variants() {
    assert_eq!(ETagCondition::Any.into_etag(), None);
    assert_eq!(
        ETagCondition::ETag(ETag::Strong("etag".to_owned())).into_etag(),
        Some(ETag::Strong("etag".to_owned()))
    );
}

#[test]
fn range_check_rejects_inconsistent_range() {
    // The public variants allow an inconsistent range that the parser rejects.
    let range = Range::Int { first: 5, last: Some(3) };
    assert!(range.check(10).is_err());

    assert_eq!(Range::Int { first: 5, last: Some(3) }.to_header_string(), "bytes=5-3");
}

#[test]
fn range_parse_rejects_non_numeric_bound() {
    assert!(Range::parse("bytes=abc-def").is_err());
    assert_eq!(Range::parse("bytes=0-9").expect("valid"), Range::Int { first: 0, last: Some(9) });
}

#[test]
fn region_as_ref_returns_the_name() {
    let region = s3s::region::Region::new("us-east-1".into()).expect("valid region");
    let as_ref: &str = region.as_ref();
    assert_eq!(as_ref, "us-east-1");
}

#[test]
fn check_bucket_name_rejects_leading_dash() {
    assert!(!s3s::path::check_bucket_name("-bucket"));
    assert!(s3s::path::check_bucket_name("bucket-name"));
}

#[test]
fn copy_source_outpost_with_version_id_formats() {
    let copy_source = CopySource::Outpost {
        partition: "aws".into(),
        region: "us-east-1".into(),
        account_id: "123456789012".into(),
        outpost_id: "op-123".into(),
        key: "path/to/key".into(),
        version_id: Some("version id".into()),
    };

    assert_eq!(
        copy_source.format_to_string(),
        "arn:aws:s3-outposts:us-east-1:123456789012:outpost/op-123/object/path/to/key?versionId=version%20id"
    );
}

// ---------------------------------------------------------------------------
// XML deserializer / serializer internals
// ---------------------------------------------------------------------------

#[test]
fn xml_deserializer_expect_eof_rejects_events() {
    let mut d = Deserializer::new(b"<a/>");
    assert!(matches!(d.expect_eof(), Err(DeError::UnexpectedStart)));

    let mut d = Deserializer::new(b"<a></a>");
    let err = d
        .named_element("a", Deserializer::expect_eof)
        .expect_err("the end tag is not eof");
    assert!(matches!(err, DeError::UnexpectedEnd));

    let mut d = Deserializer::new(b"");
    assert!(d.expect_eof().is_ok());
}

#[test]
fn xml_deserializer_named_element_reports_start_errors() {
    // A missing nested element leaves the inner lookup facing the end tag.
    let mut d = Deserializer::new(b"<a></a>");
    let err = d
        .named_element("a", |d| d.named_element("b", |_| Ok(())))
        .expect_err("missing nested element");
    assert!(matches!(err, DeError::UnexpectedEnd));

    let mut d = Deserializer::new(b"");
    assert!(matches!(d.named_element("a", |_| Ok(())), Err(DeError::UnexpectedEof)));

    let mut d = Deserializer::new(b"<b></b>");
    assert!(matches!(d.named_element("a", |_| Ok(())), Err(DeError::UnexpectedTagName)));
}

#[test]
fn xml_deserializer_named_element_any_reports_start_errors() {
    let mut d = Deserializer::new(b"<a></a>");
    let err = d
        .named_element("a", |d| d.named_element_any(&["b"], |_| Ok(())))
        .expect_err("missing nested element");
    assert!(matches!(err, DeError::UnexpectedEnd));

    let mut d = Deserializer::new(b"");
    assert!(matches!(d.named_element_any(&["a", "b"], |_| Ok(())), Err(DeError::UnexpectedEof)));

    let mut d = Deserializer::new(b"<a/>");
    assert!(d.named_element_any(&["a", "b"], |_| Ok(())).is_ok());
}

#[test]
fn xml_deserializer_element_rejects_end_and_eof() {
    let mut d = Deserializer::new(b"<a></a>");
    let err = d
        .named_element("a", |d| d.element(|_, _| Ok(())))
        .expect_err("no child element");
    assert!(matches!(err, DeError::UnexpectedEnd));

    let mut d = Deserializer::new(b"");
    assert!(matches!(d.element(|_, _| Ok(())), Err(DeError::UnexpectedEnd)));

    let mut d = Deserializer::new(b"<a>text</a>");
    d.element(|_, name| {
        assert_eq!(name, b"a");
        Ok(())
    })
    .expect("element deserializes");
}

#[test]
fn xml_list_content_skips_unknown_elements() {
    let mut d = Deserializer::new(b"<Unknown><Nested>text</Nested></Unknown><Known>value</Known>");
    let list = d.list_content::<String>("Known").expect("list content");
    assert_eq!(list, vec!["value".to_owned()]);
}

#[test]
fn xml_list_content_reports_unexpected_eof_inside_unknown_element() {
    // A forward-compatible skip of an unknown element runs out of input before
    // the element is closed.
    let mut d = Deserializer::new(b"<Root><Unknown><Nested>");
    let err = d
        .named_element("Root", |d| d.list_content::<String>("Known"))
        .expect_err("unexpected eof");
    assert!(matches!(err, DeError::UnexpectedEof));
}

#[test]
fn xml_deserializer_debug_is_non_exhaustive() {
    let d = Deserializer::new(b"<a/>");
    assert_eq!(format!("{d:?}"), "Deserializer { .. }");
}

#[test]
fn xml_serializer_debug_and_str_content() {
    let mut buf = Vec::new();
    let serializer = Serializer::new(&mut buf);
    assert_eq!(format!("{serializer:?}"), "Serializer { .. }");

    let mut buf = Vec::new();
    let mut serializer = Serializer::new(&mut buf);
    serializer.content("Name", &"value").expect("serializes");
    let text = String::from_utf8(buf).expect("utf-8");
    assert_eq!(text, "<Name>value</Name>");
}

#[test]
fn get_bucket_location_output_rejects_duplicate_and_unknown_fields() {
    let mut d = Deserializer::new(
        b"<LocationConstraint>us-east-1</LocationConstraint><LocationConstraint>eu-west-1</LocationConstraint>",
    );
    let err = GetBucketLocationOutput::deserialize(&mut d).expect_err("duplicate field");
    assert!(matches!(err, DeError::DuplicateField));

    let mut d = Deserializer::new(b"<Unknown/>");
    let err = GetBucketLocationOutput::deserialize(&mut d).expect_err("unknown field");
    assert!(matches!(err, DeError::UnexpectedTagName));

    let mut d = Deserializer::new(b"<LocationConstraint>us-east-1</LocationConstraint>");
    let output = GetBucketLocationOutput::deserialize(&mut d).expect("deserializes");
    assert_eq!(
        output.location_constraint.as_ref().map(BucketLocationConstraint::as_str),
        Some("us-east-1")
    );

    // Round trip through the hand-written serializer: an absent constraint is
    // still written as an empty element.
    let mut buf = Vec::new();
    let mut serializer = Serializer::new(&mut buf);
    GetBucketLocationOutput::default()
        .serialize(&mut serializer)
        .expect("serializes");
    let text = String::from_utf8(buf).expect("utf-8");
    assert_eq!(
        text,
        "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>"
    );
}

// ---------------------------------------------------------------------------
// StreamingBlob and ByteStream
// ---------------------------------------------------------------------------

#[test]
fn streaming_blob_size_hint_of_once_variant() {
    let blob = StreamingBlob::from_bytes(Bytes::from_static(b"x"));
    assert_eq!(Stream::size_hint(&blob), (1, Some(1)));

    let blob = StreamingBlob::from_bytes(Bytes::new());
    assert_eq!(Stream::size_hint(&blob), (0, Some(0)));
}

#[test]
fn streaming_blob_wrapper_delegates_size_hint_and_remaining_length() {
    let blob = StreamingBlob::wrap(futures::stream::empty::<Result<Bytes, std::io::Error>>());
    assert_eq!(Stream::size_hint(&blob), (0, Some(0)));
    assert_eq!(blob.remaining_length().exact(), None);
}

#[test]
fn aws_chunked_stream_size_hint_is_unknown() {
    let stream = s3s::stream::aws_chunked_stream::AwsChunkedStream::new(
        futures::stream::empty::<Result<Bytes, StdError>>(),
        sha256_hex(SHA256_EMPTY),
        s3s_sigv4::AmzDate::parse("20260101T000000Z").expect("valid amz date"),
        "us-east-1".into(),
        "s3".into(),
        s3s::auth::SecretKey::from("secret"),
        0,
        false,
        65_536,
    );

    assert_eq!(Stream::size_hint(&stream), (0, None));
}

/// A byte stream that does not override the trait default for the remaining
/// length.
struct NoRemainingLength;

impl Stream for NoRemainingLength {
    type Item = Result<Bytes, StdError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl ByteStream for NoRemainingLength {}

#[test]
fn byte_stream_default_remaining_length_is_unknown() {
    assert_eq!(NoRemainingLength.remaining_length().exact(), None);
}

// ---------------------------------------------------------------------------
// UploadStream
// ---------------------------------------------------------------------------

enum ScriptedItem {
    Data(&'static [u8]),
    Fail,
    Pending,
    Empty,
}

#[derive(Default)]
struct ScriptedStream {
    items: VecDeque<ScriptedItem>,
}

impl ScriptedStream {
    fn new(items: impl IntoIterator<Item = ScriptedItem>) -> Self {
        Self {
            items: items.into_iter().collect(),
        }
    }
}

impl Stream for ScriptedStream {
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.items.pop_front() {
            Some(ScriptedItem::Data(bytes)) => Poll::Ready(Some(Ok(Bytes::from_static(bytes)))),
            Some(ScriptedItem::Fail) => Poll::Ready(Some(Err("scripted failure".into()))),
            Some(ScriptedItem::Empty) => Poll::Ready(Some(Ok(Bytes::new()))),
            Some(ScriptedItem::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            None => Poll::Ready(None),
        }
    }
}

fn poll_next_once<S: Stream + Unpin>(stream: &mut S) -> Poll<Option<S::Item>> {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    Pin::new(stream).poll_next(&mut cx)
}

fn poll_item<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    match poll_next_once(stream) {
        Poll::Ready(item) => item,
        Poll::Pending => panic!("the scripted stream must be ready"),
    }
}

#[test]
fn upload_stream_debug_and_size_hint() {
    let stream = UploadStream::new(ScriptedStream::default(), 4, sha256_hex(SHA256_ABCD));
    let debug = format!("{stream:?}");
    assert_eq!(debug, "UploadStream { remaining_length: 4, state: Reading, .. }");
    assert_eq!(stream.size_hint(), (0, None));
}

#[test]
fn upload_stream_finishes_an_empty_verified_payload() {
    let mut stream = UploadStream::new(ScriptedStream::default(), 0, sha256_hex(SHA256_EMPTY));
    assert!(poll_next_once(&mut stream).is_ready());
    assert!(poll_item(&mut stream).is_none());
}

#[test]
fn upload_stream_reports_hash_mismatch_on_empty_payload() {
    let mut stream = UploadStream::new(ScriptedStream::default(), 0, sha256_hex(SHA256_ABCD));
    let err = poll_item(&mut stream).expect("item").expect_err("mismatch");
    assert_eq!(err.to_s3_error_code(), S3ErrorCode::BadDigest);
}

#[test]
fn upload_stream_reports_underlying_error() {
    let mut stream = UploadStream::new(ScriptedStream::new([ScriptedItem::Fail]), 4, sha256_hex(SHA256_ABCD));
    let err = poll_item(&mut stream).expect("item").expect_err("underlying");
    assert_eq!(err.to_s3_error_code(), S3ErrorCode::InternalError);
    assert_eq!(err.to_string(), "UploadStreamError: Underlying: scripted failure");
}

#[test]
fn upload_stream_reports_incomplete_payload() {
    let mut stream = UploadStream::new(ScriptedStream::default(), 4, sha256_hex(SHA256_ABCD));
    let err = poll_item(&mut stream).expect("item").expect_err("incomplete");
    assert_eq!(err.to_s3_error_code(), S3ErrorCode::IncompleteBody);
}

#[test]
fn upload_stream_is_pending_while_reading() {
    let mut stream = UploadStream::new(ScriptedStream::new([ScriptedItem::Pending]), 4, sha256_hex(SHA256_ABCD));
    assert!(poll_next_once(&mut stream).is_pending());

    let mut stream = UploadStream::new(
        ScriptedStream::new([ScriptedItem::Data(b"abcd"), ScriptedItem::Pending]),
        4,
        sha256_hex(SHA256_ABCD),
    );
    let data = poll_item(&mut stream).expect("item").expect("data");
    assert_eq!(data, Bytes::from_static(b"abcd"));
    // The declared length is complete and the digest matches, so the stream now
    // waits for EOF instead of padding: an empty chunk is skipped and a pending
    // inner stream stays pending.
    assert!(poll_next_once(&mut stream).is_pending());
}

#[test]
fn upload_stream_skips_empty_chunks_and_finishes() {
    let mut stream = UploadStream::new(
        ScriptedStream::new([ScriptedItem::Empty, ScriptedItem::Data(b"abcd"), ScriptedItem::Empty]),
        4,
        sha256_hex(SHA256_ABCD),
    );

    let data = poll_item(&mut stream).expect("item").expect("data");
    assert_eq!(data, Bytes::from_static(b"abcd"));
    assert!(poll_next_once(&mut stream).is_ready());
    // The terminal state keeps returning None without polling the inner stream.
    let mut stream = UploadStream::new(ScriptedStream::default(), 4, sha256_hex(SHA256_ABCD));
    let _ = poll_next_once(&mut stream);
    assert!(poll_next_once(&mut stream).is_ready());
    assert!(poll_next_once(&mut stream).is_ready());
}

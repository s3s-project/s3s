// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::SelectObjectContentEvent;
use super::{ContinuationEvent, EndEvent, ProgressEvent, RecordsEvent, StatsEvent};

use crate::S3Error;
use crate::S3Result;
use crate::StdError;
use crate::crypto::Checksum;
use crate::crypto::Crc32;
use crate::stream::ByteStream;
use crate::stream::DynByteStream;
use crate::{S3ErrorCode, xml};

use std::fmt;
use std::num::TryFromIntError;
use std::pin::Pin;
use std::task::ready;
use std::task::{Context, Poll};

use bytes::BufMut;
use bytes::Bytes;
use futures::Stream;
use smallvec::SmallVec;
use tracing::debug;

pub struct SelectObjectContentEventStream {
    inner: Pin<Box<dyn Stream<Item = S3Result<SelectObjectContentEvent>> + Send + Sync + 'static>>,
}

impl SelectObjectContentEventStream {
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = S3Result<SelectObjectContentEvent>> + Send + Sync + 'static,
    {
        Self { inner: Box::pin(stream) }
    }

    #[must_use]
    pub fn into_byte_stream(self) -> DynByteStream {
        Box::pin(Wrapper {
            inner: self,
            pending: SmallVec::new(),
        })
    }
}

impl Stream for SelectObjectContentEventStream {
    type Item = S3Result<SelectObjectContentEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl fmt::Debug for SelectObjectContentEventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectObjectContentEventStream")
            .field("size_hint", &self.size_hint())
            .finish_non_exhaustive()
    }
}

struct Wrapper {
    inner: SelectObjectContentEventStream,
    pending: SmallVec<[Bytes; 3]>,
}

impl Stream for Wrapper {
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(bytes) = self.pending.pop() {
                return Poll::Ready(Some(Ok(bytes)));
            }

            let item = ready!(Pin::new(&mut self.inner).poll_next(cx));
            debug!(?item, "SelectObjectContentEventStream");
            match item {
                Some(ev) => {
                    let result = event_into_chunks(ev);
                    match result {
                        Ok(mut chunks) => {
                            chunks.reverse();
                            self.pending = chunks;
                        }
                        Err(err) => {
                            debug!("SelectObjectContentEventStream: Error: {}", err);
                            return Poll::Ready(Some(Err(Box::new(err) as StdError)));
                        }
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

impl ByteStream for Wrapper {}

#[cfg(test)]
fn event_into_bytes(ev: S3Result<SelectObjectContentEvent>) -> Result<Bytes, SerError> {
    match ev {
        Ok(event) => event.into_message().serialize(),
        Err(err) => {
            debug!(?err, "SelectObjectContentEventStream: Request Level Error");
            request_level_error(&err).serialize()
        }
    }
}

fn event_into_chunks(ev: S3Result<SelectObjectContentEvent>) -> Result<SmallVec<[Bytes; 3]>, SerError> {
    match ev {
        Ok(event) => event.into_message().serialize_chunks(),
        Err(err) => {
            debug!(?err, "SelectObjectContentEventStream: Request Level Error");
            request_level_error(&err).serialize_chunks()
        }
    }
}

struct Message {
    headers: SmallVec<[Header; 4]>,
    payload: Option<Bytes>,
}

struct Header {
    name: Bytes,
    value: Bytes,
}

#[derive(Debug, thiserror::Error)]
enum SerError {
    #[error("Message Serialization: LengthOverflow")]
    LengthOverflow,

    #[error("Message Serialization: IntOverflow: {0}")]
    IntOverflow(#[from] TryFromIntError),
}

impl Message {
    /// <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTSelectObjectAppendix.html>
    #[cfg(test)]
    fn serialize(self) -> Result<Bytes, SerError> {
        let chunks = self.serialize_chunks()?;
        if chunks.len() == 1 {
            return Ok(chunks.into_iter().next().expect("one chunk"));
        }

        let total_len = chunks.iter().map(Bytes::len).sum();
        let mut buf = Vec::with_capacity(total_len);
        for chunk in chunks {
            buf.put(chunk.as_ref());
        }
        Ok(buf.into())
    }

    fn serialize_chunks(self) -> Result<SmallVec<[Bytes; 3]>, SerError> {
        let total_byte_length: u32;
        let headers_byte_length: u32;
        {
            let headers_len = self.headers.iter().try_fold(0, |mut acc: usize, h| {
                acc = acc.checked_add(1 + 1 + 2)?;
                acc = acc.checked_add(h.name.len())?;
                acc = acc.checked_add(h.value.len())?;
                Some(acc)
            });

            let payload_len = self.payload.as_ref().map_or(0, Bytes::len);

            let total_len = headers_len
                .and_then(|acc| acc.checked_add(4 + 4 + 4 + 4))
                .and_then(|acc| acc.checked_add(payload_len));

            total_byte_length = u32::try_from(total_len.ok_or(SerError::LengthOverflow)?)?;
            headers_byte_length = u32::try_from(headers_len.ok_or(SerError::LengthOverflow)?)?;
        }

        let payload = self.payload.filter(|payload| !payload.is_empty());
        let frame_overhead = if payload.is_some() { 12 } else { 16 };
        let mut buf: Vec<u8> = Vec::with_capacity(frame_overhead + headers_byte_length as usize);
        buf.put_u32(total_byte_length);
        buf.put_u32(headers_byte_length);

        let prelude_crc = Crc32::checksum_u32(&buf);
        buf.put_u32(prelude_crc);

        for h in &self.headers {
            let header_name_byte_length = u8::try_from(h.name.len())?;
            let value_string_byte_length = u16::try_from(h.value.len())?;

            buf.put_u8(header_name_byte_length);
            buf.put(&*h.name);

            buf.put_u8(7);
            buf.put_u16(value_string_byte_length);
            buf.put(&*h.value);
        }

        let mut chunks = SmallVec::new();

        if let Some(payload) = payload {
            let mut message_crc = Crc32::new();
            message_crc.update(&buf);
            message_crc.update(&payload);

            chunks.push(buf.into());
            chunks.push(payload);
            chunks.push(Bytes::copy_from_slice(&message_crc.finalize()));
        } else {
            let message_crc = Crc32::checksum_u32(&buf);
            buf.put_u32(message_crc);
            chunks.push(buf.into());
        }

        Ok(chunks)
    }
}

impl SelectObjectContentEvent {
    fn into_message(self) -> Message {
        match self {
            SelectObjectContentEvent::Cont(e) => e.into_message(),
            SelectObjectContentEvent::End(e) => e.into_message(),
            SelectObjectContentEvent::Progress(e) => e.into_message(),
            SelectObjectContentEvent::Records(e) => e.into_message(),
            SelectObjectContentEvent::Stats(e) => e.into_message(),
        }
    }
}

const EVENT_TYPE: &str = ":event-type";
const MESSAGE_TYPE: &str = ":message-type";
const CONTENT_TYPE: &str = ":content-type";

impl ContinuationEvent {
    fn into_message(self) -> Message {
        let _ = self;
        let headers = const_headers(&[
            (EVENT_TYPE, "Cont"),    //
            (MESSAGE_TYPE, "event"), //
        ]);
        let payload = None;
        Message { headers, payload }
    }
}

impl EndEvent {
    fn into_message(self) -> Message {
        let _ = self;
        let headers = const_headers(&[
            (EVENT_TYPE, "End"),     //
            (MESSAGE_TYPE, "event"), //
        ]);
        let payload = None;
        Message { headers, payload }
    }
}

impl ProgressEvent {
    fn into_message(self) -> Message {
        let headers = const_headers(&[
            (EVENT_TYPE, "Progress"),   //
            (CONTENT_TYPE, "text/xml"), //
            (MESSAGE_TYPE, "event"),    //
        ]);
        let payload = self.details.as_ref().map(xml_payload);
        Message { headers, payload }
    }
}

impl RecordsEvent {
    fn into_message(self) -> Message {
        let headers = const_headers(&[
            (EVENT_TYPE, "Records"),                    //
            (CONTENT_TYPE, "application/octet-stream"), //
            (MESSAGE_TYPE, "event"),                    //
        ]);
        let payload = self.payload;
        Message { headers, payload }
    }
}

impl StatsEvent {
    fn into_message(self) -> Message {
        let headers = const_headers(&[
            (EVENT_TYPE, "Stats"),      //
            (CONTENT_TYPE, "text/xml"), //
            (MESSAGE_TYPE, "event"),    //
        ]);
        let payload = self.details.as_ref().map(xml_payload);
        Message { headers, payload }
    }
}

fn const_headers(hs: &'static [(&'static str, &'static str)]) -> SmallVec<[Header; 4]> {
    let mut ans = SmallVec::with_capacity(hs.len());
    for (name, value) in hs {
        ans.push(header(static_str(name), static_str(value)));
    }
    ans
}

fn xml_payload<T: xml::Serialize>(val: &T) -> Bytes {
    let mut buf = Vec::with_capacity(256);
    {
        let mut ser = xml::Serializer::new(&mut buf);
        ser.decl()
            .and_then(|()| val.serialize(&mut ser))
            .expect("infallible serialization");
    }
    buf.into()
}

fn request_level_error(e: &S3Error) -> Message {
    let code = match e.code().as_static_str() {
        Some(s) => static_str(s),
        None => match e.code() {
            S3ErrorCode::Custom(s) => s.as_bytes().clone(),
            _ => unreachable!(),
        },
    };

    let message = e.message().map_or_else(Bytes::new, |s| Bytes::copy_from_slice(s.as_bytes()));

    let mut headers = SmallVec::with_capacity(3);
    headers.push(header(static_str(":error-code"), code));
    headers.push(header(static_str(":error-message"), message));
    headers.push(header(static_str(MESSAGE_TYPE), static_str("error")));
    Message { headers, payload: None }
}

#[inline]
fn static_str(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

#[inline]
fn header(name: Bytes, value: Bytes) -> Header {
    Header { name, value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn parse_message(data: &[u8]) -> (Vec<(String, String)>, Option<Vec<u8>>) {
        assert!(data.len() >= 16, "message too short");
        let total_len = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
        let headers_len = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
        assert_eq!(data.len(), total_len);

        let prelude_crc_expected = Crc32::checksum_u32(&data[..8]);
        let prelude_crc = u32::from_be_bytes(data[8..12].try_into().unwrap());
        assert_eq!(prelude_crc, prelude_crc_expected);

        let message_crc_expected = Crc32::checksum_u32(&data[..total_len - 4]);
        let message_crc = u32::from_be_bytes(data[total_len - 4..total_len].try_into().unwrap());
        assert_eq!(message_crc, message_crc_expected);

        let mut headers = Vec::new();
        let mut offset = 12;
        let headers_end = 12 + headers_len;
        while offset < headers_end {
            let name_len = data[offset] as usize;
            offset += 1;
            let name = std::str::from_utf8(&data[offset..offset + name_len]).unwrap().to_owned();
            offset += name_len;
            assert_eq!(data[offset], 7); // header value type
            offset += 1;
            let value_len = u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let value = std::str::from_utf8(&data[offset..offset + value_len]).unwrap().to_owned();
            offset += value_len;
            headers.push((name, value));
        }

        let payload_end = total_len - 4;
        let payload = if headers_end < payload_end {
            Some(data[headers_end..payload_end].to_vec())
        } else {
            None
        };

        (headers, payload)
    }

    fn wrapper(stream: SelectObjectContentEventStream) -> Wrapper {
        Wrapper {
            inner: stream,
            pending: SmallVec::new(),
        }
    }

    #[test]
    fn continuation_event_message() {
        let event = SelectObjectContentEvent::Cont(ContinuationEvent {});
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Cont"));
        assert!(headers.iter().any(|(n, v)| n == ":message-type" && v == "event"));
        assert!(payload.is_none());
    }

    #[test]
    fn end_event_message() {
        let event = SelectObjectContentEvent::End(EndEvent {});
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "End"));
        assert!(headers.iter().any(|(n, v)| n == ":message-type" && v == "event"));
        assert!(payload.is_none());
    }

    #[test]
    fn records_event_with_payload() {
        let event = SelectObjectContentEvent::Records(RecordsEvent {
            payload: Some(Bytes::from_static(b"csv,data")),
        });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Records"));
        assert!(
            headers
                .iter()
                .any(|(n, v)| n == ":content-type" && v == "application/octet-stream")
        );
        assert_eq!(payload.unwrap(), b"csv,data");
    }

    #[test]
    fn records_event_no_payload() {
        let event = SelectObjectContentEvent::Records(RecordsEvent { payload: None });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, _payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Records"));
    }

    #[test]
    fn progress_event_no_details() {
        let event = SelectObjectContentEvent::Progress(ProgressEvent { details: None });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Progress"));
        assert!(headers.iter().any(|(n, v)| n == ":content-type" && v == "text/xml"));
        assert!(payload.is_none());
    }

    #[test]
    fn progress_event_with_details() {
        use crate::dto::Progress;
        let event = SelectObjectContentEvent::Progress(ProgressEvent {
            details: Some(Progress {
                bytes_processed: Some(100),
                bytes_returned: Some(50),
                bytes_scanned: Some(200),
            }),
        });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Progress"));
        let payload_bytes = payload.unwrap();
        let xml_str = std::str::from_utf8(&payload_bytes).unwrap();
        assert!(xml_str.contains("Progress"));
    }

    #[test]
    fn stats_event_no_details() {
        let event = SelectObjectContentEvent::Stats(StatsEvent { details: None });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (headers, payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":event-type" && v == "Stats"));
        assert!(headers.iter().any(|(n, v)| n == ":content-type" && v == "text/xml"));
        assert!(payload.is_none());
    }

    #[test]
    fn stats_event_with_details() {
        use crate::dto::Stats;
        let event = SelectObjectContentEvent::Stats(StatsEvent {
            details: Some(Stats {
                bytes_processed: Some(1000),
                bytes_returned: Some(500),
                bytes_scanned: Some(2000),
            }),
        });
        let bytes = event_into_bytes(Ok(event)).unwrap();
        let (_headers, payload) = parse_message(&bytes);
        let payload_bytes = payload.unwrap();
        let xml_str = std::str::from_utf8(&payload_bytes).unwrap();
        assert!(xml_str.contains("Stats"));
    }

    #[test]
    fn request_level_error_static_code() {
        let err = S3Error::with_message(S3ErrorCode::InternalError, "something went wrong");
        let bytes = event_into_bytes(Err(err)).unwrap();
        let (headers, _payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":error-code" && v == "InternalError"));
        assert!(
            headers
                .iter()
                .any(|(n, v)| n == ":error-message" && v == "something went wrong")
        );
        assert!(headers.iter().any(|(n, v)| n == ":message-type" && v == "error"));
    }

    #[test]
    fn request_level_error_custom_code() {
        let err = S3Error::with_message(S3ErrorCode::Custom(bytestring::ByteString::from("CustomErr")), "custom message");
        let bytes = event_into_bytes(Err(err)).unwrap();
        let (headers, _payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":error-code" && v == "CustomErr"));
        assert!(headers.iter().any(|(n, v)| n == ":error-message" && v == "custom message"));
    }

    #[test]
    fn request_level_error_no_message() {
        let err = S3Error::new(S3ErrorCode::InternalError);
        let bytes = event_into_bytes(Err(err)).unwrap();
        let (headers, _payload) = parse_message(&bytes);
        assert!(headers.iter().any(|(n, v)| n == ":error-code" && v == "InternalError"));
        assert!(headers.iter().any(|(n, v)| n == ":error-message" && v.is_empty()));
    }

    #[test]
    fn message_serialize_crc_integrity() {
        let msg = Message {
            headers: const_headers(&[(":event-type", "End"), (":message-type", "event")]),
            payload: None,
        };
        let bytes = msg.serialize().unwrap();
        let data = bytes.as_ref();

        let total_len = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
        assert_eq!(data.len(), total_len);

        let prelude_crc_actual = u32::from_be_bytes(data[8..12].try_into().unwrap());
        let prelude_crc_computed = Crc32::checksum_u32(&data[..8]);
        assert_eq!(prelude_crc_actual, prelude_crc_computed);

        let message_crc_actual = u32::from_be_bytes(data[total_len - 4..total_len].try_into().unwrap());
        let message_crc_computed = Crc32::checksum_u32(&data[..total_len - 4]);
        assert_eq!(message_crc_actual, message_crc_computed);
    }

    #[test]
    fn message_serialize_with_payload() {
        let msg = Message {
            headers: const_headers(&[(":event-type", "Records")]),
            payload: Some(Bytes::from_static(b"payload-data")),
        };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 16);
        let (_, payload) = parse_message(&bytes);
        assert_eq!(payload.unwrap(), b"payload-data");
    }

    #[test]
    fn message_serialize_chunks_reuses_payload() {
        let payload = Bytes::from(vec![1, 2, 3, 4]);
        let payload_ptr = payload.as_ptr();
        let msg = Message {
            headers: const_headers(&[(":event-type", "Records")]),
            payload: Some(payload.clone()),
        };

        let chunks = msg.serialize_chunks().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1], payload);
        assert_eq!(chunks[1].as_ptr(), payload_ptr);

        let mut bytes = Vec::new();
        for chunk in chunks {
            bytes.put(chunk.as_ref());
        }
        let (_, parsed_payload) = parse_message(&bytes);
        assert_eq!(parsed_payload.unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn stream_debug() {
        let stream = SelectObjectContentEventStream::new(futures::stream::empty());
        let debug = format!("{stream:?}");
        assert!(debug.contains("SelectObjectContentEventStream"));
    }

    #[test]
    fn stream_size_hint() {
        let inner = futures::stream::iter(vec![
            Ok(SelectObjectContentEvent::End(EndEvent {})),
            Ok(SelectObjectContentEvent::End(EndEvent {})),
        ]);
        let stream = SelectObjectContentEventStream::new(inner);
        let (lo, hi) = stream.size_hint();
        assert_eq!(lo, 2);
        assert_eq!(hi, Some(2));
    }

    #[tokio::test]
    async fn stream_poll_next() {
        let events = vec![
            Ok(SelectObjectContentEvent::Cont(ContinuationEvent {})),
            Ok(SelectObjectContentEvent::End(EndEvent {})),
        ];
        let mut stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let first = stream.next().await;
        assert!(first.is_some());
        let second = stream.next().await;
        assert!(second.is_some());
        let third = stream.next().await;
        assert!(third.is_none());
    }

    #[tokio::test]
    async fn wrapper_stream_converts_events() {
        let events: Vec<S3Result<SelectObjectContentEvent>> = vec![
            Ok(SelectObjectContentEvent::Cont(ContinuationEvent {})),
            Ok(SelectObjectContentEvent::End(EndEvent {})),
        ];
        let stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let mut wrapper = wrapper(stream);
        let first = wrapper.next().await;
        assert!(first.is_some());
        assert!(first.unwrap().is_ok());

        let second = wrapper.next().await;
        assert!(second.is_some());
        assert!(second.unwrap().is_ok());

        assert!(wrapper.next().await.is_none());
    }

    #[tokio::test]
    async fn wrapper_stream_handles_errors() {
        let events: Vec<S3Result<SelectObjectContentEvent>> = vec![Err(S3Error::new(S3ErrorCode::InternalError))];
        let stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let mut wrapper = wrapper(stream);
        let result = wrapper.next().await.unwrap();
        assert!(result.is_ok()); // errors are serialized as messages, not stream errors
    }

    #[tokio::test]
    async fn into_byte_stream_produces_bytes() {
        let events: Vec<S3Result<SelectObjectContentEvent>> = vec![Ok(SelectObjectContentEvent::End(EndEvent {}))];
        let stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let mut byte_stream = stream.into_byte_stream();
        let chunk = byte_stream.next().await;
        assert!(chunk.is_some());
        assert!(chunk.unwrap().is_ok());
    }

    #[tokio::test]
    async fn into_byte_stream_reuses_records_payload_chunk() {
        let payload = Bytes::from(vec![1, 2, 3, 4]);
        let payload_ptr = payload.as_ptr();
        let events: Vec<S3Result<SelectObjectContentEvent>> = vec![Ok(SelectObjectContentEvent::Records(RecordsEvent {
            payload: Some(payload.clone()),
        }))];
        let stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let mut byte_stream = stream.into_byte_stream();

        let head = byte_stream.next().await.unwrap().unwrap();
        let payload_chunk = byte_stream.next().await.unwrap().unwrap();
        let trailer = byte_stream.next().await.unwrap().unwrap();
        assert!(byte_stream.next().await.is_none());

        assert_eq!(payload_chunk, payload);
        assert_eq!(payload_chunk.as_ptr(), payload_ptr);

        let mut bytes = Vec::new();
        bytes.put(head.as_ref());
        bytes.put(payload_chunk.as_ref());
        bytes.put(trailer.as_ref());
        let (_, parsed_payload) = parse_message(&bytes);
        assert_eq!(parsed_payload.unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn ser_error_display() {
        let e = SerError::LengthOverflow;
        let msg = format!("{e}");
        assert!(msg.contains("LengthOverflow"));

        let e = SerError::IntOverflow(u8::try_from(256u16).unwrap_err());
        let msg = format!("{e}");
        assert!(msg.contains("IntOverflow"));
    }
}

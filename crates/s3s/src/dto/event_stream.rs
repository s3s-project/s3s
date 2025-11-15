use super::SelectObjectContentEvent;
use super::{ContinuationEvent, EndEvent, ProgressEvent, RecordsEvent, StatsEvent};

use crate::S3Error;
use crate::S3Result;
use crate::StdError;
use crate::crypto::{Checksum as _, Crc32};
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
        Box::pin(Wrapper(self))
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

struct Wrapper(SelectObjectContentEventStream);

impl Stream for Wrapper {
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = ready!(Pin::new(&mut self.0).poll_next(cx));
        debug!(?item, "SelectObjectContentEventStream");
        match item {
            Some(ev) => {
                let result = event_into_bytes(ev);
                if let Err(ref err) = result {
                    debug!("SelectObjectContentEventStream: Error: {}", err);
                }
                Poll::Ready(Some(result.map_err(|e| Box::new(e) as StdError)))
            }
            None => Poll::Ready(None),
        }
    }
}

impl ByteStream for Wrapper {}

fn event_into_bytes(ev: S3Result<SelectObjectContentEvent>) -> Result<Bytes, SerError> {
    match ev {
        Ok(event) => event.into_message().serialize(),
        Err(err) => {
            debug!(?err, "SelectObjectContentEventStream: Request Level Error");
            request_level_error(&err).serialize()
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
    fn serialize(self) -> Result<Bytes, SerError> {
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

        let mut buf: Vec<u8> = Vec::with_capacity(total_byte_length as usize);
        buf.put_u32(total_byte_length);
        buf.put_u32(headers_byte_length);

        let prelude_crc = Crc32::checksum(&buf);
        buf.put_u32(u32::from_be_bytes(prelude_crc));

        for h in &self.headers {
            let header_name_byte_length = u8::try_from(h.name.len())?;
            let value_string_byte_length = u16::try_from(h.value.len())?;

            buf.put_u8(header_name_byte_length);
            buf.put(&*h.name);

            buf.put_u8(7);
            buf.put_u16(value_string_byte_length);
            buf.put(&*h.value);
        }

        if let Some(payload) = self.payload.as_deref() {
            buf.put(payload);
        }

        let message_crc = Crc32::checksum(&buf);
        buf.put_u32(u32::from_be_bytes(message_crc));

        Ok(buf.into())
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

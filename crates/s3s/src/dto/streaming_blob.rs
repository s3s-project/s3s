//! Streaming blob

use crate::error::StdError;
use crate::http::Body;
use crate::stream::*;

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use hyper::body::Bytes;

pub struct StreamingBlob {
    inner: DynByteStream,
}

impl StreamingBlob {
    pub fn new<S>(stream: S) -> Self
    where
        S: ByteStream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
    {
        Self { inner: Box::pin(stream) }
    }

    pub fn wrap<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        Self { inner: wrap(stream) }
    }

    fn into_inner(self) -> DynByteStream {
        self.inner
    }
}

impl fmt::Debug for StreamingBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingBlob")
            .field("remaining_length", &self.remaining_length())
            .finish_non_exhaustive()
    }
}

impl Stream for StreamingBlob {
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ByteStream for StreamingBlob {
    fn remaining_length(&self) -> RemainingLength {
        self.inner.remaining_length()
    }
}

impl From<StreamingBlob> for DynByteStream {
    fn from(value: StreamingBlob) -> Self {
        value.into_inner()
    }
}

impl From<DynByteStream> for StreamingBlob {
    fn from(value: DynByteStream) -> Self {
        Self { inner: value }
    }
}

impl From<StreamingBlob> for Body {
    fn from(value: StreamingBlob) -> Self {
        Body::from(value.into_inner())
    }
}

impl From<Body> for StreamingBlob {
    fn from(value: Body) -> Self {
        Self::new(value)
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct StreamWrapper<S> {
        #[pin]
        inner: S
    }
}

impl<S, E> Stream for StreamWrapper<S>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, StdError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx).map_err(|e| Box::new(e) as StdError)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S, E> ByteStream for StreamWrapper<S>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    fn remaining_length(&self) -> RemainingLength {
        let (lower, upper) = self.inner.size_hint();
        RemainingLength::new(lower, upper)
    }
}

fn wrap<S>(inner: S) -> DynByteStream
where
    StreamWrapper<S>: ByteStream<Item = Result<Bytes, StdError>> + Send + Sync + 'static,
{
    Box::pin(StreamWrapper { inner })
}

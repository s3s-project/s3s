use crate::error::*;

use s3s::StdError;
use s3s::stream::{ByteStream, RemainingLength};

use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use bytes::Bytes;
use futures::pin_mut;
use futures::{Stream, StreamExt};
use transform_stream::AsyncTryStream;
use std::pin::Pin;
use std::task::{Context, Poll};

pub async fn copy_bytes<S, W>(mut stream: S, writer: &mut W) -> Result<u64>
where
    S: Stream<Item = Result<Bytes, StdError>> + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut nwritten: u64 = 0;
    while let Some(result) = stream.next().await {
        let bytes = match result {
            Ok(x) => x,
            Err(e) => return Err(Error::new(e)),
        };
        writer.write_all(&bytes).await?;
        nwritten += bytes.len() as u64;
    }
    writer.flush().await?;
    Ok(nwritten)
}

pub fn bytes_stream<S, E>(stream: S, content_length: usize) -> impl Stream<Item = Result<Bytes, E>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    AsyncTryStream::<Bytes, E, _>::new(|mut y| async move {
        pin_mut!(stream);
        let mut remaining: usize = content_length;
        while let Some(result) = stream.next().await {
            let mut bytes = result?;
            if bytes.len() > remaining {
                bytes.truncate(remaining);
            }
            remaining -= bytes.len();
            y.yield_ok(bytes).await;
        }
        Ok(())
    })
}

pub fn hex(input: impl AsRef<[u8]>) -> String {
    hex_simd::encode_to_string(input.as_ref(), hex_simd::AsciiCase::Lower)
}

pin_project_lite::pin_project! {
    /// A wrapper that implements ByteStream with known content length
    pub struct SizedByteStream<S> {
        #[pin]
        inner: S,
        initial_length: usize,
        consumed: usize,
    }
}

impl<S> SizedByteStream<S> {
    pub fn new(stream: S, content_length: usize) -> Self {
        Self {
            inner: stream,
            initial_length: content_length,
            consumed: 0,
        }
    }
}

impl<S, E> Stream for SizedByteStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx).map(|opt| {
            opt.map(|result| {
                result.inspect(|bytes| {
                    *this.consumed += bytes.len();
                })
            })
        })
    }
}

impl<S, E> ByteStream for SizedByteStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    fn remaining_length(&self) -> RemainingLength {
        let remaining = self.initial_length.saturating_sub(self.consumed);
        RemainingLength::new_exact(remaining)
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::timer::{self, Interval};
use crate::{StdError, http::Response};

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};

// TODO: we can simplify this body type if the client does not support trailers (?)

// sends whitespace while the future is pending
pin_project_lite::pin_project! {

    pub struct KeepAliveBody<F> {
        #[pin]
        inner: F,
        initial_body: Option<Bytes>,
        response: Option<Response>,
        interval: Option<Interval>,
        done: bool,
        allow_trailers: bool,
    }
}

impl<F> KeepAliveBody<F> {
    pub fn new(inner: F, interval: Duration, initial_body: Option<Bytes>, allow_trailers: bool) -> Self {
        if !timer::available() {
            // The response is still valid, it just does not send whitespace while the
            // future is pending.
            tracing::debug!("keep-alive padding is disabled: no timer backend");
        }
        Self {
            inner,
            initial_body,
            response: None,
            interval: timer::interval(interval),
            done: false,
            allow_trailers,
        }
    }
}

impl<F> Body for KeepAliveBody<F>
where
    F: Future<Output = Result<Response, StdError>>,
{
    type Data = Bytes;

    type Error = StdError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        let mut this = self.project();
        if let Some(initial_body) = this.initial_body.take() {
            cx.waker().wake_by_ref();
            return Poll::Ready(Some(Ok(Frame::data(initial_body))));
        }
        loop {
            if let Some(response) = &mut *this.response {
                let frame = std::task::ready!(Pin::new(&mut response.body).poll_frame(cx)?);
                if let Some(frame) = frame {
                    return Poll::Ready(Some(Ok(frame)));
                }
                *this.done = true;

                if *this.allow_trailers {
                    let trailers = Frame::trailers(std::mem::take(&mut response.headers));
                    return Poll::Ready(Some(Ok(trailers)));
                }
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll(cx) {
                Poll::Ready(response) => match response {
                    Ok(response) => {
                        *this.response = Some(response);
                    }
                    Err(e) => {
                        *this.done = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                },
                Poll::Pending => {
                    // Without a tick source the body stays pending here and the inner
                    // future drives the response.
                    let Some(interval) = this.interval.as_mut() else {
                        return Poll::Pending;
                    };
                    std::task::ready!(interval.poll_tick(cx));
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b" ")))));
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use hyper::{StatusCode, header::HeaderValue};

    use super::*;

    #[tokio::test]
    async fn keep_alive_body() {
        let body = KeepAliveBody::new(
            async {
                let mut res = Response::with_status(StatusCode::OK);
                res.body = Bytes::from_static(b" world").into();
                res.headers.insert("key", HeaderValue::from_static("value"));
                Ok(res)
            },
            Duration::from_secs(1),
            Some(Bytes::from_static(b"hello")),
            true,
        );

        let aggregated = body.collect().await.unwrap();

        assert_eq!(aggregated.trailers().unwrap().get("key").unwrap(), "value");

        let buf = aggregated.to_bytes();

        assert_eq!(buf, b"hello world".as_slice());
    }

    #[tokio::test]
    async fn keep_alive_body_no_initial() {
        let body = KeepAliveBody::new(
            async {
                let mut res = Response::with_status(StatusCode::OK);
                res.body = Bytes::from_static(b"hello world").into();
                Ok(res)
            },
            Duration::from_secs(1),
            None,
            false,
        );

        let aggregated = body.collect().await.unwrap();

        let buf = aggregated.to_bytes();

        assert_eq!(buf, b"hello world".as_slice());
    }

    #[cfg(any(feature = "tokio-timer", feature = "futures-timer"))]
    #[tokio::test]
    async fn keep_alive_body_fill_withespace() {
        let body = KeepAliveBody::new(
            async {
                tokio::time::sleep(Duration::from_millis(450)).await;

                let mut res = Response::with_status(StatusCode::OK);
                res.body = Bytes::from_static(b"hello world").into();
                Ok(res)
            },
            Duration::from_millis(100),
            None,
            false,
        );

        assert!(body.interval.is_some());

        let aggregated = body.collect().await.unwrap();

        let buf = aggregated.to_bytes();

        // tokio's interval ticks immediately and keeps a steady cadence, so the
        // number of ticks is stable. The futures-timer backend runs on a helper
        // thread, whose cadence depends on the machine: a loaded macOS runner
        // produced only two ticks here, so for that backend only require that the
        // whitespace happened and the document itself is intact.
        #[cfg(not(feature = "futures-timer"))]
        {
            let ans1 = b"     hello world";
            let ans2 = b"    hello world";

            assert!(buf.as_ref() == ans1 || buf.as_ref() == ans2, "buf: {buf:?}");
        }
        #[cfg(feature = "futures-timer")]
        {
            let padding = buf.iter().take_while(|byte| **byte == b' ').count();

            assert!(padding > 0, "buf: {buf:?}");
            assert_eq!(&buf[padding..], b"hello world".as_slice(), "buf: {buf:?}");
        }
    }

    /// The point of the futures-timer backend: padding works without a tokio runtime.
    #[cfg(feature = "futures-timer")]
    #[test]
    fn keep_alive_body_pads_without_a_tokio_runtime() {
        let body = KeepAliveBody::new(
            async {
                futures_timer::Delay::new(Duration::from_millis(450)).await;

                let mut res = Response::with_status(StatusCode::OK);
                res.body = Bytes::from_static(b"hello world").into();
                Ok(res)
            },
            Duration::from_millis(100),
            None,
            false,
        );

        assert!(body.interval.is_some());

        let buf = futures::executor::block_on(body.collect()).unwrap().to_bytes();

        let padding = buf.iter().take_while(|byte| **byte == b' ').count();

        assert!(padding > 0, "buf: {buf:?}");
        assert_eq!(&buf[padding..], b"hello world".as_slice(), "buf: {buf:?}");
    }

    /// Without any timer backend the body must stay valid: no padding, no panic.
    #[cfg(not(any(feature = "tokio-timer", feature = "futures-timer")))]
    #[test]
    fn keep_alive_body_without_a_timer_has_no_padding() {
        let body = KeepAliveBody::new(
            async {
                yield_once().await;

                let mut res = Response::with_status(StatusCode::OK);
                res.body = Bytes::from_static(b"hello world").into();
                Ok(res)
            },
            Duration::from_millis(1),
            None,
            false,
        );

        // No timer is compiled in, so the body holds no interval at all.
        assert!(body.interval.is_none());

        let buf = futures::executor::block_on(body.collect()).unwrap().to_bytes();

        assert_eq!(buf.as_ref(), b"hello world");
        assert!(!crate::timer::available());
    }

    /// Resolves Pending once so the body is polled again before the response is ready.
    #[cfg(not(any(feature = "tokio-timer", feature = "futures-timer")))]
    async fn yield_once() {
        let mut yielded = false;
        futures::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }
}

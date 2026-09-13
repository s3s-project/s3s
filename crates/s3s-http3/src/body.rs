// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use bytes::{Buf, Bytes};
use h3::error::Code;
use h3::server::RequestStream;
use http_body::{Body as HttpBody, Frame, SizeHint};

use std::pin::Pin;
use std::task::{Context, Poll};

type RecvStream = RequestStream<h3_quinn::RecvStream, Bytes>;

enum State {
    Data,
    Trailers,
    Done,
}

pub(crate) struct Body {
    stream: Option<RecvStream>,
    state: State,
    expected_length: Option<u64>,
    received_length: u64,
}

#[derive(Debug)]
pub(crate) struct BodyError(Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for BodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl Body {
    pub(crate) fn new(stream: RecvStream, expected_length: Option<u64>) -> Self {
        Self {
            stream: Some(stream),
            state: State::Data,
            expected_length,
            received_length: 0,
        }
    }
}

fn content_length_error(expected: u64, actual: u64) -> BodyError {
    BodyError(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("HTTP/3 request body length mismatch: expected {expected}, received {actual}"),
    )))
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        loop {
            match this.state {
                State::Data => {
                    let Some(stream) = this.stream.as_mut() else {
                        this.state = State::Done;
                        return Poll::Ready(None);
                    };

                    match stream.poll_recv_data(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            return Poll::Ready(Some(Err(BodyError(Box::new(error)))));
                        }
                        Poll::Ready(Ok(Some(mut data))) => {
                            let data = data.copy_to_bytes(data.remaining());
                            if let Some(expected) = this.expected_length {
                                let received = this.received_length.saturating_add(data.len() as u64);
                                if received > expected {
                                    stream.stop_sending(Code::H3_MESSAGE_ERROR);
                                    return Poll::Ready(Some(Err(content_length_error(expected, received))));
                                }
                                this.received_length = received;
                            }
                            return Poll::Ready(Some(Ok(Frame::data(data))));
                        }
                        Poll::Ready(Ok(None)) => {
                            if let Some(expected) = this.expected_length
                                && this.received_length != expected
                            {
                                stream.stop_sending(Code::H3_MESSAGE_ERROR);
                                return Poll::Ready(Some(Err(content_length_error(expected, this.received_length))));
                            }
                            this.state = State::Trailers;
                        }
                    }
                }
                State::Trailers => {
                    let Some(stream) = this.stream.as_mut() else {
                        this.state = State::Done;
                        return Poll::Ready(None);
                    };

                    match stream.poll_recv_trailers(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            return Poll::Ready(Some(Err(BodyError(Box::new(error)))));
                        }
                        Poll::Ready(Ok(Some(trailers))) => {
                            this.state = State::Done;
                            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
                        }
                        Poll::Ready(Ok(None)) => {
                            this.state = State::Done;
                            return Poll::Ready(None);
                        }
                    }
                }
                State::Done => return Poll::Ready(None),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, State::Done)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if matches!(self.state, State::Done) {
            return;
        }

        let Some(mut stream) = self.stream.take() else {
            return;
        };

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let _drain_task = handle.spawn(async move {
            loop {
                match stream.recv_data().await {
                    Ok(Some(_data)) => {}
                    Ok(None) => {
                        let _ = stream.recv_trailers().await;
                        break;
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

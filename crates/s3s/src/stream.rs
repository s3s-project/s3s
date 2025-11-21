use crate::error::StdError;

use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;

pub trait ByteStream: Stream {
    fn remaining_length(&self) -> RemainingLength {
        RemainingLength::unknown()
    }
}

pub type DynByteStream = Pin<Box<dyn ByteStream<Item = Result<Bytes, StdError>> + Send + Sync + 'static>>;

pub struct RemainingLength {
    lower: usize,
    upper: Option<usize>,
}

impl RemainingLength {
    /// Creates a new `RemainingLength` with the given lower and upper bounds.
    ///
    /// # Panics
    /// This function asserts that `lower <= upper`.
    #[must_use]
    pub fn new(lower: usize, upper: Option<usize>) -> Self {
        if let Some(upper) = upper {
            assert!(lower <= upper);
        }
        Self { lower, upper }
    }

    #[must_use]
    pub fn unknown() -> Self {
        Self { lower: 0, upper: None }
    }

    #[must_use]
    pub fn new_exact(n: usize) -> Self {
        Self {
            lower: n,
            upper: Some(n),
        }
    }

    #[must_use]
    pub fn exact(&self) -> Option<usize> {
        self.upper.filter(|&upper| upper == self.lower)
    }

    #[must_use]
    fn into_size_hint(self) -> http_body::SizeHint {
        let mut sz = http_body::SizeHint::new();
        sz.set_lower(self.lower as u64);
        if let Some(upper) = self.upper {
            sz.set_upper(upper as u64);
        }
        sz
    }

    #[must_use]
    fn from_size_hint(sz: &http_body::SizeHint) -> Self {
        // inaccurate conversion on 32-bit platforms
        let lower = usize::try_from(sz.lower()).unwrap_or(usize::MAX);
        let upper = sz.upper().and_then(|x| usize::try_from(x).ok());
        Self { lower, upper }
    }
}

impl From<RemainingLength> for http_body::SizeHint {
    fn from(value: RemainingLength) -> Self {
        value.into_size_hint()
    }
}

impl From<http_body::SizeHint> for RemainingLength {
    fn from(value: http_body::SizeHint) -> Self {
        Self::from_size_hint(&value)
    }
}

impl fmt::Debug for RemainingLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(exact) = self.exact() {
            return write!(f, "{exact}");
        }
        match self.upper {
            Some(upper) => write!(f, "({}..={})", self.lower, upper),
            None => write!(f, "({}..)", self.lower),
        }
    }
}

pub(crate) fn into_dyn<S, E>(s: S) -> DynByteStream
where
    S: ByteStream<Item = Result<Bytes, E>> + Send + Sync + Unpin + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    Box::pin(Wrapper(s))
}

struct Wrapper<S>(S);

impl<S, E> Stream for Wrapper<S>
where
    S: ByteStream<Item = Result<Bytes, E>> + Send + Sync + Unpin + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::new(&mut self.0);
        this.poll_next(cx).map_err(|e| Box::new(e) as StdError)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<S, E> ByteStream for Wrapper<S>
where
    S: ByteStream<Item = Result<Bytes, E>> + Send + Sync + Unpin + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    fn remaining_length(&self) -> RemainingLength {
        self.0.remaining_length()
    }
}

pub(crate) struct VecByteStream {
    queue: VecDeque<Bytes>,
    remaining_bytes: usize,
}

impl VecByteStream {
    pub fn new(v: Vec<Bytes>) -> Self {
        let total = v
            .iter()
            .map(Bytes::len)
            .try_fold(0, usize::checked_add)
            .expect("length overflow");

        Self {
            queue: v.into(),
            remaining_bytes: total,
        }
    }

    pub fn exact_remaining_length(&self) -> usize {
        self.remaining_bytes
    }
}

impl Stream for VecByteStream {
    type Item = Result<Bytes, StdError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        match this.queue.pop_front() {
            Some(b) => {
                this.remaining_bytes -= b.len();
                Poll::Ready(Some(Ok(b)))
            }
            None => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let cnt = self.queue.len();
        (cnt, Some(cnt))
    }
}

impl ByteStream for VecByteStream {
    fn remaining_length(&self) -> RemainingLength {
        RemainingLength::new_exact(self.remaining_bytes)
    }
}

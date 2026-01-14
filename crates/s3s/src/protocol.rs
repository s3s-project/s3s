use crate::Body;
use crate::StdError;
use crate::auth::Credentials;

use http::Extensions;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use http::Uri;

use stdx::default::default;

pub type HttpRequest<B = Body> = http::Request<B>;
pub type HttpResponse<B = Body> = http::Response<B>;

/// An error that indicates a failure of an HTTP request.
/// Passing this error to `hyper` will cause it to abort the connection.
#[derive(Debug)]
pub struct HttpError(StdError);

impl HttpError {
    #[must_use]
    pub fn new(err: StdError) -> Self {
        Self(err)
    }
}

impl From<HttpError> for StdError {
    fn from(val: HttpError) -> Self {
        val.0
    }
}

/// Trailing headers handle (newtype)
///
/// This handle lets you take streaming-trailer headers after the
/// request body stream has been fully consumed.
#[derive(Clone)]
pub struct TrailingHeaders(pub(crate) std::sync::Arc<std::sync::Mutex<Option<HeaderMap>>>);

impl core::fmt::Debug for TrailingHeaders {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ready = self.is_ready();
        f.debug_struct("TrailingHeaders")
            .field("ready", &ready)
            .finish_non_exhaustive()
    }
}

impl TrailingHeaders {
    /// Returns true if trailers have been produced by the body stream.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.0.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Take the trailing headers if available.
    ///
    /// This is a one-shot operation; subsequent calls will return None.
    #[must_use]
    pub fn take(&self) -> Option<HeaderMap> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }

    /// Read the trailing headers if available, without taking them.
    pub fn read<R>(&self, f: impl FnOnce(&HeaderMap) -> R) -> Option<R> {
        if let Ok(guard) = self.0.lock()
            && let Some(ref headers) = *guard
        {
            return Some(f(headers));
        }
        None
    }
}

/// S3 request
#[derive(Debug, Clone)]
pub struct S3Request<T> {
    /// S3 operation input
    pub input: T,

    /// HTTP method
    pub method: Method,

    /// HTTP URI
    pub uri: Uri,

    /// HTTP headers
    pub headers: HeaderMap,

    /// Request extensions.
    /// This field is used to pass custom data between middlewares.
    pub extensions: Extensions,

    /// S3 identity information.
    /// `None` means anonymous request.
    pub credentials: Option<Credentials>,

    /// S3 requested region.
    pub region: Option<String>,

    /// S3 requested service.
    pub service: Option<String>,

    /// Streaming trailers handle.
    /// When the request body uses AWS `SigV4` streaming with trailers, this
    /// handle allows retrieving the verified trailing headers after the body
    /// stream is fully read.
    pub trailing_headers: Option<TrailingHeaders>,
}

impl<T> S3Request<T> {
    /// Map the input of the request to a new type.
    pub fn map_input<U>(self, f: impl FnOnce(T) -> U) -> S3Request<U> {
        S3Request {
            input: f(self.input),
            method: self.method,
            uri: self.uri,
            headers: self.headers,
            extensions: self.extensions,
            credentials: self.credentials,
            region: self.region,
            service: self.service,
            trailing_headers: self.trailing_headers,
        }
    }
}

/// S3 response
#[derive(Debug, Clone)]
pub struct S3Response<T> {
    /// S3 operation output
    pub output: T,

    /// HTTP status code.
    /// This field overrides the status code implied by the output.
    pub status: Option<StatusCode>,

    /// HTTP headers.
    /// This field overrides the headers implied by the output.
    pub headers: HeaderMap,

    /// Response extensions.
    /// This is used to pass custom data between middlewares.
    pub extensions: Extensions,
}

impl<T> S3Response<T> {
    /// Create a new S3 response with the given output.
    pub fn new(output: T) -> Self {
        Self {
            output,
            status: default(),
            headers: default(),
            extensions: default(),
        }
    }

    /// Create a new S3 response with the given output and status code.
    pub fn with_status(output: T, status: StatusCode) -> Self {
        Self {
            output,
            status: Some(status),
            headers: default(),
            extensions: default(),
        }
    }

    /// Create a new S3 response with the given output and headers.
    pub fn with_headers(output: T, headers: HeaderMap) -> Self {
        Self {
            output,
            status: default(),
            headers,
            extensions: default(),
        }
    }

    /// Map the output of the response to a new type.
    pub fn map_output<U>(self, f: impl FnOnce(T) -> U) -> S3Response<U> {
        S3Response {
            output: f(self.output),
            status: self.status,
            headers: self.headers,
            extensions: self.extensions,
        }
    }
}

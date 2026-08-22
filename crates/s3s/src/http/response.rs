// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use crate::HttpResponse;

use super::Body;

use hyper::HeaderMap;
use hyper::StatusCode;
use hyper::http::Extensions;

#[derive(Default)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
    pub extensions: Extensions,
}

impl From<Response> for HttpResponse {
    fn from(res: Response) -> Self {
        let mut ans = HttpResponse::default();
        *ans.status_mut() = res.status;
        *ans.headers_mut() = res.headers;
        *ans.body_mut() = res.body;
        *ans.extensions_mut() = res.extensions;
        ans
    }
}

impl Response {
    #[must_use]
    pub fn with_status(status: StatusCode) -> Self {
        Self {
            status,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use hyper::header::HeaderValue;

    #[test]
    fn with_status_sets_status_and_preserves_default_state() {
        let res = Response::with_status(StatusCode::CREATED);

        assert_eq!(res.status, StatusCode::CREATED);
        assert!(res.headers.is_empty());
        assert!(http_body::Body::is_end_stream(&res.body));
        assert!(res.extensions.get::<usize>().is_none());
    }

    #[test]
    fn into_http_response_preserves_headers_body_and_extensions() {
        let mut res = Response::with_status(StatusCode::ACCEPTED);
        res.headers.insert("x-test", HeaderValue::from_static("value"));
        res.extensions.insert::<usize>(42);
        res.body = Body::from(Bytes::from_static(b"body"));

        let ans: HttpResponse = res.into();

        assert_eq!(ans.status(), StatusCode::ACCEPTED);
        assert_eq!(ans.headers().get("x-test").unwrap().to_str().unwrap(), "value");
        assert_eq!(ans.extensions().get::<usize>(), Some(&42));
        assert_eq!(http_body::Body::size_hint(ans.body()).exact(), Some(4));
    }
}

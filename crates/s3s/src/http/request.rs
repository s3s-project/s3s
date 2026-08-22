// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use super::Body;
use super::Multipart;
use super::OrderedQs;

use crate::HttpRequest;
use crate::auth::Credentials;
use crate::path::S3Path;
use crate::post_policy::PostPolicy;
use crate::protocol::TrailingHeaders;
use crate::region::Region;
use crate::stream::DynByteStream;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use hyper::http::Extensions;

pub struct Request {
    pub version: http::Version,
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub extensions: Extensions,
    pub body: Body,
    pub(crate) s3ext: S3Extensions,
}

#[derive(Default)]
pub(crate) struct S3Extensions {
    pub s3_path: Option<S3Path>,
    pub qs: Option<OrderedQs>,

    pub multipart: Option<Multipart>,
    pub post_object_stream: Option<DynByteStream>,

    pub credentials: Option<Credentials>,
    pub region: Option<Region>,
    pub service: Option<String>,
    pub trailing_headers: Option<TrailingHeaders>,

    pub post_policy: Option<PostPolicy>,
}

impl From<HttpRequest> for Request {
    fn from(req: HttpRequest) -> Self {
        let (parts, body) = req.into_parts();
        Self {
            version: parts.version,
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            extensions: parts.extensions,
            body,
            s3ext: S3Extensions::default(),
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Coverage for unions whose variant is unknown to the generated conversion table.
//!
//! The SDK reports an unrecognized union member as a fieldless `Unknown` variant, which
//! cannot be constructed outside the SDK crate, so the test drives the real SDK
//! deserializer with a canned HTTP response and feeds the result to the conversion layer.

use super::*;

use s3s::S3ErrorCode;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region, SharedCredentialsProvider};
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest as AwsHttpRequest, HttpResponse as AwsHttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;

use hyper::http;

/// An analytics configuration whose filter carries a member the SDK does not model.
///
/// The root element is part of the S3 protocol for this operation, and `Filter` is a
/// union, so the unknown member makes the SDK report `AnalyticsFilter::Unknown`.
const UNKNOWN_FILTER_XML: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    r#"<AnalyticsConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    "<Id>probe</Id>",
    "<Filter><UnexpectedMember>1</UnexpectedMember></Filter>",
    "</AnalyticsConfiguration>",
);

#[derive(Debug, Clone, Copy)]
struct CannedResponse(&'static str);

impl HttpClient for CannedResponse {
    fn http_connector(&self, _: &HttpConnectorSettings, _: &RuntimeComponents) -> SharedHttpConnector {
        SharedHttpConnector::new(CannedConnector(self.0))
    }
}

#[derive(Debug, Clone, Copy)]
struct CannedConnector(&'static str);

impl HttpConnector for CannedConnector {
    fn call(&self, _: AwsHttpRequest) -> HttpConnectorFuture {
        let body = self.0;
        HttpConnectorFuture::new_boxed(Box::pin(async move {
            let response = http::Response::builder()
                .status(200)
                .header(http::header::CONTENT_TYPE, "application/xml")
                .body(SdkBody::from(body))
                .map_err(|err| ConnectorError::other(Box::new(err), None))?;
            AwsHttpResponse::try_from(response).map_err(|err| ConnectorError::other(Box::new(err), None))
        }))
    }
}

fn canned_client() -> aws_sdk_s3::Client {
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .credentials_provider(SharedCredentialsProvider::new(Credentials::new("test", "test", None, None, "test")))
        .region(Region::new("us-east-1"))
        .http_client(CannedResponse(UNKNOWN_FILTER_XML))
        .endpoint_url("http://localhost:9000")
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

#[tokio::test]
async fn unknown_union_variant_is_an_error() {
    let output = canned_client()
        .get_bucket_analytics_configuration()
        .bucket("bucket")
        .id("probe")
        .send()
        .await
        .expect("the canned response must deserialize");

    let filter = output
        .analytics_configuration
        .as_ref()
        .and_then(|config| config.filter.as_ref())
        .expect("the canned response carries a filter");
    assert!(filter.is_unknown(), "the SDK must report an unknown union variant");

    let err = try_from_aws::<s3s::dto::GetBucketAnalyticsConfigurationOutput>(output)
        .expect_err("an unknown union variant must return an error instead of panicking");
    assert_eq!(err.code(), &S3ErrorCode::InternalError);
    assert_eq!(err.message(), Some("unknown union variant: aws_sdk_s3::types::AnalyticsFilter"));
}

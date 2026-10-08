// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The POST-specific parsing and serialization of the `PostObject` operation.
//!
//! The form fields, the policy and the `success_action_*` response semantics
//! live here; the generated operation module delegates to these functions.

use crate::dto::ETag;
use crate::dto::PostObjectInput;
use crate::dto::PostObjectOutput;
use crate::dto::put_object_input_into_post_object_input;
use crate::error::*;
use crate::http;
use crate::ops::PutObject;

pub(crate) fn deserialize_http(req: &mut http::Request) -> S3Result<PostObjectInput> {
    let Some(m) = req.s3ext.multipart.take() else {
        return Err(invalid_request!("missing multipart form"));
    };

    // Parse POST-specific fields before consuming the multipart form
    let success_action_redirect: Option<String> = match http::parse_field_value(&m, "success_action_redirect")? {
        Some(v) => Some(v),
        None => http::parse_field_value(&m, "redirect")?,
    };
    let success_action_status: Option<i32> = http::parse_field_value(&m, "success_action_status")?;

    // Get the validated POST policy from request extensions
    let policy = req.s3ext.post_policy.take();

    let put_input = PutObject::deserialize_http_multipart(req, m)?;
    let mut post_input = put_object_input_into_post_object_input(put_input);
    post_input.success_action_redirect = success_action_redirect;
    post_input.success_action_status = success_action_status;
    post_input.policy = policy;
    Ok(post_input)
}

pub(crate) fn serialize_http(
    bucket: &str,
    key: &str,
    success_action_redirect: Option<&str>,
    success_action_status: Option<i32>,
    output: &PostObjectOutput,
) -> S3Result<http::Response> {
    let etag_str = output.e_tag.as_ref().map(ETag::value).unwrap_or_default();

    // Handle success_action_redirect: return 303 See Other with Location header
    if let Some(redirect_url) = success_action_redirect {
        // Defense-in-depth: Reject URLs with control characters that could enable header injection
        if redirect_url.chars().any(char::is_control) {
            return Err(s3_error!(InvalidArgument, "success_action_redirect contains invalid control characters"));
        }

        // Parse the URL to validate and manipulate it properly
        let mut url = url::Url::parse(redirect_url).map_err(|e| s3_error!(e, InvalidArgument, "Invalid redirect URL"))?;

        // Add query parameters (bucket, key, etag) to the URL
        url.query_pairs_mut()
            .append_pair("bucket", bucket)
            .append_pair("key", key)
            .append_pair("etag", etag_str);

        let mut res = http::Response::with_status(http::StatusCode::SEE_OTHER);
        res.headers
            .insert(hyper::header::LOCATION, url.as_str().parse().map_err(|e| s3_error!(e, InternalError))?);
        return Ok(res);
    }

    let mut res = match success_action_status {
        Some(201) => {
            // 201 Created with XML body using PostResponse DTO
            let location = format!("/{bucket}/{key}");
            let post_response = crate::dto::PostResponse {
                location: &location,
                bucket,
                key,
                etag: etag_str,
            };
            let mut res = http::Response::with_status(http::StatusCode::CREATED);
            http::set_xml_body(&mut res, &post_response)?;
            res
        }
        Some(200) => http::Response::with_status(http::StatusCode::OK),
        // 204 No Content (default, also for unrecognized values)
        _ => http::Response::with_status(http::StatusCode::NO_CONTENT),
    };

    if let Some(etag) = output.e_tag.as_ref() {
        let etag = etag
            .to_http_header()
            .map_err(|e| s3_error!(e, InternalError, "invalid object tag"))?;
        res.headers.insert(hyper::header::ETAG, etag);
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::S3ErrorCode;
    use hyper::StatusCode;

    fn output_with_etag(value: &str) -> PostObjectOutput {
        PostObjectOutput {
            e_tag: Some(ETag::Strong(value.to_owned())),
            ..Default::default()
        }
    }

    fn assert_no_body(res: &http::Response) {
        let bytes = res.body.bytes();
        assert!(bytes.is_none_or(|bytes| bytes.is_empty()), "the response must not carry a body");
    }

    #[test]
    fn the_default_status_is_204() {
        let res = serialize_http("bucket", "key", None, None, &PostObjectOutput::default()).expect("no status");
        assert_eq!(res.status, StatusCode::NO_CONTENT);
        assert_no_body(&res);
    }

    #[test]
    fn status_200_is_ok_without_a_body() {
        let res = serialize_http("bucket", "key", None, Some(200), &output_with_etag("\"abc\"")).expect("200");
        assert_eq!(res.status, StatusCode::OK);
        assert_no_body(&res);
    }

    #[test]
    fn status_201_writes_the_post_response_xml() {
        let res = serialize_http("bucket", "key", None, Some(201), &output_with_etag("\"abc\"")).expect("201");
        assert_eq!(res.status, StatusCode::CREATED);
        let body = res.body.bytes().expect("the XML body is buffered");
        let body = std::str::from_utf8(&body).expect("the body is UTF-8");
        assert!(body.contains("<Location>/bucket/key</Location>"), "{body}");
        assert!(body.contains("<Bucket>bucket</Bucket>"), "{body}");
        assert!(body.contains("<Key>key</Key>"), "{body}");
        assert!(body.contains("<ETag>&quot;abc&quot;</ETag>"), "{body}");
    }

    #[test]
    fn an_unrecognized_status_is_204() {
        let res = serialize_http("bucket", "key", None, Some(418), &PostObjectOutput::default()).expect("418");
        assert_eq!(res.status, StatusCode::NO_CONTENT);
        assert_no_body(&res);
    }

    #[test]
    fn a_control_character_in_the_redirect_is_rejected() {
        // The WHATWG URL parser strips tabs, so this sample only fails because of the
        // explicit control-character check.
        let redirect = "https://example.com/a\tb";
        assert!(url::Url::parse(redirect).is_ok(), "the URL parser accepts the sample");

        let err = serialize_http("bucket", "key", Some(redirect), None, &PostObjectOutput::default())
            .err()
            .expect("a control character must be rejected");
        assert_eq!(*err.code(), S3ErrorCode::InvalidArgument);
    }

    #[test]
    fn an_unparsable_redirect_is_an_invalid_argument() {
        let err = serialize_http("bucket", "key", Some("not a url"), None, &PostObjectOutput::default())
            .err()
            .expect("a relative URL must be rejected");
        assert_eq!(*err.code(), S3ErrorCode::InvalidArgument);
    }

    /// Builds a prepared POST request whose form carries one extra field.
    ///
    /// The policy conditions cover the extra field, so prepare accepts the form.
    async fn prepared_post_request(extra_field: (&str, &str)) -> crate::http::Request {
        use crate::auth::SecretKey;
        use crate::ops::tests::common::post_policy_test_helpers as post;
        use std::sync::Arc;

        let (name, value) = extra_field;
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post::TestS3NoOp);
        let config = post::create_test_config(1024 * 1024);
        let auth = post::create_test_auth();
        let ccx = post::create_test_context(&s3, &config, &auth);
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

        let field_ref = ["$", name].concat();
        let policy_json = format!(
            r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["eq","{}","{}"],{}]}}"#,
            field_ref,
            value,
            post::BASE_CONDITIONS,
        );
        let mut req =
            post::build_post_object_request_with(policy_json.as_str(), "content", &secret_key, false, &[extra_field], &[]);
        crate::ops::prepare(&mut req, &ccx).await.expect("the signed form prepares");
        req
    }

    #[test]
    fn deserialize_http_without_a_multipart_form_is_invalid_request() {
        let mut req = crate::http::Request::from(
            hyper::Request::builder()
                .method(hyper::Method::POST)
                .uri("http://localhost/bucket")
                .body(crate::http::Body::empty())
                .expect("a valid request"),
        );

        let err = deserialize_http(&mut req).expect_err("a request without a multipart form");
        assert_eq!(*err.code(), S3ErrorCode::InvalidRequest);
        assert_eq!(err.message(), Some("missing multipart form"));
    }

    #[test]
    fn status_201_rejects_a_control_character_in_the_response_xml() {
        let err = serialize_http("bucket", "key", None, Some(201), &output_with_etag("\u{1}"))
            .err()
            .expect("a control character must not reach the XML body");
        assert_eq!(*err.code(), S3ErrorCode::InternalError);
    }

    #[tokio::test]
    async fn deserialize_http_falls_back_to_the_legacy_redirect_field() {
        let redirect = "https://example.com/callback";
        let mut req = prepared_post_request(("redirect", redirect)).await;

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.success_action_redirect.as_deref(), Some(redirect));
        assert_eq!(input.success_action_status, None);
    }

    #[tokio::test]
    async fn deserialize_http_rejects_a_malformed_success_action_status() {
        let mut req = prepared_post_request(("success_action_status", "abc")).await;

        let err = deserialize_http(&mut req).expect_err("a non-numeric status is rejected");
        assert_eq!(*err.code(), S3ErrorCode::InvalidArgument);
        assert_eq!(err.message(), Some(r#"invalid field value: success_action_status: "abc""#));
    }

    #[tokio::test]
    async fn redirect_fields_are_validated_while_serializing_not_while_parsing() {
        // parse_field_value is infallible for String: every text value parses, so the ?
        // on the two redirect lookups has no reachable error path. The value is validated
        // when the response is serialized instead.
        let mut req = prepared_post_request(("success_action_redirect", "not a url")).await;
        let input = deserialize_http(&mut req).expect("a text value parses");
        assert_eq!(input.success_action_redirect.as_deref(), Some("not a url"));

        let mut req = prepared_post_request(("redirect", "also not a url")).await;
        let input = deserialize_http(&mut req).expect("a text value parses through the fallback");
        assert_eq!(input.success_action_redirect.as_deref(), Some("also not a url"));

        let err = serialize_http("bucket", "key", Some("not a url"), None, &PostObjectOutput::default())
            .err()
            .expect("the redirect is rejected when the response is serialized");
        assert_eq!(*err.code(), S3ErrorCode::InvalidArgument);
    }

    /// A successful POST names the object it stored by the `ETag` it answered with, on every
    /// status it can answer with.
    #[test]
    fn post_object_response_names_the_stored_object() {
        use hyper::header::ETAG;
        use hyper::header::LOCATION;

        let output = output_with_etag("abc123");

        for status in [None, Some(200), Some(201)] {
            let res = serialize_http("bucket", "key", None, status, &output).expect("a POST response");
            assert_eq!(
                res.headers.get(ETAG).map(|value| value.to_str().expect("an ETag header")),
                Some("\"abc123\""),
                "the response carries the ETag, quoted as a header",
            );
            assert!(
                res.headers.get(LOCATION).is_none(),
                "no location is written when the caller supplied none",
            );
        }

        // A redirect answers with the location the client asked for, and nothing else: the client
        // is not fetching the object from this response.
        let res = serialize_http("bucket", "key", Some("https://example.com/done"), None, &output).expect("a redirect response");
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        assert!(
            res.headers
                .get(LOCATION)
                .expect("a location header")
                .to_str()
                .expect("a UTF-8 location")
                .starts_with("https://example.com/done?"),
            "the redirect keeps the location the client asked for"
        );
    }

    /// An object tag that cannot become a header value is refused with an internal error rather
    /// than answering without a tag or panicking.
    #[test]
    fn an_unprintable_object_tag_is_refused() {
        let output = output_with_etag("\"bad\u{1}\"");
        let err = serialize_http("bucket", "key", None, None, &output)
            .err()
            .expect("the tag cannot become a header value");
        assert_eq!(*err.code(), S3ErrorCode::InternalError);
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The POST-specific parsing and serialization of the `PostObject` operation.
//!
//! The form fields, the policy and the `success_action_*` response semantics
//! live here; the generated operation module delegates to these functions.
//!
//! Most form fields reuse the name of the `PutObject` header they stand for, and
//! the generated `PutObject` form reader takes them by that name. Two do not:
//! Amazon S3 names the canned ACL field `acl` (the header is `x-amz-acl`) and the
//! tag set field `tagging` (the header is `x-amz-tagging`), and the `tagging`
//! field carries an XML `Tagging` document where the header carries a URL-encoded
//! query string. Both are read here, from the same form the POST policy was
//! checked against, so the value applied is the value the policy saw: a repeated
//! field resolves to its last occurrence in both places. The header-named fields
//! are still read when the AWS-named field is absent.

use crate::dto::ETag;
use crate::dto::ObjectCannedACL;
use crate::dto::PostObjectInput;
use crate::dto::PostObjectOutput;
use crate::dto::Tagging;
use crate::dto::TaggingHeader;
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

    // The AWS names of the canned ACL and tag set fields; see the module docs.
    let acl: Option<ObjectCannedACL> = http::parse_field_value(&m, "acl")?;
    let tagging: Option<TaggingHeader> = m.find_field_value("tagging").map(tagging_header).transpose()?;

    // Get the validated POST policy from request extensions
    let policy = req.s3ext.post_policy.take();

    let put_input = PutObject::deserialize_http_multipart(req, m)?;
    let mut post_input = put_object_input_into_post_object_input(put_input);
    if acl.is_some() {
        post_input.acl = acl;
    }
    if tagging.is_some() {
        post_input.tagging = tagging;
    }
    post_input.success_action_redirect = success_action_redirect;
    post_input.success_action_status = success_action_status;
    post_input.policy = policy;
    Ok(post_input)
}

/// Converts the `tagging` form field, an XML `Tagging` document, into the
/// URL-encoded query string (`k1=v1&k2=v2`) that `PutObject`'s `x-amz-tagging`
/// header carries.
///
/// Keys and values are percent-encoded except for the RFC 3986 unreserved
/// characters, so a space becomes `%20` and a literal `&`, `=` or `+` is escaped:
/// a plain percent-decoder and a form decoder read back the same tags.
///
/// # Errors
/// Returns `MalformedXML` if the document is not a `Tagging` document, or if a
/// tag lacks its `Key` or its `Value`.
fn tagging_header(xml: &str) -> S3Result<TaggingHeader> {
    let tagging: Tagging = http::deserialize_xml(xml.as_bytes())?;
    let mut header = String::new();
    for tag in &tagging.tag_set {
        let (Some(key), Some(value)) = (&tag.key, &tag.value) else {
            return Err(s3_error!(MalformedXML, "a tag in the tagging form field lacks its Key or its Value"));
        };
        if !header.is_empty() {
            header.push('&');
        }
        header.push_str(&urlencoding::encode(key));
        header.push('=');
        header.push_str(&urlencoding::encode(value));
    }
    Ok(header)
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

    // Handle success_action_status
    match success_action_status {
        Some(200) => {
            // 200 OK with empty body
            Ok(http::Response::with_status(http::StatusCode::OK))
        }
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
            Ok(res)
        }
        _ => {
            // 204 No Content (default, also for unrecognized values)
            Ok(http::Response::with_status(http::StatusCode::NO_CONTENT))
        }
    }
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

    /// The tag set `{a: b}` as the AWS POST form carries it.
    const TAGGING_XML: &str = "<Tagging><TagSet><Tag><Key>a</Key><Value>b</Value></Tag></TagSet></Tagging>";

    /// Builds a prepared POST request whose form carries `fields` in this order.
    ///
    /// Each field name is covered by an empty `starts-with` condition, so prepare accepts any
    /// value; the wire suite (`tests/post_object_wire.rs`) pins the value checks themselves.
    async fn prepared_post_form(fields: &[(&str, &str)]) -> crate::http::Request {
        use crate::auth::SecretKey;
        use crate::ops::tests::common::post_policy_test_helpers as post;
        use std::sync::Arc;

        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post::TestS3NoOp);
        let config = post::create_test_config(1024 * 1024);
        let auth = post::create_test_auth();
        let ccx = post::create_test_context(&s3, &config, &auth);
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

        let mut names: Vec<&str> = fields.iter().map(|&(name, _)| name).collect();
        names.sort_unstable();
        names.dedup();
        let mut conditions = vec![post::BASE_CONDITIONS.to_owned()];
        conditions.extend(names.iter().map(|name| format!(r#"["starts-with","${name}",""]"#)));
        let policy_json = format!(r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#, conditions.join(","));
        let mut req = post::build_post_object_request_with(policy_json.as_str(), "content", &secret_key, false, fields, &[]);
        crate::ops::prepare(&mut req, &ccx).await.expect("the signed form prepares");
        req
    }

    #[tokio::test]
    async fn deserialize_http_reads_the_aws_acl_and_tagging_fields() {
        let mut req = prepared_post_form(&[("acl", "public-read"), ("tagging", TAGGING_XML)]).await;

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.acl.as_ref().map(ObjectCannedACL::as_str), Some(ObjectCannedACL::PUBLIC_READ));
        assert_eq!(input.tagging.as_deref(), Some("a=b"));
    }

    #[tokio::test]
    async fn deserialize_http_prefers_the_aws_fields_over_the_header_named_fields() {
        let mut req = prepared_post_form(&[
            ("x-amz-acl", "public-read"),
            ("acl", "private"),
            ("x-amz-tagging", "c=d"),
            ("tagging", TAGGING_XML),
        ])
        .await;

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.acl.as_ref().map(ObjectCannedACL::as_str), Some(ObjectCannedACL::PRIVATE));
        assert_eq!(input.tagging.as_deref(), Some("a=b"));
    }

    #[tokio::test]
    async fn deserialize_http_still_reads_the_header_named_fields_alone() {
        let mut req = prepared_post_form(&[("x-amz-acl", "public-read"), ("x-amz-tagging", "c=d")]).await;

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.acl.as_ref().map(ObjectCannedACL::as_str), Some(ObjectCannedACL::PUBLIC_READ));
        assert_eq!(input.tagging.as_deref(), Some("c=d"));
    }

    #[tokio::test]
    async fn deserialize_http_without_acl_or_tagging_leaves_both_unset() {
        let mut req = prepared_post_form(&[]).await;

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.acl, None);
        assert_eq!(input.tagging, None);
    }

    #[tokio::test]
    async fn a_repeated_field_resolves_to_its_last_occurrence_like_the_policy_check() {
        let other = "<Tagging><TagSet><Tag><Key>c</Key><Value>d</Value></Tag></TagSet></Tagging>";
        let mut req = prepared_post_form(&[
            ("acl", "public-read"),
            ("tagging", other),
            ("acl", "private"),
            ("tagging", TAGGING_XML),
        ])
        .await;

        // The policy conditions are evaluated against `find_field_value`, which returns the
        // last occurrence of a repeated field; the input has to carry that same occurrence.
        let form = req.s3ext.multipart.as_ref().expect("the form is kept for the operation");
        assert_eq!(form.find_field_value("acl"), Some("private"));
        assert_eq!(form.find_field_value("tagging"), Some(TAGGING_XML));

        let input = deserialize_http(&mut req).expect("the form parses");
        assert_eq!(input.acl.as_ref().map(ObjectCannedACL::as_str), Some(ObjectCannedACL::PRIVATE));
        assert_eq!(input.tagging.as_deref(), Some("a=b"));
    }

    #[tokio::test]
    async fn deserialize_http_rejects_a_tagging_field_that_is_not_a_tagging_document() {
        let mut req = prepared_post_form(&[("tagging", "a=b")]).await;

        let err = deserialize_http(&mut req).expect_err("a query string is not a Tagging document");
        assert_eq!(*err.code(), S3ErrorCode::MalformedXML);
    }

    #[test]
    fn tagging_header_percent_encodes_each_tag_as_a_query_pair() {
        let xml = concat!(
            "<Tagging><TagSet>",
            "<Tag><Key>project name</Key><Value>a&amp;b=c+d</Value></Tag>",
            "<Tag><Key>caf\u{e9}/x</Key><Value>~ok-_.</Value></Tag>",
            "</TagSet></Tagging>",
        );
        let header = tagging_header(xml).expect("a valid Tagging document");
        assert_eq!(header, "project%20name=a%26b%3Dc%2Bd&caf%C3%A9%2Fx=~ok-_.");

        // The header decodes back to the tags the document named.
        let decoded: Vec<(String, String)> = url::form_urlencoded::parse(header.as_bytes()).into_owned().collect();
        assert_eq!(
            decoded,
            vec![
                ("project name".to_owned(), "a&b=c+d".to_owned()),
                ("caf\u{e9}/x".to_owned(), "~ok-_.".to_owned()),
            ]
        );
    }

    #[test]
    fn tagging_header_accepts_a_namespace_an_empty_value_and_an_empty_tag_set() {
        let xml = r#"<Tagging xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><TagSet><Tag><Key>k</Key><Value></Value></Tag></TagSet></Tagging>"#;
        assert_eq!(tagging_header(xml).expect("an empty value is a value"), "k=");

        let xml = "<Tagging><TagSet></TagSet></Tagging>";
        assert_eq!(tagging_header(xml).expect("an empty tag set"), "");
    }

    #[test]
    fn tagging_header_rejects_what_is_not_a_complete_tagging_document() {
        let samples = [
            ("a query string", "a=b"),
            ("an empty value", ""),
            ("another root element", "<TagSet><Tag><Key>a</Key><Value>b</Value></Tag></TagSet>"),
            ("no TagSet", "<Tagging></Tagging>"),
            ("a tag without a Value", "<Tagging><TagSet><Tag><Key>a</Key></Tag></TagSet></Tagging>"),
            ("a tag without a Key", "<Tagging><TagSet><Tag><Value>b</Value></Tag></TagSet></Tagging>"),
            ("trailing content", "<Tagging><TagSet></TagSet></Tagging><Tagging/>"),
        ];
        for (what, xml) in samples {
            let err = tagging_header(xml).expect_err(what);
            assert_eq!(*err.code(), S3ErrorCode::MalformedXML, "{what}");
        }
    }
}

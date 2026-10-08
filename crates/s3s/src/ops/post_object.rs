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
}

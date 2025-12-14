use crate::S3Result;
use crate::dto::{ETag, PostResponse};
use crate::http::Response;

use hyper::StatusCode;
use hyper::header::CONTENT_TYPE;

/// Handle `success_action_redirect` for POST object
pub fn handle_success_action_redirect(mut resp: Response, bucket: &str, key: &str, redirect_url: &str) -> S3Result<Response> {
    // Extract ETag from response headers
    let etag_header = resp
        .headers
        .get(hyper::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Build redirect URL with query parameters
    let redirect_with_params = if redirect_url.contains('?') {
        format!(
            "{}&bucket={}&key={}&etag={}",
            redirect_url,
            urlencoding::encode(bucket),
            urlencoding::encode(key),
            urlencoding::encode(etag_header)
        )
    } else {
        format!(
            "{}?bucket={}&key={}&etag={}",
            redirect_url,
            urlencoding::encode(bucket),
            urlencoding::encode(key),
            urlencoding::encode(etag_header)
        )
    };

    resp.status = StatusCode::SEE_OTHER; // 303
    resp.headers.insert(
        hyper::header::LOCATION,
        redirect_with_params
            .parse()
            .map_err(|e| invalid_request!(e, "invalid redirect URL"))?,
    );
    resp.body = crate::http::Body::empty();

    Ok(resp)
}

/// Handle `success_action_status` for POST object
pub fn handle_success_action_status(mut resp: Response, bucket: &str, key: &str, status: u16) -> S3Result<Response> {
    let status_code = match status {
        200 => StatusCode::OK,
        201 => StatusCode::CREATED,
        _ => StatusCode::NO_CONTENT, // 204 or any other value (should not happen due to validation)
    };
    resp.status = status_code;

    // For 200 and 201, return XML body with POST response information
    if status == 200 || status == 201 {
        let etag_header = resp
            .headers
            .get(hyper::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        // Build location URL
        let location = format!("/{bucket}/{key}");

        // Create PostResponse structure
        // Parse ETag from header format (already includes quotes)
        let parsed_etag =
            ETag::parse_http_header(etag_header.as_bytes()).unwrap_or_else(|_| ETag::Strong(etag_header.to_owned()));

        let post_response = PostResponse {
            location: location.clone(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            e_tag: parsed_etag,
        };

        // Serialize to XML
        crate::http::set_xml_body(&mut resp, &post_response)?;
        resp.headers.insert(CONTENT_TYPE, "application/xml".parse().unwrap());
    }

    Ok(resp)
}

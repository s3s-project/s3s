use crate::S3Request;
use crate::S3Result;
use crate::dto::GetObjectInput;
use crate::dto::Timestamp;
use crate::dto::TimestampFormat;
use crate::header;
use crate::http::Response;
use crate::utils::format::fmt_timestamp;

use hyper::HeaderMap;
use hyper::header::CONTENT_LENGTH;
use hyper::header::TRANSFER_ENCODING;
use hyper::http::HeaderName;
use hyper::http::HeaderValue;

use stdx::default::default;

pub fn extract_overridden_response_headers(req: &S3Request<GetObjectInput>) -> S3Result<HeaderMap> {
    let mut map: HeaderMap = default();

    add(&mut map, header::CONTENT_TYPE, req.input.response_content_type.as_deref())?;
    add(&mut map, header::CONTENT_LANGUAGE, req.input.response_content_language.as_deref())?;
    add_ts(&mut map, header::EXPIRES, req.input.response_expires.as_ref())?;
    add(&mut map, header::CACHE_CONTROL, req.input.response_cache_control.as_deref())?;
    add(&mut map, header::CONTENT_DISPOSITION, req.input.response_content_disposition.as_deref())?;
    add(&mut map, header::CONTENT_ENCODING, req.input.response_content_encoding.as_deref())?;

    Ok(map)
}

fn add(map: &mut HeaderMap, name: HeaderName, value: Option<&str>) -> S3Result<()> {
    let error = |e| invalid_request!(e, "invalid overridden header: {name}: {value:?}");
    if let Some(value) = value {
        let value = value.parse().map_err(error)?;
        map.insert(name, value);
    }
    Ok(())
}

fn add_ts(map: &mut HeaderMap, name: HeaderName, value: Option<&Timestamp>) -> S3Result<()> {
    let error = |e| invalid_request!(e, "invalid overridden header: {name}: {value:?}");
    if let Some(value) = value {
        let value = fmt_timestamp(value, TimestampFormat::HttpDate, HeaderValue::from_bytes).map_err(error)?;
        map.insert(name, value);
    }
    Ok(())
}

pub fn merge_custom_headers(resp: &mut Response, headers: HeaderMap) {
    resp.headers.extend(headers);

    // special case for https://github.com/Nugine/s3s/issues/80
    if let Some(val) = resp.headers.get(TRANSFER_ENCODING)
        && val.as_bytes() == b"chunked"
    {
        resp.headers.remove(CONTENT_LENGTH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dto::GetObjectInput;
    use hyper::Method;
    use hyper::StatusCode;
    use hyper::Uri;
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;

    fn make_request(input: GetObjectInput) -> S3Request<GetObjectInput> {
        S3Request {
            input,
            method: Method::GET,
            uri: Uri::from_static("http://example.com/test-bucket/test-key"),
            headers: HeaderMap::new(),
            extensions: Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    #[test]
    fn extract_overridden_response_headers_collects_all_supported_values() {
        let input = GetObjectInput {
            response_content_type: Some("text/plain".to_owned()),
            response_content_language: Some("en-US".to_owned()),
            response_expires: Some(Timestamp::parse(TimestampFormat::HttpDate, "Wed, 21 Oct 2015 07:28:00 GMT").unwrap()),
            response_cache_control: Some("max-age=60".to_owned()),
            response_content_disposition: Some("inline".to_owned()),
            response_content_encoding: Some("gzip".to_owned()),
            ..Default::default()
        };

        let headers = extract_overridden_response_headers(&make_request(input)).unwrap();

        assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), &HeaderValue::from_static("text/plain"));
        assert_eq!(headers.get(header::CONTENT_LANGUAGE).unwrap(), &HeaderValue::from_static("en-US"));
        assert_eq!(
            headers.get(header::EXPIRES).unwrap(),
            &HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), &HeaderValue::from_static("max-age=60"));
        assert_eq!(headers.get(header::CONTENT_DISPOSITION).unwrap(), &HeaderValue::from_static("inline"));
        assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), &HeaderValue::from_static("gzip"));
    }

    #[test]
    fn extract_overridden_response_headers_rejects_invalid_header_value() {
        let input = GetObjectInput {
            response_content_type: Some("text/plain\r\nX-Bad: 1".to_owned()),
            ..Default::default()
        };

        let err = extract_overridden_response_headers(&make_request(input)).unwrap_err();
        assert_eq!(*err.code(), crate::S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn extract_overridden_response_headers_returns_empty_map_without_overrides() {
        let headers = extract_overridden_response_headers(&make_request(GetObjectInput::default())).unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn merge_custom_headers_removes_content_length_for_chunked_transfer_encoding() {
        let mut resp = Response::with_status(StatusCode::OK);
        resp.headers.insert(CONTENT_LENGTH, HeaderValue::from_static("10"));

        let mut headers = HeaderMap::new();
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

        merge_custom_headers(&mut resp, headers);

        assert!(resp.headers.get(CONTENT_LENGTH).is_none());
        assert_eq!(resp.headers.get(TRANSFER_ENCODING).unwrap(), &HeaderValue::from_static("chunked"));
        assert_eq!(resp.headers.get(header::CONTENT_TYPE).unwrap(), &HeaderValue::from_static("text/plain"));
    }

    #[test]
    fn merge_custom_headers_keeps_content_length_for_non_chunked_transfer_encoding() {
        let mut resp = Response::with_status(StatusCode::OK);
        resp.headers.insert(CONTENT_LENGTH, HeaderValue::from_static("10"));

        let mut headers = HeaderMap::new();
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("gzip"));

        merge_custom_headers(&mut resp, headers);

        assert_eq!(resp.headers.get(CONTENT_LENGTH).unwrap(), &HeaderValue::from_static("10"));
        assert_eq!(resp.headers.get(TRANSFER_ENCODING).unwrap(), &HeaderValue::from_static("gzip"));
    }
}

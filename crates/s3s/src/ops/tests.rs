use super::*;

// use crate::service::S3Service;

// use stdx::mem::output_size;

// #[test]
// #[ignore]
// fn track_future_size() {
//     macro_rules! future_size {
//         ($f:path, $v:expr) => {
//             (stringify!($f), output_size(&$f), $v)
//         };
//     }

//     #[rustfmt::skip]
//     let sizes = [
//         future_size!(S3Service::call,                           2704),
//         future_size!(call,                                      1512),
//         future_size!(prepare,                                   1440),
//         future_size!(SignatureContext::check,                   776),
//         future_size!(SignatureContext::v2_check,                296),
//         future_size!(SignatureContext::v2_check_presigned_url,  168),
//         future_size!(SignatureContext::v2_check_header_auth,    184),
//         future_size!(SignatureContext::v4_check,                752),
//         future_size!(SignatureContext::v4_check_post_signature, 368),
//         future_size!(SignatureContext::v4_check_presigned_url,  456),
//         future_size!(SignatureContext::v4_check_header_auth,    640),
//     ];

//     println!("{sizes:#?}");
//     for (name, size, expected) in sizes {
//         assert_eq!(size, expected, "{name:?} size changed: prev {expected}, now {size}");
//     }
// }

#[test]
fn error_custom_headers() {
    fn redirect307(location: &str) -> S3Error {
        let mut err = S3Error::new(S3ErrorCode::TemporaryRedirect);

        err.set_headers({
            let mut headers = HeaderMap::new();
            headers.insert(crate::header::LOCATION, location.parse().unwrap());
            headers
        });

        err
    }

    let res = serialize_error(redirect307("http://example.com"), false).unwrap();
    assert_eq!(res.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(res.headers.get("location").unwrap(), "http://example.com");

    let body = res.body.bytes().unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert_eq!(
        body,
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<Error><Code>TemporaryRedirect</Code></Error>"
        )
    );
}

#[test]
fn extract_host_from_uri() {
    use crate::http::Request;
    use crate::ops::extract_host;

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("https://test.example.com:9001/%E6%95%B0%E6%8D%AE%E7%BB%93%E6%9E%84%E5%92%8C%E7%AE%97%E6%B3%95_rust.pdf?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20251213T084305Z&X-Amz-SignedHeaders=host&X-Amz-Credential=rustfsadmin%2F20251213%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Expires=3600&X-Amz-Signature=57133ee54dab71c00a10106c33cde2615b301bd2cf00e2439f3ddb4bc999ec66")
            .body(Body::empty())
            .unwrap(),
    );

    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("test.example.com:9001".to_string()));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://another.example.org/resource")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("another.example.org".to_string()));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("/no/host/header")
            .header("Host", "header.example.com:8080")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("header.example.com:8080".to_string()));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("/no/host/header")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, None);
}

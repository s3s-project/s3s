// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#[cfg(test)]
mod tests {
    use s3s::S3;
    use s3s::service::S3ServiceBuilder;
    use wasm_bindgen_test::wasm_bindgen_test;

    struct DummyS3 {}

    impl S3 for DummyS3 {}

    #[wasm_bindgen_test]
    fn test_dummy_call() {
        let s3 = DummyS3 {};

        let service = S3ServiceBuilder::new(s3).build();

        let req = {
            let mut req = s3s::HttpRequest::default();
            *req.method_mut() = http::Method::GET;
            *req.uri_mut() = "http://localhost/".parse().unwrap();
            req
        };

        let result = futures::executor::block_on(service.call(req));
        assert!(result.is_ok());

        let resp = result.unwrap();
        assert_eq!(resp.status(), 501); // Not Implemented
    }
}

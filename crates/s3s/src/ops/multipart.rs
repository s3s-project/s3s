use super::CompleteMultipartUpload;

use crate::dto::CompleteMultipartUploadOutput;
use crate::error::S3Result;
use crate::header::*;
use crate::http;

use sync_wrapper::SyncFuture;

fn add_complete_multipart_headers(res: &mut http::Response, x: &CompleteMultipartUploadOutput) -> S3Result<()> {
    http::add_opt_header(res, X_AMZ_SERVER_SIDE_ENCRYPTION_BUCKET_KEY_ENABLED, x.bucket_key_enabled)?;
    http::add_opt_header(res, X_AMZ_EXPIRATION, x.expiration.clone())?;
    http::add_opt_header(res, X_AMZ_REQUEST_CHARGED, x.request_charged.clone())?;
    http::add_opt_header(res, X_AMZ_SERVER_SIDE_ENCRYPTION_AWS_KMS_KEY_ID, x.ssekms_key_id.clone())?;
    http::add_opt_header(res, X_AMZ_SERVER_SIDE_ENCRYPTION, x.server_side_encryption.clone())?;
    http::add_opt_header(res, X_AMZ_VERSION_ID, x.version_id.clone())?;
    Ok(())
}

impl CompleteMultipartUpload {
    pub fn serialize_http(mut x: CompleteMultipartUploadOutput) -> S3Result<http::Response> {
        let mut res = http::Response::with_status(http::StatusCode::OK);

        if let Some(future) = x.future.take() {
            let future = SyncFuture::new(async move {
                let result = future.await;
                match result {
                    Ok(val) => {
                        let mut res = http::Response::default();
                        http::set_xml_body_no_decl(&mut res, &val)?;
                        add_complete_multipart_headers(&mut res, &val)?;
                        Ok(res)
                    }
                    Err(err) => super::serialize_error(err, false).map_err(Into::into),
                }
            });
            let duration = std::time::Duration::from_millis(100);
            http::set_keep_alive_xml_body(&mut res, future, duration)?;
        } else {
            http::set_xml_body(&mut res, &x)?;
        }

        add_complete_multipart_headers(&mut res, &x)?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dto::{RequestCharged, ServerSideEncryption};
    use http_body_util::BodyExt as _;
    use hyper::header::CONTENT_TYPE;
    use hyper::header::HeaderValue;

    fn sample_output() -> CompleteMultipartUploadOutput {
        CompleteMultipartUploadOutput {
            bucket: Some("bucket".to_owned()),
            bucket_key_enabled: Some(true),
            expiration: Some("expiry-date=\"Wed, 21 Oct 2015 07:28:00 GMT\", rule-id=\"rule\"".to_owned()),
            key: Some("key".to_owned()),
            location: Some("http://example.com/bucket/key".to_owned()),
            request_charged: Some(RequestCharged::from_static(RequestCharged::REQUESTER)),
            ssekms_key_id: Some("kms-key".to_owned()),
            server_side_encryption: Some(ServerSideEncryption::from_static(ServerSideEncryption::AES256)),
            version_id: Some("version-id".to_owned()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn serialize_http_without_future_sets_xml_body_and_optional_headers() {
        let resp = CompleteMultipartUpload::serialize_http(sample_output()).unwrap();

        assert_eq!(resp.status, http::StatusCode::OK);
        assert_eq!(resp.headers.get(CONTENT_TYPE).unwrap(), &HeaderValue::from_static("application/xml"));
        assert!(resp.headers.get(hyper::header::TRANSFER_ENCODING).is_none());
        assert_eq!(resp.headers.get(X_AMZ_SERVER_SIDE_ENCRYPTION_BUCKET_KEY_ENABLED).unwrap(), "true");
        assert_eq!(
            resp.headers.get(X_AMZ_EXPIRATION).unwrap(),
            "expiry-date=\"Wed, 21 Oct 2015 07:28:00 GMT\", rule-id=\"rule\""
        );
        assert_eq!(resp.headers.get(X_AMZ_REQUEST_CHARGED).unwrap(), "requester");
        assert_eq!(resp.headers.get(X_AMZ_SERVER_SIDE_ENCRYPTION_AWS_KMS_KEY_ID).unwrap(), "kms-key");
        assert_eq!(resp.headers.get(X_AMZ_SERVER_SIDE_ENCRYPTION).unwrap(), "AES256");
        assert_eq!(resp.headers.get(X_AMZ_VERSION_ID).unwrap(), "version-id");

        let body = String::from_utf8(resp.body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<CompleteMultipartUploadResult"));
        assert!(body.contains("<Bucket>bucket</Bucket>"));
        assert!(body.contains("<Key>key</Key>"));
    }

    #[tokio::test]
    async fn serialize_http_with_future_streams_success_response() {
        let resp = CompleteMultipartUpload::serialize_http(CompleteMultipartUploadOutput {
            future: Some(Box::pin(async move { Ok(sample_output()) })),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(resp.status, http::StatusCode::OK);
        assert_eq!(resp.headers.get(CONTENT_TYPE).unwrap(), &HeaderValue::from_static("application/xml"));
        assert_eq!(resp.headers.get(hyper::header::TRANSFER_ENCODING).unwrap(), "chunked");
        assert!(resp.headers.get(X_AMZ_VERSION_ID).is_none());

        let aggregated = resp.body.collect().await.unwrap();
        let trailers = aggregated
            .trailers()
            .expect("future success path should expose final headers as trailers");
        assert_eq!(trailers.get(X_AMZ_REQUEST_CHARGED).unwrap(), "requester");
        assert_eq!(trailers.get(X_AMZ_SERVER_SIDE_ENCRYPTION).unwrap(), "AES256");
        assert_eq!(trailers.get(X_AMZ_VERSION_ID).unwrap(), "version-id");
        let body = String::from_utf8(aggregated.to_bytes().to_vec()).unwrap();
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<CompleteMultipartUploadResult"));
        assert!(body.contains("<Location>http://example.com/bucket/key</Location>"));
    }

    #[tokio::test]
    async fn serialize_http_with_future_streams_serialized_s3_error() {
        let resp = CompleteMultipartUpload::serialize_http(CompleteMultipartUploadOutput {
            future: Some(Box::pin(async move { Err(crate::s3_error!(NoSuchBucket, "missing bucket")) })),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(resp.status, http::StatusCode::OK);
        assert_eq!(resp.headers.get(hyper::header::TRANSFER_ENCODING).unwrap(), "chunked");

        let aggregated = resp.body.collect().await.unwrap();
        let body = String::from_utf8(aggregated.to_bytes().to_vec()).unwrap();
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<Error>"));
        assert!(body.contains("<Code>NoSuchBucket</Code>"));
        assert!(body.contains("<Message>missing bucket</Message>"));
    }
}

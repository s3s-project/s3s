use crate::case;
use crate::utils::*;

use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use s3s_test::tcx::TestContext;

use std::sync::Arc;

use aws_credential_types::provider::ProvideCredentials;
use tracing::debug;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, PostPolicy, Basic, test_post_object_basic);
    case!(tcx, PostPolicy, Basic, test_post_object_with_conditions);
    case!(tcx, PostPolicy, Basic, test_post_object_content_length_range);
    case!(tcx, PostPolicy, Basic, test_post_object_starts_with);
    case!(tcx, PostPolicy, Basic, test_post_object_expired_policy);
    case!(tcx, PostPolicy, Basic, test_post_object_invalid_bucket);
}

struct PostPolicy {
    s3: aws_sdk_s3::Client,
    endpoint: String,
    credentials: aws_credential_types::Credentials,
}

impl TestSuite for PostPolicy {
    async fn setup() -> Result<Self> {
        let sdk_conf = aws_config::from_env().load().await;
        let s3 = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&sdk_conf)
                .force_path_style(true)
                .build(),
        );

        // Extract credentials from SDK config
        let credentials = sdk_conf
            .credentials_provider()
            .expect("credentials provider not found")
            .provide_credentials()
            .await
            .expect("failed to get credentials");

        // Get endpoint from environment or use default
        let endpoint = std::env::var("AWS_ENDPOINT_URL")
            .or_else(|_| std::env::var("S3_ENDPOINT"))
            .unwrap_or_else(|_| "http://localhost:8014".to_string());

        Ok(Self {
            s3,
            endpoint,
            credentials,
        })
    }
}

struct Basic {
    s3: aws_sdk_s3::Client,
    endpoint: String,
    credentials: aws_credential_types::Credentials,
    bucket: String,
}

impl TestFixture<PostPolicy> for Basic {
    async fn setup(suite: Arc<PostPolicy>) -> Result<Self> {
        let bucket = "test-post-policy";

        delete_bucket_loose(&suite.s3, bucket).await?;
        create_bucket(&suite.s3, bucket).await?;

        Ok(Self {
            s3: suite.s3.clone(),
            endpoint: suite.endpoint.clone(),
            credentials: suite.credentials.clone(),
            bucket: bucket.to_owned(),
        })
    }

    async fn teardown(self) -> Result {
        delete_bucket_loose(&self.s3, &self.bucket).await?;
        Ok(())
    }
}

impl Basic {
    /// Test basic POST object upload with a simple policy
    async fn test_post_object_basic(self: Arc<Self>) -> Result {
        let key = "test-basic-post.txt";
        let content = b"Hello from POST!";

        // Clean up any existing object
        delete_object_loose(&self.s3, &self.bucket, key).await?;

        // Create policy document
        let expiration = format_expiration(3600); // 1 hour from now
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": self.bucket},
                {"key": key},
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        debug!(?policy_str, ?policy_b64, "created policy");

        // Create signature using AWS Signature V4
        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        // Create multipart form data
        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        // Send POST request
        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        debug!(?url, "sending POST request");

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;
        
        debug!(?status, ?body_text, "received response");

        assert!(
            status.is_success(),
            "POST object failed: status={}, body={}",
            status,
            body_text
        );

        // Verify the object was created
        let resp = self.s3.get_object().bucket(&self.bucket).key(key).send().await?;
        let object_body = resp.body.collect().await?;
        let object_content = object_body.to_vec();

        assert_eq!(object_content, content);

        // Clean up
        delete_object_strict(&self.s3, &self.bucket, key).await?;

        Ok(())
    }

    /// Test POST object with various policy conditions
    async fn test_post_object_with_conditions(self: Arc<Self>) -> Result {
        let key = "test-conditions-post.txt";
        let content = b"Content with metadata";

        delete_object_loose(&self.s3, &self.bucket, key).await?;

        let expiration = format_expiration(3600);
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": self.bucket},
                {"key": key},
                {"acl": "private"},
                {"Content-Type": "text/plain"},
                ["eq", "$x-amz-meta-test", "value"],
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
                ("acl", "private"),
                ("Content-Type", "text/plain"),
                ("x-amz-meta-test", "value"),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;

        assert!(
            status.is_success(),
            "POST object with conditions failed: status={}, body={}",
            status,
            body_text
        );

        // Verify object
        let resp = self.s3.get_object().bucket(&self.bucket).key(key).send().await?;
        let object_body = resp.body.collect().await?;
        assert_eq!(object_body.to_vec(), content);

        delete_object_strict(&self.s3, &self.bucket, key).await?;
        Ok(())
    }

    /// Test content-length-range condition
    async fn test_post_object_content_length_range(self: Arc<Self>) -> Result {
        let key = "test-size-limit.txt";
        let content = b"Small content";

        delete_object_loose(&self.s3, &self.bucket, key).await?;

        let expiration = format_expiration(3600);
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": self.bucket},
                {"key": key},
                ["content-length-range", 1, 1024], // 1 byte to 1KB
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;

        assert!(
            status.is_success(),
            "POST with content-length-range failed: status={}, body={}",
            status,
            body_text
        );

        delete_object_strict(&self.s3, &self.bucket, key).await?;
        Ok(())
    }

    /// Test starts-with condition
    async fn test_post_object_starts_with(self: Arc<Self>) -> Result {
        let key = "uploads/test-prefix.txt";
        let content = b"File with prefix";

        delete_object_loose(&self.s3, &self.bucket, key).await?;

        let expiration = format_expiration(3600);
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": self.bucket},
                ["starts-with", "$key", "uploads/"],
                ["starts-with", "$Content-Type", "text/"],
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
                ("Content-Type", "text/plain"),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;

        assert!(
            status.is_success(),
            "POST with starts-with failed: status={}, body={}",
            status,
            body_text
        );

        delete_object_strict(&self.s3, &self.bucket, key).await?;
        Ok(())
    }

    /// Test expired policy rejection
    async fn test_post_object_expired_policy(self: Arc<Self>) -> Result {
        let key = "test-expired.txt";
        let content = b"Should not upload";

        delete_object_loose(&self.s3, &self.bucket, key).await?;

        // Create a policy that expired 1 hour ago
        let expiration = format_expiration(-3600);
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": self.bucket},
                {"key": key},
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;

        debug!(?status, ?body_text, "expired policy response");

        // Should fail with 403 or 400
        assert!(
            status.is_client_error(),
            "Expected error for expired policy, got status={}, body={}",
            status,
            body_text
        );

        // Verify object was NOT created
        let result = self.s3.get_object().bucket(&self.bucket).key(key).send().await;
        assert!(result.is_err(), "Object should not exist");

        Ok(())
    }

    /// Test invalid bucket name in policy
    async fn test_post_object_invalid_bucket(self: Arc<Self>) -> Result {
        let key = "test-wrong-bucket.txt";
        let content = b"Wrong bucket";

        // Create policy with wrong bucket name
        let expiration = format_expiration(3600);
        let policy = serde_json::json!({
            "expiration": expiration,
            "conditions": [
                {"bucket": "wrong-bucket-name"},
                {"key": key},
            ]
        });

        let policy_str = serde_json::to_string(&policy)?;
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_str.as_bytes());

        let date = time::OffsetDateTime::now_utc();
        let amz_date = format_amz_date(&date);
        let credential = format_credential(&self.credentials, &date);

        let signature = calculate_post_signature_v4(
            &policy_b64,
            &self.credentials.secret_access_key(),
            &date,
            "us-east-1",
            "s3",
        );

        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = create_multipart_body(
            boundary,
            &[
                ("key", key),
                ("policy", &policy_b64),
                ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
                ("x-amz-credential", &credential),
                ("x-amz-date", &amz_date),
                ("x-amz-signature", &signature),
            ],
            "file",
            "test.txt",
            "text/plain",
            content,
        );

        let client = reqwest::Client::new();
        let url = format!("{}/{}", self.endpoint, self.bucket);

        let response = client
            .post(&url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", boundary))
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await?;

        debug!(?status, ?body_text, "invalid bucket response");

        // Should fail
        assert!(
            status.is_client_error(),
            "Expected error for wrong bucket, got status={}, body={}",
            status,
            body_text
        );

        Ok(())
    }
}

// Helper functions

fn format_expiration(offset_seconds: i64) -> String {
    let now = time::OffsetDateTime::now_utc();
    let expiration = now + time::Duration::seconds(offset_seconds);
    expiration
        .format(&time::format_description::well_known::Rfc3339)
        .expect("failed to format expiration")
}

fn format_amz_date(date: &time::OffsetDateTime) -> String {
    date.format(&time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"))
        .expect("failed to format date")
}

fn format_credential(credentials: &aws_credential_types::Credentials, date: &time::OffsetDateTime) -> String {
    let date_str = date
        .format(&time::macros::format_description!("[year][month][day]"))
        .expect("failed to format date");
    format!(
        "{}/{}/us-east-1/s3/aws4_request",
        credentials.access_key_id(),
        date_str
    )
}

fn calculate_post_signature_v4(
    policy_b64: &str,
    secret_key: &str,
    date: &time::OffsetDateTime,
    region: &str,
    service: &str,
) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let date_str = date
        .format(&time::macros::format_description!("[year][month][day]"))
        .expect("failed to format date");

    // Create signing key
    let k_secret = format!("AWS4{}", secret_key);
    let mut mac = HmacSha256::new_from_slice(k_secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(date_str.as_bytes());
    let k_date = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_date).expect("HMAC can take key of any size");
    mac.update(region.as_bytes());
    let k_region = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_region).expect("HMAC can take key of any size");
    mac.update(service.as_bytes());
    let k_service = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_service).expect("HMAC can take key of any size");
    mac.update(b"aws4_request");
    let k_signing = mac.finalize().into_bytes();

    // Sign the policy
    let mut mac = HmacSha256::new_from_slice(&k_signing).expect("HMAC can take key of any size");
    mac.update(policy_b64.as_bytes());
    let signature = mac.finalize().into_bytes();

    hex::encode(signature)
}

fn create_multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field_name: &str,
    filename: &str,
    content_type: &str,
    file_content: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();

    // Add form fields
    for (name, value) in fields {
        body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes());
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }

    // Add file field
    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
            file_field_name, filename
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", content_type).as_bytes());
    body.extend_from_slice(file_content);
    body.extend_from_slice(b"\r\n");

    // End boundary
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    body
}

//! POST Policy validation for S3 POST object uploads
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-HTTPPOSTConstructPolicy.html>

use crate::error::S3Result;
use crate::http::Multipart;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// POST Policy document for browser-based uploads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostPolicy {
    /// Policy expiration time
    pub expiration: String,
    /// Policy conditions
    pub conditions: Vec<Condition>,
}

/// A condition in a POST policy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// Exact match: `{"field": "value"}`
    ExactMatch(HashMap<String, serde_json::Value>),
    /// Array form: `["eq", "$field", "value"]` or `["starts-with", "$field", "prefix"]` or `["content-length-range", min, max]`
    ArrayForm(Vec<serde_json::Value>),
}

impl PostPolicy {
    /// Parse policy from base64-encoded string
    ///
    /// # Errors
    /// Returns error if the policy is not valid base64 or valid JSON
    pub fn from_base64(policy_b64: &str) -> S3Result<Self> {
        let policy_bytes = base64_simd::STANDARD
            .decode_to_vec(policy_b64)
            .map_err(|_| s3_error!(InvalidPolicyDocument, "policy is not valid base64"))?;

        let policy_str =
            std::str::from_utf8(&policy_bytes).map_err(|_| s3_error!(InvalidPolicyDocument, "policy is not valid UTF-8"))?;

        let policy: PostPolicy =
            serde_json::from_str(policy_str).map_err(|e| s3_error!(e, InvalidPolicyDocument, "policy is not valid JSON"))?;

        Ok(policy)
    }

    /// Validate that the policy has not expired
    ///
    /// # Errors
    /// Returns error if the policy has expired
    pub fn validate_expiration(&self) -> S3Result<()> {
        // Parse expiration time (ISO 8601 / RFC 3339 format)
        let expiration = OffsetDateTime::parse(&self.expiration, &time::format_description::well_known::Rfc3339)
            .map_err(|_| s3_error!(InvalidPolicyDocument, "policy expiration is not valid RFC 3339 timestamp"))?;

        let now = OffsetDateTime::now_utc();
        if now > expiration {
            return Err(s3_error!(AccessDenied, "Invalid according to Policy: Policy expired"));
        }

        Ok(())
    }

    /// Validate policy conditions against the multipart form data
    ///
    /// # Errors
    /// Returns error if any condition is violated
    pub fn validate_conditions(&self, multipart: &Multipart, bucket: &str, file_size: u64) -> S3Result<()> {
        // Build a map of form fields for easier lookup
        let mut fields: HashMap<String, String> = HashMap::new();
        for (name, value) in multipart.fields() {
            fields.insert(name.clone(), value.clone());
        }

        // Always add bucket to fields for validation
        fields.insert("bucket".to_string(), bucket.to_string());

        // Validate each condition
        for condition in &self.conditions {
            match condition {
                Condition::ExactMatch(map) => {
                    for (field, value) in map {
                        Self::validate_exact_match(&fields, field, value)?;
                    }
                }
                Condition::ArrayForm(arr) => {
                    if arr.is_empty() {
                        return Err(s3_error!(InvalidPolicyDocument, "condition array is empty"));
                    }

                    // First element is the operator
                    let operator = arr[0]
                        .as_str()
                        .ok_or_else(|| s3_error!(InvalidPolicyDocument, "condition operator must be a string"))?;

                    match operator {
                        "eq" => {
                            if arr.len() != 3 {
                                return Err(s3_error!(InvalidPolicyDocument, "eq condition must have exactly 3 elements"));
                            }
                            let field = arr[1]
                                .as_str()
                                .ok_or_else(|| s3_error!(InvalidPolicyDocument, "field name must be a string"))?;
                            let value = &arr[2];
                            Self::validate_exact_match(&fields, &Self::normalize_field_name(field), value)?;
                        }
                        "starts-with" => {
                            if arr.len() != 3 {
                                return Err(s3_error!(
                                    InvalidPolicyDocument,
                                    "starts-with condition must have exactly 3 elements"
                                ));
                            }
                            let field = arr[1]
                                .as_str()
                                .ok_or_else(|| s3_error!(InvalidPolicyDocument, "field name must be a string"))?;
                            let prefix = arr[2]
                                .as_str()
                                .ok_or_else(|| s3_error!(InvalidPolicyDocument, "prefix must be a string"))?;
                            Self::validate_starts_with(&fields, &Self::normalize_field_name(field), prefix)?;
                        }
                        "content-length-range" => {
                            if arr.len() != 3 {
                                return Err(s3_error!(
                                    InvalidPolicyDocument,
                                    "content-length-range condition must have exactly 3 elements"
                                ));
                            }
                            let min = arr[1]
                                .as_u64()
                                .ok_or_else(|| s3_error!(InvalidPolicyDocument, "content-length-range min must be a number"))?;
                            let max = arr[2]
                                .as_u64()
                                .ok_or_else(|| s3_error!(InvalidPolicyDocument, "content-length-range max must be a number"))?;
                            Self::validate_content_length_range(file_size, min, max)?;
                        }
                        _ => {
                            return Err(s3_error!(InvalidPolicyDocument, "unknown condition operator"));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Normalize field name by removing $ prefix if present
    fn normalize_field_name(field: &str) -> String {
        if let Some(stripped) = field.strip_prefix('$') {
            stripped.to_string()
        } else {
            field.to_string()
        }
    }

    /// Validate exact match condition
    fn validate_exact_match(fields: &HashMap<String, String>, field: &str, expected_value: &serde_json::Value) -> S3Result<()> {
        let actual_value = fields
            .get(field)
            .ok_or_else(|| s3_error!(AccessDenied, "Invalid according to Policy: Policy Condition failed: [missing field]"))?;

        let expected_str = expected_value
            .as_str()
            .ok_or_else(|| s3_error!(InvalidPolicyDocument, "expected value must be a string"))?;

        if actual_value != expected_str {
            return Err(s3_error!(
                AccessDenied,
                "Invalid according to Policy: Policy Condition failed: [field mismatch]"
            ));
        }

        Ok(())
    }

    /// Validate starts-with condition
    fn validate_starts_with(fields: &HashMap<String, String>, field: &str, prefix: &str) -> S3Result<()> {
        let actual_value = fields
            .get(field)
            .ok_or_else(|| s3_error!(AccessDenied, "Invalid according to Policy: Policy Condition failed: [missing field]"))?;

        if !actual_value.starts_with(prefix) {
            return Err(s3_error!(
                AccessDenied,
                "Invalid according to Policy: Policy Condition failed: [starts-with]"
            ));
        }

        Ok(())
    }

    /// Validate content-length-range condition
    fn validate_content_length_range(file_size: u64, min: u64, max: u64) -> S3Result<()> {
        if file_size < min {
            return Err(s3_error!(
                EntityTooSmall,
                "Invalid according to Policy: Policy Condition failed: [content-length-range]"
            ));
        }
        if file_size > max {
            return Err(s3_error!(
                EntityTooLarge,
                "Invalid according to Policy: Policy Condition failed: [content-length-range]"
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_policy() {
        let policy_json = r#"
        {
            "expiration": "2025-12-31T12:00:00.000Z",
            "conditions": [
                {"bucket": "mybucket"},
                ["eq", "$key", "mykey"],
                ["starts-with", "$Content-Type", "text/"],
                ["content-length-range", 1024, 1048576]
            ]
        }
        "#;

        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_json.as_bytes());
        let policy = PostPolicy::from_base64(&policy_b64).unwrap();

        assert_eq!(policy.expiration, "2025-12-31T12:00:00.000Z");
        assert_eq!(policy.conditions.len(), 4);
    }

    #[test]
    fn test_validate_expiration_not_expired() {
        let policy = PostPolicy {
            expiration: "2099-12-31T12:00:00.000Z".to_string(),
            conditions: vec![],
        };

        assert!(policy.validate_expiration().is_ok());
    }

    #[test]
    fn test_validate_expiration_expired() {
        let policy = PostPolicy {
            expiration: "2020-01-01T12:00:00.000Z".to_string(),
            conditions: vec![],
        };

        assert!(policy.validate_expiration().is_err());
    }

    #[test]
    fn test_normalize_field_name() {
        assert_eq!(PostPolicy::normalize_field_name("$key"), "key");
        assert_eq!(PostPolicy::normalize_field_name("key"), "key");
        assert_eq!(PostPolicy::normalize_field_name("$Content-Type"), "Content-Type");
    }
}

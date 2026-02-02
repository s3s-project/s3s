//! POST Object Policy
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-HTTPPOSTConstructPolicy.html>

use crate::S3Error;
use crate::S3ErrorCode;
use crate::S3Result;
use crate::dto::Timestamp;
use crate::dto::TimestampFormat;
use crate::http::Multipart;

use std::collections::HashMap;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};

/// POST Object Policy
///
/// A POST policy is a JSON document that specifies conditions
/// that the request must meet when uploading objects using POST.
#[derive(Debug, Clone)]
pub struct PostPolicy {
    /// The expiration date of the policy in ISO 8601 format
    pub expiration: Timestamp,
    /// The conditions that must be met for the upload to succeed
    pub conditions: Vec<PostPolicyCondition>,
}

/// A condition in the POST policy
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostPolicyCondition {
    /// Exact match condition: the field value must equal the specified value
    Eq {
        /// Field name (without '$' prefix, lowercase)
        field: String,
        /// Expected value
        value: String,
    },
    /// Prefix match condition: the field value must start with the specified prefix
    StartsWith {
        /// Field name (without '$' prefix, lowercase)
        field: String,
        /// Expected prefix (empty string matches any value)
        prefix: String,
    },
    /// Content length range condition
    ContentLengthRange {
        /// Minimum content length (inclusive)
        min: u64,
        /// Maximum content length (inclusive)
        max: u64,
    },
}

/// Error type for POST policy parsing
#[derive(Debug, thiserror::Error)]
pub enum PostPolicyError {
    #[error("invalid base64 encoding: {0}")]
    Base64(#[from] base64_simd::Error),
    #[error("invalid UTF-8 encoding: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid condition format")]
    InvalidCondition,
    #[error("invalid expiration format: {0}")]
    InvalidExpiration(String),
}

impl PostPolicy {
    /// Parse a POST policy from a base64-encoded JSON string
    ///
    /// # Errors
    /// Returns an error if the base64 decoding or JSON parsing fails
    pub fn from_base64(encoded: &str) -> Result<Self, PostPolicyError> {
        let decoded = base64_simd::STANDARD.decode_to_vec(encoded)?;
        let json_str = std::str::from_utf8(&decoded)?;
        Self::from_json(json_str)
    }

    /// Parse a POST policy from a JSON string
    ///
    /// # Errors
    /// Returns an error if the JSON parsing fails
    pub fn from_json(json: &str) -> Result<Self, PostPolicyError> {
        let raw: RawPostPolicy = serde_json::from_str(json)?;

        let expiration = Timestamp::parse(TimestampFormat::DateTime, &raw.expiration)
            .map_err(|_| PostPolicyError::InvalidExpiration(raw.expiration.clone()))?;

        let conditions = raw
            .conditions
            .into_iter()
            .map(RawCondition::into_condition)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { expiration, conditions })
    }

    /// Validate the policy against the multipart form data
    ///
    /// # Arguments
    /// * `multipart` - The multipart form data
    /// * `file_size` - The size of the uploaded file in bytes
    /// * `now` - The current time (for expiration check)
    ///
    /// # Errors
    /// Returns `AccessDenied` if the policy has expired
    /// Returns `InvalidPolicyDocument` if any condition is not satisfied
    pub fn validate(&self, multipart: &Multipart, file_size: u64, now: time::OffsetDateTime) -> S3Result<()> {
        // Check expiration
        let expiration_time: time::OffsetDateTime = self.expiration.clone().into();
        if now >= expiration_time {
            return Err(S3Error::with_message(S3ErrorCode::AccessDenied, "Request has expired"));
        }

        // Check all conditions
        for condition in &self.conditions {
            Self::validate_condition(condition, multipart, file_size)?;
        }

        Ok(())
    }

    fn validate_condition(condition: &PostPolicyCondition, multipart: &Multipart, file_size: u64) -> S3Result<()> {
        match condition {
            PostPolicyCondition::Eq { field, value } => {
                let actual = Self::get_field_value(field, multipart);
                if actual.as_deref() != Some(value.as_str()) {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidPolicyDocument,
                        format!(
                            "Policy condition 'eq' for field '{field}' failed: expected '{value}', got '{}'",
                            actual.unwrap_or_default()
                        ),
                    ));
                }
            }
            PostPolicyCondition::StartsWith { field, prefix } => {
                let actual = Self::get_field_value(field, multipart);
                let actual_str = actual.as_deref().unwrap_or("");
                if !actual_str.starts_with(prefix.as_str()) {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidPolicyDocument,
                        format!(
                            "Policy condition 'starts-with' for field '{field}' failed: expected prefix '{prefix}', got '{actual_str}'"
                        ),
                    ));
                }
            }
            PostPolicyCondition::ContentLengthRange { min, max } => {
                if file_size < *min || file_size > *max {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidPolicyDocument,
                        format!("File size {file_size} is not within the allowed range [{min}, {max}]"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn get_field_value(field: &str, multipart: &Multipart) -> Option<String> {
        // Special handling for certain fields
        match field {
            "bucket" => {
                // bucket is typically in the URL path, not in multipart fields
                // For POST object, bucket comes from the endpoint URL
                multipart.find_field_value("bucket").map(String::from)
            }
            "key" => multipart.find_field_value("key").map(String::from),
            "content-type" => {
                // Content-Type of the file
                multipart.file.content_type.clone()
            }
            _ => {
                // For other fields, look in multipart fields (already lowercase)
                multipart.find_field_value(field).map(String::from)
            }
        }
    }

    /// Get the content-length-range condition if present
    #[must_use]
    pub fn content_length_range(&self) -> Option<(u64, u64)> {
        for condition in &self.conditions {
            if let PostPolicyCondition::ContentLengthRange { min, max } = condition {
                return Some((*min, *max));
            }
        }
        None
    }
}

/// Raw POST policy for deserialization
#[derive(Debug, Deserialize)]
struct RawPostPolicy {
    expiration: String,
    conditions: Vec<RawCondition>,
}

/// Raw condition that can be either array format or object format
#[derive(Debug)]
enum RawCondition {
    /// Array format: `["eq", "$key", "value"]` or `["starts-with", "$key", "prefix"]` or `["content-length-range", min, max]`
    Array(Vec<serde_json::Value>),
    /// Object format: `{"bucket": "mybucket"}` (shorthand for eq)
    Object(HashMap<String, String>),
}

impl<'de> Deserialize<'de> for RawCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawConditionVisitor;

        impl<'de> Visitor<'de> for RawConditionVisitor {
            type Value = RawCondition;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an array or object")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(RawCondition::Array(items))
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut items = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    items.insert(key, value);
                }
                Ok(RawCondition::Object(items))
            }
        }

        deserializer.deserialize_any(RawConditionVisitor)
    }
}

impl RawCondition {
    fn into_condition(self) -> Result<PostPolicyCondition, PostPolicyError> {
        match self {
            RawCondition::Array(items) => Self::parse_array_condition(&items),
            RawCondition::Object(map) => Self::parse_object_condition(map),
        }
    }

    fn parse_array_condition(items: &[serde_json::Value]) -> Result<PostPolicyCondition, PostPolicyError> {
        if items.is_empty() {
            return Err(PostPolicyError::InvalidCondition);
        }

        let operator = items[0].as_str().ok_or(PostPolicyError::InvalidCondition)?;

        match operator.to_ascii_lowercase().as_str() {
            "eq" => {
                if items.len() != 3 {
                    return Err(PostPolicyError::InvalidCondition);
                }
                let field = items[1].as_str().ok_or(PostPolicyError::InvalidCondition)?;
                let value = items[2].as_str().ok_or(PostPolicyError::InvalidCondition)?;
                Ok(PostPolicyCondition::Eq {
                    field: normalize_field_name(field),
                    value: value.to_owned(),
                })
            }
            "starts-with" => {
                if items.len() != 3 {
                    return Err(PostPolicyError::InvalidCondition);
                }
                let field = items[1].as_str().ok_or(PostPolicyError::InvalidCondition)?;
                let prefix = items[2].as_str().ok_or(PostPolicyError::InvalidCondition)?;
                Ok(PostPolicyCondition::StartsWith {
                    field: normalize_field_name(field),
                    prefix: prefix.to_owned(),
                })
            }
            "content-length-range" => {
                if items.len() != 3 {
                    return Err(PostPolicyError::InvalidCondition);
                }
                let min = items[1].as_u64().ok_or(PostPolicyError::InvalidCondition)?;
                let max = items[2].as_u64().ok_or(PostPolicyError::InvalidCondition)?;
                Ok(PostPolicyCondition::ContentLengthRange { min, max })
            }
            _ => Err(PostPolicyError::InvalidCondition),
        }
    }

    fn parse_object_condition(map: HashMap<String, String>) -> Result<PostPolicyCondition, PostPolicyError> {
        // Object format is shorthand for exact match
        // {"bucket": "mybucket"} means ["eq", "$bucket", "mybucket"]
        if map.len() != 1 {
            return Err(PostPolicyError::InvalidCondition);
        }
        let (field, value) = map.into_iter().next().ok_or(PostPolicyError::InvalidCondition)?;
        Ok(PostPolicyCondition::Eq {
            field: normalize_field_name(&field),
            value,
        })
    }
}

/// Normalize field name by removing '$' prefix and converting to lowercase
fn normalize_field_name(field: &str) -> String {
    let field = field.strip_prefix('$').unwrap_or(field);
    field.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_timestamp(s: &str) -> Timestamp {
        Timestamp::parse(TimestampFormat::DateTime, s).unwrap()
    }

    #[test]
    fn test_parse_policy_json() {
        let json = r#"{
            "expiration": "2030-01-01T00:00:00.000Z",
            "conditions": [
                ["eq", "$bucket", "mybucket"],
                ["starts-with", "$key", "user/"],
                ["content-length-range", 0, 10485760],
                {"acl": "public-read"}
            ]
        }"#;

        let policy = PostPolicy::from_json(json).unwrap();

        assert_eq!(policy.expiration, make_timestamp("2030-01-01T00:00:00.000Z"));
        assert_eq!(policy.conditions.len(), 4);

        assert_eq!(
            policy.conditions[0],
            PostPolicyCondition::Eq {
                field: "bucket".to_owned(),
                value: "mybucket".to_owned()
            }
        );

        assert_eq!(
            policy.conditions[1],
            PostPolicyCondition::StartsWith {
                field: "key".to_owned(),
                prefix: "user/".to_owned()
            }
        );

        assert_eq!(policy.conditions[2], PostPolicyCondition::ContentLengthRange { min: 0, max: 10485760 });

        assert_eq!(
            policy.conditions[3],
            PostPolicyCondition::Eq {
                field: "acl".to_owned(),
                value: "public-read".to_owned()
            }
        );
    }

    #[test]
    fn test_parse_policy_base64() {
        let json = r#"{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["eq","$bucket","test"]]}"#;
        let encoded = base64_simd::STANDARD.encode_to_string(json);

        let policy = PostPolicy::from_base64(&encoded).unwrap();
        assert_eq!(policy.conditions.len(), 1);
    }

    #[test]
    fn test_parse_invalid_base64() {
        let result = PostPolicy::from_base64("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_json() {
        let encoded = base64_simd::STANDARD.encode_to_string("{invalid json}");
        let result = PostPolicy::from_base64(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_expiration() {
        let json = r#"{"expiration":"not-a-date","conditions":[]}"#;
        let result = PostPolicy::from_json(json);
        assert!(matches!(result, Err(PostPolicyError::InvalidExpiration(_))));
    }

    #[test]
    fn test_parse_invalid_condition_format() {
        let json = r#"{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["unknown-op","$key","value"]]}"#;
        let result = PostPolicy::from_json(json);
        assert!(matches!(result, Err(PostPolicyError::InvalidCondition)));
    }

    #[test]
    fn test_content_length_range() {
        let json = r#"{
            "expiration": "2030-01-01T00:00:00.000Z",
            "conditions": [
                ["content-length-range", 100, 1000]
            ]
        }"#;

        let policy = PostPolicy::from_json(json).unwrap();
        assert_eq!(policy.content_length_range(), Some((100, 1000)));
    }

    #[test]
    fn test_no_content_length_range() {
        let json = r#"{
            "expiration": "2030-01-01T00:00:00.000Z",
            "conditions": [
                ["eq", "$bucket", "test"]
            ]
        }"#;

        let policy = PostPolicy::from_json(json).unwrap();
        assert_eq!(policy.content_length_range(), None);
    }

    #[test]
    fn test_normalize_field_name() {
        assert_eq!(normalize_field_name("$bucket"), "bucket");
        assert_eq!(normalize_field_name("$Key"), "key");
        assert_eq!(normalize_field_name("bucket"), "bucket");
        assert_eq!(normalize_field_name("X-Amz-Meta-Custom"), "x-amz-meta-custom");
    }

    // Validation tests require a mock Multipart, which is complex to construct.
    // These will be tested in integration tests.
}

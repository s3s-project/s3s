// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// `SigV4` POST form signature fields (extracted from the multipart field table)
pub struct PostSignatureV4<'a> {
    /// base64-encoded policy
    pub policy: &'a str,
    /// x-amz-algorithm
    pub x_amz_algorithm: &'a str,
    /// x-amz-credential
    pub x_amz_credential: &'a str,
    /// x-amz-date
    pub x_amz_date: &'a str,
    /// x-amz-signature
    pub x_amz_signature: &'a str,
}

/// [`PostSignatureV4::extract`] error
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum PostSignatureError {
    /// missing required field
    #[error("missing field: {0}")]
    MissingField(&'static str),
    /// duplicate field (value not unique)
    #[error("duplicate field: {0}")]
    DuplicateField(&'static str),
}

impl<'a> PostSignatureV4<'a> {
    /// Extracts `policy` / `x-amz-algorithm` / `x-amz-credential` /
    /// `x-amz-date` / `x-amz-signature` from the field table.
    ///
    /// `fields` is the borrowed multipart field table (`multipart.fields()`);
    /// field names must already be normalized to lowercase (contract).
    ///
    /// Duplicate fields are rejected: `s3s`'s other multipart consumers select
    /// the last value on sorted fields, so a non-unique value would be
    /// ambiguous between signature verification and policy validation.
    ///
    /// # Errors
    /// Returns [`PostSignatureError`] for a missing or duplicate field.
    pub fn extract(fields: &'a [(String, String)]) -> Result<Self, PostSignatureError> {
        let get = |name: &'static str| -> Result<&'a str, PostSignatureError> {
            let mut iter = fields.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str());
            let value = iter.next().ok_or(PostSignatureError::MissingField(name))?;
            if iter.next().is_some() {
                return Err(PostSignatureError::DuplicateField(name));
            }
            Ok(value)
        };
        Ok(Self {
            policy: get("policy")?,
            x_amz_algorithm: get("x-amz-algorithm")?,
            x_amz_credential: get("x-amz-credential")?,
            x_amz_date: get("x-amz-date")?,
            x_amz_signature: get("x-amz-signature")?,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    fn make_fields(fields: Vec<(&str, &str)>) -> Vec<(String, String)> {
        fields.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
    }

    #[test]
    fn extract_success() {
        let fields = make_fields(vec![
            ("policy", "test-policy"),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", "AKID/20130524/us-east-1/s3/aws4_request"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-signature", "abc"),
        ]);
        let v = PostSignatureV4::extract(&fields).unwrap();
        assert_eq!(v.policy, "test-policy");
        assert_eq!(v.x_amz_algorithm, "AWS4-HMAC-SHA256");
        assert_eq!(v.x_amz_credential, "AKID/20130524/us-east-1/s3/aws4_request");
        assert_eq!(v.x_amz_date, "20130524T000000Z");
        assert_eq!(v.x_amz_signature, "abc");
    }

    #[test]
    fn extract_missing_each_field() {
        let all = [
            ("policy", "test-policy"),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", "AKID/20130524/us-east-1/s3/aws4_request"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-signature", "abc"),
        ];
        for (name, _) in all {
            let fields: Vec<(String, String)> = all
                .iter()
                .filter(|&&(n, _)| n != name)
                .map(|&(n, v)| (n.to_owned(), v.to_owned()))
                .collect();
            assert!(
                matches!(PostSignatureV4::extract(&fields), Err(PostSignatureError::MissingField(f)) if f == name),
                "{name} must be required"
            );
        }
    }

    #[test]
    fn extract_rejects_duplicate_fields() {
        let mut fields = make_fields(vec![
            ("policy", "test-policy"),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", "AKID/20130524/us-east-1/s3/aws4_request"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-signature", "abc"),
        ]);
        fields.push(("policy".to_owned(), "evil-policy".to_owned()));
        assert!(matches!(
            PostSignatureV4::extract(&fields),
            Err(PostSignatureError::DuplicateField("policy"))
        ));
    }

    #[test]
    fn extract_error_display() {
        let err = PostSignatureError::MissingField("policy");
        assert_eq!(err.to_string(), "missing field: policy");
        let err = PostSignatureError::DuplicateField("x-amz-signature");
        assert_eq!(err.to_string(), "duplicate field: x-amz-signature");
    }
}

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

impl<'a> PostSignatureV4<'a> {
    /// Extracts `policy` / `x-amz-algorithm` / `x-amz-credential` /
    /// `x-amz-date` / `x-amz-signature` from the field table.
    ///
    /// `fields` is the borrowed multipart field table (`multipart.fields()`);
    /// field names must already be normalized to lowercase (contract).
    #[must_use]
    pub fn extract(fields: &'a [(String, String)]) -> Option<Self> {
        let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        Some(Self {
            policy: get("policy")?,
            x_amz_algorithm: get("x-amz-algorithm")?,
            x_amz_credential: get("x-amz-credential")?,
            x_amz_date: get("x-amz-date")?,
            x_amz_signature: get("x-amz-signature")?,
        })
    }
}

#[cfg(test)]
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
    fn extract_missing_field() {
        let fields = make_fields(vec![
            ("policy", "test-policy"),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", "AKID/20130524/us-east-1/s3/aws4_request"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        assert!(PostSignatureV4::extract(&fields).is_none());
    }
}

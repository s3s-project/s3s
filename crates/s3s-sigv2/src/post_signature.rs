// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// `SigV2` POST form signature fields (extracted from the multipart field table)
pub struct PostSignatureV2<'a> {
    /// base64-encoded policy
    pub policy: &'a str,
    /// access key id
    pub access_key_id: &'a str,
    /// signature (standard Base64, HMAC-SHA1)
    pub signature: &'a str,
}

impl<'a> PostSignatureV2<'a> {
    /// Extracts `policy` / `awsaccesskeyid` / `signature` from the field table.
    ///
    /// `fields` is the borrowed multipart field table (`multipart.fields()`);
    /// field names must already be normalized to lowercase (contract).
    ///
    /// Duplicate fields are rejected: `s3s`'s other multipart consumers select
    /// the last value on sorted fields, so a non-unique value would be
    /// ambiguous between signature verification and policy validation.
    #[must_use]
    pub fn extract(fields: &'a [(String, String)]) -> Option<Self> {
        let get = |name: &str| {
            let mut iter = fields.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str());
            let value = iter.next()?;
            (iter.next().is_none()).then_some(value)
        };
        Some(Self {
            policy: get("policy")?,
            access_key_id: get("awsaccesskeyid")?,
            signature: get("signature")?,
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
        let fields = make_fields(vec![("policy", "test-policy"), ("awsaccesskeyid", "AKID"), ("signature", "sig123")]);
        let v = PostSignatureV2::extract(&fields).unwrap();
        assert_eq!(v.policy, "test-policy");
        assert_eq!(v.access_key_id, "AKID");
        assert_eq!(v.signature, "sig123");
    }

    #[test]
    fn extract_missing_policy() {
        let fields = make_fields(vec![("awsaccesskeyid", "AKID"), ("signature", "sig123")]);
        assert!(PostSignatureV2::extract(&fields).is_none());
    }

    #[test]
    fn extract_missing_access_key() {
        let fields = make_fields(vec![("policy", "test-policy"), ("signature", "sig123")]);
        assert!(PostSignatureV2::extract(&fields).is_none());
    }

    #[test]
    fn extract_missing_signature() {
        let fields = make_fields(vec![("policy", "test-policy"), ("awsaccesskeyid", "AKID")]);
        assert!(PostSignatureV2::extract(&fields).is_none());
    }

    #[test]
    fn extract_rejects_duplicate_fields() {
        let fields = make_fields(vec![
            ("policy", "test-policy"),
            ("awsaccesskeyid", "AKID"),
            ("signature", "sig123"),
            ("policy", "evil-policy"),
        ]);
        assert!(PostSignatureV2::extract(&fields).is_none());
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use jiff::Timestamp;

/// `SigV2` presigned URL information (all borrowed).
pub struct PresignedUrlV2<'a> {
    /// access key id
    pub access_key: &'a str,
    /// expiration time
    pub expires_time: Timestamp,
    /// the decoded base64 signature (query values must already be decoded)
    pub signature: &'a str,
}

/// [`PresignedUrlV2`] parse error
#[derive(Debug, thiserror::Error)]
#[error("ParsePresignedUrlError")]
pub struct ParsePresignedUrlError {
    /// private placeholder
    _priv: (),
}

/// Returns the unique value of `name` in `pairs`, or `None` when the field is
/// missing or duplicated.
///
/// Query values must already be percent-decoded (contract).
fn get_unique<'a>(pairs: &'a [(impl AsRef<str>, impl AsRef<str>)], name: &str) -> Option<&'a str> {
    let mut iter = pairs
        .iter()
        .filter(|(key, _)| key.as_ref() == name)
        .map(|(_, value)| value.as_ref());
    let value = iter.next()?;
    (iter.next().is_none()).then_some(value)
}

impl<'a> PresignedUrlV2<'a> {
    /// Parses a `SigV2` presigned URL from query pairs.
    ///
    /// The fields `AWSAccessKeyId`/`Expires`/`Signature` must be present and
    /// unique. Query values must already be percent-decoded (contract; the
    /// `OrderedQs` type in `s3s` satisfies it) — the signature is NOT decoded
    /// again here.
    ///
    /// # Errors
    /// Returns [`ParsePresignedUrlError`] for missing/duplicate fields or an
    /// invalid expiration timestamp.
    pub fn parse(qs: &'a [(impl AsRef<str>, impl AsRef<str>)]) -> Result<Self, ParsePresignedUrlError> {
        let err = || ParsePresignedUrlError { _priv: () };

        let access_key = get_unique(qs, "AWSAccessKeyId").ok_or_else(err)?;
        let expires_str = get_unique(qs, "Expires").ok_or_else(err)?;
        let signature = get_unique(qs, "Signature").ok_or_else(err)?;

        let expires_time = parse_unix_timestamp(expires_str).ok_or_else(err)?;

        Ok(Self {
            access_key,
            expires_time,
            signature,
        })
    }
}

fn parse_unix_timestamp(s: &str) -> Option<Timestamp> {
    let ts = s.parse::<i64>().ok().filter(|&x| x >= 0)?;
    Timestamp::from_second(ts).ok()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_fields_and_uses_decoded_signature() {
        // Values are already decoded (contract); `%3D` was decoded to `=` by
        // the query parser before reaching us.
        let qs = [
            ("AWSAccessKeyId", "AKIAIOSFODNN7EXAMPLE"),
            ("Signature", "1No4mq5ETf02z8aet9voy6gui6E="),
            ("Expires", "1175139620"),
        ];

        let info = PresignedUrlV2::parse(&qs).unwrap();

        assert_eq!(info.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(info.signature, "1No4mq5ETf02z8aet9voy6gui6E=");
        assert_eq!(info.expires_time.as_second(), 1_175_139_620);
    }

    #[test]
    fn parse_rejects_missing_required_fields() {
        let qs = [("AWSAccessKeyId", "AKIAIOSFODNN7EXAMPLE"), ("Expires", "1175139620")];
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_missing_access_key() {
        let qs = [("Signature", "abc"), ("Expires", "1175139620")];
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_missing_expires() {
        let qs = [("AWSAccessKeyId", "AKIAIOSFODNN7EXAMPLE"), ("Signature", "abc")];
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_negative_timestamp() {
        let qs = [
            ("AWSAccessKeyId", "AKIAIOSFODNN7EXAMPLE"),
            ("Signature", "abc"),
            ("Expires", "-1"),
        ];
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_duplicate_signature_fields() {
        let qs = [
            ("AWSAccessKeyId", "AKIAIOSFODNN7EXAMPLE"),
            ("Signature", "abc"),
            ("Signature", "def"),
            ("Expires", "1175139620"),
        ];
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_unix_timestamp_rejects_non_numeric_input() {
        assert!(parse_unix_timestamp("abc").is_none());
    }
}

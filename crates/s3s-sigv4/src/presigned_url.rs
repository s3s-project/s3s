// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! presigned url information

use super::AmzDate;
use super::CredentialV4;

use crate::crypto::is_sha256_checksum;

use smallvec::SmallVec;

/// Presigned url information
#[derive(Debug)]
pub struct PresignedUrlV4<'a> {
    /// algorithm
    pub algorithm: &'a str,
    /// credential
    pub credential: CredentialV4<'a>,
    /// amz date
    pub amz_date: AmzDate,
    /// expires
    pub expires: jiff::SignedDuration,
    /// signed headers
    pub signed_headers: SmallVec<[&'a str; 16]>,
    /// signature
    pub signature: &'a str,
}

/// [`PresignedUrlV4`]
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

impl<'a> PresignedUrlV4<'a> {
    /// Parses `PresignedUrl` from query pairs with a caller-supplied
    /// `X-Amz-Expires` upper bound.
    ///
    /// `max_expires_secs` is the maximum allowed `X-Amz-Expires` value in seconds.
    /// Values larger than this upper bound are rejected during parsing.
    ///
    /// Query values must already be percent-decoded (contract; `OrderedQs` from
    /// `s3s` satisfies it).
    ///
    /// # Errors
    /// Returns `ParsePresignedUrlError` if it failed to parse
    pub fn parse(qs: &'a [(impl AsRef<str>, impl AsRef<str>)], max_expires_secs: u32) -> Result<Self, ParsePresignedUrlError> {
        Self::parse_impl(qs, max_expires_secs)
    }

    fn parse_impl(qs: &'a [(impl AsRef<str>, impl AsRef<str>)], max_expires_secs: u32) -> Result<Self, ParsePresignedUrlError> {
        let err = || ParsePresignedUrlError { _priv: () };

        let algorithm = get_unique(qs, "X-Amz-Algorithm").ok_or_else(err)?;
        let credential_str = get_unique(qs, "X-Amz-Credential").ok_or_else(err)?;
        let date = get_unique(qs, "X-Amz-Date").ok_or_else(err)?;
        let expires = get_unique(qs, "X-Amz-Expires").ok_or_else(err)?;
        let signed_headers = get_unique(qs, "X-Amz-SignedHeaders").ok_or_else(err)?;
        let signature = get_unique(qs, "X-Amz-Signature").ok_or_else(err)?;

        let credential = CredentialV4::parse(credential_str).map_err(|_e| err())?;

        let amz_date = AmzDate::parse(date).map_err(|_e| err())?;

        let expires = parse_expires(expires, max_expires_secs).ok_or_else(err)?;

        if !signed_headers.is_ascii() {
            return Err(err());
        }
        let signed_headers = signed_headers.split(';').collect();

        if !is_sha256_checksum(signature) {
            return Err(err());
        }

        Ok(Self {
            algorithm,
            credential,
            amz_date,
            expires,
            signed_headers,
            signature,
        })
    }
}

fn parse_expires(s: &str, max_expires_secs: u32) -> Option<jiff::SignedDuration> {
    // u32 parse rejects negative values and non-integers implicitly
    let x = s.parse::<u32>().ok()?;
    if x > max_expires_secs {
        return None;
    }
    Some(jiff::SignedDuration::from_secs(i64::from(x)))
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

    fn default_max_expires_secs() -> u32 {
        7 * 24 * 60 * 60
    }

    fn make_qs<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        pairs.to_vec()
    }

    fn valid_query_strings() -> [(&'static str, &'static str); 6] {
        [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", "AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"),
            ("X-Amz-Date", "20130524T000000Z"),
            ("X-Amz-Expires", "86400"),
            ("X-Amz-SignedHeaders", "host"),
            ("X-Amz-Signature", "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"),
        ]
    }

    #[test]
    fn parse_extracts_presigned_url_fields() {
        let qs = make_qs(&valid_query_strings());

        let info = PresignedUrlV4::parse(&qs, default_max_expires_secs()).unwrap();

        assert_eq!(info.algorithm, "AWS4-HMAC-SHA256");
        assert_eq!(info.credential.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(info.credential.aws_region, "us-east-1");
        assert_eq!(info.credential.aws_service, "s3");
        assert_eq!(info.expires.as_secs(), 86_400);
        assert_eq!(info.signed_headers.as_slice(), ["host"]);
        assert_eq!(info.signature, "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404");
        assert!(info.amz_date.to_time().is_some());
    }

    #[test]
    fn parse_rejects_missing_query_fields() {
        let qs = make_qs(&valid_query_strings()[..5]);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }

    #[test]
    fn parse_rejects_missing_each_field() {
        for field in [
            "X-Amz-Algorithm",
            "X-Amz-Credential",
            "X-Amz-Date",
            "X-Amz-Expires",
            "X-Amz-SignedHeaders",
            "X-Amz-Signature",
        ] {
            let mut pairs = valid_query_strings().to_vec();
            pairs.retain(|&(name, _)| name != field);
            assert!(
                PresignedUrlV4::parse(&pairs, default_max_expires_secs()).is_err(),
                "{field} must be required"
            );
        }
    }

    #[test]
    fn parse_rejects_non_ascii_signed_headers() {
        let mut pairs = valid_query_strings();
        pairs[4] = ("X-Amz-SignedHeaders", "höst");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }

    #[test]
    fn parse_rejects_invalid_credential() {
        let mut pairs = valid_query_strings();
        pairs[1] = ("X-Amz-Credential", "bad-credential");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }

    #[test]
    fn parse_rejects_invalid_date() {
        let mut pairs = valid_query_strings();
        pairs[2] = ("X-Amz-Date", "not-a-date");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }

    #[test]
    fn parse_rejects_invalid_signature() {
        let mut pairs = valid_query_strings();
        pairs[5] = ("X-Amz-Signature", "not-a-sha256");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }

    #[test]
    fn parse_rejects_invalid_expires() {
        for expires in ["604801", "4294967295", "4294967296", "999999999999999999999999", "NaN", "-1"] {
            let mut pairs = valid_query_strings();
            pairs[3] = ("X-Amz-Expires", expires);
            let qs = make_qs(&pairs);
            assert!(
                PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err(),
                "X-Amz-Expires={expires} must be rejected"
            );
        }
    }

    #[test]
    fn parse_accepts_expires_boundaries() {
        for expires in ["0", "1", "604800"] {
            let mut pairs = valid_query_strings();
            pairs[3] = ("X-Amz-Expires", expires);
            let qs = make_qs(&pairs);
            let parsed = PresignedUrlV4::parse(&qs, default_max_expires_secs()).expect("boundary expiration should parse");
            assert_eq!(parsed.expires.as_secs().to_string(), expires);
        }
    }

    #[test]
    fn parse_respects_custom_max_expires() {
        let mut pairs = valid_query_strings();
        pairs[3] = ("X-Amz-Expires", "604801");
        let qs = make_qs(&pairs);

        let parsed = PresignedUrlV4::parse(&qs, 700_000).expect("custom max should allow larger expires");
        assert_eq!(parsed.expires.as_secs(), 604_801);

        assert!(PresignedUrlV4::parse(&qs, 3_600).is_err(), "custom max should reject larger expires");
    }

    #[test]
    fn parse_rejects_duplicate_expires() {
        let mut pairs = valid_query_strings().to_vec();
        pairs.push(("X-Amz-Expires", "1"));
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs, default_max_expires_secs()).is_err());
    }
}

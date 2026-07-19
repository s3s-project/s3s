//! presigned url information

use super::AmzDate;
use super::CredentialV4;

use crate::http::OrderedQs;
use crate::utils::crypto::is_sha256_checksum;

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
    pub expires: time::Duration,
    /// signed headers
    pub signed_headers: SmallVec<[&'a str; 16]>,
    /// signature
    pub signature: &'a str,
}

/// [`PresignedUrlV4`]
#[derive(Debug, thiserror::Error)]
#[error("ParsePresignedUrlError")]
pub struct ParsePresignedUrlError {
    /// priv place holder
    _priv: (),
}

/// query strings of a presigned url
struct PresignedQs<'a> {
    /// X-Amz-Algorithm
    algorithm: &'a str,
    /// X-Amz-Credential
    credential: &'a str,
    /// X-Amz-Date
    date: &'a str,
    /// X-Amz-Expires
    expires: &'a str,
    /// X-Amz-SignedHeaders
    signed_headers: &'a str,
    /// X-Amz-Signature
    signature: &'a str,
}

impl<'a> PresignedQs<'a> {
    /// Creates `PresignedQs` from `OrderedQs`
    fn from_ordered_qs(qs: &'a OrderedQs) -> Option<Self> {
        Some(PresignedQs {
            algorithm: qs.get_unique("X-Amz-Algorithm")?,
            credential: qs.get_unique("X-Amz-Credential")?,
            date: qs.get_unique("X-Amz-Date")?,
            expires: qs.get_unique("X-Amz-Expires")?,
            signed_headers: qs.get_unique("X-Amz-SignedHeaders")?,
            signature: qs.get_unique("X-Amz-Signature")?,
        })
    }
}

impl<'a> PresignedUrlV4<'a> {
    /// Parses `PresignedUrl` from query
    ///
    /// # Errors
    /// Returns `ParsePresignedUrlError` if it failed to parse
    pub fn parse(qs: &'a OrderedQs) -> Result<Self, ParsePresignedUrlError> {
        let err = || ParsePresignedUrlError { _priv: () };

        let info = PresignedQs::from_ordered_qs(qs).ok_or_else(err)?;

        let algorithm = info.algorithm;

        let credential = CredentialV4::parse(info.credential).map_err(|_e| err())?;

        let amz_date = AmzDate::parse(info.date).map_err(|_e| err())?;

        let expires = parse_expires(info.expires).ok_or_else(err)?;

        if !info.signed_headers.is_ascii() {
            return Err(err());
        }
        let signed_headers = info.signed_headers.split(';').collect();

        if !is_sha256_checksum(info.signature) {
            return Err(err());
        }
        let signature = info.signature;

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

fn parse_expires(s: &str) -> Option<time::Duration> {
    let x = s.parse::<u32>().ok()?;
    Some(time::Duration::new(i64::from(x), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::OrderedQs;

    fn make_qs(pairs: &[(&str, &str)]) -> OrderedQs {
        OrderedQs::from_vec_unchecked(
            pairs
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
        )
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

        let info = PresignedUrlV4::parse(&qs).unwrap();

        assert_eq!(info.algorithm, "AWS4-HMAC-SHA256");
        assert_eq!(info.credential.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(info.credential.aws_region, "us-east-1");
        assert_eq!(info.credential.aws_service, "s3");
        assert_eq!(info.expires.whole_seconds(), 86_400);
        assert_eq!(info.signed_headers.as_slice(), ["host"]);
        assert_eq!(info.signature, "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404");
        assert!(info.amz_date.to_time().is_some());
    }

    #[test]
    fn parse_rejects_missing_query_fields() {
        let qs = make_qs(&valid_query_strings()[..5]);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_non_ascii_signed_headers() {
        let mut pairs = valid_query_strings();
        pairs[4] = ("X-Amz-SignedHeaders", "höst");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_invalid_credential() {
        let mut pairs = valid_query_strings();
        pairs[1] = ("X-Amz-Credential", "bad-credential");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_invalid_date() {
        let mut pairs = valid_query_strings();
        pairs[2] = ("X-Amz-Date", "not-a-date");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_invalid_signature() {
        let mut pairs = valid_query_strings();
        pairs[5] = ("X-Amz-Signature", "not-a-sha256");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_invalid_expires() {
        let mut pairs = valid_query_strings();
        pairs[3] = ("X-Amz-Expires", "NaN");
        let qs = make_qs(&pairs);
        assert!(PresignedUrlV4::parse(&qs).is_err());
    }
}

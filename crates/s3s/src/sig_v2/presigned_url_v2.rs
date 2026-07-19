use crate::http::OrderedQs;

use std::borrow::Cow;

use time::OffsetDateTime;

pub struct PresignedUrlV2<'a> {
    pub access_key: &'a str,
    pub expires_time: OffsetDateTime,
    pub signature: Cow<'a, str>,
}

/// [`PresignedUrlV2`]
#[derive(Debug, thiserror::Error)]
#[error("ParsePresignedUrlError")]
pub struct ParsePresignedUrlError {
    /// priv place holder
    _priv: (),
}

impl<'a> PresignedUrlV2<'a> {
    pub fn parse(qs: &'a OrderedQs) -> Result<Self, ParsePresignedUrlError> {
        let err = || ParsePresignedUrlError { _priv: () };

        let access_key = qs.get_unique("AWSAccessKeyId").ok_or_else(err)?;
        let expires_str = qs.get_unique("Expires").ok_or_else(err)?;
        let signature = qs.get_unique("Signature").ok_or_else(err)?;

        let expires_time = parse_unix_timestamp(expires_str).ok_or_else(err)?;
        let signature = urlencoding::decode(signature).map_err(|_| err())?;

        Ok(Self {
            access_key,
            expires_time,
            signature,
        })
    }
}

fn parse_unix_timestamp(s: &str) -> Option<OffsetDateTime> {
    let ts = s.parse::<i64>().ok().filter(|&x| x >= 0)?;
    OffsetDateTime::from_unix_timestamp(ts).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::OrderedQs;

    #[test]
    fn parse_extracts_fields_and_decodes_signature() {
        let qs = OrderedQs::parse(concat!(
            "AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE",
            "&Signature=1No4mq5ETf02z8aet9voy6gui6E%3D",
            "&Expires=1175139620",
        ))
        .unwrap();

        let info = PresignedUrlV2::parse(&qs).unwrap();

        assert_eq!(info.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(info.signature, "1No4mq5ETf02z8aet9voy6gui6E=");
        assert_eq!(info.expires_time.unix_timestamp(), 1_175_139_620);
    }

    #[test]
    fn parse_rejects_missing_required_fields() {
        let qs = OrderedQs::parse("AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Expires=1175139620").unwrap();
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_negative_timestamp() {
        let qs = OrderedQs::parse("AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Signature=abc&Expires=-1").unwrap();
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_rejects_duplicate_signature_fields() {
        let qs = OrderedQs::parse("AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Signature=abc&Signature=def&Expires=1175139620").unwrap();
        assert!(PresignedUrlV2::parse(&qs).is_err());
    }

    #[test]
    fn parse_unix_timestamp_rejects_non_numeric_input() {
        assert!(parse_unix_timestamp("abc").is_none());
    }
}

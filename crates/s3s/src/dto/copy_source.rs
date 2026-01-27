//! x-amz-copy-source

use crate::http;
use crate::path;

use std::fmt::Write;

/// x-amz-copy-source
#[derive(Debug, Clone, PartialEq)]
pub enum CopySource {
    /// bucket repr
    Bucket {
        /// bucket
        bucket: Box<str>,
        /// key
        key: Box<str>,
        /// version id
        version_id: Option<Box<str>>,
    },
    /// access point repr
    AccessPoint {
        /// region
        region: Box<str>,
        /// account id
        account_id: Box<str>,
        /// access point name
        access_point_name: Box<str>,
        /// key
        key: Box<str>,
    },
}

/// [`CopySource`]
#[derive(Debug, thiserror::Error)]
pub enum ParseCopySourceError {
    /// pattern mismatch
    #[error("ParseAmzCopySourceError: PatternMismatch")]
    PatternMismatch,

    /// invalid bucket name
    #[error("ParseAmzCopySourceError: InvalidBucketName")]
    InvalidBucketName,

    /// invalid key
    #[error("ParseAmzCopySourceError: InvalidKey")]
    InvalidKey,

    #[error("ParseAmzCopySourceError: InvalidEncoding")]
    InvalidEncoding,
}

impl CopySource {
    /// Parses [`CopySource`] from header
    /// # Errors
    /// Returns an error if the header is invalid
    pub fn parse(header: &str) -> Result<Self, ParseCopySourceError> {
        let (path_part, version_id) = if let Some(idx) = header.find("?versionId=") {
            let (path, version_part) = header.split_at(idx);
            let version_id_raw = version_part.strip_prefix("?versionId=");
            let version_id = version_id_raw
                .map(urlencoding::decode)
                .transpose()
                .map_err(|_| ParseCopySourceError::InvalidEncoding)?;
            (path, version_id)
        } else {
            (header, None)
        };
        let header = urlencoding::decode(path_part).map_err(|_| ParseCopySourceError::InvalidEncoding)?;
        let header = header.strip_prefix('/').unwrap_or(&header);

        // FIXME: support access point
        match header.split_once('/') {
            None => Err(ParseCopySourceError::PatternMismatch),
            Some((bucket, key)) => {
                if !path::check_bucket_name(bucket) {
                    return Err(ParseCopySourceError::InvalidBucketName);
                }
                if !path::check_key(key) {
                    return Err(ParseCopySourceError::InvalidKey);
                }
                Ok(Self::Bucket {
                    bucket: bucket.into(),
                    key: key.into(),
                    version_id: version_id.map(Into::into),
                })
            }
        }
    }

    #[must_use]
    pub fn format_to_string(&self) -> String {
        let mut buf = String::new();
        match self {
            CopySource::Bucket { bucket, key, version_id } => {
                write!(&mut buf, "{bucket}/{key}").unwrap();
                if let Some(version_id) = version_id {
                    write!(&mut buf, "?versionId={version_id}").unwrap();
                }
            }
            CopySource::AccessPoint { .. } => {
                unimplemented!()
            }
        }
        buf
    }
}

impl http::TryFromHeaderValue for CopySource {
    type Error = ParseCopySourceError;

    fn try_from_header_value(val: &http::HeaderValue) -> Result<Self, Self::Error> {
        let header = val.to_str().map_err(|_| ParseCopySourceError::InvalidEncoding)?;
        Self::parse(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_slash_and_percent_decoding() {
        let header = "/awsexamplebucket/reports/file%3Fversion.txt?versionId=abc";
        let val = CopySource::parse(header).unwrap();
        match val {
            CopySource::Bucket { bucket, key, version_id } => {
                assert_eq!(&*bucket, "awsexamplebucket");
                assert_eq!(&*key, "reports/file?version.txt");
                assert_eq!(version_id.as_deref().unwrap(), "abc");
            }
            CopySource::AccessPoint { .. } => panic!(),
        }
    }

    #[test]
    fn path_style() {
        {
            let header = "awsexamplebucket/reports/january.pdf";
            let val = CopySource::parse(header).unwrap();
            match val {
                CopySource::Bucket { bucket, key, version_id } => {
                    assert_eq!(&*bucket, "awsexamplebucket");
                    assert_eq!(&*key, "reports/january.pdf");
                    assert!(version_id.is_none());
                }
                CopySource::AccessPoint { .. } => panic!(),
            }
        }

        {
            let header = "awsexamplebucket/reports/january.pdf?versionId=QUpfdndhfd8438MNFDN93jdnJFkdmqnh893";
            let val = CopySource::parse(header).unwrap();
            match val {
                CopySource::Bucket { bucket, key, version_id } => {
                    assert_eq!(&*bucket, "awsexamplebucket");
                    assert_eq!(&*key, "reports/january.pdf");
                    assert_eq!(version_id.as_deref().unwrap(), "QUpfdndhfd8438MNFDN93jdnJFkdmqnh893");
                }
                CopySource::AccessPoint { .. } => panic!(),
            }
        }
    }
}

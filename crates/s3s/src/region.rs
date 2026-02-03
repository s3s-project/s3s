//! S3 Region utilities
//!
//! This module provides types and utilities for working with S3 regions.

use std::borrow::Cow;
use std::fmt;

/// A validated S3 region identifier.
///
/// AWS regions typically follow the pattern `{geo}-{location}-{number}` (e.g., `us-east-1`, `eu-west-2`),
/// but there are special cases like `us-gov-west-1` and `cn-north-1`.
///
/// This type validates that the region string follows a reasonable format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Region(String);

/// Error type for invalid region strings.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Invalid region format")]
pub struct RegionError {
    /// priv place holder
    _priv: (),
}

impl Region {
    /// Creates a new `Region` from a string, validating the format.
    ///
    /// # Errors
    /// Returns `RegionError` if the region string doesn't match expected patterns.
    ///
    /// # Examples
    /// ```
    /// # use s3s::region::Region;
    /// let region = Region::new("us-east-1").unwrap();
    /// assert_eq!(region.as_str(), "us-east-1");
    ///
    /// let region = Region::new("eu-west-2").unwrap();
    /// assert_eq!(region.as_str(), "eu-west-2");
    ///
    /// // Invalid format
    /// assert!(Region::new("invalid_region").is_err());
    /// ```
    pub fn new(s: &str) -> Result<Self, RegionError> {
        if is_valid_region(s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(RegionError { _priv: () })
        }
    }

    /// Creates a new `Region` without validation.
    ///
    /// # Safety
    /// The caller must ensure the string is a valid region identifier.
    /// This is useful when the region comes from a trusted source that has already been validated.
    #[must_use]
    pub fn new_unchecked(s: String) -> Self {
        Self(s)
    }

    /// Returns the region as a string slice.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the `Region` and returns the inner `String`.
    #[inline]
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for Region {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates an S3 region string.
///
/// This checks for:
/// - Standard AWS regions: `{geo}-{location}-{number}` (e.g., `us-east-1`)
/// - Government regions: `us-gov-{location}-{number}`
/// - China regions: `cn-{location}-{number}`
/// - Custom/local regions that follow similar patterns
///
/// # Examples
/// ```
/// # use s3s::region::is_valid_region;
/// assert!(is_valid_region("us-east-1"));
/// assert!(is_valid_region("eu-west-2"));
/// assert!(is_valid_region("us-gov-west-1"));
/// assert!(is_valid_region("cn-north-1"));
/// assert!(is_valid_region("local"));
/// assert!(!is_valid_region(""));
/// assert!(!is_valid_region("invalid region"));
/// ```
#[must_use]
pub fn is_valid_region(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    // Allow single-word regions like "local" or "us-east-1" or "us-gov-west-1"
    // Check that it only contains lowercase letters, numbers, and hyphens
    // Must start with a letter
    let bytes = s.as_bytes();

    if !bytes[0].is_ascii_lowercase() {
        return false;
    }

    for &b in bytes {
        if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' {
            return false;
        }
    }

    // Must not start or end with a hyphen
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }

    // Must not have consecutive hyphens
    if s.contains("--") {
        return false;
    }

    true
}

/// Attempts to extract a region from an S3 host header.
///
/// This supports several S3 endpoint patterns:
/// - Virtual-hosted-style: `bucket.s3.{region}.amazonaws.com` or `bucket.s3-{region}.amazonaws.com`
/// - Path-style: `s3.{region}.amazonaws.com` or `s3-{region}.amazonaws.com`
/// - Custom domains may not contain region information
///
/// # Examples
/// ```
/// # use s3s::region::extract_region_from_host;
/// assert_eq!(extract_region_from_host("s3.us-east-1.amazonaws.com"), Some("us-east-1"));
/// assert_eq!(extract_region_from_host("s3-us-west-2.amazonaws.com"), Some("us-west-2"));
/// assert_eq!(extract_region_from_host("bucket.s3.eu-west-1.amazonaws.com"), Some("eu-west-1"));
/// assert_eq!(extract_region_from_host("bucket.s3-ap-south-1.amazonaws.com"), Some("ap-south-1"));
/// assert_eq!(extract_region_from_host("s3.amazonaws.com"), None);
/// assert_eq!(extract_region_from_host("example.com"), None);
/// ```
#[must_use]
pub fn extract_region_from_host(host: &str) -> Option<&str> {
    // Remove port if present
    let host = host.split(':').next()?;

    // Pattern: s3.{region}.amazonaws.com or bucket.s3.{region}.amazonaws.com
    if let Some(rest) = host.strip_suffix(".amazonaws.com") {
        // Split by dots and look for s3 component
        let parts: Vec<&str> = rest.split('.').collect();

        // Try to find region after s3
        for (i, &part) in parts.iter().enumerate() {
            if part == "s3" && i + 1 < parts.len() {
                let potential_region = parts[i + 1];
                if is_valid_region(potential_region) {
                    return Some(potential_region);
                }
            }
        }
    }

    // Pattern: s3-{region}.amazonaws.com or bucket.s3-{region}.amazonaws.com
    if let Some(rest) = host.strip_suffix(".amazonaws.com") {
        let parts: Vec<&str> = rest.split('.').collect();

        for part in parts {
            if let Some(region) = part.strip_prefix("s3-") {
                if is_valid_region(region) {
                    return Some(region);
                }
            }
        }
    }

    None
}

/// A borrowed or owned region string.
///
/// This is useful for APIs that may need to store or pass around region information
/// without requiring ownership.
pub type RegionRef<'a> = Cow<'a, str>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_new() {
        assert!(Region::new("us-east-1").is_ok());
        assert!(Region::new("eu-west-2").is_ok());
        assert!(Region::new("ap-south-1").is_ok());
        assert!(Region::new("us-gov-west-1").is_ok());
        assert!(Region::new("cn-north-1").is_ok());
        assert!(Region::new("local").is_ok());

        assert!(Region::new("").is_err());
        assert!(Region::new("invalid region").is_err());
        assert!(Region::new("-us-east-1").is_err());
        assert!(Region::new("us-east-1-").is_err());
        assert!(Region::new("us--east-1").is_err());
        assert!(Region::new("Us-East-1").is_err());
        assert!(Region::new("1us-east").is_err());
    }

    #[test]
    fn test_region_as_str() {
        let region = Region::new("us-east-1").unwrap();
        assert_eq!(region.as_str(), "us-east-1");
    }

    #[test]
    fn test_region_into_string() {
        let region = Region::new("eu-west-2").unwrap();
        assert_eq!(region.into_string(), "eu-west-2");
    }

    #[test]
    fn test_is_valid_region() {
        // Valid regions
        assert!(is_valid_region("us-east-1"));
        assert!(is_valid_region("eu-west-2"));
        assert!(is_valid_region("ap-south-1"));
        assert!(is_valid_region("us-gov-west-1"));
        assert!(is_valid_region("cn-north-1"));
        assert!(is_valid_region("local"));
        assert!(is_valid_region("my-custom-region"));

        // Invalid regions
        assert!(!is_valid_region(""));
        assert!(!is_valid_region("invalid region"));
        assert!(!is_valid_region("-us-east-1"));
        assert!(!is_valid_region("us-east-1-"));
        assert!(!is_valid_region("us--east-1"));
        assert!(!is_valid_region("Us-East-1"));
        assert!(!is_valid_region("1us-east"));
        assert!(!is_valid_region("us_east_1"));
    }

    #[test]
    fn test_extract_region_from_host() {
        // Virtual-hosted-style with dot notation
        assert_eq!(extract_region_from_host("s3.us-east-1.amazonaws.com"), Some("us-east-1"));
        assert_eq!(extract_region_from_host("s3.eu-west-2.amazonaws.com"), Some("eu-west-2"));
        assert_eq!(
            extract_region_from_host("bucket.s3.us-west-1.amazonaws.com"),
            Some("us-west-1")
        );
        assert_eq!(
            extract_region_from_host("my-bucket.s3.ap-south-1.amazonaws.com"),
            Some("ap-south-1")
        );

        // Virtual-hosted-style with dash notation
        assert_eq!(extract_region_from_host("s3-us-west-2.amazonaws.com"), Some("us-west-2"));
        assert_eq!(extract_region_from_host("s3-eu-central-1.amazonaws.com"), Some("eu-central-1"));
        assert_eq!(
            extract_region_from_host("bucket.s3-ap-northeast-1.amazonaws.com"),
            Some("ap-northeast-1")
        );

        // With port
        assert_eq!(extract_region_from_host("s3.us-east-1.amazonaws.com:443"), Some("us-east-1"));

        // No region in host
        assert_eq!(extract_region_from_host("s3.amazonaws.com"), None);
        assert_eq!(extract_region_from_host("example.com"), None);
        assert_eq!(extract_region_from_host("localhost"), None);
        assert_eq!(extract_region_from_host("localhost:9000"), None);
    }

    #[test]
    fn test_region_display() {
        let region = Region::new("us-east-1").unwrap();
        assert_eq!(format!("{region}"), "us-east-1");
    }

    #[test]
    fn test_region_as_ref() {
        let region = Region::new("us-east-1").unwrap();
        let s: &str = region.as_ref();
        assert_eq!(s, "us-east-1");
    }
}

//! Integration tests for S3 region handling

use s3s::host::{MultiDomain, S3Host, SingleDomain};
use s3s::region::{extract_region_from_host, is_valid_region, Region};

#[test]
fn test_region_type_validation() {
    // Valid regions
    assert!(Region::new("us-east-1").is_ok());
    assert!(Region::new("eu-west-2").is_ok());
    assert!(Region::new("ap-south-1").is_ok());
    assert!(Region::new("us-gov-west-1").is_ok());
    assert!(Region::new("cn-north-1").is_ok());
    assert!(Region::new("local").is_ok());

    // Invalid regions
    assert!(Region::new("").is_err());
    assert!(Region::new("invalid region").is_err());
    assert!(Region::new("Us-East-1").is_err());
}

#[test]
fn test_region_validation_function() {
    assert!(is_valid_region("us-east-1"));
    assert!(is_valid_region("eu-west-2"));
    assert!(is_valid_region("local"));

    assert!(!is_valid_region(""));
    assert!(!is_valid_region("invalid region"));
    assert!(!is_valid_region("Us-East-1"));
}

#[test]
fn test_extract_region_from_host_aws_patterns() {
    // AWS standard patterns
    assert_eq!(extract_region_from_host("s3.us-east-1.amazonaws.com"), Some("us-east-1"));
    assert_eq!(extract_region_from_host("s3.eu-west-2.amazonaws.com"), Some("eu-west-2"));

    // AWS with bucket
    assert_eq!(
        extract_region_from_host("my-bucket.s3.us-west-1.amazonaws.com"),
        Some("us-west-1")
    );
    assert_eq!(
        extract_region_from_host("test.s3.ap-south-1.amazonaws.com"),
        Some("ap-south-1")
    );

    // AWS dash notation
    assert_eq!(extract_region_from_host("s3-us-west-2.amazonaws.com"), Some("us-west-2"));
    assert_eq!(
        extract_region_from_host("bucket.s3-eu-central-1.amazonaws.com"),
        Some("eu-central-1")
    );

    // With port
    assert_eq!(extract_region_from_host("s3.us-east-1.amazonaws.com:443"), Some("us-east-1"));

    // No region
    assert_eq!(extract_region_from_host("s3.amazonaws.com"), None);
    assert_eq!(extract_region_from_host("example.com"), None);
    assert_eq!(extract_region_from_host("localhost:9000"), None);
}

#[test]
fn test_virtual_host_region_extraction() {
    let domain = SingleDomain::new("example.com").unwrap();

    // Standard host with AWS pattern
    let vh = domain.parse_host_header("s3.us-east-1.amazonaws.com").unwrap();
    assert_eq!(vh.region.as_deref(), Some("us-east-1"));

    // Standard host without region
    let vh = domain.parse_host_header("example.com").unwrap();
    assert_eq!(vh.region.as_deref(), None);

    // Bucket subdomain with region
    let vh = domain.parse_host_header("bucket.s3.eu-west-2.amazonaws.com").unwrap();
    assert_eq!(vh.region.as_deref(), Some("eu-west-2"));
}

#[test]
fn test_multi_domain_region_extraction() {
    let domains = MultiDomain::new(["example.com", "test.org"]).unwrap();

    // AWS pattern with region
    let vh = domains.parse_host_header("s3.us-west-1.amazonaws.com").unwrap();
    assert_eq!(vh.region.as_deref(), Some("us-west-1"));

    // Custom domain without region
    let vh = domains.parse_host_header("example.com").unwrap();
    assert_eq!(vh.region.as_deref(), None);
}

#[test]
fn test_region_type_methods() {
    let region = Region::new("us-east-1").unwrap();

    // Test as_str
    assert_eq!(region.as_str(), "us-east-1");

    // Test AsRef
    let s: &str = region.as_ref();
    assert_eq!(s, "us-east-1");

    // Test Display
    assert_eq!(format!("{}", region), "us-east-1");

    // Test into_string
    let region_str = region.into_string();
    assert_eq!(region_str, "us-east-1");
}

#[test]
fn test_region_unchecked_creation() {
    // This should only be used with trusted input
    let region = Region::new_unchecked("us-east-1".to_string());
    assert_eq!(region.as_str(), "us-east-1");
}

#[test]
fn test_region_edge_cases() {
    // Single word region
    assert!(Region::new("local").is_ok());

    // Gov regions
    assert!(Region::new("us-gov-west-1").is_ok());
    assert!(Region::new("us-gov-east-1").is_ok());

    // China regions
    assert!(Region::new("cn-north-1").is_ok());
    assert!(Region::new("cn-northwest-1").is_ok());

    // Invalid cases
    assert!(Region::new("region-").is_err());
    assert!(Region::new("-region").is_err());
    assert!(Region::new("region--name").is_err());
    assert!(Region::new("UPPERCASE").is_err());
    assert!(Region::new("contains space").is_err());
    assert!(Region::new("has_underscore").is_err());
}

#[test]
fn test_virtual_host_with_region() {
    use s3s::host::VirtualHost;

    // Create a VirtualHost and set a region
    let vh = VirtualHost::new("example.com").with_region("us-east-1");
    assert_eq!(vh.domain(), "example.com");
    assert_eq!(vh.bucket(), None);
    assert_eq!(vh.region.as_deref(), Some("us-east-1"));

    // Create a VirtualHost with bucket and region
    let vh = VirtualHost::with_bucket("example.com", "my-bucket")
        .with_region("eu-west-2");
    assert_eq!(vh.domain(), "example.com");
    assert_eq!(vh.bucket(), Some("my-bucket"));
    assert_eq!(vh.region.as_deref(), Some("eu-west-2"));
}

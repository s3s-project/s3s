// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Virtual-host parsing for S3 request routing.
//!
//! This module provides the [`S3Host`] trait together with the built-in
//! implementations [`SingleDomain`] and [`MultiDomain`]. They parse the HTTP
//! `Host` header into a [`VirtualHost`] value that carries the base domain,
//! the bucket name (when the request uses virtual-hosted-style addressing),
//! and an optional region.
//!
//! # Built-in implementations
//!
//! - [`SingleDomain`] keeps the traditional behaviour: any unrecognized
//!   valid host becomes the bucket name (CNAME-style). The fallback can be
//!   disabled with [`SingleDomain::with_cname_fallback`].
//! - [`MultiDomain`] additionally restricts the fallback to hosts that pass
//!   bucket-name validation and allows narrowing it with
//!   [`MultiDomain::with_path_style_hosts`].
//!
//! # CNAME-style fallback ([`MultiDomain`] only)
//!
//! A host that matches no configured base domain can still be handled in two
//! ways, mirroring AWS S3:
//!
//! - If the host could itself be a bucket name (e.g. `my-bucket.com`,
//!   `localhost`), the whole host becomes the bucket name. This supports
//!   [CNAME-style virtual hosting](https://docs.aws.amazon.com/AmazonS3/latest/userguide/VirtualHosting.html#VirtualHostingCustomURLs),
//!   which is a legitimate AWS usage pattern. Each such request is recorded
//!   at `debug!` level for diagnostics.
//! - Otherwise (e.g. `localhost:8014` — a host with a port can never be a
//!   CNAME'd bucket), the host is returned without a bucket and the request
//!   is parsed as path-style instead of failing with `InvalidBucketName`.
//!
//! Hosts that are not valid domain names at all still yield
//! `InvalidRequest`.
//!
//! Hosts that should always be parsed as path-style (e.g. `localhost` when
//! the service is reached directly, outside any reverse proxy) can be
//! selected with [`MultiDomain::with_path_style_hosts`], which takes a
//! [`regex::RegexSet`] matched against the full `Host` header
//! (port included). Note: once a [`MultiDomain`] is configured, path-style
//! fallback happens only for hosts that match the rule above or fail the
//! bucket-name check; see
//! [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
#![deny(missing_docs)]

use crate::error::S3Result;
use crate::path::check_bucket_name;

use regex::RegexSet;
use std::borrow::Cow;

use stdx::default::default;
use tracing::debug;

/// The parsed result of an HTTP `Host` header.
///
/// Carries the base domain, the bucket name (when the request uses
/// virtual-hosted-style addressing), and an optional region.
#[derive(Debug, Clone)]
pub struct VirtualHost<'a> {
    domain: Cow<'a, str>,
    bucket: Option<Cow<'a, str>>,
    region: Option<Cow<'a, str>>,
}

impl<'a> VirtualHost<'a> {
    /// Creates a new [`VirtualHost`] for the given domain.
    ///
    /// The bucket and region are left unset; use
    /// [`VirtualHost::with_bucket`] and [`VirtualHost::with_region`] to set
    /// them.
    pub fn new(domain: impl Into<Cow<'a, str>>) -> Self {
        Self {
            domain: domain.into(),
            bucket: None,
            region: None,
        }
    }

    /// Sets the bucket name for this virtual host.
    ///
    /// This method follows the builder pattern and returns `self` for method chaining.
    ///
    /// # Examples
    ///
    /// ```
    /// use s3s::host::VirtualHost;
    ///
    /// let vh = VirtualHost::new("example.com")
    ///     .with_bucket("my-bucket");
    ///
    /// assert_eq!(vh.bucket(), Some("my-bucket"));
    /// ```
    #[must_use]
    pub fn with_bucket(mut self, bucket: impl Into<Cow<'a, str>>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    /// Sets the AWS region for this virtual host.
    ///
    /// This method follows the builder pattern and returns `self` for method chaining.
    /// The region represents the AWS region where the S3 bucket is located.
    ///
    /// # Examples
    ///
    /// ```
    /// use s3s::host::VirtualHost;
    ///
    /// let vh = VirtualHost::new("example.com")
    ///     .with_bucket("my-bucket")
    ///     .with_region("us-west-2");
    ///
    /// assert_eq!(vh.region(), Some("us-west-2"));
    /// ```
    #[must_use]
    pub fn with_region(mut self, region: impl Into<Cow<'a, str>>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Returns the base domain of the virtual host.
    #[inline]
    #[must_use]
    pub fn domain(&self) -> &str {
        self.domain.as_ref()
    }

    /// Returns the bucket name, if the request used virtual-hosted-style
    /// addressing.
    ///
    /// When unset, the caller parses the request as path-style.
    #[inline]
    #[must_use]
    pub fn bucket(&self) -> Option<&str> {
        self.bucket.as_deref()
    }

    /// Returns the AWS region associated with this virtual host, if set.
    ///
    /// # Returns
    ///
    /// - `Some(&str)` - The region name if it was set using `with_region()`
    /// - `None` - If no region was specified
    #[inline]
    #[must_use]
    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }
}

/// A parser that turns the HTTP `Host` header into a [`VirtualHost`].
///
/// See the [module-level docs](self) for the behaviour of the built-in
/// implementations.
pub trait S3Host: Send + Sync + 'static {
    /// Parses the `Host` header of the HTTP request.
    ///
    /// # Errors
    /// Returns an error if the `Host` is invalid for this service.
    ///
    /// The returned [`VirtualHost`] may leave the bucket unset; the caller
    /// then parses the request as path-style. Whether a host is left without
    /// a bucket depends on the implementation — see the
    /// [module-level docs](self) for the exact fallback behaviour.
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>>;
}

/// Errors returned when constructing the built-in [`S3Host`] implementations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// The domain string is not a valid domain.
    #[error("The domain is invalid")]
    InvalidDomain,

    /// Two configured domains overlap (one is a subdomain of the other).
    #[error("Some subdomains overlap with each other")]
    OverlappingSubdomains,

    /// No base domain was provided.
    #[error("No base domains are specified")]
    ZeroDomains,
}

/// Naive check for a valid domain.
fn is_valid_domain(mut s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    if let Some((host, port)) = s.split_once(':') {
        if port.is_empty() {
            return false;
        }

        if port.parse::<u16>().is_err() {
            return false;
        }

        s = host;
    }

    for part in s.split('.') {
        if part.is_empty() {
            return false;
        }

        if part.as_bytes().iter().any(|&b| !b.is_ascii_alphanumeric() && b != b'-') {
            return false;
        }
    }

    true
}

/// Checks whether two base domains would compete for the same host header.
///
/// Two domains overlap when they are equal, or when one is a DNS subdomain of the other.
/// The suffix must end on a label boundary, matching [`parse_host_header`]: `s3.example.com`
/// is a subdomain of `example.com`, while `s3-example.com` is an unrelated domain.
fn is_overlapping(a: &str, b: &str) -> bool {
    a == b || is_subdomain_of(a, b) || is_subdomain_of(b, a)
}

/// Checks whether `host` is a strict DNS subdomain of `base_domain`.
fn is_subdomain_of(host: &str, base_domain: &str) -> bool {
    host.strip_suffix(base_domain).is_some_and(|rest| rest.ends_with('.'))
}

fn parse_host_header<'a>(base_domain: &'a str, host: &'a str) -> Option<VirtualHost<'a>> {
    if host == base_domain {
        return Some(VirtualHost::new(base_domain));
    }

    if let Some(bucket) = host.strip_suffix(base_domain).and_then(|h| h.strip_suffix('.')) {
        return Some(VirtualHost::new(base_domain).with_bucket(bucket));
    }

    None
}

/// CNAME-style fallback for a host that matches no configured base domain.
///
/// Returns `None` when the host is not a valid domain at all (the caller
/// then reports `InvalidRequest`). Otherwise:
///
/// - If the host matches `path_style_hosts`, it is returned without a bucket
///   so the request is parsed as path-style.
/// - Otherwise, the lowercased host becomes the bucket name if it passes
///   bucket-name validation (CNAME-style addressing). This is a legitimate
///   AWS usage pattern and is recorded at `debug!` level.
/// - Otherwise (e.g. a host carrying a port such as `localhost:8014`), the
///   host is returned without a bucket so the request is parsed as
///   path-style.
///
/// See the [module-level docs](self) and
/// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
fn parse_cname_fallback<'a>(host: &'a str, path_style_hosts: &RegexSet) -> Option<VirtualHost<'a>> {
    if !is_valid_domain(host) {
        return None;
    }

    if path_style_hosts.is_match(host) {
        return Some(VirtualHost::new(host));
    }

    let bucket = host.to_ascii_lowercase();
    if check_bucket_name(&bucket) {
        debug!(?host, "host matches no configured base domain; treating it as a CNAME-style bucket");
        return Some(VirtualHost::new(host).with_bucket(bucket));
    }

    Some(VirtualHost::new(host))
}

/// A host parser with a single base domain.
///
/// Unrecognized hosts are handled by the CNAME-style fallback described in
/// the [module-level docs](self): any valid host becomes the bucket name.
/// The fallback can be disabled with [`SingleDomain::with_cname_fallback`],
/// in which case unrecognized hosts are always parsed as path-style.
#[derive(Debug)]
pub struct SingleDomain {
    base_domain: String,
    cname_fallback: bool,
}

impl SingleDomain {
    /// Create a new `SingleDomain` with the base domain.
    ///
    /// # Errors
    /// Returns an error if the base domain is invalid.
    pub fn new(base_domain: &str) -> Result<Self, DomainError> {
        if !is_valid_domain(base_domain) {
            return Err(DomainError::InvalidDomain);
        }

        Ok(Self {
            base_domain: base_domain.into(),
            cname_fallback: true,
        })
    }

    /// Controls the CNAME-style fallback for hosts outside the base domain.
    ///
    /// When disabled, a host that matches neither the base domain nor a
    /// subdomain is parsed as path-style instead of being treated as a
    /// bucket name. See
    /// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
    ///
    /// Default: enabled
    #[must_use]
    pub fn with_cname_fallback(mut self, enabled: bool) -> Self {
        self.cname_fallback = enabled;
        self
    }
}

impl S3Host for SingleDomain {
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>> {
        let base_domain = self.base_domain.as_str();

        if let Some(vh) = parse_host_header(base_domain, host) {
            return Ok(vh);
        }

        if is_valid_domain(host) {
            if self.cname_fallback {
                let bucket = host.to_ascii_lowercase();
                return Ok(VirtualHost::new(host).with_bucket(bucket));
            }
            return Ok(VirtualHost::new(host));
        }

        Err(s3_error!(InvalidRequest, "Invalid host header"))
    }
}

/// A host parser with multiple base domains.
///
/// Hosts outside the configured domains are handled by the CNAME-style
/// fallback described in the [module-level docs](self). Hosts matching
/// [`MultiDomain::with_path_style_hosts`] are always parsed as path-style.
#[derive(Debug)]
pub struct MultiDomain {
    base_domains: Vec<String>,
    path_style_hosts: RegexSet,
}

impl MultiDomain {
    /// Create a new `MultiDomain` with the base domains.
    ///
    /// # Errors
    /// Returns an error if
    /// + any of the base domains are invalid.
    /// + any of the base domains overlap with each other.
    /// + no base domains are specified.
    pub fn new<I>(base_domains: I) -> Result<Self, DomainError>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut v: Vec<String> = default();

        for domain in base_domains {
            let domain = domain.as_ref();

            if !is_valid_domain(domain) {
                return Err(DomainError::InvalidDomain);
            }

            for other in &v {
                if is_overlapping(domain, other) {
                    return Err(DomainError::OverlappingSubdomains);
                }
            }

            v.push(domain.to_owned());
        }

        if v.is_empty() {
            return Err(DomainError::ZeroDomains);
        }

        Ok(Self {
            base_domains: v,
            path_style_hosts: RegexSet::empty(),
        })
    }

    /// Sets the hosts that are always parsed as path-style.
    ///
    /// Hosts that match the [`regex::RegexSet`] are returned
    /// without a bucket instead of being treated as CNAME-style bucket names.
    /// The patterns are matched against the full `Host` header, port
    /// included. See
    /// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
    ///
    /// Default: no hosts
    #[must_use]
    pub fn with_path_style_hosts(mut self, path_style_hosts: RegexSet) -> Self {
        self.path_style_hosts = path_style_hosts;
        self
    }
}

impl S3Host for MultiDomain {
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>> {
        for base_domain in &self.base_domains {
            if let Some(vh) = parse_host_header(base_domain, host) {
                return Ok(vh);
            }
        }

        if let Some(vh) = parse_cname_fallback(host, &self.path_style_hosts) {
            return Ok(vh);
        }

        Err(s3_error!(InvalidRequest, "Invalid host header"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::S3ErrorCode;

    #[test]
    fn single_domain_new() {
        let domain = "example.com";
        let result = SingleDomain::new(domain);
        let sd = result.unwrap();
        assert_eq!(sd.base_domain, domain);

        let domain = "example.com.org";
        let result = SingleDomain::new(domain);
        let sd = result.unwrap();
        assert_eq!(sd.base_domain, domain);

        for domain in [
            "",                  // empty input
            "example.com.",      // empty label
            "example.com:",      // empty port
            "example.com:http",  // non-numeric port
            "example.com:65536", // port above u16::MAX
            "exa_mple.com",      // character outside [a-zA-Z0-9-]
        ] {
            let err = SingleDomain::new(domain).unwrap_err();
            assert_eq!(err, DomainError::InvalidDomain, "{domain:?}");
        }

        let domain = "example.com:80";
        let result = SingleDomain::new(domain);
        assert!(result.is_ok());
    }

    #[test]
    fn multi_domain_new() {
        let domains = ["example.com", "example.org"];
        let result = MultiDomain::new(&domains);
        let md = result.unwrap();
        assert_eq!(md.base_domains, domains);

        let domains = ["example.com", "example.com"];
        let err = MultiDomain::new(&domains).unwrap_err();
        assert_eq!(err, DomainError::OverlappingSubdomains);

        let domains = ["example.com", "example.com.org"];
        let result = MultiDomain::new(&domains);
        let md = result.unwrap();
        assert_eq!(md.base_domains, domains);

        // a real subdomain is ambiguous and stays rejected, in either order
        for domains in [
            ["example.com", "s3.example.com"],
            ["s3.example.com", "example.com"],
            ["example.com:8080", "s3.example.com:8080"],
        ] {
            let err = MultiDomain::new(&domains).unwrap_err();
            assert_eq!(err, DomainError::OverlappingSubdomains, "{domains:?}");
        }

        // a shared suffix without a label boundary is not an overlap
        for domains in [
            ["example.com", "s3-example.com"],
            ["rustfs.example.com", "s3-rustfs.example.com"],
            ["example.com:8080", "s3-example.com:8080"],
        ] {
            let md = MultiDomain::new(&domains).unwrap();
            assert_eq!(md.base_domains, domains, "{domains:?}");
        }

        // an invalid domain is rejected wherever it appears in the list
        for domains in [["", "example.com"], ["example.com", "exa_mple.com"]] {
            let err = MultiDomain::new(&domains).unwrap_err();
            assert_eq!(err, DomainError::InvalidDomain, "{domains:?}");
        }

        let domains: [&str; 0] = [];
        let err = MultiDomain::new(&domains).unwrap_err();
        assert_eq!(err, DomainError::ZeroDomains);
    }

    #[test]
    fn multi_domain_parse_shared_suffix() {
        let domains = ["rustfs.example.com", "s3-rustfs.example.com"];
        let md = MultiDomain::new(domains.iter().copied()).unwrap();

        for domain in domains {
            let vh = md.parse_host_header(domain).unwrap();
            assert_eq!(vh.domain(), domain);
            assert_eq!(vh.bucket(), None);

            let host = format!("bucket.{domain}");
            let vh = md.parse_host_header(&host).unwrap();
            assert_eq!(vh.domain(), domain);
            assert_eq!(vh.bucket(), Some("bucket"));
        }
    }

    #[test]
    fn multi_domain_parse() {
        let domains = ["example.com", "example.org"];
        let md = MultiDomain::new(domains.iter().copied()).unwrap();

        let host = "example.com";
        let result = md.parse_host_header(host);
        let vh = result.unwrap();
        assert_eq!(vh.domain(), host);
        assert_eq!(vh.bucket(), None);

        let host = "example.org";
        let result = md.parse_host_header(host);
        let vh = result.unwrap();
        assert_eq!(vh.domain(), host);
        assert_eq!(vh.bucket(), None);

        let host = "example.com.org";
        let result = md.parse_host_header(host);
        let vh = result.unwrap();
        assert_eq!(vh.domain(), host);
        assert_eq!(vh.bucket(), Some("example.com.org"));

        let host = "example.com.org.";
        let result = md.parse_host_header(host);
        let err = result.unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);

        let host = "example.com.org.example.com";
        let result = md.parse_host_header(host);
        let vh = result.unwrap();
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), Some("example.com.org"));
    }

    #[test]
    fn single_domain_parse_cname_fallback() {
        let sd = SingleDomain::new("s3.example.com").unwrap();

        // SingleDomain keeps the traditional behaviour: any unrecognized
        // valid host becomes the bucket name, even with a port.
        let vh = sd.parse_host_header("localhost").unwrap();
        assert_eq!(vh.domain(), "localhost");
        assert_eq!(vh.bucket(), Some("localhost"));

        let vh = sd.parse_host_header("localhost:8014").unwrap();
        assert_eq!(vh.domain(), "localhost:8014");
        assert_eq!(vh.bucket(), Some("localhost:8014"));

        // Invalid domain names still error.
        let err = sd.parse_host_header("example.com.").unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn single_domain_disable_cname_fallback() {
        let sd = SingleDomain::new("s3.example.com").unwrap().with_cname_fallback(false);

        // With the fallback disabled, unrecognized valid hosts are parsed
        // as path-style, even when they are valid bucket names.
        let vh = sd.parse_host_header("localhost").unwrap();
        assert_eq!(vh.domain(), "localhost");
        assert_eq!(vh.bucket(), None);

        let vh = sd.parse_host_header("localhost:8014").unwrap();
        assert_eq!(vh.domain(), "localhost:8014");
        assert_eq!(vh.bucket(), None);

        let vh = sd.parse_host_header("cdn.example.org").unwrap();
        assert_eq!(vh.domain(), "cdn.example.org");
        assert_eq!(vh.bucket(), None);

        // Invalid domain names still error.
        let err = sd.parse_host_header("example.com.").unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn multi_domain_parse_cname_fallback() {
        let domains = ["s3.example.com", "s3.example.org"];
        let md = MultiDomain::new(domains.iter().copied()).unwrap();

        // MultiDomain keeps the CNAME-style fallback.
        let vh = md.parse_host_header("localhost").unwrap();
        assert_eq!(vh.domain(), "localhost");
        assert_eq!(vh.bucket(), Some("localhost"));

        let vh = md.parse_host_header("localhost:8014").unwrap();
        assert_eq!(vh.domain(), "localhost:8014");
        assert_eq!(vh.bucket(), None);

        let err = md.parse_host_header("example.com.").unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn multi_domain_path_style_hosts() {
        let domains = ["s3.example.com", "s3.example.org"];
        let path_style_hosts = RegexSet::new([r"^localhost$", r"^localhost:\d+$"]).unwrap();
        let md = MultiDomain::new(domains.iter().copied())
            .unwrap()
            .with_path_style_hosts(path_style_hosts);

        // Hosts matching the set are parsed as path-style...
        let vh = md.parse_host_header("localhost").unwrap();
        assert_eq!(vh.domain(), "localhost");
        assert_eq!(vh.bucket(), None);

        let vh = md.parse_host_header("localhost:8014").unwrap();
        assert_eq!(vh.domain(), "localhost:8014");
        assert_eq!(vh.bucket(), None);

        // ...while other hosts keep the CNAME-style fallback.
        let vh = md.parse_host_header("cdn.example.org").unwrap();
        assert_eq!(vh.domain(), "cdn.example.org");
        assert_eq!(vh.bucket(), Some("cdn.example.org"));

        let err = md.parse_host_header("example.com.").unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn virtual_host_builder() {
        // Test basic construction
        let vh = VirtualHost::new("example.com");
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), None);
        assert_eq!(vh.region(), None);

        // Test with_bucket builder
        let vh = VirtualHost::new("example.com").with_bucket("my-bucket");
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), Some("my-bucket"));
        assert_eq!(vh.region(), None);

        // Test with_region builder
        let vh = VirtualHost::new("example.com").with_region("us-west-2");
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), None);
        assert_eq!(vh.region(), Some("us-west-2"));

        // Test chaining with_bucket and with_region
        let vh = VirtualHost::new("example.com")
            .with_bucket("my-bucket")
            .with_region("us-east-1");
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), Some("my-bucket"));
        assert_eq!(vh.region(), Some("us-east-1"));

        // Test chaining with_region and with_bucket (reversed order)
        let vh = VirtualHost::new("example.com")
            .with_region("eu-west-1")
            .with_bucket("another-bucket");
        assert_eq!(vh.domain(), "example.com");
        assert_eq!(vh.bucket(), Some("another-bucket"));
        assert_eq!(vh.region(), Some("eu-west-1"));
    }
}

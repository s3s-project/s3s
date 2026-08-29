// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! S3 Service Configuration
//!
//! This module provides configurable parameters for the S3 service.
//!
//! # Features
//! - `serde` support for serialization/deserialization
//! - Default values for all parameters
//! - Configuration values via [`S3Config`]
//! - Static configuration via [`StaticConfigProvider`]
//! - Hot-reload configuration via [`HotReloadConfigProvider`]
//!
//! # Example
//! ```
//! use std::sync::Arc;
//! use s3s::config::{S3Config, S3ConfigProvider, StaticConfigProvider, HotReloadConfigProvider};
//!
//! // Using default config values
//! let config = S3Config::default();
//!
//! // Using custom config values
//! let mut config = S3Config::default();
//! config.xml_max_body_size = 10 * 1024 * 1024;
//! config.put_object_max_size = Some(5 * 1024 * 1024 * 1024);
//!
//! // Using static config provider (immutable)
//! let static_provider = Arc::new(StaticConfigProvider::new(Arc::new(config.clone())));
//! let snapshot = static_provider.snapshot();
//! assert_eq!(snapshot.xml_max_body_size, 10 * 1024 * 1024);
//! assert_eq!(snapshot.put_object_max_size, Some(5 * 1024 * 1024 * 1024));
//!
//! // Using hot-reload config provider (can be updated at runtime)
//! let hot_reload_provider = Arc::new(HotReloadConfigProvider::default());
//! let snapshot = hot_reload_provider.snapshot();
//! assert_eq!(snapshot.xml_max_body_size, 20 * 1024 * 1024);
//!
//! // Update configuration at runtime
//! let mut new_config = S3Config::default();
//! new_config.xml_max_body_size = 10 * 1024 * 1024;
//! hot_reload_provider.update(Arc::new(new_config));
//! assert_eq!(hot_reload_provider.snapshot().xml_max_body_size, 10 * 1024 * 1024);
//! ```

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use crate::region::Region;

// AWS-compatible default: https://docs.aws.amazon.com/AmazonS3/latest/userguide/using-presigned-url.html#PresignedUrl-Expiration
pub(crate) const DEFAULT_PRESIGNED_URL_MAX_EXPIRES_SECS: u32 = 7 * 24 * 60 * 60;

// Aligned with MinIO: https://github.com/minio/minio/blob/master/cmd/streaming-signature-v4.go
pub(crate) const DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE: usize = 256 * 1024 * 1024;

const DEFAULT_SIG_V4_ALLOWED_SERVICES: &[&str] = &["s3", "sts"];

fn default_sig_v4_allowed_services() -> Vec<String> {
    DEFAULT_SIG_V4_ALLOWED_SERVICES.iter().map(ToString::to_string).collect()
}

fn is_default_sig_v4_allowed_services(services: &[String]) -> bool {
    services
        .iter()
        .map(String::as_str)
        .eq(DEFAULT_SIG_V4_ALLOWED_SERVICES.iter().copied())
}

/// S3 Service Configuration Provider trait.
///
/// This trait provides a `snapshot` method that returns an `Arc<S3Config>`.
/// This design allows for faster access and consistent reads across multiple
/// config values.
///
/// Built-in providers:
/// - [`StaticConfigProvider`] - Immutable configuration (default if not set)
/// - [`HotReloadConfigProvider`] - Runtime-updatable configuration
pub trait S3ConfigProvider: Send + Sync + 'static {
    /// Returns a snapshot of the current configuration.
    ///
    /// This operation returns an `Arc<S3Config>` that provides consistent
    /// access to all configuration values. The snapshot is immutable and will
    /// not change even if the underlying configuration is updated.
    fn snapshot(&self) -> Arc<S3Config>;
}

/// S3 Service Configuration.
///
/// Contains configurable parameters for the S3 service with sensible defaults.
/// The configuration is immutable after creation.
///
/// Streaming uploads such as `PUT Object` and `UploadPart` are not capped by
/// default. When [`S3Config::put_object_max_size`] is unset, the
/// [`S3`](crate::S3) implementation is responsible for enforcing object-size
/// limits for those streams. Set it only when the adapter should enforce an
/// opt-in deployment hardening limit before handing the stream to the
/// implementation.
///
/// Use with [`StaticConfigProvider`] or [`HotReloadConfigProvider`].
///
/// # Example
/// ```
/// use std::sync::Arc;
/// use s3s::config::{S3Config, S3ConfigProvider, StaticConfigProvider, HotReloadConfigProvider};
///
/// let mut config = S3Config::default();
/// config.xml_max_body_size = 10 * 1024 * 1024;
/// config.put_object_max_size = Some(5 * 1024 * 1024 * 1024);
///
/// // Wrap in StaticConfigProvider for immutable config
/// let static_provider = Arc::new(StaticConfigProvider::new(Arc::new(config.clone())));
/// let snapshot = static_provider.snapshot();
/// assert_eq!(snapshot.xml_max_body_size, 10 * 1024 * 1024);
/// assert_eq!(snapshot.put_object_max_size, Some(5 * 1024 * 1024 * 1024));
///
/// // Or wrap in HotReloadConfigProvider for runtime updates
/// let hot_config = Arc::new(HotReloadConfigProvider::new(Arc::new(config)));
/// let snapshot = hot_config.snapshot();
/// assert_eq!(snapshot.xml_max_body_size, 10 * 1024 * 1024);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct S3Config {
    /// Maximum size for XML body payloads in bytes.
    ///
    /// This limit prevents unbounded memory allocation for operations that require
    /// the full body in memory (e.g., XML parsing).
    ///
    /// Default: 20 MB (20 * 1024 * 1024)
    pub xml_max_body_size: usize,

    /// Maximum file size for POST object in bytes.
    ///
    /// S3 has a 5GB limit for single PUT object, so this is a reasonable default.
    ///
    /// Default: 5 GB (5 * 1024 * 1024 * 1024)
    pub post_object_max_file_size: u64,

    /// Optional maximum object size for streaming uploads in bytes.
    ///
    /// When set, `s3s` limits decoded streaming request bodies for operations
    /// such as `PUT Object` and `UploadPart` before passing them to the
    /// [`S3`](crate::S3) implementation. For aws-chunked requests, signature
    /// verification installs the decoded stream before this limit is applied.
    /// When unset, `s3s` does not impose a streaming object-size limit and the
    /// implementation must enforce any deployment-specific cap.
    ///
    /// `POST Object` keeps using [`S3Config::post_object_max_file_size`] as its
    /// file-size limit.
    ///
    /// Default: None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put_object_max_size: Option<u64>,

    /// Maximum size for custom-route request bodies in bytes.
    ///
    /// Custom routes may read request bodies outside the generated S3 operation
    /// deserializers. This limit bounds those bodies before route dispatch to
    /// reduce denial-of-service risk from oversized custom-route payloads.
    ///
    /// Set to `None` to disable the limit.
    ///
    /// Default: 1 MB (1024 * 1024)
    pub custom_route_max_body_size: Option<u64>,

    /// Maximum chunk data size for aws-chunked streaming uploads in bytes.
    ///
    /// Signed chunks must be buffered in full before their signature can be verified,
    /// so this limit bounds the memory retained per chunk. Unsigned chunks are streamed
    /// without buffering and are not subject to this limit.
    ///
    /// Default: 256 MB (256 * 1024 * 1024)
    pub aws_chunked_stream_max_chunk_size: usize,

    /// Maximum size per form field in bytes.
    ///
    /// This prevents denial-of-service attacks via oversized individual fields.
    ///
    /// Default: 1 MB (1024 * 1024)
    pub form_max_field_size: usize,

    /// Maximum total size for all form fields combined in bytes.
    ///
    /// This prevents denial-of-service attacks via accumulation of many fields.
    ///
    /// Default: 20 MB (20 * 1024 * 1024)
    pub form_max_fields_size: usize,

    /// Maximum number of parts in multipart form.
    ///
    /// This prevents denial-of-service attacks via excessive part count.
    ///
    /// Default: 1000
    pub form_max_parts: usize,

    /// Maximum allowed time skew for presigned URLs in seconds.
    ///
    /// This allows requests that are up to this many seconds in the future
    /// to account for clock skew between the client and server.
    ///
    /// Default: 900 (15 minutes)
    pub presigned_url_max_skew_time_secs: u32,

    /// Region accepted in `SigV4` credential scopes.
    ///
    /// When set, requests signed for another region are rejected with
    /// `AuthorizationHeaderMalformed`. When unset, any region is accepted.
    ///
    /// Default: None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_region: Option<Region>,

    /// Services accepted in `SigV4` credential scopes.
    ///
    /// Requests signed for services outside this list are rejected with
    /// `NotImplemented`. Add service names here when layering S3-compatible
    /// extension APIs behind the same authentication path.
    ///
    /// Default: `["s3", "sts"]`
    #[serde(
        default = "default_sig_v4_allowed_services",
        skip_serializing_if = "is_default_sig_v4_allowed_services"
    )]
    pub sig_v4_allowed_services: Vec<String>,

    /// Whether Signature Version 2 (`SigV2`) verification is enabled.
    ///
    /// `SigV2` is deprecated by AWS and does not bind the signature to a region
    /// or service. When disabled, requests carrying a `SigV2` signature (header
    /// auth, presigned URL, or POST form) are rejected with `AccessDenied`.
    ///
    /// Default: false (`SigV2` is disabled by default for security)
    pub enable_sig_v2: bool,

    /// Maximum allowed `X-Amz-Expires` value for `SigV4` presigned URLs in seconds.
    ///
    /// Default: 604800 (7 days, matching AWS S3 behavior)
    ///
    /// Values larger than 604800 intentionally diverge from AWS S3 compatibility.
    pub presigned_url_max_expires_secs: u32,

    /// Whether to normalize forward slashes in object keys.
    ///
    /// If enabled, multiple consecutive slashes will be treated as a single slash:
    ///
    /// - If the path does not start with `/`, no normalization is performed.
    /// - If the path starts with `/`, normalization:
    ///   - removes all leading slashes (unless the entire path is one or more `/`)
    ///   - replaces consecutive internal slashes with a single slash
    ///   - reduces one or more trailing slashes to a single trailing slash
    /// - Examples:
    ///   - "/"                 -> "/"
    ///   - "/keyname"          -> "keyname"
    ///   - "//keyname"         -> "keyname"
    ///   - "//keyname/"        -> "keyname/"
    ///   - "//dir////keyname"  -> "dir/keyname"
    ///   - "dir///sub//file"   -> "dir///sub//file"
    ///   - "/dir///sub//file"  -> "dir/sub/file"
    ///
    /// Default: false
    pub normalize_forward_slash_path: bool,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            xml_max_body_size: 20 * 1024 * 1024,               // 20 MB
            post_object_max_file_size: 5 * 1024 * 1024 * 1024, // 5 GB
            put_object_max_size: None,
            custom_route_max_body_size: Some(1024 * 1024), // 1 MB
            aws_chunked_stream_max_chunk_size: DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
            form_max_field_size: 1024 * 1024,       // 1 MB
            form_max_fields_size: 20 * 1024 * 1024, // 20 MB
            form_max_parts: 1000,
            presigned_url_max_skew_time_secs: 900, // 15 minutes
            expected_region: None,
            sig_v4_allowed_services: default_sig_v4_allowed_services(),
            enable_sig_v2: false,
            presigned_url_max_expires_secs: DEFAULT_PRESIGNED_URL_MAX_EXPIRES_SECS,
            normalize_forward_slash_path: false,
        }
    }
}

/// Static configuration provider.
///
/// This provider wraps an immutable configuration in an `Arc` for efficient sharing.
/// Use this when configuration does not need to be updated at runtime.
///
/// Use `Arc<StaticConfigProvider>` when sharing across threads.
///
/// # Example
/// ```
/// use std::sync::Arc;
/// use s3s::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
///
/// let config = Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default())));
///
/// // Read configuration via snapshot (just clones the Arc)
/// let snapshot = config.snapshot();
/// println!("Max XML body size: {}", snapshot.xml_max_body_size);
/// ```
#[derive(Debug)]
pub struct StaticConfigProvider {
    inner: Arc<S3Config>,
}

impl StaticConfigProvider {
    /// Creates a new static configuration provider.
    #[must_use]
    pub fn new(config: Arc<S3Config>) -> Self {
        Self { inner: config }
    }
}

impl Default for StaticConfigProvider {
    fn default() -> Self {
        Self::new(Arc::new(S3Config::default()))
    }
}

impl S3ConfigProvider for StaticConfigProvider {
    fn snapshot(&self) -> Arc<S3Config> {
        Arc::clone(&self.inner)
    }
}

/// Hot-reload configuration provider.
///
/// This provider allows updating the configuration at runtime using `ArcSwap`
/// for lock-free reads and atomic updates.
///
/// Use `Arc<HotReloadConfigProvider>` when sharing across threads.
///
/// # Example
/// ```
/// use std::sync::Arc;
/// use s3s::config::{S3Config, S3ConfigProvider, HotReloadConfigProvider};
///
/// let config = Arc::new(HotReloadConfigProvider::new(Arc::new(S3Config::default())));
///
/// // Read configuration via snapshot (lock-free, consistent)
/// let snapshot = config.snapshot();
/// println!("Max XML body size: {}", snapshot.xml_max_body_size);
///
/// // Update configuration at runtime (atomic swap)
/// let mut new_config = S3Config::default();
/// new_config.xml_max_body_size = 10 * 1024 * 1024;
/// config.update(Arc::new(new_config));
/// ```
#[derive(Debug)]
pub struct HotReloadConfigProvider {
    inner: ArcSwap<S3Config>,
}

impl HotReloadConfigProvider {
    /// Creates a new hot-reload configuration provider.
    #[must_use]
    pub fn new(config: Arc<S3Config>) -> Self {
        Self {
            inner: ArcSwap::from(config),
        }
    }

    /// Updates the configuration atomically.
    ///
    /// This operation replaces the entire configuration atomically.
    pub fn update(&self, config: Arc<S3Config>) {
        self.inner.store(config);
    }
}

impl Default for HotReloadConfigProvider {
    fn default() -> Self {
        Self::new(Arc::new(S3Config::default()))
    }
}

impl S3ConfigProvider for HotReloadConfigProvider {
    fn snapshot(&self) -> Arc<S3Config> {
        self.inner.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = S3Config::default();
        assert_eq!(config.xml_max_body_size, 20 * 1024 * 1024);
        assert_eq!(config.post_object_max_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.put_object_max_size, None);
        assert_eq!(config.custom_route_max_body_size, Some(1024 * 1024));
        assert_eq!(config.form_max_field_size, 1024 * 1024);
        assert_eq!(config.form_max_fields_size, 20 * 1024 * 1024);
        assert_eq!(config.form_max_parts, 1000);
        assert_eq!(config.presigned_url_max_skew_time_secs, 900);
        assert_eq!(config.expected_region, None);
        assert_eq!(config.sig_v4_allowed_services, ["s3", "sts"]);
        assert_eq!(config.presigned_url_max_expires_secs, DEFAULT_PRESIGNED_URL_MAX_EXPIRES_SECS);
        assert!(!config.enable_sig_v2);
    }

    #[test]
    fn test_static_config_provider() {
        let provider = StaticConfigProvider::new(Arc::new(S3Config::default()));
        assert_eq!(provider.snapshot().xml_max_body_size, 20 * 1024 * 1024);

        // Snapshots should be the same Arc
        let snapshot1 = provider.snapshot();
        let snapshot2 = provider.snapshot();
        assert!(Arc::ptr_eq(&snapshot1, &snapshot2));
    }

    #[test]
    fn test_hot_reload_config_provider() {
        let provider = HotReloadConfigProvider::new(Arc::new(S3Config::default()));
        assert_eq!(provider.snapshot().xml_max_body_size, 20 * 1024 * 1024);

        // Update configuration
        provider.update(Arc::new(S3Config {
            xml_max_body_size: 5 * 1024 * 1024,
            ..Default::default()
        }));
        assert_eq!(provider.snapshot().xml_max_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_snapshot_immutable() {
        let provider = HotReloadConfigProvider::new(Arc::new(S3Config::default()));
        let snapshot = provider.snapshot();

        // Update configuration
        provider.update(Arc::new(S3Config {
            xml_max_body_size: 5 * 1024 * 1024,
            ..Default::default()
        }));

        // Original snapshot should be unchanged
        assert_eq!(snapshot.xml_max_body_size, 20 * 1024 * 1024);

        // New read should reflect the update
        assert_eq!(provider.snapshot().xml_max_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config_provider_arc() {
        let provider = Arc::new(HotReloadConfigProvider::new(Arc::new(S3Config::default())));
        let cloned = provider.clone();

        // Both should read the same value
        assert_eq!(provider.snapshot().xml_max_body_size, 20 * 1024 * 1024);
        assert_eq!(cloned.snapshot().xml_max_body_size, 20 * 1024 * 1024);

        // Updating one should update both (they share the same ArcSwap)
        provider.update(Arc::new(S3Config {
            xml_max_body_size: 5 * 1024 * 1024,
            ..Default::default()
        }));

        assert_eq!(provider.snapshot().xml_max_body_size, 5 * 1024 * 1024);
        assert_eq!(cloned.snapshot().xml_max_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_config_provider_trait() {
        let provider: Arc<dyn S3ConfigProvider> = Arc::new(HotReloadConfigProvider::default());
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.xml_max_body_size, 20 * 1024 * 1024);
        assert_eq!(snapshot.post_object_max_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(snapshot.put_object_max_size, None);
    }

    #[test]
    fn test_static_config_provider_trait() {
        let provider: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.xml_max_body_size, 20 * 1024 * 1024);
        assert_eq!(snapshot.post_object_max_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(snapshot.put_object_max_size, None);
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = S3Config {
            xml_max_body_size: 10 * 1024 * 1024,
            post_object_max_file_size: 1024 * 1024 * 1024,
            put_object_max_size: Some(512 * 1024 * 1024),
            custom_route_max_body_size: Some(512 * 1024),
            aws_chunked_stream_max_chunk_size: DEFAULT_AWS_CHUNKED_STREAM_MAX_CHUNK_SIZE,
            form_max_field_size: 512 * 1024,
            form_max_fields_size: 5 * 1024 * 1024,
            form_max_parts: 500,
            presigned_url_max_skew_time_secs: 600,
            expected_region: Some("us-west-2".parse().expect("valid test region")),
            sig_v4_allowed_services: vec!["s3".to_owned(), "sts".to_owned(), "s3tables".to_owned()],
            enable_sig_v2: true,
            presigned_url_max_expires_secs: 86_400,
            normalize_forward_slash_path: false,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let deserialized: S3Config = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_serde_default_values() {
        // Test that missing fields use default values
        let json = r#"{"xml_max_body_size": 1024}"#;
        let config: S3Config = serde_json::from_str(json).expect("deserialize failed");

        assert_eq!(config.xml_max_body_size, 1024);
        // Other fields should have defaults
        assert_eq!(config.post_object_max_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.put_object_max_size, None);
        assert_eq!(config.custom_route_max_body_size, Some(1024 * 1024));
        assert_eq!(config.form_max_field_size, 1024 * 1024);
        assert_eq!(config.sig_v4_allowed_services, ["s3", "sts"]);
        assert_eq!(config.presigned_url_max_expires_secs, DEFAULT_PRESIGNED_URL_MAX_EXPIRES_SECS);
    }

    #[test]
    fn test_serde_omits_unset_expected_region() {
        let json = serde_json::to_value(S3Config::default()).expect("serialize failed");
        assert!(json.get("expected_region").is_none());
    }

    #[test]
    fn test_serde_omits_unset_put_object_max_size() {
        let json = serde_json::to_value(S3Config::default()).expect("serialize failed");
        assert!(json.get("put_object_max_size").is_none());
    }

    #[test]
    fn test_serde_omits_default_sig_v4_allowed_services() {
        let json = serde_json::to_value(S3Config::default()).expect("serialize failed");
        assert!(json.get("sig_v4_allowed_services").is_none());
    }

    #[test]
    fn test_hot_reload_in_service_layer() {
        // Test simulating how config would be used in service layer
        let provider = Arc::new(HotReloadConfigProvider::new(Arc::new(S3Config::default())));

        // Simulate processing requests with initial config
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.xml_max_body_size, 20 * 1024 * 1024);

        // Simulate config reload (e.g., from config file change)
        provider.update(Arc::new(S3Config {
            xml_max_body_size: 30 * 1024 * 1024,
            ..Default::default()
        }));

        // New requests should see updated config
        let new_snapshot = provider.snapshot();
        assert_eq!(new_snapshot.xml_max_body_size, 30 * 1024 * 1024);
    }
}

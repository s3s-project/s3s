//! S3 Service Configuration
//!
//! This module provides configurable parameters for the S3 service.
//!
//! # Features
//! - `serde` support for serialization/deserialization
//! - Default values for all parameters
//! - Static configuration via [`StaticConfig`]
//! - Hot-reload configuration via [`HotReloadConfig`]
//!
//! # Example
//! ```
//! use s3s::config::{S3Config, S3ConfigValues, StaticConfig, HotReloadConfig};
//!
//! // Using default config values
//! let values = S3ConfigValues::default();
//!
//! // Using builder pattern
//! let values = S3ConfigValues::new()
//!     .with_max_xml_body_size(10 * 1024 * 1024);
//!
//! // Using static config (cheaper clone, immutable)
//! let static_config = StaticConfig::new(S3ConfigValues::default());
//! assert_eq!(static_config.max_xml_body_size(), 20 * 1024 * 1024);
//!
//! // Using hot-reload config (can be updated at runtime)
//! let hot_reload_config = HotReloadConfig::new(S3ConfigValues::default());
//! hot_reload_config.update(
//!     S3ConfigValues::new().with_max_xml_body_size(10 * 1024 * 1024)
//! );
//! assert_eq!(hot_reload_config.max_xml_body_size(), 10 * 1024 * 1024);
//! ```

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// S3 Service Configuration trait.
///
/// This trait provides getter methods for configurable parameters.
/// Both [`StaticConfig`] and [`HotReloadConfig`] implement this trait.
pub trait S3Config: Send + Sync + 'static {
    /// Returns the maximum size for XML body payloads in bytes.
    fn max_xml_body_size(&self) -> usize;

    /// Returns the maximum file size for POST object in bytes.
    fn max_post_object_file_size(&self) -> u64;

    /// Returns the maximum size per form field in bytes.
    fn max_form_field_size(&self) -> usize;

    /// Returns the maximum total size for all form fields combined in bytes.
    fn max_form_fields_size(&self) -> usize;

    /// Returns the maximum number of parts in multipart form.
    fn max_form_parts(&self) -> usize;
}

/// S3 Service Configuration values.
///
/// Contains configurable parameters for the S3 service with sensible defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct S3ConfigValues {
    /// Maximum size for XML body payloads in bytes.
    ///
    /// This limit prevents unbounded memory allocation for operations that require
    /// the full body in memory (e.g., XML parsing).
    ///
    /// Default: 20 MB (20 * 1024 * 1024)
    pub max_xml_body_size: usize,

    /// Maximum file size for POST object in bytes.
    ///
    /// S3 has a 5GB limit for single PUT object, so this is a reasonable default.
    ///
    /// Default: 5 GB (5 * 1024 * 1024 * 1024)
    pub max_post_object_file_size: u64,

    /// Maximum size per form field in bytes.
    ///
    /// This prevents denial-of-service attacks via oversized individual fields.
    ///
    /// Default: 1 MB (1024 * 1024)
    pub max_form_field_size: usize,

    /// Maximum total size for all form fields combined in bytes.
    ///
    /// This prevents denial-of-service attacks via accumulation of many fields.
    ///
    /// Default: 20 MB (20 * 1024 * 1024)
    pub max_form_fields_size: usize,

    /// Maximum number of parts in multipart form.
    ///
    /// This prevents denial-of-service attacks via excessive part count.
    ///
    /// Default: 1000
    pub max_form_parts: usize,
}

impl Default for S3ConfigValues {
    fn default() -> Self {
        Self {
            max_xml_body_size: 20 * 1024 * 1024,               // 20 MB
            max_post_object_file_size: 5 * 1024 * 1024 * 1024, // 5 GB
            max_form_field_size: 1024 * 1024,                  // 1 MB
            max_form_fields_size: 20 * 1024 * 1024,            // 20 MB
            max_form_parts: 1000,
        }
    }
}

impl S3ConfigValues {
    /// Creates a new `S3ConfigValues` with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum XML body size.
    #[must_use]
    pub fn with_max_xml_body_size(mut self, size: usize) -> Self {
        self.max_xml_body_size = size;
        self
    }

    /// Sets the maximum POST object file size.
    #[must_use]
    pub fn with_max_post_object_file_size(mut self, size: u64) -> Self {
        self.max_post_object_file_size = size;
        self
    }

    /// Sets the maximum form field size.
    #[must_use]
    pub fn with_max_form_field_size(mut self, size: usize) -> Self {
        self.max_form_field_size = size;
        self
    }

    /// Sets the maximum total form fields size.
    #[must_use]
    pub fn with_max_form_fields_size(mut self, size: usize) -> Self {
        self.max_form_fields_size = size;
        self
    }

    /// Sets the maximum number of form parts.
    #[must_use]
    pub fn with_max_form_parts(mut self, count: usize) -> Self {
        self.max_form_parts = count;
        self
    }
}

/// Static configuration wrapper.
///
/// This wrapper provides a cheap clone operation by using `Arc` internally.
/// The configuration is immutable after creation.
///
/// # Example
/// ```
/// use s3s::config::{S3Config, S3ConfigValues, StaticConfig};
///
/// let config = StaticConfig::new(S3ConfigValues::default());
/// let cloned = config.clone(); // Cheap clone (Arc clone)
///
/// // Access configuration via trait methods
/// let max_size = config.max_xml_body_size();
/// ```
#[derive(Debug, Clone)]
pub struct StaticConfig {
    inner: Arc<S3ConfigValues>,
}

impl StaticConfig {
    /// Creates a new static configuration.
    #[must_use]
    pub fn new(config: S3ConfigValues) -> Self {
        Self {
            inner: Arc::new(config),
        }
    }

    /// Returns a reference to the underlying configuration values.
    #[must_use]
    pub fn get(&self) -> &S3ConfigValues {
        &self.inner
    }
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self::new(S3ConfigValues::default())
    }
}

impl From<S3ConfigValues> for StaticConfig {
    fn from(config: S3ConfigValues) -> Self {
        Self::new(config)
    }
}

impl S3Config for StaticConfig {
    fn max_xml_body_size(&self) -> usize {
        self.inner.max_xml_body_size
    }

    fn max_post_object_file_size(&self) -> u64 {
        self.inner.max_post_object_file_size
    }

    fn max_form_field_size(&self) -> usize {
        self.inner.max_form_field_size
    }

    fn max_form_fields_size(&self) -> usize {
        self.inner.max_form_fields_size
    }

    fn max_form_parts(&self) -> usize {
        self.inner.max_form_parts
    }
}

/// Hot-reload configuration wrapper.
///
/// This wrapper allows updating the configuration at runtime using `ArcSwap`
/// for lock-free reads and atomic updates.
///
/// # Example
/// ```
/// use s3s::config::{S3Config, S3ConfigValues, HotReloadConfig};
///
/// let config = HotReloadConfig::new(S3ConfigValues::default());
///
/// // Read configuration (lock-free)
/// let max_size = config.max_xml_body_size();
/// println!("Max XML body size: {}", max_size);
///
/// // Update configuration at runtime (atomic swap)
/// config.update(
///     S3ConfigValues::new().with_max_xml_body_size(10 * 1024 * 1024)
/// );
/// ```
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    inner: Arc<ArcSwap<S3ConfigValues>>,
}

impl HotReloadConfig {
    /// Creates a new hot-reload configuration.
    #[must_use]
    pub fn new(config: S3ConfigValues) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(config)),
        }
    }

    /// Returns a snapshot of the current configuration.
    ///
    /// This operation is lock-free and returns an `Arc` to the current configuration.
    /// The snapshot is immutable and will not change even if the configuration is updated.
    #[must_use]
    pub fn snapshot(&self) -> Arc<S3ConfigValues> {
        self.inner.load_full()
    }

    /// Updates the configuration atomically.
    ///
    /// This operation replaces the entire configuration atomically.
    pub fn update(&self, config: S3ConfigValues) {
        self.inner.store(Arc::new(config));
    }
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self::new(S3ConfigValues::default())
    }
}

impl From<S3ConfigValues> for HotReloadConfig {
    fn from(config: S3ConfigValues) -> Self {
        Self::new(config)
    }
}

impl S3Config for HotReloadConfig {
    fn max_xml_body_size(&self) -> usize {
        self.inner.load().max_xml_body_size
    }

    fn max_post_object_file_size(&self) -> u64 {
        self.inner.load().max_post_object_file_size
    }

    fn max_form_field_size(&self) -> usize {
        self.inner.load().max_form_field_size
    }

    fn max_form_fields_size(&self) -> usize {
        self.inner.load().max_form_fields_size
    }

    fn max_form_parts(&self) -> usize {
        self.inner.load().max_form_parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = S3ConfigValues::default();
        assert_eq!(config.max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
        assert_eq!(config.max_form_fields_size, 20 * 1024 * 1024);
        assert_eq!(config.max_form_parts, 1000);
    }

    #[test]
    fn test_static_config() {
        let config = StaticConfig::new(S3ConfigValues {
            max_xml_body_size: 10 * 1024 * 1024,
            ..Default::default()
        });
        assert_eq!(config.max_xml_body_size(), 10 * 1024 * 1024);
        assert_eq!(config.get().max_xml_body_size, 10 * 1024 * 1024);

        // Test cheap clone
        let cloned = config.clone();
        assert_eq!(cloned.max_xml_body_size(), 10 * 1024 * 1024);

        // Test that both point to the same Arc
        assert!(Arc::ptr_eq(&config.inner, &cloned.inner));
    }

    #[test]
    fn test_static_config_trait() {
        let config: Box<dyn S3Config> = Box::new(StaticConfig::default());
        assert_eq!(config.max_xml_body_size(), 20 * 1024 * 1024);
        assert_eq!(config.max_post_object_file_size(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config() {
        let config = HotReloadConfig::new(S3ConfigValues::default());
        assert_eq!(config.max_xml_body_size(), 20 * 1024 * 1024);

        // Update configuration
        config.update(S3ConfigValues {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });
        assert_eq!(config.max_xml_body_size(), 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_snapshot_immutable() {
        let config = HotReloadConfig::new(S3ConfigValues::default());
        let snapshot = config.snapshot();

        // Update configuration
        config.update(S3ConfigValues {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });

        // Original snapshot should be unchanged
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);

        // New read should reflect the update
        assert_eq!(config.max_xml_body_size(), 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config_clone() {
        let config = HotReloadConfig::new(S3ConfigValues::default());
        let cloned = config.clone();

        // Both should read the same value
        assert_eq!(config.max_xml_body_size(), 20 * 1024 * 1024);
        assert_eq!(cloned.max_xml_body_size(), 20 * 1024 * 1024);

        // Updating one should update both (they share the same ArcSwap)
        config.update(S3ConfigValues {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });

        assert_eq!(config.max_xml_body_size(), 5 * 1024 * 1024);
        assert_eq!(cloned.max_xml_body_size(), 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config_trait() {
        let config: Box<dyn S3Config> = Box::new(HotReloadConfig::default());
        assert_eq!(config.max_xml_body_size(), 20 * 1024 * 1024);
        assert_eq!(config.max_post_object_file_size(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = S3ConfigValues {
            max_xml_body_size: 10 * 1024 * 1024,
            max_post_object_file_size: 1024 * 1024 * 1024,
            max_form_field_size: 512 * 1024,
            max_form_fields_size: 5 * 1024 * 1024,
            max_form_parts: 500,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let deserialized: S3ConfigValues = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_serde_default_values() {
        // Test that missing fields use default values
        let json = r#"{"max_xml_body_size": 1024}"#;
        let config: S3ConfigValues = serde_json::from_str(json).expect("deserialize failed");

        assert_eq!(config.max_xml_body_size, 1024);
        // Other fields should have defaults
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
    }

    #[test]
    fn test_from_impl() {
        let config = S3ConfigValues::default();

        let static_config: StaticConfig = config.clone().into();
        assert_eq!(static_config.get(), &config);

        let hot_reload_config: HotReloadConfig = config.clone().into();
        assert_eq!(*hot_reload_config.snapshot(), config);
    }
}

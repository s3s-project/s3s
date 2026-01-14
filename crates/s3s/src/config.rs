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
//! use s3s::config::{S3Config, StaticConfig, HotReloadConfig};
//!
//! // Using default config
//! let config = S3Config::default();
//!
//! // Using builder pattern
//! let config = S3Config::new()
//!     .with_max_xml_body_size(10 * 1024 * 1024);
//!
//! // Using static config (cheaper clone, immutable)
//! let static_config = StaticConfig::new(S3Config::default());
//!
//! // Using hot-reload config (can be updated at runtime)
//! let hot_reload_config = HotReloadConfig::new(S3Config::default());
//! hot_reload_config.update(
//!     S3Config::new().with_max_xml_body_size(10 * 1024 * 1024)
//! );
//! ```

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// S3 Service Configuration
///
/// Contains configurable parameters for the S3 service with sensible defaults.
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
    pub max_xml_body_size: usize,

    /// Maximum file size for POST object in bytes.
    ///
    /// S3 has a 5GB limit for single PUT object, so this is a reasonable default.
    ///
    /// Default: 5 GB (5 * 1024 * 1024 * 1024)
    pub max_post_object_file_size: u64,

    /// Maximum size per form field in bytes.
    ///
    /// This prevents `DoS` attacks via oversized individual fields.
    ///
    /// Default: 1 MB (1024 * 1024)
    pub max_form_field_size: usize,

    /// Maximum total size for all form fields combined in bytes.
    ///
    /// This prevents `DoS` attacks via accumulation of many fields.
    ///
    /// Default: 20 MB (20 * 1024 * 1024)
    pub max_form_fields_size: usize,

    /// Maximum number of parts in multipart form.
    ///
    /// This prevents `DoS` attacks via excessive part count.
    ///
    /// Default: 1000
    pub max_form_parts: usize,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            max_xml_body_size: 20 * 1024 * 1024,           // 20 MB
            max_post_object_file_size: 5 * 1024 * 1024 * 1024, // 5 GB
            max_form_field_size: 1024 * 1024,             // 1 MB
            max_form_fields_size: 20 * 1024 * 1024,       // 20 MB
            max_form_parts: 1000,
        }
    }
}

impl S3Config {
    /// Creates a new `S3Config` with default values.
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
/// use s3s::config::{S3Config, StaticConfig};
///
/// let config = StaticConfig::new(S3Config::default());
/// let cloned = config.clone(); // Cheap clone (Arc clone)
///
/// // Access configuration
/// let max_size = config.get().max_xml_body_size;
/// ```
#[derive(Debug, Clone)]
pub struct StaticConfig {
    inner: Arc<S3Config>,
}

impl StaticConfig {
    /// Creates a new static configuration.
    #[must_use]
    pub fn new(config: S3Config) -> Self {
        Self {
            inner: Arc::new(config),
        }
    }

    /// Returns a reference to the configuration.
    #[must_use]
    pub fn get(&self) -> &S3Config {
        &self.inner
    }
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self::new(S3Config::default())
    }
}

impl From<S3Config> for StaticConfig {
    fn from(config: S3Config) -> Self {
        Self::new(config)
    }
}

/// Hot-reload configuration wrapper.
///
/// This wrapper allows updating the configuration at runtime.
/// Reads are lock-free in the common case, and updates are synchronized.
///
/// # Example
/// ```
/// use s3s::config::{S3Config, HotReloadConfig};
///
/// let config = HotReloadConfig::new(S3Config::default());
///
/// // Read configuration (cheap, uses RwLock read)
/// let snapshot = config.snapshot();
/// println!("Max XML body size: {}", snapshot.max_xml_body_size);
///
/// // Update configuration at runtime
/// config.update(
///     S3Config::new().with_max_xml_body_size(10 * 1024 * 1024)
/// );
/// ```
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    inner: Arc<RwLock<Arc<S3Config>>>,
}

impl HotReloadConfig {
    /// Creates a new hot-reload configuration.
    #[must_use]
    pub fn new(config: S3Config) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    /// Returns a snapshot of the current configuration.
    ///
    /// This operation is cheap and returns an `Arc` to the current configuration.
    /// The snapshot is immutable and will not change even if the configuration is updated.
    ///
    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn snapshot(&self) -> Arc<S3Config> {
        self.inner
            .read()
            .expect("RwLock poisoned")
            .clone()
    }

    /// Updates the configuration.
    ///
    /// This operation acquires a write lock and replaces the entire configuration.
    ///
    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn update(&self, config: S3Config) {
        let mut guard = self.inner.write().expect("RwLock poisoned");
        *guard = Arc::new(config);
    }
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self::new(S3Config::default())
    }
}

impl From<S3Config> for HotReloadConfig {
    fn from(config: S3Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = S3Config::default();
        assert_eq!(config.max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
        assert_eq!(config.max_form_fields_size, 20 * 1024 * 1024);
        assert_eq!(config.max_form_parts, 1000);
    }

    #[test]
    fn test_static_config() {
        let config = StaticConfig::new(S3Config {
            max_xml_body_size: 10 * 1024 * 1024,
            ..Default::default()
        });
        assert_eq!(config.get().max_xml_body_size, 10 * 1024 * 1024);

        // Test cheap clone
        let cloned = config.clone();
        assert_eq!(cloned.get().max_xml_body_size, 10 * 1024 * 1024);

        // Test that both point to the same Arc
        assert!(Arc::ptr_eq(&config.inner, &cloned.inner));
    }

    #[test]
    fn test_hot_reload_config() {
        let config = HotReloadConfig::new(S3Config::default());
        assert_eq!(config.snapshot().max_xml_body_size, 20 * 1024 * 1024);

        // Update configuration
        config.update(S3Config {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });
        assert_eq!(config.snapshot().max_xml_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_snapshot_immutable() {
        let config = HotReloadConfig::new(S3Config::default());
        let snapshot = config.snapshot();

        // Update configuration
        config.update(S3Config {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });

        // Original snapshot should be unchanged
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);

        // New snapshot should reflect the update
        assert_eq!(config.snapshot().max_xml_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = S3Config {
            max_xml_body_size: 10 * 1024 * 1024,
            max_post_object_file_size: 1024 * 1024 * 1024,
            max_form_field_size: 512 * 1024,
            max_form_fields_size: 5 * 1024 * 1024,
            max_form_parts: 500,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let deserialized: S3Config = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_serde_default_values() {
        // Test that missing fields use default values
        let json = r#"{"max_xml_body_size": 1024}"#;
        let config: S3Config = serde_json::from_str(json).expect("deserialize failed");

        assert_eq!(config.max_xml_body_size, 1024);
        // Other fields should have defaults
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
    }

    #[test]
    fn test_from_impl() {
        let config = S3Config::default();

        let static_config: StaticConfig = config.clone().into();
        assert_eq!(static_config.get(), &config);

        let hot_reload_config: HotReloadConfig = config.clone().into();
        assert_eq!(*hot_reload_config.snapshot(), config);
    }
}

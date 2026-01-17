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
//! use std::sync::Arc;
//! use s3s::config::{S3Config, StaticConfig, HotReloadConfig};
//!
//! // Using default config values
//! let config = StaticConfig::default();
//!
//! // Using custom config values
//! let mut config = StaticConfig::default();
//! config.max_xml_body_size = 10 * 1024 * 1024;
//!
//! // Using static config with snapshot
//! let static_config: Arc<dyn S3Config> = Arc::new(StaticConfig::default());
//! let snapshot = static_config.snapshot();
//! assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);
//!
//! // Using hot-reload config (can be updated at runtime)
//! let hot_reload_config = Arc::new(HotReloadConfig::default());
//! let mut new_config = StaticConfig::default();
//! new_config.max_xml_body_size = 10 * 1024 * 1024;
//! hot_reload_config.update(new_config);
//! assert_eq!(hot_reload_config.snapshot().max_xml_body_size, 10 * 1024 * 1024);
//! ```

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// S3 Service Configuration trait.
///
/// This trait provides a `snapshot` method that returns an `Arc<StaticConfig>`.
/// This design allows for faster access and consistent reads across multiple
/// config values.
///
/// Both [`StaticConfig`] and [`HotReloadConfig`] implement this trait.
pub trait S3Config: Send + Sync + 'static {
    /// Returns a snapshot of the current configuration.
    ///
    /// This operation returns an `Arc<StaticConfig>` that provides consistent
    /// access to all configuration values. The snapshot is immutable and will
    /// not change even if the underlying configuration is updated.
    fn snapshot(&self) -> Arc<StaticConfig>;
}

/// Static configuration.
///
/// Contains configurable parameters for the S3 service with sensible defaults.
/// The configuration is immutable after creation.
///
/// # Example
/// ```
/// use std::sync::Arc;
/// use s3s::config::{S3Config, StaticConfig};
///
/// let mut config = StaticConfig::default();
/// config.max_xml_body_size = 10 * 1024 * 1024;
///
/// // Access configuration via snapshot
/// let static_config: Arc<dyn S3Config> = Arc::new(config);
/// let snapshot = static_config.snapshot();
/// assert_eq!(snapshot.max_xml_body_size, 10 * 1024 * 1024);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct StaticConfig {
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

impl Default for StaticConfig {
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

impl S3Config for StaticConfig {
    fn snapshot(&self) -> Arc<StaticConfig> {
        Arc::new(self.clone())
    }
}

/// Hot-reload configuration wrapper.
///
/// This wrapper allows updating the configuration at runtime using `ArcSwap`
/// for lock-free reads and atomic updates.
///
/// Use `Arc<HotReloadConfig>` when sharing across threads.
///
/// # Example
/// ```
/// use std::sync::Arc;
/// use s3s::config::{S3Config, StaticConfig, HotReloadConfig};
///
/// let config = Arc::new(HotReloadConfig::new(StaticConfig::default()));
///
/// // Read configuration via snapshot (lock-free, consistent)
/// let snapshot = config.snapshot();
/// println!("Max XML body size: {}", snapshot.max_xml_body_size);
///
/// // Update configuration at runtime (atomic swap)
/// let mut new_config = StaticConfig::default();
/// new_config.max_xml_body_size = 10 * 1024 * 1024;
/// config.update(new_config);
/// ```
#[derive(Debug)]
pub struct HotReloadConfig {
    inner: ArcSwap<StaticConfig>,
}

impl HotReloadConfig {
    /// Creates a new hot-reload configuration.
    #[must_use]
    pub fn new(config: StaticConfig) -> Self {
        Self {
            inner: ArcSwap::from_pointee(config),
        }
    }

    /// Updates the configuration atomically.
    ///
    /// This operation replaces the entire configuration atomically.
    pub fn update(&self, config: StaticConfig) {
        self.inner.store(Arc::new(config));
    }
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self::new(StaticConfig::default())
    }
}

impl From<StaticConfig> for HotReloadConfig {
    fn from(config: StaticConfig) -> Self {
        Self::new(config)
    }
}

impl S3Config for HotReloadConfig {
    fn snapshot(&self) -> Arc<StaticConfig> {
        self.inner.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = StaticConfig::default();
        assert_eq!(config.max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
        assert_eq!(config.max_form_fields_size, 20 * 1024 * 1024);
        assert_eq!(config.max_form_parts, 1000);
    }

    #[test]
    fn test_static_config() {
        let config = StaticConfig {
            max_xml_body_size: 10 * 1024 * 1024,
            ..Default::default()
        };
        let snapshot = config.snapshot();
        assert_eq!(snapshot.max_xml_body_size, 10 * 1024 * 1024);
        assert_eq!(config.max_xml_body_size, 10 * 1024 * 1024);
    }

    #[test]
    fn test_static_config_trait() {
        let config: Box<dyn S3Config> = Box::new(StaticConfig::default());
        let snapshot = config.snapshot();
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(snapshot.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config() {
        let config = HotReloadConfig::new(StaticConfig::default());
        assert_eq!(config.snapshot().max_xml_body_size, 20 * 1024 * 1024);

        // Update configuration
        config.update(StaticConfig {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });
        assert_eq!(config.snapshot().max_xml_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_snapshot_immutable() {
        let config = HotReloadConfig::new(StaticConfig::default());
        let snapshot = config.snapshot();

        // Update configuration
        config.update(StaticConfig {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });

        // Original snapshot should be unchanged
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);

        // New read should reflect the update
        assert_eq!(config.snapshot().max_xml_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config_arc() {
        let config = Arc::new(HotReloadConfig::new(StaticConfig::default()));
        let cloned = config.clone();

        // Both should read the same value
        assert_eq!(config.snapshot().max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(cloned.snapshot().max_xml_body_size, 20 * 1024 * 1024);

        // Updating one should update both (they share the same ArcSwap)
        config.update(StaticConfig {
            max_xml_body_size: 5 * 1024 * 1024,
            ..Default::default()
        });

        assert_eq!(config.snapshot().max_xml_body_size, 5 * 1024 * 1024);
        assert_eq!(cloned.snapshot().max_xml_body_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_hot_reload_config_trait() {
        let config: Arc<dyn S3Config> = Arc::new(HotReloadConfig::default());
        let snapshot = config.snapshot();
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);
        assert_eq!(snapshot.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = StaticConfig {
            max_xml_body_size: 10 * 1024 * 1024,
            max_post_object_file_size: 1024 * 1024 * 1024,
            max_form_field_size: 512 * 1024,
            max_form_fields_size: 5 * 1024 * 1024,
            max_form_parts: 500,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let deserialized: StaticConfig = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_serde_default_values() {
        // Test that missing fields use default values
        let json = r#"{"max_xml_body_size": 1024}"#;
        let config: StaticConfig = serde_json::from_str(json).expect("deserialize failed");

        assert_eq!(config.max_xml_body_size, 1024);
        // Other fields should have defaults
        assert_eq!(config.max_post_object_file_size, 5 * 1024 * 1024 * 1024);
        assert_eq!(config.max_form_field_size, 1024 * 1024);
    }

    #[test]
    fn test_from_impl() {
        let config = StaticConfig::default();
        let hot_reload_config: HotReloadConfig = config.clone().into();
        assert_eq!(*hot_reload_config.snapshot(), config);
    }

    #[test]
    fn test_hot_reload_in_service_layer() {
        // Test simulating how config would be used in service layer
        let config = Arc::new(HotReloadConfig::new(StaticConfig::default()));

        // Simulate processing requests with initial config
        let snapshot = config.snapshot();
        assert_eq!(snapshot.max_xml_body_size, 20 * 1024 * 1024);

        // Simulate config reload (e.g., from config file change)
        config.update(StaticConfig {
            max_xml_body_size: 30 * 1024 * 1024,
            ..Default::default()
        });

        // New requests should see updated config
        let new_snapshot = config.snapshot();
        assert_eq!(new_snapshot.max_xml_body_size, 30 * 1024 * 1024);
    }
}

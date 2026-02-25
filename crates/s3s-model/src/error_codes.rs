use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorCode {
    pub code: String,
    pub description: String,
    pub http_status_code: Option<u16>,
}

/// A map from category name to a list of error codes.
pub type ErrorCodeMap = BTreeMap<String, Vec<ErrorCode>>;

/// Load S3 error codes from a JSON file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn load_json(path: &str) -> Result<ErrorCodeMap> {
    let content = std::fs::read_to_string(path)?;
    let map: ErrorCodeMap = serde_json::from_str(&content)?;
    Ok(map)
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Crawl the AWS Smithy models and the S3 error code documentation.
//!
//! This replaces `data/crawl.py`: the same URLs, the same parsing and merge
//! rules, and the same output format (four-space indented JSON, insertion order
//! of the sections preserved).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::repo_root;

const S3_MODEL_COMMIT: &str = "db89911ca6d038dd370d843a515a813c1aa47e9d";
const STS_MODEL_COMMIT: &str = "97e6a2936175d03ec1de31284613e0ef94d2f9cb";
const AWS_MODEL_RAW: &str = "https://github.com/awslabs/aws-sdk-rust/raw";
const ERROR_CODES_DOC: &str = "https://docs.aws.amazon.com/AmazonS3/latest/API/API_Error.md";
const DATE_TIME_SUITE: &str =
    "https://github.com/smithy-lang/smithy-rs/raw/main/rust-runtime/aws-smithy-types/test_data/date_time_format_test_suite.json";

/// Lines that are exactly `+` delimit the entries of the documentation list.
static BLOCK_DELIMITER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\+[ \t]*\n").expect("constant regex"));
static CODE_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\+\s+\*Code:\*\s*(.+)").expect("constant regex"));
static DESCRIPTION_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\+\s+\*Description:\*\s*(.+)").expect("constant regex"));
static HTTP_STATUS_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\+\s+\*HTTP Status Code:\*\s*(.+)").expect("constant regex"));
static HTTP_STATUS_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{3})").expect("constant regex"));
static MARKDOWN_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("constant regex"));

/// Crawl the AWS Smithy models and the S3 error code documentation.
#[derive(Debug, Subcommand)]
pub(crate) enum Crawl {
    /// Download the S3 Smithy model.
    DownloadS3Model,
    /// Download the STS Smithy model.
    DownloadStsModel,
    /// Refresh the S3 error codes from the AWS documentation.
    #[command(name = "crawl-error-codes")]
    ErrorCodes,
    /// Download the date and time format test suite.
    DownloadDateTimeFormatTestSuite,
    /// Run every step above, in order.
    Update,
}

impl Crawl {
    pub(crate) fn run(self) -> Result<bool> {
        match self {
            Self::DownloadS3Model => download_aws_sdk("s3", S3_MODEL_COMMIT)?,
            Self::DownloadStsModel => download_aws_sdk("sts", STS_MODEL_COMMIT)?,
            Self::ErrorCodes => crawl_error_codes()?,
            Self::DownloadDateTimeFormatTestSuite => download_date_time_format_test_suite()?,
            Self::Update => {
                download_aws_sdk("s3", S3_MODEL_COMMIT)?;
                download_aws_sdk("sts", STS_MODEL_COMMIT)?;
                crawl_error_codes()?;
                download_date_time_format_test_suite()?;
            }
        }
        Ok(true)
    }
}

/// The directory that holds the crawled models.
fn model_dir() -> PathBuf {
    repo_root().join("data")
}

fn download_aws_sdk(service: &str, commit: &str) -> Result<()> {
    let url = format!("{AWS_MODEL_RAW}/{commit}/aws-models/{service}.json");
    let text = download_json(&url)?;
    write_text(&model_dir().join(format!("{service}.json")), &text)
}

fn download_date_time_format_test_suite() -> Result<()> {
    let text = download_json(DATE_TIME_SUITE)?;
    write_text(&model_dir().join("date_time_format_test_suite.json"), &text)
}

fn crawl_error_codes() -> Result<()> {
    let response = reqwest::blocking::get(ERROR_CODES_DOC).with_context(|| format!("unable to fetch {ERROR_CODES_DOC}"))?;
    let md_text = response.text().with_context(|| format!("unable to read {ERROR_CODES_DOC}"))?;
    if md_text.len() < 100 {
        bail!("unexpected response from {ERROR_CODES_DOC} (len={})", md_text.len());
    }

    let mut data = parse_error_codes(&md_text).context("unable to parse S3 error code docs")?;
    let path = model_dir().join("s3_error_codes.json");
    if path.exists() {
        let old = read_json(&path)?;
        merge_error_codes(&mut data, &old);
    }
    save_json(&path, &data)
}

/// Fetch a URL whose body must be a JSON document, and return the body verbatim.
fn download_json(url: &str) -> Result<String> {
    let response = reqwest::blocking::get(url).with_context(|| format!("unable to fetch {url}"))?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "unexpected status {} from {url}",
        response.status()
    );
    let text = response.text().with_context(|| format!("unable to read {url}"))?;
    serde_json::from_str::<Value>(&text).with_context(|| format!("unexpected response from {url}"))?;
    Ok(text)
}

/// Parse the error codes out of the AWS `API_Error.md` markdown.
///
/// The list uses a definition-list-like structure. Some entries have a known
/// formatting bug where `*Code:*` is used for the status line, so the first
/// `*Code:*` field of an entry is the code and every later one is treated as a
/// status. Entries without a code or a description are skipped, and the first
/// entry for a code wins.
fn parse_error_codes(md_text: &str) -> Option<Value> {
    let mut entries: Map<String, Value> = Map::new();

    for block in BLOCK_DELIMITER.split(md_text) {
        let mut code: Option<String> = None;
        let mut description: Option<String> = None;
        let mut http_status_raw: Option<String> = None;
        let mut code_count = 0_usize;

        for line in block.trim().lines() {
            let line = line.trim();

            if let Some(captures) = CODE_FIELD.captures(line) {
                code_count += 1;
                let value = captures[1].trim();
                if code_count == 1 {
                    code = Some(value.to_owned());
                } else if http_status_raw.is_none() && value != "N/A" {
                    http_status_raw = Some(value.to_owned());
                }
                continue;
            }
            if let Some(captures) = DESCRIPTION_FIELD.captures(line) {
                description = Some(captures[1].trim().to_owned());
                continue;
            }
            if let Some(captures) = HTTP_STATUS_FIELD.captures(line) {
                let value = captures[1].trim();
                if http_status_raw.is_none() && value != "N/A" {
                    http_status_raw = Some(value.to_owned());
                }
            }
        }

        let (Some(code), Some(description)) = (code, description) else {
            continue;
        };
        if entries.contains_key(&code) {
            continue;
        }

        let http_status_code = http_status_raw
            .as_deref()
            .and_then(|raw| HTTP_STATUS_CODE.captures(raw))
            .and_then(|captures| captures[1].parse::<u32>().ok())
            .map_or(Value::Null, |status| json!(status));

        let mut entry = Map::new();
        entry.insert("code".to_owned(), Value::String(code.clone()));
        entry.insert("description".to_owned(), Value::String(clean_description(&description)));
        entry.insert("http_status_code".to_owned(), http_status_code);
        entries.insert(code, Value::Object(entry));
    }

    if entries.is_empty() {
        return None;
    }

    let mut sorted: Vec<(String, Value)> = entries.into_iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));

    let mut root = Map::new();
    root.insert("S3".to_owned(), Value::Array(sorted.into_iter().map(|(_, entry)| entry).collect()));
    Some(Value::Object(root))
}

/// Strip the markdown link syntax `[text](url)` down to `text`.
fn clean_description(description: &str) -> String {
    MARKDOWN_LINK.replace_all(description, "$1").into_owned()
}

/// Merge the freshly parsed sections into the existing file.
///
/// Sections the new documentation does not cover are preserved as they are.
/// Inside the `S3` section, entries that disappeared are kept, and a description
/// is taken from the old file when it is substantially more detailed (at least
/// 50 characters and at least three times as long), because the markdown
/// probably lost content.
fn merge_error_codes(data: &mut Value, old: &Value) {
    let (Some(data), Some(old)) = (data.as_object_mut(), old.as_object()) else {
        return;
    };

    for (section, old_entries) in old {
        if !data.contains_key(section) {
            data.insert(section.clone(), old_entries.clone());
        } else if section == "S3" {
            merge_s3_section(data, old_entries);
        }
    }
}

fn merge_s3_section(data: &mut Map<String, Value>, old_entries: &Value) {
    let Some(Value::Array(entries)) = data.get_mut("S3") else {
        return;
    };
    let Some(old_entries) = old_entries.as_array() else {
        return;
    };

    let old_by_code: HashMap<&str, &Value> = old_entries
        .iter()
        .filter_map(|entry| entry.get("code").and_then(Value::as_str).map(|code| (code, entry)))
        .collect();
    let new_codes: HashSet<String> = entries
        .iter()
        .filter_map(|entry| entry.get("code").and_then(Value::as_str).map(str::to_owned))
        .collect();

    for old_entry in old_entries {
        let Some(code) = old_entry.get("code").and_then(Value::as_str) else {
            continue;
        };
        if !new_codes.contains(code) {
            entries.push(old_entry.clone());
        }
    }

    for entry in entries.iter_mut() {
        let Some(code) = entry.get("code").and_then(Value::as_str) else {
            continue;
        };
        let Some(old_entry) = old_by_code.get(code) else {
            continue;
        };
        let old_description = old_entry.get("description").and_then(Value::as_str).unwrap_or_default();
        let new_description = entry.get("description").and_then(Value::as_str).unwrap_or_default();
        let old_len = old_description.chars().count();
        let new_len = new_description.chars().count();
        if old_len >= 50 && old_len >= new_len * 3 {
            entry["description"] = Value::String(old_description.to_owned());
        }
    }

    entries.sort_by(|left, right| code_of(left).cmp(code_of(right)));
}

fn code_of(entry: &Value) -> &str {
    entry.get("code").and_then(Value::as_str).unwrap_or_default()
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Write JSON the way `json.dump(..., indent=4)` did.
fn save_json(path: &Path, data: &Value) -> Result<()> {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    data.serialize(&mut serializer)
        .context("failed to serialize the error codes")?;
    fs::write(path, buffer).with_context(|| format!("failed to write {}", path.display()))
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{clean_description, merge_error_codes, parse_error_codes};
    use serde_json::json;

    const SAMPLE: &str = "\n+\n  +  *Code:* AccessDenied\n  +  *Description:* [Access Denied](https://example.com/x)\n  +  *HTTP Status Code:* 403 Forbidden\n  +  *SOAP Fault Code Prefix:* Client\n\n+\n  +  *Code:* NoSuchBucket\n  +  *Description:* The bucket does not exist.\n  +  *Code:* 404 Not Found\n\n+\n  +  *Code:* SlowDown\n  +  *Description:* Reduce your request rate.\n  +  *HTTP Status Code:* N/A\n\n+\n  +  *Code:* NoDescription\n";

    #[test]
    fn parses_the_documented_quirks() {
        let parsed = parse_error_codes(SAMPLE).expect("entries");

        assert_eq!(
            parsed,
            json!({"S3": [
                {"code": "AccessDenied", "description": "Access Denied", "http_status_code": 403},
                {"code": "NoSuchBucket", "description": "The bucket does not exist.", "http_status_code": 404},
                {"code": "SlowDown", "description": "Reduce your request rate.", "http_status_code": null},
            ]})
        );
    }

    #[test]
    fn an_empty_document_has_no_entries() {
        assert!(parse_error_codes("nothing to see here").is_none());
    }

    #[test]
    fn strips_markdown_links() {
        assert_eq!(clean_description("see [the docs](https://example.com) now"), "see the docs now");
    }

    #[test]
    fn merges_sections_entries_and_descriptions() {
        let mut data = parse_error_codes(SAMPLE).expect("entries");
        let old = json!({
            "S3": [
                {"code": "AccessDenied", "description": "A".repeat(60), "http_status_code": 403},
                {"code": "RemovedCode", "description": "gone", "http_status_code": 400},
            ],
            "Replication": [{"code": "ReplicationConfigurationNotFoundError", "description": "x", "http_status_code": 404}],
        });

        merge_error_codes(&mut data, &old);

        let sections = data.as_object().expect("object");
        assert_eq!(sections.keys().collect::<Vec<_>>(), ["S3", "Replication"]);
        let entries = sections["S3"].as_array().expect("array");
        let codes: Vec<&str> = entries.iter().filter_map(|entry| entry["code"].as_str()).collect();
        assert_eq!(codes, ["AccessDenied", "NoSuchBucket", "RemovedCode", "SlowDown"]);
        assert_eq!(entries[0]["description"], json!("A".repeat(60)), "the longer old description wins");
        assert_eq!(entries[2]["description"], json!("gone"), "an entry the new docs dropped is kept");
        assert_eq!(sections["Replication"], old["Replication"]);
    }
}

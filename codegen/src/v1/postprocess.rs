// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Post-process the paired `generated.rs` / `generated_minio.rs` files into a
//! single file: identical logic groups are emitted once, differing groups are
//! emitted twice with mutually exclusive `#[cfg(feature = "minio")]` gates.
//!
//! The merge is text-level: every item is emitted as its original raw line
//! slice, so comments, blank lines, and function bodies are preserved
//! verbatim. tree-sitter is used only for grouping (item boundaries, names,
//! line numbers) and comparison.

use std::collections::HashMap;
use std::fs;

use tree_sitter::{Node, Parser, Tree};

pub fn run(files: &[(&str, &str)]) {
    for (base_path, minio_path) in files {
        merge_files(base_path, minio_path);
    }
}

const MINIO_GATE: &str = "#[cfg(feature = \"minio\")]";
const NOT_MINIO_GATE: &str = "#[cfg(not(feature = \"minio\"))]";

/// A code item paired with its attached prefix lines (doc comments,
/// attributes). All line numbers are 0-based and relative to the file the
/// item was extracted from.
struct Item {
    /// first line of the item including attached prefixes
    start: usize,
    /// first line of the body node
    body_start: usize,
    /// last line of the body node
    end: usize,
    /// whether the original file had a blank line before this item
    blank_before: bool,
    /// grouping key (type name, function name, ...)
    key: String,
    /// key of the item within its group (implemented trait for impl blocks,
    /// equal to `key` for other items)
    sub_key: String,
    /// tree-sitter node kind of the body node
    kind: &'static str,
}

fn parse(text: &str) -> Tree {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let tree = parser.parse(text, None).unwrap();
    assert!(!tree.root_node().has_error(), "tree-sitter parse error");
    tree
}

fn is_prefix(kind: &str) -> bool {
    matches!(kind, "attribute_item" | "line_comment" | "block_comment" | "doc_comment")
}

fn is_top_body(kind: &str) -> bool {
    matches!(
        kind,
        "struct_item"
            | "enum_item"
            | "type_item"
            | "trait_item"
            | "function_item"
            | "mod_item"
            | "const_item"
            | "static_item"
            | "impl_item"
            | "use_declaration"
            | "macro_definition"
            | "macro_invocation"
            | "extern_crate_declaration"
    )
}

fn is_member_body(kind: &str) -> bool {
    matches!(
        kind,
        "function_item" | "function_signature_item" | "type_item" | "const_item" | "macro_invocation"
    )
}

/// Collect the direct children of `parent` into items: prefix nodes
/// (attributes, comments) are folded into the range of the next body node.
/// For top-level collections the prefix of the first item belongs to the
/// file header; for member collections it belongs to the first member.
fn collect_items(
    parent: &Node,
    text: &str,
    is_body: impl Fn(&str) -> bool,
    key_of: impl Fn(&Node, &str) -> (String, String),
    first_absorbs_prefix: bool,
) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let mut items = Vec::new();
    let mut pending: Option<usize> = None;
    let mut prev_end: Option<usize> = None;
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        let kind = child.kind();
        if is_prefix(kind) {
            pending.get_or_insert(child.start_position().row);
        } else if is_body(kind) {
            let start_row = child.start_position().row;
            let start = if items.is_empty() && !first_absorbs_prefix {
                start_row
            } else {
                pending.unwrap_or(start_row)
            };
            let blank_before = prev_end.is_some_and(|pe| lines[pe + 1..start_row].iter().any(|l| l.trim().is_empty()));
            let (key, sub_key) = key_of(&child, text);
            items.push(Item {
                start,
                body_start: start_row,
                end: child.end_position().row,
                blank_before,
                key,
                sub_key,
                kind,
            });
            pending = None;
            prev_end = Some(child.end_position().row);
        }
    }
    items
}

fn item_key(node: &Node, text: &str) -> (String, String) {
    if node.kind() == "impl_item" {
        let self_ty = node
            .child_by_field_name("type")
            .map(|n| n.utf8_text(text.as_bytes()).unwrap().to_string())
            .unwrap_or_default();
        let before_generics = self_ty.split('<').next().unwrap_or(&self_ty);
        let key = before_generics.rsplit("::").next().unwrap_or(before_generics).to_string();
        let sub_key = node
            .child_by_field_name("trait")
            .map(|n| n.utf8_text(text.as_bytes()).unwrap().to_string())
            .unwrap_or_default();
        (key, sub_key)
    } else {
        let key = node.child_by_field_name("name").map_or_else(
            || node.utf8_text(text.as_bytes()).unwrap().to_string(),
            |n| n.utf8_text(text.as_bytes()).unwrap().to_string(),
        );
        (key.clone(), key)
    }
}

fn member_key(node: &Node, text: &str) -> (String, String) {
    item_key(node, text)
}

fn group_items(items: Vec<Item>) -> Vec<(String, Vec<Item>)> {
    let mut groups: Vec<(String, Vec<Item>)> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|(k, _)| *k == item.key) {
            Some((_, items)) => items.push(item),
            None => {
                let key = item.key.clone();
                groups.push((key, vec![item]));
            }
        }
    }
    groups
}

fn body_slice(lines: &[&str], item: &Item) -> String {
    lines[item.body_start..=item.end].join("\n")
}

/// Item text used for comparison: comments are dropped (they carry no
/// semantics and differ between the base and minio variants), while
/// attributes and the body are compared verbatim.
fn comparable(lines: &[&str], item: &Item) -> String {
    let mut parts = Vec::new();
    for line in &lines[item.start..item.body_start] {
        if line.trim_start().starts_with("//") {
            continue;
        }
        parts.push(*line);
    }
    for line in &lines[item.body_start..=item.end] {
        parts.push(*line);
    }
    parts.join("\n")
}

fn same_group(base: &[Item], minio: &[Item], base_lines: &[&str], minio_lines: &[&str]) -> bool {
    base.len() == minio.len()
        && base.iter().all(|b| {
            minio
                .iter()
                .find(|m| m.sub_key == b.sub_key)
                .is_some_and(|m| comparable(base_lines, b) == comparable(minio_lines, m))
        })
}

/// Emit an item, optionally inserting a cfg gate right before the body (after
/// any doc comments, following the rustfmt convention). Plain `//` comments
/// in the prefix stay outside the gate scope: they are not part of the item
/// and must not be duplicated by cfg-gated twin emissions. A blank line
/// before the item is emitted only when the original file had one, so
/// `use` blocks keep their original layout.
fn emit_item(out: &mut String, lines: &[&str], item: &Item, gate: Option<&str>) {
    if item.blank_before && !out.ends_with("\n\n") {
        out.push('\n');
    }
    let mut insert_at = item.body_start;
    if gate.is_some() {
        for (i, line) in lines[item.start..item.body_start].iter().enumerate() {
            if line.starts_with("///") {
                insert_at = item.start + i + 1;
            }
        }
    }
    for (i, line) in lines[item.start..item.body_start].iter().enumerate() {
        let line_no = item.start + i;
        if line_no == insert_at
            && let Some(g) = gate
        {
            out.push_str(g);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
    if insert_at >= item.body_start
        && let Some(g) = gate
    {
        out.push_str(g);
        out.push('\n');
    }
    for line in &lines[item.body_start..=item.end] {
        out.push_str(line);
        out.push('\n');
    }
}

fn emit_group_merged(out: &mut String, base: &[Item], minio: &[Item], base_lines: &[&str], minio_lines: &[&str]) {
    for b in base {
        match minio.iter().find(|m| m.sub_key == b.sub_key) {
            None => emit_item(out, base_lines, b, Some(NOT_MINIO_GATE)),
            Some(m) if comparable(base_lines, b) == comparable(minio_lines, m) => {
                emit_item(out, base_lines, b, None);
            }
            Some(m) if b.kind == m.kind && (b.kind == "impl_item" || b.kind == "trait_item") => {
                emit_impl_merged(out, b, m, base_lines, minio_lines);
            }
            Some(m) => {
                emit_item(out, base_lines, b, Some(NOT_MINIO_GATE));
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
                emit_item(out, minio_lines, m, Some(MINIO_GATE));
            }
        }
    }
    for m in minio {
        if !base.iter().any(|b| b.sub_key == m.sub_key) {
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            emit_item(out, minio_lines, m, Some(MINIO_GATE));
        }
    }
}

/// Parse the members of an impl/trait body node. Member line numbers are
/// relative to the body text.
fn impl_members(body: &str) -> Vec<Item> {
    let tree = parse(body);
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let kind = child.kind();
        if kind == "impl_item" || kind == "trait_item" {
            let mut c2 = child.walk();
            for gc in child.children(&mut c2) {
                if gc.kind() == "declaration_list" {
                    return collect_items(&gc, body, is_member_body, member_key, true);
                }
            }
        }
    }
    Vec::new()
}

/// Merge two impl/trait items member-wise: identical members are kept once,
/// base-only members get `cfg(not(minio))`, minio-only members get
/// `cfg(minio)`, and same-name differing members are emitted twice.
fn emit_impl_merged(out: &mut String, base: &Item, minio: &Item, base_lines: &[&str], minio_lines: &[&str]) {
    let base_body = body_slice(base_lines, base);
    let minio_body = body_slice(minio_lines, minio);

    let base_members = impl_members(&base_body);
    let minio_members = impl_members(&minio_body);

    let b_lines: Vec<&str> = base_body.lines().collect();
    let m_lines: Vec<&str> = minio_body.lines().collect();

    for line in &base_lines[base.start..base.body_start] {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(first) = b_lines.first() {
        out.push_str(first);
        out.push('\n');
    }

    for b in &base_members {
        match minio_members.iter().find(|m| m.key == b.key) {
            None => emit_item(out, &b_lines, b, Some(NOT_MINIO_GATE)),
            Some(m) if comparable(&b_lines, b) == comparable(&m_lines, m) => {
                emit_item(out, &b_lines, b, None);
            }
            Some(m) => {
                emit_item(out, &b_lines, b, Some(NOT_MINIO_GATE));
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
                emit_item(out, &m_lines, m, Some(MINIO_GATE));
            }
        }
    }
    for m in &minio_members {
        if !base_members.iter().any(|b| b.key == m.key) {
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            emit_item(out, &m_lines, m, Some(MINIO_GATE));
        }
    }
    out.push('}');
    out.push('\n');
    out.push('\n');
}

fn merge_files(base_path: &str, minio_path: &str) {
    let base_text = fs::read_to_string(base_path).unwrap();
    let minio_text = fs::read_to_string(minio_path).unwrap();

    let base_tree = parse(&base_text);
    let minio_tree = parse(&minio_text);

    let base_items = collect_items(&base_tree.root_node(), &base_text, is_top_body, item_key, false);
    let minio_items = collect_items(&minio_tree.root_node(), &minio_text, is_top_body, item_key, false);

    let base_groups = group_items(base_items);
    let minio_groups = group_items(minio_items);

    let base_lines: Vec<&str> = base_text.lines().collect();
    let minio_lines: Vec<&str> = minio_text.lines().collect();

    let minio_index: HashMap<&str, usize> = minio_groups.iter().enumerate().map(|(i, (k, _))| (k.as_str(), i)).collect();
    let base_index: HashMap<&str, usize> = base_groups.iter().enumerate().map(|(i, (k, _))| (k.as_str(), i)).collect();

    // Minio-only groups are emitted right after the nearest preceding shared
    // group, preserving the minio file layout as much as possible.
    let mut attached: Vec<Vec<usize>> = vec![Vec::new(); base_groups.len()];
    let mut unanchored: Vec<usize> = Vec::new();
    let mut last_shared: Option<usize> = None;
    for (mi, (key, _)) in minio_groups.iter().enumerate() {
        if let Some(&bi) = base_index.get(key.as_str()) {
            last_shared = Some(bi);
        } else if let Some(bi) = last_shared {
            attached[bi].push(mi);
        } else {
            unanchored.push(mi);
        }
    }

    let mut out = String::new();

    // Header: everything before the first body item of the base file.
    let header_end = base_groups
        .first()
        .and_then(|(_, g)| g.first())
        .map_or(base_lines.len(), |i| i.body_start);
    out.push_str(&base_lines[..header_end].join("\n"));
    out.push('\n');
    if !base_lines[..header_end].is_empty() {
        out.push('\n');
    }

    let is_use_group = |group: &[Item]| group.first().is_some_and(|i| i.kind == "use_declaration");

    for (bi, (key, base_group)) in base_groups.iter().enumerate() {
        if !is_use_group(base_group) && !out.ends_with("\n\n") {
            out.push('\n');
        }
        match minio_index.get(key.as_str()).map(|&i| &minio_groups[i].1) {
            None => {
                for item in base_group {
                    emit_item(&mut out, &base_lines, item, None);
                }
            }
            Some(minio_group) if same_group(base_group, minio_group, &base_lines, &minio_lines) => {
                for item in base_group {
                    emit_item(&mut out, &base_lines, item, None);
                }
            }
            Some(minio_group) => {
                emit_group_merged(&mut out, base_group, minio_group, &base_lines, &minio_lines);
            }
        }
        for &mi in &attached[bi] {
            let (_, minio_group) = &minio_groups[mi];
            if !is_use_group(minio_group) && !out.ends_with("\n\n") {
                out.push('\n');
            }
            for item in minio_group {
                emit_item(&mut out, &minio_lines, item, Some(MINIO_GATE));
            }
        }
    }
    for &mi in &unanchored {
        let (_, minio_group) = &minio_groups[mi];
        if !is_use_group(minio_group) && !out.ends_with("\n\n") {
            out.push('\n');
        }
        for item in minio_group {
            emit_item(&mut out, &minio_lines, item, Some(MINIO_GATE));
        }
    }

    fs::write(base_path, out).unwrap();
    fs::remove_file(minio_path).unwrap();

    let minio_only = attached.iter().map(Vec::len).sum::<usize>() + unanchored.len();
    eprintln!(
        "merged {base_path} + {minio_path} -> {base_path} ({} base groups, {minio_only} minio-only groups)",
        base_groups.len(),
    );
}

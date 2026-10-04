// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The xml test family: XML round trips and deserialization error paths for the
//! generated `s3s::xml` implementations.
//!
//! Every case is derived from the Smithy models, so the suite follows the
//! generated code instead of drifting from it:
//!
//! - a type that only the union (minio) model describes is emitted behind
//!   #[cfg(feature = "minio")], mirroring the gate on the item itself;
//! - a type whose two model variants differ is emitted twice, once from each
//!   variant behind the matching gate, because the merged generated.rs carries
//!   two cfg branches for it.
//!
//! The assertions are deliberately stronger than "it did not panic":
//!
//! - the serialized XML is checked for the root element, including the S3
//!   namespace of an operation output, and for every field tag name taken from
//!   the model, so a wrong tag name fails the test;
//! - the value is parsed back and compared with `PartialEq`, so a serializer and a
//!   deserializer that are wrong in the same way still fail the test;
//! - the error cases assert the exact `DeError` variant for an unknown element, a
//!   duplicate field, a missing required field, an element where text is
//!   expected, a malformed scalar and a wrong root element name, and they pin
//!   the namespace leniency of the root element.

use crate::v1::dto::RustTypes;
use crate::v1::ops::{Operation, Operations};
use crate::v1::rust;

use super::{codegen_file_header, write_test_file};

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::ops::Not;

use heck::ToSnakeCase;
use scoped_writer::g;

/// Sample text used for string-valued fields.
///
/// It is plain ASCII on purpose: the generated cases then exercise the XML
/// plumbing without depending on the escaping rules, which the fixed golden
/// tests pin separately.
const SAMPLE_TEXT: &str = "s3s-xml-sample";

/// Maximum nesting depth of a generated value literal.
const MAX_DEPTH: u32 = 16;

/// XML namespace of the S3 REST API, mirrored from `s3s::xml::generated`.
const XMLNS_S3: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

pub(super) fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let union = Surface::collect(ops.values(), rust_types_minio);
    let base = Surface::collect(ops.values().filter(|op| op.is_minio.not()), rust_types_base);

    let mut skipped: BTreeMap<String, String> = BTreeMap::new();
    let mut emitted = 0_usize;

    write_test_file("xml.rs", || {
        codegen_file_header(Some("xml"));
        g!("//! XML test family: round trips and deserialization error paths for the");
        g!("//! generated `s3s::xml` implementations.");
        g!();
        g!("#![allow(clippy::too_many_lines)]");
        g!();
        g!("use s3s::dto;");
        g!("use s3s::xml;");
        g!();

        emit_helpers();
        emit_goldens(&union);

        for name in &union.contents {
            let in_base = base.contents.contains(name);
            let same_data = equivalent(rust_types_base, rust_types_minio, name, &mut BTreeSet::new());
            let gates: &[Gate] = if in_base && same_data {
                &[Gate::Shared]
            } else if in_base {
                &[Gate::NoMinio, Gate::Minio]
            } else {
                &[Gate::Minio]
            };

            for &gate in gates {
                let (types, surface) = match gate {
                    Gate::NoMinio => (rust_types_base, &base),
                    Gate::Shared | Gate::Minio => (rust_types_minio, &union),
                };
                emitted += emit_case(types, surface, name, gate, &mut skipped);
                emitted += emit_default_case(types, surface, name, gate, &mut skipped);
                emitted += emit_struct_enum_variants(types, surface, name, gate, &mut skipped);
            }
        }
    });

    if skipped.is_empty() {
        eprintln!("[gen_tests::xml] {emitted} case(s) emitted, nothing skipped");
    } else {
        eprintln!("[gen_tests::xml] {emitted} case(s) emitted, {} skipped:", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("[gen_tests::xml]   {name}: {reason}");
        }
    }
}

/// Feature gate of one emitted case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// The two model variants agree on this type: no gate.
    Shared,
    /// The type is absent from, or differs in, the base variant.
    NoMinio,
    /// The type is absent from, or differs in, the base variant.
    Minio,
}

impl Gate {
    fn attr(self) -> Option<&'static str> {
        match self {
            Self::Shared => None,
            Self::NoMinio => Some("#[cfg(not(feature = \"minio\"))]"),
            Self::Minio => Some("#[cfg(feature = \"minio\")]"),
        }
    }
}

/// The XML surface of one model variant: which types get which impls.
struct Surface {
    /// root type name -> root element name (Serialize / Deserialize)
    roots: BTreeMap<String, String>,
    /// types that carry `SerializeContent` / `DeserializeContent`
    contents: BTreeSet<String>,
    /// types that are the payload of an operation input
    input_roots: BTreeSet<String>,
    /// types that are an operation output (serialized with the S3 namespace)
    outputs: BTreeSet<String>,
}

impl Surface {
    /// Mirrors the traversal of `s3s_codegen::v1::xml::codegen`, which decides
    /// where the generated impls land.
    #[allow(clippy::too_many_lines)]
    fn collect<'a>(ops: impl Iterator<Item = &'a Operation>, types: &RustTypes) -> Self {
        let mut roots: BTreeMap<String, String> = BTreeMap::new();
        let mut contents: BTreeSet<String> = BTreeSet::new();
        let mut input_roots: BTreeSet<String> = BTreeSet::new();
        let mut outputs: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        let ops: Vec<&Operation> = ops.collect();

        for op in &ops {
            outputs.insert(op.output.clone());

            for ty_name in [op.input.as_str(), op.output.as_str()] {
                let Some(rust::Type::Struct(ty)) = types.get(ty_name) else {
                    continue;
                };
                if is_xml_output(ty) {
                    roots.insert(ty_name.to_owned(), root_element_name(ty, None));
                    contents.insert(ty_name.to_owned());
                    queue.push_back(ty_name.to_owned());
                }
                for field in &ty.fields {
                    if !is_xml_payload(field) {
                        continue;
                    }
                    let name = field.type_.clone();
                    let element = match types.get(&name) {
                        Some(rust::Type::Struct(payload)) => root_element_name(payload, field.xml_name.as_deref()),
                        _ => field.xml_name.clone().unwrap_or_else(|| name.clone()),
                    };
                    roots.insert(name.clone(), element);
                    contents.insert(name.clone());
                    queue.push_back(name);
                }
            }
        }

        for op in &ops {
            let Some(rust::Type::Struct(ty)) = types.get(&op.input) else {
                continue;
            };
            for field in &ty.fields {
                if is_xml_payload(field) {
                    input_roots.insert(field.type_.clone());
                }
            }
        }

        for name in ["Progress", "Stats", "AssumeRoleOutput"] {
            let Some(rust::Type::Struct(ty)) = types.get(name) else {
                continue;
            };
            roots.insert(name.to_owned(), root_element_name(ty, None));
            contents.insert(name.to_owned());
            queue.push_back(name.to_owned());
        }

        while let Some(name) = queue.pop_front() {
            let Some(ty) = types.get(&name) else { continue };
            match ty {
                rust::Type::Struct(struct_ty) => {
                    for field in &struct_ty.fields {
                        if field.position != "xml" && !is_xml_payload(field) {
                            continue;
                        }
                        if let Some(rust::Type::List(list_ty)) = types.get(field.type_.as_str()) {
                            contents.insert(list_ty.member.type_.clone());
                            queue.push_back(list_ty.member.type_.clone());
                        } else {
                            contents.insert(field.type_.clone());
                            queue.push_back(field.type_.clone());
                        }
                    }
                }
                rust::Type::List(list_ty) => {
                    contents.insert(list_ty.member.type_.clone());
                    queue.push_back(list_ty.member.type_.clone());
                }
                rust::Type::StructEnum(enum_ty) => {
                    for variant in &enum_ty.variants {
                        contents.insert(variant.type_.clone());
                        queue.push_back(variant.type_.clone());
                    }
                }
                rust::Type::Provided(provided) => {
                    if provided.name == "ETag" {
                        contents.insert(provided.name.clone());
                    }
                }
                rust::Type::Alias(_) | rust::Type::StrEnum(_) | rust::Type::Timestamp(_) | rust::Type::Map(_) => {}
            }
        }

        // Types implemented by hand in s3s::xml: the generated file carries no
        // impl for the unwrapped location output, and the STS output is special
        // cased by the production emitter.
        let unwrapped: Vec<String> = ops
            .iter()
            .filter(|op| op.s3_unwrapped_xml_output)
            .map(|op| op.output.clone())
            .collect();
        // These types have hand written or special cased root impls, but their
        // SerializeContent / DeserializeContent impls are still generated.
        for name in unwrapped.iter().map(String::as_str).chain(["AssumeRoleOutput"]) {
            roots.remove(name);
        }

        Self {
            roots,
            contents,
            input_roots,
            outputs,
        }
    }
}

fn is_xml_payload(field: &rust::StructField) -> bool {
    let streaming = field.type_ == "StreamingBlob" || field.type_ == "SelectObjectContentEventStream";
    field.position == "payload" && field.type_ != "Policy" && streaming.not()
}

fn is_xml_output(ty: &rust::Struct) -> bool {
    ty.xml_name.is_some() || ty.fields.iter().any(|field| field.position == "xml")
}

/// Whether the production emitter writes a `Deserialize` / `DeserializeContent` impl.
fn can_deserialize(types: &RustTypes, name: &str) -> bool {
    match types.get(name) {
        Some(rust::Type::Struct(struct_ty)) => !struct_ty
            .fields
            .iter()
            .any(|field| matches!(field.position.as_str(), "header" | "query" | "metadata") || field.is_xml_attr),
        Some(_) => true,
        None => false,
    }
}

/// Whether the two model variants describe the same value for a type, including
/// every type reachable from it through an XML field or a sealed field.
///
/// A case that is built from the union (minio) data may only be emitted without a
/// gate when the whole subtree agrees: the merged generated.rs gates each impl on
/// its own, so a type can be shared while a nested type is not.
fn equivalent(base: &RustTypes, minio: &RustTypes, name: &str, seen: &mut BTreeSet<String>) -> bool {
    if !seen.insert(name.to_owned()) {
        return true;
    }
    let (Some(base_ty), Some(minio_ty)) = (base.get(name), minio.get(name)) else {
        return false;
    };
    if base_ty != minio_ty {
        return false;
    }
    match minio_ty {
        rust::Type::Struct(struct_ty) => struct_ty
            .fields
            .iter()
            .filter(|field| field.position == "xml" || field.position == "sealed" || is_xml_payload(field))
            .all(|field| equivalent(base, minio, &field.type_, seen)),
        rust::Type::List(list_ty) => equivalent(base, minio, &list_ty.member.type_, seen),
        rust::Type::StructEnum(enum_ty) => enum_ty
            .variants
            .iter()
            .all(|variant| equivalent(base, minio, &variant.type_, seen)),
        rust::Type::Alias(_)
        | rust::Type::Provided(_)
        | rust::Type::StrEnum(_)
        | rust::Type::Timestamp(_)
        | rust::Type::Map(_) => true,
    }
}

fn root_element_name(ty: &rust::Struct, override_name: Option<&str>) -> String {
    override_name.or(ty.xml_name.as_deref()).unwrap_or(&ty.name).to_owned()
}

/// Emits the helpers shared by every case. They are thin wrappers around the
/// public `s3s::xml` API so that a case reads as a round trip.
fn emit_helpers() {
    g!("fn xml_serialize<T: xml::Serialize>(value: &T) -> String {{");
    g!("    let mut buf = Vec::with_capacity(512);");
    g!("    {{");
    g!("        let mut ser = xml::Serializer::new(&mut buf);");
    g!("        value.serialize(&mut ser).unwrap();");
    g!("    }}");
    g!("    String::from_utf8(buf).unwrap()");
    g!("}}");
    g!();
    g!("fn xml_serialize_content<T: xml::SerializeContent>(value: &T) -> String {{");
    g!("    let mut buf = Vec::with_capacity(512);");
    g!("    {{");
    g!("        let mut ser = xml::Serializer::new(&mut buf);");
    g!("        value.serialize_content(&mut ser).unwrap();");
    g!("    }}");
    g!("    String::from_utf8(buf).unwrap()");
    g!("}}");
    g!();
    g!("fn xml_deserialize<T>(input: &[u8]) -> xml::DeResult<T>");
    g!("where");
    g!("    T: for<'xml> xml::Deserialize<'xml>,");
    g!("{{");
    g!("    let mut d = xml::Deserializer::new(input);");
    g!("    let ans = T::deserialize(&mut d)?;");
    g!("    d.expect_eof()?;");
    g!("    Ok(ans)");
    g!("}}");
    g!();
    g!("fn xml_deserialize_content<T>(input: &[u8]) -> xml::DeResult<T>");
    g!("where");
    g!("    T: for<'xml> xml::DeserializeContent<'xml>,");
    g!("{{");
    g!("    let mut d = xml::Deserializer::new(input);");
    g!("    let ans = T::deserialize_content(&mut d)?;");
    g!("    d.expect_eof()?;");
    g!("    Ok(ans)");
    g!("}}");
    g!();
}

/// Emits the byte exact golden tests: a fixed XML document is parsed and written
/// back, so the tag names, the member names, the S3 namespace and the escaping
/// of the serializer are pinned independently of the generated round trips.
fn emit_goldens(surface: &Surface) {
    g!("// Fixed XML goldens: a document is parsed and written back byte for byte, so");
    g!("// the tag names, the member names, the namespace and the escaping of the");
    g!("// serializer are pinned independently of the generated round trips.");
    g!();

    if surface.contents.contains("Checksum") {
        g!("#[test]");
        g!("fn xml_golden_checksum() {{");
        g!(
            r#"    const GOLDEN: &str = r"<ChecksumCRC32>crc32</ChecksumCRC32><ChecksumCRC32C>crc32c</ChecksumCRC32C><ChecksumCRC64NVME>crc64</ChecksumCRC64NVME><ChecksumMD5>md5</ChecksumMD5><ChecksumSHA1>sha1</ChecksumSHA1><ChecksumSHA256>sha256</ChecksumSHA256><ChecksumSHA512>sha512</ChecksumSHA512><ChecksumType>COMPOSITE</ChecksumType><ChecksumXXHASH128>xxh128</ChecksumXXHASH128><ChecksumXXHASH3>xxh3</ChecksumXXHASH3><ChecksumXXHASH64>xxh64</ChecksumXXHASH64>";"#
        );
        g!("    let value = xml_deserialize_content::<dto::Checksum>(GOLDEN.as_bytes()).unwrap();");
        g!("    assert_eq!(value.checksum_crc32.as_deref(), Some(\"crc32\"));");
        g!("    assert_eq!(value.checksum_xxhash64.as_deref(), Some(\"xxh64\"));");
        g!("    assert_eq!(value.checksum_type.as_ref().map(dto::ChecksumType::as_str), Some(\"COMPOSITE\"));");
        g!("    let xml = xml_serialize_content(&value);");
        g!("    assert!(xml == GOLDEN, \"golden mismatch: {{xml}}\");");
        g!("}}");
        g!();
    }

    if surface.roots.contains_key("CopyObjectResult") {
        g!("#[test]");
        g!("fn xml_golden_copy_object_result() {{");
        g!(
            r##"    const GOLDEN: &str = r#"<CopyObjectResult><ChecksumCRC32>crc32</ChecksumCRC32><ChecksumType>COMPOSITE</ChecksumType><ETag>"etag"</ETag><LastModified>2024-01-02T03:04:05.000Z</LastModified></CopyObjectResult>"#;"##
        );
        g!("    let value = xml_deserialize::<dto::CopyObjectResult>(GOLDEN.as_bytes()).unwrap();");
        g!("    assert_eq!(value.e_tag.as_ref().map(dto::ETag::value), Some(\"etag\"));");
        g!("    assert!(value.last_modified.is_some());");
        g!("    let xml = xml_serialize(&value);");
        g!("    assert!(xml == GOLDEN, \"golden mismatch: {{xml}}\");");
        g!("}}");
        g!();
    }

    if surface.roots.contains_key("CompletedMultipartUpload") {
        g!("#[test]");
        g!("fn xml_golden_completed_multipart_upload() {{");
        g!(
            r##"    const GOLDEN: &str = r#"<CompleteMultipartUpload><Part><ETag>"etag-1"</ETag><PartNumber>1</PartNumber></Part><Part><ETag>"etag-2"</ETag><PartNumber>2</PartNumber></Part><Part><ETag>"etag-3"</ETag><PartNumber>3</PartNumber></Part></CompleteMultipartUpload>"#;"##
        );
        g!("    let value = xml_deserialize::<dto::CompletedMultipartUpload>(GOLDEN.as_bytes()).unwrap();");
        g!("    let parts = value.parts.as_deref().unwrap();");
        g!("    assert_eq!(parts.len(), 3);");
        g!("    assert_eq!(parts[0].part_number, Some(1));");
        g!("    assert_eq!(parts[2].e_tag.as_ref().map(dto::ETag::value), Some(\"etag-3\"));");
        g!("    let xml = xml_serialize(&value);");
        g!("    assert!(xml == GOLDEN, \"golden mismatch: {{xml}}\");");
        g!("}}");
        g!();
    }

    if surface.roots.contains_key("Tagging") {
        g!("#[test]");
        g!("fn xml_golden_tagging() {{");
        g!(
            r#"    const GOLDEN: &str = r"<Tagging><TagSet><Tag><Key>a&lt;&amp;&gt;&quot;&apos;</Key><Value>v1</Value></Tag><Tag><Key>k2</Key><Value>v2</Value></Tag></TagSet></Tagging>";"#
        );
        g!("    let value = xml_deserialize::<dto::Tagging>(GOLDEN.as_bytes()).unwrap();");
        g!("    let tags = &value.tag_set;");
        g!("    assert_eq!(tags.len(), 2);");
        g!("    assert_eq!(tags[0].key.as_deref(), Some(\"a<&>\\\"'\"));");
        g!("    assert_eq!(tags[1].value.as_deref(), Some(\"v2\"));");
        g!("    let xml = xml_serialize(&value);");
        g!("    assert!(xml == GOLDEN, \"golden mismatch: {{xml}}\");");
        g!("}}");
        g!();
    }

    if surface.roots.contains_key("ListObjectsOutput") {
        g!("#[test]");
        g!("fn xml_golden_list_objects_output() {{");
        g!(
            r##"    const GOLDEN: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>photos</Name><Prefix>photos/</Prefix><MaxKeys>2</MaxKeys><IsTruncated>true</IsTruncated><Contents><Key>a.txt</Key><Size>3</Size></Contents><Delimiter>/</Delimiter></ListBucketResult>"#;"##
        );
        g!("    let value = dto::ListObjectsOutput {{");
        g!("        name: Some(String::from(\"photos\")),");
        g!("        prefix: Some(String::from(\"photos/\")),");
        g!("        marker: None,");
        g!("        max_keys: Some(2),");
        g!("        is_truncated: Some(true),");
        // Only the fields the golden exercises are set: `Object` carries members
        // that exist in one model variant only, and a complete literal would not
        // compile in both feature configurations.
        g!("        contents: Some(vec![dto::Object {{");
        g!("            key: Some(String::from(\"a.txt\")),");
        g!("            size: Some(3),");
        g!("            ..Default::default()");
        g!("        }}]),");
        g!("        common_prefixes: None,");
        g!("        delimiter: Some(String::from(\"/\")),");
        g!("        next_marker: None,");
        g!("        encoding_type: None,");
        g!("        request_charged: None,");
        g!("    }};");
        g!("    let xml = xml_serialize(&value);");
        g!("    assert!(xml == GOLDEN, \"golden mismatch: {{xml}}\");");
        g!("}}");
        g!();
    }
}

/// Emits the round trip case and, when the type can be deserialized, the error
/// path case. Returns the number of emitted test functions.
fn emit_case(types: &RustTypes, surface: &Surface, name: &str, gate: Gate, skipped: &mut BTreeMap<String, String>) -> usize {
    let Some(ty) = types.get(name) else { return 0 };
    if !matches!(ty, rust::Type::Struct(_) | rust::Type::StrEnum(_) | rust::Type::StructEnum(_)) {
        return 0;
    }

    let values = Values { types };
    let value = match values.expr(name, 0) {
        Ok(value) => value,
        Err(reason) => {
            skipped.entry(name.to_owned()).or_insert(reason);
            return 0;
        }
    };

    let root = surface.roots.get(name);
    let namespaced = surface.outputs.contains(name);
    let deserializable = can_deserialize(types, name);
    let snake = name.to_snake_case();
    let mut count = 0;

    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_roundtrip_{snake}() {{");
    g!("    let value = {value};");
    match root {
        Some(element) => {
            g!("    let xml = xml_serialize(&value);");
            let open = if namespaced {
                format!("<{element} xmlns=\"{XMLNS_S3}\">")
            } else {
                format!("<{element}>")
            };
            let close = format!("</{element}>");
            g!("    assert!(xml.starts_with({open:?}), \"root element: {{xml}}\");");
            g!("    assert!(xml.ends_with({close:?}), \"root element: {{xml}}\");");
        }
        None => {
            g!("    let xml = xml_serialize_content(&value);");
        }
    }
    for fragment in expected_fragments(types, ty) {
        g!("    assert!(xml.contains({fragment:?}), \"missing element: {{xml}}\");");
    }
    if deserializable {
        let deserialize = if root.is_some() {
            "xml_deserialize"
        } else {
            "xml_deserialize_content"
        };
        g!("    let parsed = {deserialize}::<dto::{name}>(xml.as_bytes()).unwrap();");
        g!("    assert!(parsed == value, \"xml round trip mismatch: {{xml}}\");");
    }
    g!("}}");
    g!();
    count += 1;

    if deserializable {
        count += emit_errors(types, surface, name, gate, &value);
    }

    count
}

/// Emits the case of the value with every optional field unset.
///
/// Every optional field of that value is `None`, so the generated serializer
/// must skip all of them: the fall-through branch of each `if let Some(..)`
/// guard is the branch no populated sample reaches. The root element and every
/// required field of the model must still be written, so the case is not a
/// smoke test.
fn emit_default_case(
    types: &RustTypes,
    surface: &Surface,
    name: &str,
    gate: Gate,
    skipped: &mut BTreeMap<String, String>,
) -> usize {
    let Some(rust::Type::Struct(struct_ty)) = types.get(name) else {
        return 0;
    };
    if !surface.contents.contains(name) {
        return 0;
    }

    let values = Values { types };
    let value = match values.minimal_expr(name, 0) {
        Ok(value) => value,
        Err(reason) => {
            skipped.entry(name.to_owned()).or_insert(reason);
            return 0;
        }
    };

    let root = surface.roots.get(name);
    let namespaced = surface.outputs.contains(name);
    let mut optional: Vec<String> = Vec::new();
    let mut required: Vec<String> = Vec::new();
    for field in &struct_ty.fields {
        if field.position != "xml" || field.is_xml_attr {
            continue;
        }
        let tag = field.xml_name.clone().unwrap_or_else(|| field.camel_name.clone());
        if field.option_type || field.default_value.is_some() {
            optional.push(tag);
        } else {
            required.push(tag);
        }
    }

    // A content type without a single element has nothing to assert about the
    // serialized text beyond the serialization itself.
    let binding = if root.is_none() && optional.is_empty() && required.is_empty() {
        "_xml"
    } else {
        "xml"
    };

    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_minimal_{}() {{", name.to_snake_case());
    g!("    let value = {value};");
    match root {
        Some(element) => {
            g!("    let xml = xml_serialize(&value);");
            let open = if namespaced {
                format!("<{element} xmlns=\"{XMLNS_S3}\">")
            } else {
                format!("<{element}>")
            };
            let close = format!("</{element}>");
            g!("    assert!(xml.starts_with({open:?}), \"root element: {{xml}}\");");
            g!("    assert!(xml.ends_with({close:?}), \"root element: {{xml}}\");");
        }
        None => {
            g!("    let {binding} = xml_serialize_content(&value);");
        }
    }
    for tag in &optional {
        let element = format!("<{tag}>");
        let attributed = format!("<{tag} ");
        g!(
            "    assert!(!xml.contains({element:?}) && !xml.contains({attributed:?}), \"optional element in the default value: {{xml}}\");"
        );
    }
    for tag in &required {
        let element = format!("<{tag}>");
        let attributed = format!("<{tag} ");
        let empty = format!("<{tag}/>");
        g!(
            "    assert!(xml.contains({element:?}) || xml.contains({attributed:?}) || xml.contains({empty:?}), \"missing element: {{xml}}\");"
        );
    }
    g!("}}");
    g!();
    1
}

/// Emits the case of every variant of a struct enum.
///
/// The round trip case builds the first variant only, so the other arms of the
/// generated `match` need a value of their own, both when serializing and when
/// deserializing.
fn emit_struct_enum_variants(
    types: &RustTypes,
    surface: &Surface,
    name: &str,
    gate: Gate,
    skipped: &mut BTreeMap<String, String>,
) -> usize {
    let Some(rust::Type::StructEnum(enum_ty)) = types.get(name) else {
        return 0;
    };
    if enum_ty.variants.len() < 2 {
        return 0;
    }

    let values = Values { types };
    let mut variants: Vec<(String, String, String)> = Vec::new();
    for variant in &enum_ty.variants {
        let inner = match values.expr(&variant.type_, 0) {
            Ok(inner) => inner,
            Err(reason) => {
                skipped.entry(name.to_owned()).or_insert(reason);
                return 0;
            }
        };
        let tag = variant.xml_name.clone().unwrap_or_else(|| variant.name.clone());
        variants.push((tag, variant.name.clone(), format!("dto::{name}::{}({inner})", variant.name)));
    }

    let root = surface.roots.get(name);
    let deserialize = if root.is_some() {
        "xml_deserialize"
    } else {
        "xml_deserialize_content"
    };
    let serialize = if root.is_some() {
        "xml_serialize"
    } else {
        "xml_serialize_content"
    };

    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_variants_{}() {{", name.to_snake_case());
    for (tag, variant, expr) in &variants {
        let element = format!("<{tag}>");
        let attributed = format!("<{tag} ");
        g!("    {{");
        g!("        let value = {expr};");
        g!("        let xml = {serialize}(&value);");
        g!("        assert!(xml.contains({element:?}) || xml.contains({attributed:?}), \"{variant} variant: {{xml}}\");");
        g!("        let parsed = {deserialize}::<dto::{name}>(xml.as_bytes()).unwrap();");
        g!("        assert!(parsed == value, \"xml variant round trip mismatch: {{xml}}\");");
        g!("    }}");
    }
    g!("}}");
    g!();
    1
}

/// Emits the error path case. The probes mirror the deserialization branches the
/// production emitter generates, and each one asserts the exact error variant.
#[allow(clippy::too_many_lines)]
fn emit_errors(types: &RustTypes, surface: &Surface, name: &str, gate: Gate, value: &str) -> usize {
    let root = surface.roots.get(name);
    let deserialize = if root.is_some() {
        "xml_deserialize"
    } else {
        "xml_deserialize_content"
    };
    let wrap = |content: &str| match root {
        Some(element) => format!("<{element}>{content}</{element}>"),
        None => content.to_owned(),
    };

    match types.get(name) {
        Some(rust::Type::StrEnum(enum_ty)) => return emit_str_enum_errors(name, gate, enum_ty),
        Some(rust::Type::StructEnum(_)) => return emit_struct_enum_errors(name, gate, root),
        // An empty shape decodes from any document without reading it, so it has
        // no error path of its own; its round trip case covers the impls.
        Some(rust::Type::Struct(struct_ty)) if struct_ty.fields.is_empty() => return 0,
        _ => {}
    }

    let mut probes: Vec<Probe> = Vec::new();
    let mut duplicates: Vec<Probe> = Vec::new();
    let mut required = false;
    let mut text_field: Option<String> = None;
    let mut scalar_field: Option<String> = None;
    let mut namespace_field = false;

    if let Some(rust::Type::Struct(struct_ty)) = types.get(name) {
        for field in &struct_ty.fields {
            if field.position == "sealed" || field.is_xml_attr {
                continue;
            }
            let tag = field.xml_name.as_deref().unwrap_or(&field.camel_name).to_owned();
            if !field.option_type && field.default_value.is_none() {
                required = true;
            }
            if field.xml_namespace_prefix.is_some() {
                namespace_field = true;
            }
            if text_field.is_none() && is_text_type(types, &field.type_) {
                text_field = Some(tag.clone());
            }
            if scalar_field.is_none() && is_scalar_type(types, &field.type_) {
                scalar_field = Some(tag.clone());
            }
            // A flattened list accumulates its members, so a repeated tag is not a
            // duplicate for it. Every other field rejects a second occurrence, and
            // each of those guards is a line of its own.
            if !field.xml_flattened
                && let Some(content) = minimal_content(types, &field.type_)
            {
                let element = element_with_attributes(&tag, &field_attributes(field), &content);
                duplicates.push(Probe {
                    label: "duplicate field",
                    input: wrap(&format!("{element}{element}")),
                    expected: Some("DuplicateField"),
                });
            }
        }
    }

    // An element the model does not describe: a nested type rejects it, while the
    // payload of a request skips it for forward compatibility.
    let lenient = surface.input_roots.contains(name);
    if !lenient {
        probes.push(Probe {
            label: "unknown element",
            input: wrap("<S3sUnknown>x</S3sUnknown>"),
            expected: Some("UnexpectedTagName"),
        });
    }

    // An empty element: the required fields of the model cannot be filled.
    probes.push(Probe {
        label: "empty element",
        input: wrap(""),
        expected: required.then_some("MissingField"),
    });

    if let Some(tag) = text_field {
        probes.push(Probe {
            label: "element where text is expected",
            input: wrap(&format!("<{tag}><S3sNested/></{tag}>")),
            expected: Some("UnexpectedStart"),
        });
    }

    if let Some(tag) = scalar_field {
        probes.push(Probe {
            label: "malformed scalar content",
            input: wrap(&format!("<{tag}>s3s-not-a-value</{tag}>")),
            expected: Some("InvalidContent"),
        });
    }

    if let Some(element) = root {
        probes.push(Probe {
            label: "wrong root element name",
            input: "<S3sWrongRoot></S3sWrongRoot>".to_owned(),
            expected: Some("UnexpectedTagName"),
        });
        let _ = element;
    }

    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_errors_{}() {{", name.to_snake_case());
    if root.is_some() || lenient {
        g!("    let value = {value};");
    }
    if lenient {
        let input = match root {
            Some(element) => format!("format!(\"<{element}><S3sUnknown>x</S3sUnknown>{{content}}</{element}>\")"),
            None => "format!(\"<S3sUnknown>x</S3sUnknown>{content}\")".to_owned(),
        };
        g!("    let content = xml_serialize_content(&value);");
        g!("    let input = {input};");
        g!("    let ans = {deserialize}::<dto::{name}>(input.as_bytes());");
        g!("    if let Err(err) = ans {{");
        g!("        panic!(\"unknown element: {{err:?}}, input: {{input}}\");");
        g!("    }}");
    }
    for probe in &duplicates {
        emit_probe(probe, name, deserialize);
    }
    if let Some(element) = root {
        // The deserializer matches the root element by name and ignores its
        // attributes, so a foreign namespace must not change the parsed value.
        g!("    let content = xml_serialize_content(&value);");
        g!("    let canonical = format!(\"<{element}>{{content}}</{element}>\");");
        g!("    let foreign = format!(\"<{element} xmlns=\\\"urn:s3s-not-the-s3-namespace\\\">{{content}}</{element}>\");");
        g!("    let a = xml_deserialize::<dto::{name}>(canonical.as_bytes()).unwrap();");
        g!("    let b = xml_deserialize::<dto::{name}>(foreign.as_bytes()).unwrap();");
        g!("    assert!(a == b, \"a foreign namespace changed the parsed value\");");
    }
    if namespace_field {
        emit_namespace_attribute_probe(types, name, root, deserialize);
    }
    for probe in &probes {
        emit_probe(probe, name, deserialize);
    }
    g!("}}");
    g!();
    1
}

/// Emits the probes for a type whose model carries an xsi namespace attribute:
/// the attribute is read from the start tag, so a missing or malformed one is an
/// error path of its own.
fn emit_namespace_attribute_probe(types: &RustTypes, name: &str, root: Option<&String>, deserialize: &str) {
    let Some(rust::Type::Struct(struct_ty)) = types.get(name) else {
        return;
    };
    let Some(field) = struct_ty.fields.iter().find(|field| field.xml_namespace_prefix.is_some()) else {
        return;
    };
    let tag = field.xml_name.as_deref().unwrap_or(&field.camel_name).to_owned();
    let wrap = |content: &str| match root {
        Some(element) => format!("<{element}>{content}</{element}>"),
        None => content.to_owned(),
    };

    for (label, input) in [
        ("namespace attribute without the type attribute", wrap(&format!("<{tag}/>"))),
        (
            "namespace attribute with a malformed type attribute",
            wrap(&format!("<{tag} xsi:type=s3s/>")),
        ),
    ] {
        g!("    {{");
        g!("        let input = {input:?};");
        g!("        let err = {deserialize}::<dto::{name}>(input.as_bytes()).unwrap_err();");
        g!("        assert!(");
        g!(
            "            matches!(err, xml::DeError::MissingField | xml::DeError::InvalidAttribute | xml::DeError::InvalidXml(_)),"
        );
        g!("            \"{label}: {{err:?}}, input: {{input}}\"");
        g!("        );");
        g!("    }}");
    }
}

/// Error cases of a string enum: its content is plain text, so an element where
/// text is expected and an empty content are the two failures of the decoder, and
/// an undeclared value must still be preserved.
fn emit_str_enum_errors(name: &str, gate: Gate, enum_ty: &rust::StrEnum) -> usize {
    let mut unknown = "s3s-unknown-variant".to_owned();
    while enum_ty.variants.iter().any(|variant| variant.value == unknown) {
        unknown.push('x');
    }
    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_errors_{}() {{", name.to_snake_case());
    g!("    let input = \"<S3sUnknown>x</S3sUnknown>\";");
    g!("    let err = xml_deserialize_content::<dto::{name}>(input.as_bytes()).unwrap_err();");
    g!(
        "    assert!(matches!(err, xml::DeError::UnexpectedStart), \"element where text is expected: {{err:?}}, input: {{input}}\");"
    );
    g!("    let input = \"\";");
    g!("    let err = xml_deserialize_content::<dto::{name}>(input.as_bytes()).unwrap_err();");
    g!("    assert!(matches!(err, xml::DeError::UnexpectedEof), \"empty content: {{err:?}}\");");
    g!("    let input = {unknown:?};");
    g!("    let value = xml_deserialize_content::<dto::{name}>(input.as_bytes()).unwrap();");
    g!("    assert!(value.as_str() == input, \"an undeclared value must be preserved: input: {{input}}\");");
    g!("}}");
    g!();
    1
}

/// Error cases of a struct enum: its content is a single element whose name
/// selects the variant.
fn emit_struct_enum_errors(name: &str, gate: Gate, root: Option<&String>) -> usize {
    let (deserialize, unknown, empty) = match root {
        Some(element) => (
            "xml_deserialize",
            format!("<{element}><S3sUnknown>x</S3sUnknown></{element}>"),
            format!("<{element}></{element}>"),
        ),
        None => ("xml_deserialize_content", "<S3sUnknown>x</S3sUnknown>".to_owned(), String::new()),
    };
    if let Some(attr) = gate.attr() {
        g!("{attr}");
    }
    g!("#[test]");
    g!("fn xml_errors_{}() {{", name.to_snake_case());
    g!("    let input = {unknown:?};");
    g!("    let err = {deserialize}::<dto::{name}>(input.as_bytes()).unwrap_err();");
    g!("    assert!(matches!(err, xml::DeError::UnexpectedTagName), \"unknown element: {{err:?}}, input: {{input}}\");");
    g!("    let input = {empty:?};");
    g!("    let err = {deserialize}::<dto::{name}>(input.as_bytes()).unwrap_err();");
    g!("    assert!(matches!(err, xml::DeError::UnexpectedEnd), \"empty content: {{err:?}}\");");
    g!("}}");
    g!();
    1
}

/// One deserialization error probe.
struct Probe {
    label: &'static str,
    input: String,
    /// `None` means that this input must be accepted; otherwise the exact `DeError`
    /// variant the deserializer must return.
    expected: Option<&'static str>,
}

fn emit_probe(probe: &Probe, name: &str, deserialize: &str) {
    let input = &probe.input;
    g!("    {{");
    g!("        let input = {input:?};");
    if let Some(expected) = probe.expected {
        g!("        let err = {deserialize}::<dto::{name}>(input.as_bytes()).unwrap_err();");
        g!(
            "        assert!(matches!(err, xml::DeError::{expected}), \"{}: {{err:?}}, input: {{input}}\");",
            probe.label
        );
    } else {
        g!("        let ans = {deserialize}::<dto::{name}>(input.as_bytes());");
        g!("        if let Err(err) = ans {{");
        g!("            panic!(\"{}: {{err:?}}, input: {{input}}\");", probe.label);
        g!("        }}");
    }
    g!("    }}");
}

/// Whether the type is an alias of `String`, i.e. `Deserializer::text` is used for
/// the field and a child element is rejected with `UnexpectedStart`.
fn is_text_type(types: &RustTypes, name: &str) -> bool {
    matches!(types.get(name), Some(rust::Type::Alias(alias)) if alias.type_ == "String")
}

/// The content that makes the deserializer accept a single occurrence of an
/// element, so that a repeated one is reported as a duplicate.
///
/// Returns None when no such content can be derived from the model.
fn minimal_content(types: &RustTypes, name: &str) -> Option<String> {
    match types.get(name)? {
        rust::Type::Alias(alias) => match alias.type_.as_str() {
            "String" => Some(SAMPLE_TEXT.to_owned()),
            "bool" => Some("true".to_owned()),
            "i32" | "i64" => Some("42".to_owned()),
            "f32" | "f64" => Some("1.5".to_owned()),
            "ETagCondition" => None,
            other => minimal_content(types, other),
        },
        rust::Type::Provided(provided) => match provided.name.as_str() {
            "ETag" | "Event" => Some(SAMPLE_TEXT.to_owned()),
            "ObjectUserMetadata" => Some(format!("<{SAMPLE_TEXT}>{SAMPLE_TEXT}</{SAMPLE_TEXT}>")),
            _ => None,
        },
        rust::Type::Timestamp(timestamp) => Some(
            match timestamp.format.as_deref().unwrap_or("DateTime") {
                "HttpDate" => "Tue, 02 Jan 2024 03:04:05 GMT",
                "EpochSeconds" => "1704164645",
                _ => "2024-01-02T03:04:05.000Z",
            }
            .to_owned(),
        ),
        rust::Type::StrEnum(enum_ty) => Some(enum_ty.variants.first()?.value.clone()),
        // An empty element is accepted by a list and by a shape without required
        // fields; a shape with required fields needs them filled in recursively.
        rust::Type::List(_) => Some(String::new()),
        rust::Type::Struct(struct_ty) => struct_content(types, struct_ty, 0),
        rust::Type::StructEnum(enum_ty) => struct_enum_content(types, enum_ty, 0),
        rust::Type::Map(_) => None,
    }
}

/// The content that makes the deserializer accept one occurrence of a struct
/// element: every required field of the model, recursively.
///
/// Optional fields, lists, maps and fields with a default value are left out:
/// the deserializer fills them without reading an element.
fn struct_content(types: &RustTypes, struct_ty: &rust::Struct, depth: u32) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut out = String::new();
    for field in &struct_ty.fields {
        if field.position != "xml" || field.is_xml_attr {
            continue;
        }
        if field.option_type || field.default_value.is_some() {
            continue;
        }
        let name = field.xml_name.clone().unwrap_or_else(|| field.camel_name.clone());
        let content = minimal_content(types, &field.type_)?;
        out.push_str(&element_with_attributes(&name, &field_attributes(field), &content));
    }
    Some(out)
}

/// The namespace attributes of a field whose type is carried by an `xsi:type`
/// attribute: the deserializer reads the attribute from the start tag of the
/// element and requires it, so a probe without it fails with `MissingField`.
fn field_attributes(field: &rust::StructField) -> String {
    match (&field.xml_namespace_uri, &field.xml_namespace_prefix) {
        (Some(uri), Some(prefix)) => format!(" xmlns:{prefix}=\"{uri}\" {prefix}:type=\"{SAMPLE_TEXT}\""),
        _ => String::new(),
    }
}

fn element_with_attributes(name: &str, attributes: &str, content: &str) -> String {
    format!("<{name}{attributes}>{content}</{name}>")
}

/// The content of a struct enum: its first variant, named by the variant's XML
/// name, carrying the required fields of the variant payload.
fn struct_enum_content(types: &RustTypes, enum_ty: &rust::StructEnum, depth: u32) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let variant = enum_ty.variants.first()?;
    let name = variant.xml_name.clone().unwrap_or_else(|| variant.name.clone());
    let content = minimal_content(types, &variant.type_)?;
    Some(xml_element(&name, &content))
}

fn xml_element(name: &str, content: &str) -> String {
    format!("<{name}>{content}</{name}>")
}

/// Whether the field content is parsed from text and a malformed value is
/// rejected with `InvalidContent` (integer, boolean or timestamp field).
fn is_scalar_type(types: &RustTypes, name: &str) -> bool {
    match types.get(name) {
        Some(rust::Type::Alias(alias)) => matches!(alias.type_.as_str(), "bool" | "i32" | "i64"),
        Some(rust::Type::Timestamp(_)) => true,
        _ => false,
    }
}

/// The tag names the serializer must produce for the sample value.
fn expected_fragments(types: &RustTypes, ty: &rust::Type) -> Vec<String> {
    let rust::Type::Struct(struct_ty) = ty else {
        return Vec::new();
    };
    let mut fragments = Vec::new();
    for field in &struct_ty.fields {
        if field.position != "xml" || field.is_xml_attr {
            continue;
        }
        let tag = field.xml_name.as_deref().unwrap_or(&field.camel_name);
        if field.xml_namespace_uri.is_some() {
            fragments.push(format!("<{tag} "));
            continue;
        }
        let fragment = match types.get(&field.type_) {
            Some(rust::Type::Alias(alias)) => match alias.type_.as_str() {
                "String" => format!("<{tag}>{SAMPLE_TEXT}</{tag}>"),
                "bool" => format!("<{tag}>true</{tag}>"),
                "i32" | "i64" => format!("<{tag}>42</{tag}>"),
                "f32" | "f64" => format!("<{tag}>1.5</{tag}>"),
                _ => format!("<{tag}>"),
            },
            Some(rust::Type::StrEnum(enum_ty)) => match enum_ty.variants.first() {
                Some(variant) => format!("<{tag}>{}</{tag}>", variant.value),
                None => format!("<{tag}>"),
            },
            _ => format!("<{tag}>"),
        };
        fragments.push(fragment);
    }
    fragments
}

/// Builds sample values from the model data.
struct Values<'a> {
    types: &'a RustTypes,
}

impl Values<'_> {
    fn expr(&self, name: &str, depth: u32) -> Result<String, String> {
        if depth > MAX_DEPTH {
            return Err(format!("deeper than {MAX_DEPTH} levels"));
        }
        let Some(ty) = self.types.get(name) else {
            return Err(format!("unknown type {name}"));
        };
        match ty {
            rust::Type::Alias(alias) => match alias.type_.as_str() {
                "String" => Ok(format!("String::from({SAMPLE_TEXT:?})")),
                "bool" => Ok("true".to_owned()),
                "i32" | "i64" => Ok("42".to_owned()),
                "f32" | "f64" => Ok("1.5".to_owned()),
                "ETagCondition" => Ok(format!("dto::ETagCondition::ETag(dto::ETag::Strong(String::from({SAMPLE_TEXT:?})))")),
                other => self.expr(other, depth + 1),
            },
            rust::Type::Provided(provided) => match provided.name.as_str() {
                "ETag" => Ok(format!("dto::ETag::Strong(String::from({SAMPLE_TEXT:?}))")),
                "Event" => Ok(format!("dto::Event::from(String::from({SAMPLE_TEXT:?}))")),
                "ObjectUserMetadata" => Ok(format!(
                    "dto::ObjectUserMetadata(vec![(String::from({SAMPLE_TEXT:?}), String::from({SAMPLE_TEXT:?}))])"
                )),
                other => Err(format!("no sample value for the provided type {other}")),
            },
            rust::Type::Timestamp(timestamp) => {
                let format = timestamp.format.as_deref().unwrap_or("DateTime");
                let sample = match format {
                    "HttpDate" => "Tue, 02 Jan 2024 03:04:05 GMT",
                    "EpochSeconds" => "1704164645",
                    _ => "2024-01-02T03:04:05.000Z",
                };
                Ok(format!("dto::Timestamp::parse(dto::TimestampFormat::{format}, {sample:?}).unwrap()"))
            }
            rust::Type::List(list_ty) => {
                let member = self.expr(&list_ty.member.type_, depth + 1)?;
                Ok(format!("vec![{member}]"))
            }
            rust::Type::StrEnum(enum_ty) => {
                let variant = enum_ty.variants.first().ok_or_else(|| format!("{name} has no variant"))?;
                Ok(format!("dto::{name}::from_static({:?})", variant.value))
            }
            rust::Type::StructEnum(enum_ty) => {
                let variant = enum_ty.variants.first().ok_or_else(|| format!("{name} has no variant"))?;
                let inner = self.expr(&variant.type_, depth + 1)?;
                Ok(format!("dto::{name}::{}({inner})", variant.name))
            }
            rust::Type::Struct(struct_ty) => self.struct_expr(struct_ty, depth),
            rust::Type::Map(_) => Ok("core::default::Default::default()".to_owned()),
        }
    }

    /// Every field is set explicitly, because the generated structs do not all
    /// derive Default. Optional fields outside the XML body stay None.
    fn struct_expr(&self, struct_ty: &rust::Struct, depth: u32) -> Result<String, String> {
        let mut fields = String::new();
        for field in &struct_ty.fields {
            // A sealed field is neither serialized nor parsed: the generated
            // deserializer fills it with Default::default().
            if field.position == "sealed" {
                write!(fields, "{}: dto::{}::default(),", field.name, field.type_).expect("writing to a String cannot fail");
                continue;
            }
            if field.option_type {
                if field.position == "xml" {
                    let value = self.expr(&field.type_, depth + 1)?;
                    write!(fields, "{}: Some({value}),", field.name).expect("writing to a String cannot fail");
                } else {
                    write!(fields, "{}: None,", field.name).expect("writing to a String cannot fail");
                }
            } else {
                let value = self
                    .expr(&field.type_, depth + 1)
                    .map_err(|reason| format!("field {}: {reason}", field.name))?;
                write!(fields, "{}: {value},", field.name).expect("writing to a String cannot fail");
            }
        }
        Ok(format!("dto::{} {{ {fields} }}", struct_ty.name))
    }

    /// The value that leaves every optional field unset and fills the required
    /// ones, i.e. the value whose serialization must skip every optional element.
    ///
    /// The generated structs do not all implement `Default`, so the value is built
    /// from the model like every other sample.
    fn minimal_expr(&self, name: &str, depth: u32) -> Result<String, String> {
        let Some(rust::Type::Struct(struct_ty)) = self.types.get(name) else {
            return self.expr(name, depth);
        };
        if depth > MAX_DEPTH {
            return Err(format!("deeper than {MAX_DEPTH} levels"));
        }
        let mut fields = String::new();
        for field in &struct_ty.fields {
            if field.position == "sealed" {
                write!(fields, "{}: dto::{}::default(),", field.name, field.type_).expect("writing to a String cannot fail");
                continue;
            }
            if field.option_type {
                write!(fields, "{}: None,", field.name).expect("writing to a String cannot fail");
            } else {
                let value = self
                    .expr(&field.type_, depth + 1)
                    .map_err(|reason| format!("field {}: {reason}", field.name))?;
                write!(fields, "{}: {value},", field.name).expect("writing to a String cannot fail");
            }
        }
        Ok(format!("dto::{} {{ {fields} }}", struct_ty.name))
    }
}

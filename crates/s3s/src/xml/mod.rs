//! XML serialization and deserialization for S3 request and response bodies.
//!
//! This module provides the [`Serialize`] / [`SerializeContent`] traits and
//! the corresponding [`Deserialize`] / [`DeserializeContent`] traits, together
//! with [`Serializer`] and [`Deserializer`] implementations used to convert
//! between Rust DTOs and the XML wire format required by the S3 REST API.

#![allow(clippy::missing_errors_doc)] // TODO

mod de;
pub use self::de::*;

mod ser;
pub use self::ser::*;

#[cfg(feature = "minio")]
mod generated_minio;

#[cfg(not(feature = "minio"))]
mod generated;

mod manually {
    use super::*;

    use crate::dto::BucketLocationConstraint;
    use crate::dto::GetBucketLocationOutput;

    impl Serialize for GetBucketLocationOutput {
        fn serialize<W: std::io::Write>(&self, s: &mut Serializer<W>) -> SerResult {
            let xmlns = "http://s3.amazonaws.com/doc/2006-03-01/";
            if let Some(location_constraint) = &self.location_constraint {
                s.content_with_ns("LocationConstraint", xmlns, location_constraint)?;
            } else {
                s.content_with_ns("LocationConstraint", xmlns, "")?;
            }
            Ok(())
        }
    }

    impl<'xml> Deserialize<'xml> for GetBucketLocationOutput {
        fn deserialize(d: &mut Deserializer<'xml>) -> DeResult<Self> {
            let mut location_constraint: Option<BucketLocationConstraint> = None;
            d.for_each_element(|d, x| match x {
                b"LocationConstraint" => {
                    if location_constraint.is_some() {
                        return Err(DeError::DuplicateField);
                    }
                    let val: BucketLocationConstraint = d.content()?;
                    if !val.as_str().is_empty() {
                        location_constraint = Some(val);
                    }
                    Ok(())
                }
                _ => Err(DeError::UnexpectedTagName),
            })?;
            Ok(Self { location_constraint })
        }
    }

    use crate::dto::AssumeRoleOutput;

    impl Serialize for AssumeRoleOutput {
        fn serialize<W: std::io::Write>(&self, s: &mut Serializer<W>) -> SerResult {
            let xmlns = "https://sts.amazonaws.com/doc/2011-06-15/";
            s.element_with_ns("AssumeRoleResponse", xmlns, |s| {
                s.content("AssumeRoleResult", self) //
            })?;
            Ok(())
        }
    }

    impl<'xml> Deserialize<'xml> for AssumeRoleOutput {
        fn deserialize(d: &mut Deserializer<'xml>) -> DeResult<Self> {
            d.named_element("AssumeRoleResponse", |d| {
                d.named_element("AssumeRoleResult", Self::deserialize_content) //
            })
        }
    }

    use crate::dto::ETag;
    use crate::dto::ParseETagError;

    use stdx::default::default;

    impl SerializeContent for ETag {
        fn serialize_content<W: std::io::Write>(&self, s: &mut Serializer<W>) -> SerResult {
            let val = self.value();
            if val.len() <= 64 {
                let mut buf: arrayvec::ArrayString<72> = default();
                buf.push('"');
                buf.push_str(val);
                buf.push('"');
                s.write_raw_text(buf.as_str())
            } else {
                s.write_raw_text(&format!("\"{val}\""))
            }
        }
    }

    impl<'xml> DeserializeContent<'xml> for ETag {
        fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
            let val: String = d.content()?;

            // try to parse as quoted ETag first
            // fallback if the ETag is not quoted
            match ETag::parse_http_header(val.as_bytes()) {
                Ok(v) => Ok(v),
                Err(ParseETagError::InvalidFormat) => Ok(ETag::Strong(val)),
                Err(ParseETagError::InvalidChar) => Err(DeError::InvalidContent),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::BucketVersioningStatus;
    use crate::dto::ETag;
    use std::io::Cursor;

    #[test]
    fn etag_xml_serialization_uses_literal_quotes_not_entities() {
        let etag = ETag::Strong("b264846671938cd88cd6121b3171589b".to_string());
        let mut buf = Vec::new();
        let mut ser = Serializer::new(Cursor::new(&mut buf));
        ser.element("ETag", |s| etag.serialize_content(s)).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        assert!(
            xml.contains("\"b264846671938cd88cd6121b3171589b\""),
            "ETag must be serialized with literal quotes for S3; got: {xml}"
        );
        assert!(!xml.contains("&quot;"), "ETag must not use HTML entity encoding; got: {xml}");
    }

    #[test]
    fn etag_xml_serialization_long_value_uses_literal_quotes() {
        let long_hash = "a".repeat(65);
        let etag = ETag::Strong(long_hash.clone());
        let mut buf = Vec::new();
        let mut ser = Serializer::new(Cursor::new(&mut buf));
        ser.element("ETag", |s| etag.serialize_content(s)).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        let expected = format!("\"{long_hash}\"");
        assert!(xml.contains(&expected), "Long ETag must use literal quotes; got: {xml}");
        assert!(!xml.contains("&quot;"), "Long ETag must not use &quot;; got: {xml}");
    }

    #[test]
    fn create_session_output_xml_serialization() {
        use crate::dto::{CreateSessionOutput, SessionCredentials, Timestamp, TimestampFormat};

        let creds = SessionCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            expiration: Timestamp::parse(TimestampFormat::DateTime, "2024-01-01T00:05:00.000Z").unwrap(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: "FwoGZXIvYXdzEBYaDHqa0A".to_owned(),
        };

        let output = CreateSessionOutput {
            credentials: creds,
            ..Default::default()
        };

        let mut buf = Vec::new();
        let mut ser = Serializer::new(Cursor::new(&mut buf));
        output.serialize(&mut ser).unwrap();
        let xml = String::from_utf8(buf).unwrap();

        assert!(xml.contains("CreateSessionResult"), "root element must be CreateSessionResult: {xml}");
        assert!(xml.contains("<Credentials>"), "must contain Credentials element: {xml}");
        assert!(
            xml.contains("<AccessKeyId>AKIAIOSFODNN7EXAMPLE</AccessKeyId>"),
            "must contain AccessKeyId: {xml}"
        );
        assert!(xml.contains("<SecretAccessKey>"), "must contain SecretAccessKey: {xml}");
        assert!(xml.contains("<SessionToken>"), "must contain SessionToken: {xml}");
        assert!(xml.contains("<Expiration>"), "must contain Expiration: {xml}");
    }

    // ---------------------------------------------------------------------------
    // XML deserialization entity resolution tests
    // ---------------------------------------------------------------------------

    /// Helper: deserialize `<Root>{content}</Root>` as a `String`.
    fn deser_root_content(xml: &[u8]) -> DeResult<String> {
        let mut d = Deserializer::new(xml);
        d.named_element("Root", Deserializer::content)
    }

    #[test]
    fn deser_plain_text() {
        let xml = b"<Root>hello</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "hello");
    }

    #[test]
    fn deser_empty_element() {
        let xml = b"<Root></Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "");
    }

    #[test]
    fn deser_quot_entity() {
        let xml = b"<Root>a&quot;b</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "a\"b");
    }

    #[test]
    fn deser_amp_entity() {
        let xml = b"<Root>a&amp;b</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "a&b");
    }

    #[test]
    fn deser_lt_entity() {
        let xml = b"<Root>a&lt;b</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "a<b");
    }

    #[test]
    fn deser_gt_entity() {
        let xml = b"<Root>a&gt;b</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "a>b");
    }

    #[test]
    fn deser_apos_entity() {
        let xml = b"<Root>a&apos;b</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "a'b");
    }

    #[test]
    fn deser_all_entities_sequence() {
        let xml = b"<Root>&quot;&amp;&lt;&gt;&apos;</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "\"&<>'");
    }

    #[test]
    fn deser_mixed_text_and_entities() {
        let xml = b"<Root>foo&quot;bar&amp;baz&lt;qux&gt;end&apos;s</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "foo\"bar&baz<qux>end's");
    }

    #[test]
    fn deser_entity_at_start() {
        let xml = b"<Root>&quot;hello</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "\"hello");
    }

    #[test]
    fn deser_entity_at_end() {
        let xml = b"<Root>hello&quot;</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "hello\"");
    }

    #[test]
    fn deser_only_entity() {
        let xml = b"<Root>&amp;</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "&");
    }

    #[test]
    fn deser_unknown_entity_returns_error() {
        let xml = b"<Root>&unknown;</Root>";
        let err = deser_root_content(xml).unwrap_err();
        assert!(matches!(err, DeError::InvalidContent), "expected InvalidContent, got {err:?}");
    }

    #[test]
    fn deser_consecutive_entities() {
        let xml = b"<Root>&quot;&quot;&amp;&amp;</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "\"\"&&");
    }

    #[test]
    fn deser_entity_with_leading_trailing_text() {
        let xml = b"<Root>before &lt;middle&gt; after</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "before <middle> after");
    }

    // ---------------------------------------------------------------------------
    // Direct `text()` method tests — exercising edge cases without the
    // `named_element` wrapper.
    // ---------------------------------------------------------------------------

    /// Directly call `text()` on raw bytes and return the emitted `String`.
    /// This bypasses `expect_start` / `expect_end` so we can isolate `text()`.
    fn text_direct(xml: &[u8]) -> DeResult<String> {
        let mut d = Deserializer::new(xml);
        d.text(|s| Ok(s.to_owned()))
    }

    #[test]
    fn text_direct_plain() {
        let xml = b"hello world";
        assert_eq!(text_direct(xml).unwrap(), "hello world");
    }

    #[test]
    fn text_direct_eof_no_content() {
        // Empty input → text() peeks and sees EOF immediately → UnexpectedEof
        let xml = b"";
        let err = text_direct(xml).unwrap_err();
        assert!(matches!(err, DeError::UnexpectedEof), "expected UnexpectedEof, got {err:?}");
    }

    #[test]
    fn text_direct_eof_after_text() {
        // Text followed by EOF (no end tag) → returns accumulated text.
        // This is the truncated-XML case.
        let xml = b"partial content";
        assert_eq!(text_direct(xml).unwrap(), "partial content");
    }

    #[test]
    fn text_direct_start_event_immediate() {
        // When text() encounters a Start event as the very first event,
        // it errors with UnexpectedStart.
        let xml = b"<Root>content</Root>";
        let err = text_direct(xml).unwrap_err();
        assert!(matches!(err, DeError::UnexpectedStart), "expected UnexpectedStart, got {err:?}");
    }

    #[test]
    fn text_direct_start_event_inside_content() {
        // When text() encounters a Start event, it errors after consuming it.
        let xml = b"before<Child>inside</Child>";
        let err = text_direct(xml).unwrap_err();
        assert!(matches!(err, DeError::UnexpectedStart), "expected UnexpectedStart, got {err:?}");
    }

    #[test]
    fn text_direct_only_entity() {
        // Single entity without surrounding text
        let xml = b"&amp;";
        assert_eq!(text_direct(xml).unwrap(), "&");
    }

    #[test]
    fn text_direct_multiple_entities() {
        // Consecutive entities without plain text between them
        let xml = b"&lt;&amp;&gt;";
        assert_eq!(text_direct(xml).unwrap(), "<&>");
    }

    #[test]
    fn text_direct_text_across_multiple_events() {
        // Text → Entity → Text → Entity → Text
        // This validates the accumulation loop correctly handles all patterns.
        let xml = b"start&lt;middle&amp;end&gt;finish";
        assert_eq!(text_direct(xml).unwrap(), "start<middle&end>finish");
    }

    #[test]
    fn text_direct_leading_entity() {
        let xml = b"&quot;trailing text";
        assert_eq!(text_direct(xml).unwrap(), "\"trailing text");
    }

    #[test]
    fn text_direct_trailing_entity() {
        let xml = b"leading text&quot;";
        assert_eq!(text_direct(xml).unwrap(), "leading text\"");
    }

    // ---------------------------------------------------------------------------
    // Whitespace, newlines, and numeric character references (via named_element).
    // ---------------------------------------------------------------------------

    #[test]
    fn deser_preserves_whitespace() {
        let xml = b"<Root>  leading  middle  trailing  </Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "  leading  middle  trailing  ");
    }

    #[test]
    fn deser_preserves_newlines() {
        let xml = b"<Root>line1\nline2\r\nline3</Root>";
        let result = deser_root_content(xml).unwrap();
        assert_eq!(result, "line1\nline2\r\nline3");
    }

    #[test]
    fn deser_tab_characters() {
        let xml = b"<Root>\tindented\t</Root>";
        assert_eq!(deser_root_content(xml).unwrap(), "\tindented\t");
    }

    #[test]
    fn deser_numeric_char_ref_resolved() {
        // Decimal and hexadecimal character references should be resolved.
        assert_eq!(deser_root_content(b"<Root>&#65;</Root>").unwrap(), "A");
        assert_eq!(deser_root_content(b"<Root>&#x41;</Root>").unwrap(), "A");
        assert_eq!(deser_root_content(b"<Root>&#34;</Root>").unwrap(), "\"");
        assert_eq!(deser_root_content(b"<Root>&#9;</Root>").unwrap(), "\t");
        // Invalid numeric refs still rejected
        assert!(deser_root_content(b"<Root>&#xDEADBEEF;</Root>").is_err());
        assert!(deser_root_content(b"<Root>&#;</Root>").is_err());
    }

    #[test]
    fn deser_xml_declaration_is_ignored() {
        // XML declaration should not interfere with content extraction
        let xml = b"<?xml version=\"1.0\"?><Root>value</Root>";
        let mut d = Deserializer::new(xml);
        let result: String = d.named_element("Root", Deserializer::content).unwrap();
        assert_eq!(result, "value");
    }

    #[test]
    fn deser_empty_element_tag() {
        // <Empty/> is translated to <Empty></Empty> by read_event()
        let xml = b"<Root><Empty/></Root>";
        let mut d = Deserializer::new(xml);
        let result: String = d
            .named_element("Root", |d| d.named_element("Empty", Deserializer::content))
            .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn deser_s3_etag_roundtrip_like() {
        // Round-trip like: ETag value that would be serialized with &quot; entities
        let xml = b"<ETag>&quot;b264846671938cd88cd6121b3171589b&quot;</ETag>";
        let mut d = Deserializer::new(xml);
        let etag_str: String = d.named_element("ETag", Deserializer::content).unwrap();
        assert_eq!(etag_str, "\"b264846671938cd88cd6121b3171589b\"");

        // Re-serialize and verify it round-trips correctly
        let etag = ETag::Strong(etag_str);
        let mut buf = Vec::new();
        let mut ser = Serializer::new(Cursor::new(&mut buf));
        ser.element("ETag", |s| etag.serialize_content(s)).unwrap();
        let xml_out = String::from_utf8(buf).unwrap();
        // ETag serialization should use literal quotes, not entities
        assert!(xml_out.contains("\"b264846671938cd88cd6121b3171589b\""));
        assert!(!xml_out.contains("&quot;"));
    }

    // ---------------------------------------------------------------------------
    // named_element_any tests
    // ---------------------------------------------------------------------------

    /// Helper: deserialize `<Root>{content}</Root>` accepting either `Root` or `RootAlt`.
    fn deser_root_any(xml: &[u8]) -> DeResult<String> {
        let mut d = Deserializer::new(xml);
        d.named_element_any(&["Root", "RootAlt"], Deserializer::content)
    }

    #[test]
    fn named_element_any_matches_first_candidate() {
        let xml = b"<Root>hello</Root>";
        assert_eq!(deser_root_any(xml).unwrap(), "hello");
    }

    #[test]
    fn named_element_any_matches_second_candidate() {
        let xml = b"<RootAlt>world</RootAlt>";
        assert_eq!(deser_root_any(xml).unwrap(), "world");
    }

    #[test]
    fn named_element_any_rejects_unexpected_name() {
        let xml = b"<Unknown>content</Unknown>";
        assert!(deser_root_any(xml).is_err());
    }

    #[test]
    fn named_element_any_rejects_mismatched_end_tag() {
        let xml = b"<Root>content</RootAlt>";
        assert!(deser_root_any(xml).is_err());
    }

    #[test]
    fn named_element_any_with_empty_content() {
        let xml = b"<Root></Root>";
        assert_eq!(deser_root_any(xml).unwrap(), "");
    }

    #[test]
    fn named_element_any_single_candidate() {
        let mut d = Deserializer::new(b"<Only>42</Only>");
        let result: String = d.named_element_any(&["Only"], Deserializer::content).unwrap();
        assert_eq!(result, "42");
    }

    #[test]
    fn named_element_any_duplicate_candidates() {
        let mut d = Deserializer::new(b"<Root>dup</Root>");
        let result: String = d
            .named_element_any(&["Root", "Root", "RootAlt"], Deserializer::content)
            .unwrap();
        assert_eq!(result, "dup");
    }

    #[test]
    fn named_element_any_real_world_tag_names() {
        let mut d = Deserializer::new(b"<LifecycleConfiguration>lc</LifecycleConfiguration>");
        let result: String = d
            .named_element_any(&["BucketLifecycleConfiguration", "LifecycleConfiguration"], Deserializer::content)
            .unwrap();
        assert_eq!(result, "lc");
    }

    // ---------------------------------------------------------------------------
    // Unknown XML element — top-level request type forward-compatibility tests
    // ---------------------------------------------------------------------------

    /// Top-level struct `VersioningConfiguration` ignores unknown elements.
    #[test]
    fn deser_top_level_ignores_unknown_element() {
        use crate::dto::VersioningConfiguration;

        let xml = br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <Status>Enabled</Status>
            <NewFeatureFromFuture>some-value</NewFeatureFromFuture>
        </VersioningConfiguration>"#;

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
    }

    /// Top-level struct ignores unknown element before known fields.
    #[test]
    fn deser_top_level_ignores_unknown_element_before_known() {
        use crate::dto::VersioningConfiguration;

        let xml = br"<VersioningConfiguration>
            <FutureExtension>ignored</FutureExtension>
            <Status>Suspended</Status>
        </VersioningConfiguration>";

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Suspended"));
    }

    /// Top-level struct ignores deeply nested unknown elements.
    #[test]
    fn deser_top_level_ignores_nested_unknown_element() {
        use crate::dto::VersioningConfiguration;

        let xml = br"<VersioningConfiguration>
            <Status>Enabled</Status>
            <NewComplex>
                <Child1>a</Child1>
                <Child2><GrandChild>b</GrandChild></Child2>
            </NewComplex>
        </VersioningConfiguration>";

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
    }

    /// Top-level struct ignores empty self-closing unknown element.
    #[test]
    fn deser_top_level_ignores_empty_unknown_element() {
        use crate::dto::VersioningConfiguration;

        let xml = br"<VersioningConfiguration>
            <Status>Enabled</Status>
            <EmptyFutureTag/>
        </VersioningConfiguration>";

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
    }

    /// Unknown elements in a list wrapper are still skipped (`list_content` leniency).
    #[test]
    fn deser_list_ignores_unknown_element() {
        use crate::dto::Tagging;

        // TagSet contains Tag members; an extra unknown element should be ignored.
        let xml = br"<Tagging>
            <TagSet>
                <Tag><Key>k1</Key><Value>v1</Value></Tag>
                <UnknownTag><Data>x</Data></UnknownTag>
                <Tag><Key>k2</Key><Value>v2</Value></Tag>
            </TagSet>
        </Tagging>";

        let mut d = Deserializer::new(xml);
        let result = Tagging::deserialize(&mut d).unwrap();
        let tags = &result.tag_set;
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].key.as_deref(), Some("k1"));
        assert_eq!(tags[1].key.as_deref(), Some("k2"));
    }

    /// Top-level struct ignores multiple scattered unknown elements.
    #[test]
    fn deser_top_level_ignores_multiple_unknown_elements() {
        use crate::dto::VersioningConfiguration;

        let xml = br"<VersioningConfiguration>
            <Alpha>1</Alpha>
            <Status>Enabled</Status>
            <Beta><X>2</X></Beta>
            <Gamma/>
        </VersioningConfiguration>";

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
    }

    /// Known-field name inside an unknown parent: the top-level skip swallows
    /// the entire unknown block including nested known-named elements.
    #[test]
    fn deser_top_level_ignores_known_name_nested_in_unknown_parent() {
        use crate::dto::VersioningConfiguration;

        let xml = br"<VersioningConfiguration>
            <Status>Enabled</Status>
            <Unknown>
                <Status>Skipped</Status>
                <MFADelete>Skipped</MFADelete>
            </Unknown>
        </VersioningConfiguration>";

        let mut d = Deserializer::new(xml);
        let result = VersioningConfiguration::deserialize(&mut d).unwrap();
        assert_eq!(result.status.as_ref().map(BucketVersioningStatus::as_str), Some("Enabled"));
        // The inner <Status> and <MFADelete> must be swallowed, not parsed.
        assert!(result.mfa_delete.is_none(), "MFADelete inside <Unknown> should be skipped: {result:#?}");
    }
}

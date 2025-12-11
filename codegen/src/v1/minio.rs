use scoped_writer::g;

use super::smithy;

/// <https://github.com/Nugine/s3s/issues/192>
pub fn patch(model: &mut smithy::Model) {
    let patches = smithy::Model::load_json("data/minio-patches.json").unwrap();

    for (shape_name, patch) in patches.shapes {
        match model.shapes.get_mut(&shape_name) {
            None => {
                model.shapes.insert(shape_name, patch);
            }
            Some(shape) => match shape {
                smithy::Shape::Structure(shape) => {
                    let smithy::Shape::Structure(patch) = patch else { panic!() };
                    for (field_name, member) in patch.members {
                        assert!(shape.members.insert(field_name, member).is_none());
                    }
                }
                _ => unimplemented!(),
            },
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn codegen_in_dto() {
    let code = r#"

#[derive(Debug, Default)]
pub struct CachedTags(std::sync::OnceLock<Map<ObjectKey, Value>>);

impl Clone for CachedTags {
    fn clone(&self) -> Self {
        // CachedTags is a cache, so we create a new empty cache when cloning
        Self::default()
    }
}

impl PartialEq for CachedTags {
    fn eq(&self, _other: &Self) -> bool {
        // CachedTags is a cache, consider all instances equal for comparison purposes
        true
    }
}

impl serde::Serialize for CachedTags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // CachedTags is just a cache, serialize as empty map
        use serde::ser::SerializeMap;
        let map = serializer.serialize_map(Some(0))?;
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for CachedTags {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize and ignore the data, return default (empty cache)
        use serde::de::{Visitor, MapAccess};
        struct CachedTagsVisitor;
        
        impl<'de> Visitor<'de> for CachedTagsVisitor {
            type Value = CachedTags;
            
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map")
            }
            
            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                // Consume and discard all map entries
                while access.next_entry::<String, String>()?.is_some() {}
                Ok(CachedTags::default())
            }
        }
        
        deserializer.deserialize_map(CachedTagsVisitor)
    }
}

impl CachedTags {
    pub fn reset(&mut self) {
        self.0.take();
    }

    fn test<'a>(
        &self,
        get_and_tags: impl FnOnce() -> Option<&'a [Tag]>,
        get_tag: impl FnOnce() -> Option<&'a Tag>,
        object_tags: &Map<String, String>,
    ) -> bool {
        let cached_tags = self.0.get_or_init(|| {
            let mut map = Map::new();

            if let Some(tags) = get_and_tags() {
                for tag in tags {
                    let (Some(k), Some(v)) = (&tag.key, &tag.value) else {continue};
                    if !k.is_empty() {
                        map.insert(k.clone(), v.clone());
                    }
                }
            }

            if let Some(tag) = get_tag() {
                let (Some(k), Some(v)) = (&tag.key, &tag.value) else { return map };
                if !k.is_empty() {
                    map.insert(k.clone(), v.clone());
                }
            }

            map
        });

        if cached_tags.is_empty() {
            return true;
        }

        if object_tags.is_empty() {
            return false;
        }

        let (mut lhs, mut rhs) = (cached_tags, object_tags);
        if lhs.len() > rhs.len() {
            std::mem::swap(&mut lhs, &mut rhs);
        }

        for (k, v) in lhs {
            if rhs.get(k) == Some(v) {
                return true;
            }
        }

        false
    }
}

impl super::LifecycleRuleFilter {
    pub fn test_tags(&self, object_tags: &Map<String, String>) -> bool {
        self.cached_tags.test(
            || self.and.as_ref().and_then(|and| and.tags.as_deref()),
            || self.tag.as_ref(),
            object_tags,
        )
    }
}

impl super::ReplicationRuleFilter {
    pub fn test_tags(&self, object_tags: &Map<String, String>) -> bool {
        self.cached_tags.test(
            || self.and.as_ref().and_then(|and| and.tags.as_deref()),
            || self.tag.as_ref(),
            object_tags,
        )
    }
}

#[cfg(test)]
mod minio_tests {
    use super::*;

    use std::ops::Not;

    #[test]
    fn cached_tags() {
        let filter = ReplicationRuleFilter {
            and: Some(ReplicationRuleAndOperator {
                tags: Some(vec![
                    Tag {
                        key: Some("key1".to_owned()),
                        value: Some("value1".to_owned()),
                    },
                    Tag {
                        key: Some("key2".to_owned()),
                        value: Some("value2".to_owned()),
                    },
                ]),
                ..default()
            }),
            tag: Some(Tag {
                key: Some("key3".to_owned()),
                value: Some("value3".to_owned()),
            }),
            ..default()
        };

        let object_tags = Map::from_iter(vec![
            ("key1".to_owned(), "value1".to_owned()),
            ("key4".to_owned(), "value4".to_owned()),
            ("key5".to_owned(), "value5".to_owned()),
        ]);

        assert!(filter.test_tags(&object_tags));
        assert!(filter.test_tags(&object_tags));
        assert!(filter.test_tags(&object_tags));

        let object_tags = Map::from_iter(vec![
            ("key4".to_owned(), "value4".to_owned()),
            ("key5".to_owned(), "value5".to_owned()),
        ]);
        assert!(filter.test_tags(&object_tags).not());
        assert!(filter.test_tags(&object_tags).not());
        assert!(filter.test_tags(&object_tags).not());
    }
}

"#;
    g!("{code}");
}

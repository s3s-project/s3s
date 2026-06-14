use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use numeric_cast::numeric_cast;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Deserialize)]
pub struct Model {
    pub smithy: String,
    pub shapes: BTreeMap<String, Shape>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Shape {
    Boolean(BooleanShape),
    Integer(IntegerShape),
    Long(LongShape),
    String(StringShape),
    Timestamp(TimestampShape),
    Blob(BlobShape),
    List(ListShape),
    Map(MapShape),
    Enum(EnumShape),
    Union(UnionShape),
    Structure(StructureShape),
    Operation(OperationShape),
    Service(ServiceShape),
}

#[derive(Debug, Deserialize, Default)]
#[serde(transparent)]
pub struct Traits(Option<Map<String, Value>>);

#[derive(Debug, Deserialize)]
pub struct StringShape {
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct TimestampShape {
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct StructureShape {
    pub members: BTreeMap<String, StructureMember>,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct StructureMember {
    pub target: String,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct OperationShape {
    pub input: OperationInput,
    pub output: OperationOutput,
    // pub errors: Option<Vec<OperationError>>,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct OperationInput {
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct OperationOutput {
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct OperationError {
    // pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct BooleanShape {
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct ListShape {
    pub member: ListMember,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct ListMember {
    pub target: String,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct ServiceShape {
    // pub version: String,
    // pub operations: Vec<ServiceOperation>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceOperation {
    // pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct UnionShape {
    pub members: BTreeMap<String, UnionMember>,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct UnionMember {
    pub target: String,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct EnumShape {
    pub members: BTreeMap<String, EnumMember>,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct EnumMember {
    // pub target: String,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct BlobShape {
    // pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct LongShape {
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct IntegerShape {
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct MapShape {
    pub key: MapKey,
    pub value: MapValue,
    pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct MapKey {
    pub target: String,
    // pub traits: Traits,
}

#[derive(Debug, Deserialize)]
pub struct MapValue {
    pub target: String,
    // pub traits: Traits,
}

impl Model {
    pub fn load_json(path: &str) -> Result<Self> {
        let model: Self = serde_json::from_reader(std::fs::File::open(path)?)?;
        ensure!(model.smithy == "2.0");
        Ok(model)
    }
}

impl Traits {
    pub fn set(&mut self, key: &str, value: Value) {
        let map = self.0.get_or_insert_with(Map::new);
        map.insert(key.to_owned(), value);
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        let map = self.0.as_ref()?;
        map.get(key)
    }

    /// Returns an iterator over the trait entries.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.as_ref().into_iter().flat_map(|m| m.iter())
    }

    #[must_use]
    pub fn enum_value(&self) -> Option<&str> {
        self.get("smithy.api#enumValue")?.as_str()
    }

    #[must_use]
    pub fn doc(&self) -> Option<&str> {
        self.get("smithy.api#documentation")?.as_str()
    }

    #[must_use]
    pub fn timestamp_format(&self) -> Option<&str> {
        self.get("smithy.api#timestampFormat")?.as_str()
    }

    #[must_use]
    pub fn required(&self) -> bool {
        self.get("smithy.api#required").is_some()
    }

    #[must_use]
    pub fn default_value(&self) -> Option<&Value> {
        self.get("smithy.api#default")
    }

    #[must_use]
    pub fn http_header(&self) -> Option<&str> {
        self.get("smithy.api#httpHeader")?.as_str()
    }

    #[must_use]
    pub fn http_payload(&self) -> bool {
        self.get("smithy.api#httpPayload").is_some()
    }

    #[must_use]
    pub fn http_query(&self) -> Option<&str> {
        self.get("smithy.api#httpQuery")?.as_str()
    }

    #[must_use]
    pub fn xml_name(&self) -> Option<&str> {
        self.get("smithy.api#xmlName")?.as_str()
    }

    /// Returns the alternative XML root element names (`s3s#xmlAltName` trait).
    #[must_use]
    pub fn xml_alt_names(&self) -> Vec<&str> {
        self.get("s3s#xmlAltName")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    /// Returns the body literal value (`s3s#bodyLiteral` trait).
    #[must_use]
    pub fn body_literal(&self) -> Option<&str> {
        self.get("s3s#bodyLiteral")?.as_str()
    }

    #[must_use]
    pub fn xml_attr(&self) -> bool {
        self.get("smithy.api#xmlAttribute").is_some()
    }

    #[must_use]
    pub fn xml_flattened(&self) -> bool {
        self.get("smithy.api#xmlFlattened").is_some()
    }

    #[must_use]
    fn xml_namespace(&self) -> Option<&Map<String, Value>> {
        self.get("smithy.api#xmlNamespace")?.as_object()
    }

    #[must_use]
    pub fn xml_namespace_uri(&self) -> Option<&str> {
        self.xml_namespace()?.get("uri")?.as_str()
    }

    #[must_use]
    pub fn xml_namespace_prefix(&self) -> Option<&str> {
        self.xml_namespace()?.get("prefix")?.as_str()
    }

    #[must_use]
    pub fn s3_unwrapped_xml_output(&self) -> bool {
        self.get("aws.customizations#s3UnwrappedXmlOutput").is_some()
    }

    #[must_use]
    pub fn http_label(&self) -> Option<&Value> {
        self.get("smithy.api#httpLabel")
    }

    #[must_use]
    pub fn http_prefix_headers(&self) -> Option<&str> {
        self.get("smithy.api#httpPrefixHeaders")?.as_str()
    }

    #[must_use]
    pub fn http_method(&self) -> Option<&str> {
        self.get("smithy.api#http")?.as_object()?.get("method")?.as_str()
    }

    #[must_use]
    pub fn http_uri(&self) -> Option<&str> {
        self.get("smithy.api#http")?.as_object()?.get("uri")?.as_str()
    }

    pub fn http_code(&self) -> Option<u16> {
        self.get("smithy.api#http")?
            .as_object()?
            .get("code")?
            .as_u64()
            .map(numeric_cast::<_, u16>)
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.get("smithy.api#error")?.as_str()
    }
}

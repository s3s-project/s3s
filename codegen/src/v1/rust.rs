use std::fmt;
use std::format as f;

use scoped_writer::g;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Type {
    Alias(Alias),
    Provided(Provided),
    List(List),
    Map(Map),
    StrEnum(StrEnum),
    Struct(Struct),
    StructEnum(StructEnum),
    Timestamp(Timestamp),
}

#[derive(Debug, Clone)]
pub struct Alias {
    pub name: String,
    pub type_: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Provided {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct List {
    pub name: String,
    pub member: ListMember,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ListMember {
    pub type_: String,
    pub xml_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Map {
    pub name: String,
    pub key_type: String,
    pub value_type: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StrEnum {
    pub name: String,
    pub variants: Vec<StrEnumVariant>,
    pub doc: Option<String>,

    pub is_custom_extension: bool,
}

#[derive(Debug, Clone)]
pub struct StrEnumVariant {
    pub name: String,
    pub value: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Struct {
    pub name: String,
    pub fields: Vec<StructField>,
    pub doc: Option<String>,

    pub xml_name: Option<String>,
    pub is_error_type: bool,

    pub is_custom_extension: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct StructField {
    pub name: String,
    pub type_: String,
    pub doc: Option<String>,

    pub camel_name: String,

    pub option_type: bool,
    pub default_value: Option<Value>,
    pub is_required: bool,

    pub position: String,

    pub http_header: Option<String>,
    pub http_query: Option<String>,

    pub xml_name: Option<String>,
    pub xml_flattened: bool,

    pub is_xml_attr: bool,
    pub xml_namespace_uri: Option<String>,
    pub xml_namespace_prefix: Option<String>,

    pub is_custom_extension: bool,

    pub custom_in_derive_debug: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StructEnum {
    pub name: String,
    pub variants: Vec<StructEnumVariant>,
    pub doc: Option<String>,

    pub is_custom_extension: bool,
}

#[derive(Debug, Clone)]
pub struct StructEnumVariant {
    pub name: String,
    pub type_: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Timestamp {
    pub name: String,
    pub format: Option<String>,
    pub doc: Option<String>,
}

impl Type {
    pub fn provided(name: &str) -> Self {
        Self::Provided(Provided { name: name.to_owned() })
    }

    pub fn alias(name: &str, type_: &str, doc: Option<&str>) -> Self {
        Self::Alias(Alias {
            name: name.to_owned(),
            type_: type_.to_owned(),
            doc: doc.map(ToOwned::to_owned),
        })
    }
}

pub fn codegen_doc(doc: Option<&str>) {
    let Some(doc) = doc else { return };

    for line in doc.lines() {
        let mut line = line.trim_start().to_owned();

        let word_fixes_type1 = [
            "Region",
            "account-id",
            "access-point-name",
            "outpost-id",
            "key",
            "version-id",
            "Code",
            "Message",
        ];

        for word in word_fixes_type1 {
            if line.contains(word) {
                line = line.replace(&f!("<{word}>"), &f!("&lt;{word}&gt;"));
            }
        }

        let word_fixes_type2 = ["Grantee", "BucketLoggingStatus"];

        for word in word_fixes_type2 {
            if line.contains(word) {
                line = line.replace(&f!("<{word} xmlns"), &f!("&lt;{word} xmlns"));
            }
        }

        let word_fixes_type3 = ["NotificationConfiguration", "Grantee"];

        for word in word_fixes_type3 {
            if line.contains(word) {
                line = line.replace(&f!("<{word}></code>"), &f!("&lt;{word}&gt;</code>"));
                line = line.replace(&f!("</{word}></code>"), &f!("&lt;/{word}&gt;</code>"));
            }
        }

        if line.ends_with("<code>200") {
            line = line.replace("<code>200", "200");
        }
        if line.starts_with("OK</code>") {
            line = line.replace("OK</code>", "OK");
        }

        g!("/// {line}");
    }
}

pub fn default_value_literal(v: &Value) -> &dyn fmt::Display {
    match v {
        Value::Bool(x) => x,
        Value::Number(x) => x,
        _ => unimplemented!(),
    }
}

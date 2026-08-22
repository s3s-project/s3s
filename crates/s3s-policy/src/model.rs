// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_grammar.html>

use std::marker::PhantomData;
use std::slice;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Policy {
    pub version: Option<Version>,

    pub id: Option<Id>,

    pub statement: OneOrMore<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Version {
    #[serde(rename = "2012-10-17")]
    V2012_10_17,

    #[serde(rename = "2008-10-17")]
    V2008_10_17,
}

pub type Id = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Statement {
    pub sid: Option<Sid>,

    #[serde(flatten)]
    pub principal: Option<PrincipalRule>,

    pub effect: Effect,

    #[serde(flatten)]
    pub action: ActionRule,

    #[serde(flatten)]
    pub resource: ResourceRule,

    pub condition: Option<ConditionRule>,
}

pub type Sid = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalRule {
    Principal(Principal),
    NotPrincipal(Principal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Wildcard,
    Map(IndexMap<String, OneOrMore<String>>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionRule {
    Action(WildcardOneOrMore<String>),
    NotAction(WildcardOneOrMore<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceRule {
    Resource(WildcardOneOrMore<String>),
    NotResource(WildcardOneOrMore<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionRule(pub IndexMap<ConditionOperator, ConditionKeyValues>);

pub type ConditionOperator = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionKeyValues(pub IndexMap<String, OneOrMore<String>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneOrMore<T> {
    One(T),
    More(Vec<T>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WildcardOneOrMore<T> {
    Wildcard,
    One(T),
    More(Vec<T>),
}

impl Serialize for Principal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Principal::Wildcard => serializer.serialize_str("*"),
            Principal::Map(map) => map.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Principal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Principal;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a wildcard or a map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == "*" {
                    return Ok(Principal::Wildcard);
                }
                Err(serde::de::Error::invalid_value(serde::de::Unexpected::Str(value), &self))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let de = serde::de::value::MapAccessDeserializer::new(map);
                IndexMap::deserialize(de).map(Principal::Map)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl<T> Serialize for OneOrMore<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            OneOrMore::One(t) => t.serialize(serializer),
            OneOrMore::More(ts) => ts.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for OneOrMore<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T>(PhantomData<fn() -> T>);

        impl<'de, T> serde::de::Visitor<'de> for Visitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = OneOrMore<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a single value or an array of values")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let de = serde::de::value::StrDeserializer::new(value);
                T::deserialize(de).map(OneOrMore::One)
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let de = serde::de::value::MapAccessDeserializer::new(map);
                T::deserialize(de).map(OneOrMore::One)
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let de = serde::de::value::SeqAccessDeserializer::new(seq);
                <Vec<T>>::deserialize(de).map(OneOrMore::More)
            }
        }

        deserializer.deserialize_any(Visitor(PhantomData))
    }
}

impl<T> From<OneOrMore<T>> for WildcardOneOrMore<T> {
    fn from(value: OneOrMore<T>) -> Self {
        match value {
            OneOrMore::One(t) => WildcardOneOrMore::One(t),
            OneOrMore::More(ts) => WildcardOneOrMore::More(ts),
        }
    }
}

impl<T> Serialize for WildcardOneOrMore<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            WildcardOneOrMore::Wildcard => serializer.serialize_str("*"),
            WildcardOneOrMore::One(t) => t.serialize(serializer),
            WildcardOneOrMore::More(ts) => ts.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for WildcardOneOrMore<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T>(PhantomData<fn() -> T>);

        impl<'de, T> serde::de::Visitor<'de> for Visitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = WildcardOneOrMore<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a wildcard, a single value, or an array of values")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == "*" {
                    return Ok(WildcardOneOrMore::Wildcard);
                }
                let de = serde::de::value::StrDeserializer::new(value);
                T::deserialize(de).map(WildcardOneOrMore::One)
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let de = serde::de::value::SeqAccessDeserializer::new(seq);
                <Vec<T>>::deserialize(de).map(WildcardOneOrMore::More)
            }
        }

        deserializer.deserialize_any(Visitor(PhantomData))
    }
}

impl<T> OneOrMore<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMore::One(t) => slice::from_ref(t),
            OneOrMore::More(ts) => ts,
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            OneOrMore::One(t) => slice::from_mut(t),
            OneOrMore::More(ts) => ts,
        }
    }
}

impl<T> WildcardOneOrMore<T> {
    pub fn as_slice(&self) -> Option<&[T]> {
        match self {
            WildcardOneOrMore::Wildcard => None,
            WildcardOneOrMore::One(t) => Some(slice::from_ref(t)),
            WildcardOneOrMore::More(ts) => Some(ts),
        }
    }

    pub fn as_mut_slice(&mut self) -> Option<&mut [T]> {
        match self {
            WildcardOneOrMore::Wildcard => None,
            WildcardOneOrMore::One(t) => Some(slice::from_mut(t)),
            WildcardOneOrMore::More(ts) => Some(ts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::ActionRule::*;
    use super::ResourceRule::*;

    use indexmap::indexmap;

    fn one(s: &str) -> WildcardOneOrMore<String> {
        WildcardOneOrMore::One(s.to_owned())
    }

    fn more(ss: &[&str]) -> WildcardOneOrMore<String> {
        WildcardOneOrMore::More(ss.iter().copied().map(ToOwned::to_owned).collect())
    }

    #[test]
    fn version_serde() {
        let cases = [
            (Version::V2012_10_17, "\"2012-10-17\""),
            (Version::V2008_10_17, "\"2008-10-17\""),
        ];

        for (enum_, str_) in cases {
            let ser = serde_json::to_string(&enum_).unwrap();
            assert_eq!(ser, str_);

            let de: Version = serde_json::from_str(str_).unwrap();
            assert_eq!(de, enum_);
        }
    }

    #[test]
    fn effect_serde() {
        let cases = [
            (Effect::Allow, "\"Allow\""), //
            (Effect::Deny, "\"Deny\""),
        ];

        for (enum_, str_) in cases {
            let ser = serde_json::to_string(&enum_).unwrap();
            assert_eq!(ser, str_);

            let de: Effect = serde_json::from_str(str_).unwrap();
            assert_eq!(de, enum_);
        }
    }

    #[test]
    fn action_serde() {
        let cases = [
            (
                Action(WildcardOneOrMore::Wildcard), //
                r#"{"Action":"*"}"#,
            ),
            (
                Action(one("s3:GetObject")), //
                r#"{"Action":"s3:GetObject"}"#,
            ),
            (
                Action(more(&["s3:GetObject", "s3:PutObject"])),
                r#"{"Action":["s3:GetObject","s3:PutObject"]}"#,
            ),
            (
                NotAction(WildcardOneOrMore::Wildcard), //
                r#"{"NotAction":"*"}"#,
            ),
            (
                NotAction(one("s3:GetObject")), //
                r#"{"NotAction":"s3:GetObject"}"#,
            ),
            (
                NotAction(more(&["s3:GetObject", "s3:PutObject"])),
                r#"{"NotAction":["s3:GetObject","s3:PutObject"]}"#,
            ),
        ];

        for (enum_, str_) in cases {
            let ser = serde_json::to_string(&enum_).unwrap();
            assert_eq!(ser, str_);

            let de: ActionRule = serde_json::from_str(str_).unwrap();
            assert_eq!(de, enum_);
        }
    }

    #[test]
    fn resource_serde() {
        let cases = [
            (
                Resource(WildcardOneOrMore::Wildcard), //
                r#"{"Resource":"*"}"#,
            ),
            (
                Resource(one("arn:aws:s3:::examplebucket/*")), //
                r#"{"Resource":"arn:aws:s3:::examplebucket/*"}"#,
            ),
            (
                Resource(more(&["arn:aws:s3:::examplebucket/*", "arn:aws:s3:::examplebucket"])),
                r#"{"Resource":["arn:aws:s3:::examplebucket/*","arn:aws:s3:::examplebucket"]}"#,
            ),
            (
                NotResource(WildcardOneOrMore::Wildcard), //
                r#"{"NotResource":"*"}"#,
            ),
            (
                NotResource(one("arn:aws:s3:::examplebucket/*")), //
                r#"{"NotResource":"arn:aws:s3:::examplebucket/*"}"#,
            ),
            (
                NotResource(more(&["arn:aws:s3:::examplebucket/*", "arn:aws:s3:::examplebucket"])),
                r#"{"NotResource":["arn:aws:s3:::examplebucket/*","arn:aws:s3:::examplebucket"]}"#,
            ),
        ];

        for (enum_, str_) in cases {
            let ser = serde_json::to_string(&enum_).unwrap();
            assert_eq!(ser, str_);

            let de: ResourceRule = serde_json::from_str(str_).unwrap();
            assert_eq!(de, enum_);
        }
    }

    #[test]
    fn principal_serde() {
        let cases = [
            (
                PrincipalRule::Principal(Principal::Wildcard), //
                r#"{"Principal":"*"}"#,
            ),
            (
                PrincipalRule::Principal(Principal::Map(indexmap! {
                    "AWS".to_owned() => OneOrMore::One("arn:aws:iam::123456789012:root".to_owned())
                })),
                r#"{"Principal":{"AWS":"arn:aws:iam::123456789012:root"}}"#,
            ),
            (
                PrincipalRule::Principal(Principal::Map(indexmap! {
                    "Service".to_owned() => OneOrMore::More(vec![
                        "ecs.amazonaws.com".to_owned(),
                        "elasticloadbalancing.amazonaws.com".to_owned(),
                    ])
                })),
                r#"{"Principal":{"Service":["ecs.amazonaws.com","elasticloadbalancing.amazonaws.com"]}}"#,
            ),
            (
                PrincipalRule::NotPrincipal(Principal::Wildcard), //
                r#"{"NotPrincipal":"*"}"#,
            ),
            (
                PrincipalRule::NotPrincipal(Principal::Map(indexmap! {
                    "AWS".to_owned() => OneOrMore::One("arn:aws:iam::123456789012:root".to_owned())
                })),
                r#"{"NotPrincipal":{"AWS":"arn:aws:iam::123456789012:root"}}"#,
            ),
            (
                PrincipalRule::NotPrincipal(Principal::Map(indexmap! {
                    "Service".to_owned() => OneOrMore::More(vec![
                        "ecs.amazonaws.com".to_owned(),
                        "elasticloadbalancing.amazonaws.com".to_owned(),
                    ])
                })),
                r#"{"NotPrincipal":{"Service":["ecs.amazonaws.com","elasticloadbalancing.amazonaws.com"]}}"#,
            ),
        ];

        for (enum_, str_) in cases {
            let ser = serde_json::to_string(&enum_).unwrap();
            assert_eq!(ser, str_);

            let de: PrincipalRule = serde_json::from_str(str_).unwrap();
            assert_eq!(de, enum_);
        }
    }

    #[test]
    fn example1() {
        let json = crate::tests::example1_json();
        let policy: Policy = serde_json::from_str(json).unwrap();

        let expected = Policy {
            version: Some(Version::V2012_10_17),
            id: None,
            statement: OneOrMore::More(vec![
                Statement {
                    sid: Some("FirstStatement".to_owned()),
                    effect: Effect::Allow,
                    action: Action(more(&["iam:ChangePassword"])),
                    resource: Resource(WildcardOneOrMore::Wildcard),
                    principal: None,
                    condition: None,
                },
                Statement {
                    sid: Some("SecondStatement".to_owned()),
                    effect: Effect::Allow,
                    action: Action(one("s3:ListAllMyBuckets")),
                    resource: Resource(WildcardOneOrMore::Wildcard),
                    principal: None,
                    condition: None,
                },
                Statement {
                    sid: Some("ThirdStatement".to_owned()),
                    effect: Effect::Allow,
                    action: Action(more(&["s3:List*", "s3:Get*"])),
                    resource: Resource(more(&["arn:aws:s3:::confidential-data", "arn:aws:s3:::confidential-data/*"])),
                    principal: None,
                    condition: Some(ConditionRule(indexmap! {
                        "Bool".to_owned() => ConditionKeyValues(
                            indexmap! {
                                "aws:MultiFactorAuthPresent".to_owned() => OneOrMore::One("true".to_owned())
                            }
                        )
                    })),
                },
            ]),
        };

        assert_eq!(policy, expected);

        {
            let ser = serde_json::to_string(&policy).unwrap();
            let de: Policy = serde_json::from_str(&ser).unwrap();
            assert_eq!(de, policy);
        }
    }

    #[test]
    fn example2() {
        let json = crate::tests::example2_json();
        let policy: Policy = serde_json::from_str(json).unwrap();

        let expected = Policy {
            version: Some(Version::V2012_10_17),
            id: None,
            statement: OneOrMore::One(Statement {
                sid: None,
                effect: Effect::Allow,
                action: Action(one("s3:ListBucket")),
                resource: Resource(one("arn:aws:s3:::example_bucket")),
                principal: None,
                condition: None,
            }),
        };

        assert_eq!(policy, expected);

        {
            let ser = serde_json::to_string(&policy).unwrap();
            let de: Policy = serde_json::from_str(&ser).unwrap();
            assert_eq!(de, policy);
        }
    }

    #[test]
    fn example3() {
        let json = crate::tests::example3_json();
        let policy: Policy = serde_json::from_str(json).unwrap();

        let expected = Policy {
            version: Some(Version::V2012_10_17),
            id: None,
            statement: OneOrMore::More(vec![Statement {
                sid: Some("1".to_owned()),
                effect: Effect::Allow,
                action: Action(one("s3:*")),
                resource: Resource(more(&["arn:aws:s3:::mybucket", "arn:aws:s3:::mybucket/*"])),
                principal: Some(PrincipalRule::Principal(Principal::Map(indexmap! {
                    "AWS".to_owned() => OneOrMore::More(vec!["arn:aws:iam::account-id:root".to_owned()])
                }))),
                condition: None,
            }]),
        };

        assert_eq!(policy, expected);

        {
            let ser = serde_json::to_string(&policy).unwrap();
            let de: Policy = serde_json::from_str(&ser).unwrap();
            assert_eq!(de, policy);
        }
    }
}

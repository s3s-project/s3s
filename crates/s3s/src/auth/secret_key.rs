// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Authenticated S3 credentials.
///
/// Contains the access key ID and the corresponding [`SecretKey`].
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The access key ID.
    pub access_key: String,
    /// The secret key associated with the access key.
    pub secret_key: SecretKey,
}

/// An S3 secret key.
///
/// The secret value is zeroized on drop, and its [`Debug`](std::fmt::Debug)
/// implementation hides the actual value.
#[derive(Clone)]
pub struct SecretKey(Box<str>);

impl SecretKey {
    fn new(s: impl Into<Box<str>>) -> Self {
        Self(s.into())
    }

    /// Exposes the secret key value.
    ///
    /// This is the only way to read the secret. Use it with care.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Zeroize for SecretKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl ConstantTimeEq for SecretKey {
    fn ct_eq(&self, other: &Self) -> subtle::Choice {
        self.0.as_bytes().ct_eq(other.0.as_bytes())
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl From<String> for SecretKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<Box<str>> for SecretKey {
    fn from(value: Box<str>) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

const PLACEHOLDER: &str = "[SENSITIVE-SECRET-KEY]";

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretKey").field(&PLACEHOLDER).finish()
    }
}

impl<'de> Deserialize<'de> for SecretKey {
    fn deserialize<D>(deserializer: D) -> Result<SecretKey, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <String as Deserialize>::deserialize(deserializer).map(SecretKey::from)
    }
}

impl Serialize for SecretKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        <str as Serialize>::serialize(PLACEHOLDER, serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_from_str() {
        let key = SecretKey::from("my-secret");
        assert_eq!(key.expose(), "my-secret");
    }

    #[test]
    fn new_from_string() {
        let key = SecretKey::from("my-secret".to_owned());
        assert_eq!(key.expose(), "my-secret");
    }

    #[test]
    fn new_from_box_str() {
        let boxed: Box<str> = "my-secret".into();
        let key = SecretKey::from(boxed);
        assert_eq!(key.expose(), "my-secret");
    }

    #[test]
    fn debug_hides_value() {
        let key = SecretKey::from("super-secret-value");
        let debug = format!("{key:?}");
        assert!(!debug.contains("super-secret-value"));
        assert!(debug.contains(PLACEHOLDER));
    }

    #[test]
    fn constant_time_eq() {
        let a = SecretKey::from("same-key");
        let b = SecretKey::from("same-key");
        assert!(bool::from(a.ct_eq(&b)));

        let c = SecretKey::from("different-key");
        assert!(!bool::from(a.ct_eq(&c)));
    }

    #[test]
    fn serialize_hides_value() {
        let key = SecretKey::from("my-secret");
        let json = serde_json::to_string(&key).unwrap();
        assert!(!json.contains("my-secret"));
        assert!(json.contains(PLACEHOLDER));
    }

    #[test]
    fn deserialize() {
        let json = r#""deserialized-secret""#;
        let key: SecretKey = serde_json::from_str(json).unwrap();
        assert_eq!(key.expose(), "deserialized-secret");
    }

    #[test]
    fn clone() {
        let key = SecretKey::from("clone-me");
        let cloned = key.clone();
        assert_eq!(cloned.expose(), "clone-me");
    }

    #[test]
    fn credentials_debug() {
        let creds = Credentials {
            access_key: "AKID".to_owned(),
            secret_key: SecretKey::from("hunter2"),
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("AKID"));
        assert!(!debug.contains("hunter2"));
    }
}

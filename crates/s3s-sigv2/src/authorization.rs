// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Authorization V2
//!
//! <https://docs.aws.amazon.com/AmazonS3/latest/userguide/RESTAuthentication.html#ConstructingTheAuthenticationHeader>

/// `SigV2` `Authorization` header: `AWS <access-key>:<signature>`
pub struct AuthorizationV2<'a> {
    /// access key id
    pub access_key: &'a str,
    /// signature (standard Base64, HMAC-SHA1)
    pub signature: &'a str,
}

/// [`AuthorizationV2`] parse error
#[derive(Debug, thiserror::Error)]
#[error("ParseAuthorizationError")]
pub struct ParseAuthorizationV2Error {
    /// priv place holder
    _priv: (),
}

impl<'a> AuthorizationV2<'a> {
    /// Parses an `Authorization` header of the form `AWS <access-key>:<signature>`.
    ///
    /// # Errors
    /// Returns [`ParseAuthorizationV2Error`] when the header does not match.
    pub fn parse(mut input: &'a str) -> Result<Self, ParseAuthorizationV2Error> {
        let err = || ParseAuthorizationV2Error { _priv: () };

        input = input.strip_prefix("AWS ").ok_or_else(err)?;

        let (access_key, signature) = input.split_once(':').ok_or_else(err)?;

        Ok(Self { access_key, signature })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example() {
        let input = "AWS AKIAIOSFODNN7EXAMPLE:qgk2+6Sv9/oM7G3qLEjTH1a1l1g=";
        let ans = AuthorizationV2::parse(input).unwrap();
        assert_eq!(ans.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(ans.signature, "qgk2+6Sv9/oM7G3qLEjTH1a1l1g=");
    }
}

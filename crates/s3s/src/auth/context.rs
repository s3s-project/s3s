// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// The credential a request claims, as read by the signature verifier.
///
/// Passed to [`S3Auth::get_secret_key_with_context`](super::S3Auth::get_secret_key_with_context).
/// The fields are private so that a later addition does not break implementors.
#[derive(Debug, Clone, Copy)]
pub struct S3AuthContext<'a> {
    access_key: &'a str,
    session_token: Option<&'a str>,
}

impl<'a> S3AuthContext<'a> {
    pub(crate) fn new(access_key: &'a str, session_token: Option<&'a str>) -> Self {
        Self {
            access_key,
            session_token,
        }
    }

    /// The access key ID the request was signed with.
    #[must_use]
    pub fn access_key(&self) -> &'a str {
        self.access_key
    }

    /// The session token of a temporary credential, when the request carries one.
    ///
    /// Read from the `x-amz-security-token` header (header authentication),
    /// the `X-Amz-Security-Token` query parameter (`SigV4` presigned URL),
    /// the `x-amz-security-token` query parameter or header (`SigV2` presigned URL)
    /// or the `x-amz-security-token` form field (POST policy).
    ///
    /// Like the access key, the token is read before the signature is verified:
    /// the request is authenticated only once the returned secret key verifies it.
    #[must_use]
    pub fn session_token(&self) -> Option<&'a str> {
        self.session_token
    }
}

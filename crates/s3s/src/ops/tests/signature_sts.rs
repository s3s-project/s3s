// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! STS body hashing and its size limit.

use crate::ops::signature::*;

use crate::http::Body;
use crate::utils::crypto::hex_sha256;

#[test]
fn sts_body_hash_is_deterministic_hex() {
    // Typical STS AssumeRole request body
    let body_content = b"Action=AssumeRole&RoleArn=arn:aws:iam::123456789012:role/test-role&RoleSessionName=test-session";

    // Compute hash
    let hash = hex_sha256(body_content, str::to_owned);

    // Verify hash is a valid hex string of correct length (64 chars for SHA256)
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    // Verify hash is deterministic
    let hash2 = hex_sha256(body_content, str::to_owned);
    assert_eq!(hash, hash2);
}

#[tokio::test]
async fn test_sts_body_size_limit_enforced() {
    // Test that body size limit is enforced for STS requests
    use bytes::Bytes;

    // Create a body that exceeds MAX_STS_BODY_SIZE
    let large_body = vec![b'x'; MAX_STS_BODY_SIZE + 1];
    let mut body = Body::from(Bytes::from(large_body));

    // Try to read with limit
    let result = body.store_all_limited(MAX_STS_BODY_SIZE).await;

    // Should fail due to size limit
    assert!(result.is_err());
}

#[tokio::test]
async fn test_sts_body_within_limit() {
    // Test that body reading succeeds when within limit
    use bytes::Bytes;

    // Create a body within the limit
    let small_body = b"Action=AssumeRole&RoleArn=test&RoleSessionName=session";
    let mut body = Body::from(Bytes::from(&small_body[..]));

    // Try to read with limit
    let result = body.store_all_limited(MAX_STS_BODY_SIZE).await;

    // Should succeed
    assert!(result.is_ok());
    let bytes = result.unwrap();
    assert_eq!(&bytes[..], &small_body[..]);
}

#[test]
fn sts_max_body_size_is_in_reasonable_range() {
    // STS requests are typically small (under 2 KB for AssumeRole).
    // The limit must be large enough for real requests but small enough to
    // prevent trivial DoS via oversized bodies.
    const { assert!(MAX_STS_BODY_SIZE >= 2048, "limit must be large enough for typical STS requests") };
    const { assert!(MAX_STS_BODY_SIZE <= 65536, "limit must be small enough to bound memory use") };
}

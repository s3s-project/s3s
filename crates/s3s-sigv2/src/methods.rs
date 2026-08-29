// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Canonicalization and signing for AWS Signature Version 2.
//!
//! <https://docs.aws.amazon.com/AmazonS3/latest/userguide/RESTAuthentication.html>

use base64_simd::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use smallvec::SmallVec;

/// Request signature mode for [`create_string_to_sign`].
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Signature via the `Authorization` request header.
    HeaderAuth,
    /// Signature via query parameters (pre-signed URL).
    PresignedUrl,
}

/// Query parameters that participate in the canonicalized resource,
/// in the fixed output order required by `SigV2`.
const INCLUDED_QUERY: &[&str] = &[
    "accelerate",
    "acl",
    "analytics",
    "cors",
    "defaultObjectAcl",
    "delete",
    "inventory",
    "lifecycle",
    "location",
    "logging",
    "metrics",
    "notification",
    "object-lock",
    "partNumber",
    "policy",
    "replication",
    "requestPayment",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
    "response-content-language",
    "response-content-type",
    "response-expires",
    "restore",
    "select",
    "select-type",
    "storageClass",
    "tagging",
    "torrent",
    "uploadId",
    "uploads",
    "versionId",
    "versioning",
    "versions",
    "website",
];

/// Returns the value only when the name maps to exactly one header.
///
/// Repeated header lines (including case variants) are treated as if the
/// header were absent instead of silently picking an arbitrary duplicate.
fn get_unique_header_str<'a>(headers: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    let mut iter = headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name));
    let (_, value) = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    Some(value)
}

/// Returns the value only when the name maps to exactly one entry.
///
/// The query pairs must be sorted by name (the `OrderedQs` contract) for the
/// binary search to be valid; repeated names are treated as absent.
///
/// # Panics
///
/// The `qs[lower_bound..]` access cannot panic: `partition_point` returns a
/// `lower_bound ≤ qs.len()`. clippy cannot prove this statically, so the lint
/// is allowed here and the invariant is documented.
#[allow(clippy::indexing_slicing)]
fn get_unique_qs<'a>(qs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let lower_bound = qs.partition_point(|x| x.0.as_str() < name);

    let mut iter = qs[lower_bound..].iter();
    let pair = iter.next()?;

    if let Some(following) = iter.next()
        && following.0 == name
    {
        return None;
    }

    (pair.0.as_str() == name).then_some(pair.1.as_str())
}

/// Creates the `StringToSign` for `SigV2`.
///
/// # Inputs
///
/// - `method`: the HTTP verb, e.g. `"GET"`.
/// - `uri_path`: the percent-encoded request path (no query part).
/// - `qs`: query pairs with values already percent-decoded, sorted by name
///   (the [`OrderedQs`](https://docs.rs/s3s/latest/s3s/http/struct.OrderedQs.html)
///   contract), or `None` when the request has no query string.
/// - `headers`: `(name, value)` pairs in any order; repeated names preserve
///   the repeated header lines. Only `x-amz-*` names participate in the
///   canonicalized headers, and header names are matched case-insensitively.
/// - `virtual_host_bucket`: the bucket name from the host (virtual-hosted
///   style), prepended to the canonicalized resource.
#[must_use]
pub fn create_string_to_sign(
    mode: Mode,
    method: &str,
    uri_path: &str,
    qs: Option<&[(String, String)]>,
    headers: &[(&str, &str)],
    virtual_host_bucket: Option<&str>,
) -> String {
    let mut ans = String::with_capacity(256);

    {
        // {HTTP-Verb}\n
        ans.push_str(method);
        ans.push('\n');
    }

    {
        // {Content-MD5}\n
        if let Some(v) = get_unique_header_str(headers, "content-md5") {
            ans.push_str(v);
        }
        ans.push('\n');
    }

    {
        // {Content-Type}\n
        if let Some(v) = get_unique_header_str(headers, "content-type") {
            ans.push_str(v);
        }
        ans.push('\n');
    }

    match mode {
        // {Date}\n
        Mode::HeaderAuth => {
            //  "if you include the x-amz-date header, use the empty string
            //      for the Date when constructing the StringToSign."
            let mut date = get_unique_header_str(headers, "date").unwrap_or_default();
            if get_unique_header_str(headers, "x-amz-date").is_some() {
                date = "";
            }
            ans.push_str(date);
            ans.push('\n');
        }
        // {Expires}\n
        Mode::PresignedUrl => {
            let expires = qs.and_then(|qs| get_unique_qs(qs, "Expires")).unwrap_or_default();
            ans.push_str(expires);
            ans.push('\n');
        }
    }

    {
        // {CanonicalizedAmzHeaders}
        let mut amz_headers = SmallVec::<[(&str, &str); 8]>::new();
        for &(name, value) in headers {
            if name.starts_with("x-amz-") {
                amz_headers.push((name, value));
            }
        }
        amz_headers.sort_by(|lhs, rhs| lhs.0.cmp(rhs.0));

        push_canonicalized_amz_headers(&mut ans, &amz_headers);
    }

    {
        // {CanonicalizedResource}

        if let Some(bucket) = virtual_host_bucket {
            ans.push('/');
            ans.push_str(bucket);
        }

        ans.push_str(uri_path);

        if let Some(qs) = qs {
            let mut is_first = true;
            for q in INCLUDED_QUERY {
                if let Some(v) = get_unique_qs(qs, q) {
                    if is_first {
                        ans.push('?');
                        is_first = false;
                    } else {
                        ans.push('&');
                    }
                    ans.push_str(q);
                    if !v.is_empty() {
                        ans.push('=');
                        ans.push_str(v);
                    }
                }
            }
        }
    }

    ans
}

/// Appends the canonicalized `x-amz-*` headers: each group of adjacent
/// same-named headers becomes `name:v1,v2\n`.
///
/// # Panics
///
/// The index accesses are guarded by the loop conditions (`i < len`,
/// `j < len`); clippy cannot prove this statically, so the lint is allowed
/// here and the invariants are documented.
#[allow(clippy::indexing_slicing)]
fn push_canonicalized_amz_headers(ans: &mut String, amz_headers: &[(&str, &str)]) {
    let mut i = 0;
    while i < amz_headers.len() {
        let (name, value) = amz_headers[i];

        ans.push_str(name);
        ans.push(':');

        ans.push_str(value.trim());

        let mut j = i + 1;
        while j < amz_headers.len() && amz_headers[j].0 == name {
            ans.push(',');
            ans.push_str(amz_headers[j].1.trim());
            j += 1;
        }

        ans.push('\n');
        i = j;
    }
}

/// Computes the `SigV2` request signature: HMAC-SHA1 of the `StringToSign`
/// under the secret key, encoded as standard Base64.
///
/// # Panics
///
/// `HMAC-SHA1` accepts keys of any length, so this never panics in practice.
#[must_use]
pub fn calculate_signature(secret_key: impl AsRef<[u8]>, string_to_sign: &str) -> String {
    let mut m = new_hmac_sha1(secret_key.as_ref());
    m.update(string_to_sign.as_bytes());
    let digest = m.finalize().into_bytes();
    STANDARD.encode_to_string(digest)
}

/// `HMAC-SHA1` accepts keys of any length, so `new_from_slice` never fails.
///
/// # Panics
///
/// This `expect` is a structural invariant of the `hmac` crate API; no
/// request input can influence it. The lint is allowed here and the
/// invariant is documented.
#[allow(clippy::expect_used)]
fn new_hmac_sha1(key: &[u8]) -> Hmac<Sha1> {
    <Hmac<Sha1>>::new_from_slice(key).expect("Hmac accepts keys of any length")
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

    fn signed<'a>(
        mode: Mode,
        method: &'a str,
        uri_path: &'a str,
        qs: Option<&'a [(String, String)]>,
        headers: &'a [(&'a str, &'a str)],
        vh_bucket: Option<&'a str>,
    ) -> String {
        let sts = create_string_to_sign(mode, method, uri_path, qs, headers, vh_bucket);
        calculate_signature(SECRET_KEY, &sts)
    }

    #[test]
    fn sorted() {
        for w in INCLUDED_QUERY.windows(2) {
            assert!(w[0] < w[1], "{w:?}");
        }
    }

    #[test]
    fn duplicate_headers_treated_as_absent() {
        // duplicate content-type is treated as absent (empty third line)
        let headers = &[("content-type", "a"), ("content-type", "b"), ("date", "d")];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", None, headers, None);
        assert_eq!(sts, "GET\n\n\nd\n/b/k");

        // duplicate x-amz-* headers are merged with "," after sorting by name
        let headers = &[("x-amz-meta-b", "2"), ("x-amz-meta-a", "1"), ("x-amz-meta-b", "3")];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", None, headers, None);
        assert_eq!(sts, concat!("GET\n", "\n", "\n", "\n", "x-amz-meta-a:1\n", "x-amz-meta-b:2,3\n", "/b/k",));
    }

    #[test]
    fn missing_and_duplicate_lookups() {
        // no headers at all: content-md5 / content-type / date lines are empty
        let headers: &[(&str, &str)] = &[];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", None, headers, None);
        assert_eq!(sts, "GET\n\n\n\n/b/k");

        // duplicate Expires in the query string is treated as absent
        let qs = vec![("Expires".to_owned(), "1".to_owned()), ("Expires".to_owned(), "2".to_owned())];
        let sts = create_string_to_sign(Mode::PresignedUrl, "GET", "/b/k", Some(&qs), &[], None);
        assert_eq!(sts, "GET\n\n\n\n/b/k");

        // duplicate x-amz-date is treated as absent, so the Date line is kept
        let headers = &[("x-amz-date", "a"), ("x-amz-date", "b"), ("date", "d")];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", None, headers, None);
        assert_eq!(sts, "GET\n\n\nd\nx-amz-date:a,b\n/b/k");
    }

    #[test]
    fn multiple_included_query_parameters() {
        // two included parameters with non-empty values: joined with "&"
        // and each rendered as "name=value"
        let qs = vec![("acl".to_owned(), "x".to_owned()), ("tagging".to_owned(), "y".to_owned())];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", Some(&qs), &[], None);
        assert_eq!(sts, "GET\n\n\n\n/b/k?acl=x&tagging=y");

        // non-included parameters do not participate in the resource
        let qs = vec![("other".to_owned(), "z".to_owned())];
        let sts = create_string_to_sign(Mode::HeaderAuth, "GET", "/b/k", Some(&qs), &[], None);
        assert_eq!(sts, "GET\n\n\n\n/b/k");
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn examples() {
        {
            // Object GET
            let method = "GET";
            let uri_path = "/photos/puppy.jpg";
            let headers = &[("date", "Tue, 27 Mar 2007 19:36:42 +0000")];
            let qs = None;
            let vh_bucket = Some("awsexamplebucket1");

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "Tue, 27 Mar 2007 19:36:42 +0000\n",
                    "/awsexamplebucket1/photos/puppy.jpg",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "qgk2+6Sv9/oM7G3qLEjTH1a1l1g="
            );
        }

        {
            // Object PUT
            let method = "PUT";
            let uri_path = "/photos/puppy.jpg";
            let headers = &[("content-type", "image/jpeg"), ("date", "Tue, 27 Mar 2007 21:15:45 +0000")];
            let qs = None;
            let vh_bucket = Some("awsexamplebucket1");

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "PUT\n",
                    "\n",
                    "image/jpeg\n",
                    "Tue, 27 Mar 2007 21:15:45 +0000\n",
                    "/awsexamplebucket1/photos/puppy.jpg",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "iqRzw+ileNPu1fhspnRs8nOjjIA="
            );
        }

        {
            // List
            let method = "GET";
            let uri_path = "/";
            let headers = &[("date", "Tue, 27 Mar 2007 19:42:41 +0000")];
            let qs = None;
            let vh_bucket = Some("awsexamplebucket1");

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "Tue, 27 Mar 2007 19:42:41 +0000\n", //
                    "/awsexamplebucket1/",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "m0WP8eCtspQl5Ahe6L1SozdX9YA="
            );
        }

        {
            // Fetch
            let method = "GET";
            let uri_path = "/";
            let qs = vec![("acl".to_owned(), String::new())];
            let headers = &[("date", "Tue, 27 Mar 2007 19:44:46 +0000")];
            let vh_bucket = Some("awsexamplebucket1");

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, Some(&qs), headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "Tue, 27 Mar 2007 19:44:46 +0000\n", //
                    "/awsexamplebucket1/?acl",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, Some(&qs), headers, vh_bucket),
                "82ZHiFIjc+WbcwFKGUVEQspPn+0="
            );
        }

        {
            // Delete
            let method = "DELETE";
            let uri_path = "/awsexamplebucket1/photos/puppy.jpg";
            let headers = &[
                ("date", "Tue, 27 Mar 2007 21:20:27 +0000"),
                ("x-amz-date", "Tue, 27 Mar 2007 21:20:26 +0000"),
            ];
            let qs = None;
            let vh_bucket = None;

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "DELETE\n",
                    "\n",
                    "\n",
                    "\n",
                    "x-amz-date:Tue, 27 Mar 2007 21:20:26 +0000\n",
                    "/awsexamplebucket1/photos/puppy.jpg",
                )
            );

            // FIXME: The example is wrong?
            // assert_eq!(signed(...), "XbyTlbQdu9Xw5o8P4iMwPktxQd8=");
            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "Ri1hpB1zpS9pGqR7y8kuNFCl4sE="
            );
        }

        {
            // Upload
            let method = "PUT";
            let uri_path = "/db-backup.dat.gz";
            let headers = &[
                ("date", "Tue, 27 Mar 2007 21:06:08 +0000"),
                ("x-amz-acl", "public-read"),
                ("content-type", "application/x-download"),
                ("content-md5", "4gJE4saaMU4BqNR0kLY+lw=="),
                ("x-amz-meta-reviewedby", "joe@example.com"),
                ("x-amz-meta-reviewedby", "jane@example.com"),
                ("x-amz-meta-filechecksum", "0x02661779"),
                ("x-amz-meta-checksumalgorithm", "crc32"),
                ("content-disposition", "attachment; filename=database.dat"),
                ("content-encoding", "gzip"),
                ("content-length", "5913339"),
            ];
            let qs = None;
            let vh_bucket = Some("static.example.com");

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "PUT\n",
                    "4gJE4saaMU4BqNR0kLY+lw==\n",
                    "application/x-download\n",
                    "Tue, 27 Mar 2007 21:06:08 +0000\n",
                    "x-amz-acl:public-read\n",
                    "x-amz-meta-checksumalgorithm:crc32\n",
                    "x-amz-meta-filechecksum:0x02661779\n",
                    "x-amz-meta-reviewedby:joe@example.com,jane@example.com\n",
                    "/static.example.com/db-backup.dat.gz",
                )
            );

            // assert_eq!(signed(...), "dKZcB+bz2EPXgSdXZp9ozGeOM4I="); // The example is wrong?
            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "jtBQa0Aq+DkULFI8qrpwIjGEx0E="
            );
        }

        {
            // List all my buckets
            let method = "GET";
            let uri_path = "/";
            let headers = &[("date", "Wed, 28 Mar 2007 01:29:59 +0000")];
            let qs = None;
            let vh_bucket = None;

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "Wed, 28 Mar 2007 01:29:59 +0000\n", //
                    "/",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "qGdzdERIC03wnaRNKh6OqZehG9s="
            );
        }

        {
            // Unicode keys
            let method = "GET";
            let uri_path = "/dictionary/fran%C3%A7ais/pr%c3%a9f%c3%a8re";
            let headers = &[("date", "Wed, 28 Mar 2007 01:49:49 +0000")];
            let qs = None;
            let vh_bucket = None;

            let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "Wed, 28 Mar 2007 01:49:49 +0000\n",
                    "/dictionary/fran%C3%A7ais/pr%c3%a9f%c3%a8re",
                )
            );

            assert_eq!(
                signed(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket),
                "DNEZGsoieTZ92F3bUfSPQcbGmlM="
            );
        }

        {
            // Query string request authentication
            let method = "GET";
            let uri_path = "/photos/puppy.jpg";
            let headers: &[(&str, &str)] = &[];
            let qs = vec![
                ("AWSAccessKeyId".to_owned(), ACCESS_KEY.to_owned()),
                // "Signature=NpgCjnDzrM%2BWFzoENXmpNDUsSn8%3D", // The example is wrong?
                ("Expires".to_owned(), "1175139620".to_owned()),
                ("Signature".to_owned(), "1No4mq5ETf02z8aet9voy6gui6E=".to_owned()),
            ];
            let vh_bucket = Some("awsexamplebucket1");

            let presigned_url = crate::PresignedUrlV2::parse(&qs).unwrap();
            assert_eq!(presigned_url.access_key, ACCESS_KEY);

            let string_to_sign = create_string_to_sign(Mode::PresignedUrl, method, uri_path, Some(&qs), headers, vh_bucket);

            assert_eq!(
                string_to_sign,
                concat!(
                    "GET\n",
                    "\n",
                    "\n",
                    "1175139620\n", //
                    "/awsexamplebucket1/photos/puppy.jpg",
                )
            );

            assert_eq!(calculate_signature(SECRET_KEY, &string_to_sign), presigned_url.signature);
        }
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/137>
    ///
    /// When `x-amz-date` is present:
    /// - The Date field in the string-to-sign must be empty
    /// - `x-amz-date` must be included in `CanonicalizedAmzHeaders`
    /// - The URI path is used directly (not parsed into `S3Path`)
    #[test]
    fn regression_sig_v2_x_amz_date() {
        // Path-style request with x-amz-date: date field must be empty,
        // and x-amz-date must appear in canonicalized headers
        let method = "GET";
        let uri_path = "/mybucket/myobject";
        let headers = &[
            ("date", "Thu, 14 Mar 2024 12:00:00 +0000"),
            ("x-amz-date", "Thu, 14 Mar 2024 12:00:00 +0000"),
        ];
        let qs = None;
        let vh_bucket = None;

        let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

        // Date field must be empty when x-amz-date is present
        assert_eq!(
            string_to_sign,
            concat!(
                "GET\n",
                "\n",
                "\n",
                "\n", // empty date because x-amz-date is present
                "x-amz-date:Thu, 14 Mar 2024 12:00:00 +0000\n",
                "/mybucket/myobject",
            )
        );

        // Sanity-check: a non-empty signature is produced for this input
        let sig = calculate_signature(SECRET_KEY, &string_to_sign);
        assert_ne!(sig.len(), 0);
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/137>
    ///
    /// Virtual-hosted-style: bucket must be prepended to the canonicalized resource
    #[test]
    fn regression_sig_v2_virtual_hosted_bucket() {
        // Virtual-hosted-style: URI path is "/key" and bucket is prepended
        let method = "GET";
        let uri_path = "/myobject";
        let headers = &[("date", "Thu, 14 Mar 2024 12:00:00 +0000")];
        let qs = None;
        let vh_bucket = Some("mybucket");

        let string_to_sign = create_string_to_sign(Mode::HeaderAuth, method, uri_path, qs, headers, vh_bucket);

        assert_eq!(
            string_to_sign,
            concat!("GET\n", "\n", "\n", "Thu, 14 Mar 2024 12:00:00 +0000\n", "/mybucket/myobject",)
        );

        // Sanity-check: a non-empty signature is produced for this input
        let sig = calculate_signature(SECRET_KEY, &string_to_sign);
        assert_ne!(sig.len(), 0);
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/137>
    ///
    /// `PresignedUrl` mode: Expires field must come from query string
    #[test]
    fn regression_sig_v2_presigned_expires() {
        let method = "GET";
        let uri_path = "/myobject";
        let headers = &[("date", "Thu, 14 Mar 2024 12:00:00 +0000")];
        let qs = vec![("Expires".to_owned(), "1710417600".to_owned())];
        let vh_bucket = Some("mybucket");

        let string_to_sign = create_string_to_sign(Mode::PresignedUrl, method, uri_path, Some(&qs), headers, vh_bucket);

        // In presigned URL mode, the date line should be the Expires value from the query string
        assert_eq!(
            string_to_sign,
            concat!(
                "GET\n",
                "\n",
                "\n",
                "1710417600\n", // Expires from query string, not Date header
                "/mybucket/myobject",
            )
        );

        // Sanity-check: a non-empty signature is produced for this input
        let sig = calculate_signature(SECRET_KEY, &string_to_sign);
        assert_ne!(sig.len(), 0);
    }
}

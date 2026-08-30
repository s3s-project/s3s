// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::normalize_minio_bools;

#[test]
fn uppercase_true_is_normalized() {
    let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<PolicyStatus xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsPublic>TRUE</IsPublic></PolicyStatus>";
    let expected = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<PolicyStatus xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsPublic>true</IsPublic></PolicyStatus>";
    assert_eq!(normalize_minio_bools(Some(body)).as_deref(), Some(&expected[..]));
}

#[test]
fn uppercase_false_is_normalized() {
    let body = b"<PolicyStatus><IsPublic>FALSE</IsPublic></PolicyStatus>";
    let expected = b"<PolicyStatus><IsPublic>false</IsPublic></PolicyStatus>";
    assert_eq!(normalize_minio_bools(Some(body)).as_deref(), Some(&expected[..]));
}

#[test]
fn lowercase_body_is_untouched() {
    let body = b"<PolicyStatus><IsPublic>false</IsPublic></PolicyStatus>";
    assert_eq!(normalize_minio_bools(Some(body)), None);
}

#[test]
fn body_without_boolean_elements_is_untouched() {
    let body = b"<ListBucketResult><Name>bucket</Name></ListBucketResult>";
    assert_eq!(normalize_minio_bools(Some(body)), None);
}

#[test]
fn multiple_occurrences_are_all_normalized() {
    let body = b"<a><IsPublic>TRUE</IsPublic></a><b><IsPublic>FALSE</IsPublic></b>";
    let expected = b"<a><IsPublic>true</IsPublic></a><b><IsPublic>false</IsPublic></b>";
    assert_eq!(normalize_minio_bools(Some(body)).as_deref(), Some(&expected[..]));
}

#[test]
fn longer_prefix_values_are_left_alone() {
    let body = b"<IsPublic>TRUEISH</IsPublic>";
    assert_eq!(normalize_minio_bools(Some(body)), None);
}

#[test]
fn empty_body_is_untouched() {
    assert_eq!(normalize_minio_bools(Some(b"")), None);
}

#[test]
fn streaming_body_is_untouched() {
    assert_eq!(normalize_minio_bools(None), None);
}

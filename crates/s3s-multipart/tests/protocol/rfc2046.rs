// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::common::{parse_all, take_file_data};
use futures::executor::block_on;
use s3s_multipart::Error;

#[test]
fn minimal_body_with_single_field() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert!(form.completed);
    assert_eq!(form.parts.len(), 1);
    assert_eq!(form.parts[0].headers[0].0, b"Content-Disposition");
    assert_eq!(form.parts[0].data, b"value");
}

#[test]
fn preamble_is_skipped() {
    let form = block_on(parse_all(
        b"discarded preamble\r\n--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 1);
    assert_eq!(form.parts[0].data, b"1");
}

#[test]
fn transport_padding_is_accepted() {
    let form = block_on(parse_all(
        b"--boundary \t\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--boundary--\t\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].data, b"1");
}

#[test]
fn empty_header_and_empty_body_parts_are_valid() {
    let form = block_on(parse_all(
        b"--boundary\r\n\r\n\r\n--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 2);
    assert_eq!(form.parts[0].headers.len(), 0);
    assert_eq!(form.parts[0].data, b"");
    assert_eq!(form.parts[1].data, b"");
}

#[test]
fn boundary_must_start_a_line() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxx--boundaryyy\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].data, b"xx--boundaryyy");
}

#[test]
fn missing_first_boundary_is_invalid() {
    let err = block_on(parse_all(b"not multipart", 1024)).unwrap_err();
    assert!(matches!(err, Error::InvalidFormat));
}

#[test]
fn file_data_ends_before_the_delimiter_crlf() {
    let (data, _) = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(data, b"hello");
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::common::parse_all;
use futures::executor::block_on;
use s3s_multipart::parse_content_disposition;

/// Section 4.8 tells senders not to emit header fields other than
/// `Content-Disposition`, `Content-Type` and `Content-Transfer-Encoding`, and
/// tells receivers to ignore them. A third field is the last one the parser
/// reads, so a part that stays within that set — conforming or not — is
/// accepted.
#[test]
fn extra_headers_are_accepted() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Type: text/plain\r\nX-Extra: ignored\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].headers.len(), 3);
    assert_eq!(form.parts[0].data, b"value");
}

#[test]
fn content_disposition_tolerance_matrix() {
    type Case<'a> = (&'a [u8], Option<&'a [u8]>, Option<&'a [u8]>);
    let cases: &[Case<'_>] = &[
        (b"FORM-DATA; FILENAME=\"a.txt\"; NAME=\"file\"", Some(b"file"), Some(b"a.txt")),
        (b"form-data; name=file; filename=a.txt", Some(b"file"), Some(b"a.txt")),
        (b"form-data; size=1; name=\"file\"; filename*=UTF-8''a.txt", Some(b"file"), None),
        (b"form-data; filename=\"a.txt\"", None, Some(b"a.txt")),
    ];

    for (input, name, file_name) in cases {
        let cd = parse_content_disposition(input).unwrap();
        assert_eq!(cd.name, *name);
        assert_eq!(cd.file_name, *file_name);
    }
}

#[test]
fn content_type_optional() {
    // RFC 7578 defaults the part type to `text/plain` when no Content-Type is
    // present, so the part must parse with its Content-Disposition alone.
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 1);
    assert_eq!(form.parts[0].headers.len(), 1);
    assert_eq!(form.parts[0].headers[0].0, b"Content-Disposition");
    assert_eq!(form.parts[0].data, b"value");
}

#[test]
fn missing_name_is_a_part_with_none_name() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 1);
    assert_eq!(form.parts[0].data, b"value");

    // The part is still produced, but the `name` parameter is absent: both
    // parameters must come back as `None`. RFC 7578 requires `name`; the
    // parser reports what it found instead of rejecting the part.
    let (header_name, header_value) = form.parts[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"content-disposition"))
        .expect("the part carries a Content-Disposition header");
    assert_eq!(header_name, b"Content-Disposition");
    let content_disposition = parse_content_disposition(header_value).expect("form-data is accepted");
    assert_eq!(content_disposition.name, None);
    assert_eq!(content_disposition.file_name, None);
}

#[test]
fn same_name_parts_are_not_merged() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n2\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 2);
    assert_eq!(form.parts[0].data, b"1");
    assert_eq!(form.parts[1].data, b"2");
}

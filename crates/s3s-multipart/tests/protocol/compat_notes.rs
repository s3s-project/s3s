// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::common::{body_stream, boundary, collect_data_stream, parse_all, take_file_data};
use bytes::Bytes;
use futures::executor::block_on;
use futures::stream;
use s3s_multipart::{Error, Multipart};

#[test]
fn take_terminates_multipart() {
    block_on(async {
        let mut multipart = Multipart::new(
            body_stream(
                b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary--\r\n",
                1024,
            ),
            &boundary(),
            4096,
        );

        let mut part = multipart.next_part().await.unwrap().unwrap();
        while part.next_header().await.unwrap().is_some() {}
        let stream = part.take_data_stream().unwrap();
        assert_eq!(collect_data_stream(stream).await.unwrap(), b"hello");
        assert!(matches!(multipart.next_part().await, Err(Error::StreamAlreadyTaken)));
    });
}

/// A part carrying the three header fields RFC 7578 section 4.8 defines is
/// accepted — the shape the legacy parser's `[EMPTY_HEADER; 2]` array used to
/// reject.
#[test]
fn a_part_at_the_header_limit_is_accepted() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: binary\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].headers.len(), 3);
    assert_eq!(form.parts[0].data, b"value");
}

/// A fourth field is ignored rather than rejected, as section 4.8 requires of
/// a receiver ("MUST be ignored"): the part is still accepted and keeps its
/// first three fields, `Content-Disposition` among them.
#[test]
fn headers_beyond_the_limit_are_ignored() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"a\"\r\nX-1: 1\r\nX-2: 2\r\nX-3: 3\r\n\r\nvalue\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].headers.len(), 3);
    assert_eq!(form.parts[0].headers[0].0, b"Content-Disposition");
    assert_eq!(form.parts[0].headers[2].0, b"X-2");
    assert_eq!(form.parts[0].data, b"value");
}

/// A parsed header field as `(name, value)`.
type Header = (Vec<u8>, Vec<u8>);

/// Reads one part whose header block is exactly `fields`, in a body with one
/// extra field appended per `surplus`, and returns its headers as
/// `(name, value)` pairs. The body is built at runtime because the field shapes
/// below are not all literals.
fn headers_of_part_result(fields: &[&str], surplus: usize) -> Result<Vec<Header>, Error> {
    let mut body = Vec::new();
    body.extend_from_slice(b"--boundary\r\n");
    for field in fields {
        body.extend_from_slice(field.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    for idx in 0..surplus {
        body.extend_from_slice(format!("X-Surplus-{idx}: filler\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\nvalue\r\n--boundary--\r\n");

    let mut multipart = Multipart::new(stream::iter([Ok::<Bytes, Error>(Bytes::from(body))]), &boundary(), 4096);

    block_on(async {
        let mut part = multipart.next_part().await?.ok_or(Error::InvalidFormat)?;
        let mut headers = Vec::new();
        while let Some(header) = part.next_header().await? {
            headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
        }
        // Self-verification: a parsed part must also reach its data, so a
        // comparison cannot pass by stopping at an error.
        let mut data = Vec::new();
        while let Some(chunk) = part.next_data().await? {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, b"value");
        Ok(headers)
    })
}

/// The header block is read by two independent implementations: `httparse`
/// while the part fits in the window, and the hand-written fallback once a
/// fourth field makes `httparse` give up. Both must expose the same fields for
/// the same field lines, whatever those look like — `Part` pins the same
/// property for its poll and async paths, and this pair has already diverged
/// once (on the offset of an empty header block).
#[test]
fn a_part_over_the_header_limit_exposes_the_same_fields() {
    // Field shapes where the fallback's own OWS handling could disagree with
    // `httparse`, plus the plain and empty-value cases.
    let shapes = [
        "X-A:  \t padded \t ",
        "X-A:",
        "X-A:tight",
        "X-A: a:b",
        "X-A: caf\u{e9}",
        "X-A: \t ",
    ];

    for shape in shapes {
        let fields = [
            "Content-Disposition: form-data; name=\"a\"",
            "Content-Type: text/plain",
            shape,
        ];
        // No surplus field: the part fits the window and `httparse` reads it.
        let at_limit = headers_of_part_result(&fields, 0).expect("the part at the limit parses");
        // One field more: the same three lines, read by the fallback.
        let over_limit = headers_of_part_result(&fields, 1).expect("the part over the limit parses");

        assert_eq!(at_limit.len(), 3, "at the limit, shape {shape:?}");
        assert_eq!(over_limit.len(), 3, "over the limit, shape {shape:?}");
        assert_eq!(at_limit, over_limit, "shape {shape:?}");
    }
}

#[test]
fn preamble_and_transport_padding_are_tolerated() {
    let form = block_on(parse_all(
        b"preamble\r\n--boundary \t\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nvalue\r\n--boundary--\t\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts[0].data, b"value");
}

#[test]
fn empty_parts_are_valid() {
    let form = block_on(parse_all(b"--boundary\r\n\r\n\r\n--boundary--\r\n", 1024)).unwrap();
    assert_eq!(form.parts.len(), 1);
    assert_eq!(form.parts[0].headers.len(), 0);
    assert_eq!(form.parts[0].data, b"");
}

#[test]
fn content_disposition_is_tolerant() {
    let cd = s3s_multipart::parse_content_disposition(b"FORM-DATA; FILENAME=\"a.txt\"; NAME=\"file\"; unknown=x").unwrap();
    assert_eq!(cd.name, Some(&b"file"[..]));
    assert_eq!(cd.file_name, Some(&b"a.txt"[..]));
}

/// The compatibility note about the strict header grammar: a folded field (a
/// continuation line starting with optional whitespace) and a field name with
/// leading optional whitespace are rejected with `InvalidFormat` — on the
/// `httparse` path and, for the folded shape, before the fallback is reached.
#[test]
fn folded_and_indented_field_lines_are_rejected() {
    let shapes: [&[&str]; 2] = [
        // `X-A: a` continued on the next line.
        &["Content-Disposition: form-data; name=\"a\"", "X-A: a", "\tb"],
        // A field name that starts with a tab.
        &["Content-Disposition: form-data; name=\"a\"", "\tX-A: v", "X-B: b"],
    ];

    for fields in shapes {
        // Both with and without a surplus field, so the shape is judged the same
        // whether it fits the window or overflows it.
        for surplus in [0, 1, 2] {
            let err = headers_of_part_result(fields, surplus).expect_err("the part must be rejected");
            assert!(matches!(err, Error::InvalidFormat), "fields {fields:?} surplus {surplus}: {err}");
        }
    }
}

#[test]
fn buffer_limit_applies_to_headers_only() {
    block_on(async {
        let mut multipart = Multipart::new(
            body_stream(
                b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n0123456789\r\n--boundary--\r\n",
                4,
            ),
            &boundary(),
            128,
        );
        let mut part = multipart.next_part().await.unwrap().unwrap();
        while let Some(header) = part.next_header().await.unwrap() {
            let _ = header;
        }
        let mut data = Vec::new();
        while let Some(chunk) = part.next_data().await.unwrap() {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, b"0123456789");
    });
}

#[test]
fn file_data_is_zero_copy_shape() {
    let (data, _) = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary--\r\n",
        1,
    ))
    .unwrap();
    assert_eq!(data, b"hello");
    let _ = Bytes::new();
}

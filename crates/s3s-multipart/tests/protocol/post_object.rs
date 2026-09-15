// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::common::{body_stream, boundary, parse_all, take_file_data};
use bytes::Bytes;
use futures::executor::block_on;
use futures::stream;
use futures_util::StreamExt;
use s3s_multipart::{Error, Multipart};

#[test]
fn canonical_post_object_form_is_parsed() {
    let form = block_on(parse_all(
        b"--boundary\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nuser/file\r\n--boundary\r\nContent-Disposition: form-data; name=\"policy\"\r\n\r\ncG9saWN5\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(form.parts.len(), 3);
    assert_eq!(form.parts[0].data, b"user/file");
    assert_eq!(form.parts[1].data, b"cG9saWN5");
    assert_eq!(form.parts[2].data, b"hello");
}

#[test]
fn file_last_with_strict_trailer() {
    let (data, consumed) = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(data, b"hello");
    assert!(consumed > 0);
}

#[test]
fn multipart_consumed_derives_the_exact_file_length() {
    const FILE_DATA: &[u8] = b"hello file data";
    let body: &'static [u8] = b"--boundary\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello file data\r\n--boundary--\r\n";
    let total = body.len() as u64;
    // The strict closing trailer is `\r\n--boundary--\r\n`.
    let trailer = b"boundary".len() as u64 + 8;

    for chunk_size in [1usize, 2, 3, 5, 7, 16, 1024] {
        let (data, consumed) = block_on(take_file_data(body, chunk_size)).unwrap();
        assert_eq!(data, FILE_DATA, "chunk_size={chunk_size}");

        // Callers derive the exact file length from the consumed offset and
        // the total body length.
        let derived = total
            .checked_sub(consumed)
            .and_then(|value| value.checked_sub(trailer))
            .expect("the consumed offset is within the body");
        assert_eq!(derived, FILE_DATA.len() as u64, "chunk_size={chunk_size}");
    }
}

#[test]
fn epilogue_after_file_is_rejected() {
    let err = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello\r\n--boundary--\r\nepilogue",
        1024,
    ))
    .unwrap_err();
    assert!(matches!(err, Error::StreamPartNotLast));
}

#[test]
fn second_file_part_is_rejected() {
    let err = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nworld\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap_err();
    assert!(matches!(err, Error::StreamPartNotLast));
}

#[test]
fn truncated_file_is_reported() {
    let err = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhello\r\n--boundary--",
        1024,
    ))
    .unwrap_err();
    assert!(matches!(err, Error::IncompleteStreamPart));
}

#[test]
fn file_with_three_headers_is_accepted() {
    let (data, _) = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: binary\r\n\r\nhello\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert_eq!(data, b"hello");
}

#[test]
fn zero_length_file_is_accepted() {
    let (data, _) = block_on(take_file_data(
        b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n\r\n--boundary--\r\n",
        1024,
    ))
    .unwrap();
    assert!(data.is_empty());
}

/// `into_final` may be called before the taken stream is drained. Whatever the
/// caller did not read yet must still arrive — including bytes the stream has
/// already pulled from the body and kept in its internal buffer, which is the
/// only state in which the strict stream starts with a non-empty buffer.
#[test]
fn into_final_delivers_the_data_that_was_not_read_yet() {
    const BODY: &[u8] = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello file data\r\n--boundary--\r\n";
    const FILE_DATA: &[u8] = b"hello file data";

    for chunk_size in [1usize, 2, 3, 5, 12, 13, 4096] {
        for polls in 0..=3 {
            block_on(async {
                let mut multipart = Multipart::new(body_stream(BODY, chunk_size), &boundary(), 4096);
                let mut part = multipart.next_part().await.unwrap().unwrap();
                while part.next_header().await.unwrap().is_some() {}
                let mut stream = part.take_data_stream().unwrap();

                let mut data = Vec::new();
                for _ in 0..polls {
                    match stream.next().await {
                        Some(Ok(chunk)) => data.extend_from_slice(&chunk),
                        Some(Err(err)) => panic!("chunk_size={chunk_size} polls={polls}: {err}"),
                        None => break,
                    }
                }

                let mut final_stream = stream.into_final();
                while let Some(item) = final_stream.next().await {
                    data.extend_from_slice(&item.unwrap());
                }
                assert_eq!(data, FILE_DATA, "chunk_size={chunk_size} polls={polls}");
            });
        }
    }
}

/// The data paths never hand out an empty chunk: when the delimiter is already
/// at the front there is no item at all, so a caller's `while let Some(chunk)`
/// loop is never fed a no-op chunk and cannot mistake one for the end of the
/// stream. Swept over every split from one byte up to the whole body, on the
/// `next_data` path and on the taken stream (including its strict trailer).
#[test]
fn file_data_chunks_are_never_empty() {
    const BODY: &[u8] = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello file data\r\n--boundary--\r\n";
    const FILE_DATA: &[u8] = b"hello file data";

    for chunk_size in 1..=BODY.len() {
        block_on(async {
            let mut multipart = Multipart::new(body_stream(BODY, chunk_size), &boundary(), 4096);
            let mut part = multipart.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}

            let mut data = Vec::new();
            while let Some(chunk) = part.next_data().await.unwrap() {
                assert!(!chunk.is_empty(), "next_data yielded an empty chunk at chunk_size={chunk_size}");
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, FILE_DATA, "next_data chunk_size={chunk_size}");
        });

        block_on(async {
            let mut multipart = Multipart::new(body_stream(BODY, chunk_size), &boundary(), 4096);
            let mut part = multipart.next_part().await.unwrap().unwrap();
            while part.next_header().await.unwrap().is_some() {}
            let mut stream = part.take_data_stream().unwrap();

            let mut data = Vec::new();
            while let Some(item) = stream.next().await {
                let chunk = item.unwrap();
                assert!(!chunk.is_empty(), "PartDataStream yielded an empty chunk at chunk_size={chunk_size}");
                data.extend_from_slice(&chunk);
            }
            let mut final_stream = stream.into_final();
            while let Some(item) = final_stream.next().await {
                let chunk = item.unwrap();
                assert!(!chunk.is_empty(), "FinalPartDataStream yielded an empty chunk at chunk_size={chunk_size}");
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, FILE_DATA, "taken stream chunk_size={chunk_size}");
        });
    }

    // A part with no data yields no item at all, rather than an empty one.
    block_on(async {
        let mut multipart = Multipart::new(
            body_stream(
                b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n\r\n--boundary--\r\n",
                1,
            ),
            &boundary(),
            4096,
        );
        let mut part = multipart.next_part().await.unwrap().unwrap();
        while part.next_header().await.unwrap().is_some() {}
        assert!(part.next_data().await.unwrap().is_none());
    });
}

/// A taken stream that already failed must not be able to report success
/// through `into_final`: the failure is terminal, so the strict stream reports
/// the incomplete part instead of validating a trailer it never reached. The
/// underlying stream here keeps producing after the error, which a `Stream` is
/// allowed to do.
#[test]
fn into_final_does_not_rescue_a_failed_taken_stream() {
    block_on(async {
        let pieces: Vec<Result<Bytes, Error>> = vec![
            Ok(Bytes::from_static(
                b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nhel",
            )),
            Err(Error::InvalidFormat),
            Ok(Bytes::from_static(b"lo\r\n--boundary--\r\n")),
        ];
        let mut multipart = Multipart::new(stream::iter(pieces), &boundary(), 4096);
        let mut part = multipart.next_part().await.unwrap().unwrap();
        while part.next_header().await.unwrap().is_some() {}
        let mut stream = part.take_data_stream().unwrap();

        let mut failed = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "the taken stream must report the failure");

        let mut final_stream = stream.into_final();
        let mut err = None;
        while let Some(item) = final_stream.next().await {
            if let Err(e) = item {
                err = Some(e);
                break;
            }
        }
        assert!(matches!(err, Some(Error::IncompleteStreamPart)), "got {err:?}");
    });
}

/// A chunk above `ADAPTIVE_TAIL_THRESHOLD` whose tail is not a delimiter prefix
/// is emitted whole, which leaves the strict stream with an empty buffer. The
/// next chunk then takes the chunk arm's `Emit` branch — provided that chunk
/// holds no delimiter either, so the payload has to span at least three chunks.
/// That branch is the one piece of the strict stream's data path the split
/// sweep above cannot reach, because it always starts from a buffer holding the
/// tail of the previous chunk.
#[test]
fn a_large_chunk_reaches_the_strict_streams_chunk_emit() {
    const HEAD: &[u8] = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n";
    const PAYLOAD: usize = 15_000;

    let mut body = Vec::new();
    body.extend_from_slice(HEAD);
    body.extend_from_slice(&vec![b'x'; PAYLOAD]);
    body.extend_from_slice(b"\r\n--boundary--\r\n");

    for chunk_size in [4096usize, 6000] {
        for polls in [0usize, 1] {
            block_on(async {
                let pieces: Vec<Result<Bytes, Error>> = body
                    .chunks(chunk_size)
                    .map(|piece| Ok(Bytes::copy_from_slice(piece)))
                    .collect();
                let mut multipart = Multipart::new(stream::iter(pieces), &boundary(), 64 * 1024);
                let mut part = multipart.next_part().await.unwrap().unwrap();
                while part.next_header().await.unwrap().is_some() {}
                let mut stream = part.take_data_stream().unwrap();

                let mut data = Vec::new();
                for _ in 0..polls {
                    match stream.next().await {
                        Some(Ok(chunk)) => data.extend_from_slice(&chunk),
                        other => panic!("chunk_size={chunk_size} polls={polls}: {other:?}"),
                    }
                }

                let mut final_stream = stream.into_final();
                while let Some(item) = final_stream.next().await {
                    data.extend_from_slice(&item.unwrap());
                }
                assert_eq!(data.len(), PAYLOAD, "chunk_size={chunk_size} polls={polls}");
                assert!(data.iter().all(|byte| *byte == b'x'), "chunk_size={chunk_size} polls={polls}");
            });
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use bytes::Bytes;
use futures::stream;
use futures_core::Stream;
use s3s_multipart::{Boundary, Error, Multipart};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedPart {
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub data: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct ParsedForm {
    pub parts: Vec<ParsedPart>,
    pub completed: bool,
}

pub fn boundary() -> Boundary {
    Boundary::new(b"boundary").unwrap()
}

pub fn body_stream(body: &'static [u8], chunk_size: usize) -> impl futures_core::Stream<Item = Result<Bytes, Error>> {
    let chunks: Vec<Result<Bytes, Error>> = body
        .chunks(chunk_size.max(1))
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    stream::iter(chunks)
}

pub async fn parse_all(body: &'static [u8], chunk_size: usize) -> Result<ParsedForm, Error> {
    let mut multipart = Multipart::new(body_stream(body, chunk_size), &boundary(), 4096);
    let mut form = ParsedForm::default();

    while let Some(mut part) = multipart.next_part().await? {
        let mut parsed = ParsedPart::default();
        while let Some(header) = part.next_header().await? {
            parsed.headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
        }
        while let Some(chunk) = part.next_data().await? {
            parsed.data.extend_from_slice(&chunk);
        }
        form.parts.push(parsed);
    }

    form.completed = true;
    Ok(form)
}

pub async fn take_file_data(body: &'static [u8], chunk_size: usize) -> Result<(Vec<u8>, u64), Error> {
    let mut multipart = Multipart::new(body_stream(body, chunk_size), &boundary(), 4096);

    loop {
        let Some(mut part) = multipart.next_part().await? else {
            return Err(Error::InvalidFormat);
        };

        let mut name = None;
        while let Some(header) = part.next_header().await? {
            if header.name.eq_ignore_ascii_case("content-disposition")
                && let Some(cd) = s3s_multipart::parse_content_disposition(header.value)
            {
                name = cd.name.map(|value| String::from_utf8_lossy(value).into_owned());
            }
        }

        if name.as_deref() == Some("file") {
            // S3 POST Object: the file part must be the last part, so the
            // taken stream is converted into the strict final stream.
            let mut stream = part.take_data_stream()?;
            let consumed = stream.multipart_consumed();
            let data = collect_data_stream(&mut stream).await?;
            let final_stream = stream.into_final();
            collect_data_stream(final_stream).await?;
            return Ok((data, consumed));
        }

        while part.next_data().await?.is_some() {}
    }
}

pub async fn collect_data_stream<St>(mut stream: St) -> Result<Vec<u8>, Error>
where
    St: Stream<Item = Result<Bytes, Error>> + Unpin,
{
    let mut data = Vec::new();
    while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
        data.extend_from_slice(&item?);
    }
    Ok(data)
}

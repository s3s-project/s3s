// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

pub fn consume<I, O, F>(input: &mut I, f: F) -> Result<O, nom::Err<nom::error::Error<I>>>
where
    F: FnOnce(I) -> nom::IResult<I, O>,
    I: Copy,
{
    let (remaining, output) = f(*input)?;
    *input = remaining;
    Ok(output)
}

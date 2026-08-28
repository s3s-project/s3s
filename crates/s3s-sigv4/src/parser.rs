// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#[derive(Debug)]
pub struct Error;

#[inline(always)]
pub fn digit2(x: [u8; 2]) -> Result<u8, Error> {
    let x0 = digit(x[0])?;
    let x1 = digit(x[1])?;
    Ok(x0 * 10 + x1)
}

#[inline(always)]
pub fn digit4(x: [u8; 4]) -> Result<u16, Error> {
    let x0 = u16::from(digit2([x[0], x[1]])?);
    let x1 = u16::from(digit2([x[2], x[3]])?);
    Ok(x0 * 100 + x1)
}

#[inline(always)]
fn digit(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        _ => Err(Error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit2_digit4_parse_digits() {
        assert_eq!(digit2(*b"12").unwrap(), 12);
        assert_eq!(digit4(*b"2024").unwrap(), 2024);
    }

    #[test]
    fn digit2_digit4_reject_non_digit() {
        assert!(digit2(*b"1%").is_err());
        assert!(digit4(*b"20a4").is_err());
    }
}

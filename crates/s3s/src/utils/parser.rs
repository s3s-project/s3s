// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

pub struct Error;

#[inline(always)]
fn digit(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        _ => Err(Error),
    }
}

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

pub fn consume<I, O, F>(input: &mut I, f: F) -> Result<O, nom::Err<nom::error::Error<I>>>
where
    F: FnOnce(I) -> nom::IResult<I, O>,
    I: Copy,
{
    let (remaining, output) = f(*input)?;
    *input = remaining;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_accepts_ascii_digits() {
        for c in b'0'..=b'9' {
            assert!(matches!(digit(c), Ok(v) if v == c - b'0'));
        }
    }

    #[test]
    fn digit_rejects_non_digit_without_overflow() {
        // Regression for the fuzz-discovered panic: the subtraction used to
        // be eager, so bytes below b'0' underflowed in overflow-checked
        // builds.
        for c in [b'%', 0x01, 0x00, 0x2f, 0x3a, 0x7f, 0xff, b'a'] {
            assert!(digit(c).is_err(), "byte {c:#04x} must be rejected");
        }
    }

    #[test]
    fn digit2_digit4_reject_non_digit() {
        assert!(digit2(*b"1%").is_err());
        assert!(digit4(*b"201%").is_err());
        assert!(digit4(*b"20\x014").is_err());
    }
}

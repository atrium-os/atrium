//! ASCIIHexDecode and ASCII85Decode.

use super::Capped;
use crate::lexer::is_white;
use crate::{Error, Result};

pub fn hex(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Capped::new(limit, data.len() / 2);
    let mut hi: Option<u8> = None;
    for &b in data {
        if b == b'>' {
            break;
        }
        let Some(d) = (b as char).to_digit(16) else { continue };
        match hi.take() {
            None => hi = Some(d as u8),
            Some(h) => out.push(h << 4 | d as u8)?,
        }
    }
    if let Some(h) = hi {
        out.push(h << 4)?;
    }
    Ok(out.v)
}

pub fn a85(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Capped::new(limit, data.len() * 4 / 5);
    let mut group = [0u8; 5];
    let mut n = 0;
    let mut i = 0;
    // An optional "<~" start.
    if data.starts_with(b"<~") {
        i = 2;
    }
    while i < data.len() {
        let b = data[i];
        i += 1;
        if is_white(b) {
            continue;
        }
        if b == b'~' {
            break;
        }
        if b == b'z' && n == 0 {
            out.extend(&[0; 4])?;
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return Err(Error::Syntax { offset: i as u64, what: "a byte outside ASCII85" });
        }
        group[n] = b - b'!';
        n += 1;
        if n == 5 {
            let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64);
            if v > u32::MAX as u64 {
                return Err(Error::Syntax { offset: i as u64, what: "an ASCII85 group too large" });
            }
            out.extend(&(v as u32).to_be_bytes())?;
            n = 0;
        }
    }
    if n > 1 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64).min(u32::MAX as u64);
        out.extend(&(v as u32).to_be_bytes()[..n - 1])?;
    }
    Ok(out.v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both() {
        assert_eq!(hex(b"48 65 6c 6C 6f>", 99).unwrap(), b"Hello");
        assert_eq!(hex(b"4", 99).unwrap(), vec![0x40]);
        // "Man is d" -> 9jqo^BlbD-
        assert_eq!(a85(b"9jqo^BlbD-~>", 99).unwrap(), b"Man is d");
        assert_eq!(a85(b"9jqo^Bl~>", 99).unwrap(), b"Man i");
        assert_eq!(a85(b"<~z~>", 99).unwrap(), vec![0; 4]);
        assert!(a85(&b"z".repeat(100), 64).is_err());
    }
}

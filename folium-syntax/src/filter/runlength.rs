//! RunLengthDecode.

use super::Capped;
use crate::Result;

pub fn decode(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Capped::new(limit, data.len() * 2);
    let mut i = 0;
    while i < data.len() {
        let n = data[i];
        i += 1;
        match n {
            128 => break,
            0..=127 => {
                let k = (n as usize + 1).min(data.len() - i);
                out.extend(&data[i..i + k])?;
                i += k;
            }
            _ => {
                let Some(&b) = data.get(i) else { break };
                i += 1;
                for _ in 0..(257 - n as usize) {
                    out.push(b)?;
                }
            }
        }
    }
    Ok(out.v)
}

#[cfg(test)]
mod tests {
    #[test]
    fn runs() {
        assert_eq!(super::decode(&[2, b'a', b'b', b'c', 254, b'x', 128], 99).unwrap(), b"abcxxx");
    }
}

//! LZWDecode (ISO 32000-2 §7.4.4), 9- to 12-bit codes, early change
//! by default.

use super::Capped;
use crate::Result;

pub fn decode(data: &[u8], early: bool, limit: usize) -> Result<Vec<u8>> {
    let mut out = Capped::new(limit, data.len() * 3);
    // The table: each entry is (prefix index, last byte, length).
    let mut prefix: Vec<u16> = Vec::with_capacity(4096);
    let mut last: Vec<u8> = Vec::with_capacity(4096);
    let mut lens: Vec<u16> = Vec::with_capacity(4096);
    let reset = |prefix: &mut Vec<u16>, last: &mut Vec<u8>, lens: &mut Vec<u16>| {
        prefix.clear();
        last.clear();
        lens.clear();
        for i in 0..256u16 {
            prefix.push(u16::MAX);
            last.push(i as u8);
            lens.push(1);
        }
        // 256 clear, 257 end.
        for _ in 0..2 {
            prefix.push(u16::MAX);
            last.push(0);
            lens.push(0);
        }
    };
    reset(&mut prefix, &mut last, &mut lens);
    let (mut bits, mut nbits) = (0u32, 0u32);
    let mut width = 9u32;
    let mut prev: Option<u16> = None;
    let mut scratch = Vec::with_capacity(4096);
    let mut i = 0;
    loop {
        while nbits < width {
            let Some(&b) = data.get(i) else { return Ok(out.v) };
            i += 1;
            bits = bits << 8 | b as u32;
            nbits += 8;
        }
        let code = ((bits >> (nbits - width)) & ((1 << width) - 1)) as u16;
        nbits -= width;
        bits &= (1 << nbits) - 1;
        match code {
            256 => {
                reset(&mut prefix, &mut last, &mut lens);
                width = 9;
                prev = None;
                continue;
            }
            257 => return Ok(out.v),
            _ => {}
        }
        let next = prefix.len() as u16;
        let (entry_code, first) = if (code as usize) < prefix.len() && lens[code as usize] > 0 {
            (code, None)
        } else if code == next && prev.is_some() {
            // KwKwK: the previous string plus its own first byte.
            (prev.unwrap(), Some(()))
        } else {
            return Ok(out.v); // a code from nowhere: stop, keep what we have
        };
        scratch.clear();
        let mut c = entry_code;
        while c != u16::MAX {
            scratch.push(last[c as usize]);
            c = prefix[c as usize];
        }
        scratch.reverse();
        if first.is_some() {
            let f = scratch[0];
            scratch.push(f);
        }
        out.extend(&scratch)?;
        if let Some(p) = prev {
            if prefix.len() < 4096 {
                prefix.push(p);
                last.push(scratch[0]);
                lens.push(lens[p as usize].saturating_add(1));
            }
        }
        prev = Some(code);
        let size = prefix.len() as u32 + if early { 1 } else { 0 };
        width = if size >= 2048 { 12 } else if size >= 1024 { 11 } else if size >= 512 { 10 } else { 9 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example() {
        // ISO 32000-2 §7.4.4.2's example: 45 45 45 45 45 65 45 45 45 66.
        let enc = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        assert_eq!(decode(&enc, true, 1 << 10).unwrap(), vec![45, 45, 45, 45, 45, 65, 45, 45, 45, 66]);
    }
}

//! CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320): the same function as
//! core/src/crc.c, slice-by-8, with the tables built at compile time (the C
//! version fills them lazily on first use).

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = (c >> 1) ^ if c & 1 != 0 { 0xedb8_8320 } else { 0 };
            j += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut c = t[0][i];
        let mut k = 1;
        while k < 8 {
            c = t[0][(c & 0xff) as usize] ^ (c >> 8);
            t[k][i] = c;
            k += 1;
        }
        i += 1;
    }
    t
}

static T: [[u32; 256]; 8] = make_tables();

/// Index a table with the low byte of `x`. `x as u8` is < 256 by type, so the
/// bounds check is provably dead and compiles away.
#[inline(always)]
fn t(k: usize, x: u32) -> u32 {
    match T.get(k) {
        Some(row) => row[usize::from(x as u8)],
        None => 0,
    }
}

pub fn update(mut s: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let Ok(&[c0, c1, c2, c3, c4, c5, c6, c7]) = <&[u8; 8]>::try_from(c) else {
            continue; // unreachable: chunks_exact(8) yields 8-byte chunks
        };
        let a = s ^ u32::from_le_bytes([c0, c1, c2, c3]);
        let b = u32::from_le_bytes([c4, c5, c6, c7]);
        s = t(7, a) ^ t(6, a >> 8) ^ t(5, a >> 16) ^ t(4, a >> 24)
            ^ t(3, b) ^ t(2, b >> 8) ^ t(1, b >> 16) ^ t(0, b >> 24);
    }
    for &byte in chunks.remainder() {
        s = (s >> 8) ^ t(0, s ^ u32::from(byte));
    }
    s
}

/// `tessera_crc32()`.
pub fn crc32(data: &[u8]) -> u32 {
    !update(0xffff_ffff, data)
}

/// True if the little-endian u32 at `off` equals the CRC of `b[..off]`.
/// False (never a panic) if `off + 4` runs past `b`.
pub fn stored_matches(b: &[u8], off: usize) -> bool {
    match (b.get(..off), crate::le::u32_at(b, off)) {
        (Some(d), Some(v)) => crc32(d) == v,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn check_value() {
        // The standard CRC-32 check value.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
        let long: [u8; 37] = core::array::from_fn(|i| i as u8);
        let mut s = 0xffff_ffff;
        for b in long.iter() {
            s = update(s, core::slice::from_ref(b));
        }
        assert_eq!(!s, crc32(&long));
    }
}

//! Standard base64 (RFC 4648 §4, with padding): how WireGuard writes keys.

const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// A WireGuard key: exactly 44 characters, one `=` of padding, 32 bytes,
/// and canonical (the last character's two spare bits zero), so one key has
/// one spelling.
pub fn key(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 44 || b[43] != b'=' {
        return None;
    }
    let mut out = [0u8; 32];
    let mut n: u32 = 0;
    let mut bits = 0;
    let mut k = 0;
    for &ch in &b[..43] {
        let v = A.iter().position(|&a| a == ch)? as u32;
        n = (n << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out[k] = (n >> bits) as u8;
            k += 1;
            n &= (1 << bits) - 1;
        }
    }
    // 43 characters carry 258 bits: 256 of key, 2 that must be zero.
    (k == 32 && bits == 2 && n == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_only_canonical_ones_parse() {
        let k: [u8; 32] = std::array::from_fn(|i| (i * 37 + 11) as u8);
        let s = encode(&k);
        assert_eq!(s.len(), 44);
        assert_eq!(key(&s), Some(k));
        // The same bits with a spare bit set: not a key.
        let mut bad = s.clone().into_bytes();
        let last = A.iter().position(|&a| a == bad[42]).unwrap();
        bad[42] = A[last | 1];
        if bad[42] != s.as_bytes()[42] {
            assert_eq!(key(std::str::from_utf8(&bad).unwrap()), None);
        }
        assert_eq!(key(""), None);
        assert_eq!(key(&s[..43]), None);
        assert_eq!(key(&format!("{}=", &s[..43].replace('A', "*"))), if s[..43].contains('A') { None } else { Some(k) });
    }
}

//! EAP-TTLS phase 2 (RFC 5281 §10): Diameter AVPs inside the tunnel.

pub const USER_NAME: u32 = 1;
pub const USER_PASSWORD: u32 = 2;
pub const MICROSOFT: u32 = 311;
pub const MS_CHAP_ERROR: u32 = 2;
pub const MS_CHAP_CHALLENGE: u32 = 11;
pub const MS_CHAP2_RESPONSE: u32 = 25;
pub const MS_CHAP2_SUCCESS: u32 = 26;

const VENDOR: u8 = 0x80;
const MANDATORY: u8 = 0x40;

#[derive(Debug, PartialEq, Eq)]
pub struct Avp<'a> {
    pub code: u32,
    pub vendor: u32,
    pub mandatory: bool,
    pub data: &'a [u8],
}

/// Appends one mandatory AVP, padded to four bytes.
pub fn put(out: &mut Vec<u8>, vendor: u32, code: u32, data: &[u8]) {
    let hdr = if vendor != 0 { 12 } else { 8 };
    let len = hdr + data.len();
    out.extend_from_slice(&code.to_be_bytes());
    let flags = MANDATORY | if vendor != 0 { VENDOR } else { 0 };
    out.push(flags);
    out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
    if vendor != 0 {
        out.extend_from_slice(&vendor.to_be_bytes());
    }
    out.extend_from_slice(data);
    out.resize(out.len() + (4 - len % 4) % 4, 0);
}

/// Every AVP in `b`, or `None` when one runs past the end.
pub fn parse(mut b: &[u8]) -> Option<Vec<Avp<'_>>> {
    let mut v = Vec::new();
    while !b.is_empty() {
        if b.len() < 8 {
            return None;
        }
        let code = u32::from_be_bytes(b[..4].try_into().unwrap());
        let flags = b[4];
        let len = u32::from_be_bytes([0, b[5], b[6], b[7]]) as usize;
        let hdr = if flags & VENDOR != 0 { 12 } else { 8 };
        if len < hdr || len > b.len() {
            return None;
        }
        let vendor = if hdr == 12 { u32::from_be_bytes(b[8..12].try_into().unwrap()) } else { 0 };
        v.push(Avp { code, vendor, mandatory: flags & MANDATORY != 0, data: &b[hdr..len] });
        b = &b[(len + (4 - len % 4) % 4).min(b.len())..];
    }
    Some(v)
}

/// User-Password: zero-padded to a multiple of 16 (RFC 5281 §11.2.5).
pub fn padded_password(password: &str) -> zeroize::Zeroizing<Vec<u8>> {
    let mut p = zeroize::Zeroizing::new(Vec::with_capacity(password.len() + 16));
    p.extend_from_slice(password.as_bytes());
    let n = p.len().max(1).div_ceil(16) * 16;
    p.resize(n, 0);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut b = Vec::new();
        put(&mut b, 0, USER_NAME, b"alice");
        put(&mut b, MICROSOFT, MS_CHAP_CHALLENGE, &[7; 16]);
        assert_eq!(b.len(), 16 + 28);
        assert_eq!(&b[..8], &[0, 0, 0, 1, 0x40, 0, 0, 13]);
        let v = parse(&b).unwrap();
        assert_eq!(v[0], Avp { code: USER_NAME, vendor: 0, mandatory: true, data: b"alice" });
        assert_eq!(v[1], Avp { code: MS_CHAP_CHALLENGE, vendor: MICROSOFT, mandatory: true, data: &[7; 16] });
        assert_eq!(parse(&b[..10]), None);
        assert_eq!(parse(&[0, 0, 0, 1, 0x80, 0, 0, 9, 0]), None, "vendor flag needs 12 bytes");
        assert_eq!(padded_password("").len(), 16);
        assert_eq!(padded_password("0123456789abcdef").len(), 16);
        assert_eq!(padded_password("0123456789abcdefg").len(), 32);
    }
}

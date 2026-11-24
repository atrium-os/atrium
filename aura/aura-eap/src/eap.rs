//! EAP packets (RFC 3748 §4): code, identifier, length, and for requests
//! and responses a type. Everything here is hostile input: lengths are
//! checked against what arrived, never trusted.

pub const REQUEST: u8 = 1;
pub const RESPONSE: u8 = 2;
pub const SUCCESS: u8 = 3;
pub const FAILURE: u8 = 4;

/// EAP method types this peer knows.
pub mod typ {
    pub const IDENTITY: u8 = 1;
    pub const NOTIFICATION: u8 = 2;
    pub const NAK: u8 = 3;
    pub const TLS: u8 = 13;
    pub const TTLS: u8 = 21;
    pub const PEAP: u8 = 25;
    pub const MSCHAPV2: u8 = 26;
    /// PEAP's TLV extensions (Result, Crypto-Binding).
    pub const TLV: u8 = 33;
}

/// The largest EAP packet accepted (an EAPOL body is at most 65535 bytes;
/// a TLS fragment is far smaller).
pub const MAX: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub code: u8,
    pub id: u8,
    /// Requests and responses only.
    pub typ: Option<u8>,
    /// After the type (requests, responses) or after the header.
    pub data: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    Short,
    Length,
    Code(u8),
}

/// One EAP packet. Bytes past the length field are ignored (EAPOL can pad).
pub fn parse(b: &[u8]) -> Result<Packet<'_>, Malformed> {
    if b.len() < 4 {
        return Err(Malformed::Short);
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    if len < 4 || len > b.len() || len > MAX {
        return Err(Malformed::Length);
    }
    let b = &b[..len];
    match b[0] {
        REQUEST | RESPONSE => {
            if len < 5 {
                return Err(Malformed::Length);
            }
            Ok(Packet { code: b[0], id: b[1], typ: Some(b[4]), data: &b[5..] })
        }
        SUCCESS | FAILURE => {
            if len != 4 {
                return Err(Malformed::Length);
            }
            Ok(Packet { code: b[0], id: b[1], typ: None, data: &[] })
        }
        c => Err(Malformed::Code(c)),
    }
}

/// A response of `typ` carrying `data`.
pub fn response(id: u8, typ: u8, data: &[u8]) -> Vec<u8> {
    let len = 5 + data.len();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&[RESPONSE, id]);
    v.extend_from_slice(&(len as u16).to_be_bytes());
    v.push(typ);
    v.extend_from_slice(data);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_checks_lengths() {
        assert_eq!(parse(&[1, 7, 0, 5, 1]).unwrap(), Packet { code: 1, id: 7, typ: Some(1), data: &[] });
        assert_eq!(parse(&[1, 7, 0, 6, 25, 0x20, 0xee]).unwrap().data, &[0x20], "padding past the length ignored");
        assert_eq!(parse(&[3, 9, 0, 4]).unwrap().typ, None);
        assert_eq!(parse(&[1, 7, 0, 9, 1]), Err(Malformed::Length), "longer than what arrived");
        assert_eq!(parse(&[1, 7, 0, 4]), Err(Malformed::Length), "a request with no type");
        assert_eq!(parse(&[3, 9, 0, 5, 0]), Err(Malformed::Length), "a Success with data");
        assert_eq!(parse(&[9, 9, 0, 4]), Err(Malformed::Code(9)));
        assert_eq!(parse(&[1, 2]), Err(Malformed::Short));
        assert_eq!(response(4, typ::IDENTITY, b"me"), vec![2, 4, 0, 7, 1, b'm', b'e']);
    }
}

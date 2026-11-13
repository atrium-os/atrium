//! The Answer To Reset (ISO/IEC 7816-3 §8.2). Parsed only to report which
//! protocols the card offers and its historical bytes; the reader (with
//! automatic parameters) negotiates the rest.

use crate::Error;

pub const MAX: usize = 33;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atr {
    pub bytes: Vec<u8>,
    /// Protocols the card offers (T=0 is implied when no TD names one).
    pub t0: bool,
    pub t1: bool,
    pub historical: Vec<u8>,
}

impl Atr {
    pub fn parse(b: &[u8]) -> Result<Atr, Error> {
        if b.len() < 2 || b.len() > MAX || !matches!(b[0], 0x3B | 0x3F) {
            return Err(Error::Protocol("ATR"));
        }
        let mut i = 1;
        let mut y = b[1] >> 4;
        let k = (b[1] & 0x0F) as usize;
        let (mut t0, mut t1, mut named, mut other) = (false, false, false, false);
        i += 1;
        loop {
            // TA, TB, TC, then TD (which carries the next Y and a protocol).
            for bit in [0x1, 0x2, 0x4] {
                if y & bit != 0 {
                    i += 1;
                }
            }
            if y & 0x8 == 0 {
                break;
            }
            let td = *b.get(i).ok_or(Error::Protocol("ATR: short"))?;
            i += 1;
            named = true;
            match td & 0x0F {
                0 => t0 = true,
                1 => t1 = true,
                _ => {}
            }
            other |= td & 0x0F != 0;
            y = td >> 4;
        }
        if !named {
            t0 = true;
        }
        let historical = b.get(i..i + k).ok_or(Error::Protocol("ATR: short"))?.to_vec();
        i += k;
        // TCK is present unless only T=0 is offered; it makes the XOR of
        // T0..TCK zero.
        if other {
            if b.len() != i + 1 {
                return Err(Error::Protocol("ATR: length"));
            }
            if b[1..].iter().fold(0u8, |a, x| a ^ x) != 0 {
                return Err(Error::Protocol("ATR: TCK"));
            }
        } else if b.len() != i {
            return Err(Error::Protocol("ATR: length"));
        }
        Ok(Atr { bytes: b.to_vec(), t0, t1, historical })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innait() {
        let b = [
            0x3B, 0x8E, 0x80, 0x01, 0x50, 0x4B, 0x03, 0x58, 0x58, 0x58, 0x5F, 0x44, 0x53, 0x43, 0x5F, 0x4B, 0x45, 0x59, 0x4C,
        ];
        let a = Atr::parse(&b).unwrap();
        assert!(a.t0 && a.t1);
        assert_eq!(a.historical, b"PK\x03XXX_DSC_KEY");
        let mut bad = b;
        bad[18] ^= 1;
        assert_eq!(Atr::parse(&bad), Err(Error::Protocol("ATR: TCK")));
        assert!(Atr::parse(&b[..18]).is_err());
    }

    #[test]
    fn t0_only_has_no_tck() {
        // T0 = 0x02: no interface bytes, two historical bytes.
        let a = Atr::parse(&[0x3B, 0x02, 0x14, 0x50]).unwrap();
        assert!(a.t0 && !a.t1);
        assert_eq!(a.historical, vec![0x14, 0x50]);
        assert!(Atr::parse(&[0x3B, 0x02, 0x14]).is_err());
    }

    #[test]
    fn yubikey_style() {
        // 3B F8 13 00 00 81 31 FE 15 59 75 62 69 6B 65 79 34 D4 (YubiKey 4/5).
        let b = [0x3B, 0xF8, 0x13, 0x00, 0x00, 0x81, 0x31, 0xFE, 0x15, 0x59, 0x75, 0x62, 0x69, 0x6B, 0x65, 0x79, 0x34, 0xD4];
        let a = Atr::parse(&b).unwrap();
        assert!(a.t1 && !a.t0);
        assert_eq!(a.historical, b"Yubikey4");
    }

    #[test]
    fn junk() {
        assert!(Atr::parse(&[]).is_err());
        assert!(Atr::parse(&[0x3B]).is_err());
        assert!(Atr::parse(&[0x00, 0x00]).is_err());
        assert!(Atr::parse(&[0x3B; 40]).is_err());
        // Y says TD follows but the ATR ends.
        assert!(Atr::parse(&[0x3B, 0x80]).is_err());
    }
}

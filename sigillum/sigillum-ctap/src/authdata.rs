//! Authenticator data (WebAuthn §6.1): `rpIdHash[32] | flags | signCount[4]
//! | attested credential data? | extensions?`.

use sigillum_cbor::{decode_prefix, Limits, Value};

use crate::cose::CoseKey;
use crate::Error;

pub mod flags {
    pub const UP: u8 = 0x01;
    pub const UV: u8 = 0x04;
    pub const BE: u8 = 0x08;
    pub const BS: u8 = 0x10;
    pub const AT: u8 = 0x40;
    pub const ED: u8 = 0x80;
}

/// Longest credential id WebAuthn allows.
pub const MAX_CRED_ID: usize = 1023;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attested {
    pub aaguid: [u8; 16],
    pub cred_id: Vec<u8>,
    pub public_key: CoseKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub sign_count: u32,
    pub attested: Option<Attested>,
    pub extensions: Option<Value>,
}

impl AuthData {
    pub fn parse(b: &[u8]) -> Result<AuthData, Error> {
        if b.len() < 37 {
            return Err(Error::Protocol("short authenticator data"));
        }
        let rp_id_hash: [u8; 32] = b[..32].try_into().unwrap();
        let flags = b[32];
        let sign_count = u32::from_be_bytes(b[33..37].try_into().unwrap());
        let mut pos = 37;
        let lim = Limits { max_len: 4096, ..Limits::default() };
        let attested = if flags & flags::AT != 0 {
            if b.len() < pos + 18 {
                return Err(Error::Protocol("short attested credential data"));
            }
            let aaguid: [u8; 16] = b[pos..pos + 16].try_into().unwrap();
            let n = u16::from_be_bytes([b[pos + 16], b[pos + 17]]) as usize;
            pos += 18;
            if n == 0 || n > MAX_CRED_ID || b.len() < pos + n {
                return Err(Error::Protocol("credential id length"));
            }
            let cred_id = b[pos..pos + n].to_vec();
            pos += n;
            let (key, used) = decode_prefix(&b[pos..], lim)?;
            pos += used;
            Some(Attested { aaguid, cred_id, public_key: CoseKey::parse(&key)? })
        } else {
            None
        };
        let extensions = if flags & flags::ED != 0 {
            let (ext, used) = decode_prefix(&b[pos..], lim)?;
            if ext.as_map().is_none() {
                return Err(Error::Protocol("extensions are not a map"));
            }
            pos += used;
            Some(ext)
        } else {
            None
        };
        if pos != b.len() {
            return Err(Error::Protocol("trailing bytes after authenticator data"));
        }
        Ok(AuthData { rp_id_hash, flags, sign_count, attested, extensions })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = self.rp_id_hash.to_vec();
        v.push(self.flags);
        v.extend_from_slice(&self.sign_count.to_be_bytes());
        if let Some(a) = &self.attested {
            v.extend_from_slice(&a.aaguid);
            v.extend_from_slice(&(a.cred_id.len() as u16).to_be_bytes());
            v.extend_from_slice(&a.cred_id);
            v.extend_from_slice(&sigillum_cbor::encode(&a.public_key.to_value()));
        }
        if let Some(e) = &self.extensions {
            v.extend_from_slice(&sigillum_cbor::encode(e));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cose::alg;

    fn sample() -> AuthData {
        let sk = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        AuthData {
            rp_id_hash: [7; 32],
            flags: flags::UP | flags::AT | flags::ED,
            sign_count: 3,
            attested: Some(Attested { aaguid: [1; 16], cred_id: vec![9; 40], public_key: CoseKey::from_p256(alg::ES256, &sk.public_key()) }),
            extensions: Some(Value::Map(vec![(Value::text("credProtect"), Value::Uint(2))])),
        }
    }

    #[test]
    fn round_trip() {
        let a = sample();
        assert_eq!(AuthData::parse(&a.encode()).unwrap(), a);
    }

    #[test]
    fn refusals() {
        let a = sample();
        let mut b = a.encode();
        b.push(0);
        assert!(AuthData::parse(&b).is_err(), "trailing byte");
        let b = a.encode();
        assert!(AuthData::parse(&b[..b.len() - 1]).is_err(), "truncated");
        let mut b = a.encode();
        b[32] &= !flags::ED; // claims no extensions, but they are there
        assert!(AuthData::parse(&b).is_err());
        let mut b = a.encode();
        b[53..55].copy_from_slice(&2000u16.to_be_bytes()); // cred id length
        assert!(AuthData::parse(&b).is_err());
        // Every prefix fails cleanly (no panic).
        let b = a.encode();
        for n in 0..b.len() {
            let _ = AuthData::parse(&b[..n]);
        }
    }
}

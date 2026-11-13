//! COSE keys (RFC 9052/9053) as CTAP2 uses them: credential public keys
//! (ES256, EdDSA) and the PIN protocol's key agreement key (ECDH-ES+HKDF-256).

use sigillum_cbor::Value;

use crate::Error;

pub mod alg {
    pub const ES256: i64 = -7;
    pub const EDDSA: i64 = -8;
    /// ECDH-ES + HKDF-256: the PIN protocol key agreement key.
    pub const ECDH_ES_HKDF_256: i64 = -25;
}

const KTY: i64 = 1;
const ALG: i64 = 3;
const CRV: i64 = -1;
const X: i64 = -2;
const Y: i64 = -3;
const KTY_OKP: i64 = 1;
const KTY_EC2: i64 = 2;
const CRV_P256: i64 = 1;
const CRV_ED25519: i64 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoseKey {
    /// A P-256 point (checked on the curve), with its COSE alg (ES256 or
    /// ECDH-ES+HKDF-256).
    P256 { alg: i64, x: [u8; 32], y: [u8; 32] },
    /// An Ed25519 public key.
    Ed25519 { x: [u8; 32] },
}

impl CoseKey {
    pub fn parse(v: &Value) -> Result<CoseKey, Error> {
        let int = |k| v.get_int(k).and_then(Value::as_i64).ok_or(Error::Protocol("COSE key field missing"));
        let b32 = |k| -> Result<[u8; 32], Error> {
            v.get_int(k)
                .and_then(Value::as_bytes)
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .ok_or(Error::Protocol("COSE coordinate"))
        };
        match (int(KTY)?, int(CRV)?) {
            (KTY_EC2, CRV_P256) => {
                let alg = int(ALG)?;
                if alg != alg::ES256 && alg != alg::ECDH_ES_HKDF_256 {
                    return Err(Error::Protocol("unsupported COSE alg"));
                }
                let (x, y) = (b32(X)?, b32(Y)?);
                let k = CoseKey::P256 { alg, x, y };
                k.p256().ok_or(Error::Protocol("point not on P-256"))?;
                Ok(k)
            }
            (KTY_OKP, CRV_ED25519) => {
                if int(ALG)? != alg::EDDSA {
                    return Err(Error::Protocol("unsupported COSE alg"));
                }
                Ok(CoseKey::Ed25519 { x: b32(X)? })
            }
            _ => Err(Error::Protocol("unsupported COSE key type")),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            CoseKey::P256 { alg, x, y } => Value::Map(vec![
                (Value::int(KTY), Value::int(KTY_EC2)),
                (Value::int(ALG), Value::int(*alg)),
                (Value::int(CRV), Value::int(CRV_P256)),
                (Value::int(X), Value::bytes(x)),
                (Value::int(Y), Value::bytes(y)),
            ]),
            CoseKey::Ed25519 { x } => Value::Map(vec![
                (Value::int(KTY), Value::int(KTY_OKP)),
                (Value::int(ALG), Value::int(alg::EDDSA)),
                (Value::int(CRV), Value::int(CRV_ED25519)),
                (Value::int(X), Value::bytes(x)),
            ]),
        }
    }

    pub fn alg(&self) -> i64 {
        match self {
            CoseKey::P256 { alg, .. } => *alg,
            CoseKey::Ed25519 { .. } => alg::EDDSA,
        }
    }

    /// The P-256 public key, if this is one and the point is valid.
    pub fn p256(&self) -> Option<p256::PublicKey> {
        match self {
            CoseKey::P256 { x, y, .. } => {
                let mut sec1 = [0u8; 65];
                sec1[0] = 4;
                sec1[1..33].copy_from_slice(x);
                sec1[33..].copy_from_slice(y);
                p256::PublicKey::from_sec1_bytes(&sec1).ok()
            }
            _ => None,
        }
    }

    pub fn from_p256(alg: i64, k: &p256::PublicKey) -> CoseKey {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let p = k.to_encoded_point(false);
        CoseKey::P256 { alg, x: (*p.x().unwrap()).into(), y: (*p.y().unwrap()).into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_off_curve() {
        let sk = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let k = CoseKey::from_p256(alg::ES256, &sk.public_key());
        assert_eq!(CoseKey::parse(&k.to_value()).unwrap(), k);
        let CoseKey::P256 { x, mut y, .. } = k else { unreachable!() };
        y[31] ^= 1;
        let bad = CoseKey::P256 { alg: alg::ES256, x, y };
        assert!(CoseKey::parse(&bad.to_value()).is_err());
    }
}

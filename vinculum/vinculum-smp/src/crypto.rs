//! LE Secure Connections crypto (Core 6.0 Vol 3 Part H §2.2).
//!
//! Every input and output here is **little-endian, as on air**; the
//! functions reverse to the spec's most-significant-first order inside.
//! Checked against Bumble's independent implementation
//! (`tests/vectors.rs`).

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use cmac::{Cmac, Mac};
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey};
use zeroize::Zeroize;

fn rev<const N: usize>(b: &[u8; N]) -> [u8; N] {
    let mut o = *b;
    o.reverse();
    o
}

fn rev_vec(b: &[u8]) -> Vec<u8> {
    b.iter().rev().copied().collect()
}

/// AES-CMAC (RFC 4493) over `m` with key `k`, both in the spec's byte order
/// (this one does not reverse).
pub fn aes_cmac(k: &[u8; 16], m: &[u8]) -> [u8; 16] {
    let mut mac = <Cmac<Aes128> as Mac>::new_from_slice(k).expect("16-byte key");
    mac.update(m);
    mac.finalize().into_bytes().into()
}

/// Security function e: AES-128 (§2.2.1), little-endian in and out.
pub fn e(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(&rev(key).into());
    let mut block = rev(data).into();
    cipher.encrypt_block(&mut block);
    let out: [u8; 16] = block.into();
    rev(&out)
}

/// Random address hash ah (§2.2.2): 24-bit hash of a 24-bit prand.
pub fn ah(k: &[u8; 16], r: &[u8; 3]) -> [u8; 3] {
    let mut p = [0u8; 16];
    p[..3].copy_from_slice(r);
    let o = e(k, &p);
    [o[0], o[1], o[2]]
}

/// Confirm value f4(U, V, X, Z) (§2.2.6). U, V are public key X
/// coordinates (32 bytes), X a nonce, Z one byte.
pub fn f4(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], z: u8) -> [u8; 16] {
    let mut m = Vec::with_capacity(65);
    m.extend_from_slice(&rev(u));
    m.extend_from_slice(&rev(v));
    m.push(z);
    rev(&aes_cmac(&rev(x), &m))
}

/// Key generation f5(W, N1, N2, A1, A2) (§2.2.7) → (MacKey, LTK).
/// W is the DHKey; A1/A2 are 7 bytes: address (6, little-endian) then type.
pub fn f5(w: &[u8; 32], n1: &[u8; 16], n2: &[u8; 16], a1: &[u8; 7], a2: &[u8; 7]) -> ([u8; 16], [u8; 16]) {
    const SALT: [u8; 16] = [0x6C, 0x88, 0x83, 0x91, 0xAA, 0xF5, 0xA5, 0x38, 0x60, 0x37, 0x0B, 0xDB, 0x5A, 0x60, 0x83, 0xBE];
    let mut t = aes_cmac(&SALT, &rev(w));
    let gen = |counter: u8, t: &[u8; 16]| {
        let mut m = Vec::with_capacity(53);
        m.push(counter);
        m.extend_from_slice(b"btle");
        m.extend_from_slice(&rev(n1));
        m.extend_from_slice(&rev(n2));
        m.extend_from_slice(&rev(a1));
        m.extend_from_slice(&rev(a2));
        m.extend_from_slice(&[0x01, 0x00]); // length 256
        rev(&aes_cmac(t, &m))
    };
    let out = (gen(0, &t), gen(1, &t));
    t.zeroize();
    out
}

/// Check value f6(W, N1, N2, R, IOcap, A1, A2) (§2.2.8). IOcap is the 3
/// bytes IO capability, OOB flag, AuthReq in on-air order.
pub fn f6(w: &[u8; 16], n1: &[u8; 16], n2: &[u8; 16], r: &[u8; 16], io_cap: &[u8; 3], a1: &[u8; 7], a2: &[u8; 7]) -> [u8; 16] {
    let mut m = Vec::with_capacity(65);
    m.extend_from_slice(&rev(n1));
    m.extend_from_slice(&rev(n2));
    m.extend_from_slice(&rev(r));
    m.extend_from_slice(&rev(io_cap));
    m.extend_from_slice(&rev(a1));
    m.extend_from_slice(&rev(a2));
    rev(&aes_cmac(&rev(w), &m))
}

/// Numeric comparison value g2(U, V, X, Y) (§2.2.9); the six digits shown
/// to the user are `g2(..) % 1_000_000`.
pub fn g2(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], y: &[u8; 16]) -> u32 {
    let mut m = Vec::with_capacity(80);
    m.extend_from_slice(&rev(u));
    m.extend_from_slice(&rev(v));
    m.extend_from_slice(&rev(y));
    let o = aes_cmac(&rev(x), &m);
    u32::from_be_bytes([o[12], o[13], o[14], o[15]])
}

/// Our P-256 key pair for one pairing. The secret is dropped (and zeroed by
/// p256) once the DHKey is computed.
pub struct KeyPair {
    secret: Option<EphemeralSecret>,
    /// X then Y, each 32 bytes little-endian, as the Pairing Public Key PDU
    /// carries them.
    pub public: [u8; 64],
}

#[derive(Debug, PartialEq, Eq)]
pub enum EcdhError {
    /// The peer's point is not on P-256 (an invalid-curve attack) or is the
    /// identity.
    InvalidPoint,
    /// The peer sent back our own public key (a reflection attack).
    Reflected,
    /// DHKey already computed.
    Used,
}

fn to_wire(p: &PublicKey) -> [u8; 64] {
    let ep = p.to_encoded_point(false);
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&rev_vec(ep.x().expect("affine")));
    out[32..].copy_from_slice(&rev_vec(ep.y().expect("affine")));
    out
}

fn from_wire(w: &[u8; 64]) -> Option<PublicKey> {
    let x = rev_vec(&w[..32]);
    let y = rev_vec(&w[32..]);
    let ep = EncodedPoint::from_affine_coordinates(x.as_slice().into(), y.as_slice().into(), false);
    Option::from(PublicKey::from_encoded_point(&ep))
}

impl KeyPair {
    pub fn generate() -> KeyPair {
        let secret = EphemeralSecret::random(&mut rand_core::OsRng);
        let public = to_wire(&secret.public_key());
        KeyPair { secret: Some(secret), public }
    }

    /// The DHKey with the peer's public key (wire order), little-endian.
    /// Validates the point (p256 rejects points not on the curve).
    pub fn dhkey(&mut self, peer: &[u8; 64]) -> Result<[u8; 32], EcdhError> {
        if peer == &self.public {
            return Err(EcdhError::Reflected);
        }
        let pk = from_wire(peer).ok_or(EcdhError::InvalidPoint)?;
        let secret = self.secret.take().ok_or(EcdhError::Used)?;
        let shared = secret.diffie_hellman(&pk);
        let mut be: [u8; 32] = (*shared.raw_secret_bytes()).into();
        let out = rev(&be);
        be.zeroize();
        Ok(out)
    }
}

/// For tests: the public key and DHKey from a fixed private scalar.
#[doc(hidden)]
pub fn test_dh(private_be: &[u8; 32], peer: &[u8; 64]) -> Option<([u8; 64], [u8; 32])> {
    use p256::elliptic_curve::ScalarPrimitive;
    use p256::{NistP256, SecretKey};
    let sk = SecretKey::new(ScalarPrimitive::<NistP256>::from_slice(private_be).ok()?);
    let public = to_wire(&sk.public_key());
    let pk = from_wire(peer)?;
    let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
    let be: [u8; 32] = (*shared.raw_secret_bytes()).into();
    Some((public, rev(&be)))
}

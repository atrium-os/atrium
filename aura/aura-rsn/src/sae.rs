//! SAE, the WPA3-Personal authentication (IEEE 802.11-2020 §12.4), for
//! group 19 (NIST P-256) only: the password element by hunting-and-pecking
//! (§12.4.4.2.2) or hash-to-element (§12.4.4.2.3, H2E), commit and confirm
//! messages, and the KCK, PMK and PMKID they yield.
//!
//! Dragonblood (wifi.md §8.5): the hunting-and-pecking loop always runs its
//! 40 rounds and picks the element with constant-time selects; H2E needs no
//! loop. Only group 19 is offered or accepted (no group downgrade). Peer
//! scalars and elements are checked before use, and a reflected commit is
//! refused.

use crate::crypto::kdf_sha256;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::group::Group;
use p256::elliptic_curve::hash2curve::{FromOkm, MapToCurve};
use p256::elliptic_curve::ops::ReduceNonZero;
use p256::elliptic_curve::point::{AffineCoordinates, DecompressPoint};
use p256::elliptic_curve::sec1::{EncodedPoint, FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::subtle::{Choice, ConditionallySelectable, ConstantTimeEq};
use p256::elliptic_curve::{Field, PrimeField};
use p256::{AffinePoint, FieldBytes, NistP256, ProjectivePoint, Scalar};
use sha2::Sha256;
use zeroize::Zeroize;

pub const GROUP: u16 = 19;
const LEN: usize = 32;
/// Hunting-and-pecking rounds, always all run (§12.4.4.2.2: k = 40).
const HNP_ROUNDS: u8 = 40;

/// Authentication algorithm number and status codes used with SAE.
pub const AUTH_ALG_SAE: u16 = 3;
pub const STATUS_SUCCESS: u16 = 0;
pub const STATUS_ANTI_CLOGGING: u16 = 76;
pub const STATUS_UNSUPPORTED_GROUP: u16 = 77;
pub const STATUS_H2E: u16 = 126;
/// Element extension id of the Anti-Clogging Token Container (H2E).
const EXT_ACT: u8 = 93;

type HmacSha256 = Hmac<Sha256>;

/// The P-256 prime, big-endian (the context of the hunting-and-pecking KDF).
const P: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// HKDF-Expand (RFC 5869) with SHA-256.
fn hkdf_expand(prk: &[u8; 32], info: &[u8], out: &mut [u8]) {
    let mut t: Vec<u8> = Vec::new();
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let mut h = hmac(prk, &[&t, info, &[(i + 1) as u8]]);
        chunk.copy_from_slice(&h[..chunk.len()]);
        t.zeroize();
        t = h.to_vec();
        h.zeroize();
    }
    t.zeroize();
}

/// MAX(a, b) || MIN(a, b).
fn max_min(a: &[u8; 6], b: &[u8; 6]) -> [u8; 12] {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    let mut o = [0u8; 12];
    o[..6].copy_from_slice(hi);
    o[6..].copy_from_slice(lo);
    o
}

/// The password element by hunting-and-pecking (§12.4.4.2.2).
pub fn pwe_hnp(password: &[u8], a: &[u8; 6], b: &[u8; 6]) -> Option<ProjectivePoint> {
    let key = max_min(a, b);
    let prime = P;
    let mut found = Choice::from(0);
    let mut x = FieldBytes::default();
    let mut odd = Choice::from(0);
    let mut counter: u8 = 1;
    loop {
        let mut seed = hmac(&key, &[password, &[counter]]);
        let mut value = [0u8; LEN];
        kdf_sha256(&seed, b"SAE Hunting and Pecking", &prime, &mut value);
        // A candidate counts if it is below p and x^3 + ax + b is a square:
        // decompress answers both, without branching on the secret.
        let cand_odd = Choice::from(seed[31] & 1);
        let ok = AffinePoint::decompress(&FieldBytes::from(value), cand_odd).is_some();
        let take = ok & !found;
        for (d, s) in x.iter_mut().zip(value.iter()) {
            d.conditional_assign(s, take);
        }
        odd.conditional_assign(&cand_odd, take);
        found |= ok;
        seed.zeroize();
        value.zeroize();
        // All 40 rounds always; more only in the (2^-40) case of none found.
        if counter >= HNP_ROUNDS && bool::from(found) {
            break;
        }
        if counter == 200 {
            return None;
        }
        counter += 1;
    }
    let p = AffinePoint::decompress(&x, odd);
    x.zeroize();
    let p: Option<AffinePoint> = p.into();
    p.map(ProjectivePoint::from)
}

/// The password-derived point PT for H2E (§12.4.4.2.3); depends only on the
/// SSID and password, so it can be computed once per network.
pub fn pt_h2e(password: &[u8], ssid: &[u8]) -> ProjectivePoint {
    // pwd-seed = HKDF-Extract(ssid, password)
    let mut seed = hmac(ssid, &[password]);
    let mut point = ProjectivePoint::IDENTITY;
    for info in [&b"SAE Hash to Element u1 P1"[..], &b"SAE Hash to Element u2 P2"[..]] {
        // len = olen(p) + ceil(olen(p) / 2) = 48 for P-256
        let mut v = [0u8; 48];
        hkdf_expand(&seed, info, &mut v);
        let u = p256::FieldElement::from_okm(&v.into());
        point += u.map_to_curve();
        v.zeroize();
    }
    seed.zeroize();
    point
}

/// The password element from PT for one pair of addresses.
pub fn pwe_from_pt(pt: &ProjectivePoint, a: &[u8; 6], b: &[u8; 6]) -> ProjectivePoint {
    // val = H(0^n, MAX(a, b) || MIN(a, b)); val = val mod (r - 1) + 1
    let val = hmac(&[0u8; 32], &[&max_min(a, b)]);
    let s = <Scalar as ReduceNonZero<p256::U256>>::reduce_nonzero_bytes(&FieldBytes::from(val));
    pt * &s
}

fn encode_element(p: &AffinePoint) -> [u8; 64] {
    let e = p.to_encoded_point(false);
    let mut o = [0u8; 64];
    o.copy_from_slice(&e.as_bytes()[1..]);
    o
}

fn decode_element(b: &[u8]) -> Option<AffinePoint> {
    if b.len() != 64 {
        return None;
    }
    let e = EncodedPoint::<NistP256>::from_untagged_bytes(b.into());
    Option::from(AffinePoint::from_encoded_point(&e))
}

fn decode_scalar(b: &[u8]) -> Option<Scalar> {
    let s: Option<Scalar> = Option::from(Scalar::from_repr(FieldBytes::clone_from_slice(b.get(..LEN)?)));
    // 1 < scalar < r
    s.filter(|s| !bool::from(s.is_zero()) && *s != Scalar::ONE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaeError {
    /// Not group 19.
    Group,
    Malformed,
    BadScalar,
    BadElement,
    /// The peer sent our own commit back.
    Reflection,
    BadConfirm,
    UnexpectedMessage,
}

enum Phase {
    /// Our commit sent, waiting for the peer's.
    Committed,
    /// Both commits known, our confirm sent; waiting for the peer's.
    Confirmed { kck: [u8; 32], pmk: [u8; 32], pmkid: [u8; 16], peer_scalar: Scalar, peer_element: AffinePoint },
    Accepted { pmk: [u8; 32], pmkid: [u8; 16] },
}

/// One SAE exchange, as the side that commits first (the station). The
/// same code serves the AP side in tests.
pub struct Sae {
    pwe: ProjectivePoint,
    rand: Scalar,
    scalar: Scalar,
    element: AffinePoint,
    h2e: bool,
    send_confirm: u16,
    phase: Phase,
}

impl Drop for Sae {
    fn drop(&mut self) {
        self.rand = Scalar::ZERO;
        match &mut self.phase {
            Phase::Confirmed { kck, pmk, .. } => {
                kck.zeroize();
                pmk.zeroize();
            }
            Phase::Accepted { pmk, .. } => pmk.zeroize(),
            Phase::Committed => {}
        }
    }
}

impl Sae {
    /// Starts an exchange with password element `pwe`; `random` fills a
    /// buffer with fresh random bytes.
    pub fn new(pwe: ProjectivePoint, h2e: bool, random: &mut dyn FnMut(&mut [u8])) -> Sae {
        let draw = |random: &mut dyn FnMut(&mut [u8])| loop {
            let mut b = [0u8; LEN];
            random(&mut b);
            let s: Option<Scalar> = Option::from(Scalar::from_repr(b.into()));
            b.zeroize();
            if let Some(s) = s.filter(|s| !bool::from(s.is_zero()) && *s != Scalar::ONE) {
                return s;
            }
        };
        loop {
            let rand = draw(random);
            let mut mask = draw(random);
            let scalar = rand + mask;
            let element = (-(pwe * &mask)).to_affine();
            mask = Scalar::ZERO;
            let _ = mask;
            if bool::from(scalar.is_zero()) || scalar == Scalar::ONE {
                continue;
            }
            return Sae { pwe, rand, scalar, element, h2e, send_confirm: 0, phase: Phase::Committed };
        }
    }

    /// A fixed rand and mask, for the standard's test vectors.
    #[cfg(test)]
    pub fn with_rand_mask(pwe: ProjectivePoint, h2e: bool, rand: [u8; 32], mask: [u8; 32]) -> Sae {
        let rand: Scalar = Option::from(Scalar::from_repr(rand.into())).unwrap();
        let mask: Scalar = Option::from(Scalar::from_repr(mask.into())).unwrap();
        let element = (-(pwe * &mask)).to_affine();
        Sae { pwe, rand, scalar: rand + mask, element, h2e, send_confirm: 0, phase: Phase::Committed }
    }

    #[cfg(test)]
    pub fn kck_for_test(&self) -> Option<[u8; 32]> {
        match &self.phase {
            Phase::Confirmed { kck, .. } => Some(*kck),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn keys_for_test(&self) -> Option<([u8; 32], [u8; 16])> {
        match &self.phase {
            Phase::Confirmed { pmk, pmkid, .. } => Some((*pmk, *pmkid)),
            _ => None,
        }
    }

    /// The status code our commit carries: H2E commits are told apart by it.
    pub fn commit_status(&self) -> u16 {
        if self.h2e {
            STATUS_H2E
        } else {
            STATUS_SUCCESS
        }
    }

    /// The commit's authentication frame body after algorithm, transaction
    /// and status: group, the AP's anti-clogging token if it asked for one,
    /// scalar, element.
    pub fn commit_body(&self, token: Option<&[u8]>) -> Vec<u8> {
        let mut b = GROUP.to_le_bytes().to_vec();
        if let (Some(t), false) = (token, self.h2e) {
            b.extend_from_slice(t);
        }
        b.extend_from_slice(&self.scalar.to_bytes());
        b.extend_from_slice(&encode_element(&self.element));
        if let (Some(t), true) = (token, self.h2e) {
            b.extend_from_slice(&[255, (t.len() + 1) as u8, EXT_ACT]);
            b.extend_from_slice(t);
        }
        b
    }

    /// The token from an anti-clogging request (status 76) body.
    pub fn token_request(&self, body: &[u8]) -> Result<Vec<u8>, SaeError> {
        if body.len() < 2 || u16::from_le_bytes([body[0], body[1]]) != GROUP {
            return Err(SaeError::Group);
        }
        let rest = &body[2..];
        let t = if self.h2e {
            // Anti-Clogging Token Container element.
            if rest.len() < 3 || rest[0] != 255 || rest[2] != EXT_ACT || rest[1] as usize + 2 != rest.len() {
                return Err(SaeError::Malformed);
            }
            &rest[3..]
        } else {
            rest
        };
        if t.is_empty() || t.len() > 128 {
            return Err(SaeError::Malformed);
        }
        Ok(t.to_vec())
    }

    /// The peer's commit (body after algorithm, transaction and status).
    /// Derives the keys; then [`Sae::confirm_body`] gives our confirm.
    pub fn rx_commit(&mut self, body: &[u8]) -> Result<(), SaeError> {
        if !matches!(self.phase, Phase::Committed) {
            return Err(SaeError::UnexpectedMessage);
        }
        if body.len() < 2 + LEN + 64 {
            return Err(SaeError::Malformed);
        }
        if u16::from_le_bytes([body[0], body[1]]) != GROUP {
            return Err(SaeError::Group);
        }
        let peer_scalar = decode_scalar(&body[2..2 + LEN]).ok_or(SaeError::BadScalar)?;
        let peer_element = decode_element(&body[2 + LEN..2 + LEN + 64]).ok_or(SaeError::BadElement)?;
        // Elements after the element (password identifier, rejected groups)
        // are not used: no identifiers, one group.
        if peer_scalar == self.scalar && peer_element == self.element {
            return Err(SaeError::Reflection);
        }
        // K = rand * (peer-scalar * PWE + peer-element)
        let k = (self.pwe * &peer_scalar + ProjectivePoint::from(peer_element)) * &self.rand;
        if bool::from(k.is_identity()) {
            return Err(SaeError::BadElement);
        }
        let mut kx: [u8; 32] = k.to_affine().x().into();
        // keyseed = H(<0>32, k); no rejected groups, so the salt is zeros.
        let mut keyseed = hmac(&[0u8; 32], &[&kx]);
        kx.zeroize();
        let context = (self.scalar + peer_scalar).to_bytes();
        let mut keys = [0u8; 64];
        kdf_sha256(&keyseed, b"SAE KCK and PMK", &context, &mut keys);
        keyseed.zeroize();
        let mut kck = [0u8; 32];
        let mut pmk = [0u8; 32];
        kck.copy_from_slice(&keys[..32]);
        pmk.copy_from_slice(&keys[32..]);
        keys.zeroize();
        let mut pmkid = [0u8; 16];
        pmkid.copy_from_slice(&context[..16]);
        self.phase = Phase::Confirmed { kck, pmk, pmkid, peer_scalar, peer_element };
        Ok(())
    }

    fn confirm(kck: &[u8; 32], send_confirm: u16, s1: &Scalar, e1: &AffinePoint, s2: &Scalar, e2: &AffinePoint) -> [u8; 32] {
        hmac(kck, &[&send_confirm.to_le_bytes(), &s1.to_bytes(), &encode_element(e1), &s2.to_bytes(), &encode_element(e2)])
    }

    /// Our confirm body: send-confirm, confirm. Each call counts up
    /// send-confirm (retransmissions).
    pub fn confirm_body(&mut self) -> Result<Vec<u8>, SaeError> {
        let Phase::Confirmed { kck, peer_scalar, peer_element, .. } = &self.phase else {
            return Err(SaeError::UnexpectedMessage);
        };
        self.send_confirm = self.send_confirm.saturating_add(1).min(0xfffe);
        let c = Sae::confirm(kck, self.send_confirm, &self.scalar, &self.element, peer_scalar, peer_element);
        let mut b = self.send_confirm.to_le_bytes().to_vec();
        b.extend_from_slice(&c);
        Ok(b)
    }

    /// The peer's confirm. On success the PMK is ready.
    pub fn rx_confirm(&mut self, body: &[u8]) -> Result<(), SaeError> {
        let Phase::Confirmed { kck, pmk, pmkid, peer_scalar, peer_element } = &self.phase else {
            return Err(SaeError::UnexpectedMessage);
        };
        if body.len() != 2 + 32 {
            return Err(SaeError::Malformed);
        }
        let sc = u16::from_le_bytes([body[0], body[1]]);
        let want = Sae::confirm(kck, sc, peer_scalar, peer_element, &self.scalar, &self.element);
        if !bool::from(want.ct_eq(&body[2..])) {
            return Err(SaeError::BadConfirm);
        }
        self.phase = Phase::Accepted { pmk: *pmk, pmkid: *pmkid };
        Ok(())
    }

    /// The PMK and PMKID once the peer's confirm checked out.
    pub fn pmk(&self) -> Option<([u8; 32], [u8; 16])> {
        match &self.phase {
            Phase::Accepted { pmk, pmkid } => Some((*pmk, *pmkid)),
            _ => None,
        }
    }
}

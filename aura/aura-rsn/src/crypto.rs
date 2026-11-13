//! Key derivation, EAPOL-Key MICs and AES key wrap (IEEE 802.11-2020
//! §12.7.1).

use aes::Aes128;
use cmac::{Cmac, Mac as _};
use hmac::Hmac;
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha1 = Hmac<Sha1>;
type HmacSha256 = Hmac<Sha256>;

/// A 256-bit PMK. Zeroed on drop, never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Pmk(pub [u8; 32]);

impl core::fmt::Debug for Pmk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Pmk(..)")
    }
}

/// PSK from a WPA passphrase (8-63 ASCII characters) and the SSID:
/// PBKDF2-HMAC-SHA1, 4096 iterations (§J.4). `None` if the passphrase is not
/// a valid one. A 64-hex-digit PSK is taken as the PMK itself.
pub fn psk(passphrase: &[u8], ssid: &[u8]) -> Option<Pmk> {
    if passphrase.len() == 64 && passphrase.iter().all(|b| b.is_ascii_hexdigit()) {
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            *o = h(passphrase[2 * i]) << 4 | h(passphrase[2 * i + 1]);
        }
        return Some(Pmk(out));
    }
    if !(8..=63).contains(&passphrase.len()) || passphrase.iter().any(|&b| !(32..=126).contains(&b)) {
        return None;
    }
    if ssid.is_empty() || ssid.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2::<HmacSha1>(passphrase, ssid, 4096, &mut out).ok()?;
    Some(Pmk(out))
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    use hmac::Mac;
    let mut m = <HmacSha1 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use hmac::Mac;
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// PRF-n with HMAC-SHA1 (§12.7.1.2): `out.len()` bytes.
pub fn prf_sha1(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let mut h = hmac_sha1(key, &[label, &[0], data, &[i as u8]]);
        chunk.copy_from_slice(&h[..chunk.len()]);
        h.zeroize();
    }
}

/// KDF-Hash-Length with HMAC-SHA256 (§12.7.1.7.2): `out.len()` bytes.
pub fn kdf_sha256(key: &[u8], label: &[u8], context: &[u8], out: &mut [u8]) {
    let bits = (out.len() * 8) as u16;
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let n = (i + 1) as u16;
        let mut h = hmac_sha256(key, &[&n.to_le_bytes(), label, context, &bits.to_le_bytes()]);
        chunk.copy_from_slice(&h[..chunk.len()]);
        h.zeroize();
    }
}

/// The AKMs Aura supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Akm {
    /// 00-0f-ac:2, PSK with the SHA-1 PRF.
    Psk,
    /// 00-0f-ac:6, PSK with the SHA-256 KDF.
    PskSha256,
    /// 00-0f-ac:8, SAE (WPA3-Personal), the SHA-256 KDF.
    Sae,
}

impl Akm {
    pub fn suite(self) -> u32 {
        match self {
            Akm::Psk => 0x000f_ac02,
            Akm::PskSha256 => 0x000f_ac06,
            Akm::Sae => 0x000f_ac08,
        }
    }
    pub fn from_suite(s: u32) -> Option<Akm> {
        match s {
            0x000f_ac02 => Some(Akm::Psk),
            0x000f_ac06 => Some(Akm::PskSha256),
            0x000f_ac08 => Some(Akm::Sae),
            _ => None,
        }
    }
    /// EAPOL-Key descriptor version for CCMP (§12.7.2); 0 means "defined
    /// by the AKM", which for SAE is AES-128-CMAC and AES key wrap.
    pub fn key_version(self) -> u16 {
        match self {
            Akm::Psk => 2,
            Akm::PskSha256 => 3,
            Akm::Sae => 0,
        }
    }
}

/// A PTK for CCMP-128: KCK, KEK and TK, 16 bytes each.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

impl core::fmt::Debug for Ptk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Ptk(..)")
    }
}

fn min_max<'a>(a: &'a [u8], b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// PTK derivation (§12.7.1.3).
pub fn ptk(akm: Akm, pmk: &Pmk, aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> Ptk {
    let (a1, a2) = min_max(aa, spa);
    let (n1, n2) = min_max(anonce, snonce);
    let mut data = [0u8; 76];
    data[..6].copy_from_slice(a1);
    data[6..12].copy_from_slice(a2);
    data[12..44].copy_from_slice(n1);
    data[44..76].copy_from_slice(n2);
    let mut out = [0u8; 48];
    match akm {
        Akm::Psk => prf_sha1(&pmk.0, b"Pairwise key expansion", &data, &mut out),
        Akm::PskSha256 | Akm::Sae => kdf_sha256(&pmk.0, b"Pairwise key expansion", &data, &mut out),
    }
    let mut p = Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 16] };
    p.kck.copy_from_slice(&out[..16]);
    p.kek.copy_from_slice(&out[16..32]);
    p.tk.copy_from_slice(&out[32..48]);
    out.zeroize();
    p
}

/// The EAPOL-Key MIC over a whole EAPOL frame whose MIC field is zero.
/// `version` is the key descriptor version; 0 is only ever SAE's here.
pub fn mic(version: u16, kck: &[u8; 16], frame: &[u8]) -> Option<[u8; 16]> {
    match version {
        2 => {
            let h = hmac_sha1(kck, &[frame]);
            let mut m = [0u8; 16];
            m.copy_from_slice(&h[..16]);
            Some(m)
        }
        0 | 3 => {
            let mut c = <Cmac<Aes128> as cmac::Mac>::new_from_slice(kck).ok()?;
            c.update(frame);
            Some(c.finalize().into_bytes().into())
        }
        _ => None,
    }
}

pub fn mic_ok(version: u16, kck: &[u8; 16], frame: &[u8], got: &[u8]) -> bool {
    match mic(version, kck, frame) {
        Some(m) => bool::from(m.ct_eq(got)),
        None => false,
    }
}

/// AES key unwrap (RFC 3394) with the KEK; `None` on an integrity failure.
pub fn unwrap(kek: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 24 || data.len() % 8 != 0 {
        return None;
    }
    let kw = aes_kw::KekAes128::from(*kek);
    let mut out = vec![0u8; data.len() - 8];
    kw.unwrap(data, &mut out).ok()?;
    Some(out)
}

/// AES key wrap (RFC 3394); `data` must be a multiple of 8, at least 16.
pub fn wrap(kek: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 16 || data.len() % 8 != 0 {
        return None;
    }
    let kw = aes_kw::KekAes128::from(*kek);
    let mut out = vec![0u8; data.len() + 8];
    kw.wrap(data, &mut out).ok()?;
    Some(out)
}

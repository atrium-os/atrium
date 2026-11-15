//! Password-protected OOXML ([MS-OFFCRYPTO] §2.3.4): a compound file
//! holding `EncryptionInfo` and `EncryptedPackage` (the ZIP package,
//! encrypted). Agile encryption (what Office writes since 2010) and Standard
//! encryption (Office 2007, and LibreOffice's writer) are read; Agile is
//! written: AES-256-CBC, SHA-512, 100,000 spins, with its HMAC over the
//! encrypted package checked on reading.
//!
//! Runs in the converter process; the password comes to it with the
//! document and is never kept.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::cfb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// The password doesn't open it.
    WrongPassword,
    /// An encryption this reader doesn't do (named).
    Unsupported(String),
    Damaged(String),
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptError::WrongPassword => write!(f, "the password is wrong"),
            CryptError::Unsupported(w) => write!(f, "encrypted with {w}, which isn't supported"),
            CryptError::Damaged(w) => write!(f, "damaged encryption: {w}"),
        }
    }
}

pub(crate) fn damaged<T>(w: &str) -> Result<T, CryptError> {
    Err(CryptError::Damaged(w.into()))
}

/// The longest decrypted package accepted.
pub const MAX_PACKAGE: u64 = 4 << 30;
/// The most spins a file may ask for (Office uses 100,000).
const MAX_SPIN: u32 = 10_000_000;

/// Whether `b` is a password-protected OOXML package.
pub fn is_encrypted_ooxml(b: &[u8]) -> bool {
    cfb::Cfb::open(b).is_ok_and(|c| c.has("EncryptionInfo") && c.has("EncryptedPackage"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn of(name: &str) -> Option<Hash> {
        Some(match name {
            "SHA1" | "SHA-1" => Hash::Sha1,
            "SHA256" => Hash::Sha256,
            "SHA384" => Hash::Sha384,
            "SHA512" => Hash::Sha512,
            _ => return None,
        })
    }
    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        fn run<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
            let mut d = D::new();
            for p in parts {
                d.update(p);
            }
            d.finalize().to_vec()
        }
        match self {
            Hash::Sha1 => run::<Sha1>(parts),
            Hash::Sha256 => run::<Sha256>(parts),
            Hash::Sha384 => run::<Sha384>(parts),
            Hash::Sha512 => run::<Sha512>(parts),
        }
    }
    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        use hmac::{Hmac, Mac};
        macro_rules! run {
            ($d:ty) => {{
                let mut m = <Hmac<$d> as Mac>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }};
        }
        match self {
            Hash::Sha1 => run!(Sha1),
            Hash::Sha256 => run!(Sha256),
            Hash::Sha384 => run!(Sha384),
            Hash::Sha512 => run!(Sha512),
        }
    }
}

fn utf16(p: &str) -> Vec<u8> {
    p.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// H0 = H(salt + password), Hn = H(n + Hn-1) `spin` times.
fn spun(h: Hash, salt: &[u8], password: &str, spin: u32) -> Vec<u8> {
    let mut x = h.digest(&[salt, &utf16(password)]);
    for i in 0..spin {
        x = h.digest(&[&i.to_le_bytes(), &x]);
    }
    x
}

/// `v` cut or padded (with `pad`) to `n` bytes.
fn fit(mut v: Vec<u8>, n: usize, pad: u8) -> Vec<u8> {
    v.resize(n, pad);
    v
}

pub(crate) enum Aes {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Aes {
    pub(crate) fn new(key: &[u8]) -> Result<Aes, CryptError> {
        Ok(match key.len() {
            16 => Aes::A128(aes::Aes128::new_from_slice(key).unwrap()),
            24 => Aes::A192(aes::Aes192::new_from_slice(key).unwrap()),
            32 => Aes::A256(aes::Aes256::new_from_slice(key).unwrap()),
            n => return Err(CryptError::Unsupported(format!("a {}-bit AES key", n * 8))),
        })
    }
    fn dec(&self, b: &mut [u8; 16]) {
        let b = aes::Block::from_mut_slice(b);
        match self {
            Aes::A128(c) => c.decrypt_block(b),
            Aes::A192(c) => c.decrypt_block(b),
            Aes::A256(c) => c.decrypt_block(b),
        }
    }
    fn enc(&self, b: &mut [u8; 16]) {
        let b = aes::Block::from_mut_slice(b);
        match self {
            Aes::A128(c) => c.encrypt_block(b),
            Aes::A192(c) => c.encrypt_block(b),
            Aes::A256(c) => c.encrypt_block(b),
        }
    }
    pub(crate) fn cbc_dec(&self, iv: &[u8], data: &mut [u8]) {
        let mut prev: [u8; 16] = iv[..16].try_into().unwrap();
        for c in data.chunks_exact_mut(16) {
            let ct: [u8; 16] = (&*c).try_into().unwrap();
            let mut b = ct;
            self.dec(&mut b);
            for k in 0..16 {
                c[k] = b[k] ^ prev[k];
            }
            prev = ct;
        }
    }
    pub(crate) fn cbc_enc(&self, iv: &[u8], data: &mut [u8]) {
        let mut prev: [u8; 16] = iv[..16].try_into().unwrap();
        for c in data.chunks_exact_mut(16) {
            let mut b: [u8; 16] = (&*c).try_into().unwrap();
            for k in 0..16 {
                b[k] ^= prev[k];
            }
            self.enc(&mut b);
            c.copy_from_slice(&b);
            prev = b;
        }
    }
    fn ecb_dec(&self, data: &mut [u8]) {
        for c in data.chunks_exact_mut(16) {
            let mut b: [u8; 16] = (&*c).try_into().unwrap();
            self.dec(&mut b);
            c.copy_from_slice(&b);
        }
    }
}

pub(crate) fn b64(s: &str) -> Result<Vec<u8>, CryptError> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return damaged("base64"),
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

pub(crate) fn b64e(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(T[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

const BK_VERIFIER_INPUT: [u8; 8] = [0xfe, 0xa7, 0xd2, 0x76, 0x3b, 0x4b, 0x9e, 0x79];
const BK_VERIFIER_VALUE: [u8; 8] = [0xd7, 0xaa, 0x0f, 0x6d, 0x30, 0x61, 0x34, 0x4e];
const BK_KEY: [u8; 8] = [0x14, 0x6e, 0x0b, 0xe7, 0xab, 0xac, 0xd0, 0xd6];
const BK_HMAC_KEY: [u8; 8] = [0x5f, 0xb2, 0xad, 0x01, 0x0c, 0xb9, 0xe1, 0xf6];
const BK_HMAC_VALUE: [u8; 8] = [0xa0, 0x67, 0x7f, 0x02, 0xb2, 0x2c, 0x84, 0x33];
const SEGMENT: usize = 4096;

/// The attributes of an Agile `EncryptionInfo`'s elements.
#[derive(Default)]
struct Params {
    salt: Vec<u8>,
    block: usize,
    key_bits: usize,
    hash_size: usize,
    hash: Option<Hash>,
    spin: u32,
    verifier_input: Vec<u8>,
    verifier_value: Vec<u8>,
    key_value: Vec<u8>,
    cipher_ok: bool,
}

fn params(e: &quick_xml::events::BytesStart, p: &mut Params) -> Result<(), CryptError> {
    let (mut alg, mut chain) = (String::new(), String::new());
    for a in e.attributes().flatten() {
        let v = String::from_utf8_lossy(&a.value).into_owned();
        match a.key.local_name().as_ref() {
            b"saltValue" => p.salt = b64(&v)?,
            b"blockSize" => p.block = v.parse().unwrap_or(0),
            b"keyBits" => p.key_bits = v.parse().unwrap_or(0),
            b"hashSize" => p.hash_size = v.parse().unwrap_or(0),
            b"hashAlgorithm" => p.hash = Hash::of(&v),
            b"spinCount" => p.spin = v.parse().unwrap_or(u32::MAX),
            b"cipherAlgorithm" => alg = v,
            b"cipherChaining" => chain = v,
            b"encryptedVerifierHashInput" => p.verifier_input = b64(&v)?,
            b"encryptedVerifierHashValue" => p.verifier_value = b64(&v)?,
            b"encryptedKeyValue" => p.key_value = b64(&v)?,
            _ => {}
        }
    }
    p.cipher_ok = alg == "AES" && chain == "ChainingModeCBC";
    if !p.cipher_ok {
        return Err(CryptError::Unsupported(format!("{alg} {chain}")));
    }
    Ok(())
}

/// Decrypts a password-protected OOXML file to its ZIP package.
pub fn decrypt(file: &[u8], password: &str) -> Result<Vec<u8>, CryptError> {
    let c = cfb::Cfb::open(file).map_err(|e| CryptError::Damaged(e.to_string()))?;
    let info = c.stream("EncryptionInfo").map_err(|e| CryptError::Damaged(e.to_string()))?;
    let pkg = c.stream("EncryptedPackage").map_err(|e| CryptError::Damaged(e.to_string()))?;
    if info.len() < 8 || pkg.len() < 8 {
        return damaged("streams too short");
    }
    let (major, minor) = (u16::from_le_bytes([info[0], info[1]]), u16::from_le_bytes([info[2], info[3]]));
    match (major, minor) {
        (4, 4) => agile(&info[8..], &pkg, password),
        (2..=4, 2) => standard(&info[4..], &pkg, password),
        (1, 1) => Err(CryptError::Unsupported("RC4 (Office 97–2003)".into())),
        _ => Err(CryptError::Unsupported(format!("encryption version {major}.{minor}"))),
    }
}

fn agile(xml: &[u8], pkg: &[u8], password: &str) -> Result<Vec<u8>, CryptError> {
    let (mut data, mut pw) = (Params::default(), Params::default());
    let (mut hmac_key, mut hmac_value) = (Vec::new(), Vec::new());
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut in_password = false;
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => match e.local_name().as_ref() {
                b"keyData" => params(&e, &mut data)?,
                b"dataIntegrity" => {
                    for a in e.attributes().flatten() {
                        let v = String::from_utf8_lossy(&a.value).into_owned();
                        match a.key.local_name().as_ref() {
                            b"encryptedHmacKey" => hmac_key = b64(&v)?,
                            b"encryptedHmacValue" => hmac_value = b64(&v)?,
                            _ => {}
                        }
                    }
                }
                b"keyEncryptor" => {
                    in_password = e.attributes().flatten().any(|a| a.key.local_name().as_ref() == b"uri" && a.value.ends_with(b"keyEncryptor/password"));
                }
                b"encryptedKey" if in_password => params(&e, &mut pw)?,
                _ => {}
            },
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => return damaged("EncryptionInfo XML"),
            _ => {}
        }
        buf.clear();
    }
    let (Some(ph), Some(dh)) = (pw.hash, data.hash) else {
        return if pw.key_value.is_empty() { Err(CryptError::Unsupported("a certificate, not a password".into())) } else { damaged("hash algorithm") };
    };
    if pw.spin > MAX_SPIN || pw.block != 16 || data.block != 16 || pw.salt.len() < 16 || data.salt.len() < 16 || !matches!(pw.key_bits, 128 | 192 | 256) || !matches!(data.key_bits, 128 | 192 | 256) {
        return damaged("key parameters");
    }
    // The password's keys.
    let h = spun(ph, &pw.salt, password, pw.spin);
    let key = |bk: &[u8]| fit(ph.digest(&[&h, bk]), pw.key_bits / 8, 0x36);
    let iv = fit(pw.salt.clone(), 16, 0x36);
    let unwrap = |bk: &[u8], v: &[u8]| -> Result<Vec<u8>, CryptError> {
        if v.is_empty() || v.len() % 16 != 0 {
            return damaged("encrypted value length");
        }
        let mut b = v.to_vec();
        Aes::new(&key(bk))?.cbc_dec(&iv, &mut b);
        Ok(b)
    };
    let input = unwrap(&BK_VERIFIER_INPUT, &pw.verifier_input)?;
    let want = unwrap(&BK_VERIFIER_VALUE, &pw.verifier_value)?;
    let got = ph.digest(&[&input[..pw.salt.len().min(input.len())]]);
    if want.len() < got.len() || want[..got.len()] != got[..] {
        return Err(CryptError::WrongPassword);
    }
    let secret = unwrap(&BK_KEY, &pw.key_value)?;
    let secret = &secret[..(data.key_bits / 8).min(secret.len())];
    let aes = Aes::new(secret)?;
    // The package's HMAC, when present.
    if !hmac_key.is_empty() && !hmac_value.is_empty() && hmac_key.len() % 16 == 0 && hmac_value.len() % 16 == 0 {
        let iv_k = fit(dh.digest(&[&data.salt, &BK_HMAC_KEY]), 16, 0x36);
        let iv_v = fit(dh.digest(&[&data.salt, &BK_HMAC_VALUE]), 16, 0x36);
        let (mut k, mut v) = (hmac_key.clone(), hmac_value.clone());
        aes.cbc_dec(&iv_k, &mut k);
        aes.cbc_dec(&iv_v, &mut v);
        let n = data.hash_size.min(k.len()).min(v.len());
        let mac = dh.hmac(&k[..n], pkg);
        if mac.len() < n || mac[..n] != v[..n] {
            return damaged("the package fails its HMAC (changed or damaged)");
        }
    }
    let size = u64::from_le_bytes(pkg[..8].try_into().unwrap());
    let body = &pkg[8..];
    if size > MAX_PACKAGE || size > body.len() as u64 {
        return damaged("package size");
    }
    let mut out = Vec::with_capacity(body.len());
    for (i, seg) in body.chunks(SEGMENT).enumerate() {
        let iv = fit(dh.digest(&[&data.salt, &(i as u32).to_le_bytes()]), 16, 0x36);
        let mut b = seg.to_vec();
        b.truncate(b.len() / 16 * 16);
        aes.cbc_dec(&iv, &mut b);
        out.extend_from_slice(&b);
        if out.len() as u64 >= size {
            break;
        }
    }
    if (out.len() as u64) < size {
        return damaged("package shorter than its size");
    }
    out.truncate(size as usize);
    Ok(out)
}

fn standard(b: &[u8], pkg: &[u8], password: &str) -> Result<Vec<u8>, CryptError> {
    // Flags, header size, header (8 u32s and the CSP name), verifier.
    if b.len() < 8 {
        return damaged("EncryptionInfo");
    }
    let hsize = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let h = b.get(8..8 + hsize).ok_or(CryptError::Damaged("EncryptionInfo header".into()))?;
    if h.len() < 32 {
        return damaged("EncryptionInfo header");
    }
    let u = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    let (alg, alg_hash, key_bits) = (u(8), u(12), u(16) as usize);
    if !matches!(alg, 0x660E..=0x6610) {
        return Err(CryptError::Unsupported(format!("algorithm {alg:#x}")));
    }
    if alg_hash != 0x8004 && alg_hash != 0 {
        return Err(CryptError::Unsupported(format!("hash {alg_hash:#x}")));
    }
    let key_bits = if key_bits == 0 { 128 } else { key_bits };
    let v = &b[8 + hsize..];
    if v.len() < 4 + 16 + 16 + 4 + 32 || u32::from_le_bytes(v[0..4].try_into().unwrap()) != 16 {
        return damaged("EncryptionVerifier");
    }
    let salt = &v[4..20];
    let mut verifier: [u8; 16] = v[20..36].try_into().unwrap();
    let hash_size = u32::from_le_bytes(v[36..40].try_into().unwrap()) as usize;
    let mut vhash = v[40..72].to_vec();
    // The key: SHA-1 spun 50,000 times, block 0, then CryptDeriveKey.
    let hh = spun(Hash::Sha1, salt, password, 50_000);
    let hf = Hash::Sha1.digest(&[&hh, &0u32.to_le_bytes()]);
    let derive = |c: u8| {
        let mut x = vec![c; 64];
        for (k, v) in hf.iter().enumerate() {
            x[k] ^= v;
        }
        Hash::Sha1.digest(&[&x])
    };
    let mut x3 = derive(0x36);
    x3.extend(derive(0x5c));
    let aes = Aes::new(&x3[..key_bits / 8])?;
    aes.dec(&mut verifier);
    aes.ecb_dec(&mut vhash);
    let want = Hash::Sha1.digest(&[&verifier]);
    if hash_size > vhash.len() || hash_size < 20 || vhash[..20] != want[..] {
        return Err(CryptError::WrongPassword);
    }
    let size = u64::from_le_bytes(pkg[..8].try_into().unwrap());
    let mut body = pkg[8..].to_vec();
    if size > MAX_PACKAGE || size > body.len() as u64 {
        return damaged("package size");
    }
    body.truncate(body.len() / 16 * 16);
    aes.ecb_dec(&mut body);
    body.truncate(size as usize);
    Ok(body)
}

pub(crate) fn random(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::getrandom(&mut v).expect("the system's random numbers");
    v
}

/// The spins [`encrypt`] does (Office's own).
pub const SPIN: u32 = 100_000;

/// `package` (a ZIP) encrypted with `password`: Agile, AES-256, SHA-512.
pub fn encrypt(package: &[u8], password: &str) -> Vec<u8> {
    encrypt_with(package, password, SPIN)
}

pub fn encrypt_with(package: &[u8], password: &str, spin: u32) -> Vec<u8> {
    let h = Hash::Sha512;
    let (pw_salt, data_salt, secret) = (random(16), random(16), random(32));
    let aes_s = Aes::new(&secret).unwrap();
    // The package: size, then 4096-byte segments, each padded to 16.
    let mut pkg = (package.len() as u64).to_le_bytes().to_vec();
    for (i, seg) in package.chunks(SEGMENT).enumerate() {
        let iv = fit(h.digest(&[&data_salt, &(i as u32).to_le_bytes()]), 16, 0x36);
        let mut b = seg.to_vec();
        b.resize(seg.len().div_ceil(16) * 16, 0);
        aes_s.cbc_enc(&iv, &mut b);
        pkg.extend_from_slice(&b);
    }
    // Data integrity.
    let hmac_key = random(64);
    let mac = h.hmac(&hmac_key, &pkg);
    let mut ek = hmac_key.clone();
    aes_s.cbc_enc(&fit(h.digest(&[&data_salt, &BK_HMAC_KEY]), 16, 0x36), &mut ek);
    let mut ev = mac;
    aes_s.cbc_enc(&fit(h.digest(&[&data_salt, &BK_HMAC_VALUE]), 16, 0x36), &mut ev);
    // The password's wrapping of the verifier and the key.
    let hs = spun(h, &pw_salt, password, spin);
    let key = |bk: &[u8]| fit(h.digest(&[&hs, bk]), 32, 0x36);
    let wrap = |bk: &[u8], v: &[u8]| {
        let mut b = v.to_vec();
        b.resize(v.len().div_ceil(16) * 16, 0);
        Aes::new(&key(bk)).unwrap().cbc_enc(&pw_salt, &mut b);
        b
    };
    let input = random(16);
    let vi = wrap(&BK_VERIFIER_INPUT, &input);
    let vv = wrap(&BK_VERIFIER_VALUE, &h.digest(&[&input]));
    let kv = wrap(&BK_KEY, &secret);
    let xml = format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n",
            "<encryption xmlns=\"http://schemas.microsoft.com/office/2006/encryption\" xmlns:p=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\" xmlns:c=\"http://schemas.microsoft.com/office/2006/keyEncryptor/certificate\">",
            "<keyData saltSize=\"16\" blockSize=\"16\" keyBits=\"256\" hashSize=\"64\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA512\" saltValue=\"{ds}\"/>",
            "<dataIntegrity encryptedHmacKey=\"{ek}\" encryptedHmacValue=\"{ev}\"/>",
            "<keyEncryptors><keyEncryptor uri=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\">",
            "<p:encryptedKey spinCount=\"{spin}\" saltSize=\"16\" blockSize=\"16\" keyBits=\"256\" hashSize=\"64\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA512\" saltValue=\"{ps}\" encryptedVerifierHashInput=\"{vi}\" encryptedVerifierHashValue=\"{vv}\" encryptedKeyValue=\"{kv}\"/>",
            "</keyEncryptor></keyEncryptors></encryption>"
        ),
        ds = b64e(&data_salt),
        ek = b64e(&ek),
        ev = b64e(&ev),
        spin = spin,
        ps = b64e(&pw_salt),
        vi = b64e(&vi),
        vv = b64e(&vv),
        kv = b64e(&kv)
    );
    let mut info = vec![4, 0, 4, 0, 0x40, 0, 0, 0];
    info.extend_from_slice(xml.as_bytes());
    cfb::write(&[("EncryptionInfo", &info), ("EncryptedPackage", &pkg)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_both_ways() {
        for n in 0..40 {
            let v: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64(&b64e(&v)).unwrap(), v);
        }
    }

    #[test]
    fn agile_round_trip_and_a_wrong_password() {
        let pkg: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let f = encrypt_with(&pkg, "Pässwörd", 1000);
        assert!(is_encrypted_ooxml(&f));
        assert_eq!(decrypt(&f, "Pässwörd").unwrap(), pkg);
        assert_eq!(decrypt(&f, "password"), Err(CryptError::WrongPassword));
        // A changed byte in the package fails its HMAC.
        let c = cfb::Cfb::open(&f).unwrap();
        let p = c.stream("EncryptedPackage").unwrap();
        let mut bad = p.clone();
        bad[100] ^= 1;
        let info = c.stream("EncryptionInfo").unwrap();
        let g = cfb::write(&[("EncryptionInfo", &info), ("EncryptedPackage", &bad)]);
        assert!(matches!(decrypt(&g, "Pässwörd"), Err(CryptError::Damaged(_))));
        // Small packages (in the mini stream) too.
        let f = encrypt_with(b"PK tiny", "x", 10);
        assert_eq!(decrypt(&f, "x").unwrap(), b"PK tiny");
    }

    #[test]
    fn compound_files_with_many_fat_sectors() {
        // Over 109 FAT sectors: the DIFAT is used.
        let big: Vec<u8> = (0..8_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let f = cfb::write(&[("A", b"small"), ("Big", &big)]);
        let c = cfb::Cfb::open(&f).unwrap();
        assert_eq!(c.stream("Big").unwrap(), big);
        assert_eq!(c.stream("A").unwrap(), b"small");
    }

    #[test]
    fn hostile_files_never_panic() {
        let f = encrypt_with(&[7u8; 5000], "p", 10);
        for i in (0..f.len()).step_by(13) {
            let mut x = f.clone();
            x[i] ^= 0x5a;
            let _ = decrypt(&x, "p");
        }
        for n in (0..f.len()).step_by(97) {
            let _ = decrypt(&f[..n], "p");
        }
    }
}

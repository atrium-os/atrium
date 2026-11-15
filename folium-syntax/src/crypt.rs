//! The standard security handler (ISO 32000-2 §7.6.4): revisions 2–4
//! (RC4 and AES-128) and 5–6 (AES-256). Only opening is done here, with
//! the empty password first; permissions are reported, not enforced
//! (enforcing them is the viewer's choice, folium.md §8).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};

use crate::object::{Dict, Object, Ref};
use crate::{Error, Result};

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08, 0x2E, 0x2E, 0x00,
    0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    None,
    Rc4,
    Aes128,
    Aes256,
}

#[derive(Debug, Clone)]
pub struct Handler {
    key: Vec<u8>,
    strings: Method,
    streams: Method,
    pub revision: i64,
    /// `/P`, the permission bits.
    pub permissions: i32,
    pub encrypt_metadata: bool,
    /// Opened with the owner password.
    pub owner: bool,
}

fn s<'a>(d: &'a Dict, k: &str) -> &'a [u8] { d.get(k).and_then(Object::as_str).unwrap_or(&[]) }

impl Handler {
    /// Open with `password` (tried as user, then as owner password).
    /// `id0` is the first element of the trailer's `/ID`.
    pub fn open(enc: &Dict, id0: &[u8], password: &[u8]) -> Result<Handler> {
        if !matches!(enc.name("Filter"), Some(n) if n.is("Standard")) {
            return Err(Error::Unsupported("a security handler other than the standard one"));
        }
        let v = enc.int("V").unwrap_or(0);
        let r = enc.int("R").unwrap_or(0);
        let p = enc.int("P").unwrap_or(-1) as i32;
        let meta = enc.get("EncryptMetadata").and_then(Object::as_bool).unwrap_or(true);
        let (o, u) = (s(enc, "O"), s(enc, "U"));
        let (strings, streams, len) = match v {
            1 => (Method::Rc4, Method::Rc4, 5),
            2 | 3 => {
                let l = (enc.int("Length").unwrap_or(40) / 8).clamp(5, 16) as usize;
                (Method::Rc4, Method::Rc4, l)
            }
            4 | 5 => {
                let cf = enc.get("CF").and_then(Object::as_dict);
                let m = |key: &str| -> (Method, usize) {
                    let name = enc.name(key);
                    let Some(name) = name else { return (Method::None, 16) };
                    if name.is("Identity") {
                        return (Method::None, 16);
                    }
                    let f = cf.and_then(|c| c.get(std::str::from_utf8(name.as_bytes()).unwrap_or(""))).and_then(Object::as_dict);
                    let cfm = f.and_then(|f| f.name("CFM"));
                    let l = f.and_then(|f| f.int("Length")).unwrap_or(16);
                    // /Length is in bytes for crypt filters, though some
                    // writers put bits.
                    let l = if l > 32 { l / 8 } else { l }.clamp(5, 16) as usize;
                    match cfm {
                        Some(c) if c.is("V2") => (Method::Rc4, l),
                        Some(c) if c.is("AESV2") => (Method::Aes128, 16),
                        Some(c) if c.is("AESV3") => (Method::Aes256, 32),
                        _ => (Method::None, 16),
                    }
                };
                let (a, l) = m("StrF");
                let (b, l2) = m("StmF");
                (a, b, l.max(l2))
            }
            _ => return Err(Error::Unsupported("this encryption version")),
        };
        let (key, owner) = match r {
            2..=4 => {
                // MD5 gives 16 bytes; a crypt filter asking for more
                // (AESV3 under an old revision) is cut to that.
                let len = len.min(16);
                if o.len() < 32 || u.len() < 32 {
                    return Err(Error::Syntax { offset: 0, what: "/O or /U too short" });
                }
                if let Some(k) = check_user_r4(password, o, u, p, id0, r, len, meta) {
                    (k, false)
                } else {
                    // Owner password: it decrypts /O to the user password.
                    let mut h = md5(&padded(password));
                    if r >= 3 {
                        for _ in 0..50 {
                            h = md5(&h[..len]);
                        }
                    }
                    let okey = &h[..len];
                    let mut up = o[..32].to_vec();
                    if r == 2 {
                        rc4(okey, &mut up);
                    } else {
                        for i in (0..20u8).rev() {
                            let k: Vec<u8> = okey.iter().map(|b| b ^ i).collect();
                            rc4(&k, &mut up);
                        }
                    }
                    let k = check_user_r4(&up, o, u, p, id0, r, len, meta).ok_or(Error::NeedsPassword)?;
                    (k, true)
                }
            }
            5 | 6 => {
                if o.len() < 48 || u.len() < 48 {
                    return Err(Error::Syntax { offset: 0, what: "/O or /U too short" });
                }
                let pw = &password[..password.len().min(127)];
                let h = |salt: &[u8], udata: &[u8]| if r == 5 { sha256_cat(&[pw, salt, udata]) } else { hash_2b(pw, salt, udata) };
                let (ue, oe) = (s(enc, "UE"), s(enc, "OE"));
                if h(&u[32..40], &[])[..] == u[..32] {
                    (aes256_unwrap(&h(&u[40..48], &[]), ue)?, false)
                } else if h(&o[32..40], &u[..48])[..] == o[..32] {
                    (aes256_unwrap(&h(&o[40..48], &u[..48]), oe)?, true)
                } else {
                    return Err(Error::NeedsPassword);
                }
            }
            _ => return Err(Error::Unsupported("this security handler revision")),
        };
        Ok(Handler { key, strings, streams, revision: r, permissions: p, encrypt_metadata: meta, owner })
    }

    fn object_key(&self, r: Ref, m: Method) -> Vec<u8> {
        if m == Method::Aes256 {
            return self.key.clone();
        }
        let mut h = Md5::new();
        h.update(&self.key);
        h.update(&r.num.to_le_bytes()[..3]);
        h.update(r.gen.to_le_bytes());
        if m == Method::Aes128 {
            h.update(b"sAlT");
        }
        let d = h.finalize();
        d[..(self.key.len() + 5).min(16)].to_vec()
    }

    fn run(&self, r: Ref, m: Method, data: &[u8]) -> Vec<u8> {
        match m {
            Method::None => data.to_vec(),
            Method::Rc4 => {
                let mut v = data.to_vec();
                rc4(&self.object_key(r, m), &mut v);
                v
            }
            Method::Aes128 | Method::Aes256 => aes_cbc_decrypt(&self.object_key(r, m), data),
        }
    }

    pub fn string(&self, r: Ref, data: &[u8]) -> Vec<u8> { self.run(r, self.strings, data) }
    pub fn stream(&self, r: Ref, data: &[u8]) -> Vec<u8> { self.run(r, self.streams, data) }

    /// Decrypt every string in `o`, which is object `r`. The `/Contents`
    /// of a signature dictionary is not encrypted (§7.6.2).
    pub fn object(&self, r: Ref, o: &mut Object) {
        match o {
            Object::String(s) => *s = self.string(r, s).into(),
            Object::Array(a) => a.iter_mut().for_each(|x| self.object(r, x)),
            Object::Dict(d) => self.dict(r, d),
            Object::Stream(st) => self.dict(r, &mut st.dict),
            _ => {}
        }
    }

    fn dict(&self, r: Ref, d: &mut Dict) {
        let sig = d.get("ByteRange").is_some() && (d.type_is("Sig") || d.type_is("DocTimeStamp"));
        for (k, v) in d.0.iter_mut() {
            if sig && k.is("Contents") {
                continue;
            }
            self.object(r, v);
        }
    }
}

fn padded(pw: &[u8]) -> [u8; 32] {
    let mut p = PAD;
    let n = pw.len().min(32);
    p[..n].copy_from_slice(&pw[..n]);
    p[n..].copy_from_slice(&PAD[..32 - n]);
    p
}

fn md5(b: &[u8]) -> Vec<u8> { Md5::digest(b).to_vec() }

#[allow(clippy::too_many_arguments)]
fn check_user_r4(pw: &[u8], o: &[u8], u: &[u8], p: i32, id0: &[u8], r: i64, len: usize, meta: bool) -> Option<Vec<u8>> {
    // Algorithm 2.
    let mut h = Md5::new();
    h.update(padded(pw));
    h.update(&o[..32]);
    h.update(p.to_le_bytes());
    h.update(id0);
    if r >= 4 && !meta {
        h.update([0xff; 4]);
    }
    let mut k = h.finalize().to_vec();
    let len = if r == 2 { 5 } else { len };
    if r >= 3 {
        for _ in 0..50 {
            k = md5(&k[..len]);
        }
    }
    k.truncate(len);
    // Algorithms 4 and 5.
    let ok = if r == 2 {
        let mut x = PAD.to_vec();
        rc4(&k, &mut x);
        x[..] == u[..32]
    } else {
        let mut h = Md5::new();
        h.update(PAD);
        h.update(id0);
        let mut x = h.finalize().to_vec();
        for i in 0..20u8 {
            let ki: Vec<u8> = k.iter().map(|b| b ^ i).collect();
            rc4(&ki, &mut x);
        }
        x[..16] == u[..16]
    };
    ok.then_some(k)
}

pub fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    for b in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        *b ^= s[s[i as usize].wrapping_add(s[j as usize]) as usize];
    }
}

fn sha256_cat(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

/// Algorithm 2.B (revision 6).
fn hash_2b(pw: &[u8], salt: &[u8], udata: &[u8]) -> Vec<u8> {
    let mut k = sha256_cat(&[pw, salt, udata]);
    let mut i = 0u32;
    let mut e: Vec<u8> = Vec::new();
    while i < 64 || u32::from(*e.last().unwrap_or(&0)) > i - 32 {
        let mut k1 = Vec::with_capacity(64 * (pw.len() + k.len() + udata.len()));
        for _ in 0..64 {
            k1.extend_from_slice(pw);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        e = aes128_cbc_encrypt_raw(&k[..16], &k[16..32], &k1);
        let m = e[..16].iter().map(|&b| b as u32).sum::<u32>() % 3;
        k = match m {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        i += 1;
    }
    k.truncate(32);
    k
}

fn aes128_cbc_encrypt_raw(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    let c = aes::Aes128::new(GenericArray::from_slice(key));
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    let mut out = Vec::with_capacity(data.len());
    for blk in data.chunks_exact(16) {
        let mut b = [0u8; 16];
        for j in 0..16 {
            b[j] = blk[j] ^ prev[j];
        }
        let mut g = GenericArray::from(b);
        c.encrypt_block(&mut g);
        prev = g.into();
        out.extend_from_slice(&prev);
    }
    out
}

/// AES-256 with a zero IV and no padding, for `/UE` and `/OE`.
fn aes256_unwrap(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 32 {
        return Err(Error::Syntax { offset: 0, what: "/UE or /OE too short" });
    }
    let c = aes::Aes256::new(GenericArray::from_slice(&key[..32]));
    Ok(cbc_decrypt_blocks(|g| c.decrypt_block(g), &[0; 16], &data[..32]))
}

fn cbc_decrypt_blocks(dec: impl Fn(&mut GenericArray<u8, aes::cipher::consts::U16>), iv: &[u8], data: &[u8]) -> Vec<u8> {
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    let mut out = Vec::with_capacity(data.len());
    for blk in data.chunks_exact(16) {
        let mut g = GenericArray::clone_from_slice(blk);
        dec(&mut g);
        for j in 0..16 {
            out.push(g[j] ^ prev[j]);
        }
        prev.copy_from_slice(blk);
    }
    out
}

/// AES-CBC as streams and strings use it: a 16-byte IV first, PKCS#7
/// padding last. Damaged padding is left as is; a short input is empty.
pub fn aes_cbc_decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    if data.len() < 32 {
        return Vec::new();
    }
    let (iv, body) = data.split_at(16);
    let body = &body[..body.len() / 16 * 16];
    let mut out = if key.len() == 32 {
        let c = aes::Aes256::new(GenericArray::from_slice(key));
        cbc_decrypt_blocks(|g| c.decrypt_block(g), iv, body)
    } else {
        let c = aes::Aes128::new(GenericArray::from_slice(&key[..16]));
        cbc_decrypt_blocks(|g| c.decrypt_block(g), iv, body)
    };
    if let Some(&n) = out.last() {
        let n = n as usize;
        if (1..=16).contains(&n) && n <= out.len() && out[out.len() - n..].iter().all(|&b| b as usize == n) {
            out.truncate(out.len() - n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_vector() {
        // RFC 6229-style known answer: key "Key", plaintext "Plaintext".
        let mut d = b"Plaintext".to_vec();
        rc4(b"Key", &mut d);
        assert_eq!(d, [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]);
    }

    #[test]
    fn aesv3_filter_under_revision_4_does_not_panic() {
        // Found by fuzzing: a 32-byte key asked of the MD5 key algorithm.
        let enc = crate::lexer::Lexer::new(
            b"<< /Filter /Standard /V 4 /R 4 /P -4 /O <0000000000000000000000000000000000000000000000000000000000000000> /U <0000000000000000000000000000000000000000000000000000000000000000> /CF << /StdCF << /CFM /AESV3 >> >> /StmF /StdCF /StrF /StdCF >>",
            0,
        )
        .object()
        .unwrap();
        assert_eq!(Handler::open(enc.as_dict().unwrap(), b"id", b"").err(), Some(Error::NeedsPassword));
    }
}

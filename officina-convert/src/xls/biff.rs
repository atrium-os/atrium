//! BIFF8 records (MS-XLS §2.1.4): the `Workbook` stream cut into records,
//! `CONTINUE` records joined to the one they continue, the stream decrypted
//! when a `FILEPASS` record says it is (RC4, plain or CryptoAPI), and the
//! string forms records hold.
//!
//! Every length is checked against what is left; nothing here allocates
//! more than the stream holds.

use md5::Md5;
use sha1::Sha1;
use sha2::Digest;

use crate::crypt::CryptError;

pub const BOF: u16 = 0x0809;
pub const EOF: u16 = 0x000A;
pub const CONTINUE: u16 = 0x003C;
pub const FILEPASS: u16 = 0x002F;
pub const BOUNDSHEET: u16 = 0x0085;

/// One record: its type and its data, with any `CONTINUE` records that
/// follow kept apart (strings that cross them restart their flags byte).
#[derive(Debug, Clone)]
pub struct Record {
    pub kind: u16,
    /// Offset of the record header in the stream.
    pub at: usize,
    pub data: Vec<u8>,
    /// Where each `CONTINUE` piece starts in `data`.
    pub breaks: Vec<usize>,
}

/// The records of a substream, from its `BOF` at offset `at`, until its
/// own `EOF` (included) or the stream's end: substreams nested in it (a
/// chart in a worksheet) come along, between their `BOF` and `EOF`.
/// `CONTINUE`s join the record before them.
pub fn records(stream: &[u8], at: usize) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::new();
    let mut p = at;
    let mut depth = 0u32;
    while p + 4 <= stream.len() {
        let kind = u16::from_le_bytes([stream[p], stream[p + 1]]);
        let len = u16::from_le_bytes([stream[p + 2], stream[p + 3]]) as usize;
        let end = (p + 4 + len).min(stream.len());
        let data = &stream[p + 4..end];
        if kind == CONTINUE {
            if let Some(last) = out.last_mut() {
                last.breaks.push(last.data.len());
                last.data.extend_from_slice(data);
                p = end;
                continue;
            }
        }
        out.push(Record { kind, at: p, data: data.to_vec(), breaks: Vec::new() });
        p = end;
        if kind == BOF && out.len() > 1 {
            depth += 1;
        }
        if kind == EOF {
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
    }
    out
}

/// A cursor over a record's data.
pub struct Cur<'a> {
    pub d: &'a [u8],
    pub p: usize,
    /// The record's `CONTINUE` breaks, for strings.
    breaks: &'a [usize],
}

/// The data ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Short;

impl<'a> Cur<'a> {
    pub fn new(r: &'a Record) -> Self {
        Cur { d: &r.data, p: 0, breaks: &r.breaks }
    }
    pub fn of(d: &'a [u8]) -> Self {
        Cur { d, p: 0, breaks: &[] }
    }
    pub fn left(&self) -> usize {
        self.d.len().saturating_sub(self.p)
    }
    pub fn skip(&mut self, n: usize) -> Result<(), Short> {
        if self.left() < n {
            return Err(Short);
        }
        self.p += n;
        Ok(())
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Short> {
        if self.left() < n {
            return Err(Short);
        }
        let b = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(b)
    }
    pub fn u8(&mut self) -> Result<u8, Short> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, Short> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn i16(&mut self) -> Result<i16, Short> {
        Ok(self.u16()? as i16)
    }
    pub fn u32(&mut self) -> Result<u32, Short> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn f64(&mut self) -> Result<f64, Short> {
        let b = self.bytes(8)?;
        Ok(f64::from_le_bytes(b.try_into().unwrap()))
    }

    /// The next `CONTINUE` break after the cursor, if any.
    fn next_break(&self) -> Option<usize> {
        self.breaks.iter().copied().find(|&b| b > self.p)
    }

    /// `cch` characters, 8- or 16-bit per `high`. Where character data
    /// meets a `CONTINUE` break, the next piece starts with a new flags byte
    /// (MS-XLS §2.5.293), so the width can change mid-string.
    pub fn chars(&mut self, cch: usize, mut high: bool) -> Result<String, Short> {
        let mut s = String::with_capacity(cch.min(self.left()));
        let mut units: Vec<u16> = Vec::new();
        let mut n = 0;
        while n < cch {
            if self.breaks.contains(&self.p) {
                high = self.u8()? & 1 != 0;
            }
            let end = self.next_break().unwrap_or(self.d.len()).min(self.d.len());
            let per = if high { 2 } else { 1 };
            let take = (cch - n).min(end.saturating_sub(self.p) / per);
            if take == 0 {
                return Err(Short);
            }
            let b = self.bytes(take * per)?;
            if high {
                units.extend(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            } else {
                // Latin-1 ("compressed" UTF-16).
                flush(&mut units, &mut s);
                s.extend(b.iter().map(|&c| c as char));
            }
            n += take;
        }
        flush(&mut units, &mut s);
        Ok(s)
    }

    /// XLUnicodeString (MS-XLS §2.5.294): cch u16, flags, characters.
    pub fn xl_string(&mut self) -> Result<String, Short> {
        let cch = self.u16()? as usize;
        self.rich_tail(cch)
    }

    /// ShortXLUnicodeString (§2.5.240): cch u8, flags, characters.
    pub fn short_string(&mut self) -> Result<String, Short> {
        let cch = self.u8()? as usize;
        let flags = self.u8()?;
        self.chars(cch, flags & 1 != 0)
    }

    /// The flags, optional run and phonetic counts, characters and the runs
    /// and phonetic data skipped (XLUnicodeRichExtendedString, §2.5.293).
    pub fn rich_tail(&mut self, cch: usize) -> Result<String, Short> {
        let flags = self.u8()?;
        let runs = if flags & 0x08 != 0 { self.u16()? as usize } else { 0 };
        let ext = if flags & 0x04 != 0 { self.u32()? as usize } else { 0 };
        let s = self.chars(cch, flags & 1 != 0)?;
        self.skip_across(runs * 4)?;
        self.skip_across(ext)?;
        Ok(s)
    }

    /// Skips `n` bytes that may cross `CONTINUE` breaks (no flags bytes).
    fn skip_across(&mut self, n: usize) -> Result<(), Short> {
        self.skip(n)
    }
}

fn flush(units: &mut Vec<u16>, s: &mut String) {
    if !units.is_empty() {
        s.extend(char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or('\u{FFFD}')));
    }
}

/// RC4.
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Rc4 {
        let mut s = [0u8; 256];
        for (k, v) in s.iter_mut().enumerate() {
            *v = k as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Rc4 { s, i: 0, j: 0 }
    }
    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k = self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize];
            *b ^= k;
        }
    }
}

/// How the stream is encrypted.
pub enum Cipher {
    /// RC4 with MD5 (MS-OFFCRYPTO §2.3.6): the truncated password hash.
    Rc4 { h: [u8; 5] },
    /// RC4 CryptoAPI (§2.3.5): the SHA-1 base hash and key length in bytes.
    CryptoApi { h: [u8; 20], key_len: usize },
}

impl Cipher {
    pub(crate) fn block_key(&self, block: u32) -> Vec<u8> {
        match self {
            Cipher::Rc4 { h } => {
                let mut m = Md5::new();
                m.update(h);
                m.update(block.to_le_bytes());
                m.finalize().to_vec()
            }
            Cipher::CryptoApi { h, key_len } => {
                let mut m = Sha1::new();
                m.update(h);
                m.update(block.to_le_bytes());
                let mut k = m.finalize()[..*key_len].to_vec();
                // A 40-bit key is used as 128 bits, zero-padded.
                if *key_len == 5 {
                    k.resize(16, 0);
                }
                k
            }
        }
    }
}

fn utf16(p: &str) -> Vec<u8> {
    p.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// The cipher a `FILEPASS` record describes, checked against `password`.
pub fn filepass(data: &[u8], password: &str) -> Result<Cipher, CryptError> {
    let mut c = Cur::of(data);
    let short = |_| CryptError::Damaged("FILEPASS".into());
    match c.u16().map_err(short)? {
        0 => return Err(CryptError::Unsupported("XOR obfuscation (Excel 95)".into())),
        1 => {}
        t => return Err(CryptError::Unsupported(format!("encryption type {t}"))),
    }
    rc4_info(&mut c, password)
}

/// The RC4 encryption header that follows the version (MS-OFFCRYPTO
/// §2.3.6.1 and §2.3.5.1), as `FILEPASS` and Word's table stream hold it,
/// checked against `password`.
pub fn rc4_info(c: &mut Cur, password: &str) -> Result<Cipher, CryptError> {
    let short = |_| CryptError::Damaged("encryption header".into());
    let (major, minor) = (c.u16().map_err(short)?, c.u16().map_err(short)?);
    match (major, minor) {
        (1, 1) => {
            let salt = c.bytes(16).map_err(short)?;
            let ev = c.bytes(16).map_err(short)?;
            let evh = c.bytes(16).map_err(short)?;
            let h0 = Md5::digest(utf16(password));
            let mut buf = Vec::with_capacity(21 * 16);
            for _ in 0..16 {
                buf.extend_from_slice(&h0[..5]);
                buf.extend_from_slice(salt);
            }
            let h1 = Md5::digest(&buf);
            let cipher = Cipher::Rc4 { h: h1[..5].try_into().unwrap() };
            let mut v = [ev, evh].concat();
            Rc4::new(&cipher.block_key(0)).apply(&mut v);
            if Md5::digest(&v[..16])[..] != v[16..32] {
                return Err(CryptError::WrongPassword);
            }
            Ok(cipher)
        }
        (2..=4, 2) => {
            let _flags = c.u32().map_err(short)?;
            let hsize = c.u32().map_err(short)? as usize;
            let h = c.bytes(hsize).map_err(short)?;
            if h.len() < 32 {
                return Err(CryptError::Damaged("FILEPASS header".into()));
            }
            let u = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
            let (alg, key_bits) = (u(8), u(16) as usize);
            if alg != 0x6801 {
                return Err(CryptError::Unsupported(format!("algorithm {alg:#x}")));
            }
            let key_len = match key_bits {
                0 => 5,
                40..=128 if key_bits % 8 == 0 => key_bits / 8,
                _ => return Err(CryptError::Damaged("key size".into())),
            };
            let salt_len = c.u32().map_err(short)? as usize;
            if salt_len != 16 {
                return Err(CryptError::Damaged("salt size".into()));
            }
            let salt = c.bytes(16).map_err(short)?;
            let ev = c.bytes(16).map_err(short)?;
            let hash_len = c.u32().map_err(short)? as usize;
            let evh = c.bytes(hash_len.min(c.left())).map_err(short)?;
            let mut m = Sha1::new();
            m.update(salt);
            m.update(utf16(password));
            let cipher = Cipher::CryptoApi { h: m.finalize().into(), key_len };
            let mut v = [ev, evh].concat();
            Rc4::new(&cipher.block_key(0)).apply(&mut v);
            if v.len() < 36 || Sha1::digest(&v[..16])[..] != v[16..36] {
                return Err(CryptError::WrongPassword);
            }
            Ok(cipher)
        }
        _ => Err(CryptError::Unsupported(format!("RC4 version {major}.{minor}"))),
    }
}

/// Excel's password for files protected only against writing: they open
/// without asking.
pub const DEFAULT_PASSWORD: &str = "VelvetSweatshop";

/// Decrypts `stream` in place from the record after `FILEPASS` on: record
/// headers stay as they are (the key stream still advances over them), and
/// so do the records MS-XLS §2.2.10 leaves in the clear and a sheet's
/// stream offset in `BOUNDSHEET`. RC4 restarts every 1,024 bytes.
pub fn decrypt(stream: &mut [u8], from: usize, cipher: &Cipher) {
    // The key stream, a block at a time.
    let mut block = u32::MAX;
    let mut ks = [0u8; 1024];
    let key_at = |pos: usize, block: &mut u32, ks: &mut [u8; 1024]| -> u8 {
        let b = (pos / 1024) as u32;
        if b != *block {
            *block = b;
            *ks = [0u8; 1024];
            Rc4::new(&cipher.block_key(b)).apply(ks);
        }
        ks[pos % 1024]
    };
    let mut p = from;
    while p + 4 <= stream.len() {
        let kind = u16::from_le_bytes([stream[p], stream[p + 1]]);
        let len = u16::from_le_bytes([stream[p + 2], stream[p + 3]]) as usize;
        let start = p + 4;
        let end = (start + len).min(stream.len());
        let clear = matches!(kind, BOF | FILEPASS | 0x0194 /* USREXCL */ | 0x0195 /* FILELOCK */ | 0x00E1 /* INTERFACEHDR */ | 0x0196 /* RRDINFO */ | 0x0138 /* RRDHEAD */);
        if !clear {
            let skip = if kind == BOUNDSHEET { 4 } else { 0 };
            for q in start + skip.min(end - start)..end {
                stream[q] ^= key_at(q, &mut block, &mut ks);
            }
        }
        p = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: u16, data: &[u8]) -> Vec<u8> {
        let mut v = kind.to_le_bytes().to_vec();
        v.extend_from_slice(&(data.len() as u16).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn continues_join_and_strings_cross_them() {
        // An SST-like record: one string of 6 characters, 3 in the record and
        // 3 in a CONTINUE that switches to 16-bit characters.
        let mut s = rec(0x00FC, &[6, 0, 0, b'a', b'b', b'c']);
        s.extend(rec(CONTINUE, &[1, b'd', 0, 0xE9, 0, b'f', 0]));
        s.extend(rec(EOF, &[]));
        let r = records(&s, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].breaks, vec![6]);
        let mut c = Cur::new(&r[0]);
        assert_eq!(c.xl_string().unwrap(), "abcdéf");
        assert_eq!(c.left(), 0);
    }

    #[test]
    fn short_data_is_an_error_not_a_panic() {
        let r = Record { kind: 1, at: 0, data: vec![200, 0, 0, b'a'], breaks: vec![] };
        assert!(Cur::new(&r).xl_string().is_err());
        let r = Record { kind: 1, at: 0, data: vec![3, 0, 1, b'a', 0, b'b'], breaks: vec![3] };
        assert!(Cur::new(&r).xl_string().is_err());
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let r = Record { kind: 1, at: 0, data: data.clone(), breaks: vec![n / 2, n / 2 + 1] };
            let _ = Cur::new(&r).xl_string();
            let _ = Cur::new(&r).short_string();
            let _ = filepass(&data, "x");
        }
    }

    #[test]
    fn rc4_known_answer() {
        // RFC 6229's first vector: key 0102030405, first 16 bytes.
        let mut z = [0u8; 16];
        Rc4::new(&[1, 2, 3, 4, 5]).apply(&mut z);
        assert_eq!(z[..8], [0xb2, 0x39, 0x63, 0x05, 0xf0, 0x3d, 0xc0, 0x27]);
    }
}

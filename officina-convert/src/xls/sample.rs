//! A small BIFF8 workbook built record by record, for tests here and in
//! the apps: sheet "Data" with 2 in A1, "hello" in B1 and `=A1*21` in C1
//! (saved result 42), optionally protected with a password (RC4 CryptoAPI,
//! as Excel and LibreOffice write it).

use sha1::{Digest, Sha1};

use super::biff::{self, Cipher, Rc4};

fn rec(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
}

fn bof(dt: u16) -> Vec<u8> {
    let mut d = vec![0x00, 0x06];
    d.extend_from_slice(&dt.to_le_bytes());
    d.extend_from_slice(&[0; 12]);
    d
}

/// The workbook as a `.xls` file.
pub fn workbook(password: Option<&str>) -> Vec<u8> {
    let mut s = Vec::new();
    rec(&mut s, biff::BOF, &bof(0x0005));
    let cipher = password.map(|pw| {
        let salt = [7u8; 16];
        let verifier = [0x5Au8; 16];
        let mut m = Sha1::new();
        m.update(salt);
        m.update(pw.encode_utf16().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>());
        let cipher = Cipher::CryptoApi { h: m.finalize().into(), key_len: 16 };
        let mut v = verifier.to_vec();
        v.extend_from_slice(&Sha1::digest(verifier));
        Rc4::new(&cipher.block_key(0)).apply(&mut v);
        let mut d = Vec::new();
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        // EncryptionHeader: flags, extra, RC4, SHA-1, 128 bits, provider,
        // reserved, no CSP name.
        let header: Vec<u8> = [0x04u32, 0, 0x6801, 0x8004, 128, 1, 0, 0].iter().flat_map(|x| x.to_le_bytes()).collect();
        d.extend_from_slice(&(header.len() as u32).to_le_bytes());
        d.extend_from_slice(&header);
        d.extend_from_slice(&16u32.to_le_bytes());
        d.extend_from_slice(&salt);
        d.extend_from_slice(&v[..16]);
        d.extend_from_slice(&20u32.to_le_bytes());
        d.extend_from_slice(&v[16..]);
        rec(&mut s, biff::FILEPASS, &d);
        (s.len(), cipher)
    });
    for _ in 0..16 {
        let mut xf = [0u8; 20];
        xf[6] = 0x20;
        rec(&mut s, 0x00E0, &xf);
    }
    let sheet_at = s.len() + 4;
    rec(&mut s, biff::BOUNDSHEET, &[0, 0, 0, 0, 0, 0, 4, 0, b'D', b'a', b't', b'a']);
    rec(&mut s, 0x00FC, &[1, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, b'h', b'e', b'l', b'l', b'o']);
    rec(&mut s, biff::EOF, &[]);
    let at = s.len() as u32;
    s[sheet_at..sheet_at + 4].copy_from_slice(&at.to_le_bytes());
    rec(&mut s, biff::BOF, &bof(0x0010));
    let mut num = vec![0, 0, 0, 0, 15, 0];
    num.extend_from_slice(&2.0f64.to_le_bytes());
    rec(&mut s, 0x0203, &num);
    rec(&mut s, 0x00FD, &[0, 0, 1, 0, 15, 0, 0, 0, 0, 0]);
    let mut f = vec![0, 0, 2, 0, 15, 0];
    f.extend_from_slice(&42.0f64.to_le_bytes());
    f.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let rgce = [0x24, 0, 0, 0, 0xC0, 0x1E, 21, 0, 0x05];
    f.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    f.extend_from_slice(&rgce);
    rec(&mut s, 0x0006, &f);
    rec(&mut s, biff::EOF, &[]);
    if let Some((from, cipher)) = cipher {
        // RC4 is its own inverse.
        biff::decrypt(&mut s, from, &cipher);
    }
    crate::cfb::write(&[("Workbook", &s)])
}

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

/// An RC4 CryptoAPI cipher for `pw` and its encryption header as BIFF8's
/// FILEPASS and Word's table stream carry it (from the version on).
pub fn crypto_api(pw: &str) -> (Cipher, Vec<u8>) {
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
    (cipher, d)
}

/// The workbook as a `.xls` file.
pub fn workbook(password: Option<&str>) -> Vec<u8> {
    let mut s = Vec::new();
    rec(&mut s, biff::BOF, &bof(0x0005));
    let cipher = password.map(|pw| {
        let (cipher, header) = crypto_api(pw);
        let mut d = 1u16.to_le_bytes().to_vec();
        d.extend_from_slice(&header);
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

/// BIFF8 desktop validation fixture. The malformed record exercises local
/// recovery without interrupting the valid rules that follow it.
pub fn validation_workbook() -> Vec<u8> {
    fn dv(out: &mut Vec<u8>, flags: u32, title: &str, message: &str, first: &[u8], second: &[u8], ranges: &[[u16; 4]]) {
        let mut data = flags.to_le_bytes().to_vec();
        for text in ["Choose", title, "Enter an allowed value.", message] {
            let units: Vec<_> = text.encode_utf16().collect();
            data.extend_from_slice(&(units.len() as u16).to_le_bytes());
            data.push(1);
            for unit in units {
                data.extend_from_slice(&unit.to_le_bytes());
            }
        }
        for tokens in [first, second] {
            data.extend_from_slice(&(tokens.len() as u16).to_le_bytes());
            data.extend_from_slice(&[0, 0]);
            data.extend_from_slice(tokens);
        }
        data.extend_from_slice(&(ranges.len() as u16).to_le_bytes());
        for range in ranges {
            for bound in range {
                data.extend_from_slice(&bound.to_le_bytes());
            }
        }
        rec(out, 0x01BE, &data);
    }

    let mut s = Vec::new();
    rec(&mut s, biff::BOF, &bof(0x0005));
    let sheet_at = s.len() + 4;
    rec(&mut s, biff::BOUNDSHEET, &[0, 0, 0, 0, 0, 0, 6, 0, b'I', b'n', b'p', b'u', b't', b's']);
    rec(&mut s, biff::EOF, &[]);
    let at = s.len() as u32;
    s[sheet_at..sheet_at + 4].copy_from_slice(&at.to_le_bytes());
    rec(&mut s, biff::BOF, &bof(0x0010));
    for (row, col, value) in [
        (1u16, 1u16, 2.0f64), (2, 1, 3.0), (1, 2, 6.0), (1, 3, 9.0), (2, 2, 4.0),
        (2, 3, 5.0), (1, 4, 7.0), (2, 4, 9.0), (1, 5, 6.0), (2, 5, 8.0),
        (1, 8, 7.0), (2, 8, 7.0),
    ] {
        let mut data = Vec::new();
        for value in [row, col, 0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&value.to_le_bytes());
        rec(&mut s, 0x0203, &data);
    }
    let mut sum = vec![1, 0, 6, 0, 0, 0];
    sum.extend_from_slice(&14.0f64.to_le_bytes());
    sum.extend_from_slice(&[0; 6]);
    let add = [
        0x44, 1, 0, 1, 0xC0, 0x44, 2, 0, 1, 0xC0, 0x03,
        0x44, 2, 0, 2, 0xC0, 0x03, 0x44, 2, 0, 3, 0xC0, 0x03,
    ];
    sum.extend_from_slice(&(add.len() as u16).to_le_bytes());
    sum.extend_from_slice(&add);
    rec(&mut s, 0x0006, &sum);

    let mut header = [0; 18];
    header[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    header[14..18].copy_from_slice(&4u32.to_le_bytes());
    rec(&mut s, 0x01B2, &header);
    rec(&mut s, 0x01BE, &[0; 6]);

    let flags = (1 << 8) | (1 << 18) | (1 << 19);
    dv(&mut s, flags | 1, "Outside limit", "Use a whole number from 1 to 8.",
        &[0x1E, 1, 0], &[0x1E, 8, 0], &[[1, 2, 1, 1], [2, 2, 2, 3]]);
    let items = b"2,0\x006\x008\x00say \"yes\"\x00literal _x000D_";
    let mut literal = vec![0x17, items.len() as u8, 0];
    literal.extend_from_slice(items);
    dv(&mut s, flags | 3 | (1 << 7), "Outside list", "Choose an item from the list.",
        &literal, &[], &[[1, 2, 7, 7]]);
    // I2 > F2, relative to each validated row. I3=7 fails against F3=8.
    dv(&mut s, flags | 7, "Too low", "Enter more than the comparison cell.",
        &[0x4C, 0, 0, 0, 0xC0, 0x4C, 0, 0, 0xFD, 0xC0, 0x0D], &[], &[[1, 2, 8, 8]]);
    rec(&mut s, biff::EOF, &[]);
    crate::cfb::write(&[("Workbook", &s)])
}

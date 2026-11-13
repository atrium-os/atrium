//! CCMP-128 and BIP-CMAC-128 as a radio does them in hardware
//! (IEEE 802.11-2020 §12.5.3, §12.5.4). Test only: aura_sim has no crypto
//! engine, so aura-air plays it.

use aes::Aes128;
use ccm::aead::{generic_array::GenericArray, AeadInPlace, KeyInit};
use ccm::consts::{U13, U8};
use cmac::{Cmac, Mac};

type Ccmp = ccm::Ccm<Aes128, U8, U13>;

pub fn is_mgmt(f: &[u8]) -> bool {
    f[0] & 0x0c == 0x00
}

fn is_qos_data(f: &[u8]) -> bool {
    f[0] & 0x0c == 0x08 && f[0] & 0x80 != 0
}

/// MAC header length of a management or data frame.
pub fn hdr_len(f: &[u8]) -> Option<usize> {
    if f.len() < 24 {
        return None;
    }
    let mut n = 24;
    match f[0] & 0x0c {
        0x00 => {}
        0x08 => {
            if f[1] & 0x03 == 0x03 {
                n += 6;
            }
            if is_qos_data(f) {
                n += 2;
                if f[1] & 0x80 != 0 {
                    n += 4; // HT control
                }
            }
        }
        _ => return None,
    }
    (f.len() >= n).then_some(n)
}

fn aad(f: &[u8], hlen: usize) -> Vec<u8> {
    let mut a = Vec::with_capacity(30);
    let fc0 = if is_mgmt(f) { f[0] } else { f[0] & 0x8f };
    let mut fc1 = (f[1] & !0x38) | 0x40;
    if is_qos_data(f) {
        fc1 &= !0x80;
    }
    a.push(fc0);
    a.push(fc1);
    a.extend_from_slice(&f[4..22]);
    a.push(f[22] & 0x0f);
    a.push(0);
    let mut p = 24;
    if !is_mgmt(f) && f[1] & 0x03 == 0x03 {
        a.extend_from_slice(&f[24..30]);
        p = 30;
    }
    if is_qos_data(f) {
        a.push(f[p] & 0x0f);
        a.push(0);
    }
    let _ = hlen;
    a
}

fn nonce(f: &[u8], hlen: usize, pn: u64) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[0] = if is_mgmt(f) {
        0x10
    } else if is_qos_data(f) {
        let qc = if f[1] & 0x80 != 0 { hlen - 6 } else { hlen - 2 };
        f[qc] & 0x0f
    } else {
        0
    };
    n[1..7].copy_from_slice(&f[10..16]);
    n[7..13].copy_from_slice(&pn.to_be_bytes()[2..8]);
    n
}

/// Encrypts a plaintext frame: sets Protected, inserts the CCMP header,
/// appends the MIC.
pub fn ccmp_encrypt(tk: &[u8; 16], f: &[u8], pn: u64, keyid: u8) -> Option<Vec<u8>> {
    let hlen = hdr_len(f)?;
    let mut hdr = f[..hlen].to_vec();
    hdr[1] |= 0x40;
    let p = pn.to_le_bytes();
    let mut out = hdr.clone();
    out.extend_from_slice(&[p[0], p[1], 0, 0x20 | (keyid << 6), p[2], p[3], p[4], p[5]]);
    let mut body = f[hlen..].to_vec();
    let c = Ccmp::new(GenericArray::from_slice(tk));
    let tag = c
        .encrypt_in_place_detached(GenericArray::from_slice(&nonce(&hdr, hlen, pn)), &aad(&hdr, hlen), &mut body)
        .ok()?;
    out.extend_from_slice(&body);
    out.extend_from_slice(&tag);
    Some(out)
}

/// The key id and PN of a protected frame, from its CCMP header.
pub fn ccmp_hdr(f: &[u8]) -> Option<(u8, u64)> {
    let hlen = hdr_len(f)?;
    let h = f.get(hlen..hlen + 8)?;
    if h[3] & 0x20 == 0 {
        return None;
    }
    let pn = u64::from_le_bytes([h[0], h[1], h[4], h[5], h[6], h[7], 0, 0]);
    Some((h[3] >> 6, pn))
}

/// Decrypts a protected frame: the result keeps the MAC header (Protected
/// still set) and drops the CCMP header and MIC, as hardware delivers it.
pub fn ccmp_decrypt(tk: &[u8; 16], f: &[u8]) -> Option<(Vec<u8>, u8, u64)> {
    let hlen = hdr_len(f)?;
    if f[1] & 0x40 == 0 || f.len() < hlen + 16 {
        return None;
    }
    let (keyid, pn) = ccmp_hdr(f)?;
    let mut body = f[hlen + 8..f.len() - 8].to_vec();
    let tag = &f[f.len() - 8..];
    let c = Ccmp::new(GenericArray::from_slice(tk));
    c.decrypt_in_place_detached(
        GenericArray::from_slice(&nonce(f, hlen, pn)),
        &aad(f, hlen),
        &mut body,
        GenericArray::from_slice(tag),
    )
    .ok()?;
    let mut out = f[..hlen].to_vec();
    out.extend_from_slice(&body);
    Some((out, keyid, pn))
}

/// Checks the MMIE of a group-addressed robust management frame with
/// BIP-CMAC-128. Returns the key id and IPN.
pub fn bip_check(igtk: &[u8; 16], f: &[u8]) -> Option<(u8, u64)> {
    if f.len() < 24 + 18 {
        return None;
    }
    let m = &f[f.len() - 18..];
    if m[0] != 76 || m[1] != 16 {
        return None;
    }
    let keyid = u16::from_le_bytes([m[2], m[3]]);
    let ipn = u64::from_le_bytes([m[4], m[5], m[6], m[7], m[8], m[9], 0, 0]);
    let mut c = <Cmac<Aes128> as Mac>::new_from_slice(igtk).ok()?;
    c.update(&[f[0], f[1] & !0x38]);
    c.update(&f[4..22]);
    c.update(&f[24..f.len() - 8]);
    c.update(&[0u8; 8]);
    let mic = c.finalize().into_bytes();
    if mic[..8] != f[f.len() - 8..] || !(4..=5).contains(&keyid) {
        return None;
    }
    Some((keyid as u8, ipn))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn ccmp_vector() {
        // IEEE 802.11-2020 J.6.4.
        let tk: [u8; 16] = hex("c97c1f67ce371185514a8a19f2bdd52f").try_into().unwrap();
        let hdr = hex("08 48 c3 2c 0f d2 e1 28 a5 7c 50 30 f1 84 44 08 ab ae a5 b8 fc ba 80 33");
        let pt = hex("f8 ba 1a 55 d0 2f 85 ae 96 7b b6 2f b6 cd a8 eb 7e 78 a0 50");
        let mut f = hdr.clone();
        f.extend_from_slice(&pt);
        let e = ccmp_encrypt(&tk, &f, 0xB5039776E70C, 0).unwrap();
        assert_eq!(e[24..32].to_vec(), hex("0c e7 00 20 76 97 03 b5"));
        assert_eq!(
            e[32..].to_vec(),
            hex("f3 d0 a2 fe 9a 3d bf 23 42 a6 43 e4 32 46 e8 0c 3c 04 d0 19 78 45 ce 0b 16 f9 76 23")
        );
        let (d, kid, pn) = ccmp_decrypt(&tk, &e).unwrap();
        assert_eq!((kid, pn), (0, 0xB5039776E70C));
        assert_eq!(d[24..].to_vec(), pt);
        let mut bad = e.clone();
        bad[40] ^= 1;
        assert!(ccmp_decrypt(&tk, &bad).is_none());
    }

    #[test]
    fn qos_and_mgmt_roundtrip() {
        let tk = [7u8; 16];
        let mut qos = vec![0x88, 0x02, 0, 0];
        qos.extend_from_slice(&[2, 0xa5, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 9, 0x10, 0, 6, 0]);
        qos.extend_from_slice(b"\xaa\xaa\x03\x00\x00\x00\x08\x00hello");
        let e = ccmp_encrypt(&tk, &qos, 77, 0).unwrap();
        let (d, _, pn) = ccmp_decrypt(&tk, &e).unwrap();
        assert_eq!(pn, 77);
        assert_eq!(d[26..], qos[26..]);
        let mut deauth = vec![0xc0, 0x00, 0, 0];
        deauth.extend_from_slice(&[2, 0xa5, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0x20, 0, 7, 0]);
        let e = ccmp_encrypt(&tk, &deauth, 3, 0).unwrap();
        assert!(ccmp_decrypt(&tk, &e).is_some());
    }
}

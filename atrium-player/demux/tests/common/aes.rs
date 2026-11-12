//! AES-128 and the two CENC schemes, for checking the demuxers' sample info.
//! Test-only and slow: table-free, straight from FIPS-197.
#![allow(dead_code)]

fn sbox() -> ([u8; 256], [u8; 256]) {
    let (mut s, mut inv) = ([0u8; 256], [0u8; 256]);
    let (mut p, mut q) = (1u8, 1u8);
    loop {
        // p *= 3, q /= 3 in GF(2^8)
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
        s[p as usize] = x;
        inv[x as usize] = p;
        if p == 1 {
            break;
        }
    }
    s[0] = 0x63;
    inv[0x63] = 0;
    (s, inv)
}

fn xt(a: u8) -> u8 {
    (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 }
}
fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xt(a);
        b >>= 1;
    }
    r
}

pub struct Aes {
    rk: [[u8; 16]; 11],
    s: [u8; 256],
    inv: [u8; 256],
}

impl Aes {
    pub fn new(key: &[u8; 16]) -> Aes {
        let (s, inv) = sbox();
        let mut rk = [[0u8; 16]; 11];
        rk[0] = *key;
        let mut rc = 1u8;
        for r in 1..11 {
            let p = rk[r - 1];
            let mut t = [s[p[13] as usize] ^ rc, s[p[14] as usize], s[p[15] as usize], s[p[12] as usize]];
            for w in 0..4 {
                for b in 0..4 {
                    t[b] ^= p[w * 4 + b];
                    rk[r][w * 4 + b] = t[b];
                }
            }
            rc = xt(rc);
        }
        Aes { rk, s, inv }
    }

    pub fn encrypt(&self, b: &mut [u8; 16]) {
        for i in 0..16 {
            b[i] ^= self.rk[0][i];
        }
        for r in 1..11 {
            let mut t = [0u8; 16];
            for c in 0..4 {
                for row in 0..4 {
                    t[c * 4 + row] = self.s[b[((c + row) % 4) * 4 + row] as usize];
                }
            }
            if r != 10 {
                for c in 0..4 {
                    let a = [t[c * 4], t[c * 4 + 1], t[c * 4 + 2], t[c * 4 + 3]];
                    for row in 0..4 {
                        t[c * 4 + row] = mul(a[row], 2) ^ mul(a[(row + 1) % 4], 3) ^ a[(row + 2) % 4] ^ a[(row + 3) % 4];
                    }
                }
            }
            for i in 0..16 {
                b[i] = t[i] ^ self.rk[r][i];
            }
        }
    }

    pub fn decrypt(&self, b: &mut [u8; 16]) {
        for i in 0..16 {
            b[i] ^= self.rk[10][i];
        }
        for r in (0..10).rev() {
            let mut t = [0u8; 16];
            for c in 0..4 {
                for row in 0..4 {
                    t[((c + row) % 4) * 4 + row] = self.inv[b[c * 4 + row] as usize];
                }
            }
            for i in 0..16 {
                t[i] ^= self.rk[r][i];
            }
            if r != 0 {
                for c in 0..4 {
                    let a = [t[c * 4], t[c * 4 + 1], t[c * 4 + 2], t[c * 4 + 3]];
                    for row in 0..4 {
                        t[c * 4 + row] = mul(a[row], 14) ^ mul(a[(row + 1) % 4], 11) ^ mul(a[(row + 2) % 4], 13) ^ mul(a[(row + 3) % 4], 9);
                    }
                }
            }
            *b = t;
        }
    }
}

/// Decrypt a sample in place by its subsample map (clear, cipher) pairs; an empty map means
/// all of it is encrypted.
pub fn decrypt_sample(aes: &Aes, cbcs: bool, iv: &[u8], subs: &[(u32, u32)], pattern: (u32, u32), data: &mut [u8]) {
    let whole = [(0u32, data.len() as u32)];
    let subs = if subs.is_empty() { &whole[..] } else { subs };
    let mut iv16 = [0u8; 16];
    iv16[..iv.len()].copy_from_slice(iv);
    let mut ctr = iv16;
    let mut ks = [0u8; 16];
    let mut used = 16;
    let mut at = 0usize;
    for &(clear, cipher) in subs {
        at += clear as usize;
        let part = &mut data[at..at + cipher as usize];
        at += cipher as usize;
        if !cbcs {
            for b in part.iter_mut() {
                if used == 16 {
                    ks = ctr;
                    aes.encrypt(&mut ks);
                    for i in (0..16).rev() {
                        ctr[i] = ctr[i].wrapping_add(1);
                        if ctr[i] != 0 {
                            break;
                        }
                    }
                    used = 0;
                }
                *b ^= ks[used];
                used += 1;
            }
            continue;
        }
        let (crypt, skip) = if pattern == (0, 0) { (1, 0) } else { (pattern.0 as usize, pattern.1 as usize) };
        let mut prev = iv16;
        let blocks = part.len() / 16;
        let mut i = 0;
        while i < blocks {
            for _ in 0..crypt.min(blocks - i) {
                let blk: &mut [u8; 16] = (&mut part[i * 16..i * 16 + 16]).try_into().unwrap();
                let c = *blk;
                aes.decrypt(blk);
                for k in 0..16 {
                    blk[k] ^= prev[k];
                }
                prev = c;
                i += 1;
            }
            i += skip;
        }
    }
}

#[test]
fn fips197() {
    let key: [u8; 16] = core::array::from_fn(|i| i as u8);
    let mut b: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
    let a = Aes::new(&key);
    a.encrypt(&mut b);
    assert_eq!(b, [0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a]);
    a.decrypt(&mut b);
    assert_eq!(b, core::array::from_fn(|i| (i as u8) * 0x11));
}

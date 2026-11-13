//! The RSN element (§9.4.2.24).

pub const ID: u8 = 48;

pub const CIPHER_CCMP128: u32 = 0x000f_ac04;
pub const CIPHER_TKIP: u32 = 0x000f_ac02;
pub const CIPHER_WEP40: u32 = 0x000f_ac01;
pub const CIPHER_WEP104: u32 = 0x000f_ac05;
pub const CIPHER_BIP_CMAC128: u32 = 0x000f_ac06;
pub const CIPHER_GCMP128: u32 = 0x000f_ac08;
pub const CIPHER_GCMP256: u32 = 0x000f_ac09;
pub const CIPHER_CCMP256: u32 = 0x000f_ac0a;
pub const CIPHER_NO_GROUP: u32 = 0x000f_ac07;

pub const CAP_MFPR: u16 = 1 << 6;
pub const CAP_MFPC: u16 = 1 << 7;

const MAX_SUITES: usize = 8;

/// A parsed RSNE. Unknown suites are kept so policy can see them; counts are
/// bounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rsne {
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akms: Vec<u32>,
    pub caps: u16,
    pub pmkids: usize,
    /// Group management cipher; absent means BIP-CMAC-128 when MFPC is set.
    pub group_mgmt: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRsne;

fn suite(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Rsne {
    /// Parses the element body (after id and length).
    pub fn parse(body: &[u8]) -> Result<Rsne, BadRsne> {
        let mut p = body;
        let take = |p: &mut &[u8], n: usize| -> Result<Vec<u8>, BadRsne> {
            if p.len() < n {
                return Err(BadRsne);
            }
            let (a, b) = p.split_at(n);
            *p = b;
            Ok(a.to_vec())
        };
        let v = take(&mut p, 2)?;
        if u16::from_le_bytes([v[0], v[1]]) != 1 {
            return Err(BadRsne);
        }
        // Defaults when the element is truncated (§9.4.2.24.1).
        let mut r = Rsne {
            group: CIPHER_CCMP128,
            pairwise: vec![CIPHER_CCMP128],
            akms: vec![0x000f_ac01],
            caps: 0,
            pmkids: 0,
            group_mgmt: None,
        };
        if p.is_empty() {
            return Ok(r);
        }
        r.group = suite(&take(&mut p, 4)?);
        if p.is_empty() {
            return Ok(r);
        }
        let list = |p: &mut &[u8]| -> Result<Vec<u32>, BadRsne> {
            let n = take(p, 2)?;
            let n = u16::from_le_bytes([n[0], n[1]]) as usize;
            if n == 0 || n > MAX_SUITES {
                return Err(BadRsne);
            }
            let b = take(p, 4 * n)?;
            Ok(b.chunks(4).map(suite).collect())
        };
        r.pairwise = list(&mut p)?;
        if p.is_empty() {
            return Ok(r);
        }
        r.akms = list(&mut p)?;
        if p.is_empty() {
            return Ok(r);
        }
        let c = take(&mut p, 2)?;
        r.caps = u16::from_le_bytes([c[0], c[1]]);
        if p.is_empty() {
            return Ok(r);
        }
        let n = take(&mut p, 2)?;
        let n = u16::from_le_bytes([n[0], n[1]]) as usize;
        if n > 16 {
            return Err(BadRsne);
        }
        take(&mut p, 16 * n)?;
        r.pmkids = n;
        if p.is_empty() {
            return Ok(r);
        }
        r.group_mgmt = Some(suite(&take(&mut p, 4)?));
        // Trailing octets are allowed by later amendments; ignored.
        Ok(r)
    }

    /// The element bytes (id, length, body) for an association request:
    /// one pairwise cipher, one AKM.
    pub fn build(group: u32, pairwise: u32, akm: u32, caps: u16, group_mgmt: Option<u32>) -> Vec<u8> {
        let mut b = vec![ID, 0, 1, 0];
        b.extend_from_slice(&group.to_be_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&pairwise.to_be_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&akm.to_be_bytes());
        b.extend_from_slice(&caps.to_le_bytes());
        if let Some(g) = group_mgmt {
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&g.to_be_bytes());
        }
        b[1] = (b.len() - 2) as u8;
        b
    }

    pub fn mfpc(&self) -> bool {
        self.caps & CAP_MFPC != 0
    }
    pub fn mfpr(&self) -> bool {
        self.caps & CAP_MFPR != 0
    }
}

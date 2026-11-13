//! Information elements and the BSS description built from a beacon or
//! probe response (IEEE 802.11-2020 §9.4.2). Every input here comes off the
//! air (attackers A1, A2 in wifi.md §8.2): parsing is total, bounded and
//! fuzzed.

use aura_rsn::crypto::Akm;
use aura_rsn::rsne::{self, Rsne};

pub const SSID: u8 = 0;
pub const RATES: u8 = 1;
pub const DS_PARAMS: u8 = 3;
pub const TIM: u8 = 5;
pub const HT_CAP: u8 = 45;
pub const RSN: u8 = 48;
pub const EXT_RATES: u8 = 50;
pub const HT_OP: u8 = 61;
pub const VHT_CAP: u8 = 191;
pub const VENDOR: u8 = 221;
pub const EXT: u8 = 255;
pub const EXT_HE_CAP: u8 = 35;
pub const MMIE: u8 = 76;

pub const CAP_ESS: u16 = 1 << 0;
pub const CAP_PRIVACY: u16 = 1 << 4;
pub const CAP_SHORT_PREAMBLE: u16 = 1 << 5;
pub const CAP_SHORT_SLOT: u16 = 1 << 10;

/// Iterates the elements of a frame body. Stops at the first truncated
/// element; [`Ies::truncated`] tells whether that happened.
#[derive(Clone)]
pub struct Ies<'a> {
    rest: &'a [u8],
    bad: bool,
}

impl<'a> Ies<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Ies { rest: b, bad: false }
    }
    pub fn truncated(mut self) -> bool {
        for _ in self.by_ref() {}
        self.bad
    }
}

impl<'a> Iterator for Ies<'a> {
    /// (element id, body)
    type Item = (u8, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.len() < 2 || self.rest.len() < 2 + self.rest[1] as usize {
            self.bad = true;
            self.rest = &[];
            return None;
        }
        let n = self.rest[1] as usize;
        let item = (self.rest[0], &self.rest[2..2 + n]);
        self.rest = &self.rest[2 + n..];
        Some(item)
    }
}

/// Whether the AP must, may or cannot use management frame protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pmf {
    No,
    Capable,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Open,
    Psk { akm: Akm, pmf: Pmf },
    /// WPA3-SAE only (W2).
    Sae,
    /// Refused outright (WEP, TKIP, WPA1, unknown AKMs): wifi.md §3.4.
    Refused(&'static str),
}

impl Security {
    pub fn name(&self) -> &'static str {
        match self {
            Security::Open => "open",
            Security::Psk { akm: Akm::Psk, .. } => "psk",
            Security::Psk { akm: Akm::PskSha256, .. } => "psk-sha256",
            Security::Sae => "sae",
            Security::Refused(w) => w,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BssInfo {
    pub bssid: [u8; 6],
    pub freq: u16,
    pub rssi: i8,
    pub ssid: Vec<u8>,
    pub cap: u16,
    pub beacon_int: u16,
    pub dtim: u8,
    /// Supported rates as advertised (500 kb/s units, 0x80 = basic).
    pub rates: Vec<u8>,
    /// The whole RSN element, for the handshake's downgrade check.
    pub rsne: Option<Vec<u8>>,
    pub security: Security,
    pub wmm: bool,
    pub ht: bool,
    pub vht: bool,
    pub he: bool,
}

fn is_wmm(b: &[u8]) -> bool {
    b.len() >= 5 && b[..3] == [0x00, 0x50, 0xf2] && b[3] == 2 && b[4] <= 1
}

fn is_wpa1(b: &[u8]) -> bool {
    b.len() >= 4 && b[..4] == [0x00, 0x50, 0xf2, 1]
}

/// Classifies an AP's security from its capability field and elements.
pub fn security(cap: u16, rsne_el: Option<&[u8]>, wpa1: bool) -> Security {
    let Some(el) = rsne_el else {
        return if cap & CAP_PRIVACY != 0 || wpa1 { Security::Refused(if wpa1 { "wpa1" } else { "wep" }) } else { Security::Open };
    };
    let Ok(r) = Rsne::parse(&el[2..]) else {
        return Security::Refused("bad-rsne");
    };
    if r.group != rsne::CIPHER_CCMP128 {
        return Security::Refused(if r.group == rsne::CIPHER_TKIP { "tkip" } else { "group-cipher" });
    }
    if !r.pairwise.contains(&rsne::CIPHER_CCMP128) {
        return Security::Refused("pairwise-cipher");
    }
    if r.mfpr() && !r.mfpc() {
        return Security::Refused("bad-pmf");
    }
    let pmf = match (r.mfpc(), r.mfpr()) {
        (_, true) => Pmf::Required,
        (true, false) => Pmf::Capable,
        _ => Pmf::No,
    };
    if r.mfpc() && r.group_mgmt.is_some_and(|g| g != rsne::CIPHER_BIP_CMAC128) {
        return Security::Refused("group-mgmt-cipher");
    }
    let has = |s: u32| r.akms.contains(&s);
    if pmf != Pmf::No && has(Akm::PskSha256.suite()) {
        Security::Psk { akm: Akm::PskSha256, pmf }
    } else if has(Akm::Psk.suite()) {
        Security::Psk { akm: Akm::Psk, pmf }
    } else if has(Akm::PskSha256.suite()) {
        Security::Psk { akm: Akm::PskSha256, pmf }
    } else if has(0x000f_ac08) {
        Security::Sae
    } else {
        Security::Refused("akm")
    }
}

/// A beacon (subtype 8) or probe response (subtype 5) to a BSS description.
pub fn parse_bss(frame: &[u8], freq: u16, rssi: i8) -> Option<BssInfo> {
    if frame.len() < 36 || !matches!(frame[0], 0x80 | 0x50) {
        return None;
    }
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&frame[16..22]);
    if bssid[0] & 1 != 0 || frame[10..16] != bssid {
        return None;
    }
    let beacon_int = u16::from_le_bytes([frame[32], frame[33]]);
    let cap = u16::from_le_bytes([frame[34], frame[35]]);
    if cap & CAP_ESS == 0 || beacon_int == 0 {
        return None; // IBSS/mesh or nonsense
    }
    let mut b = BssInfo {
        bssid,
        freq,
        rssi,
        ssid: Vec::new(),
        cap,
        beacon_int,
        dtim: 1,
        rates: Vec::new(),
        rsne: None,
        security: Security::Open,
        wmm: false,
        ht: false,
        vht: false,
        he: false,
    };
    let mut seen_ssid = false;
    let mut wpa1 = false;
    let ies = Ies::new(&frame[36..]);
    for (id, body) in ies.clone() {
        match id {
            SSID if !seen_ssid => {
                if body.len() > 32 {
                    return None;
                }
                b.ssid = body.to_vec();
                seen_ssid = true;
            }
            RATES | EXT_RATES => {
                if b.rates.len() + body.len() > 64 {
                    return None;
                }
                b.rates.extend_from_slice(body);
            }
            DS_PARAMS if body.len() == 1 => {
                // A beacon heard on an adjacent channel: report the real one.
                let ch = body[0] as u16;
                if freq < 3000 && (1..=13).contains(&ch) && 2407 + 5 * ch != freq {
                    return None;
                }
            }
            TIM if body.len() >= 2 && body[1] != 0 => b.dtim = body[1],
            RSN if b.rsne.is_none() => {
                let mut el = vec![RSN, body.len() as u8];
                el.extend_from_slice(body);
                b.rsne = Some(el);
            }
            HT_CAP => b.ht = true,
            VHT_CAP => b.vht = true,
            EXT if body.first() == Some(&EXT_HE_CAP) => b.he = true,
            VENDOR if is_wmm(body) => b.wmm = true,
            VENDOR if is_wpa1(body) => wpa1 = true,
            _ => {}
        }
    }
    if !seen_ssid || ies.truncated() || b.rates.is_empty() {
        return None;
    }
    b.security = security(cap, b.rsne.as_deref(), wpa1);
    Some(b)
}

/// Legacy rates (500 kb/s units) to aura-proto's 12-bit order:
/// 1, 2, 5.5, 11, 6, 9, 12, 18, 24, 36, 48, 54 Mb/s.
pub fn rate_bits(rates: &[u8], basic_only: bool) -> u16 {
    const ORDER: [u8; 12] = [2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108];
    let mut m = 0u16;
    for &r in rates {
        if basic_only && r & 0x80 == 0 {
            continue;
        }
        if let Some(i) = ORDER.iter().position(|&o| o == r & 0x7f) {
            m |= 1 << i;
        }
    }
    m
}

/// The WMM Parameter element in an association response means QoS.
pub fn has_wmm(ies: &[u8]) -> bool {
    Ies::new(ies).any(|(id, b)| id == VENDOR && b.len() >= 5 && b[..4] == [0x00, 0x50, 0xf2, 2] && b[4] == 1)
}

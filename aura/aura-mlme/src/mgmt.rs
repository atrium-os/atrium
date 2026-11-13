//! Management frames the station sends, and the fixed fields of the ones it
//! receives (IEEE 802.11-2020 §9.3.3).

pub type Mac = [u8; 6];

pub const ASSOC_REQ: u8 = 0;
pub const ASSOC_RESP: u8 = 1;
pub const REASSOC_RESP: u8 = 3;
pub const PROBE_RESP: u8 = 5;
pub const BEACON: u8 = 8;
pub const DISASSOC: u8 = 10;
pub const AUTH: u8 = 11;
pub const DEAUTH: u8 = 12;
pub const ACTION: u8 = 13;

pub const CAT_SA_QUERY: u8 = 8;
pub const SA_QUERY_REQ: u8 = 0;
pub const SA_QUERY_RESP: u8 = 1;

/// The FC Protected bit: aura-mlme sets it on robust frames it wants
/// protected; the radio encrypts them with the PTK.
pub const FC1_PROTECTED: u8 = 0x40;

pub fn subtype(f: &[u8]) -> u8 {
    f[0] >> 4
}
pub fn addr1(f: &[u8]) -> Mac {
    f[4..10].try_into().unwrap_or([0; 6])
}
pub fn addr2(f: &[u8]) -> Mac {
    f[10..16].try_into().unwrap_or([0; 6])
}
pub fn addr3(f: &[u8]) -> Mac {
    f[16..22].try_into().unwrap_or([0; 6])
}
pub fn protected(f: &[u8]) -> bool {
    f[1] & FC1_PROTECTED != 0
}

fn hdr(sub: u8, da: &Mac, sa: &Mac, bssid: &Mac, seq: u16, protect: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(128);
    f.push(sub << 4);
    f.push(if protect { FC1_PROTECTED } else { 0 });
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(da);
    f.extend_from_slice(sa);
    f.extend_from_slice(bssid);
    f.extend_from_slice(&((seq & 0x0fff) << 4).to_le_bytes());
    f
}

/// Open System authentication, transaction 1.
pub fn auth_open(own: &Mac, bssid: &Mac, seq: u16) -> Vec<u8> {
    let mut f = hdr(AUTH, bssid, own, bssid, seq, false);
    f.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    f
}

pub struct AssocReq<'a> {
    pub cap: u16,
    pub listen_interval: u16,
    pub ssid: &'a [u8],
    pub rates: &'a [u8],
    pub rsne: Option<&'a [u8]>,
    pub wmm: bool,
}

pub fn assoc_req(own: &Mac, bssid: &Mac, seq: u16, r: &AssocReq<'_>) -> Vec<u8> {
    let mut f = hdr(ASSOC_REQ, bssid, own, bssid, seq, false);
    f.extend_from_slice(&r.cap.to_le_bytes());
    f.extend_from_slice(&r.listen_interval.to_le_bytes());
    f.push(0);
    f.push(r.ssid.len() as u8);
    f.extend_from_slice(r.ssid);
    let (a, b) = r.rates.split_at(r.rates.len().min(8));
    f.push(1);
    f.push(a.len() as u8);
    f.extend_from_slice(a);
    if !b.is_empty() {
        f.push(50);
        f.push(b.len() as u8);
        f.extend_from_slice(b);
    }
    if let Some(e) = r.rsne {
        f.extend_from_slice(e);
    }
    if r.wmm {
        // WMM Information element, no U-APSD.
        f.extend_from_slice(&[221, 7, 0x00, 0x50, 0xf2, 2, 0, 1, 0]);
    }
    f
}

pub fn deauth(sub: u8, own: &Mac, bssid: &Mac, seq: u16, reason: u16, protect: bool) -> Vec<u8> {
    let mut f = hdr(sub, bssid, own, bssid, seq, protect);
    f.extend_from_slice(&reason.to_le_bytes());
    f
}

pub fn sa_query(own: &Mac, bssid: &Mac, seq: u16, action: u8, trans: [u8; 2]) -> Vec<u8> {
    let mut f = hdr(ACTION, bssid, own, bssid, seq, true);
    f.extend_from_slice(&[CAT_SA_QUERY, action, trans[0], trans[1]]);
    f
}

/// Authentication frame fields: (algorithm, transaction, status).
pub fn parse_auth(f: &[u8]) -> Option<(u16, u16, u16)> {
    if f.len() < 30 || subtype(f) != AUTH {
        return None;
    }
    let u = |i: usize| u16::from_le_bytes([f[i], f[i + 1]]);
    Some((u(24), u(26), u(28)))
}

/// Association response: (status, AID, elements).
pub fn parse_assoc_resp(f: &[u8]) -> Option<(u16, u16, &[u8])> {
    if f.len() < 30 || !matches!(subtype(f), ASSOC_RESP | REASSOC_RESP) {
        return None;
    }
    let status = u16::from_le_bytes([f[26], f[27]]);
    let aid = u16::from_le_bytes([f[28], f[29]]) & 0x3fff;
    Some((status, aid, &f[30..]))
}

/// Deauthentication or disassociation reason code.
pub fn reason(f: &[u8]) -> Option<u16> {
    (f.len() >= 26).then(|| u16::from_le_bytes([f[24], f[25]]))
}

/// SA Query action frames: (action, transaction id).
pub fn parse_sa_query(f: &[u8]) -> Option<(u8, [u8; 2])> {
    if f.len() < 28 || subtype(f) != ACTION || f[24] != CAT_SA_QUERY {
        return None;
    }
    Some((f[25], [f[26], f[27]]))
}

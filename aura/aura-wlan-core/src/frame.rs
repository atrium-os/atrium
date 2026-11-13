//! 802.11 data frames: the fixed-format header, LLC/SNAP, and building the
//! header for transmission. Nothing here looks at management frames or
//! information elements (wifi.md §3.1).

use aura_proto::Mac;

pub const ETHERTYPE_EAPOL: u16 = 0x888e;
/// RFC 1042 SNAP header (`AA AA 03 00 00 00`).
pub const RFC1042: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00];
/// 802.1H bridge-tunnel SNAP header, used for AARP and IPX.
pub const BRIDGE_TUNNEL: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0xf8];
pub const BROADCAST: Mac = [0xff; 6];

pub fn is_group(a: &Mac) -> bool {
    a[0] & 1 != 0
}

/// Why a frame was dropped. Counted per reason by the kmod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    Malformed,
    NotData,
    /// Null-function and other frames without a body.
    NoBody,
    /// ToDS/FromDS not "from the AP", or a four-address frame.
    WrongDirection,
    NotForUs,
    NotFromBss,
    NotAssociated,
    /// Fragmented frames are not reassembled (FragAttacks; §8.5).
    Fragment,
    /// Unprotected frame while a key is installed.
    Unprotected,
    /// Protected frame the hardware did not decrypt, or with no key.
    NotDecrypted,
    Replay,
    /// An A-MSDU whose first subframe looks like an LLC/SNAP header
    /// (FragAttacks A-MSDU injection), or with an invalid subframe.
    AmsduInjection,
    /// Source address is our own (reflection).
    FromSelf,
    /// Neither RFC 1042 nor bridge-tunnel encapsulation.
    UnknownEncap,
    /// Controlled port closed and not EAPOL.
    PortClosed,
    /// EAPOL from somewhere other than the AP, or to a group address.
    BadEapol,
}

/// The parsed fixed header of a data frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataHdr {
    pub to_ds: bool,
    pub from_ds: bool,
    pub protected: bool,
    pub more_frag: bool,
    pub frag: u8,
    pub seq: u16,
    pub qos: bool,
    pub tid: u8,
    pub amsdu: bool,
    pub has_body: bool,
    pub addr1: Mac,
    pub addr2: Mac,
    pub addr3: Mac,
    pub four_addr: bool,
    pub len: usize,
}

fn mac(b: &[u8]) -> Mac {
    let mut m = [0; 6];
    m.copy_from_slice(&b[..6]);
    m
}

impl DataHdr {
    pub fn parse(f: &[u8]) -> Result<DataHdr, Drop> {
        if f.len() < 24 {
            return Err(Drop::Malformed);
        }
        let (fc0, fc1) = (f[0], f[1]);
        if fc0 & 0x03 != 0 {
            return Err(Drop::Malformed);
        }
        if (fc0 >> 2) & 0x03 != 2 {
            return Err(Drop::NotData);
        }
        let subtype = fc0 >> 4;
        let qos = subtype & 0x8 != 0;
        // Subtypes 4 (Null) and 12 (QoS Null) carry no body; the CF ones
        // (1-3, 5-7, 9-11, 13-15) are not used by stations.
        let has_body = subtype == 0 || subtype == 8;
        let to_ds = fc1 & 0x01 != 0;
        let from_ds = fc1 & 0x02 != 0;
        let four_addr = to_ds && from_ds;
        let order = fc1 & 0x80 != 0;
        let mut len = 24;
        if four_addr {
            len += 6;
        }
        let (mut tid, mut amsdu) = (0, false);
        if qos {
            if f.len() < len + 2 {
                return Err(Drop::Malformed);
            }
            tid = f[len] & 0x0f;
            amsdu = f[len] & 0x80 != 0;
            len += 2;
            if order {
                len += 4; // HT Control
            }
        }
        if f.len() < len || tid > 7 {
            return Err(Drop::Malformed);
        }
        let sc = u16::from_le_bytes([f[22], f[23]]);
        Ok(DataHdr {
            to_ds,
            from_ds,
            protected: fc1 & 0x40 != 0,
            more_frag: fc1 & 0x04 != 0,
            frag: (sc & 0x0f) as u8,
            seq: sc >> 4,
            qos,
            tid,
            amsdu,
            has_body,
            addr1: mac(&f[4..]),
            addr2: mac(&f[10..]),
            addr3: mac(&f[16..]),
            four_addr,
            len,
        })
    }
}

/// Splits an LLC/SNAP-encapsulated MSDU into ethertype and payload.
pub fn decap(msdu: &[u8]) -> Result<(u16, &[u8]), Drop> {
    if msdu.len() < 8 {
        return Err(Drop::UnknownEncap);
    }
    let et = u16::from_be_bytes([msdu[6], msdu[7]]);
    let snap = &msdu[..6];
    // 802.1H: AARP and IPX travel in bridge-tunnel; every other ethertype in
    // RFC 1042. Accept each only for its own ethertypes, so one encapsulation
    // cannot be passed off as the other.
    let bt = matches!(et, 0x80f3 | 0x8137);
    if (snap == RFC1042 && !bt) || (snap == BRIDGE_TUNNEL && bt) {
        if et < 0x0600 {
            return Err(Drop::UnknownEncap);
        }
        Ok((et, &msdu[8..]))
    } else {
        Err(Drop::UnknownEncap)
    }
}

/// The 802.11 header plus LLC/SNAP the kmod prepends to an outgoing MSDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxHdr {
    pub bytes: [u8; 34],
    pub len: usize,
    pub tid: u8,
    /// The hardware must encrypt this frame with the pairwise key.
    pub encrypt: bool,
}

impl TxHdr {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// QoS data, ToDS: addr1 = BSSID, addr2 = us, addr3 = destination. Sequence
/// numbers are assigned by the hardware.
pub fn build_tx(bssid: &Mac, own: &Mac, dst: &Mac, ethertype: u16, tid: u8, qos: bool, encrypt: bool) -> TxHdr {
    let mut b = [0u8; 34];
    b[0] = if qos { 0x88 } else { 0x08 };
    b[1] = 0x01 | if encrypt { 0x40 } else { 0 };
    b[4..10].copy_from_slice(bssid);
    b[10..16].copy_from_slice(own);
    b[16..22].copy_from_slice(dst);
    let mut n = 24;
    if qos {
        b[24] = tid & 7;
        n = 26;
    }
    let snap = if matches!(ethertype, 0x80f3 | 0x8137) { BRIDGE_TUNNEL } else { RFC1042 };
    b[n..n + 6].copy_from_slice(&snap);
    b[n + 6..n + 8].copy_from_slice(&ethertype.to_be_bytes());
    TxHdr { bytes: b, len: n + 8, tid: tid & 7, encrypt }
}

/// 802.1D priority (0-7) to TID; EAPOL goes as voice (TID 6/7), like other
/// stacks, so handshakes are not stuck behind bulk traffic.
pub fn tid_for(ethertype: u16, prio: u8) -> u8 {
    if ethertype == ETHERTYPE_EAPOL {
        7
    } else {
        prio & 7
    }
}

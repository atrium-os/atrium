//! Aura control-channel protocol (wifi.md §3.2).
//!
//! The messages that cross the kernel boundary of the Wi-Fi stack, on two
//! character devices per radio:
//!
//! - `/dev/aura/ctlN`, opened by `aura-mlme`: scans, BSS setup, management
//!   frames in both directions, regulatory, power save.
//! - `/dev/aura/keyN`, opened by `aura-keys`: keys, the controlled port, and
//!   EAPOL frames in both directions. `aura-mlme` never sees EAPOL or key
//!   material (wifi.md §8.3).
//!
//! One `read`/`write` carries exactly one message: a 16-byte header and a
//! body of `len` bytes. Everything is little-endian and fixed-layout. Lists
//! are counted arrays of fixed-size items. Frame bodies (802.11 management
//! frames, EAPOL) are passed as opaque bytes: only their minimal framing is
//! checked here, so the kernel never parses an information element.
//!
//! The crate is `no_std`, allocation-free and has no `unsafe`, so the kernel
//! core links the same decoder the daemons use. [`decode`] is total: any
//! input gives a message or an [`Error`], never a panic (fuzzed).

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod wire;

pub use wire::{Rd, Wr};

/// `"AURA"` read as a little-endian u32.
pub const MAGIC: u32 = u32::from_le_bytes(*b"AURA");
/// Protocol version. Bumped on any incompatible change; both ends must match.
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 16;
/// Largest body of any message.
pub const MAX_BODY: usize = 8192;
/// Largest 802.11 management frame carried (header + body, no FCS).
pub const MAX_MGMT_FRAME: usize = 4096;
/// Smallest management frame: the 24-byte management header.
pub const MIN_MGMT_FRAME: usize = 24;
/// Largest EAPOL frame body carried (EAP-TLS fragments stay below this).
pub const MAX_EAPOL: usize = 2048;
/// Smallest EAPOL frame: version, type, body length.
pub const MIN_EAPOL: usize = 4;
pub const MAX_SCAN_SSIDS: usize = 8;
pub const MAX_SCAN_FREQS: usize = 128;
pub const MAX_REG_RULES: usize = 32;
pub const MAX_CHANNELS: usize = 128;
pub const SSID_MAX: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than the header or the body needs.
    Short,
    BadMagic,
    BadVersion(u16),
    UnknownKind(u16),
    /// Header `len` disagrees with the buffer, or exceeds [`MAX_BODY`].
    BadLength,
    /// Bytes left over after the body was decoded.
    Trailing,
    /// A field holds a value outside its range.
    BadValue,
    /// The message kind may not travel in this direction.
    NotPermitted,
    /// The output buffer is too small to encode into.
    BufTooSmall,
}

/// Which way a message travels. The kernel decodes with `MlmeToKernel` on
/// `ctlN` and `KeysToKernel` on `keyN`; each daemon decodes with the reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    MlmeToKernel,
    KeysToKernel,
    KernelToMlme,
    KernelToKeys,
}

/// Message kinds on the wire.
pub mod kind {
    // aura-mlme -> kernel (ctlN)
    pub const GET_CAPS: u16 = 0x0001;
    pub const SCAN: u16 = 0x0002;
    pub const SET_BSS: u16 = 0x0003;
    pub const CLEAR_BSS: u16 = 0x0004;
    pub const TX_MGMT: u16 = 0x0005;
    pub const SET_POWER_SAVE: u16 = 0x0006;
    pub const SET_REGULATORY: u16 = 0x0007;
    pub const SET_MAC: u16 = 0x0008;
    pub const DISCONNECT: u16 = 0x0009;
    // aura-keys -> kernel (keyN)
    pub const SET_KEY: u16 = 0x0101;
    pub const DEL_KEY: u16 = 0x0102;
    pub const PORT_AUTHORIZED: u16 = 0x0103;
    pub const TX_EAPOL: u16 = 0x0104;
    // kernel -> daemons
    pub const ACK: u16 = 0x8001;
    pub const CAPS: u16 = 0x8002;
    pub const MGMT_RX: u16 = 0x8003;
    pub const EAPOL_RX: u16 = 0x8004;
    pub const SCAN_DONE: u16 = 0x8005;
    pub const DISASSOCIATED: u16 = 0x8006;
    pub const BEACON_LOSS: u16 = 0x8007;
    pub const RF_KILL: u16 = 0x8008;
    pub const FIRMWARE_ERROR: u16 = 0x8009;
    pub const STATS: u16 = 0x800a;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: u16,
    /// Request id chosen by the sender of a request; echoed in its [`Msg::Ack`]
    /// or [`Msg::Caps`]. 0 on unsolicited events.
    pub seq: u32,
    pub len: u32,
}

impl Header {
    pub fn decode(buf: &[u8]) -> Result<Header, Error> {
        let mut r = Rd::new(buf);
        if r.u32()? != MAGIC {
            return Err(Error::BadMagic);
        }
        let v = r.u16()?;
        if v != VERSION {
            return Err(Error::BadVersion(v));
        }
        let kind = r.u16()?;
        let seq = r.u32()?;
        let len = r.u32()?;
        if len as usize > MAX_BODY {
            return Err(Error::BadLength);
        }
        Ok(Header { kind, seq, len })
    }

    fn encode(&self, w: &mut Wr<'_>) -> Result<(), Error> {
        w.u32(MAGIC)?;
        w.u16(VERSION)?;
        w.u16(self.kind)?;
        w.u32(self.seq)?;
        w.u32(self.len)
    }
}

pub type Mac = [u8; 6];

// ---------------------------------------------------------------- lists

/// A fixed-size item of a counted list.
pub trait Item: Sized {
    const SIZE: usize;
    fn read(r: &mut Rd<'_>) -> Result<Self, Error>;
    fn write(&self, w: &mut Wr<'_>) -> Result<(), Error>;
}

/// A validated counted list borrowed from a message buffer: on the wire a
/// `u16` count followed by `count * T::SIZE` bytes. Every item was checked
/// when the list was decoded, so iteration cannot fail.
#[derive(Clone, Copy)]
pub struct List<'a, T: Item> {
    raw: &'a [u8],
    _t: core::marker::PhantomData<T>,
}

impl<'a, T: Item> List<'a, T> {
    pub fn len(&self) -> usize {
        self.raw.len() / T::SIZE
    }
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = T> + 'a {
        let raw = self.raw;
        raw.chunks_exact(T::SIZE).map(|c| {
            let mut r = Rd::new(c);
            // Checked in `read_list`.
            T::read(&mut r).unwrap_or_else(|_| unreachable!())
        })
    }
    /// Encodes `items` as a list into `out` and returns it borrowed from
    /// `out`, for building messages.
    pub fn build(items: &[T], out: &'a mut [u8]) -> Result<List<'a, T>, Error> {
        let n = items.len() * T::SIZE;
        if out.len() < n {
            return Err(Error::BufTooSmall);
        }
        let mut w = Wr::new(&mut out[..n]);
        for it in items {
            it.write(&mut w)?;
        }
        Ok(List { raw: &out[..n], _t: core::marker::PhantomData })
    }
}

impl<'a, T: Item + core::fmt::Debug> core::fmt::Debug for List<'a, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a, T: Item + PartialEq> PartialEq for List<'a, T> {
    fn eq(&self, o: &Self) -> bool {
        self.raw == o.raw
    }
}

fn read_list<'a, T: Item>(r: &mut Rd<'a>, max: usize) -> Result<List<'a, T>, Error> {
    let n = r.u16()? as usize;
    if n > max {
        return Err(Error::BadValue);
    }
    let raw = r.bytes(n * T::SIZE)?;
    let mut c = Rd::new(raw);
    for _ in 0..n {
        T::read(&mut c)?;
    }
    Ok(List { raw, _t: core::marker::PhantomData })
}

fn write_list<T: Item>(w: &mut Wr<'_>, l: &List<'_, T>, max: usize) -> Result<(), Error> {
    let n = l.len();
    if n > max {
        return Err(Error::BadValue);
    }
    w.u16(n as u16)?;
    w.bytes(l.raw)
}

/// A channel centre frequency in MHz (2.4, 5 and 6 GHz bands).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Freq(pub u16);

pub fn freq_valid(mhz: u16) -> bool {
    matches!(mhz, 2412..=2484 | 5150..=5895 | 5955..=7115)
}

impl Item for Freq {
    const SIZE: usize = 2;
    fn read(r: &mut Rd<'_>) -> Result<Self, Error> {
        let f = r.u16()?;
        if !freq_valid(f) {
            return Err(Error::BadValue);
        }
        Ok(Freq(f))
    }
    fn write(&self, w: &mut Wr<'_>) -> Result<(), Error> {
        w.u16(self.0)
    }
}

/// An SSID for a directed probe: length byte, then 32 bytes (zero padded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ssid {
    pub len: u8,
    pub bytes: [u8; SSID_MAX],
}

impl Ssid {
    pub fn new(s: &[u8]) -> Result<Ssid, Error> {
        if s.len() > SSID_MAX {
            return Err(Error::BadValue);
        }
        let mut bytes = [0u8; SSID_MAX];
        bytes[..s.len()].copy_from_slice(s);
        Ok(Ssid { len: s.len() as u8, bytes })
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl Item for Ssid {
    const SIZE: usize = 1 + SSID_MAX;
    fn read(r: &mut Rd<'_>) -> Result<Self, Error> {
        let len = r.u8()?;
        let bytes = r.array::<SSID_MAX>()?;
        if len as usize > SSID_MAX || bytes[len as usize..].iter().any(|&b| b != 0) {
            return Err(Error::BadValue);
        }
        Ok(Ssid { len, bytes })
    }
    fn write(&self, w: &mut Wr<'_>) -> Result<(), Error> {
        w.u8(self.len)?;
        w.bytes(&self.bytes)
    }
}

/// One regulatory rule (wireless-regdb shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegRule {
    pub start_khz: u32,
    pub end_khz: u32,
    pub max_bw_khz: u32,
    /// Maximum EIRP in mBm (dBm * 100).
    pub max_eirp_mbm: i32,
    pub flags: u32,
}

pub mod reg_flags {
    pub const NO_IR: u32 = 1 << 0;
    pub const DFS: u32 = 1 << 1;
    pub const INDOOR_ONLY: u32 = 1 << 2;
    pub const NO_OUTDOOR: u32 = 1 << 3;
    pub const ALL: u32 = NO_IR | DFS | INDOOR_ONLY | NO_OUTDOOR;
}

impl Item for RegRule {
    const SIZE: usize = 20;
    fn read(r: &mut Rd<'_>) -> Result<Self, Error> {
        let v = RegRule {
            start_khz: r.u32()?,
            end_khz: r.u32()?,
            max_bw_khz: r.u32()?,
            max_eirp_mbm: r.i32()?,
            flags: r.u32()?,
        };
        if v.start_khz >= v.end_khz
            || v.max_bw_khz == 0
            || v.max_bw_khz > v.end_khz - v.start_khz
            || !(0..=3600).contains(&v.max_eirp_mbm)
            || v.flags & !reg_flags::ALL != 0
        {
            return Err(Error::BadValue);
        }
        Ok(v)
    }
    fn write(&self, w: &mut Wr<'_>) -> Result<(), Error> {
        w.u32(self.start_khz)?;
        w.u32(self.end_khz)?;
        w.u32(self.max_bw_khz)?;
        w.i32(self.max_eirp_mbm)?;
        w.u32(self.flags)
    }
}

/// A channel the radio supports, as reported in [`Caps`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChanInfo {
    pub freq: u16,
    pub flags: u16,
    pub max_power_mbm: i16,
}

pub mod chan_flags {
    pub const DISABLED: u16 = 1 << 0;
    pub const NO_IR: u16 = 1 << 1;
    pub const DFS: u16 = 1 << 2;
    pub const ALL: u16 = DISABLED | NO_IR | DFS;
}

impl Item for ChanInfo {
    const SIZE: usize = 6;
    fn read(r: &mut Rd<'_>) -> Result<Self, Error> {
        let v = ChanInfo { freq: r.u16()?, flags: r.u16()?, max_power_mbm: r.i16()? };
        if !freq_valid(v.freq) || v.flags & !chan_flags::ALL != 0 {
            return Err(Error::BadValue);
        }
        Ok(v)
    }
    fn write(&self, w: &mut Wr<'_>) -> Result<(), Error> {
        w.u16(self.freq)?;
        w.u16(self.flags)?;
        w.i16(self.max_power_mbm)
    }
}

// ---------------------------------------------------------------- bodies

pub mod scan_flags {
    /// Passive only: never transmit probe requests.
    pub const PASSIVE: u16 = 1 << 0;
    /// Use `ScanReq::mac` as the source address of probes.
    pub const RANDOM_MAC: u16 = 1 << 1;
    pub const ALL: u16 = PASSIVE | RANDOM_MAC;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanReq<'a> {
    pub flags: u16,
    pub mac: Mac,
    /// Dwell per channel; 0 = driver default.
    pub dwell_ms: u16,
    /// Directed probes. Empty = wildcard SSID only (wifi.md §3.4: directed
    /// probes only for networks the user marked hidden).
    pub ssids: List<'a, Ssid>,
    /// Empty = every enabled channel.
    pub freqs: List<'a, Freq>,
}

/// Channel width of the BSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Width {
    W20 = 0,
    W40 = 1,
    W80 = 2,
    W160 = 3,
    W320 = 4,
}

impl Width {
    fn from_u8(v: u8) -> Result<Width, Error> {
        Ok(match v {
            0 => Width::W20,
            1 => Width::W40,
            2 => Width::W80,
            3 => Width::W160,
            4 => Width::W320,
            _ => return Err(Error::BadValue),
        })
    }
}

pub mod bss_flags {
    pub const QOS: u16 = 1 << 0;
    pub const HT: u16 = 1 << 1;
    pub const VHT: u16 = 1 << 2;
    pub const HE: u16 = 1 << 3;
    pub const EHT: u16 = 1 << 4;
    pub const SHORT_PREAMBLE: u16 = 1 << 5;
    pub const SHORT_SLOT: u16 = 1 << 6;
    /// Management frame protection negotiated.
    pub const PMF: u16 = 1 << 7;
    /// Signalling and payload protected A-MSDU negotiated (FragAttacks).
    pub const SPP_AMSDU: u16 = 1 << 8;
    pub const ALL: u16 = 0x01ff;
}

/// The associated BSS, as `aura-mlme` parsed it from the AP's frames. The
/// kernel takes these as fixed fields and never sees the elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bss {
    pub bssid: Mac,
    pub freq: u16,
    pub width: Width,
    /// Centre of the whole channel (0 for 20 MHz).
    pub center1: u16,
    /// Centre of the second 80 MHz segment (80+80 only), else 0.
    pub center2: u16,
    pub aid: u16,
    pub beacon_int: u16,
    pub dtim_period: u8,
    pub flags: u16,
    /// Basic rate set, bit i = i-th of 1, 2, 5.5, 11, 6, 9, 12, 18, 24, 36, 48, 54 Mb/s.
    pub basic_rates: u16,
    /// Supported legacy rates, same bit order.
    pub rates: u16,
    /// HT RX MCS bitmap (first 77 bits used).
    pub ht_mcs: [u8; 10],
    pub vht_rx_mcs: u16,
    pub vht_tx_mcs: u16,
    /// HE MCS maps: rx/tx for <=80, 160, 80+80.
    pub he_mcs: [u16; 6],
    pub ampdu_factor: u8,
    pub ampdu_density: u8,
}

/// The highest AID 802.11-2020 allows.
pub const AID_MAX: u16 = 2007;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyKind {
    Pairwise = 0,
    Group = 1,
    /// Integrity group key (PMF, BIP).
    Igtk = 2,
    /// Beacon protection key.
    Bigtk = 3,
}

impl KeyKind {
    fn from_u8(v: u8) -> Result<KeyKind, Error> {
        Ok(match v {
            0 => KeyKind::Pairwise,
            1 => KeyKind::Group,
            2 => KeyKind::Igtk,
            3 => KeyKind::Bigtk,
            _ => return Err(Error::BadValue),
        })
    }
    /// Key index ranges from 802.11-2020 §12.7.
    fn idx_ok(self, idx: u8) -> bool {
        match self {
            KeyKind::Pairwise => idx <= 1,
            KeyKind::Group => (1..=3).contains(&idx),
            KeyKind::Igtk => (4..=5).contains(&idx),
            KeyKind::Bigtk => (6..=7).contains(&idx),
        }
    }
}

/// Cipher suite types (00-0F-AC:n, 802.11-2020 Table 9-149). WEP and TKIP
/// are deliberately absent (wifi.md decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cipher {
    Ccmp128 = 4,
    BipCmac128 = 6,
    Gcmp128 = 8,
    Gcmp256 = 9,
    Ccmp256 = 10,
    BipGmac128 = 11,
    BipGmac256 = 12,
    BipCmac256 = 13,
}

impl Cipher {
    fn from_u8(v: u8) -> Result<Cipher, Error> {
        Ok(match v {
            4 => Cipher::Ccmp128,
            6 => Cipher::BipCmac128,
            8 => Cipher::Gcmp128,
            9 => Cipher::Gcmp256,
            10 => Cipher::Ccmp256,
            11 => Cipher::BipGmac128,
            12 => Cipher::BipGmac256,
            13 => Cipher::BipCmac256,
            _ => return Err(Error::BadValue),
        })
    }
    pub fn key_len(self) -> usize {
        match self {
            Cipher::Ccmp128 | Cipher::Gcmp128 | Cipher::BipCmac128 | Cipher::BipGmac128 => 16,
            _ => 32,
        }
    }
    pub fn is_integrity(self) -> bool {
        matches!(
            self,
            Cipher::BipCmac128 | Cipher::BipGmac128 | Cipher::BipGmac256 | Cipher::BipCmac256
        )
    }
    /// Bit of this cipher in [`Caps::ciphers`].
    pub fn bit(self) -> u32 {
        1 << (self as u8)
    }
}

/// A temporal key for the hardware. Its `Debug` never prints the key, and
/// the bytes are cleared on drop (best effort without `unsafe`: the kernel
/// clears its own copy with `explicit_bzero`).
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    pub kind: KeyKind,
    pub idx: u8,
    pub cipher: Cipher,
    /// Peer address (the AP) for pairwise keys; broadcast for group keys.
    pub addr: Mac,
    /// Receive sequence counter to start from (48-bit, little-endian).
    pub rsc: [u8; 6],
    pub key: [u8; 32],
}

impl Key {
    pub fn material(&self) -> &[u8] {
        &self.key[..self.cipher.key_len()]
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.key = [0; 32];
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Key")
            .field("kind", &self.kind)
            .field("idx", &self.idx)
            .field("cipher", &self.cipher)
            .field("addr", &self.addr)
            .field("rsc", &self.rsc)
            .finish_non_exhaustive()
    }
}

pub mod rx_flags {
    /// Arrived protected and the hardware decrypted it.
    pub const DECRYPTED: u8 = 1 << 0;
    /// Integrity check (BIP or CCMP/GCMP MIC) passed.
    pub const MIC_OK: u8 = 1 << 1;
    /// Part of a scan report rather than received while associated.
    pub const SCAN: u8 = 1 << 2;
    pub const ALL: u8 = DECRYPTED | MIC_OK | SCAN;
}

pub mod caps_flags {
    pub const PMF: u32 = 1 << 0;
    pub const HW_SCAN: u32 = 1 << 1;
    pub const RANDOM_MAC_SCAN: u32 = 1 << 2;
    pub const SPP_AMSDU: u32 = 1 << 3;
    pub const BEACON_PROTECTION: u32 = 1 << 4;
}

pub mod bands {
    pub const B2G: u8 = 1 << 0;
    pub const B5G: u8 = 1 << 1;
    pub const B6G: u8 = 1 << 2;
    pub const ALL: u8 = B2G | B5G | B6G;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Caps<'a> {
    pub mac: Mac,
    pub bands: u8,
    pub ciphers: u32,
    pub flags: u32,
    pub max_scan_ssids: u8,
    pub channels: List<'a, ChanInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub tx_failed: u64,
    pub rx_dropped: u64,
    /// Smoothed beacon RSSI in dBm.
    pub beacon_rssi: i8,
    pub tx_rate_kbps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerSave {
    Off = 0,
    On = 1,
}

// ---------------------------------------------------------------- messages

#[derive(Clone, Debug, PartialEq)]
pub enum Msg<'a> {
    // aura-mlme -> kernel
    GetCaps,
    Scan(ScanReq<'a>),
    SetBss(Bss),
    ClearBss,
    /// A management frame to send. The kernel checks the subtype against
    /// [`mgmt_tx_allowed`] and the channel against its regulatory list.
    TxMgmt { freq: u16, frame: &'a [u8] },
    SetPowerSave(PowerSave),
    SetRegulatory { alpha2: [u8; 2], rules: List<'a, RegRule> },
    SetMac(Mac),
    Disconnect { reason: u16 },
    // aura-keys -> kernel
    SetKey(Key),
    DelKey { kind: KeyKind, idx: u8, addr: Mac },
    /// Open (or close) the 802.1X controlled port: until it is open the
    /// kernel passes only EAPOL.
    PortAuthorized(bool),
    TxEapol { dst: Mac, body: &'a [u8] },
    // kernel -> daemons
    Ack { status: i32 },
    Caps(Caps<'a>),
    MgmtRx { freq: u16, rssi: i8, flags: u8, tsf_us: u64, frame: &'a [u8] },
    EapolRx { src: Mac, body: &'a [u8] },
    ScanDone { status: i32 },
    Disassociated { reason: u16, local: bool },
    BeaconLoss,
    RfKill { blocked: bool },
    FirmwareError { code: u32 },
    Stats(Stats),
}

impl<'a> Msg<'a> {
    pub fn kind(&self) -> u16 {
        use kind::*;
        match self {
            Msg::GetCaps => GET_CAPS,
            Msg::Scan(_) => SCAN,
            Msg::SetBss(_) => SET_BSS,
            Msg::ClearBss => CLEAR_BSS,
            Msg::TxMgmt { .. } => TX_MGMT,
            Msg::SetPowerSave(_) => SET_POWER_SAVE,
            Msg::SetRegulatory { .. } => SET_REGULATORY,
            Msg::SetMac(_) => SET_MAC,
            Msg::Disconnect { .. } => DISCONNECT,
            Msg::SetKey(_) => SET_KEY,
            Msg::DelKey { .. } => DEL_KEY,
            Msg::PortAuthorized(_) => PORT_AUTHORIZED,
            Msg::TxEapol { .. } => TX_EAPOL,
            Msg::Ack { .. } => ACK,
            Msg::Caps(_) => CAPS,
            Msg::MgmtRx { .. } => MGMT_RX,
            Msg::EapolRx { .. } => EAPOL_RX,
            Msg::ScanDone { .. } => SCAN_DONE,
            Msg::Disassociated { .. } => DISASSOCIATED,
            Msg::BeaconLoss => BEACON_LOSS,
            Msg::RfKill { .. } => RF_KILL,
            Msg::FirmwareError { .. } => FIRMWARE_ERROR,
            Msg::Stats(_) => STATS,
        }
    }
}

/// Whether a message kind may travel in `dir`. This is the channel split of
/// wifi.md §8.3: key material and EAPOL only on `keyN`, frames and scans only
/// on `ctlN`.
pub fn permitted(kind: u16, dir: Direction) -> bool {
    use kind::*;
    match dir {
        Direction::MlmeToKernel => matches!(
            kind,
            GET_CAPS
                | SCAN
                | SET_BSS
                | CLEAR_BSS
                | TX_MGMT
                | SET_POWER_SAVE
                | SET_REGULATORY
                | SET_MAC
                | DISCONNECT
        ),
        Direction::KeysToKernel => matches!(kind, SET_KEY | DEL_KEY | PORT_AUTHORIZED | TX_EAPOL),
        Direction::KernelToMlme => matches!(
            kind,
            ACK | CAPS
                | MGMT_RX
                | SCAN_DONE
                | DISASSOCIATED
                | BEACON_LOSS
                | RF_KILL
                | FIRMWARE_ERROR
                | STATS
        ),
        Direction::KernelToKeys => matches!(kind, ACK | EAPOL_RX | DISASSOCIATED),
    }
}

/// Management subtypes (frame control bits 4..7) a station may transmit:
/// association/reassociation request, probe request, disassociation,
/// authentication, deauthentication, action, action no-ack. Beacons, probe
/// responses and the AP-side responses are refused, so a compromised
/// `aura-mlme` cannot act as an AP.
pub fn mgmt_tx_allowed(subtype: u8) -> bool {
    matches!(subtype, 0 | 2 | 4 | 10 | 11 | 12 | 13 | 14)
}

/// Minimal framing check of an 802.11 management frame: length bounds and
/// frame control type = management, protocol version 0. Elements are not
/// looked at. Returns the subtype.
pub fn check_mgmt(frame: &[u8]) -> Result<u8, Error> {
    if frame.len() < MIN_MGMT_FRAME || frame.len() > MAX_MGMT_FRAME {
        return Err(Error::BadValue);
    }
    let fc0 = frame[0];
    if fc0 & 0x03 != 0 || (fc0 >> 2) & 0x03 != 0 {
        return Err(Error::BadValue);
    }
    Ok(fc0 >> 4)
}

fn check_eapol(body: &[u8]) -> Result<(), Error> {
    if body.len() < MIN_EAPOL || body.len() > MAX_EAPOL {
        return Err(Error::BadValue);
    }
    Ok(())
}

fn check_alpha2(a: [u8; 2]) -> Result<(), Error> {
    if a == *b"00" || (a[0].is_ascii_uppercase() && a[1].is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(Error::BadValue)
    }
}

fn check_bss(b: &Bss) -> Result<(), Error> {
    let ok = freq_valid(b.freq)
        && (b.width == Width::W20 || b.center1 != 0)
        && (b.center1 == 0 || freq_valid(b.center1))
        && (b.center2 == 0 || freq_valid(b.center2))
        && (1..=AID_MAX).contains(&b.aid)
        && b.beacon_int != 0
        && b.dtim_period != 0
        && b.flags & !bss_flags::ALL == 0
        && b.basic_rates & !0x0fff == 0
        && b.rates & !0x0fff == 0
        && b.basic_rates & !b.rates == 0
        && b.ampdu_factor <= 7
        && b.ampdu_density <= 7
        && b.bssid[0] & 1 == 0;
    if ok {
        Ok(())
    } else {
        Err(Error::BadValue)
    }
}

fn check_key(k: &Key) -> Result<(), Error> {
    let integrity = matches!(k.kind, KeyKind::Igtk | KeyKind::Bigtk);
    if !k.kind.idx_ok(k.idx)
        || integrity != k.cipher.is_integrity()
        || k.key[k.cipher.key_len()..].iter().any(|&b| b != 0)
        || (k.kind == KeyKind::Pairwise && k.addr[0] & 1 != 0)
    {
        return Err(Error::BadValue);
    }
    Ok(())
}

fn bool_u8(v: u8) -> Result<bool, Error> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::BadValue),
    }
}

/// Decodes exactly one message occupying all of `buf`, refusing kinds not
/// [`permitted`] in `dir`.
pub fn decode(buf: &[u8], dir: Direction) -> Result<(Header, Msg<'_>), Error> {
    let h = Header::decode(buf)?;
    if buf.len() != HEADER_LEN + h.len as usize {
        return Err(if buf.len() < HEADER_LEN + h.len as usize { Error::Short } else { Error::BadLength });
    }
    if !permitted(h.kind, dir) {
        return Err(match h.kind {
            k if is_known(k) => Error::NotPermitted,
            k => Error::UnknownKind(k),
        });
    }
    let mut r = Rd::new(&buf[HEADER_LEN..]);
    let m = decode_body(h.kind, &mut r)?;
    if !r.is_empty() {
        return Err(Error::Trailing);
    }
    Ok((h, m))
}

fn is_known(k: u16) -> bool {
    [
        Direction::MlmeToKernel,
        Direction::KeysToKernel,
        Direction::KernelToMlme,
        Direction::KernelToKeys,
    ]
    .iter()
    .any(|&d| permitted(k, d))
}

fn decode_body<'a>(k: u16, r: &mut Rd<'a>) -> Result<Msg<'a>, Error> {
    use kind::*;
    Ok(match k {
        GET_CAPS => Msg::GetCaps,
        SCAN => {
            let flags = r.u16()?;
            let mac = r.array()?;
            let dwell_ms = r.u16()?;
            let ssids = read_list(r, MAX_SCAN_SSIDS)?;
            let freqs = read_list(r, MAX_SCAN_FREQS)?;
            if flags & !scan_flags::ALL != 0
                || (flags & scan_flags::PASSIVE != 0 && !ssids.is_empty())
                || (flags & scan_flags::RANDOM_MAC != 0 && mac[0] & 1 != 0)
            {
                return Err(Error::BadValue);
            }
            Msg::Scan(ScanReq { flags, mac, dwell_ms, ssids, freqs })
        }
        SET_BSS => {
            let b = Bss {
                bssid: r.array()?,
                freq: r.u16()?,
                width: Width::from_u8(r.u8()?)?,
                center1: r.u16()?,
                center2: r.u16()?,
                aid: r.u16()?,
                beacon_int: r.u16()?,
                dtim_period: r.u8()?,
                flags: r.u16()?,
                basic_rates: r.u16()?,
                rates: r.u16()?,
                ht_mcs: r.array()?,
                vht_rx_mcs: r.u16()?,
                vht_tx_mcs: r.u16()?,
                he_mcs: {
                    let mut m = [0u16; 6];
                    for x in m.iter_mut() {
                        *x = r.u16()?;
                    }
                    m
                },
                ampdu_factor: r.u8()?,
                ampdu_density: r.u8()?,
            };
            check_bss(&b)?;
            Msg::SetBss(b)
        }
        CLEAR_BSS => Msg::ClearBss,
        TX_MGMT => {
            let freq = r.u16()?;
            let frame = r.rest();
            if !freq_valid(freq) || !mgmt_tx_allowed(check_mgmt(frame)?) {
                return Err(Error::BadValue);
            }
            Msg::TxMgmt { freq, frame }
        }
        SET_POWER_SAVE => Msg::SetPowerSave(match r.u8()? {
            0 => PowerSave::Off,
            1 => PowerSave::On,
            _ => return Err(Error::BadValue),
        }),
        SET_REGULATORY => {
            let alpha2 = r.array()?;
            check_alpha2(alpha2)?;
            Msg::SetRegulatory { alpha2, rules: read_list(r, MAX_REG_RULES)? }
        }
        SET_MAC => {
            let m: Mac = r.array()?;
            if m[0] & 1 != 0 || m == [0; 6] {
                return Err(Error::BadValue);
            }
            Msg::SetMac(m)
        }
        DISCONNECT => Msg::Disconnect { reason: r.u16()? },
        SET_KEY => {
            let k = Key {
                kind: KeyKind::from_u8(r.u8()?)?,
                idx: r.u8()?,
                cipher: Cipher::from_u8(r.u8()?)?,
                addr: r.array()?,
                rsc: r.array()?,
                key: r.array()?,
            };
            check_key(&k)?;
            Msg::SetKey(k)
        }
        DEL_KEY => {
            let kind = KeyKind::from_u8(r.u8()?)?;
            let idx = r.u8()?;
            let addr = r.array()?;
            if !kind.idx_ok(idx) {
                return Err(Error::BadValue);
            }
            Msg::DelKey { kind, idx, addr }
        }
        PORT_AUTHORIZED => Msg::PortAuthorized(bool_u8(r.u8()?)?),
        TX_EAPOL => {
            let dst = r.array()?;
            let body = r.rest();
            check_eapol(body)?;
            Msg::TxEapol { dst, body }
        }
        ACK => Msg::Ack { status: r.i32()? },
        CAPS => {
            let c = Caps {
                mac: r.array()?,
                bands: r.u8()?,
                ciphers: r.u32()?,
                flags: r.u32()?,
                max_scan_ssids: r.u8()?,
                channels: read_list(r, MAX_CHANNELS)?,
            };
            if c.bands & !bands::ALL != 0 || c.max_scan_ssids as usize > MAX_SCAN_SSIDS {
                return Err(Error::BadValue);
            }
            Msg::Caps(c)
        }
        MGMT_RX => {
            let freq = r.u16()?;
            let rssi = r.u8()? as i8;
            let flags = r.u8()?;
            let tsf_us = r.u64()?;
            let frame = r.rest();
            if !freq_valid(freq) || flags & !rx_flags::ALL != 0 {
                return Err(Error::BadValue);
            }
            check_mgmt(frame)?;
            Msg::MgmtRx { freq, rssi, flags, tsf_us, frame }
        }
        EAPOL_RX => {
            let src = r.array()?;
            let body = r.rest();
            check_eapol(body)?;
            Msg::EapolRx { src, body }
        }
        SCAN_DONE => Msg::ScanDone { status: r.i32()? },
        DISASSOCIATED => Msg::Disassociated { reason: r.u16()?, local: bool_u8(r.u8()?)? },
        BEACON_LOSS => Msg::BeaconLoss,
        RF_KILL => Msg::RfKill { blocked: bool_u8(r.u8()?)? },
        FIRMWARE_ERROR => Msg::FirmwareError { code: r.u32()? },
        STATS => Msg::Stats(Stats {
            tx_packets: r.u64()?,
            rx_packets: r.u64()?,
            tx_failed: r.u64()?,
            rx_dropped: r.u64()?,
            beacon_rssi: r.u8()? as i8,
            tx_rate_kbps: r.u32()?,
        }),
        other => return Err(Error::UnknownKind(other)),
    })
}

/// Encodes `m` with request id `seq` into `out`; returns the bytes written.
/// The same validity checks as [`decode`] apply, so nothing is encoded that
/// the other end would refuse.
pub fn encode(m: &Msg<'_>, seq: u32, out: &mut [u8]) -> Result<usize, Error> {
    if out.len() < HEADER_LEN {
        return Err(Error::BufTooSmall);
    }
    let (head, body) = out.split_at_mut(HEADER_LEN);
    let n = {
        let mut w = Wr::new(body);
        encode_body(m, &mut w)?;
        w.pos()
    };
    if n > MAX_BODY {
        return Err(Error::BadLength);
    }
    Header { kind: m.kind(), seq, len: n as u32 }.encode(&mut Wr::new(head))?;
    let total = HEADER_LEN + n;
    // Round-trip through the decoder: one place defines what is valid.
    let any_dir = [
        Direction::MlmeToKernel,
        Direction::KeysToKernel,
        Direction::KernelToMlme,
        Direction::KernelToKeys,
    ]
    .into_iter()
    .find(|&d| permitted(m.kind(), d))
    .ok_or(Error::UnknownKind(m.kind()))?;
    decode(&out[..total], any_dir)?;
    Ok(total)
}

fn encode_body(m: &Msg<'_>, w: &mut Wr<'_>) -> Result<(), Error> {
    match m {
        Msg::GetCaps | Msg::ClearBss | Msg::BeaconLoss => Ok(()),
        Msg::Scan(s) => {
            w.u16(s.flags)?;
            w.bytes(&s.mac)?;
            w.u16(s.dwell_ms)?;
            write_list(w, &s.ssids, MAX_SCAN_SSIDS)?;
            write_list(w, &s.freqs, MAX_SCAN_FREQS)
        }
        Msg::SetBss(b) => {
            w.bytes(&b.bssid)?;
            w.u16(b.freq)?;
            w.u8(b.width as u8)?;
            w.u16(b.center1)?;
            w.u16(b.center2)?;
            w.u16(b.aid)?;
            w.u16(b.beacon_int)?;
            w.u8(b.dtim_period)?;
            w.u16(b.flags)?;
            w.u16(b.basic_rates)?;
            w.u16(b.rates)?;
            w.bytes(&b.ht_mcs)?;
            w.u16(b.vht_rx_mcs)?;
            w.u16(b.vht_tx_mcs)?;
            for x in b.he_mcs {
                w.u16(x)?;
            }
            w.u8(b.ampdu_factor)?;
            w.u8(b.ampdu_density)
        }
        Msg::TxMgmt { freq, frame } => {
            w.u16(*freq)?;
            w.bytes(frame)
        }
        Msg::SetPowerSave(p) => w.u8(*p as u8),
        Msg::SetRegulatory { alpha2, rules } => {
            w.bytes(alpha2)?;
            write_list(w, rules, MAX_REG_RULES)
        }
        Msg::SetMac(m) => w.bytes(m),
        Msg::Disconnect { reason } => w.u16(*reason),
        Msg::SetKey(k) => {
            w.u8(k.kind as u8)?;
            w.u8(k.idx)?;
            w.u8(k.cipher as u8)?;
            w.bytes(&k.addr)?;
            w.bytes(&k.rsc)?;
            w.bytes(&k.key)
        }
        Msg::DelKey { kind, idx, addr } => {
            w.u8(*kind as u8)?;
            w.u8(*idx)?;
            w.bytes(addr)
        }
        Msg::PortAuthorized(b) => w.u8(*b as u8),
        Msg::TxEapol { dst, body } => {
            w.bytes(dst)?;
            w.bytes(body)
        }
        Msg::Ack { status } => w.i32(*status),
        Msg::Caps(c) => {
            w.bytes(&c.mac)?;
            w.u8(c.bands)?;
            w.u32(c.ciphers)?;
            w.u32(c.flags)?;
            w.u8(c.max_scan_ssids)?;
            write_list(w, &c.channels, MAX_CHANNELS)
        }
        Msg::MgmtRx { freq, rssi, flags, tsf_us, frame } => {
            w.u16(*freq)?;
            w.u8(*rssi as u8)?;
            w.u8(*flags)?;
            w.u64(*tsf_us)?;
            w.bytes(frame)
        }
        Msg::EapolRx { src, body } => {
            w.bytes(src)?;
            w.bytes(body)
        }
        Msg::ScanDone { status } => w.i32(*status),
        Msg::Disassociated { reason, local } => {
            w.u16(*reason)?;
            w.u8(*local as u8)
        }
        Msg::RfKill { blocked } => w.u8(*blocked as u8),
        Msg::FirmwareError { code } => w.u32(*code),
        Msg::Stats(s) => {
            w.u64(s.tx_packets)?;
            w.u64(s.rx_packets)?;
            w.u64(s.tx_failed)?;
            w.u64(s.rx_dropped)?;
            w.u8(s.beacon_rssi as u8)?;
            w.u32(s.tx_rate_kbps)
        }
    }
}

#[cfg(test)]
mod tests;

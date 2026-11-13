//! The rules of Aura's kernel data path (wifi.md §3.1, §8.3, §8.5).
//!
//! [`Wlan`] is one radio's state as the kernel core `aura_wlan` keeps it:
//! the associated BSS, the key table, the controlled port and the
//! regulatory rules. It takes decoded [`aura_proto`] requests from the two
//! channels (`ctlN` for `aura-mlme`, `keyN` for `aura-keys`), drives the
//! hardware through the [`Radio`] trait, and classifies every data frame in
//! both directions. The kmod adds only the FreeBSD parts: cdevs, ifnet,
//! mbufs, locking.
//!
//! What the kernel trusts from user space is limited to what this file
//! checks: a compromised `aura-mlme` cannot install keys, see EAPOL, send
//! frames as another station, act as an AP or leave the regulatory rules; a
//! compromised `aura-keys` cannot send management frames or scan.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod amsdu;
pub mod frame;
pub mod keys;
pub mod reg;

use amsdu::Amsdu;
use aura_proto::{Bss, KeyKind, Mac, Msg};
use frame::{decap, is_group, DataHdr, Drop, TxHdr, ETHERTYPE_EAPOL};
use keys::{Install, Table};
use reg::Regulatory;

/// FreeBSD errno values returned in `Ack`.
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const EBUSY: i32 = 16;
    pub const EINVAL: i32 = 22;
    pub const EALREADY: i32 = 37;
    pub const ENOTCONN: i32 = 57;
}

/// What the hardware driver (`aura_mt79`, `aura_intel`, `aura_sim`) does
/// for the core. Control operations reach the driver as the same
/// [`aura_proto`] messages the daemons send, after the core has checked
/// them: `Scan`, `SetBss`, `ClearBss`, `TxMgmt`, `SetPowerSave`,
/// `SetRegulatory`, `SetMac`, `SetKey`, `DelKey`. Across kmods that is the
/// C ABI too: one versioned, validated byte format, nothing else to keep in
/// step. The driver keeps the last `SetRegulatory` (the world rules until
/// one arrives) and scans channels that do not allow initiating radiation
/// passively, whatever the scan request says. Errors are errno values.
pub trait Radio {
    fn own_mac(&self) -> Mac;
    fn request(&mut self, m: &Msg<'_>) -> Result<(), i32>;
}

/// Per-frame receive information from the hardware.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxInfo {
    /// The frame arrived protected and the hardware decrypted it and checked
    /// its MIC. The crypto header and MIC are already removed.
    pub decrypted: bool,
    /// Packet number from the crypto header (48 bits).
    pub pn: u64,
    pub key_idx: u8,
}

/// Where a received MSDU goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    /// Up the ifnet as an Ethernet frame.
    Stack,
    /// To `aura-keys` as `EapolRx` (never to the ifnet, never to aura-mlme).
    Keys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msdu<'a> {
    pub dest: Dest,
    pub da: Mac,
    pub sa: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

enum Body<'a> {
    One(Msdu<'a>),
    Amsdu { a: Amsdu<'a>, port_open: bool, bssid: Mac, own: Mac },
}

/// The MSDUs of one received data frame, all already checked.
pub struct Rx<'a>(Body<'a>);

impl<'a> Rx<'a> {
    pub fn iter(&self) -> impl Iterator<Item = Msdu<'a>> + '_ {
        let (one, many) = match &self.0 {
            Body::One(m) => (Some(*m), None),
            Body::Amsdu { a, port_open, bssid, own } => {
                let (port_open, bssid, own) = (*port_open, *bssid, *own);
                (
                    None,
                    Some(a.iter().filter_map(move |s| {
                        // Every subframe passed `classify` in `rx_data`.
                        classify(s.da, s.sa, s.msdu, port_open, &bssid, &own).ok()
                    })),
                )
            }
        };
        one.into_iter().chain(many.into_iter().flatten())
    }
}

fn classify<'a>(da: Mac, sa: Mac, msdu: &'a [u8], port_open: bool, bssid: &Mac, own: &Mac) -> Result<Msdu<'a>, Drop> {
    if sa == *own {
        return Err(Drop::FromSelf);
    }
    let (ethertype, payload) = decap(msdu)?;
    if ethertype == ETHERTYPE_EAPOL {
        // EAPOL only from the AP, only unicast to us.
        if sa != *bssid || da != *own {
            return Err(Drop::BadEapol);
        }
        return Ok(Msdu { dest: Dest::Keys, da, sa, ethertype, payload });
    }
    if !port_open {
        return Err(Drop::PortClosed);
    }
    Ok(Msdu { dest: Dest::Stack, da, sa, ethertype, payload })
}

/// Who sent a transmit request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxFrom {
    /// The network stack (ifnet output).
    Stack,
    /// `aura-keys` through `TxEapol`.
    Keys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Bss(Bss),
}

pub struct Wlan<R: Radio> {
    pub radio: R,
    state: State,
    scanning: bool,
    port_open: bool,
    keys: Table,
    pub reg: Regulatory,
}

/// The reply to a request: an errno (0 = success) for `Ack`, or the caps.
pub type Status = i32;

impl<R: Radio> Wlan<R> {
    pub fn new(radio: R) -> Self {
        Wlan {
            radio,
            state: State::Idle,
            scanning: false,
            port_open: false,
            keys: Table::default(),
            reg: Regulatory::default(),
        }
    }

    pub fn bss(&self) -> Option<&Bss> {
        match &self.state {
            State::Bss(b) => Some(b),
            State::Idle => None,
        }
    }

    pub fn port_open(&self) -> bool {
        self.port_open
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// Leaves the BSS: keys gone (hardware and table), port closed.
    fn leave(&mut self) {
        if let State::Bss(_) = self.state {
            // Leaving cannot fail; the driver resets the hardware if it must.
            let _ = self.radio.request(&Msg::ClearBss);
        }
        self.state = State::Idle;
        self.port_open = false;
        self.keys.clear();
    }

    /// A request on `ctlN` (from aura-mlme). `GetCaps` is answered by the
    /// kmod from the radio's capabilities, not here.
    pub fn ctl(&mut self, m: &Msg<'_>) -> Status {
        use errno::*;
        match m {
            Msg::Scan(s) => {
                if self.scanning {
                    return EBUSY;
                }
                if s.freqs.iter().any(|f| !self.reg.allowed(f.0)) {
                    return EINVAL;
                }
                match self.radio.request(m) {
                    Ok(()) => {
                        self.scanning = true;
                        0
                    }
                    Err(e) => e,
                }
            }
            Msg::SetBss(b) => {
                if !matches!(self.state, State::Idle) {
                    return EALREADY;
                }
                if !self.reg.allowed(b.freq) {
                    return EINVAL;
                }
                match self.radio.request(m) {
                    Ok(()) => {
                        self.state = State::Bss(*b);
                        0
                    }
                    Err(e) => e,
                }
            }
            Msg::ClearBss => {
                self.leave();
                0
            }
            Msg::TxMgmt { freq, frame } => {
                // Source and transmitter must be us (no spoofing other
                // stations); the subtype was checked by aura-proto.
                let own = self.radio.own_mac();
                if frame[10..16] != own {
                    return EPERM;
                }
                let ok = match &self.state {
                    State::Bss(b) => *freq == b.freq || self.reg.may_initiate(*freq),
                    State::Idle => self.reg.may_initiate(*freq),
                };
                if !ok {
                    return EPERM;
                }
                self.radio.request(m).err().unwrap_or(0)
            }
            Msg::SetPowerSave(_) => self.radio.request(m).err().unwrap_or(0),
            Msg::SetRegulatory { alpha2, rules } => {
                self.reg.set(*alpha2, rules.iter());
                if let Err(e) = self.radio.request(m) {
                    return e;
                }
                if let Some(b) = self.bss() {
                    if !self.reg.allowed(b.freq) {
                        self.leave();
                    }
                }
                0
            }
            Msg::SetMac(_) => {
                if !matches!(self.state, State::Idle) || self.scanning {
                    return EBUSY;
                }
                self.radio.request(m).err().unwrap_or(0)
            }
            Msg::Disconnect { .. } => {
                if matches!(self.state, State::Idle) {
                    return ENOTCONN;
                }
                self.leave();
                0
            }
            _ => EINVAL,
        }
    }

    /// A request on `keyN` (from aura-keys). `TxEapol` goes through
    /// [`Wlan::tx_data`] with [`TxFrom::Keys`] instead.
    pub fn keys(&mut self, m: &Msg<'_>) -> Status {
        use errno::*;
        let Some(bss) = self.bss().copied() else {
            return ENOTCONN;
        };
        match m {
            Msg::SetKey(k) => {
                let ok_addr = match k.kind {
                    KeyKind::Pairwise => k.addr == bss.bssid,
                    _ => k.addr == frame::BROADCAST,
                };
                if !ok_addr {
                    return EINVAL;
                }
                match self.keys.install(k) {
                    Install::Unchanged => 0,
                    Install::New => match self.radio.request(m) {
                        Ok(()) => 0,
                        Err(e) => {
                            self.keys.remove(k.kind, k.idx);
                            e
                        }
                    },
                }
            }
            Msg::DelKey { kind, idx, .. } => {
                if !self.keys.remove(*kind, *idx) {
                    return EINVAL;
                }
                self.radio.request(m).err().unwrap_or(0)
            }
            Msg::PortAuthorized(open) => {
                self.port_open = *open;
                0
            }
            _ => EINVAL,
        }
    }

    /// The radio reports the scan finished.
    pub fn scan_done(&mut self) {
        self.scanning = false;
    }

    /// The radio lost the BSS (deauth, beacon loss confirmed, firmware
    /// reset). The kmod sends `Disassociated` to both channels.
    pub fn disassociated(&mut self) {
        self.leave();
    }

    /// Classifies one received data frame (FCS removed; crypto header and MIC
    /// removed when `info.decrypted`). Either every MSDU in it is good and
    /// returned, or the whole frame is dropped.
    pub fn rx_data<'a>(&mut self, f: &'a [u8], info: &RxInfo) -> Result<Rx<'a>, Drop> {
        let h = DataHdr::parse(f)?;
        let State::Bss(bss) = self.state else {
            return Err(Drop::NotAssociated);
        };
        let own = self.radio.own_mac();
        if h.to_ds || !h.from_ds || h.four_addr {
            return Err(Drop::WrongDirection);
        }
        if h.addr1 != own && !is_group(&h.addr1) {
            return Err(Drop::NotForUs);
        }
        if h.addr2 != bss.bssid {
            return Err(Drop::NotFromBss);
        }
        if h.more_frag || h.frag != 0 {
            return Err(Drop::Fragment);
        }
        if !h.has_body {
            return Err(Drop::NoBody);
        }
        let group = is_group(&h.addr1);
        if h.protected {
            if !info.decrypted {
                return Err(Drop::NotDecrypted);
            }
            let kind = if group { KeyKind::Group } else { KeyKind::Pairwise };
            if !self.keys.check_pn(kind, info.key_idx, h.tid as usize, info.pn) {
                // Unknown key or replay: both look the same to an attacker.
                return Err(if self.keys.get(kind, info.key_idx).is_none() {
                    Drop::NotDecrypted
                } else {
                    Drop::Replay
                });
            }
        } else if self.keys.has_pairwise() || (group && self.keys.has_group()) {
            // Once keys exist nothing travels in the clear, EAPOL included
            // (rekeying EAPOL is protected).
            return Err(Drop::Unprotected);
        }
        let body = &f[h.len..];
        if h.amsdu {
            let a = Amsdu::parse(body, &own)?;
            for s in a.iter() {
                classify(s.da, s.sa, s.msdu, self.port_open, &bss.bssid, &own)?;
            }
            Ok(Rx(Body::Amsdu { a, port_open: self.port_open, bssid: bss.bssid, own }))
        } else {
            let m = classify(h.addr1, h.addr3, body, self.port_open, &bss.bssid, &own)?;
            Ok(Rx(Body::One(m)))
        }
    }

    /// The header for an outgoing MSDU. The stack may not send EAPOL (only
    /// aura-keys can) and may send nothing while the port is closed; frames
    /// always leave from our own address.
    pub fn tx_data(&self, from: TxFrom, dst: &Mac, src: &Mac, ethertype: u16, prio: u8) -> Result<TxHdr, Drop> {
        let State::Bss(bss) = self.state else {
            return Err(Drop::NotAssociated);
        };
        let own = self.radio.own_mac();
        if *src != own {
            return Err(Drop::FromSelf);
        }
        match from {
            TxFrom::Stack => {
                if ethertype == ETHERTYPE_EAPOL {
                    return Err(Drop::BadEapol);
                }
                if !self.port_open {
                    return Err(Drop::PortClosed);
                }
            }
            TxFrom::Keys => {
                if ethertype != ETHERTYPE_EAPOL || *dst != bss.bssid {
                    return Err(Drop::BadEapol);
                }
            }
        }
        let qos = bss.flags & aura_proto::bss_flags::QOS != 0;
        let encrypt = self.keys.has_pairwise();
        Ok(frame::build_tx(&bss.bssid, &own, dst, ethertype, frame::tid_for(ethertype, prio), qos, encrypt))
    }
}

#[cfg(test)]
mod tests;

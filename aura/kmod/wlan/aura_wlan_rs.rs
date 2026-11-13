// SPDX-License-Identifier: BSD-2-Clause
//
// The seam between aura_wlan.c (cdevs, ifnet, mbufs, locking) and the Rust
// core (aura-wlan-core over aura-proto). docs/spec/wifi.md §3.3.
//
// Every function here is called by aura_wlan.c with the radio mutex held,
// except aura_rs_size/aura_rs_align. All bytes that came from user space or
// from the air are decoded by aura-proto / aura-wlan-core; this file only
// moves pointers across the C boundary. Its `unsafe` is that and nothing
// else: each block turns C (pointer, length) pairs into slices, or places
// the core in memory C allocated with aura_rs_size()/aura_rs_align().

#![no_std]

use aura_proto::{
    caps_flags, decode, encode, ChanInfo, Caps, Direction, List, Mac, Msg, HEADER_LEN, MAX_BODY,
    MAX_CHANNELS,
};
use aura_wlan_core::{frame::Drop, Dest, Radio, RxInfo, TxFrom, Wlan};
use core::ffi::c_void;
use core::{mem, ptr, slice};

const MSG_MAX: usize = HEADER_LEN + MAX_BODY;

unsafe extern "C" {
    /// aura_wlan.c: ops->request(drv, msg, len) for the radio `ctx`.
    fn aura_c_request(ctx: *mut c_void, msg: *const u8, len: usize) -> i32;
}

#[repr(C)]
pub struct CChan {
    freq: u16,
    flags: u16,
    max_power_mbm: i16,
}

/// Mirrors `struct aura_caps` in aura_wlan.h.
#[repr(C)]
pub struct CCaps {
    mac: [u8; 6],
    bands: u8,
    max_scan_ssids: u8,
    ciphers: u32,
    flags: u32,
    nchan: u16,
    chan: [CChan; MAX_CHANNELS],
}

/// Mirrors `struct aura_rx_info`.
#[repr(C)]
pub struct CRxInfo {
    decrypted: u8,
    key_idx: u8,
    pn: u64,
}

/// One MSDU of a received data frame: offsets into the frame.
#[repr(C)]
pub struct CMsdu {
    dest: u8, // 0 = stack, 1 = aura-keys
    da: [u8; 6],
    sa: [u8; 6],
    ethertype: u16,
    off: u32,
    len: u32,
}

/// A header to prepend and how to send.
#[repr(C)]
pub struct CTx {
    hdr: [u8; 34],
    hdr_len: u8,
    tid: u8,
    encrypt: u8,
    /// aura_rs_write only: 1 = send `body` (TxEapol) with `hdr`.
    send: u8,
    body_off: u32,
    body_len: u32,
}

struct CRadio {
    ctx: *mut c_void,
    mac: Mac,
    /// MSG_MAX bytes owned by aura_wlan.c, for re-encoding requests.
    scratch: *mut u8,
}

impl Radio for CRadio {
    fn own_mac(&self) -> Mac {
        self.mac
    }
    fn request(&mut self, m: &Msg<'_>) -> Result<(), i32> {
        // SAFETY: `scratch` is MSG_MAX bytes, exclusive to this radio, used
        // only under the radio lock (held for every call into this file).
        let buf = unsafe { slice::from_raw_parts_mut(self.scratch, MSG_MAX) };
        let n = encode(m, 0, buf).map_err(|_| 22)?;
        // SAFETY: C call; `buf[..n]` is valid for the duration.
        match unsafe { aura_c_request(self.ctx, buf.as_ptr(), n) } {
            0 => Ok(()),
            e => Err(e),
        }
    }
}

pub struct State {
    wlan: Wlan<CRadio>,
    bands: u8,
    max_scan_ssids: u8,
    ciphers: u32,
    flags: u32,
    nchan: usize,
    chans: [u8; MAX_CHANNELS * 6],
}

const EINVAL: i32 = 22;

/// # Safety
/// `p` is null or valid for `n` bytes.
unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(p, n) }
    }
}

/// # Safety
/// `p` is valid for `n` writable bytes.
unsafe fn bytes_mut<'a>(p: *mut u8, n: usize) -> &'a mut [u8] {
    unsafe { slice::from_raw_parts_mut(p, n) }
}

/// # Safety
/// `s` came from aura_rs_init and the radio lock is held.
unsafe fn st<'a>(s: *mut c_void) -> &'a mut State {
    unsafe { &mut *(s as *mut State) }
}

#[unsafe(no_mangle)]
pub extern "C" fn aura_rs_size() -> usize {
    mem::size_of::<State>()
}

#[unsafe(no_mangle)]
pub extern "C" fn aura_rs_align() -> usize {
    mem::align_of::<State>()
}

/// Places the core in `mem` (aura_rs_size() bytes, aura_rs_align()-aligned).
/// The channel list is validated here; 0 or EINVAL.
///
/// # Safety
/// As documented; `caps` and `scratch` (MSG_MAX bytes) outlive the state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_init(
    mem: *mut c_void,
    ctx: *mut c_void,
    caps: *const CCaps,
    scratch: *mut u8,
) -> i32 {
    let c = unsafe { &*caps };
    let nchan = c.nchan as usize;
    if nchan > MAX_CHANNELS || c.mac[0] & 1 != 0 {
        return EINVAL;
    }
    let mut chans = [0u8; MAX_CHANNELS * 6];
    for (i, ch) in c.chan[..nchan].iter().enumerate() {
        let o = &mut chans[i * 6..i * 6 + 6];
        o[..2].copy_from_slice(&ch.freq.to_le_bytes());
        o[2..4].copy_from_slice(&ch.flags.to_le_bytes());
        o[4..].copy_from_slice(&ch.max_power_mbm.to_le_bytes());
    }
    // Validate the list the same way a reader of Caps will.
    if List::<ChanInfo>::from_raw(&chans[..nchan * 6]).is_none() {
        return EINVAL;
    }
    let s = State {
        wlan: Wlan::new(CRadio { ctx, mac: c.mac, scratch }),
        bands: c.bands,
        max_scan_ssids: c.max_scan_ssids,
        ciphers: c.ciphers,
        flags: c.flags & (caps_flags::PMF
            | caps_flags::HW_SCAN
            | caps_flags::RANDOM_MAC_SCAN
            | caps_flags::SPP_AMSDU
            | caps_flags::BEACON_PROTECTION),
        nchan,
        chans,
    };
    // SAFETY: `mem` is sized and aligned for State (C checked both).
    unsafe { ptr::write(mem as *mut State, s) };
    0
}

/// # Safety
/// `mem` came from aura_rs_init; not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_fini(mem: *mut c_void) {
    unsafe { ptr::drop_in_place(mem as *mut State) };
}

fn reply(seq: u32, m: &Msg<'_>, out: &mut [u8]) -> usize {
    encode(m, seq, out).unwrap_or(0)
}

/// One message written to a channel (0 = ctlN, 1 = keyN). Always produces a
/// reply in `out` (Ack, or Caps for GetCaps) unless the message did not
/// decode at all, in which case it returns the errno for write(2). For a
/// TxEapol that passed, `tx.send` is 1 and C sends `buf[body_off..]` with
/// `tx.hdr`.
///
/// # Safety
/// `buf`/`len`, `out`/`cap` valid; `out_len`, `tx` writable; lock held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_write(
    mem: *mut c_void,
    chan: u32,
    buf: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
    tx: *mut CTx,
) -> i32 {
    let s = unsafe { st(mem) };
    let input = unsafe { bytes(buf, len) };
    let out = unsafe { bytes_mut(out, cap) };
    let tx = unsafe { &mut *tx };
    tx.send = 0;
    unsafe { *out_len = 0 };
    let dir = match chan {
        0 => Direction::MlmeToKernel,
        1 => Direction::KeysToKernel,
        _ => return EINVAL,
    };
    let (h, m) = match decode(input, dir) {
        Ok(v) => v,
        Err(_) => return EINVAL,
    };
    let status = match (&m, chan) {
        (Msg::GetCaps, 0) => {
            let channels = match List::<ChanInfo>::from_raw(&s.chans[..s.nchan * 6]) {
                Some(l) => l,
                None => return EINVAL,
            };
            let caps = Msg::Caps(Caps {
                mac: s.wlan.radio.mac,
                bands: s.bands,
                ciphers: s.ciphers,
                flags: s.flags,
                max_scan_ssids: s.max_scan_ssids,
                channels,
            });
            let n = reply(h.seq, &caps, out);
            unsafe { *out_len = n };
            return 0;
        }
        (Msg::SetMac(mac), 0) => {
            let st = s.wlan.ctl(&m);
            if st == 0 {
                s.wlan.radio.mac = *mac;
            }
            st
        }
        (Msg::TxEapol { dst, body }, 1) => {
            let own = s.wlan.radio.mac;
            match s.wlan.tx_data(TxFrom::Keys, dst, &own, aura_wlan_core::frame::ETHERTYPE_EAPOL, 7) {
                Ok(t) => {
                    tx.hdr = t.bytes;
                    tx.hdr_len = t.len as u8;
                    tx.tid = t.tid;
                    tx.encrypt = t.encrypt as u8;
                    tx.send = 1;
                    tx.body_off = (body.as_ptr() as usize - input.as_ptr() as usize) as u32;
                    tx.body_len = body.len() as u32;
                    0
                }
                Err(_) => 57, // ENOTCONN / not permitted now
            }
        }
        (_, 0) => s.wlan.ctl(&m),
        (_, _) => s.wlan.keys(&m),
    };
    let n = reply(h.seq, &Msg::Ack { status: -status }, out);
    unsafe { *out_len = n };
    0
}

/// The channel's owner closed it: leave the BSS (keys and port go with it).
///
/// # Safety
/// Lock held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_close(mem: *mut c_void, _chan: u32) {
    let s = unsafe { st(mem) };
    if s.wlan.bss().is_some() {
        s.wlan.disassociated();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_port_open(mem: *mut c_void) -> i32 {
    (unsafe { st(mem) }).wlan.port_open() as i32
}

/// Header for an outgoing Ethernet frame from the stack. 0 or the drop code
/// + 1.
///
/// # Safety
/// `dst`/`src` 6 bytes; `tx` writable; lock held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_tx_hdr(
    mem: *mut c_void,
    dst: *const u8,
    src: *const u8,
    ethertype: u16,
    prio: u8,
    tx: *mut CTx,
) -> i32 {
    let s = unsafe { st(mem) };
    let mut d = [0u8; 6];
    let mut sa = [0u8; 6];
    d.copy_from_slice(unsafe { bytes(dst, 6) });
    sa.copy_from_slice(unsafe { bytes(src, 6) });
    match s.wlan.tx_data(TxFrom::Stack, &d, &sa, ethertype, prio) {
        Ok(t) => {
            let tx = unsafe { &mut *tx };
            tx.hdr = t.bytes;
            tx.hdr_len = t.len as u8;
            tx.tid = t.tid;
            tx.encrypt = t.encrypt as u8;
            tx.send = 1;
            0
        }
        Err(e) => drop_code(e),
    }
}

fn drop_code(d: Drop) -> i32 {
    d as i32 + 1
}

/// Classifies a received data frame into at most `max` MSDUs. 0 with
/// `*n_out` set, or the drop code + 1.
///
/// # Safety
/// `frame`/`len` valid; `info` readable; `out` has `max` entries; lock held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_rx_data(
    mem: *mut c_void,
    frame: *const u8,
    len: usize,
    info: *const CRxInfo,
    out: *mut CMsdu,
    max: usize,
    n_out: *mut usize,
) -> i32 {
    let s = unsafe { st(mem) };
    let f = unsafe { bytes(frame, len) };
    let i = unsafe { &*info };
    let info = RxInfo { decrypted: i.decrypted != 0, pn: i.pn, key_idx: i.key_idx };
    let out = unsafe { slice::from_raw_parts_mut(out, max) };
    unsafe { *n_out = 0 };
    let rx = match s.wlan.rx_data(f, &info) {
        Ok(r) => r,
        Err(e) => return drop_code(e),
    };
    let mut n = 0;
    for m in rx.iter() {
        if n == max {
            return drop_code(Drop::AmsduInjection);
        }
        out[n] = CMsdu {
            dest: match m.dest {
                Dest::Stack => 0,
                Dest::Keys => 1,
            },
            da: m.da,
            sa: m.sa,
            ethertype: m.ethertype,
            off: (m.payload.as_ptr() as usize - f.as_ptr() as usize) as u32,
            len: m.payload.len() as u32,
        };
        n += 1;
    }
    unsafe { *n_out = n };
    0
}

/// Encodes a received management frame as MgmtRx for aura-mlme. Bytes
/// written, or 0 if the frame is not a well-formed management frame.
///
/// # Safety
/// Pointers valid as named.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_enc_mgmt_rx(
    freq: u16,
    rssi: i8,
    flags: u8,
    tsf_us: u64,
    frame: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> usize {
    let f = unsafe { bytes(frame, len) };
    let out = unsafe { bytes_mut(out, cap) };
    reply(0, &Msg::MgmtRx { freq, rssi, flags, tsf_us, frame: f }, out)
}

/// Encodes an EAPOL frame for aura-keys. Bytes written or 0.
///
/// # Safety
/// Pointers valid as named.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_enc_eapol_rx(
    src: *const u8,
    body: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> usize {
    let mut sa = [0u8; 6];
    sa.copy_from_slice(unsafe { bytes(src, 6) });
    let b = unsafe { bytes(body, len) };
    let out = unsafe { bytes_mut(out, cap) };
    reply(0, &Msg::EapolRx { src: sa, body: b }, out)
}

/// A driver event (aura-proto, kernel -> daemons). Updates the core and
/// returns the channels to forward it to (bit 0 ctlN, bit 1 keyN), or -1 if
/// it is not a valid event.
///
/// # Safety
/// `msg`/`len` valid; lock held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aura_rs_event(mem: *mut c_void, msg: *const u8, len: usize) -> i32 {
    let s = unsafe { st(mem) };
    let b = unsafe { bytes(msg, len) };
    let Ok((_, m)) = decode(b, Direction::KernelToMlme) else {
        return -1;
    };
    match m {
        Msg::ScanDone { .. } => {
            s.wlan.scan_done();
            1
        }
        Msg::Disassociated { .. } => {
            s.wlan.disassociated();
            3
        }
        Msg::BeaconLoss | Msg::RfKill { .. } | Msg::Stats(_) => 1,
        Msg::FirmwareError { .. } => {
            s.wlan.disassociated();
            1
        }
        _ => -1,
    }
}

/// Size checks for the C mirrors (aura_wlan.c asserts the same numbers).
#[unsafe(no_mangle)]
pub extern "C" fn aura_rs_abi_sizes(caps: *mut usize, rxinfo: *mut usize, msdu: *mut usize, tx: *mut usize) {
    unsafe {
        *caps = mem::size_of::<CCaps>();
        *rxinfo = mem::size_of::<CRxInfo>();
        *msdu = mem::size_of::<CMsdu>();
        *tx = mem::size_of::<CTx>();
    }
}

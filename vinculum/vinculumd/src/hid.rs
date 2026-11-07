//! The Classic HID host and the hand-off to Tactus (docs/spec/bluetooth.md
//! §10).
//!
//! For each bonded Classic link that comes up encrypted: ask the device's
//! SDP server for its HID record (the report descriptor) and its PnP
//! record (vendor, product), then open the HID control (PSM 0x11) and
//! interrupt (0x13) channels on a link we made, or take the ones the
//! device opens on a link it made (a bonded keyboard reconnecting). Once both are open and the descriptor is known, the
//! device is handed to tactusd on `hid.sock`: an `Attach` with one end of
//! a `SOCK_SEQPACKET` pair, then reports on it.
//!
//! Reports: HIDP `DATA|Input` (0xA1) on the interrupt channel, the report
//! id byte split off into the packet header when the descriptor has report
//! ids. Output reports from Tactus go back as `DATA|Output` (0xA2) on the
//! interrupt channel; feature reports as `SET_REPORT`/`GET_REPORT` on the
//! control channel. The device fd is written without blocking: when
//! tactusd falls behind, new reports are dropped and counted, and a
//! `Dropped { count }` packet goes ahead of the next one that fits, so
//! Tactus can release held keys (the spec says "oldest"; a socket buffer
//! can't drop its oldest packet, and for key state either is safe once
//! `Dropped` is seen).
//!
//! Nothing reaches Tactus before the link is encrypted with a bond, and
//! `keyboard_allowed` comes from the bond (§7).

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use vinculum_control::Addr;
use vinculum_hid_wire::{self as bt, Kind, ReportHeader, ToTactus, ToVinculum};
use vinculum_host::{Host, HostEvent};
use vinculum_sdp::{self as sdp, attr, uuid, Query};

pub const PSM_CONTROL: u16 = 0x0011;
pub const PSM_INTERRUPT: u16 = 0x0013;

/// HIDP transaction headers (HID Profile 1.1.1 §3.1.1).
mod hidp {
    pub const DATA_INPUT: u8 = 0xA1;
    pub const DATA_OUTPUT: u8 = 0xA2;
    pub const DATA_FEATURE: u8 = 0xA3;
    pub const GET_REPORT_FEATURE: u8 = 0x43;
    pub const SET_REPORT_FEATURE: u8 = 0x53;
}

/// Whether a report descriptor declares report ids (a Report ID global
/// item, tag 0x84). Then every report starts with its id byte.
pub fn has_report_ids(d: &[u8]) -> bool {
    let mut i = 0;
    while i < d.len() {
        let b = d[i];
        if b == 0xFE {
            // Long item: size, tag, data.
            let Some(&size) = d.get(i + 1) else { return false };
            i += 3 + size as usize;
            continue;
        }
        let size = [0, 1, 2, 4][(b & 3) as usize];
        if b & 0xFC == 0x84 {
            return true;
        }
        i += 1 + size;
    }
    false
}

/// What the SDP channel is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Hid,
    Pnp,
}

struct SdpState {
    cid: u16,
    open: bool,
    step: Step,
    query: Query,
    /// The request to send once the channel is open.
    first: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
struct Chan {
    cid: u16,
    open: bool,
}

struct Device {
    id: u64,
    fd: OwnedFd,
    dropped: u32,
}

struct Link {
    addr: Addr,
    /// The device paged us: it opens control and interrupt itself (HID
    /// 1.1.1 §5.4.3), and we only take them. On links we made, we open
    /// them. Never both, so the two sides don't race for the same PSM.
    incoming: bool,
    keyboard_allowed: bool,
    record: bt::Record,
    sdp: Option<SdpState>,
    /// SDP finished (with or without a PnP record).
    sdp_done: bool,
    descriptor: Option<Vec<u8>>,
    report_ids: bool,
    ctrl: Option<Chan>,
    intr: Option<Chan>,
    device: Option<Device>,
}

struct Tactus {
    sock: UnixStream,
    reader: bt::Reader,
    hello: bool,
}

/// One poll entry the daemon adds for us.
#[derive(Debug, Clone, Copy)]
pub enum Fd {
    Listener,
    Tactus,
    Device(u16),
}

pub struct Hid {
    listener: Option<UnixListener>,
    allowed_uids: Vec<u32>,
    tactus: Option<Tactus>,
    links: HashMap<u16, Link>,
    next_device: u64,
}

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Sends one packet without blocking. Ok(false): the buffer is full.
fn send_packet(fd: RawFd, p: &[u8]) -> std::io::Result<bool> {
    // SAFETY: `p` is valid for reads of its length; fd is an open socket.
    let n = unsafe { libc::send(fd, p.as_ptr().cast(), p.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if n >= 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.kind() {
        ErrorKind::WouldBlock => Ok(false),
        ErrorKind::Interrupted => Ok(false),
        _ => Err(e),
    }
}

impl Hid {
    pub fn new(path: Option<&str>, allowed_uids: Vec<u32>) -> Hid {
        let listener = path.map(|p| {
            let _ = std::fs::remove_file(p);
            let l = UnixListener::bind(p).unwrap_or_else(|e| {
                eprintln!("vinculumd: HID socket {p}: {e}");
                std::process::exit(1)
            });
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660));
            l
        });
        Hid { listener, allowed_uids, tactus: None, links: HashMap::new(), next_device: 1 }
    }

    pub fn enabled(&self) -> bool {
        self.listener.is_some()
    }

    /// A bonded Classic link is encrypted: find out whether it is a HID
    /// device and bring it up.
    #[allow(clippy::too_many_arguments)]
    pub fn start(&mut self, host: &mut Host, handle: u16, addr: Addr, incoming: bool, name: &str, keyboard_allowed: bool, now: u64) {
        if !self.enabled() || self.links.contains_key(&handle) {
            return;
        }
        let (query, first) = Query::new(uuid::HID, &[attr::HID_DESCRIPTOR_LIST, attr::SERVICE_NAME]);
        let cid = match host.l2cap_connect(handle, sdp::PSM, now) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("vinculumd: {addr}: SDP channel: {e}");
                return;
            }
        };
        let record = bt::Record { name: name.to_string(), vendor: 0, product: 0, version: 0 };
        let link = Link {
            addr,
            incoming,
            keyboard_allowed,
            record,
            sdp: Some(SdpState { cid, open: false, step: Step::Hid, query, first: Some(first) }),
            sdp_done: false,
            descriptor: None,
            report_ids: false,
            ctrl: None,
            intr: None,
            device: None,
        };
        self.links.insert(handle, link);
    }

    /// The link is gone.
    pub fn link_gone(&mut self, handle: u16) {
        if let Some(l) = self.links.remove(&handle) {
            if let Some(d) = l.device {
                self.detach(d);
            }
        }
    }

    fn detach(&mut self, d: Device) {
        // Dropping our end gives Tactus EOF on the fd as well.
        self.send_tactus(&ToTactus::Detach { device: d.id });
    }

    fn send_tactus(&mut self, m: &ToTactus) {
        let Some(t) = &mut self.tactus else { return };
        if t.sock.write_all(&bt::encode(m)).is_err() {
            eprintln!("vinculumd: tactusd too slow or gone; dropped");
            self.tactus_gone();
        }
    }

    fn tactus_gone(&mut self) {
        self.tactus = None;
        for l in self.links.values_mut() {
            l.device = None;
        }
    }

    /// Channel events; false if the event was not ours.
    pub fn on_event(&mut self, host: &mut Host, ev: &HostEvent, now: u64) -> bool {
        match *ev {
            HostEvent::ChannelOpen { handle, cid, psm, .. } => self.channel_open(host, handle, cid, psm, now),
            HostEvent::ChannelData { handle, cid, ref payload } => self.channel_data(host, handle, cid, payload, now),
            HostEvent::ChannelClosed { handle, cid } => self.channel_closed(host, handle, cid, now),
            HostEvent::ChannelFailed { handle, psm, ref why } => {
                if let Some(l) = self.links.get_mut(&handle) {
                    eprintln!("vinculumd: {}: channel to PSM {psm:#x}: {why}", l.addr);
                    match psm {
                        sdp::PSM => l.sdp = None,
                        PSM_CONTROL => l.ctrl = None,
                        PSM_INTERRUPT => l.intr = None,
                        _ => {}
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn channel_open(&mut self, host: &mut Host, handle: u16, cid: u16, psm: u16, now: u64) {
        let Some(l) = self.links.get_mut(&handle) else {
            // An encrypted link we are not running HID on (pairing still
            // finishing, or HID off): nobody to hand the channel to.
            host.l2cap_disconnect(handle, cid, now);
            return;
        };
        match psm {
            sdp::PSM => {
                let Some(s) = l.sdp.as_mut().filter(|s| s.cid == cid) else {
                    return host.l2cap_disconnect(handle, cid, now);
                };
                s.open = true;
                if let Some(req) = s.first.take() {
                    let _ = host.l2cap_send(handle, cid, &req, now);
                }
            }
            PSM_CONTROL | PSM_INTERRUPT => {
                let slot = if psm == PSM_CONTROL { &mut l.ctrl } else { &mut l.intr };
                match slot {
                    None => *slot = Some(Chan { cid, open: true }),
                    Some(c) if c.cid == cid => c.open = true,
                    // The device opened one while ours was on its way:
                    // keep the first.
                    Some(_) => return host.l2cap_disconnect(handle, cid, now),
                }
                self.advance(host, handle, now);
            }
            _ => host.l2cap_disconnect(handle, cid, now),
        }
    }

    fn channel_data(&mut self, host: &mut Host, handle: u16, cid: u16, p: &[u8], now: u64) {
        let Some(l) = self.links.get_mut(&handle) else { return };
        if l.sdp.as_ref().is_some_and(|s| s.cid == cid) {
            return self.sdp_data(host, handle, p, now);
        }
        let ts_ns = monotonic_ns();
        let (kind, body) = match (l.intr.is_some_and(|c| c.cid == cid), l.ctrl.is_some_and(|c| c.cid == cid), p.split_first()) {
            (true, _, Some((&hidp::DATA_INPUT, b))) => (Kind::Input, b),
            (_, true, Some((&hidp::DATA_FEATURE, b))) => (Kind::FeatureReply, b),
            // Handshakes and anything else: nothing for Tactus.
            _ => return,
        };
        let (report_id, report) = match (l.report_ids, body.split_first()) {
            (true, Some((&id, r))) => (id, r),
            (true, None) => return,
            (false, _) => (0, body),
        };
        if report.len() + bt::HEADER_LEN > bt::MAX_PACKET {
            return;
        }
        let Some(d) = l.device.as_mut() else { return };
        let fd = d.fd.as_raw_fd();
        if d.dropped > 0 {
            let note = bt::packet(ReportHeader { kind: Kind::Dropped, report_id: 0, ts_ns }, &d.dropped.to_le_bytes());
            match send_packet(fd, &note) {
                Ok(true) => d.dropped = 0,
                Ok(false) => {
                    d.dropped = d.dropped.saturating_add(1);
                    return;
                }
                Err(_) => return self.device_closed(handle),
            }
        }
        match send_packet(fd, &bt::packet(ReportHeader { kind, report_id, ts_ns }, report)) {
            Ok(true) => {}
            Ok(false) => d.dropped = d.dropped.saturating_add(1),
            Err(_) => self.device_closed(handle),
        }
    }

    /// Tactus closed a device fd (it could not use the device).
    fn device_closed(&mut self, handle: u16) {
        if let Some(d) = self.links.get_mut(&handle).and_then(|l| l.device.take()) {
            self.detach(d);
        }
    }

    fn sdp_data(&mut self, host: &mut Host, handle: u16, p: &[u8], now: u64) {
        let l = self.links.get_mut(&handle).expect("checked");
        let s = l.sdp.as_mut().expect("checked");
        let cid = s.cid;
        let records = match s.query.on_response(p) {
            Ok(Err(next)) => {
                let _ = host.l2cap_send(handle, cid, &next, now);
                return;
            }
            Ok(Ok(r)) => r,
            Err(e) => {
                eprintln!("vinculumd: {}: SDP: {e:?}", l.addr);
                Vec::new()
            }
        };
        match s.step {
            Step::Hid => {
                let found = records.iter().find_map(|r| sdp::hid_descriptor(r).map(|d| (d, sdp::text(r, attr::SERVICE_NAME))));
                let Some((desc, name)) = found.filter(|(d, _)| !d.is_empty() && d.len() <= bt::MAX_DESCRIPTOR) else {
                    eprintln!("vinculumd: {}: no HID service", l.addr);
                    l.sdp = None;
                    host.l2cap_disconnect(handle, cid, now);
                    self.links.remove(&handle);
                    return;
                };
                if l.record.name.is_empty() {
                    l.record.name = name.unwrap_or_default();
                }
                l.report_ids = has_report_ids(&desc);
                l.descriptor = Some(desc);
                let (q, req) = Query::new(uuid::PNP_INFO, &[attr::VENDOR_ID, attr::PRODUCT_ID, attr::VERSION]);
                s.step = Step::Pnp;
                s.query = q;
                let _ = host.l2cap_send(handle, cid, &req, now);
            }
            Step::Pnp => {
                if let Some(r) = records.first() {
                    let u = |id| sdp::uint(r, id).unwrap_or(0) as u16;
                    l.record.vendor = u(attr::VENDOR_ID);
                    l.record.product = u(attr::PRODUCT_ID);
                    l.record.version = u(attr::VERSION);
                }
                l.sdp = None;
                l.sdp_done = true;
                host.l2cap_disconnect(handle, cid, now);
                self.advance(host, handle, now);
            }
        }
    }

    fn channel_closed(&mut self, host: &mut Host, handle: u16, cid: u16, now: u64) {
        let Some(l) = self.links.get_mut(&handle) else { return };
        if l.sdp.as_ref().is_some_and(|s| s.cid == cid) {
            l.sdp = None;
            return;
        }
        let ctrl = l.ctrl.is_some_and(|c| c.cid == cid);
        let intr = l.intr.is_some_and(|c| c.cid == cid);
        if !ctrl && !intr {
            return;
        }
        // Either channel closing ends the HID session (HID 1.1.1 §5.2.3);
        // the device opens both again when it wants to talk.
        let other = if ctrl { l.intr.take() } else { l.ctrl.take() };
        if ctrl {
            l.ctrl = None;
        } else {
            l.intr = None;
        }
        if let Some(o) = other {
            host.l2cap_disconnect(handle, o.cid, now);
        }
        self.device_closed(handle);
    }

    /// Opens what is missing, and attaches the device once it is complete.
    fn advance(&mut self, host: &mut Host, handle: u16, now: u64) {
        let Some(l) = self.links.get_mut(&handle) else { return };
        if !l.sdp_done {
            return;
        }
        if l.incoming {
            return self.attach(handle);
        }
        // The control channel first, then interrupt (HID 1.1.1 §5.2.2).
        if l.ctrl.is_none() {
            match host.l2cap_connect(handle, PSM_CONTROL, now) {
                Ok(cid) => l.ctrl = Some(Chan { cid, open: false }),
                Err(e) => eprintln!("vinculumd: {}: HID control channel: {e}", l.addr),
            }
            return;
        }
        if l.ctrl.is_some_and(|c| c.open) && l.intr.is_none() {
            match host.l2cap_connect(handle, PSM_INTERRUPT, now) {
                Ok(cid) => l.intr = Some(Chan { cid, open: false }),
                Err(e) => eprintln!("vinculumd: {}: HID interrupt channel: {e}", l.addr),
            }
            return;
        }
        self.attach(handle);
    }

    fn ready(l: &Link) -> bool {
        l.descriptor.is_some() && l.ctrl.is_some_and(|c| c.open) && l.intr.is_some_and(|c| c.open)
    }

    /// Hands a complete device to tactusd, if it is connected.
    fn attach(&mut self, handle: u16) {
        if !self.tactus.as_ref().is_some_and(|t| t.hello) {
            return;
        }
        let Some(l) = self.links.get_mut(&handle) else { return };
        if !Self::ready(l) || l.device.is_some() {
            return;
        }
        let (ours, theirs) = match bt::seqpacket_pair() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("vinculumd: socketpair: {e}");
                return;
            }
        };
        let id = self.next_device;
        self.next_device += 1;
        let msgs = ToTactus::attach(id, l.record.clone(), bt::Transport::Classic, l.keyboard_allowed, l.descriptor.as_deref().expect("ready"));
        eprintln!("vinculumd: {}: HID device {id} {:?} to tactusd", l.addr, l.record.name);
        l.device = Some(Device { id, fd: ours, dropped: 0 });
        let t = self.tactus.as_mut().expect("checked");
        let mut ok = bt::send_with_fd(&t.sock, &bt::encode(&msgs[0]), theirs.as_raw_fd()).is_ok();
        for m in &msgs[1..] {
            ok = ok && t.sock.write_all(&bt::encode(m)).is_ok();
        }
        if !ok {
            eprintln!("vinculumd: tactusd too slow or gone; dropped");
            self.tactus_gone();
        }
    }

    pub fn fds(&self) -> Vec<(RawFd, Fd)> {
        let mut v = Vec::new();
        if let Some(l) = &self.listener {
            v.push((l.as_raw_fd(), Fd::Listener));
        }
        if let Some(t) = &self.tactus {
            v.push((t.sock.as_raw_fd(), Fd::Tactus));
        }
        for (h, l) in &self.links {
            if let Some(d) = &l.device {
                v.push((d.fd.as_raw_fd(), Fd::Device(*h)));
            }
        }
        v
    }

    pub fn readable(&mut self, host: &mut Host, which: Fd, now: u64) {
        match which {
            Fd::Listener => self.accept(),
            Fd::Tactus => self.tactus_readable(),
            Fd::Device(h) => self.device_readable(host, h, now),
        }
    }

    fn accept(&mut self) {
        let Some(listener) = &self.listener else { return };
        let Ok((sock, _)) = listener.accept() else { return };
        let Some(uid) = crate::daemon::peer_uid(&sock) else { return };
        if uid != crate::daemon::getuid() && !self.allowed_uids.contains(&uid) {
            eprintln!("vinculumd: HID client uid {uid} refused");
            return;
        }
        if self.tactus.is_some() {
            eprintln!("vinculumd: HID client refused: tactusd is already connected");
            return;
        }
        let _ = sock.set_write_timeout(Some(Duration::from_millis(500)));
        self.tactus = Some(Tactus { sock, reader: bt::Reader::default(), hello: false });
    }

    fn tactus_readable(&mut self) {
        let Some(t) = &mut self.tactus else { return };
        let mut buf = [0u8; 1024];
        let n = match t.sock.read(&mut buf) {
            Ok(0) => return self.tactus_gone(),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => return,
            Err(_) => return self.tactus_gone(),
        };
        t.reader.push(&buf[..n]);
        // Hello is tactusd's only message, sent once.
        match t.reader.next::<ToVinculum>() {
            Ok(None) => {}
            Ok(Some(ToVinculum::Hello { version })) if version == bt::VERSION && !t.hello => {
                t.hello = true;
                eprintln!("vinculumd: tactusd connected");
                let handles: Vec<u16> = self.links.keys().copied().collect();
                for h in handles {
                    self.attach(h);
                }
            }
            _ => {
                eprintln!("vinculumd: bad message on the HID socket; client dropped");
                self.tactus_gone();
            }
        }
    }

    fn device_readable(&mut self, host: &mut Host, handle: u16, now: u64) {
        let Some(l) = self.links.get_mut(&handle) else { return };
        let Some(d) = &l.device else { return };
        let mut buf = vec![0u8; bt::MAX_PACKET];
        // SAFETY: buf is valid for writes of its length; the fd is ours.
        let n = unsafe { libc::recv(d.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n == 0 {
            return self.device_closed(handle);
        }
        if n < 0 {
            return;
        }
        let Some((h, report)) = bt::parse_packet(&buf[..n as usize]) else { return };
        let (header, cid) = match h.kind {
            Kind::Output => (hidp::DATA_OUTPUT, l.intr),
            Kind::FeatureSet => (hidp::SET_REPORT_FEATURE, l.ctrl),
            Kind::FeatureGet => (hidp::GET_REPORT_FEATURE, l.ctrl),
            _ => return,
        };
        let Some(c) = cid.filter(|c| c.open) else { return };
        let mut pdu = vec![header];
        if h.report_id != 0 {
            pdu.push(h.report_id);
        }
        if h.kind != Kind::FeatureGet {
            pdu.extend_from_slice(report);
        }
        if let Err(e) = host.l2cap_send(handle, c.cid, &pdu, now) {
            eprintln!("vinculumd: {}: report to device: {e}", l.addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_ids() {
        // Boot keyboard: no report ids.
        let kbd = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0];
        assert!(!has_report_ids(&kbd));
        let with = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0xC0];
        assert!(has_report_ids(&with));
        // A 0x85 inside another item's data is not an item.
        assert!(!has_report_ids(&[0x26, 0x85, 0x00]));
        // Long items are skipped whole; truncation is not a report id.
        assert!(!has_report_ids(&[0xFE, 2, 0x10, 0x85, 0x01]));
        assert!(!has_report_ids(&[0x26]));
    }
}

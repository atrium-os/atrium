//! ATT/GATT client and HID-over-GATT discovery, sans-I/O (Core 6.0 Vol 3
//! Parts F and G; HOGP 1.0; BAS; DIS).
//!
//! [`Hogp`] runs on the fixed ATT channel (CID 4) of an encrypted LE link
//! with a bond: it exchanges the MTU, discovers the primary services, and
//! for the HID service (0x1812) reads the Report Map and each Report's
//! Report Reference, then enables notifications on the input reports. From
//! Device Information (0x180A) it reads the PnP ID, from Battery (0x180F)
//! the level, with notifications when offered. When everything is known it
//! emits [`Out::Ready`]; after that, notifications become [`Out::Report`]
//! and [`Out::Battery`].
//!
//! The peer is untrusted: every response is length-checked, the counts of
//! services, characteristics and descriptors are capped, the Report Map is
//! at most [`MAX_REPORT_MAP`] bytes, and a response to a request we did not
//! send is a protocol error. Requests the peer sends us (we have no GATT
//! database worth showing it) get an MTU answer or an Error Response. It
//! is fuzzed (`fuzz/fuzz_targets/hogp.rs`).
#![forbid(unsafe_code)]

use std::collections::VecDeque;

pub const ATT_CID: u16 = 0x0004;
/// Our ATT MTU (Exchange MTU).
pub const OUR_MTU: u16 = 247;
/// The default LE ATT MTU before an exchange.
pub const DEFAULT_MTU: u16 = 23;
/// HOGP 1.0 §2.6.1: a Report Map is at most 512 bytes.
pub const MAX_REPORT_MAP: usize = 512;
pub const MAX_SERVICES: usize = 32;
pub const MAX_CHARS: usize = 96;
pub const MAX_DESCS: usize = 192;
/// ATT transaction timeout (Vol 3 Part F §3.3.3): the link is then unusable
/// for ATT, and the caller drops it.
pub const ATT_TIMEOUT_MS: u64 = 30_000;

pub mod op {
    pub const ERROR_RSP: u8 = 0x01;
    pub const MTU_REQ: u8 = 0x02;
    pub const MTU_RSP: u8 = 0x03;
    pub const FIND_INFO_REQ: u8 = 0x04;
    pub const FIND_INFO_RSP: u8 = 0x05;
    pub const READ_BY_TYPE_REQ: u8 = 0x08;
    pub const READ_BY_TYPE_RSP: u8 = 0x09;
    pub const READ_REQ: u8 = 0x0A;
    pub const READ_RSP: u8 = 0x0B;
    pub const READ_BLOB_REQ: u8 = 0x0C;
    pub const READ_BLOB_RSP: u8 = 0x0D;
    pub const READ_BY_GROUP_REQ: u8 = 0x10;
    pub const READ_BY_GROUP_RSP: u8 = 0x11;
    pub const WRITE_REQ: u8 = 0x12;
    pub const WRITE_RSP: u8 = 0x13;
    pub const NOTIFY: u8 = 0x1B;
    pub const INDICATE: u8 = 0x1D;
    pub const CONFIRM: u8 = 0x1E;
    pub const WRITE_CMD: u8 = 0x52;
}

pub mod err {
    pub const INVALID_HANDLE: u8 = 0x01;
    pub const REQUEST_NOT_SUPPORTED: u8 = 0x06;
    pub const ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
    pub const ATTRIBUTE_NOT_LONG: u8 = 0x0B;
}

pub mod uuid {
    pub const PRIMARY_SERVICE: u16 = 0x2800;
    pub const CHARACTERISTIC: u16 = 0x2803;
    pub const HID: u16 = 0x1812;
    pub const DEVICE_INFO: u16 = 0x180A;
    pub const BATTERY: u16 = 0x180F;
    pub const REPORT_MAP: u16 = 0x2A4B;
    pub const REPORT: u16 = 0x2A4D;
    pub const PNP_ID: u16 = 0x2A50;
    pub const BATTERY_LEVEL: u16 = 0x2A19;
    pub const REPORT_REFERENCE: u16 = 0x2908;
    pub const CCCD: u16 = 0x2902;
}

/// Characteristic properties.
pub mod prop {
    pub const READ: u8 = 0x02;
    pub const WRITE_NO_RSP: u8 = 0x04;
    pub const WRITE: u8 = 0x08;
    pub const NOTIFY: u8 = 0x10;
}

/// Report Reference types (HIDS 1.0 §3.6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    Input,
    Output,
    Feature,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    pub report_map: Vec<u8>,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub battery: Option<u8>,
    /// Input reports: value handle → report id (0 when the map has none).
    pub inputs: Vec<(u16, u8)>,
    /// Output reports: report id → value handle.
    pub outputs: Vec<(u8, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// An ATT PDU to send on CID 4.
    Send(Vec<u8>),
    /// Discovery is done and notifications are on.
    Ready(Device),
    Report { id: u8, data: Vec<u8> },
    Battery(u8),
    /// Not a HID device, or it broke the protocol: stop using ATT on this
    /// link (the caller may disconnect).
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Service {
    start: u16,
    end: u16,
    uuid: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Char {
    decl: u16,
    props: u8,
    value: u16,
    uuid: u16,
    /// Last handle of its descriptors.
    end: u16,
    service: u16,
    cccd: Option<u16>,
    reference: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    ReportMap,
    Reference(usize),
    Pnp,
    Battery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Mtu,
    Services { from: u16 },
    Chars { svc: usize, from: u16 },
    Descs { ch: usize, from: u16 },
    Read { target: Target, handle: u16, offset: u16 },
    Write { handle: u16, value: Vec<u8> },
    /// No request: plan the next stage once the ones before are answered.
    Stage(Stage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Characteristics are in: discover the descriptors we need.
    Descriptors,
    /// Descriptors are in: read the Report Map, references, PnP, battery.
    Reads,
    /// References are in: enable input notifications.
    Notifications,
}

/// One HID-over-GATT session on one link.
pub struct Hogp {
    mtu: u16,
    queue: VecDeque<Op>,
    /// The request in flight, and when it was sent.
    pending: Option<(Op, u64)>,
    services: Vec<Service>,
    chars: Vec<Char>,
    descs: usize,
    dev: Device,
    /// Report references read, per characteristic index.
    refs: Vec<(usize, u8, ReportType)>,
    ready: bool,
    failed: bool,
    battery_handle: Option<u16>,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

/// 16-bit form of a UUID given as 2 or 16 bytes (LE); None for a 128-bit
/// UUID off the Bluetooth base.
fn uuid16(b: &[u8]) -> Option<u16> {
    const BASE: [u8; 12] = [0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00];
    match b.len() {
        2 => Some(u16le(b, 0)),
        16 if b[..12] == BASE && b[14] == 0 && b[15] == 0 => Some(u16le(b, 12)),
        _ => None,
    }
}

impl Default for Hogp {
    fn default() -> Self {
        Self::new()
    }
}

impl Hogp {
    pub fn new() -> Hogp {
        Hogp {
            mtu: DEFAULT_MTU,
            queue: VecDeque::from([Op::Mtu, Op::Services { from: 1 }]),
            pending: None,
            services: Vec::new(),
            chars: Vec::new(),
            descs: 0,
            dev: Device::default(),
            refs: Vec::new(),
            ready: false,
            failed: false,
            battery_handle: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Sends the first request.
    pub fn start(&mut self, now_ms: u64) -> Vec<Out> {
        let mut out = Vec::new();
        self.pump(now_ms, &mut out);
        out
    }

    /// When the request in flight times out (then the caller drops the link).
    pub fn deadline(&self) -> Option<u64> {
        self.pending.as_ref().map(|(_, t)| t + ATT_TIMEOUT_MS)
    }

    /// An output report for the device: a Write Command on its Report
    /// value (HOGP: Output Reports take Write Without Response).
    pub fn output(&self, id: u8, data: &[u8]) -> Option<Vec<u8>> {
        let &(_, handle) = self.dev.outputs.iter().find(|(i, _)| *i == id)?;
        if data.len() + 3 > self.mtu as usize {
            return None;
        }
        let mut p = vec![op::WRITE_CMD];
        p.extend_from_slice(&handle.to_le_bytes());
        p.extend_from_slice(data);
        Some(p)
    }

    fn fail(&mut self, why: impl Into<String>, out: &mut Vec<Out>) {
        if !self.failed {
            self.failed = true;
            self.queue.clear();
            self.pending = None;
            out.push(Out::Failed(why.into()));
        }
    }

    fn pump(&mut self, now_ms: u64, out: &mut Vec<Out>) {
        if self.failed || self.pending.is_some() {
            return;
        }
        let o = loop {
            match self.queue.pop_front() {
                None => {
                    if !self.ready {
                        self.finish(out);
                    }
                    return;
                }
                Some(Op::Stage(st)) => {
                    let r = match st {
                        Stage::Descriptors => self.plan(),
                        Stage::Reads => self.plan_reads(),
                        Stage::Notifications => self.plan_notifications(),
                    };
                    if let Err(e) = r {
                        return self.fail(e, out);
                    }
                }
                Some(o) => break o,
            }
        };
        let pdu = match &o {
            Op::Mtu => {
                let mut p = vec![op::MTU_REQ];
                p.extend_from_slice(&OUR_MTU.to_le_bytes());
                p
            }
            Op::Services { from } => req_range(op::READ_BY_GROUP_REQ, *from, 0xFFFF, Some(uuid::PRIMARY_SERVICE)),
            Op::Chars { svc, from } => req_range(op::READ_BY_TYPE_REQ, *from, self.services[*svc].end, Some(uuid::CHARACTERISTIC)),
            Op::Descs { ch, from } => req_range(op::FIND_INFO_REQ, *from, self.chars[*ch].end, None),
            Op::Read { handle, offset: 0, .. } => {
                let mut p = vec![op::READ_REQ];
                p.extend_from_slice(&handle.to_le_bytes());
                p
            }
            Op::Read { handle, offset, .. } => {
                let mut p = vec![op::READ_BLOB_REQ];
                p.extend_from_slice(&handle.to_le_bytes());
                p.extend_from_slice(&offset.to_le_bytes());
                p
            }
            Op::Write { handle, value } => {
                let mut p = vec![op::WRITE_REQ];
                p.extend_from_slice(&handle.to_le_bytes());
                p.extend_from_slice(value);
                p
            }
            Op::Stage(_) => unreachable!("handled above"),
        };
        self.pending = Some((o, now_ms));
        out.push(Out::Send(pdu));
    }

    /// An ATT PDU from the peer.
    pub fn on_pdu(&mut self, pdu: &[u8], now_ms: u64) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(&code) = pdu.first() else { return out };
        match code {
            op::NOTIFY | op::INDICATE => {
                if code == op::INDICATE {
                    out.push(Out::Send(vec![op::CONFIRM]));
                }
                if pdu.len() >= 3 {
                    self.notification(u16le(pdu, 1), &pdu[3..], &mut out);
                }
            }
            // Requests to our (empty) server.
            op::MTU_REQ if pdu.len() == 3 => {
                let mut p = vec![op::MTU_RSP];
                p.extend_from_slice(&OUR_MTU.to_le_bytes());
                out.push(Out::Send(p));
                // The MTU is the smaller of the two sides' (§3.4.2.2).
                self.mtu = u16le(pdu, 1).clamp(DEFAULT_MTU, OUR_MTU);
            }
            // Commands (bit 6) and signed writes get no response.
            c if c & 0x40 != 0 => {}
            c if is_request(c) => {
                let mut p = vec![op::ERROR_RSP, c];
                let handle = if pdu.len() >= 3 { u16le(pdu, 1) } else { 0 };
                p.extend_from_slice(&handle.to_le_bytes());
                let discovery = matches!(c, op::FIND_INFO_REQ | 0x06 | op::READ_BY_TYPE_REQ | op::READ_BY_GROUP_REQ);
                p.push(if discovery { err::ATTRIBUTE_NOT_FOUND } else if c == op::READ_REQ || c == op::READ_BLOB_REQ { err::INVALID_HANDLE } else { err::REQUEST_NOT_SUPPORTED });
                out.push(Out::Send(p));
            }
            _ => self.response(pdu, now_ms, &mut out),
        }
        out
    }

    fn notification(&mut self, handle: u16, value: &[u8], out: &mut Vec<Out>) {
        if !self.ready {
            // Before discovery is done we don't know what it is.
            return;
        }
        if let Some(&(_, id)) = self.dev.inputs.iter().find(|(h, _)| *h == handle) {
            out.push(Out::Report { id, data: value.to_vec() });
        } else if Some(handle) == self.battery_handle {
            if let Some(&pct) = value.first() {
                out.push(Out::Battery(pct.min(100)));
            }
        }
    }

    fn response(&mut self, pdu: &[u8], now_ms: u64, out: &mut Vec<Out>) {
        let Some((o, _)) = self.pending.take() else {
            return self.fail("ATT response with no request outstanding", out);
        };
        let code = pdu[0];
        let error = if code == op::ERROR_RSP {
            if pdu.len() != 5 {
                return self.fail("malformed ATT Error Response", out);
            }
            Some(pdu[4])
        } else {
            None
        };
        match o {
            Op::Mtu => match (code, error) {
                (op::MTU_RSP, _) if pdu.len() == 3 => {
                    self.mtu = u16le(pdu, 1).clamp(DEFAULT_MTU, OUR_MTU);
                }
                // A server that can't change it keeps the default.
                (_, Some(_)) => {}
                _ => return self.fail("bad Exchange MTU response", out),
            },
            Op::Services { .. } => match error {
                Some(err::ATTRIBUTE_NOT_FOUND) => self.services_done(),
                Some(e) => return self.fail(format!("service discovery: ATT error {e:#x}"), out),
                None => {
                    if code != op::READ_BY_GROUP_RSP || pdu.len() < 2 {
                        return self.fail("bad Read By Group Type response", out);
                    }
                    let len = pdu[1] as usize;
                    let data = &pdu[2..];
                    if (len != 6 && len != 20) || data.is_empty() || data.len() % len != 0 {
                        return self.fail("bad Read By Group Type response", out);
                    }
                    let mut last = 0;
                    for e in data.chunks(len) {
                        let (start, end) = (u16le(e, 0), u16le(e, 2));
                        if start == 0 || end < start || start <= last {
                            return self.fail("service handles out of order", out);
                        }
                        last = end;
                        if self.services.len() >= MAX_SERVICES {
                            return self.fail("too many services", out);
                        }
                        if let Some(u) = uuid16(&e[4..]) {
                            self.services.push(Service { start, end, uuid: u });
                        }
                    }
                    if last == 0xFFFF {
                        self.services_done();
                    } else {
                        self.queue.push_front(Op::Services { from: last + 1 });
                    }
                }
            },
            Op::Chars { svc, from } => match error {
                Some(err::ATTRIBUTE_NOT_FOUND) => {}
                Some(e) => return self.fail(format!("characteristic discovery: ATT error {e:#x}"), out),
                None => {
                    if code != op::READ_BY_TYPE_RSP || pdu.len() < 2 {
                        return self.fail("bad Read By Type response", out);
                    }
                    let len = pdu[1] as usize;
                    let data = &pdu[2..];
                    if (len != 7 && len != 21) || data.is_empty() || data.len() % len != 0 {
                        return self.fail("bad Read By Type response", out);
                    }
                    let s = self.services[svc];
                    let mut last = from - 1;
                    for e in data.chunks(len) {
                        let decl = u16le(e, 0);
                        let (props, value) = (e[2], u16le(e, 3));
                        if decl <= last || decl < s.start || decl > s.end || value <= decl || value > s.end {
                            return self.fail("characteristic handles out of range", out);
                        }
                        last = decl;
                        if self.chars.len() >= MAX_CHARS {
                            return self.fail("too many characteristics", out);
                        }
                        // A characteristic's descriptors run to the next
                        // declaration (fixed up below) or the service end.
                        if let Some(prev) = self.chars.last_mut().filter(|c| c.service == s.start) {
                            prev.end = decl - 1;
                        }
                        let u = uuid16(&e[5..]).unwrap_or(0);
                        self.chars.push(Char { decl, props, value, uuid: u, end: s.end, service: s.start, cccd: None, reference: None });
                    }
                    if last < s.end {
                        self.queue.push_front(Op::Chars { svc, from: last + 1 });
                    }
                }
            },
            Op::Descs { ch, .. } => match error {
                Some(err::ATTRIBUTE_NOT_FOUND) => {}
                Some(e) => return self.fail(format!("descriptor discovery: ATT error {e:#x}"), out),
                None => {
                    if code != op::FIND_INFO_RSP || pdu.len() < 2 {
                        return self.fail("bad Find Information response", out);
                    }
                    let len = match pdu[1] {
                        1 => 4,
                        2 => 18,
                        _ => return self.fail("bad Find Information format", out),
                    };
                    let data = &pdu[2..];
                    if data.is_empty() || data.len() % len != 0 {
                        return self.fail("bad Find Information response", out);
                    }
                    let c = self.chars[ch];
                    let mut last = c.value;
                    for e in data.chunks(len) {
                        let h = u16le(e, 0);
                        if h <= last || h > c.end {
                            return self.fail("descriptor handles out of range", out);
                        }
                        last = h;
                        self.descs += 1;
                        if self.descs > MAX_DESCS {
                            return self.fail("too many descriptors", out);
                        }
                        match uuid16(&e[2..]) {
                            Some(uuid::CCCD) => self.chars[ch].cccd = Some(h),
                            Some(uuid::REPORT_REFERENCE) => self.chars[ch].reference = Some(h),
                            _ => {}
                        }
                    }
                    if last < c.end {
                        self.queue.push_front(Op::Descs { ch, from: last + 1 });
                    }
                }
            },
            Op::Read { target, handle, offset } => {
                let value = match (code, error) {
                    (op::READ_RSP, _) if offset == 0 => &pdu[1..],
                    (op::READ_BLOB_RSP, _) if offset > 0 => &pdu[1..],
                    // The end of a long value read exactly at a boundary.
                    (_, Some(err::ATTRIBUTE_NOT_LONG | err::INVALID_HANDLE)) if offset > 0 => &[][..],
                    (_, Some(e)) => return self.fail(format!("read {handle:#x}: ATT error {e:#x}"), out),
                    _ => return self.fail("bad read response", out),
                };
                let full = value.len() == self.mtu as usize - 1;
                if self.read_value(target, value, out).is_err() {
                    return;
                }
                // A full response means there may be more (Read Blob).
                if full && target == Target::ReportMap {
                    let next = offset as usize + value.len();
                    if next >= MAX_REPORT_MAP {
                        return self.fail("Report Map over 512 bytes", out);
                    }
                    self.queue.push_front(Op::Read { target, handle, offset: next as u16 });
                }
            }
            Op::Stage(_) => unreachable!("never pending"),
            Op::Write { .. } => match (code, error) {
                (op::WRITE_RSP, _) if pdu.len() == 1 => {}
                (_, Some(e)) => return self.fail(format!("enabling notifications: ATT error {e:#x}"), out),
                _ => return self.fail("bad Write response", out),
            },
        }
        self.pump(now_ms, out);
    }

    fn services_done(&mut self) {
        // Characteristics of the services we use, in order.
        let mut ops: Vec<Op> = Vec::new();
        for (i, s) in self.services.iter().enumerate() {
            if matches!(s.uuid, uuid::HID | uuid::DEVICE_INFO | uuid::BATTERY) {
                ops.push(Op::Chars { svc: i, from: s.start });
            }
        }
        ops.push(Op::Stage(Stage::Descriptors));
        for o in ops.into_iter().rev() {
            self.queue.push_front(o);
        }
    }

    /// Plans descriptor discovery and reads for what the services hold.
    fn plan(&mut self) -> Result<(), String> {
        if !self.services.iter().any(|s| s.uuid == uuid::HID) {
            return Err("no HID service".into());
        }
        let mut ops = Vec::new();
        for (i, c) in self.chars.iter().enumerate() {
            let svc = self.services.iter().find(|s| s.start == c.service).map(|s| s.uuid);
            let wanted = matches!((svc, c.uuid), (Some(uuid::HID), uuid::REPORT) | (Some(uuid::BATTERY), uuid::BATTERY_LEVEL));
            if wanted && c.end > c.value {
                ops.push(Op::Descs { ch: i, from: c.value + 1 });
            }
        }
        ops.push(Op::Stage(Stage::Reads));
        for o in ops.into_iter().rev() {
            self.queue.push_front(o);
        }
        Ok(())
    }

    /// After descriptors: the reads, then the notification writes.
    fn plan_reads(&mut self) -> Result<(), String> {
        let hid = |c: &Char, svcs: &[Service], u| svcs.iter().any(|s| s.start == c.service && s.uuid == u);
        let map = self.chars.iter().find(|c| c.uuid == uuid::REPORT_MAP && hid(c, &self.services, uuid::HID)).ok_or("no Report Map")?;
        let mut ops = vec![Op::Read { target: Target::ReportMap, handle: map.value, offset: 0 }];
        for (i, c) in self.chars.iter().enumerate() {
            if c.uuid == uuid::REPORT && hid(c, &self.services, uuid::HID) {
                if let Some(r) = c.reference {
                    ops.push(Op::Read { target: Target::Reference(i), handle: r, offset: 0 });
                }
            }
            if c.uuid == uuid::PNP_ID && hid(c, &self.services, uuid::DEVICE_INFO) && c.props & prop::READ != 0 {
                ops.push(Op::Read { target: Target::Pnp, handle: c.value, offset: 0 });
            }
            if c.uuid == uuid::BATTERY_LEVEL && hid(c, &self.services, uuid::BATTERY) && c.props & prop::READ != 0 {
                ops.push(Op::Read { target: Target::Battery, handle: c.value, offset: 0 });
                if c.props & prop::NOTIFY != 0 {
                    if let Some(cccd) = c.cccd {
                        self.battery_handle = Some(c.value);
                        ops.push(Op::Write { handle: cccd, value: vec![1, 0] });
                    }
                }
            }
        }
        ops.push(Op::Stage(Stage::Notifications));
        for o in ops.into_iter().rev() {
            self.queue.push_front(o);
        }
        Ok(())
    }

    /// After the reads: which reports are which; enable input notifications.
    fn plan_notifications(&mut self) -> Result<(), String> {
        let mut ops = Vec::new();
        for &(i, id, ty) in &self.refs {
            let c = self.chars[i];
            match ty {
                ReportType::Input => {
                    if c.props & prop::NOTIFY == 0 {
                        continue;
                    }
                    let cccd = c.cccd.ok_or("input report without a CCCD")?;
                    self.dev.inputs.push((c.value, id));
                    ops.push(Op::Write { handle: cccd, value: vec![1, 0] });
                }
                ReportType::Output => self.dev.outputs.push((id, c.value)),
                ReportType::Feature => {}
            }
        }
        if self.dev.inputs.is_empty() {
            return Err("no input reports".into());
        }
        for o in ops.into_iter().rev() {
            self.queue.push_front(o);
        }
        Ok(())
    }

    fn read_value(&mut self, target: Target, v: &[u8], out: &mut Vec<Out>) -> Result<(), ()> {
        match target {
            Target::ReportMap => {
                if self.dev.report_map.len() + v.len() > MAX_REPORT_MAP {
                    self.fail("Report Map over 512 bytes", out);
                    return Err(());
                }
                self.dev.report_map.extend_from_slice(v);
            }
            Target::Reference(i) => {
                if v.len() != 2 {
                    self.fail("bad Report Reference", out);
                    return Err(());
                }
                let ty = match v[1] {
                    1 => ReportType::Input,
                    2 => ReportType::Output,
                    3 => ReportType::Feature,
                    _ => {
                        self.fail("bad Report Reference type", out);
                        return Err(());
                    }
                };
                self.refs.push((i, v[0], ty));
            }
            Target::Pnp => {
                if v.len() == 7 {
                    self.dev.vendor = u16le(v, 1);
                    self.dev.product = u16le(v, 3);
                    self.dev.version = u16le(v, 5);
                }
            }
            Target::Battery => self.dev.battery = v.first().map(|p| (*p).min(100)),
        }
        Ok(())
    }

    /// Every stage is done.
    fn finish(&mut self, out: &mut Vec<Out>) {
        self.ready = true;
        out.push(Out::Ready(self.dev.clone()));
    }
}

fn is_request(c: u8) -> bool {
    matches!(c, 0x02 | 0x04 | 0x06 | 0x08 | 0x0A | 0x0C | 0x0E | 0x10 | 0x12 | 0x16 | 0x18 | 0x20)
}

fn req_range(code: u8, start: u16, end: u16, ty: Option<u16>) -> Vec<u8> {
    let mut p = vec![code];
    p.extend_from_slice(&start.to_le_bytes());
    p.extend_from_slice(&end.to_le_bytes());
    if let Some(t) = ty {
        p.extend_from_slice(&t.to_le_bytes());
    }
    p
}

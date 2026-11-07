//! L2CAP connection-oriented channels on BR/EDR, basic mode (Core 6.0
//! Vol 3 Part A §4, §5). Enough for SDP and Classic HID; the LE credit-based
//! channels come with apps (spec §12).
//!
//! Rules (spec §7):
//! - **No profile channel on an unbonded link**: an incoming Connection
//!   Request is refused with "security block" unless the link is encrypted
//!   (which only a bond can do), and "PSM not supported" unless the caller
//!   registered that PSM.
//! - Bounded: at most [`MAX_CHANNELS`] channels per link; signalling
//!   commands are parsed with every length checked; anything malformed is
//!   rejected (Command Reject) and counted.
//! - Basic mode only: a configuration asking for another mode, or an MTU
//!   under the minimum 48, is refused.
//! - Pending connects and configurations time out after
//!   [`SIGNAL_TIMEOUT_MS`].

use crate::{Host, HostEvent, Transport};

pub const SIGNALING_CID: u16 = 0x0001;
pub const MAX_CHANNELS: usize = 8;
pub const SIGNAL_TIMEOUT_MS: u64 = 10_000;
/// The MTU we accept on our channels.
pub const OUR_MTU: u16 = 672;
const MIN_MTU: u16 = 48;
const FIRST_DYNAMIC: u16 = 0x0040;

mod code {
    pub const COMMAND_REJECT: u8 = 0x01;
    pub const CONN_REQ: u8 = 0x02;
    pub const CONN_RSP: u8 = 0x03;
    pub const CONF_REQ: u8 = 0x04;
    pub const CONF_RSP: u8 = 0x05;
    pub const DISC_REQ: u8 = 0x06;
    pub const DISC_RSP: u8 = 0x07;
    pub const ECHO_REQ: u8 = 0x08;
    pub const ECHO_RSP: u8 = 0x09;
    pub const INFO_REQ: u8 = 0x0A;
    pub const INFO_RSP: u8 = 0x0B;
}

mod result {
    pub const SUCCESS: u16 = 0x0000;
    pub const PENDING: u16 = 0x0001;
    pub const PSM_NOT_SUPPORTED: u16 = 0x0002;
    pub const SECURITY_BLOCK: u16 = 0x0003;
    pub const NO_RESOURCES: u16 = 0x0004;
    pub const INVALID_SCID: u16 = 0x0006;
    pub const SCID_IN_USE: u16 = 0x0007;
}

mod conf {
    pub const SUCCESS: u16 = 0x0000;
    pub const UNACCEPTABLE: u16 = 0x0001;
    pub const REJECTED: u16 = 0x0002;
    pub const UNKNOWN_OPTIONS: u16 = 0x0003;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// We sent Connection Request.
    WaitConnect,
    /// Connected; configuring both directions.
    Config,
    Open,
    /// We sent Disconnection Request.
    Closing,
}

#[derive(Debug)]
pub(crate) struct Channel {
    pub local: u16,
    pub remote: u16,
    pub psm: u16,
    state: State,
    /// Our Configuration Request was accepted.
    our_conf_done: bool,
    /// We accepted the peer's Configuration Request.
    their_conf_done: bool,
    /// The peer's MTU: the most we may send on this channel.
    pub peer_mtu: u16,
    deadline: Option<u64>,
    ident: u8,
}

#[derive(Debug, Default)]
pub(crate) struct Signaling {
    pub channels: Vec<Channel>,
    next_ident: u8,
}

impl Signaling {
    fn ident(&mut self) -> u8 {
        self.next_ident = self.next_ident.wrapping_add(1);
        if self.next_ident == 0 {
            self.next_ident = 1;
        }
        self.next_ident
    }

    fn free_cid(&self) -> Option<u16> {
        (FIRST_DYNAMIC..FIRST_DYNAMIC + 64).find(|c| !self.channels.iter().any(|ch| ch.local == *c))
    }

    pub fn deadline(&self) -> Option<u64> {
        self.channels.iter().filter_map(|c| c.deadline).min()
    }
}

fn cmd(code: u8, ident: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![code, ident];
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn mtu_option(mtu: u16) -> Vec<u8> {
    let mut v = vec![0x01, 2];
    v.extend_from_slice(&mtu.to_le_bytes());
    v
}

impl Host {
    /// PSMs incoming channels may open (on encrypted links only).
    pub fn register_psm(&mut self, psm: u16) {
        if !self.psms.contains(&psm) {
            self.psms.push(psm);
        }
    }

    /// Opens a channel to `psm`; `ChannelOpen` or `ChannelFailed` follows.
    /// Returns our channel id.
    pub fn l2cap_connect(&mut self, handle: u16, psm: u16, now_ms: u64) -> Result<u16, &'static str> {
        let c = self.conns.get_mut(&handle).ok_or("no such connection")?;
        if c.transport != Transport::BrEdr {
            return Err("dynamic channels are BR/EDR only for now");
        }
        if c.sig.channels.len() >= MAX_CHANNELS {
            return Err("too many channels");
        }
        let local = c.sig.free_cid().ok_or("no channel id")?;
        let ident = c.sig.ident();
        c.sig.channels.push(Channel {
            local,
            remote: 0,
            psm,
            state: State::WaitConnect,
            our_conf_done: false,
            their_conf_done: false,
            peer_mtu: MIN_MTU,
            deadline: Some(now_ms + SIGNAL_TIMEOUT_MS),
            ident,
        });
        let mut d = psm.to_le_bytes().to_vec();
        d.extend_from_slice(&local.to_le_bytes());
        self.sig_send(handle, &cmd(code::CONN_REQ, ident, &d));
        self.pump(now_ms);
        Ok(local)
    }

    /// Sends one SDU on an open channel (at most the peer's MTU).
    pub fn l2cap_send(&mut self, handle: u16, cid: u16, payload: &[u8], now_ms: u64) -> Result<(), &'static str> {
        let c = self.conns.get(&handle).ok_or("no such connection")?;
        let ch = c.sig.channels.iter().find(|ch| ch.local == cid && ch.state == State::Open).ok_or("channel not open")?;
        if payload.len() > ch.peer_mtu as usize {
            return Err("over the peer's MTU");
        }
        let remote = ch.remote;
        self.send_l2cap(handle, remote, payload, now_ms)
    }

    pub fn l2cap_disconnect(&mut self, handle: u16, cid: u16, now_ms: u64) {
        let Some(c) = self.conns.get_mut(&handle) else { return };
        let ident = c.sig.ident();
        let Some(ch) = c.sig.channels.iter_mut().find(|ch| ch.local == cid) else { return };
        if ch.state == State::WaitConnect {
            // No remote id yet: just forget it (a late response is refused).
            c.sig.channels.retain(|ch| ch.local != cid);
            return;
        }
        ch.state = State::Closing;
        ch.ident = ident;
        ch.deadline = Some(now_ms + SIGNAL_TIMEOUT_MS);
        let mut d = ch.remote.to_le_bytes().to_vec();
        d.extend_from_slice(&ch.local.to_le_bytes());
        self.sig_send(handle, &cmd(code::DISC_REQ, ident, &d));
        self.pump(now_ms);
    }

    fn sig_send(&mut self, handle: u16, c: &[u8]) {
        let _ = self.queue_l2cap(handle, SIGNALING_CID, c);
    }

    fn sig_violation(&mut self, handle: u16) {
        self.stats.link_violations += 1;
        if let Some(c) = self.conns.get_mut(&handle) {
            c.violations += 1;
            if c.violations >= crate::MAX_LINK_VIOLATIONS {
                self.kick(handle);
            }
        }
    }

    /// Data on a dynamic channel, or a signalling PDU.
    pub(crate) fn on_l2cap_pdu(&mut self, handle: u16, cid: u16, payload: Vec<u8>, now_ms: u64) {
        if cid == SIGNALING_CID {
            return self.on_signaling(handle, &payload, now_ms);
        }
        let open = self.conns.get(&handle).and_then(|c| c.sig.channels.iter().find(|ch| ch.local == cid)).map(|ch| ch.state);
        match open {
            Some(State::Open) => {
                if payload.len() > OUR_MTU as usize {
                    self.sig_violation(handle);
                    return;
                }
                self.out_events.push_back(HostEvent::ChannelData { handle, cid, payload });
            }
            // Data for a channel that isn't open, or doesn't exist: dropped.
            _ => self.sig_violation(handle),
        }
    }

    fn on_signaling(&mut self, handle: u16, mut p: &[u8], now_ms: u64) {
        while !p.is_empty() {
            if p.len() < 4 {
                return self.sig_violation(handle);
            }
            let (code, ident, len) = (p[0], p[1], u16le(p, 2) as usize);
            if p.len() < 4 + len {
                return self.sig_violation(handle);
            }
            let data = p[4..4 + len].to_vec();
            p = &p[4 + len..];
            self.on_command(handle, code, ident, &data, now_ms);
        }
    }

    fn reject(&mut self, handle: u16, ident: u8) {
        self.sig_violation(handle);
        self.sig_send(handle, &cmd(code::COMMAND_REJECT, ident, &[0, 0])); // not understood
    }

    fn on_command(&mut self, handle: u16, code: u8, ident: u8, d: &[u8], now_ms: u64) {
        let Some(c) = self.conns.get(&handle) else { return };
        let encrypted = c.encrypted;
        match code {
            code::CONN_REQ if d.len() == 4 => {
                let (psm, scid) = (u16le(d, 0), u16le(d, 2));
                let c = self.conns.get_mut(&handle).expect("present");
                let (dcid, res) = if !self.psms.contains(&psm) {
                    (0, result::PSM_NOT_SUPPORTED)
                } else if !encrypted {
                    (0, result::SECURITY_BLOCK)
                } else if scid < FIRST_DYNAMIC {
                    (0, result::INVALID_SCID)
                } else if c.sig.channels.iter().any(|ch| ch.remote == scid) {
                    (0, result::SCID_IN_USE)
                } else if c.sig.channels.len() >= MAX_CHANNELS {
                    (0, result::NO_RESOURCES)
                } else {
                    match c.sig.free_cid() {
                        Some(local) => {
                            c.sig.channels.push(Channel {
                                local,
                                remote: scid,
                                psm,
                                state: State::Config,
                                our_conf_done: false,
                                their_conf_done: false,
                                peer_mtu: 672, // the default until configured
                                deadline: Some(now_ms + SIGNAL_TIMEOUT_MS),
                                ident: 0,
                            });
                            (local, result::SUCCESS)
                        }
                        None => (0, result::NO_RESOURCES),
                    }
                };
                if res == result::SECURITY_BLOCK {
                    self.stats.link_violations += 1;
                }
                let mut r = dcid.to_le_bytes().to_vec();
                r.extend_from_slice(&scid.to_le_bytes());
                r.extend_from_slice(&res.to_le_bytes());
                r.extend_from_slice(&0u16.to_le_bytes());
                self.sig_send(handle, &cmd(code::CONN_RSP, ident, &r));
                if res == result::SUCCESS {
                    self.send_conf_req(handle, dcid);
                }
            }
            code::CONN_RSP if d.len() == 8 => {
                let (dcid, scid, res) = (u16le(d, 0), u16le(d, 2), u16le(d, 4));
                let c = self.conns.get_mut(&handle).expect("present");
                let Some(i) = c.sig.channels.iter().position(|ch| ch.local == scid && ch.state == State::WaitConnect && ch.ident == ident) else {
                    return self.sig_violation(handle);
                };
                match res {
                    result::SUCCESS if dcid >= FIRST_DYNAMIC => {
                        let ch = &mut c.sig.channels[i];
                        ch.remote = dcid;
                        ch.state = State::Config;
                        self.send_conf_req(handle, scid);
                    }
                    result::PENDING => {}
                    _ => {
                        let ch = c.sig.channels.remove(i);
                        self.out_events.push_back(HostEvent::ChannelFailed { handle, psm: ch.psm, why: format!("refused (result {res})") });
                    }
                }
            }
            code::CONF_REQ if d.len() >= 4 => {
                let (dcid, flags) = (u16le(d, 0), u16le(d, 2));
                let c = self.conns.get_mut(&handle).expect("present");
                let Some(ch) = c.sig.channels.iter_mut().find(|ch| ch.local == dcid && ch.state == State::Config) else {
                    return self.reject(handle, ident);
                };
                let remote = ch.remote;
                // Options: type (bit 7 = hint), length, value.
                let mut opts = &d[4..];
                let mut mtu = 672u16;
                let mut res = conf::SUCCESS;
                let mut unknown = Vec::new();
                while !opts.is_empty() {
                    if opts.len() < 2 || opts.len() < 2 + opts[1] as usize {
                        res = conf::REJECTED;
                        break;
                    }
                    let (t, l) = (opts[0], opts[1] as usize);
                    let v = &opts[2..2 + l];
                    match t & 0x7F {
                        0x01 if l == 2 => mtu = u16le(v, 0),
                        0x02 if l == 2 => {} // flush timeout: accepted as given
                        0x04 if l >= 1 && v[0] != 0 => res = conf::UNACCEPTABLE, // not basic mode
                        0x04 => {}
                        0x05 if l == 1 => {} // FCS option: irrelevant in basic mode
                        _ if t & 0x80 != 0 => {} // a hint we don't know: ignore
                        _ => {
                            unknown.push(t);
                            res = conf::UNKNOWN_OPTIONS;
                        }
                    }
                    opts = &opts[2 + l..];
                }
                if res == conf::SUCCESS && mtu < MIN_MTU {
                    res = conf::UNACCEPTABLE;
                }
                if flags & 1 != 0 {
                    // Continuation flag: we don't take split configurations.
                    res = conf::REJECTED;
                }
                let mut r = remote.to_le_bytes().to_vec();
                r.extend_from_slice(&0u16.to_le_bytes());
                r.extend_from_slice(&res.to_le_bytes());
                match res {
                    conf::UNACCEPTABLE => r.extend(mtu_option(mtu.max(MIN_MTU))),
                    conf::UNKNOWN_OPTIONS => unknown.iter().for_each(|t| r.extend_from_slice(&[*t, 0])),
                    _ => {}
                }
                if res == conf::SUCCESS {
                    ch.peer_mtu = mtu;
                    ch.their_conf_done = true;
                }
                self.sig_send(handle, &cmd(code::CONF_RSP, ident, &r));
                if res != conf::SUCCESS {
                    self.sig_violation(handle);
                }
                self.maybe_open(handle, dcid);
            }
            code::CONF_RSP if d.len() >= 6 => {
                let (scid, res) = (u16le(d, 0), u16le(d, 4));
                let c = self.conns.get_mut(&handle).expect("present");
                let Some(ch) = c.sig.channels.iter_mut().find(|ch| ch.local == scid && ch.state == State::Config && ch.ident == ident) else {
                    return self.sig_violation(handle);
                };
                if res == conf::SUCCESS {
                    ch.our_conf_done = true;
                    self.maybe_open(handle, scid);
                } else {
                    // Our only option is our MTU; a peer that won't take it
                    // gets no channel.
                    self.fail_channel(handle, scid, "configuration refused", now_ms);
                }
            }
            code::DISC_REQ if d.len() == 4 => {
                let (dcid, scid) = (u16le(d, 0), u16le(d, 2));
                let c = self.conns.get_mut(&handle).expect("present");
                let Some(i) = c.sig.channels.iter().position(|ch| ch.local == dcid && ch.remote == scid) else {
                    // Invalid CID in request.
                    let mut r = vec![0x02, 0x00];
                    r.extend_from_slice(&scid.to_le_bytes());
                    r.extend_from_slice(&dcid.to_le_bytes());
                    self.sig_violation(handle);
                    return self.sig_send(handle, &cmd(code::COMMAND_REJECT, ident, &r));
                };
                c.sig.channels.remove(i);
                self.sig_send(handle, &cmd(code::DISC_RSP, ident, d));
                self.out_events.push_back(HostEvent::ChannelClosed { handle, cid: dcid });
            }
            code::DISC_RSP if d.len() == 4 => {
                let scid = u16le(d, 2);
                let c = self.conns.get_mut(&handle).expect("present");
                if let Some(i) = c.sig.channels.iter().position(|ch| ch.local == scid && ch.state == State::Closing) {
                    c.sig.channels.remove(i);
                    self.out_events.push_back(HostEvent::ChannelClosed { handle, cid: scid });
                }
            }
            code::ECHO_REQ => self.sig_send(handle, &cmd(code::ECHO_RSP, ident, &[])),
            code::INFO_REQ if d.len() == 2 => {
                let t = u16le(d, 0);
                let mut r = t.to_le_bytes().to_vec();
                match t {
                    // Extended features: none (basic mode only).
                    0x0002 => {
                        r.extend_from_slice(&0u16.to_le_bytes());
                        r.extend_from_slice(&0u32.to_le_bytes());
                    }
                    // Fixed channels: signalling only.
                    0x0003 => {
                        r.extend_from_slice(&0u16.to_le_bytes());
                        r.extend_from_slice(&0x02u64.to_le_bytes());
                    }
                    _ => r.extend_from_slice(&1u16.to_le_bytes()), // not supported
                }
                self.sig_send(handle, &cmd(code::INFO_RSP, ident, &r));
            }
            code::COMMAND_REJECT | code::ECHO_RSP | code::INFO_RSP => {}
            _ => self.reject(handle, ident),
        }
    }

    fn send_conf_req(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conns.get_mut(&handle) else { return };
        let ident = c.sig.ident();
        let Some(ch) = c.sig.channels.iter_mut().find(|ch| ch.local == local) else { return };
        ch.ident = ident;
        let mut d = ch.remote.to_le_bytes().to_vec();
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend(mtu_option(OUR_MTU));
        self.sig_send(handle, &cmd(code::CONF_REQ, ident, &d));
    }

    fn maybe_open(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conns.get_mut(&handle) else { return };
        let Some(ch) = c.sig.channels.iter_mut().find(|ch| ch.local == local) else { return };
        if ch.state == State::Config && ch.our_conf_done && ch.their_conf_done {
            ch.state = State::Open;
            ch.deadline = None;
            let (psm, mtu) = (ch.psm, ch.peer_mtu);
            self.out_events.push_back(HostEvent::ChannelOpen { handle, cid: local, psm, peer_mtu: mtu });
        }
    }

    fn fail_channel(&mut self, handle: u16, local: u16, why: &str, now_ms: u64) {
        let Some(c) = self.conns.get_mut(&handle) else { return };
        let Some(i) = c.sig.channels.iter().position(|ch| ch.local == local) else { return };
        let psm = c.sig.channels[i].psm;
        let had_remote = c.sig.channels[i].remote != 0;
        if had_remote {
            self.l2cap_disconnect(handle, local, now_ms);
            // Forget it now; a late Disconnection Response is ignored.
            if let Some(c) = self.conns.get_mut(&handle) {
                c.sig.channels.retain(|ch| ch.local != local);
            }
        } else {
            c.sig.channels.remove(i);
        }
        self.out_events.push_back(HostEvent::ChannelFailed { handle, psm, why: why.into() });
    }

    pub(crate) fn l2cap_tick(&mut self, now_ms: u64) {
        let due: Vec<(u16, u16, State)> = self
            .conns
            .iter()
            .flat_map(|(h, c)| c.sig.channels.iter().filter(|ch| ch.deadline.is_some_and(|d| now_ms >= d)).map(move |ch| (*h, ch.local, ch.state)))
            .collect();
        for (h, local, st) in due {
            if st == State::Closing {
                if let Some(c) = self.conns.get_mut(&h) {
                    c.sig.channels.retain(|ch| ch.local != local);
                }
                self.out_events.push_back(HostEvent::ChannelClosed { handle: h, cid: local });
            } else {
                self.fail_channel(h, local, "signalling timeout", now_ms);
            }
        }
    }
}

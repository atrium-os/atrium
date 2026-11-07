//! Vinculum's host core (docs/spec/bluetooth.md §6), sans-I/O.
//!
//! [`Host`] is one controller's state machine: H4 frames and the clock go
//! in, frames for the controller and [`HostEvent`]s come out. It has no
//! sockets, threads or timers of its own, so every behaviour is testable
//! with a scripted controller and fuzzable with scripted frames.
//!
//! B1 scope: command flow control, controller bring-up, LE scanning and
//! connections, the connection table with its bound, ACL credits and L2CAP
//! reassembly. Classic connection requests are refused (no bonds exist yet,
//! spec §7). B2: LE Secure Connections pairing as central (`security`),
//! and encryption with a stored key. L2CAP channels and GATT come in B3–B4.
#![forbid(unsafe_code)]

pub mod acl;
pub mod cmd;
mod security;
pub mod testing;

pub use security::{PairConfig, Prompt};

use std::collections::{HashMap, VecDeque};

use acl::{Credits, Pool, Reassembler, ReassemblyError};
use cmd::CommandQueue;
use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, Command, Event, EventChild, LeMetaEventChild, OpCode};
use vinculum_hci_proto::{gap, Packet};

/// Most connections one controller may hold (spec §6.1).
pub const MAX_CONNECTIONS: usize = 16;
/// How long a disconnect waits for the link's queued data to go out.
pub const DISCONNECT_DRAIN_MS: u64 = 1000;
/// Protocol violations from one connection before it is dropped.
pub const MAX_LINK_VIOLATIONS: u32 = 8;

/// A Bluetooth device address with its type, as the controller reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Peer {
    pub address: [u8; 6], // most significant byte first, as printed
    pub random: bool,
}

impl Peer {
    fn from_hci(a: hci::Address, t: hci::AddressType) -> Peer {
        let v: u64 = a.into();
        let b = v.to_le_bytes();
        Peer {
            address: [b[5], b[4], b[3], b[2], b[1], b[0]],
            random: matches!(t, hci::AddressType::RandomDeviceAddress | hci::AddressType::RandomIdentityAddress),
        }
    }

    fn to_hci(self) -> (hci::Address, hci::AddressType) {
        let a = self.address;
        let v = u64::from_le_bytes([a[5], a[4], a[3], a[2], a[1], a[0], 0, 0]);
        let t = if self.random { hci::AddressType::RandomDeviceAddress } else { hci::AddressType::PublicDeviceAddress };
        (hci::Address::try_from(v).expect("48-bit address"), t)
    }
}

impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let a = self.address;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[0], a[1], a[2], a[3], a[4], a[5])?;
        if self.random {
            write!(f, " (random)")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Le,
    BrEdr,
}

/// What bring-up learned about the controller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerInfo {
    pub address: Option<[u8; 6]>,
    pub hci_version: u8,
    pub manufacturer: u16,
    pub lmp_features: u64,
    pub supported_commands: Vec<u8>,
    pub acl_mtu: u16,
    pub acl_buffers: u16,
    pub le_mtu: u16,
    pub le_buffers: u16,
    pub iso_mtu: u16,
    pub iso_buffers: u16,
}

impl ControllerInfo {
    /// Whether the controller lists a command (Core Vol 4 Part E §6.27).
    pub fn supports(&self, octet: usize, bit: u8) -> bool {
        self.supported_commands.get(octet).is_some_and(|o| o & (1 << bit) != 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Bring-up finished; the controller is usable.
    Ready(ControllerInfo),
    /// The controller is unusable (a command timed out or bring-up failed).
    /// The caller resets the transport and starts a new `Host`.
    Fatal(String),
    Advertisement { peer: Peer, rssi: i8, name: Option<String> },
    Connected { handle: u16, peer: Peer, transport: Transport, central: bool },
    ConnectFailed { peer: Option<Peer>, status: String },
    Disconnected { handle: u16, reason: String },
    /// A complete L2CAP PDU from a connection (fixed or dynamic channel).
    L2cap { handle: u16, cid: u16, payload: Vec<u8> },
    /// A command the caller asked for failed.
    CommandFailed { op: String, status: String },
    /// Pairing needs the user (answer with `pairing_confirm` or
    /// `pairing_passkey`).
    PairingPrompt { handle: u16, prompt: Prompt },
    /// Pairing finished; the caller stores the bond.
    Paired { handle: u16, bond: Box<vinculum_smp::pairing::Bond> },
    PairingFailed { handle: u16, reason: u8, why: String },
    /// The link is encrypted (after pairing, or with a stored key).
    Encrypted { handle: u16, key_size: u8 },
    /// Encryption with a stored key failed (the peer lost the bond, or the
    /// key is wrong). The link is left unencrypted.
    EncryptionFailed { handle: u16, status: String },
    /// The peripheral asks the central to secure the link (SMP Security
    /// Request); the caller decides between `encrypt` and `pair`.
    SecurityRequest { handle: u16, auth_req: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Reset,
    ReadVersion,
    ReadCommands,
    ReadFeatures,
    ReadBdAddr,
    ReadBufferSize,
    LeReadBufferSizeV2,
    LeReadBufferSizeV1,
    SetEventMask,
    LeSetEventMask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Init(Step),
    /// Issued for the caller; failures are reported as CommandFailed.
    User,
    /// Issued by policy (rejects, disconnects of misbehaving links);
    /// failures are logged only.
    Policy,
    /// LE Start Encryption on a connection.
    Encrypt(u16),
}

#[derive(Debug)]
struct Connection {
    peer: Peer,
    transport: Transport,
    reassembly: Reassembler,
    violations: u32,
    central: bool,
    smp: Option<vinculum_smp::pairing::Initiator>,
    /// LE Start Encryption sent, Encryption Change not yet seen.
    encrypting: bool,
    encrypted: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub command_violations: u32,
    pub credit_violations: u32,
    pub link_violations: u32,
    pub unparsed_events: u32,
    pub dropped_connections: u32,
}

pub struct Host {
    cmds: CommandQueue<Tag>,
    info: ControllerInfo,
    ready: bool,
    dead: bool,
    credits: Credits,
    conns: HashMap<u16, Connection>,
    /// ACL packets waiting for a credit, per connection, in order.
    acl_out: VecDeque<(u16, Vec<u8>)>,
    /// Disconnects waiting for their link's data to drain, with deadlines.
    closing: Vec<(u16, u64)>,
    out_frames: VecDeque<Frame>,
    out_events: VecDeque<HostEvent>,
    stats: Stats,
}

/// Event mask: the spec default plus Encryption Key Refresh (bit 47) and
/// LE Meta (bit 61).
const EVENT_MASK: u64 = 0x0000_1FFF_FFFF_FFFF | (1 << 47) | (1 << 61);
/// LE event mask: connection complete, advertising report, connection
/// update, remote features, LTK request, remote connection parameter request,
/// data length change, enhanced connection complete, extended advertising
/// report.
const LE_EVENT_MASK: u64 = 0x7F | (1 << 9) | (1 << 12);

fn to_cmd<C>(c: C) -> Command
where
    C: TryInto<Command>,
    C::Error: std::fmt::Debug,
{
    c.try_into().expect("encode command")
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    pub fn new() -> Host {
        Host {
            cmds: CommandQueue::default(),
            info: ControllerInfo::default(),
            ready: false,
            dead: false,
            credits: Credits::new(0, 0),
            conns: HashMap::new(),
            acl_out: VecDeque::new(),
            closing: Vec::new(),
            out_frames: VecDeque::new(),
            out_events: VecDeque::new(),
            stats: Stats::default(),
        }
    }

    /// Starts bring-up. Call once, then drive with the other methods.
    pub fn start(&mut self, now_ms: u64) {
        self.issue(to_cmd(hci::Reset {}), Tag::Init(Step::Reset));
        self.pump(now_ms);
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn info(&self) -> &ControllerInfo {
        &self.info
    }

    pub fn stats(&self) -> Stats {
        Stats {
            command_violations: self.cmds.violations,
            credit_violations: self.credits.violations,
            ..self.stats
        }
    }

    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// The peer on a connection.
    pub fn peer(&self, handle: u16) -> Option<Peer> {
        self.conns.get(&handle).map(|c| c.peer)
    }

    /// The next frame to write to the controller.
    pub fn poll_frame(&mut self) -> Option<Frame> {
        self.out_frames.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<HostEvent> {
        self.out_events.pop_front()
    }

    /// Whether events are waiting (handling one can queue more).
    pub fn poll_event_pending(&self) -> bool {
        !self.out_events.is_empty()
    }

    /// When [`Host::tick`] must next be called, if ever.
    pub fn deadline(&self) -> Option<u64> {
        let smp = self.conns.values().filter_map(|c| c.smp.as_ref()?.deadline()).chain(self.closing.iter().map(|c| c.1)).min();
        match (self.cmds.deadline(), smp) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn tick(&mut self, now_ms: u64) {
        if let Some(op) = self.cmds.expired(now_ms) {
            self.fatal(format!("{op:?}: no completion within {} ms", cmd::COMMAND_TIMEOUT_MS));
        }
        self.smp_tick(now_ms);
        self.pump(now_ms);
    }

    // ---- requests from the caller -------------------------------------

    pub fn le_scan(&mut self, on: bool, now_ms: u64) {
        if on {
            self.issue(
                to_cmd(hci::LeSetScanParameters {
                    le_scan_type: hci::LeScanType::Active,
                    le_scan_interval: 0x0060, // 60 ms
                    le_scan_window: 0x0030,   // 30 ms
                    own_address_type: hci::OwnAddressType::PublicDeviceAddress,
                    scanning_filter_policy: hci::LeScanningFilterPolicy::AcceptAll,
                }),
                Tag::User,
            );
        }
        let e = if on { hci::Enable::Enabled } else { hci::Enable::Disabled };
        self.issue(to_cmd(hci::LeSetScanEnable { le_scan_enable: e, filter_duplicates: hci::Enable::Enabled }), Tag::User);
        self.pump(now_ms);
    }

    pub fn le_connect(&mut self, peer: Peer, now_ms: u64) {
        let (address, peer_address_type) = peer.to_hci();
        self.issue(
            to_cmd(hci::LeCreateConnection {
                le_scan_interval: 0x0060,
                le_scan_window: 0x0030,
                initiator_filter_policy: hci::InitiatorFilterPolicy::UsePeerAddress,
                peer_address_type,
                peer_address: address,
                own_address_type: hci::OwnAddressType::PublicDeviceAddress,
                connection_interval_min: 0x0018, // 30 ms
                connection_interval_max: 0x0028, // 50 ms
                max_latency: 0,
                supervision_timeout: 0x01F4, // 5 s
                min_ce_length: 0,
                max_ce_length: 0,
            }),
            Tag::User,
        );
        self.pump(now_ms);
    }

    /// Disconnects once the data already queued on the link has reached the
    /// peer (the controller completed it), or after [`DISCONNECT_DRAIN_MS`].
    /// A last PDU such as SMP key distribution is not lost to the race.
    pub fn disconnect(&mut self, handle: u16, now_ms: u64) {
        if !self.closing.iter().any(|c| c.0 == handle) {
            self.closing.push((handle, now_ms + DISCONNECT_DRAIN_MS));
        }
        self.pump(now_ms);
    }

    /// Queues an L2CAP PDU on a connection, fragmented to the controller's
    /// buffer size. Fails if the handle is unknown or the host is not ready.
    pub fn send_l2cap(&mut self, handle: u16, cid: u16, payload: &[u8], now_ms: u64) -> Result<(), &'static str> {
        self.queue_l2cap(handle, cid, payload)?;
        self.pump(now_ms);
        Ok(())
    }

    fn queue_l2cap(&mut self, handle: u16, cid: u16, payload: &[u8]) -> Result<(), &'static str> {
        if !self.ready {
            return Err("controller not ready");
        }
        let conn = self.conns.get(&handle).ok_or("no such connection")?;
        if payload.len() > 0xFFFF {
            return Err("PDU too long");
        }
        let mtu = match conn.transport {
            Transport::Le if self.info.le_buffers > 0 => self.info.le_mtu,
            _ => self.info.acl_mtu,
        } as usize;
        let first_flag = match conn.transport {
            Transport::Le => hci::PacketBoundaryFlag::FirstNonAutomaticallyFlushable,
            Transport::BrEdr => hci::PacketBoundaryFlag::FirstAutomaticallyFlushable,
        };
        for (i, frag) in acl::fragment(cid, payload, mtu.max(1)).into_iter().enumerate() {
            let pb = if i == 0 { first_flag } else { hci::PacketBoundaryFlag::ContinuingFragment };
            let acl = hci::Acl {
                handle,
                packet_boundary_flag: pb,
                broadcast_flag: hci::BroadcastFlag::PointToPoint,
                payload: frag,
            };
            self.acl_out.push_back((handle, acl.encode_to_vec().map_err(|_| "encode")?));
        }
        Ok(())
    }

    // ---- input from the controller ------------------------------------

    pub fn on_frame(&mut self, f: Frame, now_ms: u64) {
        if self.dead {
            return;
        }
        match f.kind {
            Kind::Event => self.on_event(&f.packet),
            Kind::Acl => self.on_acl(&f.packet, now_ms),
            // SCO and ISO have no consumer before B5/B7; commands never come
            // from a controller.
            Kind::Sco | Kind::Iso => {}
            Kind::Command => self.stats.unparsed_events += 1,
        }
        self.pump(now_ms);
    }

    fn on_event(&mut self, bytes: &[u8]) {
        let Ok(ev) = Event::decode_full(bytes) else {
            self.stats.unparsed_events += 1;
            return;
        };
        let Ok(child) = ev.specialize() else {
            self.stats.unparsed_events += 1;
            return;
        };
        match child {
            EventChild::CommandComplete(cc) => {
                let status = cc.payload.first().copied().and_then(|s| hci::ErrorCode::try_from(s).ok());
                if let Some(tag) = self.cmds.complete(cc.command_op_code, cc.num_hci_command_packets) {
                    self.on_complete(tag, cc.command_op_code, status.unwrap_or(hci::ErrorCode::StatusUnknown), Some(&cc));
                }
            }
            EventChild::CommandStatus(cs) => {
                if let Some(tag) = self.cmds.complete(cs.command_op_code, cs.num_hci_command_packets) {
                    self.on_complete(tag, cs.command_op_code, cs.status, None);
                }
            }
            EventChild::NumberOfCompletedPackets(n) => {
                for c in n.completed_packets {
                    self.credits.completed(c.connection_handle, c.host_num_of_completed_packets);
                }
            }
            EventChild::DisconnectionComplete(d) => {
                if d.status == hci::ErrorCode::Success {
                    self.drop_connection(d.connection_handle);
                    self.out_events.push_back(HostEvent::Disconnected {
                        handle: d.connection_handle,
                        reason: format!("{:?}", d.reason),
                    });
                }
            }
            EventChild::ConnectionRequest(r) => {
                // No bonds yet (B2), so no Classic peer may connect (spec §7).
                self.issue(
                    to_cmd(hci::RejectConnectionRequest { bd_addr: r.bd_addr, reason: hci::RejectConnectionReason::SecurityReasons }),
                    Tag::Policy,
                );
            }
            EventChild::ConnectionComplete(c) => {
                let peer = Peer::from_hci(c.bd_addr, hci::AddressType::PublicDeviceAddress);
                if c.status == hci::ErrorCode::Success {
                    self.add_connection(c.connection_handle, peer, Transport::BrEdr, true);
                } else {
                    self.out_events.push_back(HostEvent::ConnectFailed { peer: Some(peer), status: format!("{:?}", c.status) });
                }
            }
            EventChild::EncryptionChange(e) => {
                let on = e.encryption_enabled != hci::EncryptionEnabled::Off;
                self.on_encryption_change(e.connection_handle, e.status, on, None)
            }
            EventChild::EncryptionChangeV2(e) => {
                let on = e.encryption_enabled != hci::EncryptionEnabled::Off;
                self.on_encryption_change(e.connection_handle, e.status, on, Some(e.key_size))
            }
            EventChild::LeMetaEvent(m) => self.on_le_meta(m),
            EventChild::HardwareError(h) => self.fatal(format!("controller hardware error 0x{:02x}", h.hardware_code)),
            _ => {}
        }
    }

    fn on_le_meta(&mut self, m: hci::LeMetaEvent) {
        let Ok(child) = m.specialize() else {
            self.stats.unparsed_events += 1;
            return;
        };
        match child {
            LeMetaEventChild::LeAdvertisingReport(r) => {
                for x in r.responses {
                    self.advert(Peer::from_hci(x.address, x.address_type), x.rssi, &x.advertising_data);
                }
            }
            LeMetaEventChild::LeExtendedAdvertisingReport(r) => {
                for x in r.responses {
                    let t = match x.address_type {
                        hci::DirectAdvertisingAddressType::RandomDeviceAddress
                        | hci::DirectAdvertisingAddressType::RandomIdentityAddress => hci::AddressType::RandomDeviceAddress,
                        _ => hci::AddressType::PublicDeviceAddress,
                    };
                    self.advert(Peer::from_hci(x.address, t), x.rssi, &x.advertising_data);
                }
            }
            LeMetaEventChild::LeConnectionComplete(c) => {
                self.le_connected(c.status, c.connection_handle, Peer::from_hci(c.peer_address, c.peer_address_type), c.role)
            }
            LeMetaEventChild::LeEnhancedConnectionCompleteV1(c) => {
                self.le_connected(c.status, c.connection_handle, Peer::from_hci(c.peer_address, c.peer_address_type), c.role)
            }
            LeMetaEventChild::LeEnhancedConnectionCompleteV2(c) => {
                self.le_connected(c.status, c.connection_handle, Peer::from_hci(c.peer_address, c.peer_address_type), c.role)
            }
            LeMetaEventChild::LeLongTermKeyRequest(r) => {
                // Vinculum is never the LE peripheral, so it holds no key a
                // central could ask for.
                self.issue(to_cmd(hci::LeLongTermKeyRequestNegativeReply { connection_handle: r.connection_handle }), Tag::Policy);
            }
            _ => {}
        }
    }

    fn advert(&mut self, peer: Peer, rssi: u8, data: &[u8]) {
        let name = gap::parse(data).ok().and_then(|s| gap::local_name(&s));
        self.out_events.push_back(HostEvent::Advertisement { peer, rssi: rssi as i8, name });
    }

    fn le_connected(&mut self, status: hci::ErrorCode, handle: u16, peer: Peer, role: hci::Role) {
        if status == hci::ErrorCode::Success {
            self.add_connection(handle, peer, Transport::Le, role == hci::Role::Central);
        } else {
            self.out_events.push_back(HostEvent::ConnectFailed { peer: Some(peer), status: format!("{status:?}") });
        }
    }

    fn add_connection(&mut self, handle: u16, peer: Peer, transport: Transport, central: bool) {
        if self.conns.contains_key(&handle) {
            // A controller reusing a live handle is broken; drop both.
            self.stats.link_violations += 1;
            self.kick(handle);
            return;
        }
        if self.conns.len() >= MAX_CONNECTIONS {
            self.stats.dropped_connections += 1;
            self.issue(
                to_cmd(hci::Disconnect {
                    connection_handle: handle,
                    reason: hci::DisconnectReason::RemoteDeviceTerminatedConnectionLowResources,
                }),
                Tag::Policy,
            );
            return;
        }
        self.conns.insert(
            handle,
            Connection {
                peer,
                transport,
                reassembly: Reassembler::default(),
                violations: 0,
                central,
                smp: None,
                encrypting: false,
                encrypted: false,
            },
        );
        self.out_events.push_back(HostEvent::Connected { handle, peer, transport, central });
    }

    fn drop_connection(&mut self, handle: u16) {
        // Dropping the Initiator zeroes its keys.
        self.conns.remove(&handle);
        self.closing.retain(|c| c.0 != handle);
        self.credits.disconnected(handle);
        self.acl_out.retain(|(h, _)| *h != handle);
    }

    /// Disconnects a misbehaving link.
    fn kick(&mut self, handle: u16) {
        self.issue(
            to_cmd(hci::Disconnect { connection_handle: handle, reason: hci::DisconnectReason::AuthenticationFailure }),
            Tag::Policy,
        );
    }

    fn on_acl(&mut self, bytes: &[u8], now_ms: u64) {
        let Ok(acl) = hci::Acl::decode_full(bytes) else {
            self.stats.link_violations += 1;
            return;
        };
        let handle = acl.handle;
        let Some(conn) = self.conns.get_mut(&handle) else {
            // Data for a handle we don't know (a race with disconnection, or
            // a broken controller): drop.
            return;
        };
        let first = acl.packet_boundary_flag != hci::PacketBoundaryFlag::ContinuingFragment;
        let mut bad = false;
        let mut fatal = false;
        match conn.reassembly.push(first, &acl.payload) {
            Ok(p) => {
                bad = p.interrupted;
                if let Some(pdu) = p.pdu {
                    if pdu.cid == security::SMP_CID && conn.transport == Transport::Le {
                        self.on_smp(handle, &pdu.payload, now_ms);
                    } else {
                        self.out_events.push_back(HostEvent::L2cap { handle, cid: pdu.cid, payload: pdu.payload });
                    }
                }
            }
            Err(ReassemblyError::Orphan) => bad = true,
            Err(ReassemblyError::TooLong) => fatal = true,
        }
        if bad || fatal {
            self.stats.link_violations += 1;
            let conn = self.conns.get_mut(&handle).expect("present");
            conn.violations += 1;
            if fatal || conn.violations >= MAX_LINK_VIOLATIONS {
                self.kick(handle);
            }
        }
    }

    // ---- internals ------------------------------------------------------

    fn issue(&mut self, c: Command, tag: Tag) {
        if self.cmds.push(c, tag).is_err() {
            self.fatal("command queue full".into());
        }
    }

    fn fatal(&mut self, why: String) {
        if !self.dead {
            self.dead = true;
            self.ready = false;
            self.out_events.push_back(HostEvent::Fatal(why));
        }
    }

    /// Moves queued commands and ACL packets out as far as flow control allows.
    fn pump(&mut self, now_ms: u64) {
        if self.dead {
            return;
        }
        if self.ready {
            self.pump_acl();
            self.pump_closing(now_ms);
        }
        while let Some(c) = self.cmds.next_to_send(now_ms) {
            match c.encode_to_vec() {
                Ok(packet) => self.out_frames.push_back(Frame { kind: Kind::Command, packet }),
                Err(e) => self.fatal(format!("encode {:?}: {e:?}", c.op_code)),
            }
        }
    }

    fn pump_closing(&mut self, now_ms: u64) {
        let mut i = 0;
        while i < self.closing.len() {
            let (h, deadline) = self.closing[i];
            let drained = !self.acl_out.iter().any(|(x, _)| *x == h) && self.credits.outstanding(h) == 0;
            if drained || now_ms >= deadline || !self.conns.contains_key(&h) {
                self.closing.swap_remove(i);
                self.issue(
                    to_cmd(hci::Disconnect { connection_handle: h, reason: hci::DisconnectReason::RemoteUserTerminatedConnection }),
                    Tag::User,
                );
            } else {
                i += 1;
            }
        }
    }

    fn pump_acl(&mut self) {
        let mut held = VecDeque::new();
        while let Some((handle, pkt)) = self.acl_out.pop_front() {
            let pool = match self.conns.get(&handle).map(|c| c.transport) {
                Some(Transport::Le) => Pool::Le,
                Some(Transport::BrEdr) => Pool::BrEdr,
                None => continue,
            };
            // Keep per-connection order: once one packet of a handle waits,
            // the rest of that handle waits behind it.
            if held.iter().any(|(h, _)| *h == handle) || !self.credits.take(handle, pool) {
                held.push_back((handle, pkt));
                continue;
            }
            self.out_frames.push_back(Frame { kind: Kind::Acl, packet: pkt });
        }
        self.acl_out = held;
    }

    fn on_complete(&mut self, tag: Tag, op: OpCode, status: hci::ErrorCode, cc: Option<&hci::CommandComplete>) {
        match tag {
            Tag::User => {
                if status != hci::ErrorCode::Success {
                    self.out_events.push_back(HostEvent::CommandFailed { op: format!("{op:?}"), status: format!("{status:?}") });
                }
            }
            Tag::Policy => {}
            Tag::Encrypt(handle) => {
                if status != hci::ErrorCode::Success {
                    self.on_encryption_change(handle, status, false, None);
                }
            }
            Tag::Init(step) => self.init_step(step, status, cc),
        }
    }

    fn init_step(&mut self, step: Step, status: hci::ErrorCode, cc: Option<&hci::CommandComplete>) {
        use Step::*;
        // LE Read Buffer Size v2 is optional (5.2+); fall back to v1.
        if step == LeReadBufferSizeV2 && status != hci::ErrorCode::Success {
            self.issue(to_cmd(hci::LeReadBufferSizeV1 {}), Tag::Init(LeReadBufferSizeV1));
            return;
        }
        if status != hci::ErrorCode::Success {
            self.fatal(format!("bring-up {step:?}: {status:?}"));
            return;
        }
        let Some(cc) = cc else {
            self.fatal(format!("bring-up {step:?}: Command Status instead of Command Complete"));
            return;
        };
        let bad = |what: &str| format!("bring-up {step:?}: cannot parse {what}");
        let next: Option<(Command, Step)> = match step {
            Reset => Some((to_cmd(hci::ReadLocalVersionInformation {}), ReadVersion)),
            ReadVersion => match hci::ReadLocalVersionInformationComplete::try_from(cc) {
                Ok(r) => {
                    self.info.hci_version = r.local_version_information.hci_version as u8;
                    self.info.manufacturer = r.local_version_information.manufacturer_name;
                    Some((to_cmd(hci::ReadLocalSupportedCommands {}), ReadCommands))
                }
                Err(_) => return self.fatal(bad("version")),
            },
            ReadCommands => match hci::ReadLocalSupportedCommandsComplete::try_from(cc) {
                Ok(r) => {
                    self.info.supported_commands = r.supported_commands.to_vec();
                    Some((to_cmd(hci::ReadLocalSupportedFeatures {}), ReadFeatures))
                }
                Err(_) => return self.fatal(bad("supported commands")),
            },
            ReadFeatures => match hci::ReadLocalSupportedFeaturesComplete::try_from(cc) {
                Ok(r) => {
                    self.info.lmp_features = r.lmp_features;
                    Some((to_cmd(hci::ReadBdAddr {}), ReadBdAddr))
                }
                Err(_) => return self.fatal(bad("features")),
            },
            ReadBdAddr => match hci::ReadBdAddrComplete::try_from(cc) {
                Ok(r) => {
                    self.info.address = Some(Peer::from_hci(r.bd_addr, hci::AddressType::PublicDeviceAddress).address);
                    Some((to_cmd(hci::ReadBufferSize {}), ReadBufferSize))
                }
                Err(_) => return self.fatal(bad("address")),
            },
            ReadBufferSize => match hci::ReadBufferSizeComplete::try_from(cc) {
                Ok(r) => {
                    self.info.acl_mtu = r.acl_data_packet_length;
                    self.info.acl_buffers = r.total_num_acl_data_packets;
                    Some((to_cmd(hci::LeReadBufferSizeV2 {}), LeReadBufferSizeV2))
                }
                Err(_) => return self.fatal(bad("buffer size")),
            },
            LeReadBufferSizeV2 => match hci::LeReadBufferSizeV2Complete::try_from(cc) {
                Ok(r) => {
                    self.info.le_mtu = r.le_buffer_size.le_data_packet_length;
                    self.info.le_buffers = r.le_buffer_size.total_num_le_packets as u16;
                    self.info.iso_mtu = r.iso_buffer_size.le_data_packet_length;
                    self.info.iso_buffers = r.iso_buffer_size.total_num_le_packets as u16;
                    Some((to_cmd(hci::SetEventMask { event_mask: EVENT_MASK }), SetEventMask))
                }
                Err(_) => return self.fatal(bad("LE buffer size v2")),
            },
            LeReadBufferSizeV1 => match hci::LeReadBufferSizeV1Complete::try_from(cc) {
                Ok(r) => {
                    self.info.le_mtu = r.le_buffer_size.le_data_packet_length;
                    self.info.le_buffers = r.le_buffer_size.total_num_le_packets as u16;
                    Some((to_cmd(hci::SetEventMask { event_mask: EVENT_MASK }), SetEventMask))
                }
                Err(_) => return self.fatal(bad("LE buffer size")),
            },
            SetEventMask => Some((to_cmd(hci::LeSetEventMask { le_event_mask: LE_EVENT_MASK }), LeSetEventMask)),
            LeSetEventMask => None,
        };
        match next {
            Some((c, s)) => self.issue(c, Tag::Init(s)),
            None => {
                if self.info.acl_buffers == 0 && self.info.le_buffers == 0 {
                    return self.fatal("controller reports no ACL buffers".into());
                }
                self.credits = Credits::new(self.info.acl_buffers, self.info.le_buffers);
                self.ready = true;
                self.out_events.push_back(HostEvent::Ready(self.info.clone()));
            }
        }
    }
}

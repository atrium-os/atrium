//! BR/EDR (Classic) security: Secure Simple Pairing with Secure
//! Connections, link keys and encryption (Core 6.0 Vol 2 Part F §4.2,
//! Vol 4 Part E §7.1; docs/spec/bluetooth.md §7).
//!
//! On Classic the controller runs the pairing protocol; the host answers
//! its questions (IO capabilities, link key, the user's confirmation) and
//! checks the result. Rules enforced here:
//!
//! - **Secure Connections only.** A link key that is not P-256 (key types
//!   7 and 8) fails the pairing, and an SC-bonded link that comes up with
//!   E0 encryption instead of AES-CCM is dropped (downgrade, BLUFFS class).
//! - **16-byte keys** (KNOB): Read Encryption Key Size after every
//!   encryption change; under 16, disconnect.
//! - **No pairing nobody asked for.** An IO Capability Request on a link
//!   where Vinculum did not start pairing is refused (Pairing Not Allowed).
//! - **We authenticate every link ourselves** (BIAS): a bonded device is
//!   encrypted only after our own Authentication Requested, never because
//!   the peer authenticated us; we stay central (no role switch).
//! - Incoming connections are the caller's decision (`ConnectionRequest`);
//!   the daemon accepts bonded devices only.

use vinculum_hci_proto::hci;
use vinculum_smp::pairing::{method, Bond, IoCap, Method};

use crate::{to_cmd, Host, HostEvent, Peer, Prompt, Tag, Transport};

/// Classic security progress on one connection.
#[derive(Debug, Default)]
pub(crate) struct ClassicSec {
    /// Pairing we started (Authentication Requested with no key to give).
    pairing: bool,
    io_cap: Option<IoCap>,
    /// The stored link key to answer Link Key Request with.
    key: Option<[u8; 16]>,
    peer_io: Option<u8>,
    peer_mitm: bool,
    new_key: Option<([u8; 16], hci::KeyType)>,
    /// Authentication Requested sent, Authentication Complete not seen.
    authenticating: bool,
    /// Bonded with a P-256 key: encryption must be AES-CCM.
    sc_key: bool,
}

impl Drop for ClassicSec {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(k) = self.key.as_mut() {
            k.zeroize();
        }
        if let Some((k, _)) = self.new_key.as_mut() {
            k.zeroize();
        }
    }
}

fn hci_io(io: IoCap) -> hci::IoCapability {
    match io {
        IoCap::DisplayOnly => hci::IoCapability::DisplayOnly,
        // Classic has no KeyboardDisplay; DisplayYesNo is the spec's mapping.
        IoCap::DisplayYesNo | IoCap::KeyboardDisplay => hci::IoCapability::DisplayYesNo,
        IoCap::KeyboardOnly => hci::IoCapability::KeyboardOnly,
        IoCap::NoInputNoOutput => hci::IoCapability::NoInputNoOutput,
    }
}

fn io_from(v: u8) -> IoCap {
    match v {
        0 => IoCap::DisplayOnly,
        1 => IoCap::DisplayYesNo,
        2 => IoCap::KeyboardOnly,
        _ => IoCap::NoInputNoOutput,
    }
}

fn is_mitm(a: hci::AuthenticationRequirements) -> bool {
    (a as u8) & 1 == 1
}

impl Host {
    /// Connects to a Classic device by address (page). We stay central:
    /// no role switch, so we run authentication as the central (§7).
    pub fn classic_connect(&mut self, peer: Peer, now_ms: u64) {
        let (bd_addr, _) = peer.to_hci();
        self.issue(
            to_cmd(hci::CreateConnection {
                bd_addr,
                packet_type: 0xCC18, // DM1 DH1 DM3 DH3 DM5 DH5
                page_scan_repetition_mode: hci::PageScanRepetitionMode::R1,
                clock_offset: 0,
                clock_offset_valid: hci::ClockOffsetValid::Invalid,
                allow_role_switch: hci::CreateConnectionRoleSwitch::RemainCentral,
            }),
            Tag::User,
        );
        self.pump(now_ms);
    }

    /// Accepts an incoming Classic connection (after `ConnectionRequest`),
    /// taking the central role.
    pub fn accept_connection(&mut self, peer: Peer, now_ms: u64) {
        let (bd_addr, _) = peer.to_hci();
        self.issue(to_cmd(hci::AcceptConnectionRequest { bd_addr, role: hci::AcceptConnectionRequestRole::BecomeCentral }), Tag::Policy);
        self.pump(now_ms);
    }

    pub fn reject_connection(&mut self, peer: Peer, now_ms: u64) {
        let (bd_addr, _) = peer.to_hci();
        self.issue(
            to_cmd(hci::RejectConnectionRequest { bd_addr, reason: hci::RejectConnectionReason::SecurityReasons }),
            Tag::Policy,
        );
        self.pump(now_ms);
    }

    fn classic_conn(&mut self, a: hci::Address) -> Option<u16> {
        let p = Peer::from_hci(a, hci::AddressType::PublicDeviceAddress);
        self.conns.iter().find(|(_, c)| c.transport == Transport::BrEdr && c.peer == p).map(|(h, _)| *h)
    }

    pub(crate) fn classic_pair(&mut self, handle: u16, io_cap: IoCap) {
        let c = self.conns.get_mut(&handle).expect("checked");
        c.classic = Some(ClassicSec { pairing: true, io_cap: Some(io_cap), authenticating: true, ..Default::default() });
        self.issue(to_cmd(hci::AuthenticationRequested { connection_handle: handle }), Tag::Encrypt(handle));
    }

    pub(crate) fn classic_encrypt(&mut self, handle: u16, key: [u8; 16]) {
        let c = self.conns.get_mut(&handle).expect("checked");
        // Bonds are SC-only (pairing refuses anything else), so the link
        // must come up with AES-CCM.
        c.classic = Some(ClassicSec { key: Some(key), authenticating: true, sc_key: true, ..Default::default() });
        self.issue(to_cmd(hci::AuthenticationRequested { connection_handle: handle }), Tag::Encrypt(handle));
    }

    pub(crate) fn classic_fail(&mut self, handle: u16, why: &str) {
        let pairing = self.conns.get(&handle).and_then(|c| c.classic.as_ref()).is_some_and(|s| s.pairing);
        if let Some(c) = self.conns.get_mut(&handle) {
            c.classic = None;
            c.encrypting = false;
        }
        if pairing {
            self.out_events.push_back(HostEvent::PairingFailed { handle, reason: 0, why: why.into() });
        } else {
            self.out_events.push_back(HostEvent::EncryptionFailed { handle, status: why.into() });
        }
    }

    pub(crate) fn on_classic_event(&mut self, ev: hci::EventChild) {
        use hci::EventChild as E;
        match ev {
            E::LinkKeyRequest(r) => {
                let key = self.classic_conn(r.bd_addr).and_then(|h| self.conns[&h].classic.as_ref()).and_then(|s| s.key);
                match key {
                    Some(link_key) => self.issue(to_cmd(hci::LinkKeyRequestReply { bd_addr: r.bd_addr, link_key }), Tag::Policy),
                    None => self.issue(to_cmd(hci::LinkKeyRequestNegativeReply { bd_addr: r.bd_addr }), Tag::Policy),
                }
            }
            E::IoCapabilityRequest(r) => {
                let ours = self.classic_conn(r.bd_addr).and_then(|h| self.conns[&h].classic.as_ref()).filter(|s| s.pairing).and_then(|s| s.io_cap);
                match ours {
                    Some(io) => self.issue(
                        to_cmd(hci::IoCapabilityRequestReply {
                            bd_addr: r.bd_addr,
                            io_capability: hci_io(io),
                            oob_present: hci::OobDataPresent::NotPresent,
                            authentication_requirements: hci::AuthenticationRequirements::GeneralBondingMitmProtection,
                        }),
                        Tag::Policy,
                    ),
                    None => {
                        // The peer wants to pair and nobody asked (§7).
                        self.stats.link_violations += 1;
                        self.issue(
                            to_cmd(hci::IoCapabilityRequestNegativeReply { bd_addr: r.bd_addr, reason: hci::ErrorCode::PairingNotAllowed }),
                            Tag::Policy,
                        );
                    }
                }
            }
            E::IoCapabilityResponse(r) => {
                if let Some(s) = self.classic_conn(r.bd_addr).and_then(|h| self.conns.get_mut(&h)?.classic.as_mut()) {
                    s.peer_io = Some(r.io_capability as u8);
                    s.peer_mitm = is_mitm(r.authentication_requirements);
                }
            }
            E::UserConfirmationRequest(r) => {
                let Some(h) = self.classic_conn(r.bd_addr) else { return };
                let Some(s) = self.conns[&h].classic.as_ref().filter(|s| s.pairing) else {
                    self.issue(to_cmd(hci::UserConfirmationRequestNegativeReply { bd_addr: r.bd_addr }), Tag::Policy);
                    return;
                };
                let m = method(s.io_cap.unwrap_or(IoCap::NoInputNoOutput), io_from(s.peer_io.unwrap_or(3)), true);
                let number = (m == Method::NumericComparison).then_some(r.numeric_value);
                self.out_events.push_back(HostEvent::PairingPrompt { handle: h, prompt: Prompt::Confirm { number } });
            }
            E::UserPasskeyRequest(r) => {
                let Some(h) = self.classic_conn(r.bd_addr).filter(|h| self.conns[h].classic.as_ref().is_some_and(|s| s.pairing)) else {
                    self.issue(to_cmd(hci::UserPasskeyRequestNegativeReply { bd_addr: r.bd_addr }), Tag::Policy);
                    return;
                };
                self.out_events.push_back(HostEvent::PairingPrompt { handle: h, prompt: Prompt::EnterPasskey });
            }
            E::UserPasskeyNotification(r) => {
                if let Some(h) = self.classic_conn(r.bd_addr) {
                    self.out_events.push_back(HostEvent::PairingPrompt { handle: h, prompt: Prompt::Display { passkey: r.passkey } });
                }
            }
            E::SimplePairingComplete(r) => {
                if r.status != hci::ErrorCode::Success {
                    if let Some(h) = self.classic_conn(r.bd_addr) {
                        self.classic_fail(h, &format!("simple pairing failed: {:?}", r.status));
                    }
                }
            }
            E::LinkKeyNotification(r) => {
                if let Some(s) = self.classic_conn(r.bd_addr).and_then(|h| self.conns.get_mut(&h)?.classic.as_mut()) {
                    if s.pairing {
                        s.new_key = Some((r.link_key, r.key_type));
                    }
                }
            }
            E::AuthenticationComplete(a) => {
                let h = a.connection_handle;
                let Some(s) = self.conns.get_mut(&h).and_then(|c| c.classic.as_mut()) else {
                    self.stats.link_violations += 1;
                    return;
                };
                if !s.authenticating {
                    self.stats.link_violations += 1;
                    return;
                }
                s.authenticating = false;
                if a.status != hci::ErrorCode::Success {
                    return self.classic_fail(h, &format!("authentication failed: {:?}", a.status));
                }
                if s.pairing {
                    match s.new_key.as_ref().map(|k| k.1) {
                        Some(hci::KeyType::AuthenticatedP256 | hci::KeyType::UnauthenticatedP256) => s.sc_key = true,
                        Some(t) => {
                            // Legacy (P-192 or combination) keys: refused (§7).
                            self.kick(h);
                            return self.classic_fail(h, &format!("peer gave a {t:?} key; Secure Connections required"));
                        }
                        None => return self.classic_fail(h, "no link key from pairing"),
                    }
                }
                self.conns.get_mut(&h).expect("present").encrypting = true;
                self.issue(to_cmd(hci::SetConnectionEncryption { connection_handle: h, encryption_enable: hci::Enable::Enabled }), Tag::Encrypt(h));
            }
            _ => {}
        }
    }

    /// Encryption Change on a Classic link.
    pub(crate) fn on_classic_encryption_change(&mut self, handle: u16, status: hci::ErrorCode, mode: hci::EncryptionEnabled) {
        let Some(c) = self.conns.get_mut(&handle) else { return };
        let ok = status == hci::ErrorCode::Success && mode != hci::EncryptionEnabled::Off;
        if !c.encrypting {
            if c.encrypted && !ok {
                c.encrypted = false;
                self.stats.link_violations += 1;
                self.kick(handle);
            } else if !c.encrypted {
                self.stats.link_violations += 1;
            }
            return;
        }
        c.encrypting = false;
        if !ok {
            return self.classic_fail(handle, &format!("encryption failed: {status:?}"));
        }
        let sc = c.classic.as_ref().is_some_and(|s| s.sc_key);
        if sc && mode != hci::EncryptionEnabled::BrEdrAesCcm {
            self.stats.link_violations += 1;
            self.kick(handle);
            return self.classic_fail(handle, "E0 encryption on a Secure Connections key (downgrade)");
        }
        self.issue(to_cmd(hci::ReadEncryptionKeySize { connection_handle: handle }), Tag::KeySize(handle));
    }

    /// Read Encryption Key Size completed: the last check before trust.
    pub(crate) fn on_key_size(&mut self, handle: u16, status: hci::ErrorCode, size: u8) {
        if !self.conns.contains_key(&handle) {
            return;
        }
        if status != hci::ErrorCode::Success || size != 16 {
            self.kick(handle);
            return self.classic_fail(handle, &format!("encryption key size {size} (16 required)"));
        }
        let c = self.conns.get_mut(&handle).expect("present");
        c.encrypted = true;
        let s = c.classic.take();
        self.out_events.push_back(HostEvent::Encrypted { handle, key_size: size });
        if let Some(mut s) = s {
            if s.pairing {
                let (key, t) = s.new_key.take().expect("checked at authentication");
                let authenticated = t == hci::KeyType::AuthenticatedP256;
                let m = method(s.io_cap.unwrap_or(IoCap::NoInputNoOutput), io_from(s.peer_io.unwrap_or(3)), true);
                let bond = Bond {
                    ltk: key,
                    key_size: 16,
                    secure_connections: true,
                    authenticated,
                    method: if authenticated { m } else { Method::JustWorks },
                    peer_irk: None,
                    peer_identity: None,
                };
                self.out_events.push_back(HostEvent::Paired { handle, bond: Box::new(bond) });
            }
        }
    }

    /// The user's answer to a Classic `Prompt::Confirm`.
    pub(crate) fn classic_confirm(&mut self, handle: u16, yes: bool) {
        let Some(c) = self.conns.get(&handle) else { return };
        let (bd_addr, _) = c.peer.to_hci();
        if yes {
            self.issue(to_cmd(hci::UserConfirmationRequestReply { bd_addr }), Tag::Policy);
        } else {
            self.issue(to_cmd(hci::UserConfirmationRequestNegativeReply { bd_addr }), Tag::Policy);
        }
    }

    pub(crate) fn classic_passkey(&mut self, handle: u16, passkey: Option<u32>) {
        let Some(c) = self.conns.get(&handle) else { return };
        let (bd_addr, _) = c.peer.to_hci();
        match passkey {
            Some(p) if p <= 999_999 => self.issue(to_cmd(hci::UserPasskeyRequestReply { bd_addr, numeric_value: p }), Tag::Policy),
            _ => self.issue(to_cmd(hci::UserPasskeyRequestNegativeReply { bd_addr }), Tag::Policy),
        }
    }
}

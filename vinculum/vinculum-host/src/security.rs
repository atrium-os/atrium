//! LE security on the host: SMP routing, pairing as central, and link
//! encryption (docs/spec/bluetooth.md §7).
//!
//! The pairing logic lives in `vinculum_smp::pairing`; this module carries
//! its PDUs over the SMP fixed channel, turns its actions into HCI commands
//! and [`HostEvent`]s, and enforces the link rules that belong below the
//! policy layer:
//!
//! - only a central pairs or starts encryption (Vinculum is never the LE
//!   peripheral, so LTK requests are refused);
//! - an LE link must come up encrypted with a 16-byte key, or it is
//!   disconnected (KNOB-style downgrades);
//! - an SMP timeout ends the link (Core 6.0 Vol 3 Part H §3.4: no further
//!   SMP on it);
//! - unsolicited SMP PDUs from the peripheral, other than Security Request,
//!   are refused with Pairing Not Supported.

use vinculum_hci_proto::hci;
use vinculum_smp::pairing::{reason, Action, Config, Initiator, IoCap};

use crate::{to_cmd, Host, HostEvent, Tag, Transport};

/// The LE Security Manager fixed channel.
pub const SMP_CID: u16 = 0x0006;

/// What the user must do for a pairing in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// Confirm the pairing; `number` is the six digits to compare with the
    /// peer's display (numeric comparison), None for Just Works.
    Confirm { number: Option<u32> },
    /// Show this passkey; the user types it on the peer.
    Display { passkey: u32 },
    /// Ask the user for the passkey the peer shows.
    EnterPasskey,
}

/// Our side of a pairing.
#[derive(Clone)]
pub struct PairConfig {
    pub io_cap: IoCap,
    /// Our identity resolving key, distributed to the peer.
    pub irk: [u8; 16],
}

impl std::fmt::Debug for PairConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairConfig").field("io_cap", &self.io_cap).field("irk", &"<redacted>").finish()
    }
}

impl Host {
    /// Starts LE Secure Connections pairing on a connection where we are
    /// central. Progress arrives as `PairingPrompt`, then `Encrypted` and
    /// `Paired`, or `PairingFailed`.
    pub fn pair(&mut self, handle: u16, cfg: PairConfig, now_ms: u64) -> Result<(), &'static str> {
        let own = self.info.address.ok_or("controller not ready")?;
        let conn = self.conns.get(&handle).ok_or("no such connection")?;
        if conn.transport != Transport::Le || !conn.central {
            return Err("pairing needs an LE link where we are central");
        }
        if conn.smp.is_some() || conn.encrypting {
            return Err("security procedure already running");
        }
        // SMP addresses: 6 bytes little-endian, then the type (§2.2.7).
        let a = smp_addr(own, false);
        let b = smp_addr(conn.peer.address, conn.peer.random);
        let local_identity = (0u8, a[..6].try_into().expect("6"));
        let (init, actions) = Initiator::start(Config { io_cap: cfg.io_cap, local_irk: cfg.irk, local_identity }, a, b, now_ms);
        self.conns.get_mut(&handle).expect("present").smp = Some(init);
        self.smp_actions(handle, actions);
        self.pump(now_ms);
        Ok(())
    }

    /// The user's answer to `Prompt::Confirm`.
    pub fn pairing_confirm(&mut self, handle: u16, yes: bool, now_ms: u64) {
        let Some(init) = self.conns.get_mut(&handle).and_then(|c| c.smp.as_mut()) else { return };
        let a = init.confirmed(yes);
        self.smp_actions(handle, a);
        self.pump(now_ms);
    }

    /// The user's answer to `Prompt::EnterPasskey`; None cancels.
    pub fn pairing_passkey(&mut self, handle: u16, passkey: Option<u32>, now_ms: u64) {
        let Some(init) = self.conns.get_mut(&handle).and_then(|c| c.smp.as_mut()) else { return };
        let a = init.passkey_entered(passkey);
        self.smp_actions(handle, a);
        self.pump(now_ms);
    }

    /// Encrypts a link with a stored LE Secure Connections key (EDIV and
    /// Rand are zero for SC keys). Result: `Encrypted` or `EncryptionFailed`.
    pub fn encrypt(&mut self, handle: u16, ltk: [u8; 16], now_ms: u64) -> Result<(), &'static str> {
        let conn = self.conns.get_mut(&handle).ok_or("no such connection")?;
        if conn.transport != Transport::Le || !conn.central {
            return Err("encryption needs an LE link where we are central");
        }
        if conn.smp.is_some() || conn.encrypting {
            return Err("security procedure already running");
        }
        self.start_encryption(handle, ltk);
        self.pump(now_ms);
        Ok(())
    }

    /// Whether a connection is encrypted.
    pub fn is_encrypted(&self, handle: u16) -> bool {
        self.conns.get(&handle).is_some_and(|c| c.encrypted)
    }

    fn start_encryption(&mut self, handle: u16, ltk: [u8; 16]) {
        if let Some(c) = self.conns.get_mut(&handle) {
            c.encrypting = true;
        }
        self.issue(to_cmd(hci::LeStartEncryption { connection_handle: handle, rand: [0; 8], ediv: 0, ltk }), Tag::Encrypt(handle));
    }

    pub(crate) fn on_smp(&mut self, handle: u16, pdu: &[u8], now_ms: u64) {
        let Some(conn) = self.conns.get_mut(&handle) else { return };
        if let Some(init) = conn.smp.as_mut() {
            let a = init.on_pdu(pdu, now_ms);
            self.smp_actions(handle, a);
            return;
        }
        match pdu.first() {
            // Security Request (auth_req follows).
            Some(0x0B) if pdu.len() == 2 && conn.central => {
                self.out_events.push_back(HostEvent::SecurityRequest { handle, auth_req: pdu[1] });
            }
            // A Pairing Failed with nothing running: nothing to answer.
            Some(0x05) => {}
            _ => {
                self.stats.link_violations += 1;
                let _ = self.queue_l2cap(handle, SMP_CID, &[0x05, reason::PAIRING_NOT_SUPPORTED]);
            }
        }
    }

    pub(crate) fn smp_tick(&mut self, now_ms: u64) {
        let due: Vec<u16> = self
            .conns
            .iter()
            .filter(|(_, c)| c.smp.as_ref().and_then(|s| s.deadline()).is_some_and(|d| now_ms >= d))
            .map(|(h, _)| *h)
            .collect();
        for h in due {
            let a = self.conns.get_mut(&h).and_then(|c| c.smp.as_mut()).map(|s| s.tick(now_ms)).unwrap_or_default();
            self.smp_actions(h, a);
            // §3.4: after an SMP timeout no further SMP may run on the link.
            self.kick(h);
        }
    }

    fn smp_actions(&mut self, handle: u16, actions: Vec<Action>) {
        for act in actions {
            match act {
                Action::Send(p) => {
                    let _ = self.queue_l2cap(handle, SMP_CID, &p);
                }
                Action::Confirm { number } => {
                    self.out_events.push_back(HostEvent::PairingPrompt { handle, prompt: Prompt::Confirm { number } })
                }
                Action::Display { passkey } => {
                    self.out_events.push_back(HostEvent::PairingPrompt { handle, prompt: Prompt::Display { passkey } })
                }
                Action::EnterPasskey => {
                    self.out_events.push_back(HostEvent::PairingPrompt { handle, prompt: Prompt::EnterPasskey })
                }
                Action::StartEncryption { ltk } => self.start_encryption(handle, ltk),
                Action::Done(bond) => {
                    if let Some(c) = self.conns.get_mut(&handle) {
                        c.smp = None;
                    }
                    self.out_events.push_back(HostEvent::Paired { handle, bond });
                }
                Action::Failed { reason, why } => {
                    if let Some(c) = self.conns.get_mut(&handle) {
                        c.smp = None;
                    }
                    self.out_events.push_back(HostEvent::PairingFailed { handle, reason, why: why.into() });
                }
            }
        }
    }

    /// Encryption Change (v1 or v2), or LE Start Encryption refused.
    pub(crate) fn on_encryption_change(&mut self, handle: u16, status: hci::ErrorCode, on: bool, key_size: Option<u8>) {
        let Some(conn) = self.conns.get_mut(&handle) else { return };
        let ok = status == hci::ErrorCode::Success && on;
        if !conn.encrypting && !conn.encrypted {
            // We never asked for encryption on this link.
            self.stats.link_violations += 1;
            return;
        }
        if conn.encrypted && !conn.encrypting {
            // A change on an encrypted link: only switching it off matters,
            // and Vinculum never does that, so the link is no longer trusted.
            if !ok {
                conn.encrypted = false;
                self.stats.link_violations += 1;
                self.kick(handle);
            }
            return;
        }
        conn.encrypting = false;
        conn.encrypted = ok;
        // LE keys are 16 bytes unless SMP negotiated less, and Vinculum
        // never does; a v2 event that says otherwise is believed.
        let size = key_size.unwrap_or(16);
        if let Some(init) = conn.smp.as_mut() {
            let a = init.encrypted(ok, size);
            if ok && size == 16 {
                self.out_events.push_back(HostEvent::Encrypted { handle, key_size: size });
            }
            self.smp_actions(handle, a);
            if ok && size != 16 {
                self.kick(handle);
            }
        } else if !ok {
            self.out_events.push_back(HostEvent::EncryptionFailed { handle, status: format!("{status:?}") });
        } else if size != 16 {
            self.stats.link_violations += 1;
            self.out_events.push_back(HostEvent::EncryptionFailed { handle, status: format!("key size {size}") });
            self.kick(handle);
        } else {
            self.out_events.push_back(HostEvent::Encrypted { handle, key_size: size });
        }
    }
}

fn smp_addr(msb_first: [u8; 6], random: bool) -> [u8; 7] {
    let a = msb_first;
    [a[5], a[4], a[3], a[2], a[1], a[0], random as u8]
}

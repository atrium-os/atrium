//! The supplicant side of the 4-way and group key handshakes (§12.7.6,
//! §12.7.7), as a state machine with no I/O: feed it received EAPOL frames,
//! carry out the [`Action`]s it returns.
//!
//! Rules from the threat model (wifi.md §8.5): the MIC is checked before
//! key data is decrypted or parsed; the replay counter only moves forward
//! and only on MIC-checked frames; a retransmitted message 3 is answered
//! but never reinstalls keys (KRACK); message 3's RSNE must equal the one
//! the AP advertised, byte for byte (downgrade protection); with PMF the
//! IGTK must come with the GTK.

use crate::crypto::{self, Akm, Pmk, Ptk};
use crate::eapol::{self, info, KeyFrame};
use zeroize::Zeroize;

pub struct Config {
    pub akm: Akm,
    pub pmk: Pmk,
    /// The AP's address (authenticator) and ours (supplicant).
    pub aa: [u8; 6],
    pub spa: [u8; 6],
    /// The RSNE we sent in the association request (whole element).
    pub own_rsne: Vec<u8>,
    /// The RSNE the AP advertised in its beacon or probe response.
    pub ap_rsne: Vec<u8>,
    /// PMF negotiated: an IGTK is required.
    pub pmf: bool,
}

pub enum Action {
    Send(Vec<u8>),
    SetPtk([u8; 16]),
    SetGtk { idx: u8, key: Vec<u8>, rsc: [u8; 6] },
    SetIgtk { idx: u8, key: Vec<u8>, ipn: [u8; 6] },
    /// The 4-way handshake is complete: open the controlled port.
    Authorized,
}

impl Drop for Action {
    fn drop(&mut self) {
        match self {
            Action::SetPtk(k) => k.zeroize(),
            Action::SetGtk { key, .. } | Action::SetIgtk { key, .. } => key.zeroize(),
            _ => {}
        }
    }
}

impl core::fmt::Debug for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Action::Send(b) => write!(f, "Send({} bytes)", b.len()),
            Action::SetPtk(_) => f.write_str("SetPtk"),
            Action::SetGtk { idx, rsc, .. } => write!(f, "SetGtk(idx {idx}, rsc {rsc:02x?})"),
            Action::SetIgtk { idx, ipn, .. } => write!(f, "SetIgtk(idx {idx}, ipn {ipn:02x?})"),
            Action::Authorized => f.write_str("Authorized"),
        }
    }
}

/// Why a frame was ignored. Ignoring is always safe: the AP retransmits,
/// and an attacker's frames must not tear the association down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    Malformed,
    WrongVersion,
    UnexpectedMessage,
    Replay,
    BadMic,
    AnonceChanged,
    KeyData,
    RsneMismatch,
    NoGtk,
    NoIgtk,
}

enum State {
    WaitMsg1,
    WaitMsg3 { anonce: [u8; 32], ptk: Ptk },
    Done { anonce: [u8; 32], ptk: Ptk },
}

pub struct Supplicant {
    cfg: Config,
    snonce: [u8; 32],
    state: State,
    /// Last replay counter of a MIC-checked frame.
    replay: Option<u64>,
}

impl Drop for Supplicant {
    fn drop(&mut self) {
        self.snonce.zeroize();
    }
}

impl Supplicant {
    /// `snonce` must be fresh random bytes for this association.
    pub fn new(cfg: Config, snonce: [u8; 32]) -> Self {
        Supplicant { cfg, snonce, state: State::WaitMsg1, replay: None }
    }

    pub fn authorized(&self) -> bool {
        matches!(self.state, State::Done { .. })
    }

    fn version(&self) -> u16 {
        self.cfg.akm.key_version()
    }

    fn replay_ok(&self, r: u64) -> bool {
        self.replay.is_none_or(|last| r > last)
    }

    fn signed(&self, kck: &[u8; 16], eapol_version: u8, info: u16, replay: u64, nonce: &[u8; 32], data: &[u8]) -> Vec<u8> {
        let mut f = eapol::build(eapol_version, info, 0, replay, nonce, data);
        let m = crypto::mic(self.version(), kck, &f).unwrap_or([0; 16]);
        f[eapol::MIC_OFF..eapol::MIC_OFF + 16].copy_from_slice(&m);
        f
    }

    /// Handles one received EAPOL frame (the whole EAPOL PDU).
    pub fn rx(&mut self, frame: &[u8]) -> Result<Vec<Action>, Ignored> {
        let k = KeyFrame::parse(frame).map_err(|_| Ignored::Malformed)?;
        if k.info & info::VERSION != self.version() {
            return Err(Ignored::WrongVersion);
        }
        if k.has(info::REQUEST) || k.has(info::ERROR) || !k.has(info::ACK) {
            return Err(Ignored::UnexpectedMessage);
        }
        if k.has(info::PAIRWISE) {
            if k.has(info::MIC) {
                self.msg3(frame, &k)
            } else {
                self.msg1(&k)
            }
        } else {
            self.group1(frame, &k)
        }
    }

    fn msg1(&mut self, k: &KeyFrame<'_>) -> Result<Vec<Action>, Ignored> {
        if k.has(info::INSTALL) || k.has(info::SECURE) || k.has(info::ENCRYPTED) {
            return Err(Ignored::UnexpectedMessage);
        }
        // A PTK rekey (msg 1 after the handshake) is not supported in W1;
        // the AP's retransmits time out and it deauthenticates.
        if matches!(self.state, State::Done { .. }) {
            return Err(Ignored::UnexpectedMessage);
        }
        if !self.replay_ok(k.replay) {
            return Err(Ignored::Replay);
        }
        let anonce = *k.nonce;
        let ptk = crypto::ptk(self.cfg.akm, &self.cfg.pmk, &self.cfg.aa, &self.cfg.spa, &anonce, &self.snonce);
        let info = self.version() | info::PAIRWISE | info::MIC;
        let snonce = self.snonce;
        let msg2 = self.signed(&ptk.kck, k.version, info, k.replay, &snonce, &self.cfg.own_rsne.clone());
        self.state = State::WaitMsg3 { anonce, ptk };
        Ok(vec![Action::Send(msg2)])
    }

    fn msg3(&mut self, frame: &[u8], k: &KeyFrame<'_>) -> Result<Vec<Action>, Ignored> {
        let need = info::INSTALL | info::SECURE | info::ENCRYPTED;
        if k.info & need != need {
            return Err(Ignored::UnexpectedMessage);
        }
        let (anonce, ptk, again) = match &self.state {
            State::WaitMsg1 => return Err(Ignored::UnexpectedMessage),
            State::WaitMsg3 { anonce, ptk } => (*anonce, ptk.clone(), false),
            State::Done { anonce, ptk } => (*anonce, ptk.clone(), true),
        };
        if *k.nonce != anonce {
            return Err(Ignored::AnonceChanged);
        }
        if !crypto::mic_ok(self.version(), &ptk.kck, &eapol::zero_mic(frame), k.mic) {
            return Err(Ignored::BadMic);
        }
        if !self.replay_ok(k.replay) {
            return Err(Ignored::Replay);
        }
        let mut plain = crypto::unwrap(&ptk.kek, k.data).ok_or(Ignored::KeyData)?;
        let kd = eapol::parse_key_data(&plain);
        plain.zeroize();
        let kd = kd.map_err(|_| Ignored::KeyData)?;
        if kd.rsne.as_deref() != Some(&self.cfg.ap_rsne[..]) || kd.rsne2 {
            return Err(Ignored::RsneMismatch);
        }
        let Some((gidx, gtk)) = &kd.gtk else {
            return Err(Ignored::NoGtk);
        };
        if gtk.len() != 16 || *gidx == 0 {
            return Err(Ignored::KeyData);
        }
        if self.cfg.pmf && kd.igtk.is_none() {
            return Err(Ignored::NoIgtk);
        }
        self.replay = Some(k.replay);
        let info = self.version() | info::PAIRWISE | info::MIC | info::SECURE;
        let msg4 = self.signed(&ptk.kck, k.version, info, k.replay, &[0; 32], &[]);
        if again {
            // Retransmitted message 3: answer, never reinstall (KRACK).
            return Ok(vec![Action::Send(msg4)]);
        }
        let mut rsc = [0u8; 6];
        rsc.copy_from_slice(&k.rsc[..6]);
        let mut out = vec![
            Action::Send(msg4),
            Action::SetPtk(ptk.tk),
            Action::SetGtk { idx: *gidx, key: gtk.clone(), rsc },
        ];
        if let Some((idx, ipn, key)) = &kd.igtk {
            if key.len() != 16 {
                return Err(Ignored::KeyData);
            }
            out.push(Action::SetIgtk { idx: *idx, key: key.clone(), ipn: *ipn });
        }
        out.push(Action::Authorized);
        self.state = State::Done { anonce, ptk };
        Ok(out)
    }

    fn group1(&mut self, frame: &[u8], k: &KeyFrame<'_>) -> Result<Vec<Action>, Ignored> {
        let need = info::MIC | info::SECURE | info::ENCRYPTED;
        if k.info & need != need || k.has(info::INSTALL) {
            return Err(Ignored::UnexpectedMessage);
        }
        let State::Done { ptk, .. } = &self.state else {
            return Err(Ignored::UnexpectedMessage);
        };
        let ptk = ptk.clone();
        if !crypto::mic_ok(self.version(), &ptk.kck, &eapol::zero_mic(frame), k.mic) {
            return Err(Ignored::BadMic);
        }
        if !self.replay_ok(k.replay) {
            return Err(Ignored::Replay);
        }
        let mut plain = crypto::unwrap(&ptk.kek, k.data).ok_or(Ignored::KeyData)?;
        let kd = eapol::parse_key_data(&plain);
        plain.zeroize();
        let kd = kd.map_err(|_| Ignored::KeyData)?;
        let Some((gidx, gtk)) = &kd.gtk else {
            return Err(Ignored::NoGtk);
        };
        if gtk.len() != 16 || *gidx == 0 {
            return Err(Ignored::KeyData);
        }
        if self.cfg.pmf && kd.igtk.is_none() {
            return Err(Ignored::NoIgtk);
        }
        self.replay = Some(k.replay);
        let info = self.version() | info::MIC | info::SECURE;
        let reply = self.signed(&ptk.kck, k.version, info, k.replay, &[0; 32], &[]);
        let mut rsc = [0u8; 6];
        rsc.copy_from_slice(&k.rsc[..6]);
        let mut out = vec![Action::Send(reply), Action::SetGtk { idx: *gidx, key: gtk.clone(), rsc }];
        if let Some((idx, ipn, key)) = &kd.igtk {
            if key.len() != 16 {
                return Err(Ignored::KeyData);
            }
            out.push(Action::SetIgtk { idx: *idx, key: key.clone(), ipn: *ipn });
        }
        Ok(out)
    }
}

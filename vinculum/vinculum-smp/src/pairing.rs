//! LE Secure Connections pairing, initiator (central) side, sans-I/O
//! (Core 6.0 Vol 3 Part H §2.3, §3).
//!
//! Vinculum connects out to keyboards, mice and headphones, so it is the
//! initiator. Legacy pairing is refused (spec §7, decision 5): a responder
//! without the SC flag fails with Authentication Requirements. The key size
//! must be 16 (KNOB). Every method, Just Works included, waits for the
//! user's confirmation in the trusted UI before keys are made (§7).

use crate::crypto::{f4, f5, f6, g2, KeyPair};
use crate::pdu::{self, SmpChild};
use pdl_runtime::Packet;
use zeroize::Zeroize;

/// The SMP timeout (§3.4): 30 s without progress ends the pairing.
pub const SMP_TIMEOUT_MS: u64 = 30_000;

pub mod auth {
    pub const BONDING: u8 = 0x01;
    pub const MITM: u8 = 0x04;
    pub const SC: u8 = 0x08;
    pub const KEYPRESS: u8 = 0x10;
    pub const CT2: u8 = 0x20;
}

pub mod dist {
    pub const ENC_KEY: u8 = 0x01;
    pub const ID_KEY: u8 = 0x02;
    pub const SIGN_KEY: u8 = 0x04;
    pub const LINK_KEY: u8 = 0x08;
}

/// Pairing Failed reason codes (§3.5.5).
pub mod reason {
    pub const PASSKEY_ENTRY_FAILED: u8 = 0x01;
    pub const AUTHENTICATION_REQUIREMENTS: u8 = 0x03;
    pub const CONFIRM_VALUE_FAILED: u8 = 0x04;
    pub const PAIRING_NOT_SUPPORTED: u8 = 0x05;
    pub const ENCRYPTION_KEY_SIZE: u8 = 0x06;
    pub const UNSPECIFIED_REASON: u8 = 0x08;
    pub const INVALID_PARAMETERS: u8 = 0x0A;
    pub const DHKEY_CHECK_FAILED: u8 = 0x0B;
    pub const NUMERIC_COMPARISON_FAILED: u8 = 0x0C;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IoCap {
    DisplayOnly = 0,
    DisplayYesNo = 1,
    KeyboardOnly = 2,
    NoInputNoOutput = 3,
    KeyboardDisplay = 4,
}

impl IoCap {
    fn from_u8(v: u8) -> Option<IoCap> {
        Some(match v {
            0 => IoCap::DisplayOnly,
            1 => IoCap::DisplayYesNo,
            2 => IoCap::KeyboardOnly,
            3 => IoCap::NoInputNoOutput,
            4 => IoCap::KeyboardDisplay,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    JustWorks,
    NumericComparison,
    /// We show a passkey; the peer types it.
    PasskeyWeDisplay,
    /// The peer shows a passkey (or both type one); we enter it.
    PasskeyWeEnter,
}

impl Method {
    /// Whether the method protects against a man in the middle.
    pub fn authenticated(self) -> bool {
        self != Method::JustWorks
    }
}

/// The SC association model (§2.3.5.1, Table 2.8), initiator's view.
pub fn method(initiator: IoCap, responder: IoCap, mitm: bool) -> Method {
    use IoCap::*;
    use Method::*;
    if !mitm {
        return JustWorks;
    }
    match (initiator, responder) {
        (NoInputNoOutput, _) | (_, NoInputNoOutput) => JustWorks,
        (DisplayOnly, DisplayOnly) | (DisplayOnly, DisplayYesNo) | (DisplayYesNo, DisplayOnly) => JustWorks,
        (DisplayOnly, KeyboardOnly) | (DisplayOnly, KeyboardDisplay) => PasskeyWeDisplay,
        (DisplayYesNo, DisplayYesNo) | (DisplayYesNo, KeyboardDisplay) => NumericComparison,
        (DisplayYesNo, KeyboardOnly) => PasskeyWeDisplay,
        (KeyboardOnly, _) => PasskeyWeEnter,
        (KeyboardDisplay, DisplayOnly) => PasskeyWeEnter,
        (KeyboardDisplay, DisplayYesNo) | (KeyboardDisplay, KeyboardDisplay) => NumericComparison,
        (KeyboardDisplay, KeyboardOnly) => PasskeyWeDisplay,
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub io_cap: IoCap,
    /// Our IRK and identity address, distributed when the peer asks.
    pub local_irk: [u8; 16],
    /// Identity address type (0 public, 1 random static) and address
    /// (little-endian).
    pub local_identity: (u8, [u8; 6]),
}

/// The keys and their strength, once pairing completes.
#[derive(Clone, PartialEq, Eq)]
pub struct Bond {
    pub ltk: [u8; 16],
    pub key_size: u8,
    pub secure_connections: bool,
    pub authenticated: bool,
    pub method: Method,
    pub peer_irk: Option<[u8; 16]>,
    /// Peer identity address type and address (little-endian).
    pub peer_identity: Option<(u8, [u8; 6])>,
}

impl std::fmt::Debug for Bond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bond")
            .field("ltk", &"<redacted>")
            .field("key_size", &self.key_size)
            .field("secure_connections", &self.secure_connections)
            .field("authenticated", &self.authenticated)
            .field("method", &self.method)
            .field("peer_irk", &self.peer_irk.map(|_| "<redacted>"))
            .field("peer_identity", &self.peer_identity)
            .finish()
    }
}

impl Drop for Bond {
    fn drop(&mut self) {
        self.ltk.zeroize();
        if let Some(k) = self.peer_irk.as_mut() {
            k.zeroize();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this SMP PDU on the SMP fixed channel (CID 0x0006).
    Send(Vec<u8>),
    /// Ask the user to confirm pairing. `number` is the six digits to compare
    /// for numeric comparison, None for Just Works.
    Confirm { number: Option<u32> },
    /// Show this passkey to the user; the peer types it.
    Display { passkey: u32 },
    /// Ask the user for the passkey the peer shows.
    EnterPasskey,
    /// Start LE encryption with this key (LE_Start_Encryption, EDIV 0,
    /// Rand 0); report the result with [`Initiator::encrypted`].
    StartEncryption { ltk: [u8; 16] },
    Done(Box<Bond>),
    Failed { reason: u8, why: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    WaitResponse,
    WaitPublicKey,
    /// Passkey: waiting for the user's passkey before round 0.
    WaitPasskey,
    WaitConfirm,
    WaitRandom,
    WaitUser,
    WaitDhKeyCheck,
    WaitEncryption,
    WaitIdInfo,
    WaitIdAddr,
    Done,
    Failed,
}

pub struct Initiator {
    cfg: Config,
    state: State,
    a: [u8; 7],
    b: [u8; 7],
    preq: [u8; 6],
    pres: [u8; 6],
    keys: KeyPair,
    pkb: [u8; 64],
    dhkey: [u8; 32],
    na: [u8; 16],
    nb: [u8; 16],
    cb: [u8; 16],
    method: Method,
    passkey: u32,
    round: u8,
    mackey: [u8; 16],
    ltk: [u8; 16],
    resp_dist: u8,
    init_dist: u8,
    peer_irk: Option<[u8; 16]>,
    peer_identity: Option<(u8, [u8; 6])>,
    deadline: u64,
}

impl std::fmt::Debug for Initiator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initiator").field("state", &self.state).field("method", &self.method).finish_non_exhaustive()
    }
}

impl Drop for Initiator {
    fn drop(&mut self) {
        self.dhkey.zeroize();
        self.mackey.zeroize();
        self.ltk.zeroize();
        self.na.zeroize();
        self.nb.zeroize();
        self.passkey.zeroize();
    }
}

fn random16() -> [u8; 16] {
    use rand_core::RngCore;
    let mut r = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut r);
    r
}

fn random_passkey() -> u32 {
    use rand_core::RngCore;
    // Rejection sampling for an unbiased 0..=999999.
    loop {
        let v = rand_core::OsRng.next_u32() & 0x000F_FFFF; // < 1_048_576
        if v < 1_000_000 {
            return v;
        }
    }
}

fn encode<P>(p: P) -> Vec<u8>
where
    P: TryInto<pdu::Smp>,
    P::Error: std::fmt::Debug,
{
    let s: pdu::Smp = p.try_into().expect("smp encode");
    s.encode_to_vec().expect("smp encode")
}

fn x_of(pk: &[u8; 64]) -> [u8; 32] {
    pk[..32].try_into().unwrap()
}

impl Initiator {
    /// Starts pairing. `a` is our address (6 bytes little-endian + type),
    /// `b` the peer's, as used on this connection.
    pub fn start(cfg: Config, a: [u8; 7], b: [u8; 7], now_ms: u64) -> (Initiator, Vec<Action>) {
        let preq = [
            cfg.io_cap as u8,
            0, // no OOB data
            auth::BONDING | auth::MITM | auth::SC | auth::CT2,
            16,
            dist::ID_KEY,
            dist::ID_KEY,
        ];
        let req = pdu::PairingRequest {
            io_capability: preq[0],
            oob_data_flag: preq[1],
            auth_req: preq[2],
            maximum_encryption_key_size: preq[3],
            initiator_key_distribution: preq[4],
            responder_key_distribution: preq[5],
        };
        let me = Initiator {
            cfg,
            state: State::WaitResponse,
            a,
            b,
            preq,
            pres: [0; 6],
            keys: KeyPair::generate(),
            pkb: [0; 64],
            dhkey: [0; 32],
            na: [0; 16],
            nb: [0; 16],
            cb: [0; 16],
            method: Method::JustWorks,
            passkey: 0,
            round: 0,
            mackey: [0; 16],
            ltk: [0; 16],
            resp_dist: 0,
            init_dist: 0,
            peer_irk: None,
            peer_identity: None,
            deadline: now_ms + SMP_TIMEOUT_MS,
        };
        (me, vec![Action::Send(encode(req))])
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn deadline(&self) -> Option<u64> {
        (!matches!(self.state, State::Done | State::Failed)).then_some(self.deadline)
    }

    pub fn finished(&self) -> bool {
        matches!(self.state, State::Done | State::Failed)
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        if !self.finished() && now_ms >= self.deadline {
            self.state = State::Failed;
            // §3.4: on timeout no PDU is sent; the link needs a new
            // connection before SMP can run again.
            return vec![Action::Failed { reason: reason::UNSPECIFIED_REASON, why: "SMP timeout" }];
        }
        vec![]
    }

    fn fail(&mut self, r: u8, why: &'static str) -> Vec<Action> {
        self.state = State::Failed;
        vec![Action::Send(encode(pdu::PairingFailed { reason: r })), Action::Failed { reason: r, why }]
    }

    /// An SMP PDU from the peer.
    pub fn on_pdu(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Action> {
        if self.finished() {
            return vec![];
        }
        let Ok(smp) = pdu::Smp::decode_full(bytes) else {
            return self.fail(reason::INVALID_PARAMETERS, "malformed SMP PDU");
        };
        let Ok(child) = smp.specialize() else {
            return self.fail(reason::INVALID_PARAMETERS, "malformed SMP PDU");
        };
        if let SmpChild::PairingFailed(f) = child {
            self.state = State::Failed;
            return vec![Action::Failed { reason: f.reason, why: "peer failed the pairing" }];
        }
        self.deadline = now_ms + SMP_TIMEOUT_MS;
        match (self.state, child) {
            (State::WaitResponse, SmpChild::PairingResponse(r)) => self.on_response(r),
            (State::WaitPublicKey, SmpChild::PairingPublicKey(p)) => self.on_public_key(p),
            (State::WaitConfirm, SmpChild::PairingConfirm(c)) => self.on_confirm(c),
            (State::WaitRandom, SmpChild::PairingRandom(r)) => self.on_random(r),
            (State::WaitDhKeyCheck, SmpChild::PairingDhKeyCheck(c)) => self.on_dhkey_check(c),
            (State::WaitIdInfo, SmpChild::IdentityInformation(i)) => {
                self.peer_irk = Some(i.identity_resolving_key);
                self.state = State::WaitIdAddr;
                vec![]
            }
            (State::WaitIdAddr, SmpChild::IdentityAddressInformation(i)) => {
                if i.addr_type > 1 {
                    return self.fail(reason::INVALID_PARAMETERS, "bad identity address type");
                }
                self.peer_identity = Some((i.addr_type, i.bd_addr));
                self.distribute_ours()
            }
            // Keypress notifications are informational.
            (_, SmpChild::PairingKeypressNotification(_)) => vec![],
            // A peripheral asking for security mid-pairing: ignore.
            (_, SmpChild::SecurityRequest(_)) => vec![],
            _ => self.fail(reason::UNSPECIFIED_REASON, "unexpected SMP PDU"),
        }
    }

    fn on_response(&mut self, r: pdu::PairingResponse) -> Vec<Action> {
        let Some(resp_io) = IoCap::from_u8(r.io_capability) else {
            return self.fail(reason::INVALID_PARAMETERS, "bad IO capability");
        };
        if r.auth_req & auth::SC == 0 {
            return self.fail(reason::AUTHENTICATION_REQUIREMENTS, "peer offers only legacy pairing");
        }
        if r.maximum_encryption_key_size != 16 {
            return self.fail(reason::ENCRYPTION_KEY_SIZE, "peer key size under 16 bytes");
        }
        self.pres = [
            r.io_capability,
            r.oob_data_flag,
            r.auth_req,
            r.maximum_encryption_key_size,
            r.initiator_key_distribution,
            r.responder_key_distribution,
        ];
        // Distribution is what both asked for; LinkKey and EncKey are not
        // used in SC (the LTK is derived), SignKey we don't use.
        self.init_dist = self.preq[4] & r.initiator_key_distribution & dist::ID_KEY;
        self.resp_dist = self.preq[5] & r.responder_key_distribution & dist::ID_KEY;
        let mitm = (self.preq[2] | r.auth_req) & auth::MITM != 0;
        self.method = method(self.cfg.io_cap, resp_io, mitm);
        self.state = State::WaitPublicKey;
        let pk = self.keys.public;
        vec![Action::Send(encode(pdu::PairingPublicKey {
            public_key_x: pk[..32].try_into().unwrap(),
            public_key_y: pk[32..].try_into().unwrap(),
        }))]
    }

    fn on_public_key(&mut self, p: pdu::PairingPublicKey) -> Vec<Action> {
        let mut pkb = [0u8; 64];
        pkb[..32].copy_from_slice(&p.public_key_x);
        pkb[32..].copy_from_slice(&p.public_key_y);
        match self.keys.dhkey(&pkb) {
            Ok(d) => self.dhkey = d,
            Err(_) => return self.fail(reason::DHKEY_CHECK_FAILED, "invalid or reflected public key"),
        }
        self.pkb = pkb;
        match self.method {
            Method::JustWorks | Method::NumericComparison => {
                self.state = State::WaitConfirm;
                vec![]
            }
            Method::PasskeyWeDisplay => {
                self.passkey = random_passkey();
                let mut v = vec![Action::Display { passkey: self.passkey }];
                v.extend(self.passkey_round());
                v
            }
            Method::PasskeyWeEnter => {
                self.state = State::WaitPasskey;
                vec![Action::EnterPasskey]
            }
        }
    }

    /// The user's answer to [`Action::EnterPasskey`]; None cancels.
    pub fn passkey_entered(&mut self, passkey: Option<u32>) -> Vec<Action> {
        if self.state != State::WaitPasskey {
            return vec![];
        }
        match passkey {
            Some(p) if p <= 999_999 => {
                self.passkey = p;
                self.passkey_round()
            }
            _ => self.fail(reason::PASSKEY_ENTRY_FAILED, "user cancelled passkey entry"),
        }
    }

    /// Passkey round `self.round`: send Cai.
    fn passkey_round(&mut self) -> Vec<Action> {
        self.na = random16();
        let bit = ((self.passkey >> self.round) & 1) as u8;
        let ca = f4(&x_of(&self.keys.public), &x_of(&self.pkb), &self.na, 0x80 | bit);
        self.state = State::WaitConfirm;
        vec![Action::Send(encode(pdu::PairingConfirm { confirm_value: ca }))]
    }

    fn passkey_mode(&self) -> bool {
        matches!(self.method, Method::PasskeyWeDisplay | Method::PasskeyWeEnter)
    }

    fn on_confirm(&mut self, c: pdu::PairingConfirm) -> Vec<Action> {
        self.cb = c.confirm_value;
        if !self.passkey_mode() {
            self.na = random16();
        }
        self.state = State::WaitRandom;
        vec![Action::Send(encode(pdu::PairingRandom { random_value: self.na }))]
    }

    fn on_random(&mut self, r: pdu::PairingRandom) -> Vec<Action> {
        self.nb = r.random_value;
        let pka_x = x_of(&self.keys.public);
        let pkb_x = x_of(&self.pkb);
        if self.passkey_mode() {
            let bit = ((self.passkey >> self.round) & 1) as u8;
            if f4(&pkb_x, &pka_x, &self.nb, 0x80 | bit) != self.cb {
                return self.fail(reason::CONFIRM_VALUE_FAILED, "passkey confirm mismatch");
            }
            self.round += 1;
            if self.round < 20 {
                return self.passkey_round();
            }
            // After 20 rounds the last Na/Nb feed f5/f6; the user already
            // acted (typed or saw the passkey).
            return self.stage2();
        }
        if f4(&pkb_x, &pka_x, &self.nb, 0) != self.cb {
            return self.fail(reason::CONFIRM_VALUE_FAILED, "confirm value mismatch");
        }
        self.state = State::WaitUser;
        let number = (self.method == Method::NumericComparison).then(|| g2(&pka_x, &pkb_x, &self.na, &self.nb) % 1_000_000);
        vec![Action::Confirm { number }]
    }

    /// The user's answer to [`Action::Confirm`].
    pub fn confirmed(&mut self, yes: bool) -> Vec<Action> {
        if self.state != State::WaitUser {
            return vec![];
        }
        if !yes {
            let r = if self.method == Method::NumericComparison { reason::NUMERIC_COMPARISON_FAILED } else { reason::UNSPECIFIED_REASON };
            return self.fail(r, "user declined");
        }
        self.stage2()
    }

    fn r(&self) -> [u8; 16] {
        let mut r = [0u8; 16];
        if self.passkey_mode() {
            r[..4].copy_from_slice(&self.passkey.to_le_bytes());
        }
        r
    }

    fn stage2(&mut self) -> Vec<Action> {
        let (mackey, ltk) = f5(&self.dhkey, &self.na, &self.nb, &self.a, &self.b);
        self.mackey = mackey;
        self.ltk = ltk;
        let io_a: [u8; 3] = self.preq[..3].try_into().unwrap();
        let ea = f6(&self.mackey, &self.na, &self.nb, &self.r(), &io_a, &self.a, &self.b);
        self.state = State::WaitDhKeyCheck;
        vec![Action::Send(encode(pdu::PairingDhKeyCheck { dhkey_check: ea }))]
    }

    fn on_dhkey_check(&mut self, c: pdu::PairingDhKeyCheck) -> Vec<Action> {
        let io_b: [u8; 3] = self.pres[..3].try_into().unwrap();
        let eb = f6(&self.mackey, &self.nb, &self.na, &self.r(), &io_b, &self.b, &self.a);
        if eb != c.dhkey_check {
            return self.fail(reason::DHKEY_CHECK_FAILED, "DHKey check mismatch");
        }
        self.state = State::WaitEncryption;
        vec![Action::StartEncryption { ltk: self.ltk }]
    }

    /// The link's encryption result after [`Action::StartEncryption`]
    /// (Encryption Change), with the controller's key size.
    pub fn encrypted(&mut self, ok: bool, key_size: u8) -> Vec<Action> {
        if self.state != State::WaitEncryption {
            return vec![];
        }
        if !ok {
            self.state = State::Failed;
            return vec![Action::Failed { reason: reason::UNSPECIFIED_REASON, why: "encryption failed" }];
        }
        if key_size != 16 {
            return self.fail(reason::ENCRYPTION_KEY_SIZE, "encrypted with a key under 16 bytes");
        }
        if self.resp_dist & dist::ID_KEY != 0 {
            self.state = State::WaitIdInfo;
            return vec![];
        }
        self.distribute_ours()
    }

    fn distribute_ours(&mut self) -> Vec<Action> {
        let mut v = Vec::new();
        if self.init_dist & dist::ID_KEY != 0 {
            v.push(Action::Send(encode(pdu::IdentityInformation { identity_resolving_key: self.cfg.local_irk })));
            v.push(Action::Send(encode(pdu::IdentityAddressInformation {
                addr_type: self.cfg.local_identity.0,
                bd_addr: self.cfg.local_identity.1,
            })));
        }
        self.state = State::Done;
        v.push(Action::Done(Box::new(Bond {
            ltk: self.ltk,
            key_size: 16,
            secure_connections: true,
            authenticated: self.method.authenticated(),
            method: self.method,
            peer_irk: self.peer_irk,
            peer_identity: self.peer_identity,
        })));
        v
    }
}

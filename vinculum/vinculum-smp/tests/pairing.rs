//! The SC initiator against an honest test responder built straight from
//! the crypto functions (Core 6.0 Vol 3 Part H §2.3.5.6), plus the attacks
//! and policy refusals the initiator must catch.

use vinculum_smp::crypto::{f4, f5, f6, KeyPair};
use vinculum_smp::pairing::{auth, method, reason, Action, Bond, Config, Initiator, IoCap, Method, SMP_TIMEOUT_MS};

const A: [u8; 7] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x00];
const B: [u8; 7] = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xC6, 0x01];
const PEER_IRK: [u8; 16] = [0x5A; 16];

fn cfg(io: IoCap) -> Config {
    Config { io_cap: io, local_irk: [0x77; 16], local_identity: (0, [1, 2, 3, 4, 5, 6]) }
}

fn x(pk: &[u8; 64]) -> [u8; 32] {
    pk[..32].try_into().unwrap()
}

fn arr<const N: usize>(b: &[u8]) -> [u8; N] {
    b.try_into().unwrap()
}

/// Knobs for a misbehaving responder.
#[derive(Default, Clone)]
struct Evil {
    legacy: bool,
    key_size: Option<u8>,
    reflect_key: bool,
    bad_confirm: bool,
    bad_dhkey_check: bool,
    /// Passkey the responder believes in, when it differs from the user's.
    wrong_passkey: Option<u32>,
}

struct Responder {
    io: IoCap,
    auth_req: u8,
    evil: Evil,
    keys: KeyPair,
    preq: [u8; 6],
    pres: [u8; 6],
    pka: [u8; 64],
    dhkey: [u8; 32],
    na: [u8; 16],
    nb: [u8; 16],
    cai: [u8; 16],
    method: Method,
    passkey: u32,
    round: u32,
    ltk: Option<[u8; 16]>,
}

impl Responder {
    fn new(io: IoCap, evil: Evil) -> Responder {
        Responder {
            io,
            auth_req: if evil.legacy { auth::BONDING | auth::MITM } else { auth::BONDING | auth::MITM | auth::SC },
            evil,
            keys: KeyPair::generate(),
            preq: [0; 6],
            pres: [0; 6],
            pka: [0; 64],
            dhkey: [0; 32],
            na: [0; 16],
            nb: [0; 16],
            cai: [0; 16],
            method: Method::JustWorks,
            passkey: 0,
            round: 0,
            ltk: None,
        }
    }

    fn passkey_mode(&self) -> bool {
        matches!(self.method, Method::PasskeyWeDisplay | Method::PasskeyWeEnter)
    }

    fn r(&self) -> [u8; 16] {
        let mut r = [0u8; 16];
        if self.passkey_mode() {
            r[..4].copy_from_slice(&self.passkey.to_le_bytes());
        }
        r
    }

    fn bit(&self) -> u8 {
        0x80 | ((self.passkey >> self.round) & 1) as u8
    }

    /// One PDU from the initiator; returns the PDUs to send back.
    fn on(&mut self, p: &[u8]) -> Vec<Vec<u8>> {
        match p[0] {
            0x01 => {
                self.preq = arr(&p[1..7]);
                self.pres = [self.io as u8, 0, self.auth_req, self.evil.key_size.unwrap_or(16), 0x02, 0x02];
                let mitm = (self.preq[2] | self.pres[2]) & auth::MITM != 0;
                self.method = method(IoCap::from_test(self.preq[0]), self.io, mitm);
                let mut v = vec![0x02];
                v.extend_from_slice(&self.pres);
                vec![v]
            }
            0x0C => {
                self.pka = arr(&p[1..65]);
                self.dhkey = self.keys.dhkey(&self.pka).expect("initiator key valid");
                let mut out = vec![0x0C];
                out.extend_from_slice(if self.evil.reflect_key { &self.pka } else { &self.keys.public });
                let mut v = vec![out];
                if !self.passkey_mode() {
                    self.nb = rand16();
                    let mut cb = f4(&x(&self.keys.public), &x(&self.pka), &self.nb, 0);
                    if self.evil.bad_confirm {
                        cb[0] ^= 1;
                    }
                    let mut c = vec![0x03];
                    c.extend_from_slice(&cb);
                    v.push(c);
                }
                v
            }
            0x03 => {
                assert!(self.passkey_mode(), "initiator sends Confirm only in passkey mode");
                self.cai = arr(&p[1..17]);
                self.nb = rand16();
                let cb = f4(&x(&self.keys.public), &x(&self.pka), &self.nb, self.bit());
                let mut c = vec![0x03];
                c.extend_from_slice(&cb);
                vec![c]
            }
            0x04 => {
                self.na = arr(&p[1..17]);
                if self.passkey_mode() {
                    let want = f4(&x(&self.pka), &x(&self.keys.public), &self.na, self.bit());
                    if want != self.cai {
                        return vec![vec![0x05, reason::CONFIRM_VALUE_FAILED]];
                    }
                    self.round += 1;
                }
                let mut v = vec![0x04];
                v.extend_from_slice(&self.nb);
                vec![v]
            }
            0x0D => {
                let (mackey, ltk) = f5(&self.dhkey, &self.na, &self.nb, &A, &B);
                let ea = f6(&mackey, &self.na, &self.nb, &self.r(), &arr(&self.preq[..3]), &A, &B);
                if ea[..] != p[1..17] {
                    return vec![vec![0x05, reason::DHKEY_CHECK_FAILED]];
                }
                let mut eb = f6(&mackey, &self.nb, &self.na, &self.r(), &arr(&self.pres[..3]), &B, &A);
                if self.evil.bad_dhkey_check {
                    eb[15] ^= 0x80;
                }
                self.ltk = Some(ltk);
                let mut v = vec![0x0D];
                v.extend_from_slice(&eb);
                vec![v]
            }
            0x05 | 0x08 | 0x09 => vec![],
            c => panic!("responder got unexpected code {c:#x}"),
        }
    }

    /// Key distribution after encryption.
    fn keys_out(&self) -> Vec<Vec<u8>> {
        let mut i = vec![0x08];
        i.extend_from_slice(&PEER_IRK);
        let mut a = vec![0x09, 0x01];
        a.extend_from_slice(&B[..6]);
        vec![i, a]
    }
}

trait FromTest {
    fn from_test(v: u8) -> IoCap;
}
impl FromTest for IoCap {
    fn from_test(v: u8) -> IoCap {
        [IoCap::DisplayOnly, IoCap::DisplayYesNo, IoCap::KeyboardOnly, IoCap::NoInputNoOutput, IoCap::KeyboardDisplay][v as usize]
    }
}

fn rand16() -> [u8; 16] {
    KeyPair::generate().public[..16].try_into().unwrap()
}

#[derive(Debug)]
enum Outcome {
    Bond(Box<Bond>),
    Failed(u8, &'static str),
}

/// Runs a pairing to its end. The "user" says yes (or `user_yes`) and types
/// whatever passkey the responder side shows.
fn run(init_io: IoCap, resp_io: IoCap, evil: Evil, user_yes: bool) -> (Outcome, Vec<Action>, Option<[u8; 16]>) {
    let mut resp = Responder::new(resp_io, evil);
    let (mut ini, first) = Initiator::start(cfg(init_io), A, B, 0);
    let mut pending: std::collections::VecDeque<Action> = first.into();
    let mut ui = Vec::new();
    let mut now = 0;
    while let Some(act) = pending.pop_front() {
        now += 1;
        match act {
            Action::Send(p) => {
                for reply in resp.on(&p) {
                    pending.extend(ini.on_pdu(&reply, now));
                }
            }
            Action::Confirm { number } => {
                ui.push(Action::Confirm { number });
                pending.extend(ini.confirmed(user_yes));
            }
            Action::Display { passkey } => {
                ui.push(Action::Display { passkey });
                // The peer's user types what we show (or a typo).
                resp.passkey = resp.evil.wrong_passkey.unwrap_or(passkey);
            }
            Action::EnterPasskey => {
                ui.push(Action::EnterPasskey);
                resp.passkey = 123_456;
                let typed = resp.evil.wrong_passkey.unwrap_or(123_456);
                pending.extend(ini.passkey_entered(user_yes.then_some(typed)));
            }
            Action::StartEncryption { ltk } => {
                assert_eq!(Some(ltk), resp.ltk, "both sides derive one LTK");
                pending.extend(ini.encrypted(true, 16));
                for k in resp.keys_out() {
                    pending.extend(ini.on_pdu(&k, now));
                }
            }
            Action::Done(b) => {
                assert!(ini.finished());
                return (Outcome::Bond(b), ui, resp.ltk);
            }
            Action::Failed { reason, why } => {
                assert!(ini.finished());
                return (Outcome::Failed(reason, why), ui, resp.ltk);
            }
        }
    }
    panic!("pairing stalled");
}

fn bonded(o: Outcome) -> Box<Bond> {
    match o {
        Outcome::Bond(b) => b,
        Outcome::Failed(r, why) => panic!("pairing failed: {r:#x} {why}"),
    }
}

fn failed(o: Outcome) -> u8 {
    match o {
        Outcome::Failed(r, _) => r,
        Outcome::Bond(_) => panic!("pairing should have failed"),
    }
}

#[test]
fn just_works_needs_the_user_and_is_unauthenticated() {
    let (o, ui, ltk) = run(IoCap::KeyboardDisplay, IoCap::NoInputNoOutput, Evil::default(), true);
    let b = bonded(o);
    assert_eq!(ui, vec![Action::Confirm { number: None }]);
    assert_eq!(b.method, Method::JustWorks);
    assert!(!b.authenticated && b.secure_connections && b.key_size == 16);
    assert_eq!(Some(b.ltk), ltk);
    assert_eq!(b.peer_irk, Some(PEER_IRK));
    assert_eq!(b.peer_identity, Some((1, arr(&B[..6]))));
    assert!(format!("{b:?}").contains("ltk: \"<redacted>\""), "Debug redacts the LTK");
}

#[test]
fn numeric_comparison_shows_six_digits() {
    let (o, ui, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayYesNo, Evil::default(), true);
    let b = bonded(o);
    assert_eq!(b.method, Method::NumericComparison);
    assert!(b.authenticated);
    match ui.as_slice() {
        [Action::Confirm { number: Some(n) }] => assert!(*n < 1_000_000),
        u => panic!("unexpected UI {u:?}"),
    }
}

#[test]
fn user_declining_fails_the_pairing() {
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayYesNo, Evil::default(), false);
    assert_eq!(failed(o), reason::NUMERIC_COMPARISON_FAILED);
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::NoInputNoOutput, Evil::default(), false);
    assert_eq!(failed(o), reason::UNSPECIFIED_REASON);
}

#[test]
fn passkey_we_display_for_a_keyboard() {
    let (o, ui, _) = run(IoCap::KeyboardDisplay, IoCap::KeyboardOnly, Evil::default(), true);
    let b = bonded(o);
    assert_eq!(b.method, Method::PasskeyWeDisplay);
    assert!(b.authenticated);
    assert!(matches!(ui.as_slice(), [Action::Display { passkey }] if *passkey <= 999_999));
}

#[test]
fn passkey_we_enter() {
    let (o, ui, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayOnly, Evil::default(), true);
    assert_eq!(bonded(o).method, Method::PasskeyWeEnter);
    assert_eq!(ui, vec![Action::EnterPasskey]);
}

#[test]
fn wrong_passkey_fails_in_a_round() {
    // Bit 0 differs, so round 0 already fails.
    let evil = Evil { wrong_passkey: Some(123_457), ..Evil::default() };
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayOnly, evil.clone(), true);
    assert_eq!(failed(o), reason::CONFIRM_VALUE_FAILED);
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::KeyboardOnly, Evil { wrong_passkey: Some(0), ..evil }, true);
    // Whichever side notices first fails it.
    let r = failed(o);
    assert!(r == reason::CONFIRM_VALUE_FAILED || r == reason::PASSKEY_ENTRY_FAILED, "{r:#x}");
}

#[test]
fn cancelled_passkey_entry() {
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayOnly, Evil::default(), false);
    assert_eq!(failed(o), reason::PASSKEY_ENTRY_FAILED);
}

#[test]
fn legacy_pairing_refused() {
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::KeyboardOnly, Evil { legacy: true, ..Evil::default() }, true);
    assert_eq!(failed(o), reason::AUTHENTICATION_REQUIREMENTS);
}

#[test]
fn short_keys_refused() {
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::KeyboardOnly, Evil { key_size: Some(7), ..Evil::default() }, true);
    assert_eq!(failed(o), reason::ENCRYPTION_KEY_SIZE);
}

#[test]
fn reflected_public_key_refused() {
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::NoInputNoOutput, Evil { reflect_key: true, ..Evil::default() }, true);
    assert_eq!(failed(o), reason::DHKEY_CHECK_FAILED);
}

#[test]
fn bad_confirm_and_bad_dhkey_check_refused() {
    let (o, ui, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayYesNo, Evil { bad_confirm: true, ..Evil::default() }, true);
    assert_eq!(failed(o), reason::CONFIRM_VALUE_FAILED);
    assert!(ui.is_empty(), "the user is never asked about a forged confirm");
    let (o, _, _) = run(IoCap::KeyboardDisplay, IoCap::DisplayYesNo, Evil { bad_dhkey_check: true, ..Evil::default() }, true);
    assert_eq!(failed(o), reason::DHKEY_CHECK_FAILED);
}

#[test]
fn encryption_with_a_short_key_fails() {
    let mut resp = Responder::new(IoCap::NoInputNoOutput, Evil::default());
    let (mut ini, first) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 0);
    let mut pending: std::collections::VecDeque<Action> = first.into();
    while let Some(a) = pending.pop_front() {
        match a {
            Action::Send(p) => resp.on(&p).iter().for_each(|r| pending.extend(ini.on_pdu(r, 1))),
            Action::Confirm { .. } => pending.extend(ini.confirmed(true)),
            Action::StartEncryption { .. } => {
                let out = ini.encrypted(true, 7);
                assert!(matches!(out.last(), Some(Action::Failed { reason: reason::ENCRYPTION_KEY_SIZE, .. })));
                return;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("never reached encryption");
}

#[test]
fn malformed_and_out_of_order_pdus() {
    let (mut ini, _) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 0);
    let out = ini.on_pdu(&[0x02, 1, 2], 1);
    assert!(matches!(out.last(), Some(Action::Failed { reason: reason::INVALID_PARAMETERS, .. })));
    assert!(ini.on_pdu(&[0x02, 1, 0, 0x0D, 16, 2, 2], 2).is_empty(), "dead after failing");

    let (mut ini, _) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 0);
    let mut dhcheck = vec![0x0D];
    dhcheck.extend_from_slice(&[0; 16]);
    let out = ini.on_pdu(&dhcheck, 1);
    assert!(matches!(out.last(), Some(Action::Failed { reason: reason::UNSPECIFIED_REASON, .. })));

    for io in [5u8, 0xFF] {
        let (mut ini, _) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 0);
        let out = ini.on_pdu(&[0x02, io, 0, 0x0D, 16, 2, 2], 1);
        assert!(matches!(out.last(), Some(Action::Failed { reason: reason::INVALID_PARAMETERS, .. })));
    }
}

#[test]
fn peer_failure_and_timeout() {
    let (mut ini, _) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 0);
    assert_eq!(ini.on_pdu(&[0x05, 0x08], 1), vec![Action::Failed { reason: 0x08, why: "peer failed the pairing" }]);

    let (mut ini, _) = Initiator::start(cfg(IoCap::KeyboardDisplay), A, B, 100);
    assert_eq!(ini.deadline(), Some(100 + SMP_TIMEOUT_MS));
    assert!(ini.tick(100 + SMP_TIMEOUT_MS - 1).is_empty());
    let out = ini.tick(100 + SMP_TIMEOUT_MS);
    assert!(matches!(out.as_slice(), [Action::Failed { .. }]), "no PDU on timeout");
    assert_eq!(ini.deadline(), None);
}

#[test]
fn association_table_corners() {
    use IoCap::*;
    assert_eq!(method(KeyboardDisplay, KeyboardOnly, false), Method::JustWorks, "no MITM asked: Just Works");
    assert_eq!(method(DisplayYesNo, KeyboardOnly, true), Method::PasskeyWeDisplay);
    assert_eq!(method(KeyboardOnly, KeyboardOnly, true), Method::PasskeyWeEnter);
    assert_eq!(method(DisplayYesNo, DisplayOnly, true), Method::JustWorks);
}

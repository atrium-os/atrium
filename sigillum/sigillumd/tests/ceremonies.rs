//! S1's gate (sigillum.md §16): the broker, the FIDO driver and a soft key,
//! wired the way sigillumd will wire them, with a relying party at the end
//! verifying what comes out.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use p256::ecdsa::{signature::Verifier, DerSignature, VerifyingKey};
use sha2::{Digest, Sha256};
use sigillum_cbor::Value;
use sigillum_ctap::authdata::{flags, AuthData};
use sigillum_ctap::hid::{HidDevice, REPORT};
use sigillum_fido::Driver;
use sigillum_proto::client::{Assert, Attestation, Purpose, Refusal, Register, Reply, Request, Strength, Uv, VerifyUser};
use sigillum_proto::driver::{FromDriver, Pin};
use sigillum_softkey::{Lie, SoftKey};
use sigillumd::broker::{Action, Broker, Choice, Config, Key, Sheet};
use sigillumd::label::{AppId, Label};
use sigillumd::platform::{Arca, Platform, Wire};
use sigillumd::broker::PlatformResult;
use zeroize::Zeroizing;

const DEV: u64 = 1;
const PIN: &[u8] = b"4711";

#[derive(Clone)]
struct Key_(Rc<RefCell<SoftKey>>);

impl HidDevice for Key_ {
    fn write_report(&mut self, r: &[u8; REPORT]) -> Result<(), sigillum_ctap::Error> {
        self.0.borrow_mut().write_report(r)
    }
    fn read_report(&mut self, t: u32) -> Result<[u8; REPORT], sigillum_ctap::Error> {
        self.0.borrow_mut().read_report(t)
    }
}

fn broker_app() -> AppId {
    AppId { publisher: [0xAB; 32], app_id: "org.atrium.navigator".into() }
}

fn web_broker() -> Label {
    Label::App { user: "alice".into(), app: broker_app(), rp_ids: vec![] }
}

fn some_app() -> Label {
    Label::App { user: "alice".into(), app: AppId { publisher: [0x11; 32], app_id: "com.example.mail".into() }, rp_ids: vec!["mail.example.com".into()] }
}

struct Rig {
    broker: Broker,
    driver: Driver<Key_>,
    key: Rc<RefCell<SoftKey>>,
    /// The PIN the "user" types when asked; None = they cancel.
    pin: Option<&'static [u8]>,
    pin_asks: usize,
    replies: Vec<Reply>,
    sheets: Vec<(Key, Sheet)>,
    audit: Vec<String>,
    /// A record to substitute for the driver's next one (a lying driver).
    tamper: Option<Box<dyn FnMut(FromDriver) -> FromDriver>>,
    /// Arca, in process (the real arcad request handler), when offered.
    arca: Option<Arca<InProc>>,
    /// What ostiarius answers when the passcode is asked.
    passcode_ok: bool,
    passcode_asks: usize,
    /// Answers for the next passcode checks, before `passcode_ok`.
    passcode_answers: std::collections::VecDeque<bool>,
    /// Substitute for Arca's next answer (a lying arcad).
    arca_tamper: Option<Box<dyn FnMut(PlatformResult) -> PlatformResult>>,
}

/// arcad's handler behind the `Wire` sigillumd speaks, as `sigillumd`'s
/// uid would reach it.
struct InProc(Arc<Mutex<arcad::server::State>>, #[allow(dead_code)] tempfile::TempDir);

impl Wire for InProc {
    fn call(&mut self, op: u16, body: &[u8]) -> Option<(u8, Vec<u8>)> {
        let r = self.0.lock().unwrap().handle(&arcad::label::Label::Service("sigillumd".into()), op, body);
        Some((r.status, r.body.bytes().to_vec()))
    }
}

/// A fresh arcad with alice's store unlocked.
fn arca() -> Arca<InProc> {
    let d = tempfile::tempdir().unwrap();
    let mut s = arcad::server::State::new(d.path().into()).unwrap();
    let mut body = vec![5, 0, 0, 0];
    body.extend_from_slice(b"alice");
    body.extend_from_slice(&[1; 32]);
    assert_eq!(s.handle(&arcad::label::Label::Vestibulum, arcad::proto::op::V_CREATE, &body).status, 0);
    Arca(InProc(Arc::new(Mutex::new(s)), d))
}

impl Rig {
    fn new(soft: SoftKey) -> Rig {
        let key = Rc::new(RefCell::new(soft));
        let (mut driver, hello) = Driver::open(Key_(key.clone()), 0x1050, 0x0407, "Soft Key").unwrap();
        driver.u2f_poll = std::time::Duration::ZERO;
        let mut broker = Broker::new(Config { web_broker: Some(broker_app()), ceremony_ms: 120_000 });
        let a = broker.driver(DEV, hello);
        assert!(matches!(a[0], Action::Audit(_)));
        Rig { broker, driver, key, pin: Some(PIN), pin_asks: 0, replies: vec![], sheets: vec![], audit: vec![], tamper: None, arca: None, passcode_ok: true, passcode_asks: 0, passcode_answers: Default::default(), arca_tamper: None }
    }

    fn run(&mut self, actions: Vec<Action>) {
        let mut queue: std::collections::VecDeque<Action> = actions.into();
        while let Some(a) = queue.pop_front() {
            match a {
                Action::Reply { reply, .. } => self.replies.push(reply),
                Action::Sheet { key, sheet } => self.sheets.push((key, sheet)),
                Action::ToDriver { device, msg } => {
                    assert_eq!(device, DEV);
                    if let Some(mut rec) = self.driver.handle(msg) {
                        if let Some(t) = self.tamper.as_mut() {
                            rec = t(rec);
                        }
                        queue.extend(self.broker.driver(DEV, rec));
                    }
                }
                Action::AskPin { key, .. } => {
                    self.pin_asks += 1;
                    assert!(self.pin_asks < 10, "PIN loop");
                    if let Some(p) = self.pin {
                        queue.extend(self.broker.pin(key, Pin(Zeroizing::new(p.to_vec()))));
                    }
                }
                Action::Audit(s) => self.audit.push(s),
                Action::CheckPasscode { key, .. } => {
                    self.passcode_asks += 1;
                    assert!(self.passcode_asks < 10, "passcode loop");
                    let ok = self.passcode_answers.pop_front().unwrap_or(self.passcode_ok);
                    queue.extend(self.broker.passcode_result(key, ok));
                }
                Action::ListPasskeys { key, user, rp_id } => {
                    let l = self.arca.as_mut().and_then(|a| a.list(&user, &rp_id));
                    queue.extend(self.broker.passkeys(key, l));
                }
                Action::PlatformCreate { key, user, rp_id, user_handle, name, display_name, uv } => {
                    let mut res = self.arca.as_mut().expect("Arca offered").create(&user, &rp_id, &user_handle, &name, &display_name, uv);
                    if let Some(t) = self.arca_tamper.as_mut() {
                        res = t(res);
                    }
                    queue.extend(self.broker.platform_result(key, res));
                }
                Action::PlatformAssert { key, user, rp_id, id, uv, client_data_hash } => {
                    let mut res = self.arca.as_mut().expect("Arca offered").assert(&user, &rp_id, &id, uv, &client_data_hash);
                    if let Some(t) = self.arca_tamper.as_mut() {
                        res = t(res);
                    }
                    queue.extend(self.broker.platform_result(key, res));
                }
                Action::CloseSheet { .. } | Action::Touch { .. } => {}
            }
        }
    }

    fn request(&mut self, label: &Label, req: Request) {
        let a = self.broker.request(7, label, req, 0);
        self.run(a);
    }

    fn press_last(&mut self, choice: Choice) {
        let key = self.sheets.last().expect("a sheet").0;
        let a = self.broker.press(key, choice, true);
        self.run(a);
    }

    fn reply(&mut self) -> Reply {
        assert_eq!(self.replies.len(), 1, "{:?}", self.replies);
        self.replies.pop().unwrap()
    }
}

fn cdh(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn register(rp: &str, uv: Uv) -> Request {
    Request::Register(Register {
        ceremony: 1,
        timeout_ms: 0,
        rp_id: rp.into(),
        rp_name: "Example".into(),
        client_data_hash: cdh("create"),
        user_id: vec![1, 2, 3, 4],
        user_name: "shankar@example.com".into(),
        user_display_name: "Shankar".into(),
        exclude: vec![],
        resident_key: false,
        uv,
        attestation: Attestation::None,
    })
}

fn assert_req(rp: &str, allow: Vec<Vec<u8>>, uv: Uv) -> Request {
    Request::Assert(Assert { ceremony: 2, timeout_ms: 0, rp_id: rp.into(), client_data_hash: cdh("get"), allow, uv })
}

/// The relying party's side: what a server does with the reply.
fn rp_check_registration(reply: &Reply, rp: &str) -> (Vec<u8>, VerifyingKey) {
    let Reply::Registered { cred_id, auth_data, att_obj, .. } = reply else { panic!("{reply:?}") };
    let obj = sigillum_cbor::decode(att_obj).unwrap();
    assert_eq!(obj.get_text("fmt"), Some(&Value::text("none")));
    assert_eq!(obj.get_text("attStmt"), Some(&Value::Map(vec![])));
    assert_eq!(obj.get_text("authData").and_then(Value::as_bytes), Some(&auth_data[..]));
    let ad = AuthData::parse(auth_data).unwrap();
    assert_eq!(ad.rp_id_hash, cdh(rp));
    assert!(ad.flags & flags::UP != 0);
    let att = ad.attested.unwrap();
    assert_eq!(att.aaguid, [0; 16], "AAGUID zeroed under attestation none");
    assert_eq!(&att.cred_id, cred_id);
    let pk = att.public_key.p256().unwrap();
    (cred_id.clone(), VerifyingKey::from(&pk))
}

fn rp_check_assertion(reply: &Reply, rp: &str, key: &VerifyingKey) -> u8 {
    let Reply::Asserted { auth_data, signature, .. } = reply else { panic!("{reply:?}") };
    let ad = AuthData::parse(auth_data).unwrap();
    assert_eq!(ad.rp_id_hash, cdh(rp));
    let mut signed = auth_data.clone();
    signed.extend_from_slice(&cdh("get"));
    let sig = DerSignature::try_from(&signature[..]).unwrap();
    key.verify(&signed, &sig).expect("signature verifies");
    ad.flags
}

#[test]
fn navigator_registers_and_signs_in_with_a_pin_protected_key() {
    let mut r = Rig::new(SoftKey::new(Some(PIN)));
    r.request(&web_broker(), register("example.com", Uv::Preferred));
    assert!(matches!(&r.sheets[0].1, Sheet::Register { requester, rp_id, .. } if requester == "Navigator" && rp_id == "example.com"));
    assert!(r.key.borrow().commands.iter().all(|c| *c == sigillum_ctap::cmd::GET_INFO), "nothing but GetInfo before the press");
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.pin_asks, 1);
    let reply = r.reply();
    let (cred, vk) = rp_check_registration(&reply, "example.com");

    r.request(&web_broker(), assert_req("example.com", vec![cred], Uv::Required));
    r.press_last(Choice::Device(DEV));
    let reply = r.reply();
    let f = rp_check_assertion(&reply, "example.com", &vk);
    assert_eq!(f & (flags::UP | flags::UV), flags::UP | flags::UV);
}

/// A CTAP 2.0 key (PIN/UV protocol 1 only, `getPinToken`, as a YubiKey 5
/// on firmware 5.4.3): the S2 gate's real key answered protocol 2's token
/// subcommand with INVALID_COMMAND.
#[test]
fn pin_protocol_one_key_registers_and_signs_in() {
    let mut soft = SoftKey::new(Some(PIN));
    soft.pin_protocols = vec![sigillum_ctap::pin::PROTOCOL_ONE];
    let mut r = Rig::new(soft);
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.pin_asks, 1);
    let (cred, vk) = rp_check_registration(&r.reply(), "example.com");
    assert!(r.key.borrow().commands.iter().all(|c| *c != sigillum_ctap::cmd::RESET));

    r.request(&web_broker(), assert_req("example.com", vec![cred], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let f = rp_check_assertion(&r.reply(), "example.com", &vk);
    assert_eq!(f & (flags::UP | flags::UV), flags::UP | flags::UV);
    assert_eq!(r.key.borrow().pin_retries, 8);
}

#[test]
fn pin_protocol_one_wrong_pin_costs_one_try() {
    let mut soft = SoftKey::new(Some(PIN));
    soft.pin_protocols = vec![sigillum_ctap::pin::PROTOCOL_ONE];
    let mut r = Rig::new(soft);
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let key = r.sheets[0].0;
    let a = r.broker.press(key, Choice::Device(DEV), true);
    r.pin = None;
    r.run(a);
    let a = r.broker.pin(key, Pin(Zeroizing::new(b"0000".to_vec())));
    r.run(a);
    assert_eq!(r.pin_asks, 2, "PinInvalid asks again");
    assert_eq!(r.key.borrow().pin_retries, 7);
    let a = r.broker.pin(key, Pin(Zeroizing::new(PIN.to_vec())));
    r.run(a);
    rp_check_registration(&r.reply(), "example.com");
}

#[test]
fn key_without_pin_works_without_uv_and_refuses_required_uv() {
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let reply = r.reply();
    let (cred, vk) = rp_check_registration(&reply, "example.com");
    assert_eq!(r.pin_asks, 0);

    r.request(&web_broker(), assert_req("example.com", vec![cred.clone()], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let reply = r.reply();
    assert_eq!(rp_check_assertion(&reply, "example.com", &vk) & flags::UV, 0);

    r.request(&web_broker(), assert_req("example.com", vec![cred], Uv::Required));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 2, reason: Refusal::NotAllowed });
}

#[test]
fn wrong_pin_asks_again_then_succeeds() {
    let mut r = Rig::new(SoftKey::new(Some(PIN)));
    r.pin = Some(b"0000");
    r.request(&web_broker(), register("example.com", Uv::Preferred));
    let key = r.sheets[0].0;
    let a = r.broker.press(key, Choice::Device(DEV), true);
    // First the driver asks for a PIN, the user types a wrong one, the
    // driver reports PinInvalid and the broker asks again; stop there.
    r.pin = None;
    r.run(a);
    assert_eq!(r.pin_asks, 1);
    let a = r.broker.pin(key, Pin(Zeroizing::new(b"0000".to_vec())));
    r.run(a);
    assert_eq!(r.pin_asks, 2, "PinInvalid asks again");
    assert_eq!(r.key.borrow().pin_retries, 7);
    let a = r.broker.pin(key, Pin(Zeroizing::new(PIN.to_vec())));
    r.run(a);
    rp_check_registration(&r.reply(), "example.com");
    assert_eq!(r.key.borrow().pin_retries, 8);
}

#[test]
fn nothing_reaches_the_key_without_a_valid_press() {
    let mut r = Rig::new(SoftKey::new(None));
    let before = r.key.borrow().commands.len();
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let key = r.sheets[0].0;
    // An obscured sheet or a synthetic press: frescod refuses the token.
    let a = r.broker.press(key, Choice::Device(DEV), false);
    r.run(a);
    assert_eq!(r.key.borrow().commands.len(), before);
    assert!(r.replies.is_empty());
    // Then it times out.
    let a = r.broker.tick(120_000);
    r.run(a);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
    assert_eq!(r.key.borrow().commands.len(), before);
}

#[test]
fn a_key_whose_own_touch_wait_runs_out_is_asked_again() {
    let mut r = Rig::new(SoftKey::new(None));
    r.key.borrow_mut().slow_touch = 2;
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert!(matches!(r.reply(), Reply::Registered { .. }), "registered on the third ask");
    assert_eq!(r.key.borrow().slow_touch, 0);
}

#[test]
fn the_sites_timeout_sets_the_limit_within_bounds() {
    use sigillumd::broker::{ceremony_limit, CEREMONY_MS, MAX_CEREMONY_MS, MIN_CEREMONY_MS};
    assert_eq!(ceremony_limit(0, CEREMONY_MS), CEREMONY_MS, "none given: the default");
    assert_eq!(ceremony_limit(5_000, CEREMONY_MS), MIN_CEREMONY_MS, "too short to find a key");
    assert_eq!(ceremony_limit(90_000, CEREMONY_MS), 90_000);
    assert_eq!(ceremony_limit(u32::MAX, CEREMONY_MS), MAX_CEREMONY_MS);
    // A site that asks for 60 s gets 60 s, not the default.
    let mut r = Rig::new(SoftKey::new(None));
    let mut req = register("example.com", Uv::Discouraged);
    if let Request::Register(x) = &mut req {
        x.timeout_ms = 60_000;
    }
    r.request(&web_broker(), req);
    let a = r.broker.tick(59_999);
    r.run(a);
    assert!(r.replies.is_empty(), "still open just before");
    let a = r.broker.tick(60_000);
    r.run(a);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
}

#[test]
fn user_does_not_touch() {
    let mut r = Rig::new(SoftKey::new(None));
    r.key.borrow_mut().touch = false;
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
}

#[test]
fn apps_get_only_their_own_rp_ids() {
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&some_app(), register("bank.example", Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Denied });
    assert!(r.sheets.is_empty(), "no sheet for a refused request");

    r.request(&some_app(), register("mail.example.com", Uv::Discouraged));
    assert!(matches!(&r.sheets[0].1, Sheet::Register { requester, .. } if requester == "com.example.mail"));
    r.press_last(Choice::Device(DEV));
    rp_check_registration(&r.reply(), "mail.example.com");

    // Its private RP works; another app's private RP doesn't.
    let Label::App { app, .. } = some_app() else { unreachable!() };
    let own = app.private_rp();
    r.request(&some_app(), register(&own, Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert!(matches!(r.reply(), Reply::Registered { .. }));
    let other = AppId { publisher: [0x22; 32], app_id: "com.example.mail".into() }.private_rp();
    r.request(&some_app(), register(&other, Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Denied });

    // The web broker may not use app RPs or malformed ones.
    r.request(&web_broker(), register(&own, Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Malformed });
    r.request(&Label::Unknown, register("example.com", Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::UnknownPeer });
    r.request(&Label::Arca, register("example.com", Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Denied });
}

#[test]
fn a_lying_key_is_caught_by_the_driver() {
    for lie in [Lie::WrongRpHash, Lie::NoUp] {
        let mut r = Rig::new(SoftKey::new(None));
        r.key.borrow_mut().lie = Some(lie);
        r.request(&web_broker(), register("example.com", Uv::Discouraged));
        r.press_last(Choice::Device(DEV));
        assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed }, "{lie:?}");
    }
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let (cred, _) = rp_check_registration(&r.reply(), "example.com");
    r.key.borrow_mut().lie = Some(Lie::OtherCredential);
    r.request(&web_broker(), assert_req("example.com", vec![cred], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 2, reason: Refusal::NotAllowed });
}

#[test]
fn a_lying_driver_is_caught_by_the_broker() {
    type Tamper = fn(FromDriver) -> FromDriver;
    let tampers: [(&str, Tamper); 5] = [
        ("other RP hash in the record and the data", |rec| match rec {
            FromDriver::Registered { ceremony, cred_id, flags, sign_count, cose_alg, public_key, mut auth_data, att_obj, .. } => {
                let h: [u8; 32] = Sha256::digest(b"evil.example").into();
                auth_data[..32].copy_from_slice(&h);
                FromDriver::Registered { ceremony, cred_id, rp_id_hash: h, flags, sign_count, cose_alg, public_key, auth_data, att_obj }
            }
            r => r,
        }),
        ("record's hash right, data's wrong", |rec| match rec {
            FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, mut auth_data, att_obj } => {
                auth_data[0] ^= 1;
                FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, att_obj }
            }
            r => r,
        }),
        ("UP claimed in the record only", |rec| match rec {
            FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, mut auth_data, att_obj } => {
                auth_data[32] &= !flags::UP;
                FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, att_obj }
            }
            r => r,
        }),
        ("credential id not the one in the data", |rec| match rec {
            FromDriver::Registered { ceremony, mut cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, att_obj } => {
                cred_id[0] ^= 1;
                FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, att_obj }
            }
            r => r,
        }),
        ("an oversized blob", |rec| match rec {
            FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, .. } => {
                FromDriver::Registered { ceremony, cred_id, rp_id_hash, flags, sign_count, cose_alg, public_key, auth_data, att_obj: vec![0; 5000] }
            }
            r => r,
        }),
    ];
    for (what, t) in tampers {
        let mut r = Rig::new(SoftKey::new(None));
        r.tamper = Some(Box::new(t));
        r.request(&web_broker(), register("example.com", Uv::Discouraged));
        r.press_last(Choice::Device(DEV));
        assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed }, "{what}");
        assert!(r.audit.iter().any(|l| l.contains("fault") || l.contains("failed checks")), "{what}: {:?}", r.audit);
    }
}

#[test]
fn records_for_other_ceremonies_are_ignored() {
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let key = r.sheets[0].0;
    // Before the press, a driver volunteering a result is stray.
    let a = r.broker.driver(DEV, FromDriver::NeedPin { ceremony: key, retries: 8 });
    assert!(matches!(&a[..], [Action::Audit(s)] if s.contains("stray")));
    let a = r.broker.driver(2, FromDriver::NeedPin { ceremony: key, retries: 8 });
    assert!(matches!(&a[..], [Action::Audit(s)] if s.contains("stray")));
}

#[test]
fn one_ceremony_at_a_time_and_cancel() {
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.request(&some_app(), register("mail.example.com", Uv::Discouraged));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Busy });
    // The first client cancels its own ceremony.
    let a = r.broker.request(7, &web_broker(), Request::Cancel { ceremony: 1 }, 0);
    r.run(a);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
    // Another connection can't cancel someone else's.
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let a = r.broker.request(8, &some_app(), Request::Cancel { ceremony: 1 }, 0);
    assert!(a.is_empty());
    r.press_last(Choice::Cancel);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
}

#[test]
fn cancel_on_the_sheet_needs_no_token() {
    let mut r = Rig::new(SoftKey::new(None));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let key = r.sheets.last().unwrap().0;
    // A device choice without a valid press does nothing...
    assert!(matches!(&r.broker.press(key, Choice::Device(DEV), false)[..], [Action::Audit(_)]));
    assert!(r.replies.is_empty());
    // ...but Escape (no token) still cancels.
    let a = r.broker.press(key, Choice::Cancel, false);
    r.run(a);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
}

#[test]
fn verify_user_is_for_arca_and_ostiarius_only() {
    let mut r = Rig::new(SoftKey::new(None));
    let v = |purpose| Request::VerifyUser(VerifyUser { ceremony: 3, user: "shankar".into(), purpose, min_strength: Strength::BiometricSecure });
    r.request(&web_broker(), v(Purpose::Unlock));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 3, reason: Refusal::Denied });
    r.request(&Label::Ostiarius, v(Purpose::ArcaItem { description: "x".into() }));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 3, reason: Refusal::Denied });

    r.request(&Label::Arca, v(Purpose::ArcaItem { description: "Bank password".into() }));
    let key = r.sheets.last().unwrap().0;
    assert!(matches!(&r.sheets.last().unwrap().1, Sheet::Verify { requester, .. } if requester == "Arca"));
    let a = r.broker.press(key, Choice::Passcode, true);
    assert_eq!(a, vec![Action::CheckPasscode { key, user: "shankar".into(), retry: false }]);
    let a = r.broker.passcode_result(key, false);
    assert!(a.contains(&Action::CheckPasscode { key, user: "shankar".into(), retry: true }), "a wrong passcode is asked again");
    assert!(!a.iter().any(|x| matches!(x, Action::Reply { .. })));
    let a = r.broker.passcode_result(key, true);
    r.run(a);
    assert!(matches!(r.reply(), Reply::Verified { ceremony: 3, strength: Strength::Passcode, .. }));
}

#[test]
fn unplugging_ends_the_ceremony() {
    let mut r = Rig::new(SoftKey::new(Some(PIN)));
    r.pin = None;
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.pin_asks, 1);
    let a = r.broker.device_gone(DEV);
    assert!(!a.iter().any(|a| matches!(a, Action::ToDriver { .. })));
    r.run(a);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
    assert_eq!(r.broker.devices().count(), 0);
}

fn u2f_key() -> SoftKey {
    let mut k = SoftKey::new(None);
    k.u2f_only = true;
    k
}

#[test]
fn u2f_only_key_registers_and_signs_in() {
    let mut r = Rig::new(u2f_key());
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let (cred, vk) = rp_check_registration(&r.reply(), "example.com");

    r.request(&web_broker(), assert_req("example.com", vec![vec![7; 32], cred.clone()], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    let reply = r.reply();
    assert!(matches!(&reply, Reply::Asserted { cred_id, .. } if *cred_id == cred));
    assert_eq!(rp_check_assertion(&reply, "example.com", &vk) & flags::UP, flags::UP);

    // Another RP's handle is not usable here.
    r.request(&web_broker(), assert_req("other.example", vec![cred.clone()], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 2, reason: Refusal::NotAllowed });

    // Excluded, after a touch.
    let Request::Register(mut reg) = register("example.com", Uv::Discouraged) else { unreachable!() };
    reg.exclude = vec![cred];
    r.request(&web_broker(), Request::Register(reg));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Excluded });
}

#[test]
fn u2f_only_key_refusals() {
    let mut r = Rig::new(u2f_key());
    r.request(&web_broker(), register("example.com", Uv::Required));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });

    r.key.borrow_mut().touch = false;
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });

    // U2F has no discoverable credentials.
    r.key.borrow_mut().touch = true;
    r.request(&web_broker(), assert_req("example.com", vec![], Uv::Discouraged));
    r.press_last(Choice::Device(DEV));
    assert_eq!(r.reply(), Reply::Refused { ceremony: 2, reason: Refusal::NotAllowed });
}

// ── Arca as the platform authenticator (§9.2) ───────────────────────────

fn with_arca() -> Rig {
    let mut r = Rig::new(SoftKey::new(Some(PIN)));
    r.arca = Some(arca());
    r
}

#[test]
fn arca_passkey_registers_and_signs_in_verified_by_the_rp() {
    let mut r = with_arca();
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    assert!(matches!(&r.sheets[0].1, Sheet::Register { platform: true, .. }));
    r.press_last(Choice::Platform);
    let reply = r.reply();
    assert!(matches!(reply, Reply::Registered { authenticator: sigillum_proto::client::AuthenticatorClass::Platform, .. }));
    let (cred, vk) = rp_check_registration(&reply, "example.com");
    assert_eq!(cred.len(), 16);
    assert_eq!(r.passcode_asks, 0, "no UV when discouraged");
    assert!(r.key.borrow().commands.iter().all(|c| *c == sigillum_ctap::cmd::GET_INFO), "the security key was never asked");

    // Discoverable sign-in: the sheet lists the passkey first.
    r.request(&web_broker(), assert_req("example.com", vec![], Uv::Required));
    let Sheet::Assert { passkeys, .. } = &r.sheets.last().unwrap().1 else { panic!() };
    assert_eq!(passkeys.len(), 1);
    assert_eq!((passkeys[0].id.to_vec(), passkeys[0].name.as_str()), (cred.clone(), "shankar@example.com"));
    r.press_last(Choice::Passkey(cred[..].try_into().unwrap()));
    assert_eq!(r.passcode_asks, 1, "UV required: the passcode first");
    let reply = r.reply();
    let f = rp_check_assertion(&reply, "example.com", &vk);
    assert_eq!(f & (flags::UP | flags::UV), flags::UP | flags::UV);
    let Reply::Asserted { cred_id, user_handle, .. } = reply else { panic!() };
    assert_eq!((cred_id, user_handle), (cred, Some(vec![1, 2, 3, 4])));
}

#[test]
fn arca_honours_exclude_and_allow_lists() {
    let mut r = with_arca();
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Platform);
    let Reply::Registered { cred_id, .. } = r.reply() else { panic!() };

    // excludeCredentials naming it: InvalidStateError after the press.
    let Request::Register(mut reg) = register("example.com", Uv::Discouraged) else { unreachable!() };
    reg.exclude = vec![cred_id.clone()];
    r.request(&web_broker(), Request::Register(reg));
    r.press_last(Choice::Platform);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::Excluded });

    // An allow list without it: the sheet doesn't offer it, and a press
    // naming it anyway does nothing.
    r.request(&web_broker(), assert_req("example.com", vec![vec![9; 16]], Uv::Discouraged));
    let (key, Sheet::Assert { passkeys, .. }) = r.sheets.last().unwrap().clone() else { panic!() };
    assert!(passkeys.is_empty());
    let a = r.broker.press(key, Choice::Passkey(cred_id[..].try_into().unwrap()), true);
    assert!(a.is_empty());
    let a = r.broker.press(key, Choice::Cancel, true);
    r.run(a);
    r.reply();

    // Another RP's sheet doesn't list it.
    r.request(&web_broker(), assert_req("other.example", vec![], Uv::Discouraged));
    assert!(matches!(&r.sheets.last().unwrap().1, Sheet::Assert { passkeys, .. } if passkeys.is_empty()));
}

#[test]
fn a_wrong_passcode_is_asked_again_three_times_at_most() {
    // Wrong, then right: the UV flag is set.
    let mut r = with_arca();
    r.passcode_answers = [false].into();
    r.request(&web_broker(), register("example.com", Uv::Required));
    r.press_last(Choice::Platform);
    assert_eq!(r.passcode_asks, 2);
    let reply = r.reply();
    let Reply::Registered { auth_data, .. } = &reply else { panic!("{reply:?}") };
    assert!(auth_data[32] & flags::UV != 0);

    // Three wrong: the ceremony ends, nothing is made.
    let mut r = with_arca();
    r.passcode_ok = false;
    r.request(&web_broker(), register("example.com", Uv::Preferred));
    r.press_last(Choice::Platform);
    assert_eq!(r.passcode_asks, 3);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
    r.request(&web_broker(), assert_req("example.com", vec![], Uv::Discouraged));
    assert!(matches!(&r.sheets.last().unwrap().1, Sheet::Assert { passkeys, .. } if passkeys.is_empty()), "no passkey was made");
}

#[test]
fn arcas_answer_is_checked_like_a_drivers() {
    let mut r = with_arca();
    r.arca_tamper = Some(Box::new(|res| match res {
        PlatformResult::Created { id, mut auth_data } => {
            auth_data[..32].copy_from_slice(&cdh("evil.example"));
            PlatformResult::Created { id, auth_data }
        }
        other => other,
    }));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    r.press_last(Choice::Platform);
    assert_eq!(r.reply(), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
    assert!(r.audit.iter().any(|l| l.contains("arca: RP ID hash")));
}

#[test]
fn no_arca_for_apps_private_rp_ids_or_without_arca() {
    // An app's private RP ID is no Arca recipient: no ListPasskeys.
    let mut r = with_arca();
    let app = some_app();
    let Label::App { app: id, .. } = &app else { unreachable!() };
    let a = r.broker.request(7, &app, register(&id.private_rp(), Uv::Discouraged), 0);
    assert!(a.iter().any(|x| matches!(x, Action::Sheet { sheet: Sheet::Register { platform: false, .. }, .. })));
    assert!(!a.iter().any(|x| matches!(x, Action::ListPasskeys { .. })));

    // Arca unreachable or locked: security keys only, and Platform does nothing.
    let mut r = Rig::new(SoftKey::new(Some(PIN)));
    r.request(&web_broker(), register("example.com", Uv::Discouraged));
    let (key, sheet) = r.sheets.last().unwrap().clone();
    assert!(matches!(sheet, Sheet::Register { platform: false, .. }));
    assert!(r.broker.press(key, Choice::Platform, true).is_empty());
}

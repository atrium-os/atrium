//! Ceremonies (sigillum.md §7.2): a request becomes a sheet, a press on the
//! sheet becomes driver work, a checked driver record becomes the reply.
//!
//! Rules enforced here:
//!
//! - **Labels.** The web broker may ask for any well-formed RP ID; an app
//!   only for its own (manifest-declared, verified) RP IDs or its private
//!   one; arcad and ostiarius only for `VerifyUser`; anyone else nothing.
//! - **Nothing touches a device before a press.** A ceremony starts as a
//!   sheet; only [`Broker::press`] with a valid activation sends driver work.
//! - **Driver records are checked by byte comparison.** The RP ID hash, the
//!   flags and the sign count in a record must equal the first 37 bytes of
//!   the authenticator data it forwards, the hash must be SHA-256 of the RP
//!   ID this ceremony asked for, UP must be set and UV when it was required.
//!   A registration's credential id must be the one inside its authenticator
//!   data; an assertion's must be in the allow list.
//! - **One ceremony per seat**, 120 s each; ending one cancels its driver
//!   work and closes its sheet.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use sigillum_proto::client::{
    AuthenticatorClass, CeremonyId, Method, Purpose, Refusal, Reply, Request, Strength, Uv, VerifyUser,
};
use sigillum_proto::client::{Assert, Register};
use sigillum_proto::driver::{cap, DeviceClass, DriverError, FidoCaps, FromDriver, Pin, ToDriver};

use crate::label::Label;
use crate::rp::valid_domain;

pub type ConnId = u64;
pub type DeviceId = u64;
/// sigillumd's own name for a ceremony. Drivers see this, never the
/// client's id, so two clients' ids can't collide at a driver.
pub type Key = u64;

pub const CEREMONY_MS: u64 = 120_000;

const UP: u8 = 0x01;
const UV: u8 = 0x04;
const AT: u8 = 0x40;

#[derive(Debug, Clone)]
pub struct Config {
    /// `SIGILLUM_WEB_BROKER`: the one app that may name any RP ID; `None`,
    /// no app may.
    pub web_broker: Option<crate::label::AppId>,
    pub ceremony_ms: u64,
}

pub use sigillum_proto::ui::{Choice, Sheet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Reply { conn: ConnId, reply: Reply },
    Sheet { key: Key, sheet: Sheet },
    /// Ask the user for the PIN of this device.
    AskPin { key: Key, device: DeviceId, retries: u8 },
    /// Tell the user to touch the key.
    Touch { key: Key, device: DeviceId },
    CloseSheet { key: Key },
    ToDriver { device: DeviceId, msg: ToDriver },
    /// Ask ostiarius to check the passcode the user typed on the sheet.
    CheckPasscode { key: Key, user: String },
    /// One audit line (arca.md §10 style): no values, ever.
    Audit(String),
}

#[derive(Debug)]
enum Kind {
    Register(Register),
    Assert(Assert),
    Verify(VerifyUser),
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    AwaitingPress,
    Driving,
    AwaitingPin,
    AwaitingPasscode,
}

#[derive(Debug)]
struct Ceremony {
    conn: ConnId,
    id: CeremonyId,
    kind: Kind,
    rp_hash: [u8; 32],
    uv_required: bool,
    state: State,
    started: u64,
    device: Option<DeviceId>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub class: DeviceClass,
    pub name: String,
    pub fido: Option<FidoCaps>,
}

pub struct Broker {
    cfg: Config,
    ceremonies: BTreeMap<Key, Ceremony>,
    next_key: Key,
    devices: BTreeMap<DeviceId, Device>,
}

fn refused(conn: ConnId, ceremony: CeremonyId, reason: Refusal) -> Action {
    Action::Reply { conn, reply: Reply::Refused { ceremony, reason } }
}

fn requester(label: &Label, cfg: &Config) -> String {
    match label {
        Label::App { app, .. } if Some(app) == cfg.web_broker.as_ref() => "Navigator".into(),
        Label::App { app, .. } => app.app_id.clone(),
        Label::Arca => "Arca".into(),
        Label::Ostiarius => "Lock screen".into(),
        Label::Unknown => "?".into(),
    }
}

impl Broker {
    pub fn new(cfg: Config) -> Broker {
        Broker { cfg, ceremonies: BTreeMap::new(), next_key: 1, devices: BTreeMap::new() }
    }

    pub fn devices(&self) -> impl Iterator<Item = (&DeviceId, &Device)> {
        self.devices.iter()
    }

    /// May this label run a WebAuthn ceremony for this RP ID?
    fn may_use_rp(&self, label: &Label, rp_id: &str) -> Result<(), Refusal> {
        match label {
            Label::App { app, .. } if Some(app) == self.cfg.web_broker.as_ref() => {
                if valid_domain(rp_id) {
                    Ok(())
                } else {
                    Err(Refusal::Malformed)
                }
            }
            Label::App { app, rp_ids, .. } => {
                if rp_id == app.private_rp() || (valid_domain(rp_id) && rp_ids.iter().any(|r| r == rp_id)) {
                    Ok(())
                } else {
                    Err(Refusal::Denied)
                }
            }
            Label::Unknown => Err(Refusal::UnknownPeer),
            Label::Arca | Label::Ostiarius => Err(Refusal::Denied),
        }
    }

    fn start(&mut self, conn: ConnId, id: CeremonyId, kind: Kind, rp_id: &str, uv_required: bool, now: u64, sheet: Sheet) -> Vec<Action> {
        let key = self.next_key;
        self.next_key += 1;
        let audit = Action::Audit(format!("start key={key} conn={conn} {:?}", sheet));
        self.ceremonies.insert(
            key,
            Ceremony {
                conn,
                id,
                kind,
                rp_hash: Sha256::digest(rp_id.as_bytes()).into(),
                uv_required,
                state: State::AwaitingPress,
                started: now,
                device: None,
            },
        );
        vec![audit, Action::Sheet { key, sheet }]
    }

    /// A client's request, with the label its peer uid resolved to.
    pub fn request(&mut self, conn: ConnId, label: &Label, req: Request, now: u64) -> Vec<Action> {
        let id = req.ceremony();
        if let Request::Cancel { ceremony } = req {
            let key = self.ceremonies.iter().find(|(_, c)| c.conn == conn && c.id == ceremony).map(|(k, _)| *k);
            return match key {
                Some(k) => self.end(k, Reply::Refused { ceremony, reason: Refusal::NotAllowed }, "cancelled by client"),
                None => Vec::new(),
            };
        }
        if *label == Label::Unknown {
            return vec![refused(conn, id, Refusal::UnknownPeer)];
        }
        if !self.ceremonies.is_empty() {
            return vec![refused(conn, id, Refusal::Busy)];
        }
        let who = requester(label, &self.cfg);
        match req {
            Request::Register(r) => {
                if let Err(e) = self.may_use_rp(label, &r.rp_id) {
                    return vec![refused(conn, id, e)];
                }
                if r.user_id.is_empty()
                    || r.user_id.len() > cap::USER_HANDLE
                    || r.exclude.len() > cap::LIST
                    || r.exclude.iter().any(|c| c.is_empty() || c.len() > cap::CRED_ID)
                    || r.user_name.len() > 64
                    || r.user_display_name.len() > 64
                    || r.rp_name.len() > 64
                {
                    return vec![refused(conn, id, Refusal::Malformed)];
                }
                let sheet = Sheet::Register { requester: who, rp_id: r.rp_id.clone(), user_name: r.user_name.clone() };
                let rp = r.rp_id.clone();
                let uv = r.uv == Uv::Required;
                self.start(conn, id, Kind::Register(r), &rp, uv, now, sheet)
            }
            Request::Assert(a) => {
                if let Err(e) = self.may_use_rp(label, &a.rp_id) {
                    return vec![refused(conn, id, e)];
                }
                if a.allow.len() > cap::LIST || a.allow.iter().any(|c| c.is_empty() || c.len() > cap::CRED_ID) {
                    return vec![refused(conn, id, Refusal::Malformed)];
                }
                let sheet = Sheet::Assert { requester: who, rp_id: a.rp_id.clone() };
                let rp = a.rp_id.clone();
                let uv = a.uv == Uv::Required;
                self.start(conn, id, Kind::Assert(a), &rp, uv, now, sheet)
            }
            Request::VerifyUser(v) => {
                if !matches!(label, Label::Arca | Label::Ostiarius) {
                    return vec![refused(conn, id, Refusal::Denied)];
                }
                if matches!(v.purpose, Purpose::ArcaItem { .. }) && *label != Label::Arca {
                    return vec![refused(conn, id, Refusal::Denied)];
                }
                if v.user.is_empty() || v.user.len() > 64 {
                    return vec![refused(conn, id, Refusal::Malformed)];
                }
                let sheet = Sheet::Verify { requester: who, user: v.user.clone(), purpose: v.purpose.clone() };
                self.start(conn, id, Kind::Verify(v), "", false, now, sheet)
            }
            Request::Cancel { .. } => unreachable!(),
        }
    }

    fn driver_msg(&self, key: Key, c: &Ceremony, pin: Option<Pin>) -> Option<ToDriver> {
        Some(match &c.kind {
            Kind::Register(r) => ToDriver::Register {
                ceremony: key,
                rp_id: r.rp_id.clone(),
                rp_name: r.rp_name.clone(),
                client_data_hash: r.client_data_hash,
                user_id: r.user_id.clone(),
                user_name: r.user_name.clone(),
                user_display_name: r.user_display_name.clone(),
                exclude: r.exclude.clone(),
                resident_key: r.resident_key,
                uv_required: c.uv_required,
                pin,
            },
            Kind::Assert(a) => ToDriver::Assert {
                ceremony: key,
                rp_id: a.rp_id.clone(),
                client_data_hash: a.client_data_hash,
                allow: a.allow.clone(),
                uv_required: c.uv_required,
                pin,
            },
            Kind::Verify(_) => return None,
        })
    }

    /// The user pressed on the sheet. `activation_ok` is frescod's answer
    /// for the press token (purpose `IDENTITY`): false if the sheet was
    /// covered, moved or faded, or the press was synthetic.
    pub fn press(&mut self, key: Key, choice: Choice, activation_ok: bool) -> Vec<Action> {
        let Some(c) = self.ceremonies.get(&key) else { return Vec::new() };
        // Cancelling needs no proof of a press: it only ever ends a
        // ceremony, and Escape carries no token.
        if !activation_ok && choice != Choice::Cancel {
            return vec![Action::Audit(format!("press refused key={key}: no valid activation"))];
        }
        if c.state != State::AwaitingPress && choice != Choice::Cancel {
            return Vec::new();
        }
        match (choice, &c.kind) {
            (Choice::Cancel, _) => {
                let id = c.id;
                self.end(key, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, "cancelled on the sheet")
            }
            (Choice::Device(d), Kind::Register(_) | Kind::Assert(_)) => {
                if !self.devices.get(&d).is_some_and(|dev| dev.class == DeviceClass::Fido) {
                    return Vec::new();
                }
                let msg = self.driver_msg(key, c, None).expect("register or assert");
                let c = self.ceremonies.get_mut(&key).unwrap();
                c.state = State::Driving;
                c.device = Some(d);
                vec![Action::ToDriver { device: d, msg }]
            }
            (Choice::Passcode, Kind::Verify(v)) => {
                let user = v.user.clone();
                self.ceremonies.get_mut(&key).unwrap().state = State::AwaitingPasscode;
                vec![Action::CheckPasscode { key, user }]
            }
            _ => Vec::new(),
        }
    }

    /// The PIN the user typed into `sigillum-ui` for this ceremony's device.
    pub fn pin(&mut self, key: Key, pin: Pin) -> Vec<Action> {
        let Some(c) = self.ceremonies.get(&key) else { return Vec::new() };
        if c.state != State::AwaitingPin {
            return Vec::new();
        }
        let device = c.device.expect("a ceremony waiting for a PIN has a device");
        let msg = self.driver_msg(key, c, Some(pin)).expect("register or assert");
        self.ceremonies.get_mut(&key).unwrap().state = State::Driving;
        vec![Action::ToDriver { device, msg }]
    }

    /// ostiarius's answer for a `CheckPasscode`.
    pub fn passcode_result(&mut self, key: Key, ok: bool) -> Vec<Action> {
        let Some(c) = self.ceremonies.get_mut(&key) else { return Vec::new() };
        if c.state != State::AwaitingPasscode {
            return Vec::new();
        }
        if !ok {
            // The sheet stays; portcullisd delays and rate-limits failures.
            c.state = State::AwaitingPress;
            return vec![Action::Audit(format!("passcode wrong key={key}"))];
        }
        let id = c.id;
        let Kind::Verify(v) = &c.kind else { return Vec::new() };
        if v.min_strength > Strength::Passcode {
            return Vec::new();
        }
        self.end(key, Reply::Verified { ceremony: id, method: Method::Passcode, strength: Strength::Passcode }, "verified by passcode")
    }

    /// A driver attached (its `Hello`) or answered.
    pub fn driver(&mut self, device: DeviceId, rec: FromDriver) -> Vec<Action> {
        if let Err(why) = rec.check_caps() {
            return self.device_fault(device, why);
        }
        if let FromDriver::Hello { class, name, fido, .. } = rec {
            self.devices.insert(device, Device { class, name, fido });
            return vec![Action::Audit(format!("device {device} attached: {class:?}"))];
        }
        let key = rec.ceremony().expect("not Hello");
        let Some(c) = self.ceremonies.get(&key) else {
            return vec![Action::Audit(format!("stray record from device {device} for key={key}"))];
        };
        if c.device != Some(device) || c.state != State::Driving {
            return vec![Action::Audit(format!("stray record from device {device} for key={key}"))];
        }
        let id = c.id;
        match rec {
            FromDriver::NeedPin { retries, .. } => {
                self.ceremonies.get_mut(&key).unwrap().state = State::AwaitingPin;
                vec![Action::AskPin { key, device, retries }]
            }
            FromDriver::NeedTouch { .. } => vec![Action::Touch { key, device }],
            FromDriver::Error { error: DriverError::PinInvalid { retries }, .. } => {
                self.ceremonies.get_mut(&key).unwrap().state = State::AwaitingPin;
                vec![Action::AskPin { key, device, retries }]
            }
            FromDriver::Error { error: DriverError::Excluded, .. } => {
                self.end(key, Reply::Refused { ceremony: id, reason: Refusal::Excluded }, "excluded")
            }
            FromDriver::Error { error, .. } => {
                self.end(key, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, &format!("driver: {error:?}"))
            }
            FromDriver::Registered { .. } | FromDriver::Asserted { .. } => match check(c, &rec) {
                Ok(()) => {
                    let reply = match rec {
                        FromDriver::Registered { cred_id, auth_data, att_obj, .. } => Reply::Registered {
                            ceremony: id,
                            cred_id,
                            auth_data,
                            att_obj,
                            authenticator: AuthenticatorClass::Usb,
                        },
                        FromDriver::Asserted { cred_id, auth_data, signature, user_handle, .. } => Reply::Asserted {
                            ceremony: id,
                            cred_id,
                            auth_data,
                            signature,
                            user_handle,
                            authenticator: AuthenticatorClass::Usb,
                        },
                        _ => unreachable!(),
                    };
                    self.end(key, reply, "done")
                }
                Err(why) => {
                    let mut a = self.end(key, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, &format!("record refused: {why}"));
                    a.push(Action::Audit(format!("device {device} sent a record that failed checks: {why}")));
                    a
                }
            },
            FromDriver::Hello { .. } => unreachable!(),
        }
    }

    fn device_fault(&mut self, device: DeviceId, why: &str) -> Vec<Action> {
        let mut out = vec![Action::Audit(format!("device {device} fault: {why}"))];
        let keys: Vec<Key> = self.ceremonies.iter().filter(|(_, c)| c.device == Some(device)).map(|(k, _)| *k).collect();
        for k in keys {
            let id = self.ceremonies[&k].id;
            out.extend(self.end(k, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, "device fault"));
        }
        out
    }

    /// The driver's jail exited or the device was unplugged.
    pub fn device_gone(&mut self, device: DeviceId) -> Vec<Action> {
        self.devices.remove(&device);
        let mut out = self.device_fault(device, "gone");
        out.retain(|a| !matches!(a, Action::ToDriver { .. }));
        out
    }

    /// The client's connection closed: its ceremonies end quietly.
    pub fn disconnected(&mut self, conn: ConnId) -> Vec<Action> {
        let keys: Vec<Key> = self.ceremonies.iter().filter(|(_, c)| c.conn == conn).map(|(k, _)| *k).collect();
        let mut out = Vec::new();
        for k in keys {
            let id = self.ceremonies[&k].id;
            out.extend(self.end(k, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, "client gone"));
            out.retain(|a| !matches!(a, Action::Reply { .. }));
        }
        out
    }

    /// End one ceremony from outside: no trusted UI to show its sheet, or
    /// the UI went away.
    pub fn abort(&mut self, key: Key, why: &str) -> Vec<Action> {
        let Some(c) = self.ceremonies.get(&key) else { return Vec::new() };
        let id = c.id;
        self.end(key, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, why)
    }

    /// End every ceremony (the trusted UI disconnected).
    pub fn abort_all(&mut self, why: &str) -> Vec<Action> {
        let keys: Vec<Key> = self.ceremonies.keys().copied().collect();
        keys.into_iter().flat_map(|k| self.abort(k, why)).collect()
    }

    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        let expired: Vec<Key> =
            self.ceremonies.iter().filter(|(_, c)| now.saturating_sub(c.started) >= self.cfg.ceremony_ms).map(|(k, _)| *k).collect();
        let mut out = Vec::new();
        for k in expired {
            let id = self.ceremonies[&k].id;
            out.extend(self.end(k, Reply::Refused { ceremony: id, reason: Refusal::NotAllowed }, "timed out"));
        }
        out
    }

    fn end(&mut self, key: Key, reply: Reply, why: &str) -> Vec<Action> {
        let c = self.ceremonies.remove(&key).expect("ending a live ceremony");
        let mut out = Vec::new();
        if let (Some(device), true) = (c.device, matches!(reply, Reply::Refused { .. })) {
            if c.state == State::Driving {
                out.push(Action::ToDriver { device, msg: ToDriver::Cancel { ceremony: key } });
            }
        }
        out.push(Action::CloseSheet { key });
        out.push(Action::Audit(format!("end key={key}: {why}")));
        out.push(Action::Reply { conn: c.conn, reply });
        out
    }
}

/// §14.2's byte comparisons between a record's fields and the blobs it
/// forwards. `sigillumd` reads nothing else of device data.
fn check(c: &Ceremony, rec: &FromDriver) -> Result<(), &'static str> {
    let (rp_id_hash, flags, sign_count, auth_data) = match rec {
        FromDriver::Registered { rp_id_hash, flags, sign_count, auth_data, .. }
        | FromDriver::Asserted { rp_id_hash, flags, sign_count, auth_data, .. } => (rp_id_hash, *flags, *sign_count, auth_data),
        _ => return Err("not a result"),
    };
    if *rp_id_hash != c.rp_hash {
        return Err("RP ID hash is not this ceremony's");
    }
    if auth_data.len() < 37 || auth_data[..32] != c.rp_hash || auth_data[32] != flags || auth_data[33..37] != sign_count.to_be_bytes() {
        return Err("authenticator data disagrees with the record");
    }
    if flags & UP == 0 {
        return Err("no user presence");
    }
    if c.uv_required && flags & UV == 0 {
        return Err("no user verification");
    }
    match (rec, &c.kind) {
        (FromDriver::Registered { cred_id, cose_alg, public_key, att_obj, .. }, Kind::Register(_)) => {
            if flags & AT == 0 || auth_data.len() < 55 + cred_id.len() {
                return Err("no attested credential data");
            }
            if auth_data[53..55] != (cred_id.len() as u16).to_be_bytes() || auth_data[55..55 + cred_id.len()] != cred_id[..] {
                return Err("credential id is not the one in the authenticator data");
            }
            if *cose_alg != -7 || public_key.len() != 65 || public_key[0] != 4 {
                return Err("not an ES256 key");
            }
            if att_obj.is_empty() {
                return Err("empty attestation object");
            }
            Ok(())
        }
        (FromDriver::Asserted { cred_id, signature, .. }, Kind::Assert(a)) => {
            if flags & AT != 0 {
                return Err("assertion carries attested credential data");
            }
            if !a.allow.is_empty() && !a.allow.contains(cred_id) {
                return Err("credential not in the allow list");
            }
            if signature.is_empty() {
                return Err("empty signature");
            }
            Ok(())
        }
        _ => Err("record does not match the ceremony"),
    }
}

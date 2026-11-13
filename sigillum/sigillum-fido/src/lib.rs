//! The FIDO driver's logic (sigillum.md §6.1), over any CTAPHID device.
//!
//! It takes [`ToDriver`] requests from `sigillumd`, runs them against its
//! one authenticator, parses and checks everything the device says, and
//! answers with [`FromDriver`] records. It is the only place device bytes
//! are parsed; what it forwards is either a fixed field or an opaque blob
//! whose first 37 bytes `sigillumd` compares against those fields.
//!
//! Checks here (and again in `sigillumd`): the RP ID hash is SHA-256 of the
//! RP ID asked for; UP is set; UV is set when it was required; an
//! assertion's credential was in the allow list; the credential key is a
//! valid P-256 point. Attestation leaves as `none` with a zero AAGUID.
#![forbid(unsafe_code)]

pub mod serve;
pub use serve::serve;

use std::cell::RefCell;
use std::rc::Rc;

use sha2::{Digest, Sha256};
use sigillum_ctap::authdata::flags;
use sigillum_ctap::client::Client;
use sigillum_ctap::cmd::{GetAssertion, Info, MakeCredential, User};
use sigillum_ctap::cose::{alg, CoseKey};
use sigillum_ctap::hid::HidDevice;
use sigillum_ctap::pin::permission;
use sigillum_ctap::{status, u2f, Error};
use sigillum_proto::client::CeremonyId;
use sigillum_proto::driver::{DeviceClass, DriverError, FidoCaps, FromDriver, Pin, ToDriver};

pub struct Driver<D: HidDevice> {
    client: Client<D>,
    /// `None`: a U2F-only key.
    info: Option<Info>,
    /// How long to wait between U2F presence polls (`69 85`).
    pub u2f_poll: std::time::Duration,
    /// How many polls before giving up on a touch (the broker's ceremony
    /// limit usually ends it first, with a cancel).
    pub u2f_polls: u32,
    touch: Rc<RefCell<Touch>>,
}

/// "Touch your key" once per request: on CTAP2's `UPNEEDED` keepalive or
/// U2F's first "conditions not satisfied".
#[derive(Default)]
struct Touch {
    hook: Option<Box<dyn FnMut()>>,
    said: bool,
}

impl Touch {
    fn needed(&mut self) {
        if !self.said {
            self.said = true;
            if let Some(f) = self.hook.as_mut() {
                f();
            }
        }
    }
}

/// How long the driver keeps asking a key for a touch: past any ceremony's
/// limit (sigillumd's `MAX_CEREMONY_MS`, 10 min), which ends it first with a
/// `Cancel`. A key gives up on its own after about 30 s (CTAP2
/// `USER_ACTION_TIMEOUT`; a U2F key answers "not yet" until touched), so
/// the driver asks again while the ceremony lasts: the sheet stays up and
/// the user is not cut off by the key's clock. Time spent typing a PIN is
/// before the request reaches the key, so it never counts against the key.
const TOUCH_WAIT: std::time::Duration = std::time::Duration::from_secs(660);
const U2F_POLLS: u32 = (TOUCH_WAIT.as_millis() / 200) as u32;

/// CTAPHID keepalive status: waiting for the user's touch.
const KEEPALIVE_UPNEEDED: u8 = 2;

fn map_err(e: Error) -> DriverError {
    match e {
        Error::Status(status::OPERATION_DENIED) | Error::Status(status::USER_ACTION_TIMEOUT) | Error::Status(status::KEEPALIVE_CANCEL) => {
            DriverError::Denied
        }
        Error::Status(status::CREDENTIAL_EXCLUDED) => DriverError::Excluded,
        Error::Status(status::NO_CREDENTIALS) => DriverError::NoCredentials,
        Error::Status(status::PIN_BLOCKED) | Error::Status(status::PIN_AUTH_BLOCKED) => DriverError::PinBlocked,
        Error::Io(_) => DriverError::Gone,
        Error::Cancelled => DriverError::Denied,
        e => {
            // The broker gets only the class; the detail stays in this
            // jail's log (jaild's per-jail log under /var/log/atrium).
            eprintln!("sigillum-fido: authenticator error: {e:?}");
            DriverError::Device
        }
    }
}

impl<D: HidDevice> Driver<D> {
    /// `INIT` and `GetInfo`; the `Hello` record for `sigillumd`.
    pub fn open(dev: D, vendor: u16, product: u16, name: &str) -> Result<(Driver<D>, FromDriver), Error> {
        let mut client = Client::open(dev)?;
        let touch = Rc::new(RefCell::new(Touch::default()));
        let t = touch.clone();
        client.on_keepalive = Some(Box::new(move |s| {
            if s == KEEPALIVE_UPNEEDED {
                t.borrow_mut().needed();
            }
        }));
        if !client.ctap2() {
            let hello = FromDriver::Hello {
                class: DeviceClass::Fido,
                vendor,
                product,
                name: name.chars().take(32).collect(),
                fido: Some(FidoCaps {
                    ctap2: false,
                    resident_keys: false,
                    client_pin_set: false,
                    builtin_uv: false,
                    cred_protect: false,
                    hmac_secret: false,
                }),
            };
            return Ok((Driver { client, info: None, u2f_poll: std::time::Duration::from_millis(200), u2f_polls: U2F_POLLS, touch }, hello));
        }
        let info = client.get_info()?;
        let opt = |k| info.option(k).unwrap_or(false);
        let ext = |k: &str| info.extensions.iter().any(|e| e == k);
        let hello = FromDriver::Hello {
            class: DeviceClass::Fido,
            vendor,
            product,
            name: name.chars().take(32).collect(),
            fido: Some(FidoCaps {
                ctap2: info.versions.iter().any(|v| v.starts_with("FIDO_2")),
                resident_keys: opt("rk"),
                client_pin_set: opt("clientPin"),
                builtin_uv: opt("uv"),
                cred_protect: ext("credProtect"),
                hmac_secret: ext("hmac-secret"),
            }),
        };
        Ok((Driver { client, info: Some(info), u2f_poll: std::time::Duration::from_millis(200), u2f_polls: U2F_POLLS, touch }, hello))
    }

    /// Run one request to its record. Nothing here blocks on the user
    /// except the authenticator's own touch wait.
    /// The flag the socket reader sets when the broker cancels.
    pub fn cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.client.cancel.clone()
    }

    /// Called (at most once per request) when the authenticator waits for
    /// a touch; the daemon turns it into a `NeedTouch` record.
    pub fn on_touch(&mut self, f: impl FnMut() + 'static) {
        self.touch.borrow_mut().hook = Some(Box::new(f));
    }

    pub fn handle(&mut self, req: ToDriver) -> Option<FromDriver> {
        self.touch.borrow_mut().said = false;
        let out = self.handle_inner(req);
        // A cancel that arrived for this request is spent now.
        self.client.cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        out
    }

    fn handle_inner(&mut self, req: ToDriver) -> Option<FromDriver> {
        if self.info.is_none() {
            return self.handle_u2f(req);
        }
        match req {
            ToDriver::Register { ceremony, rp_id, rp_name, client_data_hash, user_id, user_name, user_display_name, exclude, resident_key, uv_required, pin } => {
                let user = User { id: user_id, name: user_name, display_name: user_display_name };
                Some(self.register(ceremony, rp_id, rp_name, client_data_hash, user, exclude, resident_key, uv_required, pin))
            }
            ToDriver::Assert { ceremony, rp_id, client_data_hash, allow, uv_required, pin } => {
                Some(self.assert(ceremony, rp_id, client_data_hash, allow, uv_required, pin))
            }
            ToDriver::Cancel { .. } => {
                let _ = self.client.cancel();
                None
            }
            // A token driver's requests (S5): a FIDO key has no certificates.
            ToDriver::ListCertificates { ceremony } | ToDriver::SignDigest { ceremony, .. } => {
                Some(FromDriver::Error { ceremony, error: DriverError::Unsupported })
            }
        }
    }

    /// `Ok(None)`: go ahead without a PIN. `Err(record)`: answer with it.
    fn pin_auth(
        &mut self,
        ceremony: CeremonyId,
        pin: Option<Pin>,
        uv_required: bool,
        perm: u8,
        rp_id: &str,
        cdh: &[u8; 32],
    ) -> Result<Option<sigillum_ctap::cmd::PinAuth>, FromDriver> {
        let info = self.info.as_ref().expect("CTAP2");
        let pin_set = info.option("clientPin") == Some(true);
        if !pin_set {
            if uv_required && info.option("uv") != Some(true) {
                return Err(FromDriver::Error { ceremony, error: DriverError::UvUnavailable });
            }
            // Built-in UV through the PIN token API is S6's (on-key
            // fingerprints); S1 keys either have a PIN or don't verify.
            if uv_required {
                return Err(FromDriver::Error { ceremony, error: DriverError::UvUnavailable });
            }
            return Ok(None);
        }
        let Some(pin) = pin else {
            let retries = self.client.pin_retries().map(|r| r.min(255) as u8).unwrap_or(0);
            return Err(FromDriver::NeedPin { ceremony, retries });
        };
        match self.client.pin_token(&pin.0, perm, rp_id) {
            Ok(t) => Ok(Some(t.auth(cdh))),
            Err(Error::Status(status::PIN_INVALID)) => {
                let retries = self.client.pin_retries().map(|r| r.min(255) as u8).unwrap_or(0);
                Err(FromDriver::Error { ceremony, error: DriverError::PinInvalid { retries } })
            }
            Err(e) => Err(FromDriver::Error { ceremony, error: map_err(e) }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &mut self,
        ceremony: CeremonyId,
        rp_id: String,
        rp_name: String,
        cdh: [u8; 32],
        user: User,
        exclude: Vec<Vec<u8>>,
        resident_key: bool,
        uv_required: bool,
        pin: Option<Pin>,
    ) -> FromDriver {
        let pin_auth = match self.pin_auth(ceremony, pin, uv_required, permission::MAKE_CREDENTIAL, &rp_id, &cdh) {
            Ok(p) => p,
            Err(rec) => return rec,
        };
        let uv = pin_auth.is_some();
        let req = MakeCredential {
            client_data_hash: cdh,
            rp_id: rp_id.clone(),
            rp_name,
            user,
            exclude,
            resident_key,
            cred_protect: None,
            hmac_secret: false,
            pin_auth,
        };
        let fault = |why: &str| {
            eprintln!("sigillum-fido: refusing the authenticator's credential: {why}");
            FromDriver::Error { ceremony, error: DriverError::Device }
        };
        let att = match self.until_touched(|c| c.make_credential(&req)) {
            Ok(a) => a,
            Err(e) => return FromDriver::Error { ceremony, error: map_err(e) },
        };
        let ad = &att.auth_data;
        let expected: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        if ad.rp_id_hash != expected {
            return fault("RP ID hash");
        }
        if ad.flags & flags::UP == 0 || (uv && ad.flags & flags::UV == 0) {
            return fault("UP/UV flags");
        }
        let Some(cred) = ad.attested.as_ref() else { return fault("no attested credential") };
        let CoseKey::P256 { alg: alg::ES256, .. } = cred.public_key else { return fault("not an ES256 key") };
        let Some(pk) = cred.public_key.p256() else { return fault("bad P-256 point") };
        use sigillum_ctap_sec1::ToSec1;
        let public_key = pk.to_sec1_vec();
        let (auth_data, att_obj) = att.none_object();
        FromDriver::Registered {
            ceremony,
            cred_id: cred.cred_id.clone(),
            rp_id_hash: ad.rp_id_hash,
            flags: ad.flags,
            sign_count: ad.sign_count,
            cose_alg: alg::ES256,
            public_key,
            auth_data,
            att_obj,
        }
    }

    fn assert(&mut self, ceremony: CeremonyId, rp_id: String, cdh: [u8; 32], allow: Vec<Vec<u8>>, uv_required: bool, pin: Option<Pin>) -> FromDriver {
        let pin_auth = match self.pin_auth(ceremony, pin, uv_required, permission::GET_ASSERTION, &rp_id, &cdh) {
            Ok(p) => p,
            Err(rec) => return rec,
        };
        let uv = pin_auth.is_some();
        let req = GetAssertion { rp_id: rp_id.clone(), client_data_hash: cdh, allow, up: true, pin_auth };
        let a = match self.until_touched(|c| c.get_assertion(&req)) {
            Ok(a) => a,
            // Parse failures include "a credential we did not allow".
            Err(e) => return FromDriver::Error { ceremony, error: map_err(e) },
        };
        let ad = &a.auth_data;
        let expected: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        if ad.rp_id_hash != expected || ad.flags & flags::UP == 0 || (uv && ad.flags & flags::UV == 0) {
            return FromDriver::Error { ceremony, error: DriverError::Device };
        }
        FromDriver::Asserted {
            ceremony,
            cred_id: a.cred_id,
            rp_id_hash: ad.rp_id_hash,
            flags: ad.flags,
            sign_count: ad.sign_count,
            auth_data: a.auth_data_raw,
            signature: a.signature,
            user_handle: a.user_id,
        }
    }
}

impl<D: HidDevice> Driver<D> {
    /// Run a CTAP2 command that waits for a touch, asking again when the
    /// key's own wait runs out (see [`TOUCH_WAIT`]).
    fn until_touched<T>(&mut self, mut f: impl FnMut(&mut Client<D>) -> Result<T, Error>) -> Result<T, Error> {
        let start = std::time::Instant::now();
        loop {
            match f(&mut self.client) {
                Err(Error::Status(status::USER_ACTION_TIMEOUT)) if !self.client.cancelled() && start.elapsed() < TOUCH_WAIT => continue,
                r => return r,
            }
        }
    }
}

impl<D: HidDevice> Driver<D> {
    fn handle_u2f(&mut self, req: ToDriver) -> Option<FromDriver> {
        match req {
            ToDriver::Register { ceremony, rp_id, client_data_hash, exclude, uv_required, .. } => {
                Some(self.u2f_register(ceremony, &rp_id, &client_data_hash, &exclude, uv_required))
            }
            ToDriver::Assert { ceremony, rp_id, client_data_hash, allow, uv_required, .. } => {
                Some(self.u2f_assert(ceremony, &rp_id, &client_data_hash, &allow, uv_required))
            }
            ToDriver::Cancel { .. } => None,
            ToDriver::ListCertificates { ceremony } | ToDriver::SignDigest { ceremony, .. } => {
                Some(FromDriver::Error { ceremony, error: DriverError::Unsupported })
            }
        }
    }

    /// Send an APDU until the user touches (`69 85` means not yet).
    fn u2f_until_touch(&mut self, apdu: &[u8]) -> Result<Vec<u8>, DriverError> {
        for _ in 0..self.u2f_polls {
            if self.client.cancelled() {
                return Err(DriverError::Denied);
            }
            match self.client.msg(apdu) {
                Ok((b, u2f::sw::OK)) => return Ok(b),
                Ok((_, u2f::sw::CONDITIONS_NOT_SATISFIED)) => {
                    self.touch.borrow_mut().needed();
                    std::thread::sleep(self.u2f_poll)
                }
                Ok((_, u2f::sw::WRONG_DATA)) => return Err(DriverError::NoCredentials),
                Ok((_, sw)) => {
                    eprintln!("sigillum-fido: U2F status {sw:#06x}");
                    return Err(DriverError::Device);
                }
                Err(e) => return Err(map_err(e)),
            }
        }
        Err(DriverError::Denied)
    }

    /// Which of these handles belong to this key for this app.
    fn u2f_owned(&mut self, cdh: &[u8; 32], app: &[u8; 32], handles: &[Vec<u8>]) -> Result<Option<Vec<u8>>, DriverError> {
        for h in handles {
            let Ok(apdu) = u2f::authenticate_request(u2f::P1_CHECK_ONLY, cdh, app, h) else { continue };
            match self.client.msg(&apdu) {
                Ok((_, u2f::sw::CONDITIONS_NOT_SATISFIED)) => return Ok(Some(h.clone())),
                Ok(_) => {}
                Err(e) => return Err(map_err(e)),
            }
        }
        Ok(None)
    }

    fn u2f_register(&mut self, ceremony: CeremonyId, rp_id: &str, cdh: &[u8; 32], exclude: &[Vec<u8>], uv_required: bool) -> FromDriver {
        let err = |error| FromDriver::Error { ceremony, error };
        if uv_required {
            return err(DriverError::UvUnavailable);
        }
        let app: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        match self.u2f_owned(cdh, &app, exclude) {
            Ok(Some(_)) => {
                // CTAP 2.1 §10.1.1: collect a touch on a throwaway
                // registration first, so exclusion can't be probed silently.
                let bogus = u2f::register_request(&[0; 32], &[0x41; 32]);
                return match self.u2f_until_touch(&bogus) {
                    Ok(_) => err(DriverError::Excluded),
                    Err(e) => err(e),
                };
            }
            Ok(None) => {}
            Err(e) => return err(e),
        }
        let body = match self.u2f_until_touch(&u2f::register_request(cdh, &app)) {
            Ok(b) => b,
            Err(e) => return err(e),
        };
        let reg = match u2f::parse_registration(&body) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("sigillum-fido: U2F registration ({} bytes) does not parse: {e:?}", body.len());
                return err(DriverError::Device);
            }
        };
        let Ok(pk) = p256::PublicKey::from_sec1_bytes(&reg.public_key) else {
            eprintln!("sigillum-fido: U2F registration has a bad public key");
            return err(DriverError::Device);
        };
        let cose = CoseKey::from_p256(alg::ES256, &pk);
        let ad = sigillum_ctap::authdata::AuthData {
            rp_id_hash: app,
            flags: flags::UP | flags::AT,
            sign_count: 0,
            attested: Some(sigillum_ctap::authdata::Attested { aaguid: [0; 16], cred_id: reg.key_handle.clone(), public_key: cose }),
            extensions: None,
        };
        let auth_data = ad.encode();
        let att_obj = sigillum_cbor::encode(&sigillum_cbor::Value::Map(vec![
            (sigillum_cbor::Value::text("fmt"), sigillum_cbor::Value::text("none")),
            (sigillum_cbor::Value::text("attStmt"), sigillum_cbor::Value::Map(Vec::new())),
            (sigillum_cbor::Value::text("authData"), sigillum_cbor::Value::bytes(&auth_data)),
        ]));
        FromDriver::Registered {
            ceremony,
            cred_id: reg.key_handle,
            rp_id_hash: app,
            flags: flags::UP | flags::AT,
            sign_count: 0,
            cose_alg: alg::ES256,
            public_key: reg.public_key.to_vec(),
            auth_data,
            att_obj,
        }
    }

    fn u2f_assert(&mut self, ceremony: CeremonyId, rp_id: &str, cdh: &[u8; 32], allow: &[Vec<u8>], uv_required: bool) -> FromDriver {
        let err = |error| FromDriver::Error { ceremony, error };
        if uv_required {
            return err(DriverError::UvUnavailable);
        }
        let app: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        let handle = match self.u2f_owned(cdh, &app, allow) {
            Ok(Some(h)) => h,
            Ok(None) => return err(DriverError::NoCredentials),
            Err(e) => return err(e),
        };
        let Ok(apdu) = u2f::authenticate_request(u2f::P1_ENFORCE, cdh, &app, &handle) else { return err(DriverError::Device) };
        let body = match self.u2f_until_touch(&apdu) {
            Ok(b) => b,
            Err(e) => return err(e),
        };
        let a = match u2f::parse_authentication(&body) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("sigillum-fido: U2F authentication ({} bytes) does not parse: {e:?}", body.len());
                return err(DriverError::Device);
            }
        };
        if a.up & flags::UP == 0 {
            return err(DriverError::Device);
        }
        let mut auth_data = app.to_vec();
        auth_data.push(a.up);
        auth_data.extend_from_slice(&a.counter.to_be_bytes());
        FromDriver::Asserted {
            ceremony,
            cred_id: handle,
            rp_id_hash: app,
            flags: a.up,
            sign_count: a.counter,
            auth_data,
            signature: a.signature,
            user_handle: None,
        }
    }
}

mod sigillum_ctap_sec1 {
    pub trait ToSec1 {
        fn to_sec1_vec(&self) -> Vec<u8>;
    }
    impl ToSec1 for p256::PublicKey {
        fn to_sec1_vec(&self) -> Vec<u8> {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            self.to_encoded_point(false).as_bytes().to_vec()
        }
    }
}

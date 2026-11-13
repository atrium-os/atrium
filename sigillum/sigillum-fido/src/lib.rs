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

use sha2::{Digest, Sha256};
use sigillum_ctap::authdata::flags;
use sigillum_ctap::client::Client;
use sigillum_ctap::cmd::{GetAssertion, Info, MakeCredential, User};
use sigillum_ctap::cose::{alg, CoseKey};
use sigillum_ctap::hid::HidDevice;
use sigillum_ctap::pin::permission;
use sigillum_ctap::{status, Error};
use sigillum_proto::client::CeremonyId;
use sigillum_proto::driver::{DeviceClass, DriverError, FidoCaps, FromDriver, Pin, ToDriver};

pub struct Driver<D: HidDevice> {
    client: Client<D>,
    info: Info,
}

fn map_err(e: Error) -> DriverError {
    match e {
        Error::Status(status::OPERATION_DENIED) | Error::Status(status::USER_ACTION_TIMEOUT) | Error::Status(status::KEEPALIVE_CANCEL) => {
            DriverError::Denied
        }
        Error::Status(status::CREDENTIAL_EXCLUDED) => DriverError::Excluded,
        Error::Status(status::NO_CREDENTIALS) => DriverError::NoCredentials,
        Error::Status(status::PIN_BLOCKED) | Error::Status(status::PIN_AUTH_BLOCKED) => DriverError::PinBlocked,
        Error::Io(_) => DriverError::Gone,
        _ => DriverError::Device,
    }
}

impl<D: HidDevice> Driver<D> {
    /// `INIT` and `GetInfo`; the `Hello` record for `sigillumd`.
    pub fn open(dev: D, vendor: u16, product: u16, name: &str) -> Result<(Driver<D>, FromDriver), Error> {
        let mut client = Client::open(dev)?;
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
        Ok((Driver { client, info }, hello))
    }

    /// Run one request to its record. Nothing here blocks on the user
    /// except the authenticator's own touch wait.
    pub fn handle(&mut self, req: ToDriver) -> Option<FromDriver> {
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
        let pin_set = self.info.option("clientPin") == Some(true);
        if !pin_set {
            if uv_required && self.info.option("uv") != Some(true) {
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
        let fault = |_| FromDriver::Error { ceremony, error: DriverError::Device };
        let att = match self.client.make_credential(&req) {
            Ok(a) => a,
            Err(e) => return FromDriver::Error { ceremony, error: map_err(e) },
        };
        let ad = &att.auth_data;
        let expected: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        if ad.rp_id_hash != expected || ad.flags & flags::UP == 0 || (uv && ad.flags & flags::UV == 0) {
            return fault(());
        }
        let Some(cred) = ad.attested.as_ref() else { return fault(()) };
        let CoseKey::P256 { alg: alg::ES256, .. } = cred.public_key else { return fault(()) };
        let Some(pk) = cred.public_key.p256() else { return fault(()) };
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
        let a = match self.client.get_assertion(&req) {
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

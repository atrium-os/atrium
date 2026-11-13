//! The two ceremonies, end to end: checked options and the origin the
//! broker knows in, a `PublicKeyCredential` in WebAuthn L3's JSON forms
//! (`RegistrationResponseJSON`, `AuthenticationResponseJSON`) out.
//!
//! What comes back from `sigillumd` is checked again before the page sees
//! it: the RP ID hash, user presence, user verification when it was
//! required, and that the credential is the one the reply names. A reply
//! that fails a check is `NotAllowedError`, like any other failure the
//! page may not learn more about (§9.4).

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use sigillum_cbor::Value;
use sigillum_ctap::authdata::{flags, AuthData};
use sigillum_ctap::cose::{alg, CoseKey};
use sigillum_proto::client::{self as wire, Assert, Attestation, AuthenticatorClass, CeremonyId, Refusal, Register, Reply, Request};
use sigillum_proto::{read_msg, write_msg};

use crate::client_data::{self, client_data_json};
use crate::options::{CreationOptions, RequestOptions, Uv};
use crate::origin::Origin;
use crate::{b64url, DomError};

/// The way to `sigillumd`: one request out, its reply back.
pub trait Transport {
    /// A ceremony id not used before on this transport.
    fn next_ceremony(&mut self) -> CeremonyId;
    fn ask(&mut self, req: &Request) -> io::Result<Reply>;
}

/// `sigillumd`'s client socket.
pub struct Sigillumd {
    stream: UnixStream,
    next: CeremonyId,
    /// The ceremony in flight, for [`Canceller`]; 0 when none.
    current: Arc<AtomicU64>,
}

impl Sigillumd {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Sigillumd> {
        Ok(Sigillumd::from_stream(UnixStream::connect(path)?))
    }

    pub fn from_stream(stream: UnixStream) -> Sigillumd {
        Sigillumd { stream, next: 1, current: Arc::new(AtomicU64::new(0)) }
    }

    /// A handle that cancels the ceremony in flight from another thread
    /// (the page's `AbortSignal`, or the tab going away).
    pub fn canceller(&self) -> io::Result<Canceller> {
        Ok(Canceller { stream: self.stream.try_clone()?, current: self.current.clone() })
    }
}

impl Transport for Sigillumd {
    fn next_ceremony(&mut self) -> CeremonyId {
        let id = self.next;
        self.next += 1;
        id
    }

    fn ask(&mut self, req: &Request) -> io::Result<Reply> {
        self.current.store(req.ceremony(), Ordering::SeqCst);
        let r = write_msg(&mut self.stream, req, wire::MAX_MESSAGE).and_then(|()| read_msg::<Reply>(&mut self.stream, wire::MAX_MESSAGE));
        self.current.store(0, Ordering::SeqCst);
        r?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "sigillumd closed"))
    }
}

pub struct Canceller {
    stream: UnixStream,
    current: Arc<AtomicU64>,
}

impl Canceller {
    /// Cancel the ceremony in flight, if any. Its `ask` then returns the
    /// daemon's `Refused`, which the page sees as `NotAllowedError`.
    pub fn cancel(&mut self) -> io::Result<()> {
        match self.current.load(Ordering::SeqCst) {
            0 => Ok(()),
            id => write_msg(&mut self.stream, &Request::Cancel { ceremony: id }, wire::MAX_MESSAGE),
        }
    }
}

/// `RegistrationResponseJSON`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub id: String,
    pub raw_id: String,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub authenticator_attachment: &'static str,
    pub response: AttestationResponse,
    pub client_extension_results: Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub authenticator_data: String,
    pub transports: Vec<&'static str>,
    /// SubjectPublicKeyInfo, DER.
    pub public_key: String,
    pub public_key_algorithm: i64,
    pub attestation_object: String,
}

/// `AuthenticationResponseJSON`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authentication {
    pub id: String,
    pub raw_id: String,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub authenticator_attachment: &'static str,
    pub response: AssertionResponse,
    pub client_extension_results: Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssertionResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub authenticator_data: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_handle: Option<String>,
}

/// `{}`: no client extensions are processed yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Empty {}

impl Registration {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("plain data")
    }
}

impl Authentication {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("plain data")
    }
}

fn wire_uv(uv: Uv) -> wire::Uv {
    match uv {
        Uv::Required => wire::Uv::Required,
        Uv::Preferred => wire::Uv::Preferred,
        Uv::Discouraged => wire::Uv::Discouraged,
    }
}

fn refusal(r: Refusal) -> DomError {
    match r {
        Refusal::Excluded => DomError::InvalidState,
        // Everything else, including a policy refusal, is NotAllowed: the
        // page learns nothing about why (§9.4).
        _ => DomError::NotAllowed,
    }
}

fn attachment(c: AuthenticatorClass) -> &'static str {
    match c {
        AuthenticatorClass::Platform => "platform",
        _ => "cross-platform",
    }
}

fn transports(c: AuthenticatorClass) -> Vec<&'static str> {
    vec![match c {
        AuthenticatorClass::Platform => "internal",
        AuthenticatorClass::Usb => "usb",
        AuthenticatorClass::Ble => "ble",
        AuthenticatorClass::Nfc => "nfc",
        AuthenticatorClass::Hybrid => "hybrid",
    }]
}

/// The checks every reply's authenticator data must pass.
fn check(auth_data: &[u8], rp_id: &str, uv: Uv) -> Result<AuthData, DomError> {
    let ad = AuthData::parse(auth_data).map_err(|_| DomError::NotAllowed)?;
    let want: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
    if ad.rp_id_hash != want || ad.flags & flags::UP == 0 {
        return Err(DomError::NotAllowed);
    }
    if uv == Uv::Required && ad.flags & flags::UV == 0 {
        return Err(DomError::NotAllowed);
    }
    Ok(ad)
}

/// The DER prefix of a P-256 SubjectPublicKeyInfo (id-ecPublicKey,
/// prime256v1), before the 65-byte uncompressed point.
const P256_SPKI: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

fn spki(x: &[u8; 32], y: &[u8; 32]) -> Vec<u8> {
    let mut v = P256_SPKI.to_vec();
    v.push(0x04);
    v.extend_from_slice(x);
    v.extend_from_slice(y);
    v
}

/// `navigator.credentials.create()`. `origin` is the broker's (from the
/// courier), never the page's word; `top_origin` only for a cross-origin
/// frame.
pub fn create(t: &mut impl Transport, origin: &Origin, top_origin: Option<&Origin>, o: &CreationOptions) -> Result<Registration, DomError> {
    let rp_id = origin.rp_id(o.rp_id.as_deref())?;
    let cdj = client_data_json(client_data::Type::Create, &o.challenge, origin, top_origin);
    let ceremony = t.next_ceremony();
    let req = Request::Register(Register {
        ceremony,
        timeout_ms: o.timeout_ms,
        rp_id: rp_id.clone(),
        rp_name: o.rp_name.clone(),
        client_data_hash: client_data::hash(&cdj),
        user_id: o.user_id.clone(),
        user_name: o.user_name.clone(),
        user_display_name: o.user_display_name.clone(),
        exclude: o.exclude.clone(),
        resident_key: o.resident_key,
        uv: wire_uv(o.uv),
        // `direct` needs the user's yes on the sheet (§9.3), not built.
        attestation: Attestation::None,
    });
    let reply = t.ask(&req).map_err(|_| DomError::NotAllowed)?;
    let (cred_id, auth_data, att_obj, class) = match reply {
        Reply::Registered { ceremony: c, cred_id, auth_data, att_obj, authenticator } if c == ceremony => (cred_id, auth_data, att_obj, authenticator),
        Reply::Refused { ceremony: c, reason } if c == ceremony => return Err(refusal(reason)),
        _ => return Err(DomError::NotAllowed),
    };
    let ad = check(&auth_data, &rp_id, o.uv)?;
    let attested = ad.attested.as_ref().ok_or(DomError::NotAllowed)?;
    if attested.cred_id != cred_id {
        return Err(DomError::NotAllowed);
    }
    let CoseKey::P256 { alg: alg::ES256, x, y } = &attested.public_key else { return Err(DomError::NotAllowed) };
    // The attestation object must carry the same authenticator data.
    let obj = sigillum_cbor::decode(&att_obj).map_err(|_| DomError::NotAllowed)?;
    if obj.get_text("authData").and_then(Value::as_bytes) != Some(&auth_data[..]) {
        return Err(DomError::NotAllowed);
    }
    Ok(Registration {
        id: b64url(&cred_id),
        raw_id: b64url(&cred_id),
        ty: "public-key",
        authenticator_attachment: attachment(class),
        response: AttestationResponse {
            client_data_json: b64url(cdj.as_bytes()),
            authenticator_data: b64url(&auth_data),
            transports: transports(class),
            public_key: b64url(&spki(x, y)),
            public_key_algorithm: alg::ES256,
            attestation_object: b64url(&att_obj),
        },
        client_extension_results: Empty {},
    })
}

/// `navigator.credentials.get()`, as [`create`].
pub fn get(t: &mut impl Transport, origin: &Origin, top_origin: Option<&Origin>, o: &RequestOptions) -> Result<Authentication, DomError> {
    let rp_id = origin.rp_id(o.rp_id.as_deref())?;
    let cdj = client_data_json(client_data::Type::Get, &o.challenge, origin, top_origin);
    let ceremony = t.next_ceremony();
    let req = Request::Assert(Assert {
        ceremony,
        timeout_ms: o.timeout_ms,
        rp_id: rp_id.clone(),
        client_data_hash: client_data::hash(&cdj),
        allow: o.allow.clone(),
        uv: wire_uv(o.uv),
    });
    let reply = t.ask(&req).map_err(|_| DomError::NotAllowed)?;
    let (cred_id, auth_data, signature, user_handle, class) = match reply {
        Reply::Asserted { ceremony: c, cred_id, auth_data, signature, user_handle, authenticator } if c == ceremony => {
            (cred_id, auth_data, signature, user_handle, authenticator)
        }
        Reply::Refused { ceremony: c, reason } if c == ceremony => return Err(refusal(reason)),
        _ => return Err(DomError::NotAllowed),
    };
    check(&auth_data, &rp_id, o.uv)?;
    if !o.allow.is_empty() && !o.allow.contains(&cred_id) {
        return Err(DomError::NotAllowed);
    }
    // A discoverable credential (empty allow list) must name its user.
    if o.allow.is_empty() && user_handle.as_ref().is_none_or(|h| h.is_empty()) {
        return Err(DomError::NotAllowed);
    }
    Ok(Authentication {
        id: b64url(&cred_id),
        raw_id: b64url(&cred_id),
        ty: "public-key",
        authenticator_attachment: attachment(class),
        response: AssertionResponse {
            client_data_json: b64url(cdj.as_bytes()),
            authenticator_data: b64url(&auth_data),
            signature: b64url(&signature),
            user_handle: user_handle.filter(|h| !h.is_empty()).map(|h| b64url(&h)),
        },
        client_extension_results: Empty {},
    })
}

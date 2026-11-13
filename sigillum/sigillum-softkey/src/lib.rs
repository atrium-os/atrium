//! A software FIDO2 authenticator for tests (sigillum.md §15).
//!
//! It speaks CTAPHID over 64-byte reports through [`HidDevice`], so the
//! platform code under test is exactly what drives a real key: channel
//! allocation, framing, keepalives, CTAP2 `GetInfo`, `MakeCredential`,
//! `GetAssertion` and `ClientPIN` with PIN/UV auth protocol 2. Credentials
//! are P-256 (ES256); attestation is `packed` self-attestation, so the
//! "strip to none" path is exercised.
//!
//! Test knobs: [`SoftKey::touch`] (does the user touch it), and
//! [`SoftKey::lie`] (misbehave in a chosen way, to test that the platform
//! catches it). It is not a security boundary and never ships.
#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};

use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use sha2::{Digest, Sha256};
use sigillum_cbor::Value;
use sigillum_ctap::authdata::{flags, Attested, AuthData};
use sigillum_ctap::cmd::{self, Assertion, Attestation, ClientPin, ClientPinResponse, GetAssertion, Info, MakeCredential};
use sigillum_ctap::cose::{alg, CoseKey};
use sigillum_ctap::hid::{self, Event, HidDevice, InitResponse, Reassembler, REPORT};
use sigillum_ctap::pin::{self, permission, Shared};
use sigillum_ctap::{status, Error};
use zeroize::Zeroizing;

pub const AAGUID: [u8; 16] = *b"sigillum-softkey";

/// Ways to misbehave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lie {
    /// Put another RP's hash in the authenticator data.
    WrongRpHash,
    /// Answer without the UP flag.
    NoUp,
    /// Answer a GetAssertion with a credential that wasn't allowed.
    OtherCredential,
}

struct Cred {
    id: Vec<u8>,
    /// SHA-256 of the RP ID (the U2F application parameter).
    app: [u8; 32],
    user_id: Vec<u8>,
    resident: bool,
    key: SigningKey,
}

struct Token {
    value: Zeroizing<[u8; 32]>,
    permissions: u8,
    rp_id: Option<String>,
    proto: u8,
}

pub struct SoftKey {
    next_cid: u32,
    inbox: HashMap<u32, Reassembler>,
    outbox: VecDeque<[u8; REPORT]>,
    pin_hash: Option<Zeroizing<[u8; 16]>>,
    pub pin_retries: u64,
    key_agreement: p256::SecretKey,
    token: Option<Token>,
    creds: Vec<Cred>,
    sign_count: u32,
    /// Whether the simulated user touches the key when asked.
    pub touch: bool,
    /// How many touch waits run out first (CTAP2 `USER_ACTION_TIMEOUT`, as
    /// a real key answers after about 30 s with no touch).
    pub slow_touch: u32,
    /// Send a keepalive (`UP_NEEDED`) before each answer that needed a touch.
    pub keepalives: bool,
    pub lie: Option<Lie>,
    /// Commands seen, for tests that check nothing reached the key.
    pub commands: Vec<u8>,
    /// Behave as a U2F-only key (no `CBOR` capability), like QEMU's
    /// `u2f-emulated`.
    pub u2f_only: bool,
    /// The PIN/UV auth protocols it lists: [2] by default; [1] behaves as a
    /// CTAP 2.0 key (a YubiKey 5 on firmware 5.4): `getPinToken` only, and
    /// protocol 2's token subcommand is INVALID_COMMAND.
    pub pin_protocols: Vec<u8>,
    /// Lists the `pinUvAuthToken` option and answers 0x09; false behaves
    /// as a CTAP 2.0 / 2.1-PRE key (0x09 is INVALID_COMMAND).
    pub pin_uv_auth_token: bool,
}

impl Default for SoftKey {
    fn default() -> SoftKey {
        SoftKey::new(None)
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("getrandom");
    b
}

fn app_of(rp_id: &str) -> [u8; 32] {
    Sha256::digest(rp_id.as_bytes()).into()
}

fn new_secret() -> p256::SecretKey {
    p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng)
}

impl SoftKey {
    pub fn new(pin_utf8: Option<&[u8]>) -> SoftKey {
        SoftKey {
            next_cid: 0x1000_0001,
            inbox: HashMap::new(),
            outbox: VecDeque::new(),
            pin_hash: pin_utf8.map(pin::pin_hash),
            pin_retries: 8,
            key_agreement: new_secret(),
            token: None,
            creds: Vec::new(),
            sign_count: 0,
            touch: true,
            slow_touch: 0,
            keepalives: true,
            lie: None,
            commands: Vec::new(),
            u2f_only: false,
            pin_protocols: vec![pin::PROTOCOL],
            pin_uv_auth_token: true,
        }
    }

    pub fn credential_count(&self) -> usize {
        self.creds.len()
    }

    fn send(&mut self, cid: u32, command: u8, payload: &[u8]) {
        for p in hid::frame(cid, command, payload).expect("response fits CTAPHID") {
            self.outbox.push_back(p);
        }
    }

    fn handle(&mut self, cid: u32, command: u8, body: Vec<u8>) {
        match command {
            hid::cmd::INIT => {
                if body.len() != 8 {
                    return self.send(cid, hid::cmd::ERROR, &[hid::err::INVALID_LEN]);
                }
                let new = if cid == hid::BROADCAST {
                    let c = self.next_cid;
                    self.next_cid += 1;
                    c
                } else {
                    cid
                };
                let r = InitResponse {
                    nonce: body[..8].try_into().unwrap(),
                    cid: new,
                    protocol: 2,
                    version: (1, 0, 0),
                    caps: if self.u2f_only { hid::caps::WINK } else { hid::caps::CBOR | hid::caps::NMSG },
                };
                self.send(cid, hid::cmd::INIT, &r.encode());
            }
            hid::cmd::MSG if cid != hid::BROADCAST && self.u2f_only => {
                let out = self.u2f(&body);
                self.send(cid, hid::cmd::MSG, &out);
            }
            hid::cmd::CBOR if cid != hid::BROADCAST && !body.is_empty() && !self.u2f_only => {
                self.commands.push(body[0]);
                let (st, resp, needed_touch) = match self.cbor(body[0], &body[1..]) {
                    Ok((r, t)) => (status::OK, r, t),
                    Err(Error::Status(s)) => (s, Vec::new(), false),
                    Err(_) => (status::INVALID_CBOR, Vec::new(), false),
                };
                if needed_touch && self.keepalives {
                    self.send(cid, hid::cmd::KEEPALIVE, &[hid::keepalive::UP_NEEDED]);
                }
                let mut out = vec![st];
                out.extend_from_slice(&resp);
                self.send(cid, hid::cmd::CBOR, &out);
            }
            hid::cmd::CANCEL => {}
            _ => self.send(cid, hid::cmd::ERROR, &[hid::err::INVALID_CMD]),
        }
    }

    /// One CTAP2 command: (response body, whether it asked for a touch).
    fn cbor(&mut self, command: u8, params: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        match command {
            cmd::GET_INFO => Ok((sigillum_cbor::encode(&self.info().to_value()), false)),
            cmd::MAKE_CREDENTIAL => self.make_credential(MakeCredential::parse(params)?),
            cmd::GET_ASSERTION => self.get_assertion(GetAssertion::parse(params)?),
            cmd::CLIENT_PIN => Ok((self.client_pin(ClientPin::parse(params)?)?, false)),
            _ => Err(Error::Status(status::INVALID_COMMAND)),
        }
    }

    /// One U2F APDU (extended length) → response with status word.
    fn u2f(&mut self, apdu: &[u8]) -> Vec<u8> {
        use sigillum_ctap::u2f;
        let sw = |s: u16| s.to_be_bytes().to_vec();
        if apdu.len() < 7 || apdu[0] != 0 || apdu[4] != 0 {
            return sw(0x6700);
        }
        let (ins, p1) = (apdu[1], apdu[2]);
        let n = u16::from_be_bytes([apdu[5], apdu[6]]) as usize;
        let Some(data) = apdu.get(7..7 + n) else { return sw(0x6700) };
        self.commands.push(0x80 | ins);
        match ins {
            u2f::REGISTER if n == 64 => {
                if !self.touch {
                    return sw(u2f::sw::CONDITIONS_NOT_SATISFIED);
                }
                let cdh: [u8; 32] = data[..32].try_into().unwrap();
                let app: [u8; 32] = data[32..].try_into().unwrap();
                let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
                let id = random::<32>().to_vec();
                let pk = p256::PublicKey::from(key.verifying_key());
                use p256::elliptic_curve::sec1::ToEncodedPoint;
                let point = pk.to_encoded_point(false);
                // Self-signed "attestation": the signature is over
                // 00 | app | cdh | handle | pubkey with the credential key.
                let mut signed = vec![0u8];
                signed.extend_from_slice(&app);
                signed.extend_from_slice(&cdh);
                signed.extend_from_slice(&id);
                signed.extend_from_slice(point.as_bytes());
                let sig: Signature = key.sign(&signed);
                let mut out = vec![0x05];
                out.extend_from_slice(point.as_bytes());
                out.push(id.len() as u8);
                out.extend_from_slice(&id);
                out.extend_from_slice(&[0x30, 0x03, 0x02, 0x01, 0x00]); // a stub DER "certificate"
                out.extend_from_slice(sig.to_der().as_bytes());
                self.creds.push(Cred { id, app, user_id: Vec::new(), resident: false, key });
                out.extend_from_slice(&sw(u2f::sw::OK));
                out
            }
            u2f::AUTHENTICATE if n >= 65 => {
                let cdh: [u8; 32] = data[..32].try_into().unwrap();
                let app: [u8; 32] = data[32..64].try_into().unwrap();
                let l = data[64] as usize;
                let Some(handle) = data.get(65..65 + l) else { return sw(0x6700) };
                let Some(i) = self.creds.iter().position(|c| c.id == handle && c.app == app) else {
                    return sw(u2f::sw::WRONG_DATA);
                };
                if p1 == u2f::P1_CHECK_ONLY || !self.touch {
                    return sw(u2f::sw::CONDITIONS_NOT_SATISFIED);
                }
                self.sign_count += 1;
                let up = self.up_flag();
                let mut signed = app.to_vec();
                signed.push(up);
                signed.extend_from_slice(&self.sign_count.to_be_bytes());
                signed.extend_from_slice(&cdh);
                let sig: Signature = self.creds[i].key.sign(&signed);
                let mut out = vec![up];
                out.extend_from_slice(&self.sign_count.to_be_bytes());
                out.extend_from_slice(sig.to_der().as_bytes());
                out.extend_from_slice(&sw(u2f::sw::OK));
                out
            }
            u2f::VERSION => {
                let mut out = b"U2F_V2".to_vec();
                out.extend_from_slice(&sw(u2f::sw::OK));
                out
            }
            _ => sw(0x6d00),
        }
    }

    pub fn info(&self) -> Info {
        Info {
            versions: vec!["FIDO_2_0".into(), "FIDO_2_1".into()],
            extensions: vec!["credProtect".into(), "hmac-secret".into()],
            aaguid: AAGUID,
            options: vec![
                ("rk".into(), true),
                ("up".into(), true),
                ("clientPin".into(), self.pin_hash.is_some()),
                ("pinUvAuthToken".into(), self.pin_uv_auth_token),
            ],
            max_msg_size: Some(1200),
            pin_protocols: self.pin_protocols.clone(),
        }
    }

    /// Check a pinUvAuthParam; `Ok(true)` when it verifies the user.
    fn check_pin_auth(&self, pa: &Option<cmd::PinAuth>, cdh: &[u8; 32], perm: u8, rp_id: &str) -> Result<bool, Error> {
        match pa {
            None if self.pin_hash.is_some() => Err(Error::Status(status::PUAT_REQUIRED)),
            None => Ok(false),
            Some(p) => {
                if !self.pin_protocols.contains(&p.protocol) {
                    return Err(Error::Status(status::INVALID_PARAMETER));
                }
                let t = self.token.as_ref().ok_or(Error::Status(status::PIN_AUTH_INVALID))?;
                if t.proto != p.protocol || !pin::verify_with(p.protocol, &*t.value, cdh, &p.param) {
                    return Err(Error::Status(status::PIN_AUTH_INVALID));
                }
                if t.permissions & perm == 0 || t.rp_id.as_deref().is_some_and(|r| r != rp_id) {
                    return Err(Error::Status(status::PIN_AUTH_INVALID));
                }
                Ok(true)
            }
        }
    }

    fn rp_hash(&self, rp_id: &str) -> [u8; 32] {
        let real: [u8; 32] = Sha256::digest(rp_id.as_bytes()).into();
        if self.lie == Some(Lie::WrongRpHash) {
            Sha256::digest(b"evil.example").into()
        } else {
            real
        }
    }

    fn up_flag(&self) -> u8 {
        if self.lie == Some(Lie::NoUp) {
            0
        } else {
            flags::UP
        }
    }

    fn make_credential(&mut self, req: MakeCredential) -> Result<(Vec<u8>, bool), Error> {
        let uv = self.check_pin_auth(&req.pin_auth, &req.client_data_hash, permission::MAKE_CREDENTIAL, &req.rp_id)?;
        if self.creds.iter().any(|c| c.app == app_of(&req.rp_id) && req.exclude.contains(&c.id)) {
            // The spec asks for a touch first, so excluded-ness can't be probed.
            return Err(Error::Status(if self.touch { status::CREDENTIAL_EXCLUDED } else { status::OPERATION_DENIED }));
        }
        if self.slow_touch > 0 {
            self.slow_touch -= 1;
            return Err(Error::Status(status::USER_ACTION_TIMEOUT));
        }
        if !self.touch {
            return Err(Error::Status(status::OPERATION_DENIED));
        }
        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let id = random::<32>().to_vec();
        self.sign_count += 1;
        let mut f = self.up_flag() | flags::AT;
        if uv {
            f |= flags::UV;
        }
        let extensions = req.cred_protect.map(|p| {
            f |= flags::ED;
            Value::Map(vec![(Value::text("credProtect"), Value::Uint(p as u64))])
        });
        let ad = AuthData {
            rp_id_hash: self.rp_hash(&req.rp_id),
            flags: f,
            sign_count: self.sign_count,
            attested: Some(Attested {
                aaguid: AAGUID,
                cred_id: id.clone(),
                public_key: CoseKey::from_p256(alg::ES256, &p256::PublicKey::from(key.verifying_key())),
            }),
            extensions,
        };
        let raw = ad.encode();
        let mut signed = raw.clone();
        signed.extend_from_slice(&req.client_data_hash);
        let sig: Signature = key.sign(&signed);
        let att = Attestation {
            fmt: "packed".into(),
            auth_data_raw: raw,
            auth_data: ad,
            att_stmt: Value::Map(vec![
                (Value::text("alg"), Value::int(alg::ES256)),
                (Value::text("sig"), Value::bytes(sig.to_der().as_bytes())),
            ]),
        };
        self.creds.push(Cred { id, app: app_of(&req.rp_id), user_id: req.user.id, resident: req.resident_key, key });
        Ok((att.encode(), true))
    }

    fn get_assertion(&mut self, req: GetAssertion) -> Result<(Vec<u8>, bool), Error> {
        let uv = self.check_pin_auth(&req.pin_auth, &req.client_data_hash, permission::GET_ASSERTION, &req.rp_id)?;
        let matching: Vec<usize> = self
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.app == app_of(&req.rp_id) && (if req.allow.is_empty() { c.resident } else { req.allow.contains(&c.id) }))
            .map(|(i, _)| i)
            .collect();
        let Some(&first) = matching.first() else {
            return Err(Error::Status(status::NO_CREDENTIALS));
        };
        if req.up && self.slow_touch > 0 {
            self.slow_touch -= 1;
            return Err(Error::Status(status::USER_ACTION_TIMEOUT));
        }
        if req.up && !self.touch {
            return Err(Error::Status(status::OPERATION_DENIED));
        }
        self.sign_count += 1;
        let mut f = if req.up { self.up_flag() } else { 0 };
        if uv {
            f |= flags::UV;
        }
        let ad = AuthData { rp_id_hash: self.rp_hash(&req.rp_id), flags: f, sign_count: self.sign_count, attested: None, extensions: None };
        let raw = ad.encode();
        let mut signed = raw.clone();
        signed.extend_from_slice(&req.client_data_hash);
        let c = &self.creds[first];
        let sig: Signature = c.key.sign(&signed);
        let cred_id = if self.lie == Some(Lie::OtherCredential) { vec![0xEE; 32] } else { c.id.clone() };
        let a = Assertion {
            cred_id,
            auth_data_raw: raw,
            auth_data: ad,
            signature: sig.to_der().as_bytes().to_vec(),
            user_id: if c.resident { Some(c.user_id.clone()) } else { None },
            count: if req.allow.is_empty() && matching.len() > 1 { Some(matching.len() as u64) } else { None },
        };
        Ok((a.encode(), req.up))
    }

    fn client_pin(&mut self, req: ClientPin) -> Result<Vec<u8>, Error> {
        if !self.pin_protocols.contains(&req.protocol) {
            return Err(Error::Status(status::INVALID_PARAMETER));
        }
        let v21 = self.pin_uv_auth_token;
        match req.sub {
            cmd::pin_sub::GET_PIN_RETRIES => Ok(ClientPinResponse { retries: Some(self.pin_retries), ..Default::default() }.encode()),
            cmd::pin_sub::GET_KEY_AGREEMENT => Ok(ClientPinResponse {
                key_agreement: Some(CoseKey::from_p256(alg::ECDH_ES_HKDF_256, &self.key_agreement.public_key())),
                ..Default::default()
            }
            .encode()),
            cmd::pin_sub::GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS if !v21 => Err(Error::Status(status::INVALID_COMMAND)),
            // CTAP 2.0's getPinToken: a token for MakeCredential and
            // GetAssertion on any RP.
            cmd::pin_sub::GET_PIN_TOKEN => {
                let Some(stored) = self.pin_hash.as_ref() else {
                    return Err(Error::Status(status::PIN_NOT_SET));
                };
                if self.pin_retries == 0 {
                    return Err(Error::Status(status::PIN_BLOCKED));
                }
                let (Some(peer), Some(enc)) = (req.key_agreement, req.pin_hash_enc) else {
                    return Err(Error::Status(status::MISSING_PARAMETER));
                };
                let shared = Shared::agree_with(req.protocol, &self.key_agreement, &peer)?;
                let got = shared.decrypt(&enc)?;
                self.pin_retries -= 1;
                if got.len() != 16 || got[..] != stored[..] {
                    self.key_agreement = new_secret();
                    return Err(Error::Status(if self.pin_retries == 0 { status::PIN_BLOCKED } else { status::PIN_INVALID }));
                }
                self.pin_retries = 8;
                let value = Zeroizing::new(random::<32>());
                let enc = shared.encrypt(&*value)?;
                self.token = Some(Token { value, permissions: permission::MAKE_CREDENTIAL | permission::GET_ASSERTION, rp_id: None, proto: req.protocol });
                Ok(ClientPinResponse { token_enc: Some(enc), ..Default::default() }.encode())
            }
            cmd::pin_sub::GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS if req.protocol == pin::PROTOCOL => {
                let Some(stored) = self.pin_hash.as_ref() else {
                    return Err(Error::Status(status::PIN_NOT_SET));
                };
                if self.pin_retries == 0 {
                    return Err(Error::Status(status::PIN_BLOCKED));
                }
                let (Some(peer), Some(enc), Some(perms)) = (req.key_agreement, req.pin_hash_enc, req.permissions) else {
                    return Err(Error::Status(status::MISSING_PARAMETER));
                };
                if perms == 0 {
                    return Err(Error::Status(status::INVALID_PARAMETER));
                }
                let shared = Shared::agree(&self.key_agreement, &peer)?;
                let got = shared.decrypt(&enc)?;
                self.pin_retries -= 1;
                if got.len() != 16 || got[..] != stored[..] {
                    self.key_agreement = new_secret();
                    return Err(Error::Status(if self.pin_retries == 0 { status::PIN_BLOCKED } else { status::PIN_INVALID }));
                }
                self.pin_retries = 8;
                let value = Zeroizing::new(random::<32>());
                let enc = shared.encrypt(&*value)?;
                self.token = Some(Token { value, permissions: perms, rp_id: req.rp_id, proto: pin::PROTOCOL });
                Ok(ClientPinResponse { token_enc: Some(enc), ..Default::default() }.encode())
            }
            _ => Err(Error::Status(status::INVALID_PARAMETER)),
        }
    }
}


impl HidDevice for SoftKey {
    fn write_report(&mut self, report: &[u8; REPORT]) -> Result<(), Error> {
        let cid = u32::from_be_bytes(report[..4].try_into().unwrap());
        let r = self.inbox.entry(cid).or_insert_with(|| Reassembler::new(cid));
        match r.feed(report) {
            Ok(Event::Message(c, body)) => self.handle(cid, c, body),
            Ok(_) => {}
            Err(_) => {
                self.inbox.remove(&cid);
                self.send(cid, hid::cmd::ERROR, &[hid::err::INVALID_SEQ]);
            }
        }
        Ok(())
    }

    fn read_report(&mut self, _timeout_ms: u32) -> Result<[u8; REPORT], Error> {
        self.outbox.pop_front().ok_or(Error::Timeout)
    }
}

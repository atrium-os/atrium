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
    rp_id: String,
    user_id: Vec<u8>,
    resident: bool,
    key: SigningKey,
}

struct Token {
    value: Zeroizing<[u8; 32]>,
    permissions: u8,
    rp_id: Option<String>,
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
    /// Send a keepalive (`UP_NEEDED`) before each answer that needed a touch.
    pub keepalives: bool,
    pub lie: Option<Lie>,
    /// Commands seen, for tests that check nothing reached the key.
    pub commands: Vec<u8>,
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
            keepalives: true,
            lie: None,
            commands: Vec::new(),
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
                    caps: hid::caps::CBOR | hid::caps::NMSG,
                };
                self.send(cid, hid::cmd::INIT, &r.encode());
            }
            hid::cmd::CBOR if cid != hid::BROADCAST && !body.is_empty() => {
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

    pub fn info(&self) -> Info {
        Info {
            versions: vec!["FIDO_2_0".into(), "FIDO_2_1".into()],
            extensions: vec!["credProtect".into(), "hmac-secret".into()],
            aaguid: AAGUID,
            options: vec![
                ("rk".into(), true),
                ("up".into(), true),
                ("clientPin".into(), self.pin_hash.is_some()),
                ("pinUvAuthToken".into(), true),
            ],
            max_msg_size: Some(1200),
            pin_protocols: vec![pin::PROTOCOL],
        }
    }

    /// Check a pinUvAuthParam; `Ok(true)` when it verifies the user.
    fn check_pin_auth(&self, pa: &Option<cmd::PinAuth>, cdh: &[u8; 32], perm: u8, rp_id: &str) -> Result<bool, Error> {
        match pa {
            None if self.pin_hash.is_some() => Err(Error::Status(status::PUAT_REQUIRED)),
            None => Ok(false),
            Some(p) => {
                if p.protocol != pin::PROTOCOL {
                    return Err(Error::Status(status::INVALID_PARAMETER));
                }
                let t = self.token.as_ref().ok_or(Error::Status(status::PIN_AUTH_INVALID))?;
                if !pin::verify(&*t.value, cdh, &p.param) {
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
        if self.creds.iter().any(|c| c.rp_id == req.rp_id && req.exclude.contains(&c.id)) {
            // The spec asks for a touch first, so excluded-ness can't be probed.
            return Err(Error::Status(if self.touch { status::CREDENTIAL_EXCLUDED } else { status::OPERATION_DENIED }));
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
        self.creds.push(Cred { id, rp_id: req.rp_id, user_id: req.user.id, resident: req.resident_key, key });
        Ok((att.encode(), true))
    }

    fn get_assertion(&mut self, req: GetAssertion) -> Result<(Vec<u8>, bool), Error> {
        let uv = self.check_pin_auth(&req.pin_auth, &req.client_data_hash, permission::GET_ASSERTION, &req.rp_id)?;
        let matching: Vec<usize> = self
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.rp_id == req.rp_id && (if req.allow.is_empty() { c.resident } else { req.allow.contains(&c.id) }))
            .map(|(i, _)| i)
            .collect();
        let Some(&first) = matching.first() else {
            return Err(Error::Status(status::NO_CREDENTIALS));
        };
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
        if req.protocol != pin::PROTOCOL {
            return Err(Error::Status(status::INVALID_PARAMETER));
        }
        match req.sub {
            cmd::pin_sub::GET_PIN_RETRIES => Ok(ClientPinResponse { retries: Some(self.pin_retries), ..Default::default() }.encode()),
            cmd::pin_sub::GET_KEY_AGREEMENT => Ok(ClientPinResponse {
                key_agreement: Some(CoseKey::from_p256(alg::ECDH_ES_HKDF_256, &self.key_agreement.public_key())),
                ..Default::default()
            }
            .encode()),
            cmd::pin_sub::GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS => {
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
                self.token = Some(Token { value, permissions: perms, rp_id: req.rp_id });
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

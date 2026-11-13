//! CTAP 2.1 authenticator API (§6): the requests a platform builds and the
//! responses it parses. Every parser caps what it accepts.

use sigillum_cbor::{decode, encode, Value};

use crate::authdata::{AuthData, MAX_CRED_ID};
use crate::cose::{alg, CoseKey};
use crate::Error;

pub const MAKE_CREDENTIAL: u8 = 0x01;
pub const GET_ASSERTION: u8 = 0x02;
pub const GET_INFO: u8 = 0x04;
pub const CLIENT_PIN: u8 = 0x06;
pub const RESET: u8 = 0x07;
pub const GET_NEXT_ASSERTION: u8 = 0x08;
pub const SELECTION: u8 = 0x0b;

/// Caps on lists we send or accept.
pub const MAX_LIST: usize = 64;
pub const MAX_USER_ID: usize = 64;
pub const MAX_NAME: usize = 64;

fn text_list(v: Option<&Value>) -> Result<Vec<String>, Error> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let a = v.as_array().ok_or(Error::Protocol("expected an array"))?;
    if a.len() > MAX_LIST {
        return Err(Error::Protocol("list too long"));
    }
    a.iter().map(|e| e.as_text().map(|s| s.chars().take(64).collect()).ok_or(Error::Protocol("expected text"))).collect()
}

/// `authenticatorGetInfo` (§6.4), the fields we use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub versions: Vec<String>,
    pub extensions: Vec<String>,
    pub aaguid: [u8; 16],
    /// (option id, value), e.g. ("rk", true), ("clientPin", false).
    pub options: Vec<(String, bool)>,
    pub max_msg_size: Option<u64>,
    pub pin_protocols: Vec<u8>,
}

impl Info {
    pub fn parse(body: &[u8]) -> Result<Info, Error> {
        let v = decode(body)?;
        let mut info = Info { versions: text_list(v.get_int(1))?, extensions: text_list(v.get_int(2))?, ..Info::default() };
        if info.versions.is_empty() {
            return Err(Error::Protocol("GetInfo without versions"));
        }
        info.aaguid = v
            .get_int(3)
            .and_then(Value::as_bytes)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .ok_or(Error::Protocol("GetInfo aaguid"))?;
        if let Some(o) = v.get_int(4) {
            let m = o.as_map().ok_or(Error::Protocol("GetInfo options"))?;
            if m.len() > MAX_LIST {
                return Err(Error::Protocol("GetInfo options"));
            }
            for (k, val) in m {
                if let (Some(k), Some(b)) = (k.as_text(), val.as_bool()) {
                    info.options.push((k.chars().take(32).collect(), b));
                }
            }
        }
        info.max_msg_size = v.get_int(5).and_then(Value::as_u64);
        if let Some(p) = v.get_int(6) {
            let a = p.as_array().ok_or(Error::Protocol("GetInfo pin protocols"))?;
            for e in a.iter().take(8) {
                if let Some(n) = e.as_u64().filter(|n| *n <= 255) {
                    info.pin_protocols.push(n as u8);
                }
            }
        }
        Ok(info)
    }

    pub fn option(&self, id: &str) -> Option<bool> {
        self.options.iter().find(|(k, _)| k == id).map(|(_, v)| *v)
    }

    pub fn to_value(&self) -> Value {
        let texts = |l: &[String]| Value::Array(l.iter().map(|s| Value::text(s)).collect());
        let mut m = vec![
            (Value::Uint(1), texts(&self.versions)),
            (Value::Uint(3), Value::bytes(&self.aaguid)),
            (
                Value::Uint(4),
                Value::Map(self.options.iter().map(|(k, v)| (Value::text(k), Value::Bool(*v))).collect()),
            ),
            (Value::Uint(6), Value::Array(self.pin_protocols.iter().map(|p| Value::Uint(*p as u64)).collect())),
        ];
        if !self.extensions.is_empty() {
            m.push((Value::Uint(2), texts(&self.extensions)));
        }
        if let Some(n) = self.max_msg_size {
            m.push((Value::Uint(5), Value::Uint(n)));
        }
        Value::Map(m)
    }
}

fn descriptor(id: &[u8]) -> Value {
    Value::Map(vec![(Value::text("id"), Value::bytes(id)), (Value::text("type"), Value::text("public-key"))])
}

/// A `pinUvAuthParam` with its protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinAuth {
    pub param: Vec<u8>,
    pub protocol: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Vec<u8>,
    pub name: String,
    pub display_name: String,
}

/// `authenticatorMakeCredential` (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakeCredential {
    pub client_data_hash: [u8; 32],
    pub rp_id: String,
    pub rp_name: String,
    pub user: User,
    pub exclude: Vec<Vec<u8>>,
    pub resident_key: bool,
    pub cred_protect: Option<u8>,
    pub hmac_secret: bool,
    pub pin_auth: Option<PinAuth>,
}

impl MakeCredential {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.user.id.is_empty() || self.user.id.len() > MAX_USER_ID || self.exclude.len() > MAX_LIST {
            return Err(Error::Request("MakeCredential user id or exclude list"));
        }
        let mut m = vec![
            (Value::Uint(1), Value::bytes(&self.client_data_hash)),
            (
                Value::Uint(2),
                Value::Map(vec![(Value::text("id"), Value::text(&self.rp_id)), (Value::text("name"), Value::text(&self.rp_name))]),
            ),
            (
                Value::Uint(3),
                Value::Map(vec![
                    (Value::text("id"), Value::bytes(&self.user.id)),
                    (Value::text("name"), Value::text(&self.user.name)),
                    (Value::text("displayName"), Value::text(&self.user.display_name)),
                ]),
            ),
            (
                Value::Uint(4),
                Value::Array(vec![Value::Map(vec![
                    (Value::text("alg"), Value::int(alg::ES256)),
                    (Value::text("type"), Value::text("public-key")),
                ])]),
            ),
        ];
        if !self.exclude.is_empty() {
            m.push((Value::Uint(5), Value::Array(self.exclude.iter().map(|id| descriptor(id)).collect())));
        }
        let mut ext = Vec::new();
        if let Some(p) = self.cred_protect {
            ext.push((Value::text("credProtect"), Value::Uint(p as u64)));
        }
        if self.hmac_secret {
            ext.push((Value::text("hmac-secret"), Value::Bool(true)));
        }
        if !ext.is_empty() {
            m.push((Value::Uint(6), Value::Map(ext)));
        }
        if self.resident_key {
            m.push((Value::Uint(7), Value::Map(vec![(Value::text("rk"), Value::Bool(true))])));
        }
        if let Some(p) = &self.pin_auth {
            m.push((Value::Uint(8), Value::bytes(&p.param)));
            m.push((Value::Uint(9), Value::Uint(p.protocol as u64)));
        }
        Ok(encode(&Value::Map(m)))
    }

    pub fn parse(body: &[u8]) -> Result<MakeCredential, Error> {
        let v = decode(body)?;
        let bad = || Error::Protocol("MakeCredential request");
        let cdh = v.get_int(1).and_then(Value::as_bytes).and_then(|b| <[u8; 32]>::try_from(b).ok()).ok_or_else(bad)?;
        let rp = v.get_int(2).ok_or_else(bad)?;
        let user = v.get_int(3).ok_or_else(bad)?;
        let s = |m: &Value, k: &str| m.get_text(k).and_then(Value::as_text).unwrap_or("").to_owned();
        let algs = v.get_int(4).and_then(Value::as_array).ok_or_else(bad)?;
        if !algs.iter().any(|a| a.get_text("alg").and_then(Value::as_i64) == Some(alg::ES256)) {
            return Err(Error::Status(crate::status::UNSUPPORTED_ALGORITHM));
        }
        let exclude = match v.get_int(5) {
            Some(l) => l
                .as_array()
                .ok_or_else(bad)?
                .iter()
                .take(MAX_LIST)
                .filter_map(|d| d.get_text("id").and_then(Value::as_bytes).map(<[u8]>::to_vec))
                .collect(),
            None => Vec::new(),
        };
        let ext = v.get_int(6);
        let pin_auth = match (v.get_int(8).and_then(Value::as_bytes), v.get_int(9).and_then(Value::as_u64)) {
            (Some(p), Some(n)) if n <= 255 => Some(PinAuth { param: p.to_vec(), protocol: n as u8 }),
            (None, _) => None,
            _ => return Err(bad()),
        };
        Ok(MakeCredential {
            client_data_hash: cdh,
            rp_id: rp.get_text("id").and_then(Value::as_text).ok_or_else(bad)?.to_owned(),
            rp_name: s(rp, "name"),
            user: User {
                id: user.get_text("id").and_then(Value::as_bytes).ok_or_else(bad)?.to_vec(),
                name: s(user, "name"),
                display_name: s(user, "displayName"),
            },
            exclude,
            resident_key: v.get_int(7).and_then(|o| o.get_text("rk")).and_then(Value::as_bool).unwrap_or(false),
            cred_protect: ext.and_then(|e| e.get_text("credProtect")).and_then(Value::as_u64).map(|n| n as u8),
            hmac_secret: ext.and_then(|e| e.get_text("hmac-secret")).and_then(Value::as_bool).unwrap_or(false),
            pin_auth,
        })
    }
}

/// The `authenticatorMakeCredential` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    pub fmt: String,
    pub auth_data_raw: Vec<u8>,
    pub auth_data: AuthData,
    pub att_stmt: Value,
}

impl Attestation {
    pub fn parse(body: &[u8]) -> Result<Attestation, Error> {
        let v = decode(body)?;
        let fmt = v.get_int(1).and_then(Value::as_text).ok_or(Error::Protocol("attestation fmt"))?;
        if fmt.len() > 32 {
            return Err(Error::Protocol("attestation fmt"));
        }
        let raw = v.get_int(2).and_then(Value::as_bytes).ok_or(Error::Protocol("attestation authData"))?.to_vec();
        let auth_data = AuthData::parse(&raw)?;
        if auth_data.attested.is_none() {
            return Err(Error::Protocol("MakeCredential without attested credential data"));
        }
        let att_stmt = v.get_int(3).cloned().ok_or(Error::Protocol("attestation statement"))?;
        if att_stmt.as_map().is_none() {
            return Err(Error::Protocol("attestation statement"));
        }
        Ok(Attestation { fmt: fmt.to_owned(), auth_data_raw: raw, auth_data, att_stmt })
    }

    pub fn encode(&self) -> Vec<u8> {
        encode(&Value::Map(vec![
            (Value::Uint(1), Value::text(&self.fmt)),
            (Value::Uint(2), Value::bytes(&self.auth_data_raw)),
            (Value::Uint(3), self.att_stmt.clone()),
        ]))
    }

    /// The WebAuthn attestation object (text keys) for `fmt: "none"`, with
    /// the AAGUID zeroed (sigillum.md §9.3). Under `none` nothing signs the
    /// authenticator data at registration, so zeroing it is allowed.
    pub fn none_object(&self) -> (Vec<u8>, Vec<u8>) {
        let mut ad = self.auth_data.clone();
        if let Some(a) = ad.attested.as_mut() {
            a.aaguid = [0; 16];
        }
        let raw = ad.encode();
        let obj = encode(&Value::Map(vec![
            (Value::text("fmt"), Value::text("none")),
            (Value::text("attStmt"), Value::Map(Vec::new())),
            (Value::text("authData"), Value::bytes(&raw)),
        ]));
        (raw, obj)
    }
}

/// `authenticatorGetAssertion` (§6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetAssertion {
    pub rp_id: String,
    pub client_data_hash: [u8; 32],
    pub allow: Vec<Vec<u8>>,
    pub up: bool,
    pub pin_auth: Option<PinAuth>,
}

impl GetAssertion {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.allow.len() > MAX_LIST || self.allow.iter().any(|c| c.is_empty() || c.len() > MAX_CRED_ID) {
            return Err(Error::Request("GetAssertion allow list"));
        }
        let mut m = vec![(Value::Uint(1), Value::text(&self.rp_id)), (Value::Uint(2), Value::bytes(&self.client_data_hash))];
        if !self.allow.is_empty() {
            m.push((Value::Uint(3), Value::Array(self.allow.iter().map(|id| descriptor(id)).collect())));
        }
        if !self.up {
            m.push((Value::Uint(5), Value::Map(vec![(Value::text("up"), Value::Bool(false))])));
        }
        if let Some(p) = &self.pin_auth {
            m.push((Value::Uint(6), Value::bytes(&p.param)));
            m.push((Value::Uint(7), Value::Uint(p.protocol as u64)));
        }
        Ok(encode(&Value::Map(m)))
    }

    pub fn parse(body: &[u8]) -> Result<GetAssertion, Error> {
        let v = decode(body)?;
        let bad = || Error::Protocol("GetAssertion request");
        let allow = match v.get_int(3) {
            Some(l) => l
                .as_array()
                .ok_or_else(bad)?
                .iter()
                .take(MAX_LIST)
                .filter_map(|d| d.get_text("id").and_then(Value::as_bytes).map(<[u8]>::to_vec))
                .collect(),
            None => Vec::new(),
        };
        let pin_auth = match (v.get_int(6).and_then(Value::as_bytes), v.get_int(7).and_then(Value::as_u64)) {
            (Some(p), Some(n)) if n <= 255 => Some(PinAuth { param: p.to_vec(), protocol: n as u8 }),
            (None, _) => None,
            _ => return Err(bad()),
        };
        Ok(GetAssertion {
            rp_id: v.get_int(1).and_then(Value::as_text).ok_or_else(bad)?.to_owned(),
            client_data_hash: v.get_int(2).and_then(Value::as_bytes).and_then(|b| <[u8; 32]>::try_from(b).ok()).ok_or_else(bad)?,
            allow,
            up: v.get_int(5).and_then(|o| o.get_text("up")).and_then(Value::as_bool).unwrap_or(true),
            pin_auth,
        })
    }
}

/// The `authenticatorGetAssertion` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub cred_id: Vec<u8>,
    pub auth_data_raw: Vec<u8>,
    pub auth_data: AuthData,
    pub signature: Vec<u8>,
    pub user_id: Option<Vec<u8>>,
    pub count: Option<u64>,
}

impl Assertion {
    /// `allow` is the list we sent: with exactly one entry the authenticator
    /// may omit the credential (§6.2.2), and then it is that one.
    pub fn parse(body: &[u8], allow: &[Vec<u8>]) -> Result<Assertion, Error> {
        let v = decode(body)?;
        let cred_id = match v.get_int(1) {
            Some(c) => c.get_text("id").and_then(Value::as_bytes).ok_or(Error::Protocol("assertion credential"))?.to_vec(),
            None if allow.len() == 1 => allow[0].clone(),
            None => return Err(Error::Protocol("assertion without a credential")),
        };
        if cred_id.is_empty() || cred_id.len() > MAX_CRED_ID {
            return Err(Error::Protocol("credential id length"));
        }
        if !allow.is_empty() && !allow.contains(&cred_id) {
            return Err(Error::Protocol("assertion for a credential we did not allow"));
        }
        let raw = v.get_int(2).and_then(Value::as_bytes).ok_or(Error::Protocol("assertion authData"))?.to_vec();
        let auth_data = AuthData::parse(&raw)?;
        if auth_data.attested.is_some() {
            return Err(Error::Protocol("assertion with attested credential data"));
        }
        let signature = v.get_int(3).and_then(Value::as_bytes).ok_or(Error::Protocol("assertion signature"))?.to_vec();
        if signature.is_empty() || signature.len() > 512 {
            return Err(Error::Protocol("signature length"));
        }
        let user_id = match v.get_int(4) {
            Some(u) => {
                let id = u.get_text("id").and_then(Value::as_bytes).ok_or(Error::Protocol("assertion user"))?;
                if id.len() > MAX_USER_ID {
                    return Err(Error::Protocol("user handle length"));
                }
                Some(id.to_vec())
            }
            None => None,
        };
        Ok(Assertion { cred_id, auth_data_raw: raw, auth_data, signature, user_id, count: v.get_int(5).and_then(Value::as_u64) })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut m = vec![
            (Value::Uint(1), descriptor(&self.cred_id)),
            (Value::Uint(2), Value::bytes(&self.auth_data_raw)),
            (Value::Uint(3), Value::bytes(&self.signature)),
        ];
        if let Some(u) = &self.user_id {
            m.push((Value::Uint(4), Value::Map(vec![(Value::text("id"), Value::bytes(u))])));
        }
        if let Some(n) = self.count {
            m.push((Value::Uint(5), Value::Uint(n)));
        }
        encode(&Value::Map(m))
    }
}

/// `authenticatorClientPIN` (§6.5.5) subcommands we use.
pub mod pin_sub {
    pub const GET_PIN_RETRIES: u8 = 0x01;
    pub const GET_KEY_AGREEMENT: u8 = 0x02;
    pub const SET_PIN: u8 = 0x03;
    /// `getPinToken`: CTAP 2.0's, without permissions (protocol 1 keys).
    pub const GET_PIN_TOKEN: u8 = 0x05;
    pub const GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS: u8 = 0x09;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientPin {
    pub protocol: u8,
    pub sub: u8,
    pub key_agreement: Option<CoseKey>,
    pub pin_auth: Option<Vec<u8>>,
    pub new_pin_enc: Option<Vec<u8>>,
    pub pin_hash_enc: Option<Vec<u8>>,
    pub permissions: Option<u8>,
    pub rp_id: Option<String>,
}

impl ClientPin {
    pub fn encode(&self) -> Vec<u8> {
        let mut m = vec![(Value::Uint(1), Value::Uint(self.protocol as u64)), (Value::Uint(2), Value::Uint(self.sub as u64))];
        if let Some(k) = &self.key_agreement {
            m.push((Value::Uint(3), k.to_value()));
        }
        if let Some(p) = &self.pin_auth {
            m.push((Value::Uint(4), Value::bytes(p)));
        }
        if let Some(p) = &self.new_pin_enc {
            m.push((Value::Uint(5), Value::bytes(p)));
        }
        if let Some(p) = &self.pin_hash_enc {
            m.push((Value::Uint(6), Value::bytes(p)));
        }
        if let Some(p) = self.permissions {
            m.push((Value::Uint(9), Value::Uint(p as u64)));
        }
        if let Some(r) = &self.rp_id {
            m.push((Value::Uint(10), Value::text(r)));
        }
        encode(&Value::Map(m))
    }

    pub fn parse(body: &[u8]) -> Result<ClientPin, Error> {
        let v = decode(body)?;
        let bytes = |k| v.get_int(k).and_then(Value::as_bytes).map(<[u8]>::to_vec);
        Ok(ClientPin {
            protocol: v.get_int(1).and_then(Value::as_u64).filter(|n| *n <= 255).ok_or(Error::Protocol("ClientPIN protocol"))? as u8,
            sub: v.get_int(2).and_then(Value::as_u64).filter(|n| *n <= 255).ok_or(Error::Protocol("ClientPIN subcommand"))? as u8,
            key_agreement: v.get_int(3).map(CoseKey::parse).transpose()?,
            pin_auth: bytes(4),
            new_pin_enc: bytes(5),
            pin_hash_enc: bytes(6),
            permissions: v.get_int(9).and_then(Value::as_u64).map(|n| n as u8),
            rp_id: v.get_int(10).and_then(Value::as_text).map(str::to_owned),
        })
    }
}

/// The `authenticatorClientPIN` response fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientPinResponse {
    pub key_agreement: Option<CoseKey>,
    pub token_enc: Option<Vec<u8>>,
    pub retries: Option<u64>,
}

impl ClientPinResponse {
    pub fn parse(body: &[u8]) -> Result<ClientPinResponse, Error> {
        if body.is_empty() {
            return Ok(ClientPinResponse::default());
        }
        let v = decode(body)?;
        let key_agreement = v.get_int(1).map(CoseKey::parse).transpose()?;
        if let Some(k) = &key_agreement {
            if k.alg() != alg::ECDH_ES_HKDF_256 {
                return Err(Error::Protocol("key agreement alg"));
            }
        }
        let token_enc = v.get_int(2).and_then(Value::as_bytes).map(<[u8]>::to_vec);
        if token_enc.as_ref().is_some_and(|t| t.len() > 64) {
            return Err(Error::Protocol("token length"));
        }
        Ok(ClientPinResponse { key_agreement, token_enc, retries: v.get_int(3).and_then(Value::as_u64) })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut m = Vec::new();
        if let Some(k) = &self.key_agreement {
            m.push((Value::Uint(1), k.to_value()));
        }
        if let Some(t) = &self.token_enc {
            m.push((Value::Uint(2), Value::bytes(t)));
        }
        if let Some(r) = self.retries {
            m.push((Value::Uint(3), Value::Uint(r)));
        }
        encode(&Value::Map(m))
    }
}

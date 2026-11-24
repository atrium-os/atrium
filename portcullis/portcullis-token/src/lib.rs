//! Keys on hardware tokens (opifex.md §5.1: the catalog's root keys).
//!
//! A key file that names a token key instead of holding a PEM:
//!
//! ```text
//! # A key on a hardware token (opifex token ref). Signing asks for its PIN.
//! atrium-token-key = 1
//! module = "/opt/homebrew/lib/libykcs11.dylib"
//! serial = "31234567"
//! id = "01"
//! ```
//!
//! `module` is the token's PKCS#11 library (Yubico's `libykcs11` for a YubiKey's
//! PIV application; OpenSC or the vendor's for others), `serial` the token's
//! serial as PKCS#11 reports it, `id` the key's `CKA_ID` in hex (YubiKey PIV:
//! slot 9a is `01`, 9c `02`). The key never leaves the token: the digest goes
//! in, an ECDSA P-256 signature comes out, checked against the token's public
//! key before it is used.
//!
//! [`install`] makes every `--key`/`--sign` of `opifex` accept such a file
//! (through `portcullis_sig::set_external_signer`). The PIN is typed at the
//! terminal with echo off, once per token per run; never an argument, a file
//! or the environment.

use std::collections::HashMap;
use std::sync::Mutex;

pub mod agent;

use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::error::{Error as CkError, RvError};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, AttributeType, KeyType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::slot::Slot;
use cryptoki::types::AuthPin;
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use p256::pkcs8::{EncodePublicKey, LineEnding};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The DER of the P-256 curve's OID, as `CKA_EC_PARAMS` carries it.
const P256_PARAMS: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyRef {
    #[serde(rename = "atrium-token-key")]
    pub format: u32,
    pub module: String,
    pub serial: String,
    /// `CKA_ID`, hex.
    pub id: String,
}

impl KeyRef {
    /// `None` when the text isn't a token key reference at all.
    pub fn parse(text: &str) -> Option<Result<KeyRef, String>> {
        if !text.lines().any(|l| l.trim_start().starts_with("atrium-token-key")) {
            return None;
        }
        Some((|| {
            let r: KeyRef = toml::from_str(text).map_err(|e| format!("token key file: {e}"))?;
            if r.format != 1 {
                return Err(format!("token key file: format {} (this opifex reads 1)", r.format));
            }
            hex(&r.id)?;
            Ok(r)
        })())
    }

    pub fn text(&self) -> String {
        format!("# A key on a hardware token (opifex token ref). Signing asks for its PIN.\n\
                 atrium-token-key = 1\nmodule = {:?}\nserial = {:?}\nid = {:?}\n",
                self.module, self.serial, self.id)
    }

    fn what(&self) -> String { format!("token {} key {}", self.serial, self.id) }
}

fn hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("token key id {s:?}: hex bytes, e.g. \"01\""));
    }
    Ok((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect())
}

fn to_hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn ck(what: &str) -> impl Fn(CkError) -> String + '_ { move |e| format!("{what}: {e}") }

fn open(module: &str) -> Result<Pkcs11, String> {
    let p = Pkcs11::new(module).map_err(ck(module))?;
    p.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK)).map_err(ck(module))?;
    Ok(p)
}

fn slot_of(p: &Pkcs11, r: &KeyRef) -> Result<Slot, String> {
    let mut seen = Vec::new();
    for s in p.get_slots_with_initialized_token().map_err(ck(&r.module))? {
        let serial = p.get_token_info(s).map_err(ck(&r.module))?.serial_number().trim().to_string();
        if serial == r.serial.trim() {
            return Ok(s);
        }
        seen.push(serial);
    }
    Err(if seen.is_empty() { format!("{}: no token plugged in", r.what()) }
        else { format!("{}: not plugged in (tokens present: {})", r.what(), seen.join(", ")) })
}

fn one(s: &Session, class: ObjectClass, id: &[u8], what: &str) -> Result<Option<ObjectHandle>, String> {
    let found = s.find_objects(&[Attribute::Class(class), Attribute::KeyType(KeyType::EC), Attribute::Id(id.to_vec())])
        .map_err(ck(what))?;
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found[0])),
        n => Err(format!("{what}: {n} keys with that id")),
    }
}

/// The P-256 public key of the token key with `id`, from its public key object.
fn public_key(s: &Session, id: &[u8], what: &str) -> Result<VerifyingKey, String> {
    let h = one(s, ObjectClass::PUBLIC_KEY, id, what)?.ok_or_else(|| format!("{what}: no public key object (on a YubiKey, the slot needs a certificate: ykman piv certificates generate)"))?;
    let attrs = s.get_attributes(h, &[AttributeType::EcParams, AttributeType::EcPoint]).map_err(ck(what))?;
    let (mut params, mut point) = (None, None);
    for a in attrs {
        match a {
            Attribute::EcParams(v) => params = Some(v),
            Attribute::EcPoint(v) => point = Some(v),
            _ => {}
        }
    }
    if params.as_deref() != Some(&P256_PARAMS[..]) {
        return Err(format!("{what}: not a P-256 key"));
    }
    let point = point.ok_or_else(|| format!("{what}: no EC point"))?;
    // CKA_EC_POINT is a DER OCTET STRING around the SEC1 point; some modules give it bare.
    let sec1 = if point.len() == 67 && point[0] == 0x04 && point[1] == 65 { &point[2..] } else { &point[..] };
    VerifyingKey::from_sec1_bytes(sec1).map_err(|_| format!("{what}: bad EC point"))
}

/// The public SPKI PEM of a token key. Needs no PIN.
pub fn public_pem(r: &KeyRef) -> Result<String, String> {
    let p = open(&r.module)?;
    let s = p.open_ro_session(slot_of(&p, r)?).map_err(ck(&r.module))?;
    let vk = public_key(&s, &hex(&r.id)?, &r.what())?;
    vk.to_public_key_pem(LineEnding::LF).map_err(|e| format!("{}: {e}", r.what()))
}

/// Sign `msg` (SHA-256 prehash, DER ECDSA: what `portcullis_sig::verify_one`
/// checks) with the token key, logging in with `pin`.
pub fn sign_with_pin(r: &KeyRef, msg: &[u8], pin: &str) -> Result<Vec<u8>, String> {
    let what = r.what();
    let id = hex(&r.id)?;
    let p = open(&r.module)?;
    let s = p.open_ro_session(slot_of(&p, r)?).map_err(ck(&r.module))?;
    let vk = public_key(&s, &id, &what)?;
    let pin = AuthPin::from(pin.to_string());
    match s.login(UserType::User, Some(&pin)) {
        Ok(()) | Err(CkError::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => {}
        Err(CkError::Pkcs11(RvError::PinIncorrect, _)) => return Err(format!("{what}: wrong PIN")),
        Err(CkError::Pkcs11(RvError::PinLocked, _)) => return Err(format!("{what}: PIN locked")),
        Err(e) => return Err(format!("{what}: login: {e}")),
    }
    let key = one(&s, ObjectClass::PRIVATE_KEY, &id, &what)?.ok_or_else(|| format!("{what}: no private key with that id"))?;
    let always = s.get_attributes(key, &[AttributeType::AlwaysAuthenticate]).ok()
        .and_then(|v| v.into_iter().find_map(|a| match a { Attribute::AlwaysAuthenticate(b) => Some(b), _ => None }))
        .unwrap_or(false);
    let digest = Sha256::digest(msg);
    let raw = if always {
        // A key that wants the PIN for every signature (YubiKey slot 9c).
        s.sign_init(&Mechanism::Ecdsa, key).map_err(ck(&what))?;
        s.login(UserType::ContextSpecific, Some(&pin)).map_err(ck(&what))?;
        s.sign_update(&digest).and_then(|_| s.sign_final())
            .map_err(|e| format!("{what}: {e} (this key wants its PIN per signature; a key in YubiKey slot 9a doesn't)"))?
    } else {
        s.sign(&Mechanism::Ecdsa, key, &digest).map_err(ck(&what))?
    };
    let sig = Signature::from_slice(&raw).map_err(|_| format!("{what}: the token's signature isn't P-256 r||s"))?;
    let sig = sig.normalize_s().unwrap_or(sig);
    vk.verify_prehash(&digest, &sig).map_err(|_| format!("{what}: the token's signature doesn't verify"))?;
    let _ = s.logout();
    Ok(sig.to_der().as_bytes().to_vec())
}

/// Read a PIN at the terminal with echo off.
fn prompt_pin(what: &str) -> Result<String, String> {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")
        .map_err(|e| format!("{what}: the PIN is typed at a terminal (/dev/tty: {e})"))?;
    let stty = |arg: &str| Command::new("stty").arg(arg).stdin(tty.try_clone().map(Stdio::from).unwrap_or(Stdio::null())).status();
    let mut out = tty.try_clone().map_err(|e| e.to_string())?;
    write!(out, "PIN for {what}: ").and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    let _ = stty("-echo");
    let mut line = String::new();
    let r = std::io::BufReader::new(tty.try_clone().map_err(|e| e.to_string())?).read_line(&mut line);
    let _ = stty("echo");
    let _ = writeln!(out);
    r.map_err(|e| e.to_string())?;
    let pin = line.trim_end_matches(['\r', '\n']).to_string();
    if pin.is_empty() {
        return Err(format!("{what}: no PIN typed"));
    }
    Ok(pin)
}

type PinSource = fn(&str) -> Result<String, String>;
static PIN_SOURCE: Mutex<Option<PinSource>> = Mutex::new(None);
static PINS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn sign_ref(msg: &[u8], text: &str) -> Option<Result<Vec<u8>, String>> {
    if let Some(r) = agent::sign_ref(msg, text) {
        return Some(r);
    }
    let r = match KeyRef::parse(text)? {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    Some((|| {
        let token = format!("token {}", r.serial);
        let cached = PINS.lock().unwrap().as_ref().and_then(|m| m.get(&r.serial).cloned());
        let pin = match cached {
            Some(p) => p,
            None => (PIN_SOURCE.lock().unwrap().unwrap_or(prompt_pin))(&token)?,
        };
        eprintln!("opifex: signing with {} (touch it if it blinks)", r.what());
        let sig = sign_with_pin(&r, msg, &pin)?;
        PINS.lock().unwrap().get_or_insert_with(HashMap::new).insert(r.serial.clone(), pin);
        Ok(sig)
    })())
}

fn public_ref(text: &str) -> Option<Result<String, String>> {
    if let Some(r) = agent::public_ref(text) {
        return Some(r);
    }
    Some(KeyRef::parse(text)?.and_then(|r| public_pem(&r)))
}

/// Make `portcullis_sig::sign`/`public_pem` take token key files and agent
/// key files ([`agent`]: a key in Arca, through the SSH agent).
pub fn install() {
    portcullis_sig::set_external_signer(portcullis_sig::ExternalSigner { sign: sign_ref, public_pem: public_ref });
}

/// [`install`], with PINs from `src` instead of the terminal (tests with a
/// software token).
pub fn install_with_pins(src: PinSource) {
    *PIN_SOURCE.lock().unwrap() = Some(src);
    install();
}

/// A token and the P-256 keys on it, for `opifex token list`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub serial: String,
    pub label: String,
    pub model: String,
    /// (id hex, label, public PEM)
    pub keys: Vec<(String, String, String)>,
}

/// Every token the module sees and its P-256 public keys. Needs no PIN.
pub fn list(module: &str) -> Result<Vec<Listed>, String> {
    let p = open(module)?;
    let mut out = Vec::new();
    for slot in p.get_slots_with_initialized_token().map_err(ck(module))? {
        let ti = p.get_token_info(slot).map_err(ck(module))?;
        let mut t = Listed { serial: ti.serial_number().trim().into(), label: ti.label().trim().into(),
                             model: ti.model().trim().into(), keys: Vec::new() };
        let s = p.open_ro_session(slot).map_err(ck(module))?;
        let objs = s.find_objects(&[Attribute::Class(ObjectClass::PUBLIC_KEY), Attribute::KeyType(KeyType::EC)])
            .map_err(ck(module))?;
        for h in objs {
            let attrs = s.get_attributes(h, &[AttributeType::Id, AttributeType::Label]).map_err(ck(module))?;
            let mut id = Vec::new();
            let mut label = String::new();
            for a in attrs {
                match a {
                    Attribute::Id(v) => id = v,
                    Attribute::Label(v) => label = String::from_utf8_lossy(&v).trim().to_string(),
                    _ => {}
                }
            }
            if let Ok(vk) = public_key(&s, &id, module) {
                if let Ok(pem) = vk.to_public_key_pem(LineEnding::LF) {
                    t.keys.push((to_hex(&id), label, pem));
                }
            }
        }
        out.push(t);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

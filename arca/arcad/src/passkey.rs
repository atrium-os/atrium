//! Arca as the platform authenticator (arca.md §8.4, sigillum.md §9.2):
//! the `P_*` ops, served only to `sigillumd`.
//!
//! A passkey is a [`Kind::P256`] key item: recipient = the RP ID, the
//! user's main persona, class while-unlocked, account = the user name,
//! scope = `bytes user_handle | str display_name`, value = the private
//! scalar. The scalar never leaves this module except as a signature.
//!
//! The authenticator data is built here, from the item's own recipient: the
//! RP ID hash can't name another RP than the one the key is bound to.
//! Flags: UP always (sigillumd asks only after the user's press on the
//! sheet), UV only when sigillumd says its `VERIFY_USER` passed, BE (Arca
//! passkeys are meant to sync through Vestibulum), BS clear until sync
//! exists. The signature counter is 0, as for synced passkeys, and the
//! AAGUID is zero.

use arca_store::codec::{Reader, Writer};
use arca_store::model::{caps, valid_user};
use arca_store::secret::LockedBuf;
use arca_store::{Access, Class, Error, ItemId, Kind, NewItem, Provenance, Recipient, Store};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};

/// The `ARCA_SERVICES` name the `P_*` ops are served to.
pub const SIGILLUM: &str = "sigillumd";

/// WebAuthn caps the user handle at 64 bytes.
pub const MAX_USER_HANDLE: usize = 64;
/// Display strings, as authenticators store them.
pub const MAX_NAME: usize = 64;
/// Passkeys listed for one RP.
pub const MAX_LIST: usize = 64;

mod flag {
    pub const UP: u8 = 0x01;
    pub const UV: u8 = 0x04;
    pub const BE: u8 = 0x08;
    pub const AT: u8 = 0x40;
}

/// A passkey as the sheet sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passkey {
    pub id: ItemId,
    pub user_handle: Vec<u8>,
    pub name: String,
    pub display_name: String,
}

fn scope(user_handle: &[u8], display_name: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(user_handle);
    w.str(display_name);
    w.buf
}

fn unscope(s: &[u8]) -> Result<(Vec<u8>, String), Error> {
    let mut r = Reader::new(s);
    let h = r.bytes(MAX_USER_HANDLE).map_err(|_| Error::Corrupt("passkey scope"))?.to_vec();
    let d = r.str(MAX_NAME).map_err(|_| Error::Corrupt("passkey scope"))?.to_string();
    r.end().map_err(|_| Error::Corrupt("passkey scope"))?;
    Ok((h, d))
}

/// A fresh P-256 private scalar, in locked memory.
fn new_scalar() -> Result<(LockedBuf, SigningKey), Error> {
    loop {
        let s = LockedBuf::random(32)?;
        // Out of range (zero or ≥ n) has probability ~2⁻³²: draw again.
        if let Ok(k) = SigningKey::from_slice(s.as_ref()) {
            return Ok((s, k));
        }
    }
}

/// The credential public key, as CTAP2's canonical COSE map.
fn cose(k: &SigningKey) -> Vec<u8> {
    let p = k.verifying_key().to_encoded_point(false);
    let mut v = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
    v.extend_from_slice(p.x().expect("uncompressed"));
    v.extend_from_slice(&[0x22, 0x58, 0x20]);
    v.extend_from_slice(p.y().expect("uncompressed"));
    v
}

fn auth_data(rp_id: &str, flags: u8, attested: Option<(&ItemId, &SigningKey)>) -> Vec<u8> {
    let mut v = Sha256::digest(rp_id.as_bytes()).to_vec();
    v.push(flags);
    v.extend_from_slice(&0u32.to_be_bytes());
    if let Some((id, k)) = attested {
        v.extend_from_slice(&[0; 16]);
        v.extend_from_slice(&(id.0.len() as u16).to_be_bytes());
        v.extend_from_slice(&id.0);
        v.extend_from_slice(&cose(k));
    }
    v
}

fn base_flags(uv: bool) -> u8 {
    flag::UP | flag::BE | if uv { flag::UV } else { 0 }
}

/// The user's passkeys for `rp_id`.
pub fn list(store: &Store, persona: &str, rp_id: &str) -> Result<Vec<Passkey>, Error> {
    let rp = Recipient::rp_id(rp_id)?;
    let mut out = Vec::new();
    for m in store.list(&Access { persona, recipient: &rp })? {
        if m.kind != Kind::P256 {
            continue;
        }
        let (user_handle, display_name) = unscope(&m.scope)?;
        out.push(Passkey { id: m.id, user_handle, name: m.account, display_name });
    }
    out.truncate(MAX_LIST);
    Ok(out)
}

/// Make a passkey; returns its id and the registration's authenticator data.
pub fn create(store: &mut Store, persona: &str, rp_id: &str, user_handle: &[u8], name: &str, display_name: &str, uv: bool) -> Result<(ItemId, Vec<u8>), Error> {
    if user_handle.is_empty() || user_handle.len() > MAX_USER_HANDLE || name.len() > MAX_NAME || display_name.len() > MAX_NAME {
        return Err(Error::BadInput("passkey field"));
    }
    let rp = Recipient::rp_id(rp_id)?;
    let older: Vec<ItemId> = list(store, persona, rp_id)?.into_iter().filter(|p| p.user_handle == user_handle).map(|p| p.id).collect();
    let (scalar, key) = new_scalar()?;
    let m = store.add(NewItem {
        persona,
        recipient: rp.clone(),
        class: Class::WhileUnlocked,
        kind: Kind::P256,
        provenance: Provenance::ArcaGenerated,
        account: name,
        scope: &scope(user_handle, display_name),
        value: scalar.as_ref(),
    })?;
    drop(scalar);
    // Only after the new one is stored: a failure leaves the old one usable.
    for id in older {
        store.delete(&id, &Access { persona, recipient: &rp })?;
    }
    Ok((m.id, auth_data(rp_id, base_flags(uv) | flag::AT, Some((&m.id, &key)))))
}

/// Sign an assertion: `(auth_data, DER signature, user_handle)`.
pub fn assert(store: &Store, persona: &str, rp_id: &str, id: &ItemId, uv: bool, client_data_hash: &[u8; 32]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), Error> {
    let rp = Recipient::rp_id(rp_id)?;
    let (m, scalar) = store.release(id, &Access { persona, recipient: &rp }, None)?;
    if m.kind != Kind::P256 {
        return Err(Error::Denied);
    }
    let key = SigningKey::from_slice(scalar.as_ref()).map_err(|_| Error::Corrupt("passkey scalar"))?;
    drop(scalar);
    let (user_handle, _) = unscope(&m.scope)?;
    let ad = auth_data(rp_id, base_flags(uv), None);
    let mut signed = ad.clone();
    signed.extend_from_slice(client_data_hash);
    let sig: Signature = key.sign(&signed);
    Ok((ad, sig.to_der().as_bytes().to_vec(), user_handle))
}

/// The `P_*` wire for one request.
pub fn handle(stores: &mut std::collections::HashMap<String, Store>, persona: &str, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Result<Vec<u8>, Error> {
    use crate::proto::sigillum_op::*;
    let mut r = Reader::new(body);
    let bad = |_| Error::BadInput("passkey request");
    let user = r.str(caps::USER).map_err(bad)?;
    if !valid_user(user) {
        return Err(Error::BadInput("user"));
    }
    let rp_id = r.str(caps::RP_ID).map_err(bad)?;
    let store = stores.get_mut(user).ok_or(Error::Locked)?;
    let mut w = Writer::new();
    match op {
        P_CREATE => {
            let handle = r.bytes(MAX_USER_HANDLE).map_err(bad)?;
            let name = r.str(MAX_NAME).map_err(bad)?;
            let display = r.str(MAX_NAME).map_err(bad)?;
            let uv = r.u8().map_err(bad)? == 1;
            r.end().map_err(bad)?;
            let (id, ad) = create(store, persona, rp_id, handle, name, display, uv)?;
            *item = Some(id);
            w.raw(&id.0);
            w.bytes(&ad);
        }
        P_ASSERT => {
            let id = ItemId(r.array::<16>().map_err(bad)?);
            let uv = r.u8().map_err(bad)? == 1;
            let cdh = r.array::<32>().map_err(bad)?;
            r.end().map_err(bad)?;
            *item = Some(id);
            let (ad, sig, handle) = assert(store, persona, rp_id, &id, uv, &cdh)?;
            w.bytes(&ad);
            w.bytes(&sig);
            w.bytes(&handle);
        }
        P_LIST => {
            r.end().map_err(bad)?;
            let all = list(store, persona, rp_id)?;
            w.u32(all.len() as u32);
            for p in all {
                w.raw(&p.id.0);
                w.bytes(&p.user_handle);
                w.str(&p.name);
                w.str(&p.display_name);
            }
        }
        _ => return Err(Error::Denied),
    }
    Ok(w.buf)
}

//! The store: key hierarchy, recipient groups, classes, lock (arca.md §6).
//!
//! ```text
//! root key (from Vestibulum)
//!  └─ master key                      header, wrapped by root
//!       ├─ class key AfterFirstUnlock header, wrapped by master; kept while locked
//!       ├─ class key WhileUnlocked    header, wrapped by master; evicted at lock
//!       └─ class key VerifyEachUse    header, wrapped by master; unwrapped per use
//!            └─ data key, one per (persona, recipient, class): a group file
//!                 └─ items: meta and value sealed separately
//! ```
//!
//! # Files
//!
//! `<dir>/header` and `<dir>/groups/<16 random bytes, hex>`. File names
//! say nothing about recipients: which sites a user has accounts on is
//! itself private. Every write is atomic (temp file, fsync, rename, fsync
//! the directory).
//!
//! # Binding
//!
//! Every AEAD's associated data names exactly what the sealed bytes belong
//! to. An item's AAD carries user, persona, class, recipient, group, item
//! id, version and which part (meta or value) it is. A record copied into
//! another recipient's group, or replayed under another id or version,
//! fails to decrypt, and is also under the wrong data key.
//!
//! # What this module does not do
//!
//! It does not decide who is asking. Callers pass the recipient the system
//! verified (arca.md §3), and the store releases an item only when that
//! recipient equals the item's. Rollback detection needs a TPM monotonic
//! counter (A4). "Forget" deletes the recipient's wrapped data key from the
//! live store; against someone holding both the passcode and raw old disk
//! blocks it is only as final as the medium, until the class keys are
//! sealed under a TPM-held key that is rotated on forget (A4).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};

use crate::codec::{Reader, Writer};
use crate::model::*;
use crate::root::RootKey;
use crate::secret::LockedBuf;
use crate::Error;

const HEADER_MAGIC: &[u8; 8] = b"ARCAHDR1";
const GROUP_MAGIC: &[u8; 8] = b"ARCAGRP1";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// Cap on any one sealed blob read from disk.
const SEALED_CAP: usize = NONCE_LEN + TAG_LEN + caps::VALUE + caps::ACCOUNT + caps::SCOPE + 64;
/// Cap on one group file: its own header plus its items.
const GROUP_FILE_CAP: usize = 32 * 1024 * 1024;
/// Cap on a sealed group header: persona, recipient, data key.
const GROUP_HDR_CAP: usize = NONCE_LEN + TAG_LEN + 1024;

// ── AEAD ─────────────────────────────────────────────────────────────────────

/// nonce | ciphertext | tag. The plaintext is never copied to unlocked memory.
fn seal(key: &LockedBuf, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, Error> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| Error::Corrupt("key length"))?;
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(std::io::Error::other)?;
    let mut buf = LockedBuf::from_slice(plain)?;
    let tag = cipher
        .encrypt_in_place_detached(XNonce::from_slice(&nonce), aad, buf.as_mut())
        .map_err(|_| Error::Corrupt("seal"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + plain.len() + TAG_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(buf.as_ref());
    out.extend_from_slice(&tag);
    Ok(out)
}

/// Open into locked memory. Any failure is `Error::Unsealed`: wrong key,
/// wrong binding and tampering look the same.
fn open(key: &LockedBuf, aad: &[u8], sealed: &[u8]) -> Result<LockedBuf, Error> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(Error::Unsealed);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| Error::Corrupt("key length"))?;
    let (nonce, rest) = sealed.split_at(NONCE_LEN);
    let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
    let mut buf = LockedBuf::from_slice(ct)?;
    cipher
        .decrypt_in_place_detached(XNonce::from_slice(nonce), aad, buf.as_mut(), Tag::from_slice(tag))
        .map_err(|_| Error::Unsealed)?;
    Ok(buf)
}

fn aad_master(user: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.str("arca/v1/master");
    w.str(user);
    w.buf
}

fn aad_class(user: &str, class: Class) -> Vec<u8> {
    let mut w = Writer::new();
    w.str("arca/v1/class");
    w.str(user);
    w.u8(class as u8);
    w.buf
}

fn aad_group(user: &str, gid: &[u8; 16], class: Class) -> Vec<u8> {
    let mut w = Writer::new();
    w.str("arca/v1/group");
    w.str(user);
    w.raw(gid);
    w.u8(class as u8);
    w.buf
}

#[derive(Clone, Copy)]
enum Part {
    Meta = 1,
    Value = 2,
}

fn aad_item(user: &str, g: &Group, gid: &[u8; 16], id: &ItemId, version: u64, part: Part) -> Vec<u8> {
    let mut w = Writer::new();
    w.str("arca/v1/item");
    w.str(user);
    w.str(&g.persona);
    w.u8(g.class as u8);
    g.recipient.encode(&mut w);
    w.raw(gid);
    w.raw(&id.0);
    w.u64(version);
    w.u8(part as u8);
    w.buf
}

// ── files ────────────────────────────────────────────────────────────────────

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let dir = path.parent().ok_or(Error::BadInput("path"))?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn read_capped(path: &Path, cap: usize) -> Result<Vec<u8>, Error> {
    let len = fs::metadata(path)?.len();
    if len > cap as u64 {
        return Err(Error::Corrupt("file over its cap"));
    }
    Ok(fs::read(path)?)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── in-memory index ──────────────────────────────────────────────────────────

struct ItemRec {
    id: ItemId,
    version: u64,
    meta: Vec<u8>,
    value: Vec<u8>,
}

struct Group {
    class: Class,
    persona: String,
    recipient: Recipient,
    /// The sealed group header (persona, recipient, data key) as on disk.
    sealed_hdr: Vec<u8>,
    items: Vec<ItemRec>,
}

/// Proof that Vestibulum verified the user for one verify-each-use access.
/// arcad constructs one only from Vestibulum's answer on its Vestibulum
/// link, and the store consumes it: one check, one use.
pub struct UserVerified {
    _private: (),
}

impl UserVerified {
    /// For arcad: call only after Vestibulum's verification reply for this
    /// request arrived on the Vestibulum channel.
    pub fn vouched_by_vestibulum() -> UserVerified {
        UserVerified { _private: () }
    }
}

/// Who is asking, as the system verified it (arca.md §3, §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access<'a> {
    pub persona: &'a str,
    pub recipient: &'a Recipient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// Opened after boot, never unlocked: nothing is readable.
    Sealed,
    /// After-first-unlock items only.
    Locked,
    Unlocked,
}

pub struct Store {
    dir: PathBuf,
    user: String,
    /// The sealed master key and class keys as in the header file.
    hdr_master: Vec<u8>,
    hdr_class: Vec<(Class, Vec<u8>)>,
    master: Option<LockedBuf>,
    afu: Option<LockedBuf>,
    wu: Option<LockedBuf>,
    groups: HashMap<[u8; 16], Group>,
    by_item: HashMap<ItemId, [u8; 16]>,
}

impl Store {
    // ── lifecycle ────────────────────────────────────────────────────────

    /// Make a new store for `user` under `root`. Refuses if one exists.
    pub fn create(dir: &Path, user: &str, root: &RootKey) -> Result<Store, Error> {
        if !valid_user(user) {
            return Err(Error::BadInput("user"));
        }
        if dir.join("header").exists() {
            return Err(Error::Exists);
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.join("groups"))?;
        let master = LockedBuf::random(KEY_LEN)?;
        let hdr_master = seal(&root.0, &aad_master(user), master.as_ref())?;
        let mut hdr_class = Vec::new();
        for c in Class::ALL {
            let k = LockedBuf::random(KEY_LEN)?;
            hdr_class.push((c, seal(&master, &aad_class(user, c), k.as_ref())?));
        }
        let mut s = Store {
            dir: dir.into(),
            user: user.into(),
            hdr_master,
            hdr_class,
            master: None,
            afu: None,
            wu: None,
            groups: HashMap::new(),
            by_item: HashMap::new(),
        };
        s.write_header()?;
        s.unlock(root)?;
        Ok(s)
    }

    /// Open an existing store, sealed. `user` must match the header.
    pub fn open(dir: &Path, user: &str) -> Result<Store, Error> {
        let bytes = read_capped(&dir.join("header"), 4096)?;
        let mut r = Reader::new(&bytes);
        if r.raw(8)? != HEADER_MAGIC {
            return Err(Error::Corrupt("header magic"));
        }
        let hdr_user = r.str(caps::USER)?;
        if hdr_user != user {
            return Err(Error::Corrupt("header is for another user"));
        }
        let hdr_master = r.bytes(NONCE_LEN + KEY_LEN + TAG_LEN)?.to_vec();
        let n = r.u8()?;
        if n as usize != Class::ALL.len() {
            return Err(Error::Corrupt("class count"));
        }
        let mut hdr_class = Vec::new();
        for want in Class::ALL {
            let c = Class::from_u8(r.u8()?)?;
            if c != want {
                return Err(Error::Corrupt("class order"));
            }
            hdr_class.push((c, r.bytes(NONCE_LEN + KEY_LEN + TAG_LEN)?.to_vec()));
        }
        r.end()?;
        Ok(Store {
            dir: dir.into(),
            user: user.into(),
            hdr_master,
            hdr_class,
            master: None,
            afu: None,
            wu: None,
            groups: HashMap::new(),
            by_item: HashMap::new(),
        })
    }

    fn write_header(&self) -> Result<(), Error> {
        let mut w = Writer::new();
        w.raw(HEADER_MAGIC);
        w.str(&self.user);
        w.bytes(&self.hdr_master);
        w.u8(self.hdr_class.len() as u8);
        for (c, s) in &self.hdr_class {
            w.u8(*c as u8);
            w.bytes(s);
        }
        write_atomic(&self.dir.join("header"), &w.buf)
    }

    fn sealed_class(&self, c: Class) -> &[u8] {
        &self.hdr_class.iter().find(|(k, _)| *k == c).expect("all classes present").1
    }

    /// Unlock with the root key. A wrong key is `WrongRootKey` and changes
    /// nothing.
    pub fn unlock(&mut self, root: &RootKey) -> Result<(), Error> {
        let master = open(&root.0, &aad_master(&self.user), &self.hdr_master).map_err(|e| match e {
            Error::Unsealed => Error::WrongRootKey,
            e => e,
        })?;
        let afu = open(&master, &aad_class(&self.user, Class::AfterFirstUnlock), self.sealed_class(Class::AfterFirstUnlock))?;
        let wu = open(&master, &aad_class(&self.user, Class::WhileUnlocked), self.sealed_class(Class::WhileUnlocked))?;
        self.master = Some(master);
        self.afu = Some(afu);
        self.wu = Some(wu);
        self.load_groups()
    }

    /// Lock: evict the master and while-unlocked keys, and forget the
    /// recipients of every group that isn't after-first-unlock, so not even
    /// the list of accounts stays in memory while locked.
    pub fn lock(&mut self) {
        self.master = None;
        self.wu = None;
        self.groups.retain(|_, g| g.class == Class::AfterFirstUnlock);
        let groups = &self.groups;
        self.by_item.retain(|_, gid| groups.contains_key(gid));
    }

    pub fn state(&self) -> LockState {
        match (&self.master, &self.afu) {
            (Some(_), _) => LockState::Unlocked,
            (None, Some(_)) => LockState::Locked,
            (None, None) => LockState::Sealed,
        }
    }

    /// Change the root key (the user changed their passcode). Needs the old
    /// root as proof, not only an unlocked store.
    pub fn rewrap_root(&mut self, old: &RootKey, new: &RootKey) -> Result<(), Error> {
        let master = open(&old.0, &aad_master(&self.user), &self.hdr_master).map_err(|_| Error::WrongRootKey)?;
        self.hdr_master = seal(&new.0, &aad_master(&self.user), master.as_ref())?;
        self.write_header()
    }

    // ── keys ─────────────────────────────────────────────────────────────

    /// The class key for `c`, borrowed or unwrapped for this use.
    fn class_key(&self, c: Class, verified: Option<UserVerified>) -> Result<ClassKey<'_>, Error> {
        match c {
            Class::AfterFirstUnlock => self.afu.as_ref().map(ClassKey::Resident).ok_or(Error::Locked),
            Class::WhileUnlocked => self.wu.as_ref().map(ClassKey::Resident).ok_or(Error::Locked),
            Class::VerifyEachUse => {
                let m = self.master.as_ref().ok_or(Error::Locked)?;
                verified.ok_or(Error::NeedsVerification)?;
                Ok(ClassKey::Transient(open(m, &aad_class(&self.user, c), self.sealed_class(c))?))
            }
        }
    }

    /// Metadata for listing needs the class key but never user verification:
    /// the trusted UI may list verify-each-use items while unlocked.
    fn class_key_for_meta(&self, c: Class) -> Result<ClassKey<'_>, Error> {
        match c {
            Class::VerifyEachUse => {
                let m = self.master.as_ref().ok_or(Error::Locked)?;
                Ok(ClassKey::Transient(open(m, &aad_class(&self.user, c), self.sealed_class(c))?))
            }
            _ => self.class_key(c, None),
        }
    }

    fn data_key(&self, gid: &[u8; 16], g: &Group, ck: &LockedBuf) -> Result<LockedBuf, Error> {
        let plain = open(ck, &aad_group(&self.user, gid, g.class), &g.sealed_hdr)?;
        let mut r = Reader::new(plain.as_ref());
        let _persona = r.str(caps::PERSONA)?;
        let _recipient = Recipient::decode(&mut r)?;
        let dk = LockedBuf::from_slice(r.raw(KEY_LEN)?)?;
        r.end()?;
        Ok(dk)
    }

    // ── groups on disk ───────────────────────────────────────────────────

    fn load_groups(&mut self) -> Result<(), Error> {
        self.groups.clear();
        self.by_item.clear();
        for entry in fs::read_dir(self.dir.join("groups"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.len() != 32 || !name.bytes().all(|c| c.is_ascii_hexdigit()) {
                continue; // temp files and anything else that isn't ours
            }
            let bytes = read_capped(&entry.path(), GROUP_FILE_CAP)?;
            let (gid, g) = self.decode_group(&bytes)?;
            if hex16(&gid) != name {
                return Err(Error::Corrupt("group file name"));
            }
            let Some(g) = g else { continue }; // a class not unlocked
            if self.groups.values().any(|o| o.persona == g.persona && o.recipient == g.recipient && o.class == g.class) {
                return Err(Error::Corrupt("duplicate group"));
            }
            for it in &g.items {
                if self.by_item.insert(it.id, gid).is_some() {
                    return Err(Error::Corrupt("duplicate item id"));
                }
            }
            self.groups.insert(gid, g);
        }
        Ok(())
    }

    /// Decode a group file. `Ok((gid, None))` if its class key isn't
    /// available (for a verify-each-use group, the master key is enough).
    fn decode_group(&self, bytes: &[u8]) -> Result<([u8; 16], Option<Group>), Error> {
        let mut r = Reader::new(bytes);
        if r.raw(8)? != GROUP_MAGIC {
            return Err(Error::Corrupt("group magic"));
        }
        let gid = r.array::<16>()?;
        let class = Class::from_u8(r.u8()?)?;
        let sealed_hdr = r.bytes(GROUP_HDR_CAP)?.to_vec();
        let ck = match self.class_key_for_meta(class) {
            Ok(k) => k,
            Err(Error::Locked) => return Ok((gid, None)),
            Err(e) => return Err(e),
        };
        let plain = open(ck.key(), &aad_group(&self.user, &gid, class), &sealed_hdr)?;
        let mut hr = Reader::new(plain.as_ref());
        let persona = hr.str(caps::PERSONA)?.to_string();
        let recipient = Recipient::decode(&mut hr)?;
        hr.raw(KEY_LEN)?;
        hr.end()?;
        let n = r.u32()? as usize;
        if n > caps::ITEMS_PER_GROUP {
            return Err(Error::Corrupt("item count"));
        }
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            let id = ItemId(r.array::<16>()?);
            let version = r.u64()?;
            let meta = r.bytes(SEALED_CAP)?.to_vec();
            let value = r.bytes(SEALED_CAP)?.to_vec();
            items.push(ItemRec { id, version, meta, value });
        }
        r.end()?;
        Ok((gid, Some(Group { class, persona, recipient, sealed_hdr, items })))
    }

    fn write_group(&self, gid: &[u8; 16]) -> Result<(), Error> {
        let g = &self.groups[gid];
        let mut w = Writer::new();
        w.raw(GROUP_MAGIC);
        w.raw(gid);
        w.u8(g.class as u8);
        w.bytes(&g.sealed_hdr);
        w.u32(g.items.len() as u32);
        for it in &g.items {
            w.raw(&it.id.0);
            w.u64(it.version);
            w.bytes(&it.meta);
            w.bytes(&it.value);
        }
        write_atomic(&self.dir.join("groups").join(hex16(gid)), &w.buf)
    }

    fn find_group(&self, persona: &str, recipient: &Recipient, class: Class) -> Option<[u8; 16]> {
        self.groups
            .iter()
            .find(|(_, g)| g.persona == persona && &g.recipient == recipient && g.class == class)
            .map(|(k, _)| *k)
    }

    fn new_group(&mut self, persona: &str, recipient: &Recipient, class: Class) -> Result<[u8; 16], Error> {
        let mut gid = [0u8; 16];
        getrandom::getrandom(&mut gid).map_err(std::io::Error::other)?;
        let dk = LockedBuf::random(KEY_LEN)?;
        let mut w = Writer::new();
        w.str(persona);
        recipient.encode(&mut w);
        let mut plain = LockedBuf::zeroed(w.buf.len() + KEY_LEN)?;
        plain.as_mut()[..w.buf.len()].copy_from_slice(&w.buf);
        plain.as_mut()[w.buf.len()..].copy_from_slice(dk.as_ref());
        let ck = self.class_key_for_meta(class)?;
        let sealed_hdr = seal(ck.key(), &aad_group(&self.user, &gid, class), plain.as_ref())?;
        drop(ck);
        self.groups.insert(
            gid,
            Group { class, persona: persona.into(), recipient: recipient.clone(), sealed_hdr, items: Vec::new() },
        );
        Ok(gid)
    }

    // ── items ────────────────────────────────────────────────────────────

    fn encode_meta(m: &ItemMeta) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16(m.kind as u16);
        w.u8(m.provenance as u8);
        w.u64(m.created);
        w.u64(m.updated);
        w.str(&m.account);
        w.bytes(&m.scope);
        w.u32(m.flags);
        w.buf
    }

    fn decode_meta(g: &Group, id: ItemId, version: u64, plain: &[u8]) -> Result<ItemMeta, Error> {
        let mut r = Reader::new(plain);
        let kind = Kind::from_u16(r.u16()?)?;
        let provenance = Provenance::from_u8(r.u8()?)?;
        let created = r.u64()?;
        let updated = r.u64()?;
        let account = r.str(caps::ACCOUNT)?.to_string();
        let scope = r.bytes(caps::SCOPE)?.to_vec();
        let item_flags = r.u32()?;
        if item_flags & !flags::ALL != 0 {
            return Err(Error::Corrupt("unknown flags"));
        }
        r.end()?;
        Ok(ItemMeta {
            id,
            version,
            persona: g.persona.clone(),
            recipient: g.recipient.clone(),
            class: g.class,
            kind,
            provenance,
            account,
            scope,
            created,
            updated,
            flags: item_flags,
        })
    }

    /// Store a new item. Classes other than after-first-unlock need the
    /// store unlocked.
    pub fn add(&mut self, n: NewItem<'_>) -> Result<ItemMeta, Error> {
        if !valid_persona(n.persona) {
            return Err(Error::BadInput("persona"));
        }
        if n.account.len() > caps::ACCOUNT || n.scope.len() > caps::SCOPE || n.value.len() > caps::VALUE {
            return Err(Error::BadInput("field too long"));
        }
        if n.class != Class::AfterFirstUnlock && self.master.is_none() {
            return Err(Error::Locked);
        }
        if self.afu.is_none() {
            return Err(Error::Locked);
        }
        let gid = match self.find_group(n.persona, &n.recipient, n.class) {
            Some(g) => g,
            None => self.new_group(n.persona, &n.recipient, n.class)?,
        };
        if self.groups[&gid].items.len() >= caps::ITEMS_PER_GROUP {
            return Err(Error::Full);
        }
        let t = now();
        let id = ItemId::random();
        let meta = ItemMeta {
            id,
            version: 1,
            persona: n.persona.into(),
            recipient: n.recipient.clone(),
            class: n.class,
            kind: n.kind,
            provenance: n.provenance,
            account: n.account.into(),
            scope: n.scope.to_vec(),
            created: t,
            updated: t,
            flags: 0,
        };
        let rec = self.seal_item(&gid, &meta, n.value)?;
        self.groups.get_mut(&gid).unwrap().items.push(rec);
        self.by_item.insert(id, gid);
        if let Err(e) = self.write_group(&gid) {
            // Keep memory and disk in step: undo the insert.
            let g = self.groups.get_mut(&gid).unwrap();
            g.items.retain(|r| r.id != id);
            self.by_item.remove(&id);
            if g.items.is_empty() {
                self.groups.remove(&gid);
            }
            return Err(e);
        }
        Ok(meta)
    }

    fn seal_item(&self, gid: &[u8; 16], meta: &ItemMeta, value: &[u8]) -> Result<ItemRec, Error> {
        let g = &self.groups[gid];
        let ck = self.class_key_for_meta(g.class)?;
        let dk = self.data_key(gid, g, ck.key())?;
        let m = seal(&dk, &aad_item(&self.user, g, gid, &meta.id, meta.version, Part::Meta), &Self::encode_meta(meta))?;
        let v = seal(&dk, &aad_item(&self.user, g, gid, &meta.id, meta.version, Part::Value), value)?;
        Ok(ItemRec { id: meta.id, version: meta.version, meta: m, value: v })
    }

    /// Resolve an id for a verified asker. Missing, wrong persona and wrong
    /// recipient are all `Denied`: an asker learns nothing about items that
    /// aren't its own.
    fn locate(&self, id: &ItemId, who: &Access<'_>) -> Result<([u8; 16], usize), Error> {
        let gid = *self.by_item.get(id).ok_or(Error::Denied)?;
        let g = &self.groups[&gid];
        if g.persona != who.persona || &g.recipient != who.recipient {
            return Err(Error::Denied);
        }
        let idx = g.items.iter().position(|r| r.id == *id).ok_or(Error::Denied)?;
        Ok((gid, idx))
    }

    fn meta_of(&self, gid: &[u8; 16], idx: usize, dk: &LockedBuf) -> Result<ItemMeta, Error> {
        let g = &self.groups[gid];
        let it = &g.items[idx];
        let plain = open(dk, &aad_item(&self.user, g, gid, &it.id, it.version, Part::Meta), &it.meta)?;
        Self::decode_meta(g, it.id, it.version, plain.as_ref())
    }

    /// The item's metadata, no value.
    pub fn meta(&self, id: &ItemId, who: &Access<'_>) -> Result<ItemMeta, Error> {
        let (gid, idx) = self.locate(id, who)?;
        let g = &self.groups[&gid];
        let ck = self.class_key_for_meta(g.class)?;
        let dk = self.data_key(&gid, g, ck.key())?;
        self.meta_of(&gid, idx, &dk)
    }

    /// Release an item's value to its verified recipient. Verify-each-use
    /// items need a fresh [`UserVerified`]. The value is in locked memory;
    /// drop it as soon as it has been used.
    pub fn release(
        &self,
        id: &ItemId,
        who: &Access<'_>,
        verified: Option<UserVerified>,
    ) -> Result<(ItemMeta, LockedBuf), Error> {
        let (gid, idx) = self.locate(id, who)?;
        let g = &self.groups[&gid];
        let ck = self.class_key(g.class, verified)?;
        let dk = self.data_key(&gid, g, ck.key())?;
        let meta = self.meta_of(&gid, idx, &dk)?;
        let it = &g.items[idx];
        let value = open(&dk, &aad_item(&self.user, g, &gid, &it.id, it.version, Part::Value), &it.value)?;
        Ok((meta, value))
    }

    /// Items for one recipient, metadata only.
    pub fn list(&self, who: &Access<'_>) -> Result<Vec<ItemMeta>, Error> {
        let mut out = Vec::new();
        for (gid, g) in &self.groups {
            if g.persona != who.persona || &g.recipient != who.recipient {
                continue;
            }
            let ck = self.class_key_for_meta(g.class)?;
            let dk = self.data_key(gid, g, ck.key())?;
            for idx in 0..g.items.len() {
                out.push(self.meta_of(gid, idx, &dk)?);
            }
        }
        out.sort_by_key(|m| (m.created, m.id));
        Ok(out)
    }

    /// Replace an item's value (and optionally its account). The version
    /// goes up, so an old sealed record can't be replayed in its place.
    pub fn update(&mut self, id: &ItemId, who: &Access<'_>, value: &[u8], account: Option<&str>) -> Result<ItemMeta, Error> {
        if value.len() > caps::VALUE || account.is_some_and(|a| a.len() > caps::ACCOUNT) {
            return Err(Error::BadInput("field too long"));
        }
        let (gid, idx) = self.locate(id, who)?;
        let class = self.groups[&gid].class;
        if class != Class::AfterFirstUnlock && self.master.is_none() {
            return Err(Error::Locked);
        }
        let mut meta = self.meta(id, who)?;
        meta.version += 1;
        meta.updated = now();
        if let Some(a) = account {
            meta.account = a.into();
        }
        let rec = self.seal_item(&gid, &meta, value)?;
        let old = std::mem::replace(&mut self.groups.get_mut(&gid).unwrap().items[idx], rec);
        if let Err(e) = self.write_group(&gid) {
            self.groups.get_mut(&gid).unwrap().items[idx] = old;
            return Err(e);
        }
        Ok(meta)
    }

    /// Set or clear flag bits. Same version rule as [`Store::update`]: the
    /// value is re-sealed under the new version.
    pub fn set_flags(&mut self, id: &ItemId, who: &Access<'_>, set: u32, clear: u32) -> Result<ItemMeta, Error> {
        if (set | clear) & !flags::ALL != 0 {
            return Err(Error::BadInput("flags"));
        }
        let (gid, idx) = self.locate(id, who)?;
        let class = self.groups[&gid].class;
        if class != Class::AfterFirstUnlock && self.master.is_none() {
            return Err(Error::Locked);
        }
        let verified = (class == Class::VerifyEachUse).then(UserVerified::vouched_by_vestibulum);
        let (mut meta, value) = self.release(id, who, verified)?;
        meta.version += 1;
        meta.updated = now();
        meta.flags = (meta.flags | set) & !clear;
        let rec = self.seal_item(&gid, &meta, value.as_ref())?;
        drop(value);
        let old = std::mem::replace(&mut self.groups.get_mut(&gid).unwrap().items[idx], rec);
        if let Err(e) = self.write_group(&gid) {
            self.groups.get_mut(&gid).unwrap().items[idx] = old;
            return Err(e);
        }
        Ok(meta)
    }

    /// Delete one item.
    pub fn delete(&mut self, id: &ItemId, who: &Access<'_>) -> Result<(), Error> {
        let (gid, idx) = self.locate(id, who)?;
        let class = self.groups[&gid].class;
        if class != Class::AfterFirstUnlock && self.master.is_none() {
            return Err(Error::Locked);
        }
        let g = self.groups.get_mut(&gid).unwrap();
        let old = g.items.remove(idx);
        self.by_item.remove(id);
        if g.items.is_empty() {
            self.groups.remove(&gid);
            fs::remove_file(self.dir.join("groups").join(hex16(&gid)))?;
            File::open(self.dir.join("groups"))?.sync_all()?;
            return Ok(());
        }
        if let Err(e) = self.write_group(&gid) {
            self.groups.get_mut(&gid).unwrap().items.insert(idx, old);
            self.by_item.insert(*id, gid);
            return Err(e);
        }
        Ok(())
    }

    /// "Forget this recipient": destroy its groups, and with them their
    /// data keys, in every class (arca.md §6.1, §8.6). Needs the store
    /// unlocked, so that groups of every class are known.
    pub fn forget(&mut self, who: &Access<'_>) -> Result<usize, Error> {
        if self.master.is_none() {
            return Err(Error::Locked);
        }
        let gids: Vec<[u8; 16]> = self
            .groups
            .iter()
            .filter(|(_, g)| g.persona == who.persona && &g.recipient == who.recipient)
            .map(|(k, _)| *k)
            .collect();
        let mut n = 0;
        for gid in &gids {
            let g = self.groups.remove(gid).unwrap();
            n += g.items.len();
            for it in &g.items {
                self.by_item.remove(&it.id);
            }
            fs::remove_file(self.dir.join("groups").join(hex16(gid)))?;
        }
        File::open(self.dir.join("groups"))?.sync_all()?;
        Ok(n)
    }

    /// Every (persona, recipient) with items, for the trusted UI. Needs the
    /// store unlocked.
    pub fn recipients(&self) -> Result<Vec<(String, Recipient)>, Error> {
        if self.master.is_none() {
            return Err(Error::Locked);
        }
        let mut v: Vec<(String, Recipient)> =
            self.groups.values().map(|g| (g.persona.clone(), g.recipient.clone())).collect();
        v.sort();
        v.dedup();
        Ok(v)
    }
}

/// A class key: resident (borrowed) or unwrapped for one use (dropped and
/// zeroized when the borrow ends).
enum ClassKey<'a> {
    Resident(&'a LockedBuf),
    Transient(LockedBuf),
}

impl ClassKey<'_> {
    fn key(&self) -> &LockedBuf {
        match self {
            ClassKey::Resident(k) => k,
            ClassKey::Transient(k) => k,
        }
    }
}

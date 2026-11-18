//! The Opifex catalog: keys, the append-only log, signed heads, and the checks
//! a machine makes (docs/spec/opifex.md §5, §6; build step C1).
//!
//! On the server side a catalog is a folder of static files:
//!
//! ```text
//! root.toml, root.toml.sig   the keys, their roles and scopes; signed by a threshold of root keys
//! head, head.sig             size, Merkle root, time, expiry; signed by the catalog key
//! entries/000000             256 entries, each a u32 BE length + TOML text
//! entries/000001.p17         the last, partial bundle (17 entries)
//! ```
//!
//! Every machine downloads every entry, so it rebuilds the Merkle tree itself
//! (RFC 6962 hashing) and needs no inclusion or consistency proofs: it keeps
//! the leaf hashes it accepted, and a new head must extend them exactly.
//! Each release entry also carries its publisher's own signature over the
//! package's `atrium.toml`, so the catalog key can order and delay but not
//! forge.
//!
//! No network and no file system policy here: [`Client::update`] takes a
//! fetch function, so the jailed checker can run it on bytes the fetcher
//! handed over.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub mod driver;
pub mod font;
pub mod publisher;

use publisher::{Publisher, PublisherEntry};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ROOT: &str = "root.toml";
pub const ROOT_SIG: &str = "root.toml.sig";
pub const HEAD: &str = "head";
pub const HEAD_SIG: &str = "head.sig";
pub const ENTRIES: &str = "entries";
pub const BUNDLE: u64 = 256;
const HEAD_MAGIC: &str = "atrium-catalog/1";
const MAX_ENTRY: usize = 256 * 1024;

pub type Hash = [u8; 32];

pub fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

pub fn unhex32(s: &str) -> Option<Hash> {
    if s.len() != 64 { return None; }
    let mut o = [0u8; 32];
    for i in 0..32 { o[i] = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?; }
    Some(o)
}

fn b64(b: &[u8]) -> String { base64::engine::general_purpose::STANDARD.encode(b) }

fn sign(msg: &[u8], key_pem: &str) -> Result<String, String> {
    portcullis_sig::sign(msg, key_pem).map(|s| b64(&s)).map_err(|e| format!("sign: {e:?}"))
}

fn verify(msg: &[u8], sig_b64: &str, pem: &str) -> bool {
    portcullis_sig::sig_from_base64(sig_b64).ok()
        .is_some_and(|der| portcullis_sig::verify_one(msg, &der, pem).is_ok())
}

/// A key's id: SHA-256 of its SPKI DER, as portcullis-sig computes it.
pub fn key_id(pem: &str) -> Result<String, String> {
    portcullis_sig::publisher_id(pem).map(|h| hex(&h)).map_err(|e| format!("key: {e:?}"))
}

// ── Merkle (RFC 6962) ───────────────────────────────────────────────────────

pub fn leaf_hash(entry: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(entry);
    h.finalize().into()
}

fn node(l: &Hash, r: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(l);
    h.update(r);
    h.finalize().into()
}

/// The Merkle tree hash of `leaves` (RFC 6962 §2.1).
pub fn tree_hash(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => Sha256::digest(b"").into(),
        1 => leaves[0],
        n => {
            let mut k = 1;
            while k * 2 < n { k *= 2; }
            node(&tree_hash(&leaves[..k]), &tree_hash(&leaves[k..]))
        }
    }
}

// ── root.toml ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub pem: String,
    /// `root`, `catalog`, `platform`.
    pub roles: Vec<String>,
    /// App id prefixes this key may publish (`org.atrium.`).
    #[serde(default)]
    pub prefixes: Vec<String>,
    /// Package kinds this key may publish.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// May ship `devices.d` entries (device-assignment §4.2.6).
    #[serde(default)]
    pub devices: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub schema: String,
    pub name: String,
    /// Increases with every new root.toml.
    pub version: u64,
    /// Unix seconds.
    pub expires: u64,
    /// How many root keys must sign a root.toml.
    pub threshold: usize,
    #[serde(rename = "key")]
    pub keys: Vec<Key>,
}

pub const ROOT_SCHEMA: &str = "atrium-catalog-root/1";
pub const KINDS: [&str; 6] = ["app", "service", "layer", "font", "driver", "firmware"];

impl Root {
    pub fn parse(text: &str) -> Result<Root, String> {
        let r: Root = toml::from_str(text).map_err(|e| format!("{ROOT}: {e}"))?;
        if r.schema != ROOT_SCHEMA {
            return Err(format!("{ROOT}: schema {:?}, want {ROOT_SCHEMA}", r.schema));
        }
        let roots = r.with_role("root").count();
        if r.threshold == 0 || r.threshold > roots {
            return Err(format!("{ROOT}: threshold {} with {roots} root keys", r.threshold));
        }
        if r.with_role("catalog").count() == 0 {
            return Err(format!("{ROOT}: no catalog key"));
        }
        let mut ids = HashSet::new();
        for k in &r.keys {
            if !ids.insert(key_id(&k.pem)?) {
                return Err(format!("{ROOT}: a key listed twice"));
            }
            for role in &k.roles {
                if !["root", "catalog", "platform"].contains(&role.as_str()) {
                    return Err(format!("{ROOT}: unknown role {role:?}"));
                }
            }
            if k.roles.iter().any(|x| x == "root") && k.roles.len() > 1 {
                return Err(format!("{ROOT}: a root key has no other role (it stays offline)"));
            }
            for kind in &k.kinds {
                if !KINDS.contains(&kind.as_str()) {
                    return Err(format!("{ROOT}: unknown kind {kind:?}"));
                }
            }
        }
        Ok(r)
    }

    pub fn with_role<'a>(&'a self, role: &'a str) -> impl Iterator<Item = &'a Key> + 'a {
        self.keys.iter().filter(move |k| k.roles.iter().any(|r| r == role))
    }

    pub fn key(&self, id: &str) -> Option<&Key> {
        self.keys.iter().find(|k| key_id(&k.pem).ok().as_deref() == Some(id))
    }

    /// How many of *this* root's root keys signed `text` (sig file: lines of
    /// `<key id> <base64 signature>`).
    pub fn count_signers(&self, text: &str, sigs: &str) -> usize {
        let mut seen = HashSet::new();
        for line in sigs.lines() {
            let mut it = line.split_whitespace();
            let (Some(id), Some(sig)) = (it.next(), it.next()) else { continue };
            let Some(k) = self.key(id) else { continue };
            if k.roles.iter().any(|r| r == "root") && verify(text.as_bytes(), sig, &k.pem) {
                seen.insert(id.to_string());
            }
        }
        seen.len()
    }

    /// Is `next` (text + sigs) a valid successor of this root? Signed by our
    /// threshold of our root keys *and* its own threshold of its root keys,
    /// with a higher version.
    pub fn accept_next(&self, text: &str, sigs: &str, now: u64) -> Result<Root, String> {
        let next = Root::parse(text)?;
        if next.version <= self.version {
            return Err(format!("{ROOT}: version {} is not newer than {}", next.version, self.version));
        }
        if self.count_signers(text, sigs) < self.threshold {
            return Err(format!("{ROOT}: not signed by {} of the current root keys", self.threshold));
        }
        if next.count_signers(text, sigs) < next.threshold {
            return Err(format!("{ROOT}: not signed by {} of its own root keys", next.threshold));
        }
        if next.expires <= now {
            return Err(format!("{ROOT}: expired"));
        }
        Ok(next)
    }
}

/// Sign a root.toml with one root key: the line to append to root.toml.sig.
pub fn sign_root(text: &str, key_pem: &str, pub_pem: &str) -> Result<String, String> {
    Ok(format!("{} {}\n", key_id(pub_pem)?, sign(text.as_bytes(), key_pem)?))
}

// ── heads ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    pub name: String,
    pub size: u64,
    pub root: Hash,
    pub time: u64,
    pub expires: u64,
}

impl Head {
    pub fn text(&self) -> String {
        format!("{HEAD_MAGIC}\nname {}\nsize {}\nroot {}\ntime {}\nexpires {}\n",
                self.name, self.size, hex(&self.root), self.time, self.expires)
    }

    pub fn parse(text: &str) -> Result<Head, String> {
        let mut l = text.lines();
        if l.next() != Some(HEAD_MAGIC) {
            return Err("head: not an atrium catalog head".into());
        }
        let mut field = |k: &str| -> Result<String, String> {
            let line = l.next().ok_or_else(|| format!("head: no {k}"))?;
            line.strip_prefix(&format!("{k} ")).map(str::to_string).ok_or_else(|| format!("head: want {k}"))
        };
        let name = field("name")?;
        let size = field("size")?.parse().map_err(|_| "head: size")?;
        let root = unhex32(&field("root")?).ok_or("head: root")?;
        let time = field("time")?.parse().map_err(|_| "head: time")?;
        let expires = field("expires")?.parse().map_err(|_| "head: expires")?;
        let h = Head { name, size, root, time, expires };
        if h.text() != text {
            return Err("head: not in canonical form".into());
        }
        Ok(h)
    }

    /// Check a head against the root's catalog keys.
    pub fn verify(text: &str, sig: &str, root: &Root, now: u64) -> Result<Head, String> {
        let h = Head::parse(text)?;
        if !root.with_role("catalog").any(|k| verify(text.as_bytes(), sig.trim(), &k.pem)) {
            return Err("head: not signed by the catalog key".into());
        }
        if h.name != root.name {
            return Err(format!("head: for catalog {:?}, not {:?}", h.name, root.name));
        }
        if h.expires <= now {
            return Err(format!("head: expired at {} (the catalog may be frozen; now {now})", h.expires));
        }
        Ok(h)
    }
}

// ── entries ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub id: String,
    pub version: String,
    pub arch: String,
    pub kind: String,
    /// Key id of the signer.
    pub signer: String,
    /// The package's `atrium.toml`, exactly as signed.
    pub manifest: String,
    /// The publisher's signature over `manifest` (base64 DER).
    pub sig: String,
    pub archive_sha256: String,
    pub archive_size: u64,
    pub urls: Vec<String>,
    /// The publisher record whose key signed (C7); empty for root.toml keys.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Yank {
    pub id: String,
    pub version: String,
    pub reason: String,
    pub signer: String,
    pub sig: String,
}

impl Yank {
    pub fn message(id: &str, version: &str, reason: &str) -> String {
        format!("atrium-catalog yank\n{id}\n{version}\n{reason}\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Locations {
    /// Base URLs the catalog is served from, first preferred.
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(rename = "type")]
    pub kind: String,
    pub time: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Release>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yank: Option<Yank>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Locations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<PublisherEntry>,
}

impl Entry {
    pub fn parse(bytes: &[u8]) -> Result<Entry, String> {
        let t = std::str::from_utf8(bytes).map_err(|_| "entry: not UTF-8")?;
        let e: Entry = toml::from_str(t).map_err(|e| format!("entry: {e}"))?;
        let n = [e.release.is_some(), e.yank.is_some(), e.locations.is_some(), e.publisher.is_some()]
            .iter().filter(|x| **x).count();
        let want = match e.kind.as_str() {
            "release" => e.release.is_some(),
            "yank" => e.yank.is_some(),
            "locations" => e.locations.is_some(),
            "publisher" => e.publisher.is_some(),
            _ => false,
        };
        if n != 1 || !want {
            return Err(format!("entry: type {:?} with the wrong body", e.kind));
        }
        Ok(e)
    }

    pub fn bytes(&self) -> Vec<u8> { toml::to_string(self).expect("entry serialises").into_bytes() }
}

/// What a release's manifest says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub kind: String,
    pub id: String,
    pub version: String,
    pub arches: Vec<String>,
    pub content: Option<String>,
}

/// Read the identifying fields of a signed `atrium.toml` (an app bundle, a
/// toolchain layer or a font package).
pub fn describe(manifest: &str) -> Result<Described, String> {
    let v: toml::Value = manifest.parse().map_err(|e| format!("manifest: {e}"))?;
    if v.get("kind").and_then(|k| k.as_str()) == Some("driver") {
        let m = driver::DriverManifest::from_str(manifest)?;
        let content = (!m.driver.content.is_empty()).then(|| m.driver.content.clone());
        return Ok(Described { kind: "driver".into(), id: m.driver.id, version: m.driver.version, arches: Vec::new(), content });
    }
    if v.get("kind").and_then(|k| k.as_str()) == Some("firmware") {
        let m = driver::FirmwareManifest::from_str(manifest)?;
        let content = (!m.firmware.content.is_empty()).then(|| m.firmware.content.clone());
        return Ok(Described { kind: "firmware".into(), id: m.firmware.id, version: m.firmware.version, arches: vec![font::ARCH.into()], content });
    }
    if v.get("kind").and_then(|k| k.as_str()) == Some("font") {
        let m = font::FontManifest::from_str(manifest)?;
        let content = (!m.font.content.is_empty()).then(|| m.font.content.clone());
        return Ok(Described { kind: "font".into(), id: m.font.id, version: m.font.version, arches: vec![font::ARCH.into()], content });
    }
    if v.get("kind").and_then(|k| k.as_str()) == Some("layer") {
        let m = portcullis_toml::layer::LayerManifest::from_str(manifest)?;
        let content = (!m.layer.content.is_empty()).then(|| m.layer.content.clone());
        return Ok(Described { kind: "layer".into(), id: m.layer.name.clone(), version: m.layer.version.clone(),
                              arches: vec![m.layer.arch.clone()], content });
    }
    let m = portcullis_toml::Manifest::from_str(manifest).map_err(|e| format!("manifest: {e:?}"))?;
    let b = m.bundle.as_ref();
    Ok(Described { kind: "app".into(), id: m.app.id.clone(), version: m.app.version.clone(),
                   arches: b.map(|b| b.arches.clone()).unwrap_or_default(),
                   content: b.and_then(|b| b.content.clone()) })
}

/// Who may publish in a catalog: its root.toml, whether it is a third-party
/// catalog (apps only, never `org.atrium.`, publishers only), and the
/// publisher records seen so far in its log.
pub struct Scope<'a> {
    pub root: &'a Root,
    pub third_party: bool,
    pub publishers: &'a [Publisher],
}

/// The signing key's pem and scopes for a release.
fn signer_of<'a>(r: &Release, s: &'a Scope) -> Result<(&'a str, &'a [String], &'a [String]), String> {
    let what = format!("{} {} {}", r.id, r.version, r.arch);
    if !r.publisher.is_empty() {
        let p = s.publishers.iter().find(|p| p.id == r.publisher)
            .ok_or_else(|| format!("{what}: publisher {} not registered", r.publisher))?;
        if p.key_id != r.signer {
            return Err(format!("{what}: not signed by {}'s current key", p.id));
        }
        return Ok((&p.key, &p.prefixes, &p.kinds));
    }
    if s.third_party {
        return Err(format!("{what}: a third-party catalog publishes through publisher records"));
    }
    let key = s.root.key(&r.signer).ok_or_else(|| format!("{what}: signer not in {ROOT}"))?;
    // The Atrium catalog publishes only what Atrium builds (opifex.md §1).
    if !key.roles.iter().any(|x| x == "platform") {
        return Err(format!("{what}: signer is not a publishing key"));
    }
    Ok((&key.pem, &key.prefixes, &key.kinds))
}

/// Check one release against its signer's scopes and its own signature.
pub fn check_release(r: &Release, s: &Scope) -> Result<(), String> {
    let what = format!("{} {} {}", r.id, r.version, r.arch);
    let (pem, prefixes, kinds) = signer_of(r, s)?;
    if s.third_party && (r.kind != "app" || r.id.starts_with("org.atrium.")) {
        return Err(format!("{what}: a third-party catalog carries apps only, never org.atrium."));
    }
    if !kinds.iter().any(|k| *k == r.kind) {
        return Err(format!("{what}: signer may not publish kind {}", r.kind));
    }
    if !verify(r.manifest.as_bytes(), &r.sig, pem) {
        return Err(format!("{what}: manifest not signed by its signer"));
    }
    let d = describe(&r.manifest)?;
    if d.kind != r.kind || d.id != r.id || d.version != r.version {
        return Err(format!("{what}: entry doesn't match its manifest ({} {} {})", d.kind, d.id, d.version));
    }
    if r.kind != "layer" && !prefixes.iter().any(|p| r.id.starts_with(p.as_str())) {
        return Err(format!("{what}: id outside the signer's prefixes"));
    }
    if r.id.starts_with("dev.") {
        return Err(format!("{what}: dev. ids are never in a catalog"));
    }
    if !d.arches.is_empty() && !d.arches.contains(&r.arch) {
        return Err(format!("{what}: manifest doesn't list arch {}", r.arch));
    }
    if d.content.is_none() {
        return Err(format!("{what}: not packed (no content index in the manifest)"));
    }
    if unhex32(&r.archive_sha256).is_none() || r.archive_size == 0 {
        return Err(format!("{what}: archive hash or size"));
    }
    if r.urls.is_empty() || !r.urls.iter().all(|u| u.starts_with("https://")) {
        return Err(format!("{what}: urls must be https"));
    }
    Ok(())
}

// ── entry bundles ───────────────────────────────────────────────────────────

/// File name of bundle `n` holding `count` entries.
pub fn bundle_name(n: u64, count: u64) -> String {
    if count == BUNDLE { format!("{ENTRIES}/{n:06}") } else { format!("{ENTRIES}/{n:06}.p{count}") }
}

pub fn encode_bundle(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(&(e.len() as u32).to_be_bytes());
        out.extend_from_slice(e);
    }
    out
}

pub fn decode_bundle(b: &[u8], want: u64) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let n = u32::from_be_bytes(b.get(i..i + 4).ok_or("bundle: cut length")?.try_into().unwrap()) as usize;
        if n > MAX_ENTRY {
            return Err("bundle: entry too big".into());
        }
        out.push(b.get(i + 4..i + 4 + n).ok_or("bundle: cut entry")?.to_vec());
        i += 4 + n;
    }
    if out.len() as u64 != want {
        return Err(format!("bundle: {} entries, want {want}", out.len()));
    }
    Ok(out)
}

// ── the publishing side: a catalog folder ───────────────────────────────────

pub struct Log { pub dir: PathBuf, pub third_party: bool }

impl Log {
    /// The Atrium catalog's log (platform keys from root.toml).
    pub fn open(dir: &Path) -> Log { Log { dir: dir.to_path_buf(), third_party: false } }

    /// A third-party catalog's log: apps from publisher records only.
    pub fn third_party(dir: &Path) -> Log { Log { dir: dir.to_path_buf(), third_party: true } }

    pub fn root(&self) -> Result<Root, String> {
        Root::parse(&fs::read_to_string(self.dir.join(ROOT)).map_err(|e| format!("{ROOT}: {e}"))?)
    }

    /// Every entry, in order.
    pub fn entries(&self) -> Result<Vec<Vec<u8>>, String> {
        let d = self.dir.join(ENTRIES);
        let mut names: Vec<String> = match fs::read_dir(&d) {
            Ok(rd) => rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect(),
            Err(_) => return Ok(Vec::new()),
        };
        names.sort();
        let mut out = Vec::new();
        for (i, n) in names.iter().enumerate() {
            let (num, count) = match n.split_once(".p") {
                Some((a, c)) => (a, c.parse::<u64>().map_err(|_| format!("{n}: name"))?),
                None => (n.as_str(), BUNDLE),
            };
            if num.parse::<u64>().ok() != Some(i as u64) || (count != BUNDLE && i + 1 != names.len()) {
                return Err(format!("{ENTRIES}/{n}: out of sequence"));
            }
            out.extend(decode_bundle(&fs::read(d.join(n)).map_err(|e| e.to_string())?, count)?);
        }
        Ok(out)
    }

    /// Append entries, checking each against the root first.
    pub fn append(&self, new: &[Entry]) -> Result<u64, String> {
        let root = self.root()?;
        let mut all = self.entries()?;
        // Replay what is there, exactly as a machine will, then the new ones.
        let mut local = Local::default();
        let mut seen = HashSet::new();
        for (i, e) in all.iter().enumerate() {
            let e = Entry::parse(e).map_err(|m| format!("entry {i}: {m}"))?;
            apply(&e, &root, self.third_party, &mut local, &mut seen).map_err(|m| format!("entry {i}: {m}"))?;
        }
        for e in new {
            apply(e, &root, self.third_party, &mut local, &mut seen)?;
            all.push(e.bytes());
        }
        let d = self.dir.join(ENTRIES);
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        // Rewrite from the first bundle that changed.
        let old_partial: Vec<PathBuf> = fs::read_dir(&d).map_err(|e| e.to_string())?.flatten()
            .map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".p")).collect();
        let first = (all.len() as u64 - new.len() as u64) / BUNDLE;
        for (n, chunk) in all.chunks(BUNDLE as usize).enumerate().skip(first as usize) {
            let name = bundle_name(n as u64, chunk.len() as u64);
            write_atomic(&self.dir.join(&name), &encode_bundle(chunk))?;
        }
        for p in old_partial {
            let keep = all.chunks(BUNDLE as usize).enumerate()
                .any(|(n, c)| self.dir.join(bundle_name(n as u64, c.len() as u64)) == p);
            if !keep {
                let _ = fs::remove_file(p);
            }
        }
        Ok(all.len() as u64)
    }

    /// Recompute the tree and sign a new head with the catalog key.
    pub fn sign_head(&self, key_pem: &str, now: u64, days: u64) -> Result<Head, String> {
        let root = self.root()?;
        let leaves: Vec<Hash> = self.entries()?.iter().map(|e| leaf_hash(e)).collect();
        let h = Head { name: root.name.clone(), size: leaves.len() as u64, root: tree_hash(&leaves),
                       time: now, expires: now + days * 86400 };
        let text = h.text();
        let sig = sign(text.as_bytes(), key_pem)?;
        if !root.with_role("catalog").any(|k| verify(text.as_bytes(), &sig, &k.pem)) {
            return Err("that key is not the catalog key in root.toml".into());
        }
        write_atomic(&self.dir.join(HEAD), text.as_bytes())?;
        write_atomic(&self.dir.join(HEAD_SIG), format!("{sig}\n").as_bytes())?;
        Ok(h)
    }
}

/// Build a release entry from a packed, signed package folder.
pub fn release_entry(pkg: &Path, arch: &str, archive_sha256: &str, archive_size: u64, urls: Vec<String>,
                     signer_pem: &str, now: u64) -> Result<Entry, String> {
    let manifest = fs::read_to_string(pkg.join("atrium.toml")).map_err(|e| format!("atrium.toml: {e}"))?;
    let sig = fs::read_to_string(pkg.join("atrium.toml.sig")).map_err(|e| format!("atrium.toml.sig: {e}"))?;
    let d = describe(&manifest)?;
    Ok(Entry {
        kind: "release".into(),
        time: now,
        release: Some(Release {
            id: d.id, version: d.version, arch: arch.into(), kind: d.kind, signer: key_id(signer_pem)?,
            manifest, sig: sig.trim().into(), archive_sha256: archive_sha256.into(), archive_size, urls,
            publisher: String::new(),
        }),
        yank: None,
        locations: None,
        publisher: None,
    })
}

pub fn yank_entry(id: &str, version: &str, reason: &str, key_pem: &str, pub_pem: &str, now: u64) -> Result<Entry, String> {
    Ok(Entry {
        kind: "yank".into(),
        time: now,
        release: None,
        yank: Some(Yank { id: id.into(), version: version.into(), reason: reason.into(), signer: key_id(pub_pem)?,
                          sig: sign(Yank::message(id, version, reason).as_bytes(), key_pem)? }),
        locations: None,
        publisher: None,
    })
}

fn write_atomic(p: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

// ── the machine side ────────────────────────────────────────────────────────

/// The checked local catalog: every release still offered, newest last.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Local {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub locations: Vec<String>,
    #[serde(default)]
    pub yanked: Vec<(String, String)>,
    #[serde(default, rename = "release")]
    pub releases: Vec<Release>,
    #[serde(default, rename = "publisher")]
    pub publishers: Vec<Publisher>,
}

impl Local {
    /// The newest release of `id` for `arch` (or for any arch) that isn't yanked.
    pub fn latest(&self, id: &str, arch: &str) -> Option<&Release> {
        self.releases.iter()
            .filter(|r| r.id == id && (r.arch == arch || r.arch == font::ARCH) && !self.yanked.iter().any(|(i, v)| i == id && *v == r.version))
            .max_by(|a, b| version_cmp(&a.version, &b.version))
    }
}

/// Compare dotted versions numerically where both parts are numbers.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (pa, pb): (Vec<&str>, Vec<&str>) = (a.split(['.', '-', '+']).collect(), b.split(['.', '-', '+']).collect());
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or("0"), pb.get(i).copied().unwrap_or("0"));
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            _ => x.cmp(y),
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

/// A machine's view of one catalog, kept in a state folder:
/// `root.toml` (pinned), `leaves` (accepted leaf hashes), `head`, `catalog.toml`.
pub struct Client { pub dir: PathBuf, pub third_party: bool }

const LEAVES: &str = "leaves";
pub const LOCAL: &str = "catalog.toml";

impl Client {
    /// Start a state folder from a root.toml shipped with the system.
    pub fn init(dir: &Path, root_text: &str) -> Result<Client, String> {
        Root::parse(root_text)?;
        write_atomic(&dir.join(ROOT), root_text.as_bytes())?;
        Ok(Client { dir: dir.to_path_buf(), third_party: false })
    }

    /// The Atrium catalog's state folder.
    pub fn open(dir: &Path) -> Client { Client { dir: dir.to_path_buf(), third_party: false } }

    /// A third-party catalog's state folder: apps only, from publisher records.
    pub fn third_party(dir: &Path) -> Client { Client { dir: dir.to_path_buf(), third_party: true } }

    pub fn root(&self) -> Result<Root, String> {
        Root::parse(&fs::read_to_string(self.dir.join(ROOT)).map_err(|e| format!("pinned {ROOT}: {e}"))?)
    }

    pub fn local(&self) -> Result<Local, String> {
        match fs::read_to_string(self.dir.join(LOCAL)) {
            Ok(t) => toml::from_str(&t).map_err(|e| format!("{LOCAL}: {e}")),
            Err(_) => Ok(Local::default()),
        }
    }

    fn leaves(&self) -> Result<Vec<Hash>, String> {
        let b = fs::read(self.dir.join(LEAVES)).unwrap_or_default();
        if b.len() % 32 != 0 {
            return Err(format!("{LEAVES}: damaged"));
        }
        Ok(b.chunks(32).map(|c| c.try_into().unwrap()).collect())
    }

    /// Sync: `fetch(path)` returns a catalog file's bytes (`root.toml`, `head`,
    /// `entries/…`). Everything is checked; on any failure the state is unchanged.
    pub fn update(&self, fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>, now: u64) -> Result<Local, String> {
        let utf8 = |b: Vec<u8>, w: &str| String::from_utf8(b).map_err(|_| format!("{w}: not UTF-8"));
        // 1. Root: adopt a newer one only if the pinned one's keys signed it.
        let mut root = self.root()?;
        let root_text = utf8(fetch(ROOT)?, ROOT)?;
        let pinned_text = fs::read_to_string(self.dir.join(ROOT)).unwrap_or_default();
        let new_root = if root_text != pinned_text {
            let sigs = utf8(fetch(ROOT_SIG)?, ROOT_SIG)?;
            root = root.accept_next(&root_text, &sigs, now)?;
            true
        } else {
            false
        };
        if root.expires <= now {
            return Err(format!("{ROOT}: expired"));
        }
        // 2. Head.
        let head_text = utf8(fetch(HEAD)?, HEAD)?;
        let head_sig = utf8(fetch(HEAD_SIG)?, HEAD_SIG)?;
        let head = Head::verify(&head_text, &head_sig, &root, now)?;
        let mut leaves = self.leaves()?;
        let old = leaves.len() as u64;
        if head.size < old {
            return Err(format!("head: size {} is smaller than the {old} entries already seen (history rewritten)", head.size));
        }
        // 3. New entries, from the bundle that holds entry `old`.
        let mut local = self.local()?;
        local.name = root.name.clone();
        let mut seen: HashSet<(String, String, String, String)> = local.releases.iter()
            .map(|r| (r.kind.clone(), r.id.clone(), r.version.clone(), r.arch.clone())).collect();
        let mut n = old / BUNDLE;
        while n * BUNDLE < head.size {
            let count = (head.size - n * BUNDLE).min(BUNDLE);
            let bytes = fetch(&bundle_name(n, count))?;
            let entries = decode_bundle(&bytes, count)?;
            for (i, e) in entries.iter().enumerate() {
                let idx = (n * BUNDLE + i as u64) as usize;
                let h = leaf_hash(e);
                if idx < leaves.len() {
                    if leaves[idx] != h {
                        return Err(format!("entry {idx} differs from the one seen before (history rewritten)"));
                    }
                    continue;
                }
                let entry = Entry::parse(e).map_err(|m| format!("entry {idx}: {m}"))?;
                apply(&entry, &root, self.third_party, &mut local, &mut seen).map_err(|m| format!("entry {idx}: {m}"))?;
                leaves.push(h);
            }
            n += 1;
        }
        if leaves.len() as u64 != head.size || tree_hash(&leaves) != head.root {
            return Err("head: entries don't hash to the signed root".into());
        }
        local.size = head.size;
        // 4. Commit.
        let flat: Vec<u8> = leaves.iter().flatten().copied().collect();
        write_atomic(&self.dir.join(LEAVES), &flat)?;
        write_atomic(&self.dir.join(LOCAL), toml::to_string(&local).map_err(|e| e.to_string())?.as_bytes())?;
        write_atomic(&self.dir.join(HEAD), head_text.as_bytes())?;
        if new_root {
            write_atomic(&self.dir.join(ROOT), root_text.as_bytes())?;
        }
        Ok(local)
    }
}

fn apply(e: &Entry, root: &Root, third_party: bool, local: &mut Local,
         seen: &mut HashSet<(String, String, String, String)>) -> Result<(), String> {
    if let Some(p) = &e.publisher {
        p.apply(root, third_party, &mut local.publishers)?;
    }
    if let Some(r) = &e.release {
        check_release(r, &Scope { root, third_party, publishers: &local.publishers })?;
        if !seen.insert((r.kind.clone(), r.id.clone(), r.version.clone(), r.arch.clone())) {
            return Err(format!("{} {} {} is published already", r.id, r.version, r.arch));
        }
        local.releases.push(r.clone());
    }
    if let Some(y) = &e.yank {
        // A publishing key yanks its own releases; a publisher's current key
        // yanks any of that publisher's (also those signed by a past key).
        let pem = match local.publishers.iter().find(|p| p.key_id == y.signer) {
            Some(p) => {
                if !local.releases.iter().any(|r| r.id == y.id && r.version == y.version && r.publisher == p.id) {
                    return Err("yank: of a release this publisher didn't publish".into());
                }
                p.key.clone()
            }
            None if !third_party => {
                let key = root.key(&y.signer).ok_or("yank: signer not in root.toml")?;
                if !key.roles.iter().any(|x| x == "platform") {
                    return Err("yank: not signed by a publishing key".into());
                }
                if !local.releases.iter().any(|r| r.id == y.id && r.version == y.version && r.signer == y.signer) {
                    return Err("yank: of a release this key didn't publish".into());
                }
                key.pem.clone()
            }
            None => return Err("yank: not signed by a publisher's current key".into()),
        };
        if !verify(Yank::message(&y.id, &y.version, &y.reason).as_bytes(), &y.sig, &pem) {
            return Err("yank: bad signature".into());
        }
        local.yanked.push((y.id.clone(), y.version.clone()));
    }
    if let Some(l) = &e.locations {
        if l.urls.is_empty() || !l.urls.iter().all(|u| u.starts_with("https://")) {
            return Err("locations: https urls".into());
        }
        local.locations = l.urls.clone();
    }
    Ok(())
}

/// Group releases by id for listing: id → versions.
pub fn by_id(local: &Local) -> BTreeMap<String, Vec<&Release>> {
    let mut m: BTreeMap<String, Vec<&Release>> = BTreeMap::new();
    for r in &local.releases {
        m.entry(r.id.clone()).or_default().push(r);
    }
    m
}

#[cfg(test)]
mod tests;

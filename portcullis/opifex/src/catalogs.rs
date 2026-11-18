//! The catalogs a machine follows (opifex.md §6.5; build step C7).
//!
//! The Atrium catalog lives in `<state>/catalog/` and is pinned by the
//! system's `root.toml`. Every other catalog was added by a person, with a
//! press on the sheet, and lives in `<state>/catalogs/<slug>/`:
//!
//! ```text
//! root.toml, leaves, head, catalog.toml   as for the Atrium catalog (opifex_catalog::Client)
//! source.toml                             where it is served, its root.toml's hash, who added it, for whom
//! proofs.toml                             the publisher domains this machine checked itself
//! ```
//!
//! A third-party catalog carries apps only, never `org.atrium.`, and only
//! through publisher records (opifex_catalog::Scope). Its releases are
//! offered only once this machine has checked the publisher's domain proof
//! itself ([`prove`]), so a catalog can't list `com.hdfcbank.` unless
//! whoever controls hdfcbank.com named that publisher there.

use std::fs;
use std::path::{Path, PathBuf};

use opifex_catalog::{self as oc, publisher};
use serde::{Deserialize, Serialize};

pub const DIR: &str = "catalogs";
pub const SOURCE: &str = "source.toml";
pub const PROOFS: &str = "proofs.toml";
/// What the sheet says before the press (opifex.md §6.5).
pub const WARNING: &str = "Apps from this catalog are checked by nobody but its publisher; Atrium has not reviewed them.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The https base the catalog was added from.
    pub url: String,
    /// sha256 of the `root.toml` the person approved.
    pub root_sha256: String,
    /// Added by an administrator for everyone; else for `added_by` only.
    pub machine: bool,
    pub added_by: u32,
    pub time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proof {
    pub publisher: String,
    pub domain: String,
    /// `txt` or `well-known`.
    pub how: String,
    pub time: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Proofs {
    #[serde(default, rename = "proof")]
    pub proofs: Vec<Proof>,
}

/// One catalog the machine follows.
pub struct Cat {
    /// `""` for the Atrium catalog; else a host name, maybe with `-2`.
    pub slug: String,
    pub client: oc::Client,
    pub source: Option<Source>,
}

/// One catalog as `Request::Catalogs` lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Listed {
    pub slug: String,
    pub name: String,
    pub url: String,
    pub third_party: bool,
    pub machine: bool,
    pub added_by: u32,
    pub releases: usize,
    pub publishers: Vec<String>,
}

fn write_atomic(p: &Path, b: &[u8]) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, b).and_then(|_| fs::rename(&tmp, p)).map_err(|e| format!("{}: {e}", p.display()))
}

impl Cat {
    pub fn third_party(&self) -> bool { self.source.is_some() }

    /// The installed record's `source` for packages from here. The Atrium
    /// catalog's is its root's name (no dot); a third party's is its slug (a
    /// host name, always with a dot), so the two never meet.
    pub fn tag(&self, local: &oc::Local) -> String {
        if self.third_party() { format!("catalog:{}", self.slug) } else { format!("catalog:{}", local.name) }
    }

    /// Whether `uid` sees this catalog's packages.
    pub fn visible_to(&self, uid: u32) -> bool {
        self.source.as_ref().is_none_or(|s| s.machine || uid == 0 || s.added_by == uid)
    }

    pub fn proofs(&self) -> Proofs {
        fs::read_to_string(self.client.dir.join(PROOFS)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_proofs(&self, p: &Proofs) -> Result<(), String> {
        write_atomic(&self.client.dir.join(PROOFS), toml::to_string(p).map_err(|e| e.to_string())?.as_bytes())
    }

    /// Whether this machine may offer `r`: always in the Atrium catalog; in a
    /// third-party one only when its publisher proved, to this machine, the
    /// domain whose prefix holds the id.
    pub fn proven(&self, local: &oc::Local, r: &oc::Release) -> bool {
        if !self.third_party() {
            return true;
        }
        let Some(p) = local.publishers.iter().find(|p| p.id == r.publisher) else { return false };
        let proofs = self.proofs();
        p.domains.iter().any(|d| r.id.starts_with(&publisher::prefix_of(d))
            && proofs.proofs.iter().any(|x| x.publisher == p.id && x.domain == *d))
    }

    /// The newest release of `id` this machine may offer from here.
    pub fn offer(&self, local: &oc::Local, id: &str, arch: &str) -> Option<oc::Release> {
        local.latest(id, arch).filter(|r| self.proven(local, r)).cloned()
    }
}

pub fn atrium(state: &Path) -> Cat {
    Cat { slug: String::new(), client: oc::Client::open(&state.join("catalog")), source: None }
}

/// Every catalog: the Atrium catalog first, then the others by slug.
pub fn all(state: &Path) -> Vec<Cat> {
    let mut out = vec![atrium(state)];
    let mut names: Vec<String> = fs::read_dir(state.join(DIR)).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    for n in names {
        let dir = state.join(DIR).join(&n);
        let Ok(t) = fs::read_to_string(dir.join(SOURCE)) else { continue };
        match toml::from_str::<Source>(&t) {
            Ok(s) => out.push(Cat { slug: n, client: oc::Client::third_party(&dir), source: Some(s) }),
            Err(e) => eprintln!("opifex: {}: {e}", dir.display()),
        }
    }
    out
}

/// The catalog an installed record came from.
pub fn of_source<'a>(cats: &'a [Cat], source: &str) -> Option<&'a Cat> {
    cats.iter().find(|c| c.client.local().is_ok_and(|l| c.tag(&l) == source))
}

/// The host of an https base URL, checked as a plain lowercase host name.
pub fn host_of(url: &str) -> Result<String, String> {
    if !crate::fetch::plain_https(url) {
        return Err(format!("{url}: a catalog is added by its https address"));
    }
    let host = url["https://".len()..].split('/').next().unwrap_or("").to_ascii_lowercase();
    if host.contains(':') || host.split('.').count() < 2
        || !host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.') {
        return Err(format!("{url}: not a plain host name"));
    }
    Ok(host)
}

fn slug_for(state: &Path, url: &str) -> Result<String, String> {
    let host = host_of(url)?;
    for n in 1..100 {
        let s = if n == 1 { host.clone() } else { format!("{host}-{n}") };
        if !state.join(DIR).join(&s).exists() {
            return Ok(s);
        }
    }
    Err(format!("{host}: too many catalogs from one host"))
}

/// Start following a catalog whose `root.toml` text the person approved.
pub fn add(state: &Path, url: &str, root_text: &str, machine: bool, uid: u32, now: u64) -> Result<Cat, String> {
    let root = oc::Root::parse(root_text)?;
    let sha = oc::hex(&<sha2::Sha256 as sha2::Digest>::digest(root_text.as_bytes()));
    for c in all(state).iter().filter(|c| c.third_party()) {
        let s = c.source.as_ref().unwrap();
        if s.root_sha256 == sha && c.visible_to(uid) && (s.machine || !machine) {
            return Err(format!("{} is added already ({})", root.name, c.slug));
        }
    }
    let slug = slug_for(state, url)?;
    let dir = state.join(DIR).join(&slug);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    oc::Client::init(&dir, root_text)?;
    let src = Source { url: url.into(), root_sha256: sha, machine, added_by: uid, time: now };
    write_atomic(&dir.join(SOURCE), toml::to_string(&src).map_err(|e| e.to_string())?.as_bytes())?;
    Ok(Cat { slug, client: oc::Client::third_party(&dir), source: Some(src) })
}

/// Stop following a catalog. Its installed apps stay; their updates stop.
pub fn remove(state: &Path, slug: &str) -> Result<PathBuf, String> {
    if slug.is_empty() || slug.contains('/') || slug.starts_with('.') {
        return Err(format!("{slug:?}: not a catalog"));
    }
    let dir = state.join(DIR).join(slug);
    if !dir.join(SOURCE).is_file() {
        return Err(format!("{slug}: not a catalog this machine follows"));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Check, from this machine, each publisher domain not checked before: the
/// TXT record `_atrium-publisher.<domain>`, else
/// `https://<domain>/.well-known/atrium-publisher`, must name the publisher.
/// Returns the domains still unproven, with why.
pub fn prove(cat: &Cat, local: &oc::Local, txt: &mut dyn FnMut(&str) -> Result<Vec<String>, String>,
             get: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>, now: u64) -> Result<Vec<String>, String> {
    let mut proofs = cat.proofs();
    // Drop proofs of domains a publisher no longer claims.
    proofs.proofs.retain(|x| local.publishers.iter().any(|p| p.id == x.publisher && p.domains.contains(&x.domain)));
    let mut missing = Vec::new();
    for p in &local.publishers {
        for d in &p.domains {
            if proofs.proofs.iter().any(|x| x.publisher == p.id && x.domain == *d) {
                continue;
            }
            let by_txt = txt(&publisher::proof_txt_name(d)).map(|v| publisher::proof_ok(&v.join("\n"), &p.id));
            let how = if by_txt == Ok(true) {
                Some("txt")
            } else {
                match get(&publisher::proof_url(d)) {
                    Ok(b) if publisher::proof_ok(&String::from_utf8_lossy(&b), &p.id) => Some("well-known"),
                    Ok(_) => { missing.push(format!("{d} ({}): its proof names someone else", p.name)); None }
                    Err(e) => { missing.push(format!("{d} ({}): no proof ({e})", p.name)); None }
                }
            };
            if let Some(how) = how {
                proofs.proofs.push(Proof { publisher: p.id.clone(), domain: d.clone(), how: how.into(), time: now });
            }
        }
    }
    cat.save_proofs(&proofs)?;
    Ok(missing)
}

pub fn listed(c: &Cat) -> Listed {
    let l = c.client.local().unwrap_or_default();
    let s = c.source.clone();
    let proofs = c.proofs();
    Listed {
        slug: c.slug.clone(), name: if l.name.is_empty() { c.client.root().map(|r| r.name).unwrap_or_default() } else { l.name.clone() },
        url: s.as_ref().map(|s| s.url.clone()).unwrap_or_default(), third_party: c.third_party(),
        machine: s.as_ref().is_none_or(|s| s.machine), added_by: s.as_ref().map(|s| s.added_by).unwrap_or(0),
        releases: l.releases.iter().filter(|r| c.proven(&l, r)).count(),
        publishers: l.publishers.iter().map(|p| {
            let ok: Vec<&str> = p.domains.iter().filter(|d| proofs.proofs.iter().any(|x| x.publisher == p.id && x.domain == **d))
                .map(String::as_str).collect();
            format!("{} ({}; proven: {})", p.name, p.id, if ok.is_empty() { "none".into() } else { ok.join(", ") })
        }).collect(),
    }
}

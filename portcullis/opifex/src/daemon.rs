//! opifexd's work (opifex.md §6.6, §7; build step C2). The binary
//! (`src/bin/opifexd.rs`) only listens; everything it does is here, so the
//! tests run it without a socket.
//!
//! opifexd is the privileged part. It never parses an archive: `opifex-check`
//! does, sandboxed. It never touches the network (C4 adds `opifex-fetch`).
//! What it does itself is what has to be right: the signature, every file
//! against the signed index (hashed as it copies), the swap into
//! `apps/<id>`, and the installed record.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use opifex_catalog as oc;
use sha2::{Digest, Sha256};

use crate::app;
use crate::fetch;
use crate::updates::{self, Action, Pending, PrefStore};
use crate::installed::{Record, Store};
use crate::proto::{Described, Request, Response};

pub struct Daemon {
    /// The Atrium state root (`/var/lib/atrium`): `apps/`, `opifex/`.
    pub root: PathBuf,
    /// Opifex's own state (`/var/db/atrium/opifex`): `catalog/`, `dl/`,
    /// `unpack/`, `installed.toml`.
    pub state: PathBuf,
    /// The system's root.toml, pinned on first sync.
    pub root_toml: PathBuf,
    /// The worker binary, and whether it may sandbox (false on host tests).
    pub check_bin: PathBuf,
    pub sandbox: bool,
    pub worker_uid: u32,
    /// The install sheet's uid (`_opifexsheet`), which may send
    /// `InstallApproved`; None: nobody but root installs.
    pub sheet_uid: Option<u32>,
    pub trust: portcullis_trust::Trust,
    pub require_content: bool,
    /// How to start the fetcher (`fetch::FETCH_CMD`), and where the catalog
    /// is served until it names its own locations (`fetch::CATALOG_URL`).
    pub fetch_cmd: Vec<String>,
    pub catalog_url: String,
    /// praecod's socket, for the notice about updates waiting on a person
    /// (opifexd posts as a system poster: `PRAECO_SYSTEM_POSTERS=0:Updates`).
    pub praeco: Option<PathBuf>,
    /// Installed font packages and their pin list
    /// (`/usr/share/fonts/atrium-extra`, font.rs).
    pub fonts: PathBuf,
    /// Where side-loaded bundles' libraries are read from (`/`).
    pub sysroot: PathBuf,
    /// One change at a time.
    pub lock: Mutex<()>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Daemon {
    pub fn catalog(&self) -> oc::Client { oc::Client::open(&self.state.join("catalog")) }
    pub fn store(&self) -> Store { Store::new(&self.state.join("installed.toml")) }
    pub fn downloads(&self) -> PathBuf { self.state.join("dl") }

    pub fn handle(&self, uid: u32, r: Request) -> Response {
        let admin = uid == 0;
        let res = match r {
            Request::Installed => self.store().load().map(|i| Response::Installed { apps: i.apps }),
            Request::Describe { id } => self.describe(&id).map(|app| Response::Described { app }),
            Request::Search { text } => self.search(&text).map(|ids| Response::Found { ids }),
            Request::Updates => self.updates().map(|items| {
                let p = self.prefs().load();
                Response::Updates { items, last_sync: p.last_sync, last_error: p.last_error }
            }),
            Request::SetUpdates { id, care } => self.set_care(uid, &id, &care).map(|_| Response::Ok),
            Request::InstallApproved { id, manifest_sha256, for_uid } if admin || Some(uid) == self.sheet_uid =>
                self.locked(|| self.install_catalog(&id, Some(&manifest_sha256), for_uid)),
            _ if !admin => Err("only an administrator, or a person on the install sheet, may change what is installed".into()),
            Request::InstallApproved { .. } => unreachable!("matched above"),
            Request::Install { id } => self.locked(|| self.install_catalog(&id, None, uid)),
            Request::InstallLocal { path } => self.locked(|| self.install_local(Path::new(&path), uid)),
            Request::Uninstall { id } => self.locked(|| self.uninstall(&id)),
            Request::Rollback { id } => self.locked(|| self.rollback(&id)),
            Request::Sync { from } => self.locked(|| self.sync(Path::new(&from))),
            Request::Tick => self.tick(true),
        };
        res.unwrap_or_else(|message| Response::Error { message })
    }

    fn locked(&self, f: impl FnOnce() -> Result<Response, String>) -> Result<Response, String> {
        let _g = self.lock.lock().map_err(|_| "opifexd: lock poisoned".to_string())?;
        f()
    }

    // ── questions ───────────────────────────────────────────────────────────

    pub fn describe(&self, id: &str) -> Result<Described, String> {
        let local = self.catalog().local()?;
        let r = local.latest(id, portcullis_toml::layer::host_arch())
            .ok_or_else(|| format!("{id}: not in the catalog"))?;
        let v: toml::Value = r.manifest.parse().map_err(|e| format!("{id}: {e}"))?;
        let app = v.get("app").or_else(|| v.get("font")).or_else(|| v.get("layer"));
        let s = |k: &str| app.and_then(|a| a.get(k)).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let plain = |c: String| c.split('=').next().unwrap_or("").to_string();
        let capabilities: Vec<String> = updates::caps(&r.manifest).into_iter().map(plain).collect();
        let rec = self.store().get(id)?;
        let added = match &rec {
            Some(_) => {
                let have = fs::read_to_string(self.root.join("apps").join(id).join("atrium.toml")).unwrap_or_default();
                updates::decide(&have, &r.manifest, true, None).1.into_iter().map(plain).collect()
            }
            None => Vec::new(),
        };
        Ok(Described {
            id: id.into(), kind: r.kind.clone(), name: s("name"), description: s("description"),
            latest: r.version.clone(), publisher: r.signer.clone(), catalog: local.name.clone(),
            capabilities, archive_size: r.archive_size,
            installed: rec.map(|x| x.version),
            manifest_sha256: oc::hex(&Sha256::digest(r.manifest.as_bytes())), added,
        })
    }

    pub fn search(&self, text: &str) -> Result<Vec<String>, String> {
        let t = text.to_lowercase();
        let local = self.catalog().local()?;
        let mut ids: Vec<String> = oc::by_id(&local).into_iter()
            .filter(|(id, rs)| id.to_lowercase().contains(&t) || rs.iter().any(|r| r.manifest.to_lowercase().contains(&t)))
            .map(|(id, _)| id).collect();
        ids.dedup();
        Ok(ids)
    }

    // ── changes ─────────────────────────────────────────────────────────────

    fn sync(&self, from: &Path) -> Result<Response, String> {
        let c = self.catalog();
        if !c.dir.join(oc::ROOT).is_file() {
            let text = fs::read_to_string(&self.root_toml).map_err(|e| format!("{}: {e}", self.root_toml.display()))?;
            oc::Client::init(&c.dir, &text)?;
        }
        let from = from.to_string_lossy().to_string();
        let l = if from.is_empty() || from.starts_with("https://") {
            // Over the network: the given base, else where the catalog says it
            // lives, else the default.
            let base = if !from.is_empty() { from } else {
                c.local()?.locations.first().cloned().unwrap_or_else(|| self.catalog_url.clone())
            };
            let net = std::cell::RefCell::new(fetch::Fetcher::new(self.fetch_cmd.clone()));
            let get = |p: &str| net.borrow_mut().get(&fetch::join(&base, p), fetch::CATALOG_FILE_MAX);
            c.update(&get, now())?
        } else {
            let src = PathBuf::from(from);
            let get = move |p: &str| fs::read(src.join(p)).map_err(|e| format!("{p}: {e}"));
            c.update(&get, now())?
        };
        Ok(Response::Synced { entries: l.size, releases: l.releases.len() })
    }

    /// Run the worker on an archive file: a fresh folder, owned by the
    /// worker's uid, filled by it.
    fn unpack(&self, archive: &Path, size: u64) -> Result<PathBuf, String> {
        let base = self.state.join("unpack");
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let dir = base.join(format!("job-{}-{}", std::process::id(), now()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if self.sandbox {
            std::os::unix::fs::chown(&dir, Some(self.worker_uid), Some(self.worker_uid)).map_err(|e| e.to_string())?;
        }
        let f = fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        let mut cmd = Command::new(&self.check_bin);
        cmd.arg("unpack").arg(size.to_string()).arg("--into").arg(&dir)
            .arg("--uid").arg(self.worker_uid.to_string())
            .stdin(Stdio::from(f)).stdout(Stdio::piped()).stderr(Stdio::piped()).env_clear();
        if !self.sandbox {
            cmd.arg("--no-sandbox");
        }
        let out = cmd.output().map_err(|e| format!("{}: {e}", self.check_bin.display()))?;
        if !out.status.success() {
            let _ = fs::remove_dir_all(&dir);
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(dir)
    }

    fn finish(&self, m: &portcullis_toml::Manifest, stage: &Path, rec: Record) -> Result<Response, String> {
        let dest = app::commit(&self.root, &m.app.id, stage)?;
        let _ = dest;
        let (id, version) = (rec.id.clone(), rec.version.clone());
        self.store().put(rec)?;
        Ok(Response::Done { id, version })
    }

    /// Install the newest catalog release of `id` for `uid`. With `approved`,
    /// only if that release's manifest is the one the sheet showed.
    // ── updates (C5) ────────────────────────────────────────────────────────

    pub fn prefs(&self) -> PrefStore { PrefStore::new(&self.state) }

    /// Whether `signer` publishes for the same publisher as `have`: from this
    /// catalog, and the same key or, in the Atrium catalog where the platform
    /// is the only publisher, a platform key of the current root (a rotated
    /// platform key is still Atrium).
    fn same_publisher(&self, have: &crate::installed::Record, signer: &str, catalog: &str, root: &oc::Root) -> bool {
        if have.source != format!("catalog:{catalog}") {
            return false;
        }
        let platform = |k: &str| root.key(k).is_some_and(|k| k.roles.iter().any(|r| r == "platform"));
        have.publisher == signer || (platform(signer) && (platform(&have.publisher) || root.key(&have.publisher).is_none()))
    }

    /// Installed apps with a newer release, and what happens to each.
    pub fn updates(&self) -> Result<Vec<Pending>, String> {
        let c = self.catalog();
        if !c.dir.join(oc::ROOT).is_file() {
            return Ok(Vec::new());
        }
        let (local, root) = (c.local()?, c.root()?);
        let prefs = self.prefs().load();
        let mut out = Vec::new();
        for rec in self.store().load()?.apps {
            let Some(r) = local.latest(&rec.id, portcullis_toml::layer::host_arch()) else { continue };
            if r.kind != rec.kind || oc::version_cmp(&r.version, &rec.version) != std::cmp::Ordering::Greater {
                continue;
            }
            let same = self.same_publisher(&rec, &r.signer, &local.name, &root);
            let care = prefs.care.get(&rec.id).map(String::as_str);
            let (action, added) = match rec.kind.as_str() {
                "app" => {
                    let have = fs::read_to_string(self.root.join("apps").join(&rec.id).join("atrium.toml")).unwrap_or_default();
                    updates::decide(&have, &r.manifest, same, care)
                }
                _ if !same => (Action::Refused, Vec::new()),
                // Fonts hold no power: like an app that asks for nothing.
                "font" if care != Some("notify") => (Action::Background, Vec::new()),
                // Layers, drivers, firmware: an administrator applies them.
                _ => (Action::Notify, Vec::new()),
            };
            let why = match (action, rec.source.as_str()) {
                (Action::Refused, "side-load") => "side-loaded: update it by side-loading".to_string(),
                (Action::Refused, _) => "another publisher".to_string(),
                _ => String::new(),
            };
            let name = updates::name(&r.manifest).unwrap_or_else(|| rec.id.clone());
            out.push(Pending { id: rec.id.clone(), name, from: rec.version.clone(), to: r.version.clone(), action, added, why });
        }
        Ok(out)
    }

    fn set_care(&self, uid: u32, id: &str, care: &str) -> Result<(), String> {
        let rec = self.store().get(id)?.ok_or_else(|| format!("{id}: not installed"))?;
        if uid != 0 && uid != rec.installed_by {
            return Err("only an administrator or the person who installed it may change how it updates".into());
        }
        let ps = self.prefs();
        let mut p = ps.load();
        match care {
            "notify" => { p.care.insert(id.into(), "notify".into()); }
            "auto" => { p.care.remove(id); }
            _ => return Err("care: \"notify\" or \"auto\"".into()),
        }
        ps.save(&p)
    }

    /// The daily round: sync (over the network when `net`), install what may
    /// go in the background (each for the person who installed it), announce
    /// what waits on a person, once per version.
    pub fn tick(&self, net: bool) -> Result<Response, String> {
        let ps = self.prefs();
        if net {
            let r = self.locked(|| self.sync(Path::new("")));
            let mut p = ps.load();
            p.last_sync = now();
            p.last_error = match &r { Err(e) => e.clone(), Ok(_) => String::new() };
            ps.save(&p)?;
            r?;
        }
        let mut installed = Vec::new();
        let mut waiting = Vec::new();
        for u in self.updates()? {
            match u.action {
                Action::Background => {
                    let by = self.store().get(&u.id)?.map(|r| r.installed_by).unwrap_or(0);
                    match self.locked(|| self.install_catalog(&u.id, None, by)) {
                        Ok(_) => installed.push(format!("{} {}", u.id, u.to)),
                        Err(e) => eprintln!("opifexd: update {} {}: {e}", u.id, u.to),
                    }
                }
                Action::Notify | Action::Ask => waiting.push(u),
                Action::Refused => eprintln!("opifexd: update {} {} refused: {}", u.id, u.to, u.why),
            }
        }
        let mut p = ps.load();
        let new: Vec<Pending> = waiting.iter().filter(|u| !p.announced.contains(&format!("{} {}", u.id, u.to))).cloned().collect();
        if !new.is_empty() {
            let (title, body) = updates::notice(&new);
            if let Some(sock) = &self.praeco {
                if let Err(e) = praeco::client::post(sock, praeco::proto::urgency::LOW, &title, &body) {
                    eprintln!("opifexd: praecod: {e}");
                }
            }
            p.announced.extend(new.iter().map(|u| format!("{} {}", u.id, u.to)));
            ps.save(&p)?;
        }
        Ok(Response::Ticked { installed, waiting: waiting.len() })
    }

    /// Fetch `r`'s archive from its URLs into `to`, checked by size and hash
    /// before it is kept.
    fn download(&self, r: &oc::Release, to: &Path) -> Result<(), String> {
        let mut net = fetch::Fetcher::new(self.fetch_cmd.clone());
        let mut why = Vec::new();
        for url in &r.urls {
            match net.get(url, r.archive_size) {
                Ok(b) if b.len() as u64 == r.archive_size && oc::hex(&Sha256::digest(&b)) == r.archive_sha256 => {
                    fs::create_dir_all(self.downloads()).map_err(|e| e.to_string())?;
                    let tmp = to.with_extension("part");
                    fs::write(&tmp, &b).and_then(|_| fs::rename(&tmp, to)).map_err(|e| format!("{}: {e}", to.display()))?;
                    return Ok(());
                }
                Ok(_) => why.push(format!("{url}: not the archive the catalog names")),
                Err(e) => why.push(e),
            }
        }
        Err(format!("{}: download failed: {}", r.id, if why.is_empty() { "no URLs".into() } else { why.join("; ") }))
    }

    fn install_catalog(&self, id: &str, approved: Option<&str>, uid: u32) -> Result<Response, String> {
        let c = self.catalog();
        let local = c.local()?;
        let root = c.root()?;
        let r = local.latest(id, portcullis_toml::layer::host_arch())
            .ok_or_else(|| format!("{id}: not in the catalog for this machine"))?.clone();
        if !["app", "font", "layer"].contains(&r.kind.as_str()) {
            return Err(format!("{id}: a {} (not installed by opifexd yet)", r.kind));
        }
        // The sheet asks for anyone, and only apps and fonts are anyone's to
        // install (§7.6, decision 11); the rest is an administrator's.
        if approved.is_some() && uid != 0 && !["app", "font"].contains(&r.kind.as_str()) {
            return Err(format!("{id}: a {} is installed by an administrator", r.kind));
        }
        if let Some(want) = approved {
            if oc::hex(&Sha256::digest(r.manifest.as_bytes())) != want {
                return Err(format!("{id}: the catalog changed since the sheet was shown; ask again"));
            }
        }
        let signer = root.key(&r.signer).ok_or("signer gone from root.toml")?.pem.clone();
        if let Some(have) = self.store().get(id)? {
            if !self.same_publisher(&have, &r.signer, &local.name, &root) {
                return Err(format!("{id}: this release is from another publisher than the installed one; not installed"));
            }
        }
        // The archive: fetched by C4; for now it must be in the download folder.
        let archive = self.downloads().join(format!("{}.insula", r.archive_sha256));
        if !archive.is_file() {
            self.download(&r, &archive)?;
        }
        let mut f = fs::File::open(&archive).map_err(|_| format!("{id}: archive not downloaded ({})", archive.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0u64;
        loop {
            let k = f.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 { break; }
            h.update(&buf[..k]);
            n += k as u64;
        }
        if n != r.archive_size || oc::hex(&h.finalize()) != r.archive_sha256 {
            let _ = fs::remove_file(&archive);
            return Err(format!("{id}: the archive is not the one the catalog names (removed)"));
        }
        let dir = self.unpack(&archive, n)?;
        let res = (|| {
            // The manifest unpacked must be the one the catalog carries.
            let got = fs::read_to_string(dir.join("atrium.toml")).map_err(|e| format!("atrium.toml: {e}"))?;
            if got != r.manifest {
                return Err(format!("{id}: the archive's manifest differs from the catalog's"));
            }
            let record = |version: &str, content: &str| Record {
                id: id.into(), version: version.into(), kind: r.kind.clone(), publisher: r.signer.clone(),
                source: format!("catalog:{}", local.name), archive_sha256: r.archive_sha256.clone(),
                content: content.into(), installed_by: uid, time: now(), verified_rps: Vec::new(),
            };
            match r.kind.as_str() {
                "font" => {
                    let (m, _) = crate::font::install(&dir, &self.fonts, std::slice::from_ref(&signer))?;
                    self.store().put(record(&m.font.version, &m.font.content))?;
                    return Ok(Response::Done { id: id.into(), version: m.font.version });
                }
                "layer" => {
                    let o = crate::layer::InstallOpts { root: &self.root, keys: std::slice::from_ref(&signer),
                                                        arch: portcullis_toml::layer::host_arch(), allow_unsigned: false };
                    let (m, _) = crate::layer::install(&dir, &o)?;
                    self.store().put(record(&m.layer.version, &m.layer.content))?;
                    return Ok(Response::Done { id: id.into(), version: m.layer.version });
                }
                _ => {}
            }
            let opts = app::Opts { root: &self.root, trust: &self.trust, allow_unsigned: false,
                                   require_content: true, catalog_key: Some(&signer) };
            let (m, stage) = app::stage(&dir, &opts)?;
            // Catalog packages carry their own closure (no install-time resolving).
            if !stage.join(m.entry()).is_file() {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("{id}: entry {} missing", m.entry()));
            }
            let rec = Record {
                id: m.app.id.clone(), version: m.app.version.clone(), kind: r.kind.clone(), publisher: r.signer.clone(),
                source: format!("catalog:{}", local.name), archive_sha256: r.archive_sha256.clone(),
                content: m.bundle.as_ref().and_then(|b| b.content.clone()).unwrap_or_default(),
                installed_by: uid, time: now(), verified_rps: Vec::new(),
            };
            self.finish(&m, &stage, rec)
        })();
        let _ = fs::remove_dir_all(&dir);
        res
    }

    fn install_local(&self, path: &Path, uid: u32) -> Result<Response, String> {
        let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (dir, temp) = if meta.is_file() {
            (self.unpack(path, meta.len())?, true)
        } else {
            (path.to_path_buf(), false)
        };
        let res = (|| {
            let opts = app::Opts { root: &self.root, trust: &self.trust, allow_unsigned: false,
                                   require_content: self.require_content, catalog_key: None };
            let (m, stage) = app::stage(&dir, &opts)?;
            let entry = stage.join(m.entry());
            if !entry.is_file() {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("entry {} missing", m.entry()));
            }
            if let Err(e) = crate::runtime::resolve_runtime(&entry, &stage, &self.sysroot) {
                let _ = fs::remove_dir_all(&stage);
                return Err(e);
            }
            let text = fs::read_to_string(dir.join("atrium.toml")).map_err(|e| e.to_string())?;
            let publisher = self.trust.verify_publisher(&dir, &text, portcullis_trust::Demand::PolicyDefault)
                .ok().flatten().map(|h| oc::hex(&h)).unwrap_or_default();
            let rec = Record {
                id: m.app.id.clone(), version: m.app.version.clone(), kind: "app".into(), publisher,
                source: "side-load".into(), archive_sha256: String::new(),
                content: m.bundle.as_ref().and_then(|b| b.content.clone()).unwrap_or_default(),
                installed_by: uid, time: now(), verified_rps: Vec::new(),
            };
            self.finish(&m, &stage, rec)
        })();
        if temp {
            let _ = fs::remove_dir_all(&dir);
        }
        res
    }

    fn uninstall(&self, id: &str) -> Result<Response, String> {
        match self.store().get(id)? {
            Some(r) if r.kind == "font" => crate::font::remove(&self.fonts, id)?,
            // Older versions a workspace still names stay; this one goes,
            // unless a running workspace has it mounted.
            Some(r) if r.kind == "layer" => { crate::layer::remove(&self.root, &format!("{id}@{}", r.version))?; }
            _ => {}
        }
        let rec = self.store().remove(id)?;
        let mut gone = false;
        for p in [self.root.join("apps").join(id), app::state(&self.root).join("prev").join(id)] {
            if fs::symlink_metadata(&p).is_ok() {
                fs::remove_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                gone = true;
            }
        }
        if rec.is_none() && !gone {
            return Err(format!("{id} is not installed"));
        }
        Ok(Response::Done { id: id.into(), version: rec.map(|r| r.version).unwrap_or_default() })
    }

    fn rollback(&self, id: &str) -> Result<Response, String> {
        if let Some(r) = self.store().get(id)? {
            if r.kind != "app" {
                return Err(format!("{id}: only apps keep the version before"));
            }
        }
        let dest = app::rollback(&self.root, id)?;
        let text = fs::read_to_string(dest.join("atrium.toml")).map_err(|e| e.to_string())?;
        let m = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("{e:?}"))?;
        // The record follows the tree that is now installed.
        let mut rec = self.store().get(id)?.ok_or_else(|| format!("{id}: no record"))?;
        rec.version = m.app.version.clone();
        rec.content = m.bundle.as_ref().and_then(|b| b.content.clone()).unwrap_or_default();
        rec.time = now();
        self.store().put(rec)?;
        Ok(Response::Done { id: id.into(), version: m.app.version })
    }
}


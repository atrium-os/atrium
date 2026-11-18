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
use crate::catalogs::{self, Cat};
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
    /// The fetcher for everything outside the Atrium catalog: catalogs people
    /// added, their archives, domain proofs (`fetch::FETCH_ANY_CMD`, C7).
    pub fetch_any_cmd: Vec<String>,
    /// praecod's socket, for the notice about updates waiting on a person
    /// (opifexd posts as a system poster: `PRAECO_SYSTEM_POSTERS=0:Updates`).
    pub praeco: Option<PathBuf>,
    /// Installed font packages and their pin list
    /// (`/usr/share/fonts/atrium-extra`, font.rs).
    pub fonts: PathBuf,
    /// The system root drivers and firmware are installed under (`/`,
    /// driver.rs).
    pub system: PathBuf,
    /// Where side-loaded bundles' libraries are read from (`/`).
    pub sysroot: PathBuf,
    /// One change at a time.
    pub lock: Mutex<()>,
}

/// The largest file the sheet installs from (§7.4).
const LOCAL_MAX: u64 = 4 << 30;
/// How long a previewed file waits for the person's press.
const HOLD_SECS: u64 = 15 * 60;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Daemon {
    pub fn catalog(&self) -> oc::Client { oc::Client::open(&self.state.join("catalog")) }
    pub fn store(&self) -> Store { Store::new(&self.state.join("installed.toml")) }
    pub fn downloads(&self) -> PathBuf { self.state.join("dl") }

    /// [`Daemon::handle`], for a request that may have come with a file
    /// (fdpass): only `PreviewLocal` takes one, and only it needs one.
    pub fn handle_with(&self, uid: u32, r: Request, file: Option<std::os::fd::OwnedFd>) -> Response {
        match (r, file) {
            (Request::PreviewLocal { for_uid }, Some(f)) if uid == 0 || Some(uid) == self.sheet_uid =>
                self.preview_local(f, for_uid).unwrap_or_else(|message| Response::Error { message }),
            (Request::PreviewLocal { .. }, Some(_)) => Response::Error { message: "only the install sheet previews a file".into() },
            (r, _) => self.handle(uid, r),
        }
    }

    pub fn handle(&self, uid: u32, r: Request) -> Response {
        let admin = uid == 0;
        let res = match r {
            Request::Installed => self.store().load().map(|i| Response::Installed { apps: i.apps }),
            Request::Describe { id, for_uid } => {
                let who = match for_uid { Some(u) if admin || Some(uid) == self.sheet_uid => u, _ => uid };
                self.describe(&id, who).map(|app| Response::Described { app })
            }
            Request::Search { text } => self.search(&text, uid).map(|ids| Response::Found { ids }),
            Request::Catalogs => Ok(Response::Catalogs {
                items: catalogs::all(&self.state).iter().filter(|c| c.visible_to(uid)).map(catalogs::listed).collect() }),
            Request::PreviewCatalog { url } => self.preview_catalog(&url),
            Request::AddCatalog { url, root_sha256, machine, for_uid } if admin || Some(uid) == self.sheet_uid => {
                if machine && !admin {
                    Err("a catalog for everyone is added by an administrator".into())
                } else {
                    self.locked(|| self.add_catalog(&url, &root_sha256, machine, if admin && machine { 0 } else { for_uid }))
                }
            }
            Request::RemoveCatalog { slug } => self.locked(|| {
                let cats = catalogs::all(&self.state);
                let c = cats.iter().find(|c| c.third_party() && c.slug == slug).ok_or_else(|| format!("{slug}: not followed"))?;
                let s = c.source.as_ref().unwrap();
                if !admin && (s.machine || s.added_by != uid) {
                    return Err("only an administrator, or the person who added it, may remove a catalog".into());
                }
                catalogs::remove(&self.state, &slug)?;
                Ok(Response::Ok)
            }),
            Request::Updates => self.updates().map(|items| {
                let p = self.prefs().load();
                Response::Updates { items, last_sync: p.last_sync, last_error: p.last_error }
            }),
            Request::SetUpdates { id, care } => self.set_care(uid, &id, &care).map(|_| Response::Ok),
            Request::InstallApproved { id, manifest_sha256, for_uid } if admin || Some(uid) == self.sheet_uid =>
                self.locked(|| self.install_catalog(&id, Some(&manifest_sha256), for_uid)),
            Request::PreviewLocal { .. } => Err("a preview comes with the file itself".into()),
            Request::InstallLocalApproved { token, manifest_sha256, for_uid } if admin || Some(uid) == self.sheet_uid =>
                self.locked(|| self.install_held(&token, &manifest_sha256, for_uid)),
            _ if !admin => Err("only an administrator, or a person on the install sheet, may change what is installed".into()),
            Request::InstallApproved { .. } | Request::AddCatalog { .. } | Request::InstallLocalApproved { .. } =>
                unreachable!("matched above"),
            Request::Install { id } => self.locked(|| self.install_catalog(&id, None, uid)),
            Request::InstallLocal { path } => self.locked(|| self.install_local(Path::new(&path), uid, None)),
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

    /// The catalog `id` comes from for `uid`, and its newest release there:
    /// the one it was installed from, if installed (an id is never replaced
    /// from another catalog); else the Atrium catalog, then the others this
    /// person sees, in order. Third-party releases only once this machine
    /// checked their publisher's domain.
    pub fn find(&self, id: &str, uid: u32) -> Result<(Cat, oc::Local, oc::Release), String> {
        let arch = portcullis_toml::layer::host_arch();
        let rec = self.store().get(id)?;
        let mut cats = catalogs::all(&self.state);
        if let Some(rec) = rec.filter(|r| r.source.starts_with("catalog:")) {
            let i = cats.iter().position(|c| c.client.local().is_ok_and(|l| c.tag(&l) == rec.source))
                .ok_or_else(|| format!("{id}: the catalog it came from ({}) is no longer followed", rec.source))?;
            let c = cats.swap_remove(i);
            let local = c.client.local()?;
            let r = c.offer(&local, id, arch).ok_or_else(|| format!("{id}: not in {} any more", rec.source))?;
            return Ok((c, local, r));
        }
        for c in cats.into_iter().filter(|c| c.visible_to(uid)) {
            let local = c.client.local()?;
            if let Some(r) = c.offer(&local, id, arch) {
                return Ok((c, local, r));
            }
        }
        Err(format!("{id}: not in the catalog"))
    }

    pub fn describe(&self, id: &str, uid: u32) -> Result<Described, String> {
        let (cat, local, r) = self.find(id, uid)?;
        let r = &r;
        let pubrec = local.publishers.iter().find(|p| !r.publisher.is_empty() && p.id == r.publisher);
        let v: toml::Value = r.manifest.parse().map_err(|e| format!("{id}: {e}"))?;
        let app = ["app", "font", "driver", "firmware", "layer"].iter().find_map(|k| v.get(*k));
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
        let recovered = match (&rec, pubrec) { (Some(x), Some(p)) => p.recoveries > x.recoveries, _ => false };
        Ok(Described {
            id: id.into(), kind: r.kind.clone(), name: s("name"), description: s("description"),
            latest: r.version.clone(),
            publisher: if r.publisher.is_empty() { r.signer.clone() } else { r.publisher.clone() },
            catalog: local.name.clone(),
            capabilities, archive_size: r.archive_size,
            installed: rec.map(|x| x.version),
            manifest_sha256: oc::hex(&Sha256::digest(r.manifest.as_bytes())), added,
            publisher_name: pubrec.map(|p| p.name.clone()).unwrap_or_default(),
            publisher_domains: pubrec.map(|p| p.domains.clone()).unwrap_or_default(),
            third_party: cat.third_party(), recovered, local: false,
        })
    }

    pub fn search(&self, text: &str, uid: u32) -> Result<Vec<String>, String> {
        let t = text.to_lowercase();
        let mut ids = Vec::new();
        for c in catalogs::all(&self.state).iter().filter(|c| c.visible_to(uid)) {
            let local = c.client.local()?;
            ids.extend(oc::by_id(&local).into_iter()
                .filter(|(id, rs)| (id.to_lowercase().contains(&t) || rs.iter().any(|r| r.manifest.to_lowercase().contains(&t)))
                        && rs.iter().any(|r| c.proven(&local, r)))
                .map(|(id, _)| id));
        }
        ids.sort();
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
        let everything = from.is_empty();
        // The Atrium catalog first; a failure there is reported after the
        // others have had their turn.
        let atrium: Result<oc::Local, String> = (|| Ok(if from.is_empty() || from.starts_with("https://") {
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
        }))();
        // What the catalog's drivers are for, for dispensatord's lookup.
        if let Ok(l) = &atrium {
            if let Err(e) = crate::driver::write_idx(&self.state, l, portcullis_toml::layer::host_arch()) {
                eprintln!("opifexd: {e}");
            }
        }
        // The catalogs people added, each on its own; one failing stops none.
        let mut others = Vec::new();
        if everything {
            for cat in catalogs::all(&self.state).into_iter().filter(Cat::third_party) {
                let line = match self.sync_third(&cat) {
                    Ok(s) => format!("{}: {s}", cat.slug),
                    Err(e) => format!("{}: {e}", cat.slug),
                };
                eprintln!("opifexd: {line}");
                others.push(line);
            }
        }
        let l = atrium.map_err(|e| if others.is_empty() { e } else { format!("{e} (others: {})", others.join("; ")) })?;
        Ok(Response::Synced { entries: l.size, releases: l.releases.len(), others })
    }

    /// Sync one added catalog through the open fetcher, then check the
    /// domains of publishers this machine hasn't checked yet.
    fn sync_third(&self, cat: &Cat) -> Result<String, String> {
        let src = cat.source.as_ref().ok_or("not an added catalog")?;
        let base = cat.client.local()?.locations.first().cloned().unwrap_or_else(|| src.url.clone());
        let net = std::cell::RefCell::new(fetch::Fetcher::new(self.fetch_any_cmd.clone()));
        let get = |p: &str| net.borrow_mut().get(&fetch::join(&base, p), fetch::CATALOG_FILE_MAX);
        let l = cat.client.update(&get, now())?;
        let missing = catalogs::prove(cat, &l, &mut |n| net.borrow_mut().txt(n),
                                      &mut |u| net.borrow_mut().get(u, fetch::PROOF_MAX), now())?;
        let offered = l.releases.iter().filter(|r| cat.proven(&l, r)).count();
        let mut s = format!("{} entries, {offered} releases offered, {} publishers", l.size, l.publishers.len());
        if !missing.is_empty() {
            s += &format!("; not proven: {}", missing.join("; "));
        }
        Ok(s)
    }

    fn preview_catalog(&self, url: &str) -> Result<Response, String> {
        catalogs::host_of(url)?;
        let mut net = fetch::Fetcher::new(self.fetch_any_cmd.clone());
        let text = String::from_utf8(net.get(&fetch::join(url, oc::ROOT), fetch::CATALOG_FILE_MAX)?)
            .map_err(|_| "root.toml: not UTF-8")?;
        let root = oc::Root::parse(&text)?;
        if root.name.eq_ignore_ascii_case(&self.catalog().root().map(|r| r.name).unwrap_or_else(|_| "Atrium".into())) {
            return Err(format!("{url}: calls itself {:?}, the name of the system's catalog", root.name));
        }
        Ok(Response::CatalogPreview { url: url.into(), name: root.name, root_sha256: oc::hex(&Sha256::digest(text.as_bytes())),
                                      warning: catalogs::WARNING.into() })
    }

    fn add_catalog(&self, url: &str, sha: &str, machine: bool, uid: u32) -> Result<Response, String> {
        let Response::CatalogPreview { root_sha256, name, .. } = self.preview_catalog(url)? else { unreachable!() };
        if root_sha256 != sha {
            return Err(format!("{url}: its root.toml changed since it was shown; look again"));
        }
        let mut net = fetch::Fetcher::new(self.fetch_any_cmd.clone());
        let text = String::from_utf8(net.get(&fetch::join(url, oc::ROOT), fetch::CATALOG_FILE_MAX)?).map_err(|_| "root.toml")?;
        if oc::hex(&Sha256::digest(text.as_bytes())) != sha {
            return Err(format!("{url}: its root.toml changed since it was shown; look again"));
        }
        let cat = catalogs::add(&self.state, url, &text, machine, uid, now())?;
        match self.sync_third(&cat) {
            Ok(summary) => Ok(Response::CatalogAdded { slug: cat.slug.clone(), name, summary }),
            Err(e) => {
                let _ = catalogs::remove(&self.state, &cat.slug);
                Err(format!("{url}: not added: {e}"))
            }
        }
    }

    /// Run the worker on an archive file: a fresh folder, owned by the
    /// worker's uid, filled by it.
    fn unpack(&self, archive: &Path, size: u64) -> Result<PathBuf, String> {
        let base = self.state.join("unpack");
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        // One folder per job, even within a second (a driver unpacks its
        // firmware while its own folder is still in use).
        static JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = base.join(format!("job-{}-{}-{n}", std::process::id(), now()));
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
    fn same_publisher(&self, have: &crate::installed::Record, r: &oc::Release, cat: &Cat, local: &oc::Local, root: &oc::Root) -> bool {
        if have.source != cat.tag(local) {
            return false;
        }
        // A publisher record: the same publisher id, whatever key signs now.
        if !r.publisher.is_empty() {
            return have.publisher == r.publisher;
        }
        let signer = r.signer.as_str();
        let platform = |k: &str| root.key(k).is_some_and(|k| k.roles.iter().any(|r| r == "platform"));
        have.publisher == signer || (platform(signer) && (platform(&have.publisher) || root.key(&have.publisher).is_none()))
    }

    /// Installed apps with a newer release, and what happens to each.
    pub fn updates(&self) -> Result<Vec<Pending>, String> {
        let cats = catalogs::all(&self.state);
        let prefs = self.prefs().load();
        let mut out = Vec::new();
        for rec in self.store().load()?.apps {
            // From the catalog it came from; a side-loaded app is compared
            // with the Atrium catalog only to say why it isn't updated.
            let cat = if rec.source == "side-load" { cats.first() } else { catalogs::of_source(&cats, &rec.source) };
            let Some(cat) = cat else { continue };
            if !cat.client.dir.join(oc::ROOT).is_file() {
                continue;
            }
            let (local, root) = (cat.client.local()?, cat.client.root()?);
            let Some(r) = cat.offer(&local, &rec.id, portcullis_toml::layer::host_arch()) else { continue };
            let r = &r;
            if r.kind != rec.kind || oc::version_cmp(&r.version, &rec.version) != std::cmp::Ordering::Greater {
                continue;
            }
            let same = self.same_publisher(&rec, r, cat, &local, &root);
            let recovered = local.publishers.iter().any(|p| p.id == r.publisher && p.recoveries > rec.recoveries);
            let care = prefs.care.get(&rec.id).map(String::as_str);
            let (action, added) = match rec.kind.as_str() {
                "app" => {
                    let have = fs::read_to_string(self.root.join("apps").join(&rec.id).join("atrium.toml")).unwrap_or_default();
                    match updates::decide(&have, &r.manifest, same, care) {
                        // After a key recovery the person is asked again (§7.7).
                        (Action::Background | Action::Notify, a) if same && recovered => (Action::Ask, a),
                        d => d,
                    }
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
                (Action::Ask, _) if recovered => "its publisher's key was replaced by a recovery".to_string(),
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
        let (cat, local, r) = self.find(id, uid).map_err(|e| e.replace("not in the catalog", "not in the catalog for this machine"))?;
        let root = cat.client.root()?;
        let pubrec = local.publishers.iter().find(|p| !r.publisher.is_empty() && p.id == r.publisher).cloned();
        if cat.third_party() && r.kind != "app" {
            return Err(format!("{id}: catalogs people add carry apps only"));
        }
        if !["app", "font", "layer", "driver", "firmware"].contains(&r.kind.as_str()) {
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
        let signer = match &pubrec {
            Some(p) => p.key.clone(),
            None => root.key(&r.signer).ok_or("signer gone from root.toml")?.pem.clone(),
        };
        let (publisher, recoveries) = match &pubrec { Some(p) => (p.id.clone(), p.recoveries), None => (r.signer.clone(), 0) };
        let source = cat.tag(&local);
        if let Some(have) = self.store().get(id)? {
            if !self.same_publisher(&have, &r, &cat, &local, &root) {
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
                id: id.into(), version: version.into(), kind: r.kind.clone(), publisher: publisher.clone(),
                source: source.clone(), archive_sha256: r.archive_sha256.clone(),
                content: content.into(), installed_by: uid, time: now(), verified_rps: Vec::new(), recoveries,
            };
            match r.kind.as_str() {
                "font" => {
                    let (m, _) = crate::font::install(&dir, &self.fonts, std::slice::from_ref(&signer))?;
                    self.store().put(record(&m.font.version, &m.font.content))?;
                    return Ok(Response::Done { id: id.into(), version: m.font.version });
                }
                "driver" => {
                    let m = opifex_catalog::driver::DriverManifest::from_str(&r.manifest)?;
                    // Its firmware first, each from the catalog.
                    for fw in &m.driver.firmware {
                        let have = self.store().get(fw)?.map(|x| x.version);
                        let latest = local.latest(fw, portcullis_toml::layer::host_arch()).map(|x| x.version.clone());
                        if latest.is_none() {
                            return Err(format!("{id}: its firmware {fw} is not in the catalog"));
                        }
                        if have != latest {
                            self.install_catalog(fw, None, uid).map_err(|e| format!("{id}: firmware {fw}: {e}"))?;
                        }
                    }
                    let m = crate::driver::install_driver(&dir, &self.system, std::slice::from_ref(&signer))?;
                    self.store().put(record(&m.driver.version, &m.driver.content))?;
                    eprintln!("opifexd: driver {id} {}: dispensatord takes its devices from its next start", m.driver.version);
                    return Ok(Response::Done { id: id.into(), version: m.driver.version });
                }
                "firmware" => {
                    let m = crate::driver::install_firmware(&dir, &self.system, std::slice::from_ref(&signer))?;
                    self.store().put(record(&m.firmware.version, &m.firmware.content))?;
                    return Ok(Response::Done { id: id.into(), version: m.firmware.version });
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
            // WebAuthn RP ids: only those whose site names this app (§7.3).
            let verified_rps = if crate::rp::rp_ids(&r.manifest).is_empty() { Vec::new() } else {
                let mut net = fetch::Fetcher::new(self.fetch_any_cmd.clone());
                let (ok, dropped) = crate::rp::verify(&r.manifest, &publisher, id, &mut |u| net.get(u, fetch::PROOF_MAX));
                for d in dropped {
                    eprintln!("opifexd: {id}: WebAuthn RP dropped: {d}");
                }
                ok
            };
            let rec = Record {
                id: m.app.id.clone(), version: m.app.version.clone(), kind: r.kind.clone(), publisher: publisher.clone(),
                source: source.clone(), archive_sha256: r.archive_sha256.clone(),
                content: m.bundle.as_ref().and_then(|b| b.content.clone()).unwrap_or_default(),
                installed_by: uid, time: now(), verified_rps, recoveries,
            };
            self.finish(&m, &stage, rec)
        })();
        let _ = fs::remove_dir_all(&dir);
        res
    }

    // ── side-loading through the sheet (§7.4) ───────────────────────────────

    fn held(&self) -> PathBuf { self.state.join("held") }

    /// Forget previews nobody approved within [`HOLD_SECS`].
    fn prune_held(&self) {
        let Ok(rd) = fs::read_dir(self.held()) else { return };
        for e in rd.flatten() {
            let old = e.metadata().ok().and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok()).map_or(true, |age| age.as_secs() > HOLD_SECS);
            if old {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// Copy the file the sheet was handed, check it as a side-load, and hold
    /// it under a token for the person to approve.
    fn preview_local(&self, file: std::os::fd::OwnedFd, for_uid: u32) -> Result<Response, String> {
        use std::io::Read;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        self.prune_held();
        let held = self.held();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&held).map_err(|e| format!("{}: {e}", held.display()))?;
        let mut t = [0u8; 16];
        fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut t)).map_err(|e| format!("/dev/urandom: {e}"))?;
        let token = oc::hex(&t);
        let path = held.join(format!("{token}.insula"));
        let mut dst = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let n = std::io::copy(&mut fs::File::from(file).take(LOCAL_MAX + 1), &mut dst);
        let res = (|| {
            let n = n.map_err(|e| format!("reading the file: {e}"))?;
            if n > LOCAL_MAX {
                return Err(format!("larger than {} GB", LOCAL_MAX >> 30));
            }
            let dir = self.unpack(&path, n)?;
            let app = self.describe_local(&dir, n, for_uid);
            let _ = fs::remove_dir_all(&dir);
            let app = app?;
            fs::write(held.join(format!("{token}.toml")), format!("{}\n{for_uid}\n", app.manifest_sha256))
                .map_err(|e| e.to_string())?;
            Ok(Response::LocalPreview { token: token.clone(), app })
        })();
        if res.is_err() {
            let _ = fs::remove_file(&path);
        }
        res
    }

    /// What the sheet shows for an unpacked side-load: an app signed by a
    /// key on the side-load list, and not one a catalog keeps up to date.
    fn describe_local(&self, dir: &Path, size: u64, uid: u32) -> Result<Described, String> {
        let text = fs::read_to_string(dir.join("atrium.toml")).map_err(|_| "not an app bundle (no atrium.toml)".to_string())?;
        let v: toml::Value = text.parse().map_err(|e| format!("atrium.toml: {e}"))?;
        let a = v.get("app").ok_or("only apps install from a file")?;
        let s = |k: &str| a.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let id = s("id");
        let publisher = match self.trust.verify_publisher(dir, &text, portcullis_trust::Demand::PolicyDefault) {
            Ok(Some(h)) => oc::hex(&h),
            Ok(None) => return Err(format!("{id}: not signed; a file installs only when signed by a publisher this machine trusts")),
            Err(e) => return Err(format!("{id}: {e}")),
        };
        let rec = self.store().get(&id)?;
        if let Some(r) = rec.as_ref().filter(|r| r.source != "side-load") {
            return Err(format!("{id} is installed from {}; it updates from there", r.source));
        }
        let plain = |c: String| c.split('=').next().unwrap_or("").to_string();
        let added = match &rec {
            Some(_) => {
                let have = fs::read_to_string(self.root.join("apps").join(&id).join("atrium.toml")).unwrap_or_default();
                updates::decide(&have, &text, true, None).1.into_iter().map(plain).collect()
            }
            None => Vec::new(),
        };
        let _ = uid;
        Ok(Described {
            id: id.clone(), kind: "app".into(), name: s("name"), description: s("description"), latest: s("version"),
            publisher, capabilities: updates::caps(&text).into_iter().map(plain).collect(), archive_size: size,
            installed: rec.map(|x| x.version), manifest_sha256: oc::hex(&Sha256::digest(text.as_bytes())), added,
            local: true, ..Default::default()
        })
    }

    fn install_held(&self, token: &str, manifest_sha256: &str, for_uid: u32) -> Result<Response, String> {
        if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("not a token".into());
        }
        self.prune_held();
        let (path, meta) = (self.held().join(format!("{token}.insula")), self.held().join(format!("{token}.toml")));
        let held = fs::read_to_string(&meta).map_err(|_| "that file is no longer held; open it again".to_string())?;
        let _ = fs::remove_file(&meta);
        let res = (|| {
            let mut l = held.lines();
            if l.next() != Some(manifest_sha256) || l.next() != Some(for_uid.to_string().as_str()) {
                return Err("not the file the sheet showed".to_string());
            }
            self.install_local(&path, for_uid, Some(manifest_sha256))
        })();
        let _ = fs::remove_file(&path);
        res
    }

    fn install_local(&self, path: &Path, uid: u32, expect: Option<&str>) -> Result<Response, String> {
        let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (dir, temp) = if meta.is_file() {
            (self.unpack(path, meta.len())?, true)
        } else {
            (path.to_path_buf(), false)
        };
        let res = (|| {
            if let Some(want) = expect {
                let text = fs::read(dir.join("atrium.toml")).map_err(|e| e.to_string())?;
                if oc::hex(&Sha256::digest(&text)) != want {
                    return Err("not the file the sheet showed".into());
                }
            }
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
                installed_by: uid, time: now(), verified_rps: Vec::new(), recoveries: 0,
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
            Some(r) if r.kind == "driver" => crate::driver::remove_driver(&self.system, id)?,
            Some(r) if r.kind == "firmware" => {
                // Its folder is the one whose manifest names it.
                for base in [crate::driver::FIRMWARE_JAILED, crate::driver::FIRMWARE_KERNEL] {
                    for e in fs::read_dir(self.system.join(base)).into_iter().flatten().flatten() {
                        if let Ok(t) = fs::read_to_string(e.path().join("atrium.toml")) {
                            if opifex_catalog::driver::FirmwareManifest::from_str(&t).is_ok_and(|m| m.firmware.id == id) {
                                crate::driver::remove_firmware(&self.system, &t)?;
                            }
                        }
                    }
                }
            }
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


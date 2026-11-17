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
    pub trust: portcullis_trust::Trust,
    pub require_content: bool,
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
            _ if !admin => Err("only an administrator may change what is installed (the install sheet comes with C3)".into()),
            Request::Install { id } => self.locked(|| self.install_catalog(&id, uid)),
            Request::InstallLocal { path } => self.locked(|| self.install_local(Path::new(&path), uid)),
            Request::Uninstall { id } => self.locked(|| self.uninstall(&id)),
            Request::Rollback { id } => self.locked(|| self.rollback(&id)),
            Request::Sync { from } => self.locked(|| self.sync(Path::new(&from))),
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
        let app = v.get("app");
        let s = |k: &str| app.and_then(|a| a.get(k)).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let capabilities = v.get("capabilities").and_then(|c| c.as_table()).map(|t| {
            t.iter().filter(|(_, v)| v.as_bool() != Some(false)).map(|(k, _)| k.clone()).collect()
        }).unwrap_or_default();
        Ok(Described {
            id: id.into(), kind: r.kind.clone(), name: s("name"), description: s("description"),
            latest: r.version.clone(), publisher: r.signer.clone(), catalog: local.name.clone(),
            capabilities, archive_size: r.archive_size,
            installed: self.store().get(id)?.map(|x| x.version),
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
        let src = from.to_path_buf();
        let fetch = move |p: &str| fs::read(src.join(p)).map_err(|e| format!("{p}: {e}"));
        let l = c.update(&fetch, now())?;
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

    fn install_catalog(&self, id: &str, uid: u32) -> Result<Response, String> {
        let c = self.catalog();
        let local = c.local()?;
        let root = c.root()?;
        let r = local.latest(id, portcullis_toml::layer::host_arch())
            .ok_or_else(|| format!("{id}: not in the catalog for this machine"))?.clone();
        if r.kind != "app" {
            return Err(format!("{id}: a {} (installed by its own path)", r.kind));
        }
        let signer = root.key(&r.signer).ok_or("signer gone from root.toml")?.pem.clone();
        // The archive: fetched by C4; for now it must be in the download folder.
        let archive = self.downloads().join(format!("{}.insula", r.archive_sha256));
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


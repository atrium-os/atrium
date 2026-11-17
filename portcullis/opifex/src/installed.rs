//! The installed record, `/var/db/atrium/opifex/installed.toml` (opifex.md
//! §7.5): per app, what was installed, from where, signed by whom.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT: &str = "/var/db/atrium/opifex/installed.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub version: String,
    pub kind: String,
    /// SHA-256 of the signing key (portcullis-sig's publisher id), hex; empty
    /// when trust is unconfigured and an unsigned bundle was allowed.
    pub publisher: String,
    /// `catalog:<name>` or `side-load`.
    pub source: String,
    /// The archive's SHA-256 (catalog installs).
    #[serde(default)]
    pub archive_sha256: String,
    /// `[bundle] content`: the signed content index's hash.
    #[serde(default)]
    pub content: String,
    pub installed_by: u32,
    pub time: u64,
    /// WebAuthn RP ids verified at install (sigillum.md; C7).
    #[serde(default)]
    pub verified_rps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    #[serde(default, rename = "app")]
    pub apps: Vec<Record>,
}

pub struct Store { pub path: PathBuf }

impl Store {
    pub fn new(path: &Path) -> Store { Store { path: path.to_path_buf() } }

    pub fn load(&self) -> Result<Installed, String> {
        match fs::read_to_string(&self.path) {
            Ok(t) => toml::from_str(&t).map_err(|e| format!("{}: {e}", self.path.display())),
            Err(_) => Ok(Installed::default()),
        }
    }

    fn save(&self, i: &Installed) -> Result<(), String> {
        if let Some(d) = self.path.parent() {
            fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, toml::to_string(i).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    /// Record an install, replacing the app's previous record; returns that one.
    pub fn put(&self, r: Record) -> Result<Option<Record>, String> {
        let mut i = self.load()?;
        let old = i.apps.iter().position(|a| a.id == r.id).map(|p| i.apps.remove(p));
        i.apps.push(r);
        i.apps.sort_by(|a, b| a.id.cmp(&b.id));
        self.save(&i)?;
        Ok(old)
    }

    pub fn remove(&self, id: &str) -> Result<Option<Record>, String> {
        let mut i = self.load()?;
        let old = i.apps.iter().position(|a| a.id == id).map(|p| i.apps.remove(p));
        self.save(&i)?;
        Ok(old)
    }

    pub fn get(&self, id: &str) -> Result<Option<Record>, String> {
        Ok(self.load()?.apps.into_iter().find(|a| a.id == id))
    }
}

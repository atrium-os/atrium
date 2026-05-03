//! `JailConfig` — the structured form of a jail.conf section.
//!
//! Insertion-ordered for deterministic rendering (golden-file
//! tests benefit from stable output).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Bool(bool),
    Number(i64),
    /// Symbolic / unquoted token (e.g. "disable", "inherit"). The
    /// renderer emits these without surrounding quotes.
    Symbolic(String),
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub src:    PathBuf,
    pub dst:    PathBuf,
    pub fstype: String,
    /// Mount options like "rw" / "ro" / "nosuid".
    pub opts:   Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DevfsAction {
    /// Raw devfs.rules-style action, e.g. `path 'fresco0' unhide`.
    pub line: String,
}

pub struct JailConfig {
    pub name:      String,
    pub root_path: PathBuf,

    /// Insertion-ordered jail.conf parameters (key, value).
    /// The same key may appear at most once; later sets overwrite
    /// earlier (with a recorded set in `set_keys` so callers can
    /// query `has_set`).
    pub params:    Vec<(String, Value)>,
    set_keys:      HashSet<String>,

    pub mounts:        Vec<MountSpec>,
    pub devfs_actions: Vec<DevfsAction>,
}

impl JailConfig {
    pub fn new(name: String, root_path: PathBuf) -> Self {
        Self {
            name, root_path,
            params: Vec::new(),
            set_keys: HashSet::new(),
            mounts: Vec::new(),
            devfs_actions: Vec::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: Value) -> &mut Self {
        if self.set_keys.contains(key) {
            for (k, v) in &mut self.params {
                if k == key { *v = value; return self; }
            }
        }
        self.params.push((key.to_string(), value));
        self.set_keys.insert(key.to_string());
        self
    }

    pub fn has_set(&self, key: &str) -> bool {
        self.set_keys.contains(key)
    }

    pub fn add_mount(&mut self, src: &Path, dst: &Path, fstype: &str, opts: &[&str]) {
        self.mounts.push(MountSpec {
            src:    src.to_path_buf(),
            dst:    dst.to_path_buf(),
            fstype: fstype.to_string(),
            opts:   opts.iter().map(|s| s.to_string()).collect(),
        });
    }

    pub fn add_devfs_action(&mut self, line: &str) {
        self.devfs_actions.push(DevfsAction { line: line.to_string() });
    }
}

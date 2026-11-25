//! `JailConfig`: the jail an app's manifest builds, in jail(8)'s parameter
//! names, which portcullisd maps to a jaild request (jail_request.rs).
//! Insertion-ordered, so the mapping and its tests see a stable order.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Bool(bool),
    Number(i64),
    /// Symbolic token (e.g. "new", "disable", "inherit").
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

#[derive(Debug, Clone)]
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
    /// Host files COPIED into the jail's writable root before it starts
    /// (source, destination under the root): the machine's time zone. A
    /// read-only file mount isn't seen through a layered (unionfs) root.
    pub copies:        Vec<(PathBuf, PathBuf)>,

    /// ★ The `full` network capability's pending network (network.md §0):
    /// `Some(mac)` when this jail needs a point-to-point epair from jaild,
    /// carrying the app's derived MAC. jaild builds it from the request
    /// (`NetworkConfig::Routed`), never the host's stack.
    pub needs_routed_net: Option<String>,
    /// The app's id and `network` spec for that network — what the launcher
    /// turns into jaild grants (network.md §0.1). Set by build() with the MAC.
    pub routed_net_spec: Option<(String, portcullis_toml::NetworkSpec)>,
    /// The routing table jaild puts the network's host end in: a persona's
    /// VPN route domain (vpn.md §5.3), from atrium-net. `None`: the host's.
    pub route_fib: Option<u32>,
}

impl JailConfig {
    pub fn new(name: String, root_path: PathBuf) -> Self {
        Self {
            name, root_path,
            params: Vec::new(),
            set_keys: HashSet::new(),
            mounts: Vec::new(),
            devfs_actions: Vec::new(),
            copies: Vec::new(),
            needs_routed_net: None,
            routed_net_spec: None,
            route_fib: None,
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

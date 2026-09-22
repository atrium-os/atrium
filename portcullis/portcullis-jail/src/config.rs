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

    /// ★ The `full` network capability's pending network (network.md §0):
    /// `Some(mac)` when this jail needs a point-to-point epair from jaild,
    /// carrying the app's derived MAC. Not rendered — whoever runs `jail -c`
    /// asks jaild for the allocation and calls [`JailConfig::attach_routed_net`];
    /// a config still pending is refused (portcullis_mounts::ensure_network_ready),
    /// never run on the host's stack.
    pub needs_routed_net: Option<String>,
    /// Set by [`JailConfig::attach_routed_net`].
    pub routed_net_attached: bool,
}

impl JailConfig {
    pub fn new(name: String, root_path: PathBuf) -> Self {
        Self {
            name, root_path,
            params: Vec::new(),
            set_keys: HashSet::new(),
            mounts: Vec::new(),
            devfs_actions: Vec::new(),
            needs_routed_net: None,
            routed_net_attached: false,
        }
    }

    /// Give this jail the network jaild allocated for it. `exec.created` —
    /// which jail(8) runs on the host after creation and before the app starts
    /// — moves `epair_b` into the jail's vnet, then sets the derived MAC, the
    /// address, its own loopback and the default route.
    ///
    /// ★ NOT `vnet.interface`: jail(8) moves that interface AFTER exec.created
    /// (usr.sbin/jail/jail.c: IP_EXEC_CREATED, IP_ZFS_DATASET,
    /// IP_VNET_INTERFACE, IP_EXEC_START), so configuring it from exec.created
    /// failed with the interface not yet in the jail — measured.
    ///
    /// ★ Every command here is QUIET (`route -q`): jail(8) passes exec.created's
    /// stdout through, and on the daemon lane that is the caller's pipe —
    /// `route` printed "add net default: gateway …" into the app's output
    /// (measured), the same corruption `jail -q` was added for.
    ///
    /// ★ And a 1 s MSL in the app's own stack (per-vnet; the host keeps 30 s):
    /// TIME_WAIT in a stack that is destroyed at exit protects nothing, and it
    /// kept the jail dying — root pinned — for 2×30 s (measured).
    pub fn attach_routed_net(&mut self, epair_b: &str, app_addr: &str, host_addr: &str) -> Result<(), String> {
        let Some(mac) = self.needs_routed_net.clone() else {
            return Err(format!("jail {} did not ask for a network", self.name));
        };
        let n = self.name.clone();
        self.set("exec.created", Value::String(format!(
            "ifconfig {epair_b} vnet {n} && ifconfig -j {n} {epair_b} ether {mac} \
             && ifconfig -j {n} {epair_b} inet {app_addr}/30 up \
             && ifconfig -j {n} lo0 inet 127.0.0.1/8 up && route -q -j {n} add default {host_addr} \
             && sysctl -j {n} net.inet.tcp.msl=1000 >/dev/null")));
        self.routed_net_attached = true;
        Ok(())
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

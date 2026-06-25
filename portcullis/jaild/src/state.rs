//! Persistent state — the list of jaild-managed *persistent* jails
//! that a restarted jaild needs to know about.
//!
//! Lives at `/var/run/atrium/jaild.state.toml`. Atomically updated
//! (`<path>.tmp` + fsync + rename) on every persistent
//! create/remove. On startup, jaild loads this file and uses it
//! to refuse re-creating a jail that already exists.
//!
//! Exec'd jails (those with `ExecSpec`) are *not* tracked here.
//! They use `persist=0` so they die with their process; they're
//! also not jaild's responsibility to remember — the procdesc fd
//! lives with the requester (portcullisd) and is the
//! authoritative lifecycle handle.
//!
//! ## Crash semantics
//!
//! - jaild crashes mid-write: the `.tmp` file may be partial; the
//!   rename hasn't happened, so the canonical file is still the
//!   pre-crash state. On restart we'll re-claim those jails. The
//!   only loss is the in-flight create/remove that was racing the
//!   crash.
//! - jaild crashes between `jail_set` returning and writing
//!   state: the kernel has a jail jaild doesn't know about. On
//!   restart we'd refuse to re-create the same name (because the
//!   kernel still has it under that name), but our state file
//!   wouldn't list it. **Not handled in V1b.** A future
//!   "reconcile against `kern.jail.list`" pass at startup would
//!   close this.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::JaildError;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PersistentState {
    pub schema_version: u32,
    #[serde(default)]
    pub written_at_unix: u64,
    #[serde(default)]
    pub jails: Vec<JailRecord>,
    /// Runtime mounts attached on a per-jail basis after the jail
    /// was created. See `docs/spec/storage.md` §6.2.
    #[serde(default)]
    pub runtime_mounts: Vec<RuntimeMount>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JailRecord {
    pub name:            String,
    pub jid:             i32,
    pub created_at_unix: u64,
    /// CIDR form lo0 alias address allocated for this jail, if
    /// any. `None` if the jail had `network = disable` or
    /// hasn't been migrated to the network-aware schema.
    /// On RemoveJail this is what gets `ifconfig -alias`d.
    #[serde(default)]
    pub lo0_alias:       Option<String>,
    /// Jail chroot path on the host. Used by AttachMount to
    /// resolve in-jail dest paths. Empty for V1 records loaded
    /// from a pre-V2 state file; AttachMount returns an error
    /// in that case.
    #[serde(default)]
    pub path:            String,
    /// The uid/gid the jail's exec'd process runs as (its dedicated
    /// app-uid). 0 for create-without-exec jails and pre-existing
    /// records. `ExecInJail` defaults to this so a jexec shell runs as
    /// the jail's own (non-root) uid — the app's exact view (stoa.md §4.5).
    #[serde(default)]
    pub uid:             u32,
    #[serde(default)]
    pub gid:             u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeMount {
    pub jail_name:        String,
    pub source:           String,
    /// In-jail dest path, e.g. `/var/projects`. The host-side
    /// mount point is `<jail.path>/<this dest>` — derived at
    /// detach time from the JailRecord's path (which we don't
    /// currently store; AttachMount looks it up per-call).
    pub dest:             String,
    pub kind:             String,    // "ro_nullfs" / "rw_nullfs" / "tmpfs"
    pub host_dest:        String,    // resolved <jail_path>/<dest>
    pub attached_at_unix: u64,
}

impl PersistentState {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            written_at_unix: now_unix(),
            jails: Vec::new(),
            runtime_mounts: Vec::new(),
        }
    }

    /// Load from `path` if it exists. Missing file → empty state
    /// (first boot). Schema mismatch → error (operator must
    /// migrate manually).
    pub fn load(path: &Path) -> Result<Self, JaildError> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let st: PersistentState = toml::from_str(&s)
                    .map_err(|e| JaildError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("parse state file: {e}"),
                    )))?;
                if st.schema_version != SCHEMA_VERSION {
                    return Err(JaildError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "state schema_version {} unsupported (expect {})",
                            st.schema_version, SCHEMA_VERSION),
                    )));
                }
                Ok(st)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::empty())
            }
            Err(e) => Err(JaildError::Io(e)),
        }
    }

    /// Atomically replace the file at `path` with the current
    /// state. Pattern: write to `<path>.tmp`, fsync, rename.
    /// Crash mid-write leaves the canonical file intact.
    pub fn save(&self, path: &Path) -> Result<(), JaildError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut tmp_path = PathBuf::from(path);
        tmp_path.set_extension("toml.tmp");
        {
            let mut f = File::create(&tmp_path)?;
            let body = toml::to_string_pretty(self)
                .map_err(|e| JaildError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("serialize state: {e}"),
                )))?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    pub fn add(&mut self, name: &str, jid: i32, lo0_alias: Option<String>, path: &str) {
        self.add_exec(name, jid, lo0_alias, path, 0, 0);
    }

    /// Like [`add`](Self::add) but records the exec'd process's app
    /// uid/gid, so [`crate::protocol::Request::ExecInJail`] can run a
    /// jexec shell as the jail's own (non-root) uid.
    pub fn add_exec(
        &mut self,
        name: &str,
        jid: i32,
        lo0_alias: Option<String>,
        path: &str,
        uid: u32,
        gid: u32,
    ) {
        self.jails.push(JailRecord {
            name: name.to_string(),
            jid,
            created_at_unix: now_unix(),
            lo0_alias,
            path: path.to_string(),
            uid,
            gid,
        });
        self.written_at_unix = now_unix();
    }

    pub fn add_runtime_mount(
        &mut self,
        jail_name: &str,
        source:    &str,
        dest:      &str,
        kind:      &str,
        host_dest: &str,
    ) {
        self.runtime_mounts.push(RuntimeMount {
            jail_name:        jail_name.to_string(),
            source:           source.to_string(),
            dest:             dest.to_string(),
            kind:             kind.to_string(),
            host_dest:        host_dest.to_string(),
            attached_at_unix: now_unix(),
        });
        self.written_at_unix = now_unix();
    }

    /// Remove the runtime-mount record matching jail_name + dest.
    /// Returns the removed record's host_dest if any.
    pub fn remove_runtime_mount(&mut self, jail_name: &str, dest: &str) -> Option<String> {
        let pos = self.runtime_mounts.iter()
            .position(|m| m.jail_name == jail_name && m.dest == dest)?;
        let m = self.runtime_mounts.remove(pos);
        self.written_at_unix = now_unix();
        Some(m.host_dest)
    }

    pub fn remove_by_jid(&mut self, jid: i32) -> bool {
        let before = self.jails.len();
        self.jails.retain(|r| r.jid != jid);
        if self.jails.len() != before {
            self.written_at_unix = now_unix();
            true
        } else {
            false
        }
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.jails.iter().any(|r| r.name == name)
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nope.toml");
        let st = PersistentState::load(&p).unwrap();
        assert_eq!(st.schema_version, SCHEMA_VERSION);
        assert!(st.jails.is_empty());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.toml");

        let mut st = PersistentState::empty();
        st.add("atrium-foo", 7, None, "/");
        st.add("atrium-bar", 8, None, "/");
        st.save(&p).unwrap();

        let back = PersistentState::load(&p).unwrap();
        assert_eq!(back.jails.len(), 2);
        assert_eq!(back.jails[0].name, "atrium-foo");
        assert_eq!(back.jails[0].jid, 7);
        assert!(back.has_name("atrium-bar"));
    }

    #[test]
    fn remove_by_jid_works() {
        let mut st = PersistentState::empty();
        st.add("atrium-a", 1, None, "/");
        st.add("atrium-b", 2, None, "/");
        assert!(st.remove_by_jid(1));
        assert!(!st.has_name("atrium-a"));
        assert!(st.has_name("atrium-b"));
        assert!(!st.remove_by_jid(99));   // no-op for unknown jid
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.toml");
        std::fs::write(&p, "schema_version = 999\nwritten_at_unix = 0\njails = []\n").unwrap();
        let err = PersistentState::load(&p).unwrap_err();
        match err {
            JaildError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn atomic_save_leaves_no_tmp() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.toml");
        let mut st = PersistentState::empty();
        st.add("atrium-x", 5, None, "/");
        st.save(&p).unwrap();
        let mut tmp = p.clone();
        tmp.set_extension("toml.tmp");
        assert!(p.exists());
        assert!(!tmp.exists(), "tmp should be renamed away");
    }
}

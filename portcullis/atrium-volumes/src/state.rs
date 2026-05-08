//! `/var/run/atrium/volumes.state.toml` — authoritative
//! inventory of allocated volumes.
//!
//! Spec: `docs/spec/atrium-volumes.md` §5. Same atomic-replace
//! pattern as jaild's state file.
//!
//! Tmpfs volumes are NOT tracked here — they're ephemeral and
//! jaild applies the mount inline at jail-launch time.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::VolumesError;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub schema_version: u32,
    #[serde(default)]
    pub written_at_unix: u64,
    #[serde(default, rename = "volume")]
    pub volumes: Vec<VolumeRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeRecord {
    pub jail_name:        String,
    pub volume_name:      String,
    pub backend:          String,
    pub backend_kind:     String,
    pub host_path:        String,
    pub mount_at:         String,
    pub allocated_at_unix: u64,
    pub mode:             u32,
    pub owner_uid:        u32,
    pub owner_gid:        u32,
    pub size_max:         Option<u64>,
}

impl State {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            written_at_unix: now_unix(),
            volumes: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, VolumesError> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let st: State = toml::from_str(&s)
                    .map_err(|e| VolumesError::State(
                        format!("parse {}: {e}", path.display())))?;
                if st.schema_version != SCHEMA_VERSION {
                    return Err(VolumesError::State(format!(
                        "schema_version {} unsupported (expect {})",
                        st.schema_version, SCHEMA_VERSION,
                    )));
                }
                Ok(st)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(VolumesError::Io(e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), VolumesError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut tmp = PathBuf::from(path);
        tmp.set_extension("toml.tmp");
        {
            let mut f = File::create(&tmp)?;
            let body = toml::to_string_pretty(self)
                .map_err(|e| VolumesError::State(
                    format!("serialize: {e}")))?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn find(&self, jail: &str, vol: &str) -> Option<&VolumeRecord> {
        self.volumes.iter()
            .find(|v| v.jail_name == jail && v.volume_name == vol)
    }

    pub fn add(&mut self, rec: VolumeRecord) {
        self.volumes.push(rec);
        self.written_at_unix = now_unix();
    }

    pub fn remove(&mut self, jail: &str, vol: &str) -> bool {
        let before = self.volumes.len();
        self.volumes.retain(|v| !(v.jail_name == jail && v.volume_name == vol));
        if self.volumes.len() != before {
            self.written_at_unix = now_unix();
            true
        } else {
            false
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(jail: &str, vol: &str) -> VolumeRecord {
        VolumeRecord {
            jail_name:   jail.into(),
            volume_name: vol.into(),
            backend:     "default".into(),
            backend_kind: "tessera".into(),
            host_path:   format!("/var/lib/atrium/storage/jails/{jail}/{vol}"),
            mount_at:    format!("/var/db/{vol}"),
            allocated_at_unix: 0,
            mode:        0o700,
            owner_uid:   88,
            owner_gid:   88,
            size_max:    None,
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.toml");
        let mut st = State::empty();
        st.add(rec("mysqld", "data"));
        st.add(rec("mysqld", "logs"));
        st.save(&p).unwrap();
        let back = State::load(&p).unwrap();
        assert_eq!(back.volumes.len(), 2);
        assert!(back.find("mysqld", "data").is_some());
    }

    #[test]
    fn remove_works() {
        let mut st = State::empty();
        st.add(rec("mysqld", "data"));
        assert!(st.remove("mysqld", "data"));
        assert!(!st.remove("mysqld", "data")); // already gone
    }

    #[test]
    fn missing_returns_empty() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nope.toml");
        let st = State::load(&p).unwrap();
        assert_eq!(st.schema_version, SCHEMA_VERSION);
        assert!(st.volumes.is_empty());
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.toml");
        std::fs::write(&p, "schema_version = 999\n").unwrap();
        assert!(State::load(&p).is_err());
    }
}

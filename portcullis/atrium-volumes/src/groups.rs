//! Expected groups (tessera-encryption.md §5.4, E4.6): which of a person's
//! folders must be encrypted, and under which key group and key id. Kept
//! at `/var/db/atrium/volumes.groups.toml` (it must outlive a reboot, so not
//! in `/var/run`). At login a folder that should be in its group and isn't,
//! or is in another, is refused: that is an offline edit trying to get new
//! files written in the clear.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::VolumesError;

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_PATH: &str = "/var/db/atrium/volumes.groups.toml";

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Groups {
    pub schema_version: u32,
    #[serde(default, rename = "group")]
    pub groups: Vec<GroupEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct GroupEntry {
    pub person:      String,
    pub persona:     String,
    /// The volume the group is on (hex).
    pub volume_uuid: String,
    pub group_id:    u64,
    /// The group key's Arca id (hex).
    pub key_id:      String,
    /// Folders that are in the group: each must stay so.
    #[serde(default)]
    pub folders:     Vec<String>,
    /// The persona was removed and its key retired; the erase is still
    /// to finish (it waits for the group's folders to be out of use).
    #[serde(default)]
    pub retired:     bool,
}

impl Groups {
    pub fn load(path: &Path) -> Result<Self, VolumesError> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let g: Groups = toml::from_str(&s)
                    .map_err(|e| VolumesError::State(format!("parse {}: {e}", path.display())))?;
                if g.schema_version != SCHEMA_VERSION {
                    return Err(VolumesError::State(format!(
                        "{}: schema_version {} unsupported", path.display(), g.schema_version)));
                }
                Ok(g)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Groups { schema_version: SCHEMA_VERSION, groups: Vec::new() })
            }
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
                .map_err(|e| VolumesError::State(format!("serialize: {e}")))?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn find(&self, person: &str, persona: &str) -> Option<&GroupEntry> {
        self.groups.iter().find(|g| g.person == person && g.persona == persona)
    }

    pub fn find_mut(&mut self, person: &str, persona: &str) -> Option<&mut GroupEntry> {
        self.groups.iter_mut().find(|g| g.person == person && g.persona == persona)
    }

    pub fn remove(&mut self, person: &str, persona: &str) {
        self.groups.retain(|g| !(g.person == person && g.persona == persona));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("db/groups.toml");
        let mut g = Groups::load(&p).unwrap();
        assert!(g.groups.is_empty());
        g.groups.push(GroupEntry { person: "alice".into(), persona: "work".into(), group_id: 5,
            folders: vec!["/home/alice/.atrium/personas/work/Documents".into()], ..Default::default() });
        g.save(&p).unwrap();
        let back = Groups::load(&p).unwrap();
        assert_eq!(back.find("alice", "work").unwrap().group_id, 5);
        assert!(back.find("alice", "personal").is_none());
    }
}

//! `/etc/atrium/volumes.policy.toml` — operator-configured
//! named backends. Spec: `docs/spec/atrium-volumes.md` §4.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::VolumesError;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Policy {
    pub schema_version: u32,
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BackendConfig {
    pub name: String,
    pub kind: BackendKind,

    /// Root directory on the host where this backend allocates
    /// volumes. For `tessera` and `plain`, this is the directory
    /// the operator mounted whatever filesystem at; for `tmpfs`,
    /// ignored. (For `zfs` — V1 — there's a separate `pool` +
    /// `mount_root` pair; not modelled here yet.)
    #[serde(default)]
    pub root: Option<String>,

    /// Marks this backend as the default. Exactly one backend
    /// must have `default = true`.
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Tessera,
    Zfs,
    Plain,
    Tmpfs,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self, VolumesError> {
        let body = fs::read_to_string(path)
            .map_err(|e| VolumesError::Policy(format!("read {}: {e}", path.display())))?;
        let p: Policy = toml::from_str(&body)
            .map_err(|e| VolumesError::Policy(format!("parse {}: {e}", path.display())))?;
        if p.schema_version != SCHEMA_VERSION {
            return Err(VolumesError::Policy(format!(
                "schema_version {} unsupported (expect {})",
                p.schema_version, SCHEMA_VERSION,
            )));
        }
        p.validate()?;
        Ok(p)
    }

    fn validate(&self) -> Result<(), VolumesError> {
        let mut by_name: BTreeMap<&str, &BackendConfig> = BTreeMap::new();
        let mut default_count = 0;
        for b in &self.backends {
            if by_name.insert(&b.name, b).is_some() {
                return Err(VolumesError::Policy(format!(
                    "duplicate backend name: {:?}", b.name)));
            }
            if b.default {
                default_count += 1;
            }
            if matches!(b.kind, BackendKind::Tessera | BackendKind::Plain)
                && b.root.is_none()
            {
                return Err(VolumesError::Policy(format!(
                    "backend {:?} ({:?}) requires `root = \"...\"`",
                    b.name, b.kind)));
            }
        }
        if !self.backends.is_empty() && default_count != 1 {
            return Err(VolumesError::Policy(format!(
                "exactly one backend must be marked `default = true`; got {default_count}")));
        }
        Ok(())
    }

    /// Resolve a backend reference. `None` = use the default.
    pub fn resolve(&self, name: Option<&str>) -> Option<&BackendConfig> {
        match name {
            None => self.backends.iter().find(|b| b.default),
            Some(n) => self.backends.iter().find(|b| b.name == n),
        }
    }

    pub fn backend_names(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(p: &Path, s: &str) {
        fs::write(p, s).unwrap();
    }

    #[test]
    fn parse_minimal() {
        let s = r#"
            schema_version = 1

            [[backend]]
            name    = "default"
            kind    = "tessera"
            root    = "/var/lib/atrium/storage"
            default = true
        "#;
        let p: Policy = toml::from_str(s).unwrap();
        p.validate().unwrap();
        assert_eq!(p.backends.len(), 1);
        assert_eq!(p.backends[0].kind, BackendKind::Tessera);
    }

    #[test]
    fn parse_full() {
        let s = r#"
            schema_version = 1

            [[backend]]
            name    = "default"
            kind    = "tessera"
            root    = "/var/lib/atrium/storage"
            default = true

            [[backend]]
            name = "fast-db"
            kind = "zfs"

            [[backend]]
            name = "plain-on-ufs"
            kind = "plain"
            root = "/var/lib/atrium-plain"
        "#;
        let p: Policy = toml::from_str(s).unwrap();
        p.validate().unwrap();
        assert_eq!(p.backends.len(), 3);
    }

    #[test]
    fn rejects_duplicate_name() {
        let s = r#"
            schema_version = 1

            [[backend]]
            name = "x"
            kind = "tessera"
            root = "/a"
            default = true

            [[backend]]
            name = "x"
            kind = "plain"
            root = "/b"
        "#;
        let p: Policy = toml::from_str(s).unwrap();
        let err = p.validate().unwrap_err();
        match err {
            VolumesError::Policy(s) => assert!(s.contains("duplicate"), "{s}"),
            _ => panic!("wrong"),
        }
    }

    #[test]
    fn rejects_zero_or_two_defaults() {
        let no_default = r#"
            schema_version = 1

            [[backend]]
            name = "a"
            kind = "tessera"
            root = "/a"
        "#;
        let p: Policy = toml::from_str(no_default).unwrap();
        assert!(p.validate().is_err());

        let two_defaults = r#"
            schema_version = 1

            [[backend]]
            name = "a"
            kind = "tessera"
            root = "/a"
            default = true

            [[backend]]
            name = "b"
            kind = "plain"
            root = "/b"
            default = true
        "#;
        let p: Policy = toml::from_str(two_defaults).unwrap();
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_missing_root_for_tessera_or_plain() {
        let s = r#"
            schema_version = 1

            [[backend]]
            name = "default"
            kind = "tessera"
            default = true
        "#;
        let p: Policy = toml::from_str(s).unwrap();
        assert!(p.validate().is_err());
    }

    #[test]
    fn load_real_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("volumes.policy.toml");
        write(&p, r#"
            schema_version = 1

            [[backend]]
            name    = "default"
            kind    = "tessera"
            root    = "/var/lib/atrium/storage"
            default = true
        "#);
        let pol = Policy::load(&p).unwrap();
        assert_eq!(pol.backends.len(), 1);
    }

    #[test]
    fn resolve_default() {
        let s = r#"
            schema_version = 1

            [[backend]]
            name = "default"
            kind = "tessera"
            root = "/a"
            default = true

            [[backend]]
            name = "fast-db"
            kind = "zfs"
        "#;
        let p: Policy = toml::from_str(s).unwrap();
        p.validate().unwrap();
        assert_eq!(p.resolve(None).unwrap().name, "default");
        assert_eq!(p.resolve(Some("fast-db")).unwrap().name, "fast-db");
        assert!(p.resolve(Some("nope")).is_none());
    }
}

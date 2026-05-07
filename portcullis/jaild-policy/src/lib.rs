//! jaild-policy — typed schema for `/etc/atrium/jaild.policy.toml`.
//!
//! The file is parsed once at jaild startup. The structures here
//! are pure data; validation logic (matching a request against the
//! parsed policy) lives in the jaild crate itself, layered on top.
//!
//! Spec: `docs/spec/jaild-policy.md`. Sample file:
//! `etc/jaild.policy.toml` in the Atrium repo.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version we know how to parse. Bumping is a breaking-change
/// action — old jails fail to start until policy is migrated.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("schema_version {found} unsupported (this jaild knows {known})")]
    SchemaVersion { found: u32, known: u32 },
    #[error("missing required section: {0}")]
    MissingSection(&'static str),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Policy {
    pub schema_version: u32,
    pub mount_sources:  MountSources,
    pub devfs_rulesets: DevfsRulesets,
    pub exec_paths:     ExecPaths,
    pub env:            EnvAllow,
    pub uid:             UidPolicy,
    pub gid:             GidPolicy,
    pub children_max:   ChildrenMax,
    pub network:        NetworkPolicy,
    pub gpu_drivers:    GpuDrivers,
    /// Named system-service profiles, keyed by service id.
    #[serde(default)]
    pub services:       BTreeMap<String, ServiceProfile>,
    pub apps:           AppsPolicy,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MountSources {
    #[serde(default)]
    pub ro_paths:    Vec<String>,
    #[serde(default)]
    pub rw_paths:    Vec<String>,
    /// Single-segment trailing-`*` globs: `/usr/home/*` matches
    /// `/usr/home/alice` but NOT `/usr/home/alice/foo`. For the
    /// per-user-home and similar single-level cases.
    #[serde(default)]
    pub rw_patterns: Vec<String>,
    /// Prefix-match subtrees: any path starting with one of
    /// these is permitted as an rw mount source. Used for
    /// atrium-volumes' deep-nested allocation paths
    /// (`/var/lib/atrium/storage/jails/<jail>/<vol>` etc.).
    /// Conservative defaults: leave empty until you have an
    /// allocator daemon producing the paths.
    #[serde(default)]
    pub rw_subtrees: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DevfsRulesets {
    /// Named rulesets, for documentation / cross-reference with
    /// `/etc/devfs.rules`. Not used for kernel calls — `jail_set`
    /// takes a number. See `allowed_ids`.
    #[serde(default)]
    pub allowed: Vec<String>,
    /// Numeric ruleset IDs jaild may pass to `jail_set` as
    /// `devfs_ruleset`. The kernel side of `jail_set` takes a
    /// number; the names above are operator-readable only.
    /// Empty list = no devfs_ruleset may be set on jails (inherit
    /// host devfs). 0 is always permitted (= "inherit").
    #[serde(default)]
    pub allowed_ids: Vec<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecPaths {
    #[serde(default)]
    pub allowed_prefixes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnvAllow {
    #[serde(default)]
    pub allowed_keys:     Vec<String>,
    #[serde(default)]
    pub allowed_prefixes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UidPolicy {
    pub min_user_uid:     u32,
    pub max_user_uid:     u32,
    #[serde(default)]
    pub allowed_system_uids: Vec<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GidPolicy {
    #[serde(default)]
    pub allowed_system_gids: Vec<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChildrenMax {
    pub max: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub allow_disable: bool,
    #[serde(default)]
    pub allow_host:    bool,
    #[serde(default)]
    pub allowed_addrs_on_lo0: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GpuDrivers {
    /// Map keyed by driver name. Inner key in TOML is `attested.<name>`.
    #[serde(default)]
    pub attested: BTreeMap<String, GpuDriverAttestation>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GpuDriverAttestation {
    pub status:                GpuDriverStatus,
    pub isolation_test_passed: bool,
    #[serde(default)]
    pub isolation_test_date:   String,
    #[serde(default)]
    pub isolation_test_commit: String,
    #[serde(default)]
    pub notes:                 String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuDriverStatus {
    Production,
    Experimental,
    Broken,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServiceProfile {
    pub exec_path:             String,
    pub allowed_devfs_ruleset: String,
    #[serde(default)]
    pub required_mounts_ro:    Vec<String>,
    #[serde(default)]
    pub required_mounts_rw:    Vec<String>,
    #[serde(default)]
    pub allowed_extra_mounts_ro: Vec<String>,
    #[serde(default)]
    pub allowed_extra_mounts_rw: Vec<String>,
    pub network:               NetworkConfig,
    pub uid:                   UidSpec,
    pub children_max:          u32,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkConfig {
    Disable,
    Host,
    #[serde(rename = "lo0-only")]
    Lo0Only,
}

/// `uid` field on a service profile. Either a fixed value, or
/// "user" meaning "filled in per-instance from the request".
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum UidSpec {
    Symbolic(String),       // "root" | "user" | "_atrium-frescod"
    Numeric(u32),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AppsPolicy {
    pub allowed_exec_root:         String,
    pub default_devfs_ruleset:     String,
    pub max_simultaneous_per_user: u32,
}

impl Policy {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path = path.as_ref();
        let bytes = fs::read_to_string(path)
            .map_err(|e| PolicyError::Read(path.display().to_string(), e))?;
        let policy: Policy = toml::from_str(&bytes)?;
        if policy.schema_version != SCHEMA_VERSION {
            return Err(PolicyError::SchemaVersion {
                found: policy.schema_version,
                known: SCHEMA_VERSION,
            });
        }
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the shipped sample at etc/jaild.policy.toml.
    /// Confirms the schema is internally consistent + serde-able
    /// against the canonical sample.
    #[test]
    fn parses_shipped_sample() {
        // Locate the sample relative to this crate's source.
        // CARGO_MANIFEST_DIR = portcullis/jaild-policy/.
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::path::Path::new(crate_dir)
            .parent().unwrap()      // portcullis/
            .parent().unwrap()      // bsd/
            .join("etc/jaild.policy.toml");

        let p = Policy::load(&sample)
            .unwrap_or_else(|e| panic!("load {}: {e}", sample.display()));

        assert_eq!(p.schema_version, SCHEMA_VERSION);

        // Sanity-check a few fields that V7 / D2 directly depend on.
        assert!(!p.mount_sources.ro_paths.is_empty());
        assert!(p.devfs_rulesets.allowed.contains(&"atrium-gpu".to_string()));
        assert!(p.exec_paths.allowed_prefixes.iter()
            .any(|s| s.starts_with("/usr/local/bin/atrium-")));

        let frescod = p.services.get("frescod")
            .expect("services.frescod missing");
        assert_eq!(frescod.allowed_devfs_ruleset, "atrium-gpu");
        assert_eq!(frescod.network, NetworkConfig::Disable);

        let supervisor = p.services.get("atrium-supervisor")
            .expect("services.atrium-supervisor missing");
        // The user supervisor's uid is symbolic ("user"), filled
        // in per-instance.
        match &supervisor.uid {
            UidSpec::Symbolic(s) => assert_eq!(s, "user"),
            UidSpec::Numeric(_)  => panic!("supervisor uid should be symbolic"),
        }

        let virtio = p.gpu_drivers.attested.get("atrium-virtio-gpu")
            .expect("attested atrium-virtio-gpu missing");
        assert_eq!(virtio.status, GpuDriverStatus::Production);
        assert!(virtio.isolation_test_passed);
    }

    /// Schema version mismatch must fail.
    #[test]
    fn rejects_wrong_schema_version() {
        let bad = format!(r#"
schema_version = {}

[mount_sources]
[devfs_rulesets]
[exec_paths]
[env]
[uid]
min_user_uid = 1000
max_user_uid = 65000
[gid]
[children_max]
max = 64
[network]
[gpu_drivers]
[apps]
allowed_exec_root = "/x"
default_devfs_ruleset = "x"
max_simultaneous_per_user = 1
"#, SCHEMA_VERSION + 99);
        let dir = std::env::temp_dir();
        let p = dir.join(format!("jaild-policy-test-{}.toml", std::process::id()));
        std::fs::write(&p, bad).unwrap();
        let err = Policy::load(&p).expect_err("should reject wrong version");
        match err {
            PolicyError::SchemaVersion { .. } => {}
            other => panic!("wrong error: {other}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}

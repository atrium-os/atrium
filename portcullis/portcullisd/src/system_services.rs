//! System-service manifest loader.
//!
//! Each file under `/etc/atrium/services.d/<name>.toml` describes
//! one system service that portcullisd should ask jaild to launch
//! at boot. These are the "atrium daemons" — frescod,
//! atrium-devevents, vestibulum, etc. — not user apps.
//!
//! Schema is intentionally a thin layer over `jaild::protocol::
//! CreateJailRequest`. portcullisd's job is mostly to pre-validate
//! shape, expand templates (`@<seat>` instances in V2), and submit
//! the requests in a sensible order.
//!
//! V1 schema (this commit):
//!
//! ```toml
//! # /etc/atrium/services.d/atrium-frescod.toml
//! enabled = true
//! name    = "atrium-frescod"
//! path    = "/"            # placeholder until D5 atrium-rootfs lands
//! children_max = 0
//! devfs_ruleset = 0        # 0 = inherit; production uses non-zero
//!
//! [[mounts]]
//! source = "/usr/local/lib"
//! dest   = "usr/local/lib"
//! kind   = "ro_nullfs"
//!
//! [exec]
//! path = "/usr/local/bin/atrium-frescod"
//! argv = ["atrium-frescod", "--seat", "seat0"]
//! uid  = 1001
//! gid  = 1001
//!
//! [[exec.env]]
//! key = "PATH"
//! value = "/bin:/usr/bin"
//! ```
//!
//! Loader returns the list sorted by filename — boot order is
//! lexicographic for V1 (operator names with a numeric prefix:
//! `00-atrium-devevents.toml`, `10-atrium-frescod.toml`,
//! `20-vestibulum-seat0.toml`, …). True dependency-graph ordering
//! lands in V2 if a real cycle ever shows up.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use jaild::protocol::{
    CreateJailRequest, EnvPair, ExecSpec, MountKind, MountSpec, NetworkConfig,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct ServiceManifest {
    /// Operator switch — disabled services are loaded but skipped
    /// at launch time. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,

    pub name: String,
    pub path: String,

    #[serde(default)]
    pub children_max: u32,

    #[serde(default)]
    pub devfs_ruleset: u32,

    #[serde(default)]
    pub mounts: Vec<ManifestMount>,

    /// Network configuration. Default is `disable` (jail has no
    /// network). See `docs/spec/network.md`.
    #[serde(default)]
    pub network: ManifestNetwork,

    pub exec: Option<ManifestExec>,

    #[serde(default)]
    pub supervision: Supervision,
}

fn default_true() -> bool { true }

/// Restart policy for exec'd services (persistent-jail entries
/// without exec are launched once and not supervised).
///
/// Two orthogonal limits prevent runaway behaviour:
///
/// - **`max_restarts_per_minute`** is a *soft rate limit*.
///   Hitting it pauses restarts on the service for
///   `cooldown_after_burst_secs`, then retries resume. NOT a
///   give-up threshold; just CPU thrashing protection.
/// - **`failure_budget_secs`** is a *give-up threshold* that
///   scales naturally with how badly the service is failing.
///   See "Budget model" below.
///
/// ## Budget model
///
/// Each failed exit consumes failure-budget proportional to how
/// short its lifetime was vs `min_lifetime_for_success_secs`. A
/// successful run (alive ≥ `min_lifetime_for_success_secs`)
/// refills the budget to full.
///
/// ```text
/// on exit, with lifetime = now - launched_at:
///   if lifetime >= min_lifetime_for_success_secs:
///     budget_remaining = failure_budget_secs       // refill
///   else:
///     deficit = min_lifetime_for_success_secs - lifetime
///     budget_remaining -= deficit
///     if budget_remaining <= 0:
///       PERMANENTLY FAILED — give up
/// ```
///
/// Effect: faster fails consume more budget per attempt, so they
/// run out sooner. Slower fails consume less, so they get more
/// retries. With defaults `min = 5 s`, `budget = 25 s`:
///
/// | lifetime per attempt | retries before give-up |
/// |----------------------|------------------------|
/// | 0.1 s                | ~5                     |
/// | 1 s                  | ~6                     |
/// | 3 s                  | ~12                    |
/// | 4 s                  | 25                     |
/// | ≥ 5 s                | unlimited (refills)    |
///
/// Setting `failure_budget_secs = 0` disables the give-up
/// entirely — equivalent to systemd `StartLimitBurst=0`. Use
/// only for services where infinite restart is genuinely
/// desired.
///
/// systemd's equivalent is `StartLimitBurst` +
/// `StartLimitInterval` + an internal "start-limit-hit" failure;
/// our model differs in that the cost-per-failure scales with
/// lifetime instead of being a constant count over a window —
/// catches "survives 4 s every time" patterns that a simple
/// count-based limit would let run forever.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Supervision {
    #[serde(default = "default_restart")]
    pub restart: RestartPolicy,
    /// Cooldown between an exit and the next restart attempt.
    /// Defaults to 1 second; 0 means "immediately."
    #[serde(default = "default_restart_after_secs")]
    pub restart_after_secs: u64,
    /// Soft rate cap. If a service restarts more often than this
    /// in a 60-second window, the supervisor pauses restarts on
    /// it for `cooldown_after_burst_secs`. NOT a give-up
    /// threshold; just thrashing protection.
    #[serde(default = "default_max_restarts_per_minute")]
    pub max_restarts_per_minute: u32,
    #[serde(default = "default_cooldown_after_burst_secs")]
    pub cooldown_after_burst_secs: u64,

    /// Total "unhealthy-lifetime deficit" the supervisor will
    /// tolerate before giving up. Per-failure deficit =
    /// `min_lifetime_for_success_secs - actual_lifetime` (only
    /// for fast-failures; healthy exits are free + refill).
    /// Setting to 0 disables give-up. Default 25.
    #[serde(default = "default_failure_budget_secs")]
    pub failure_budget_secs: u64,
    /// A service that survives at least this many seconds after
    /// launch is considered to have started successfully; the
    /// failure budget refills to full. Default 5 s.
    #[serde(default = "default_min_lifetime_for_success_secs")]
    pub min_lifetime_for_success_secs: u64,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            restart: default_restart(),
            restart_after_secs: default_restart_after_secs(),
            max_restarts_per_minute: default_max_restarts_per_minute(),
            cooldown_after_burst_secs: default_cooldown_after_burst_secs(),
            failure_budget_secs: default_failure_budget_secs(),
            min_lifetime_for_success_secs: default_min_lifetime_for_success_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Restart unconditionally on any exit.
    Always,
    /// Restart only on non-zero exit / signal-induced exit.
    OnFailure,
    /// Never restart. The service runs once.
    Never,
}

fn default_restart() -> RestartPolicy { RestartPolicy::OnFailure }
fn default_restart_after_secs() -> u64 { 1 }
fn default_max_restarts_per_minute() -> u32 { 5 }
fn default_cooldown_after_burst_secs() -> u64 { 30 }
fn default_failure_budget_secs() -> u64 { 25 }
fn default_min_lifetime_for_success_secs() -> u64 { 5 }

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestMount {
    pub source: String,
    pub dest:   String,
    pub kind:   MountKindStr,
}

/// Local enum so the manifest TOML can use snake_case strings
/// directly. Maps 1:1 to `jaild::protocol::MountKind` for the
/// outbound request.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKindStr {
    RoNullfs,
    RwNullfs,
    Tmpfs,
}

impl From<MountKindStr> for MountKind {
    fn from(k: MountKindStr) -> Self {
        match k {
            MountKindStr::RoNullfs => MountKind::RoNullfs,
            MountKindStr::RwNullfs => MountKind::RwNullfs,
            MountKindStr::Tmpfs    => MountKind::Tmpfs,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ManifestNetwork {
    #[default]
    Disable,
    Lo0Alias { addr: String },
}

impl From<&ManifestNetwork> for NetworkConfig {
    fn from(n: &ManifestNetwork) -> Self {
        match n {
            ManifestNetwork::Disable           => NetworkConfig::Disable,
            ManifestNetwork::Lo0Alias { addr } => NetworkConfig::Lo0Alias {
                addr: addr.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestExec {
    pub path: String,
    pub argv: Vec<String>,
    pub uid:  u32,
    pub gid:  u32,
    #[serde(default)]
    pub env:  Vec<ManifestEnv>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestEnv {
    pub key:   String,
    pub value: String,
}

impl ServiceManifest {
    pub fn load(path: &Path) -> io::Result<Self> {
        let s = fs::read_to_string(path)?;
        let m: ServiceManifest = toml::from_str(&s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("parse {}: {e}", path.display())))?;
        Ok(m)
    }

    /// Translate to the wire-protocol type.
    pub fn to_create_request(&self) -> CreateJailRequest {
        CreateJailRequest {
            name:          self.name.clone(),
            path:          self.path.clone(),
            children_max:  self.children_max,
            devfs_ruleset: self.devfs_ruleset,
            network:       (&self.network).into(),
            mounts: self.mounts.iter().map(|m| MountSpec {
                source: m.source.clone(),
                dest:   m.dest.clone(),
                kind:   m.kind.into(),
            }).collect(),
            exec: self.exec.as_ref().map(|e| ExecSpec {
                path: e.path.clone(),
                argv: e.argv.clone(),
                uid:  e.uid,
                gid:  e.gid,
                env:  e.env.iter().map(|p| EnvPair {
                    key:   p.key.clone(),
                    value: p.value.clone(),
                }).collect(),
            }),
        }
    }
}

/// Read every `.toml` file under `dir`, parse, sort by filename
/// for stable boot order. Files that fail to parse are reported
/// in the returned error list (loader doesn't half-boot — the
/// caller decides whether to abort).
pub fn load_dir(dir: &Path) -> io::Result<LoadOutcome> {
    let mut by_filename: BTreeMap<String, ServiceManifest> = BTreeMap::new();
    let mut errors: Vec<(PathBuf, io::Error)> = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadOutcome { manifests: Vec::new(), errors });
        }
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = match entry { Ok(e) => e, Err(e) => { errors.push((dir.into(), e)); continue; } };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None    => continue,
        };
        match ServiceManifest::load(&path) {
            Ok(m)  => { by_filename.insert(fname, m); }
            Err(e) => errors.push((path, e)),
        }
    }

    Ok(LoadOutcome {
        manifests: by_filename.into_values().collect(),
        errors,
    })
}

pub struct LoadOutcome {
    pub manifests: Vec<ServiceManifest>,
    pub errors:    Vec<(PathBuf, io::Error)>,
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
            name = "atrium-foo"
            path = "/"
        "#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.name, "atrium-foo");
        assert!(m.enabled);
        assert!(m.exec.is_none());
        assert_eq!(m.mounts.len(), 0);
    }

    #[test]
    fn parse_full() {
        let s = r#"
            name = "atrium-frescod"
            path = "/"
            children_max = 0
            devfs_ruleset = 6

            [[mounts]]
            source = "/usr/local/lib"
            dest   = "usr/local/lib"
            kind   = "ro_nullfs"

            [exec]
            path = "/usr/local/bin/atrium-frescod"
            argv = ["atrium-frescod"]
            uid  = 1001
            gid  = 1001

            [[exec.env]]
            key = "PATH"
            value = "/bin:/usr/bin"
        "#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.devfs_ruleset, 6);
        assert_eq!(m.mounts.len(), 1);
        assert!(matches!(m.mounts[0].kind, MountKindStr::RoNullfs));
        let e = m.exec.as_ref().unwrap();
        assert_eq!(e.uid, 1001);
        assert_eq!(e.env.len(), 1);
    }

    #[test]
    fn to_create_request_round_trip() {
        let m = ServiceManifest {
            enabled: true,
            name: "atrium-x".into(),
            path: "/".into(),
            children_max: 0,
            devfs_ruleset: 0,
            mounts: vec![ManifestMount {
                source: "/usr/local/lib".into(),
                dest:   "usr/local/lib".into(),
                kind:   MountKindStr::RoNullfs,
            }],
            network: ManifestNetwork::Disable,
            exec: None,
            supervision: Supervision::default(),
        };
        let req = m.to_create_request();
        assert_eq!(req.name, "atrium-x");
        assert_eq!(req.mounts.len(), 1);
        assert!(matches!(req.mounts[0].kind, MountKind::RoNullfs));
    }

    #[test]
    fn load_dir_sorted_by_name() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("20-bbb.toml"),
              "name = \"atrium-bbb\"\npath = \"/\"\n");
        write(&dir.path().join("10-aaa.toml"),
              "name = \"atrium-aaa\"\npath = \"/\"\n");
        write(&dir.path().join("not-a-toml.txt"), "junk");
        let out = load_dir(dir.path()).unwrap();
        assert_eq!(out.manifests.len(), 2);
        assert_eq!(out.manifests[0].name, "atrium-aaa");
        assert_eq!(out.manifests[1].name, "atrium-bbb");
        assert!(out.errors.is_empty());
    }

    #[test]
    fn load_dir_reports_parse_errors_without_aborting() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("00-good.toml"),
              "name = \"atrium-good\"\npath = \"/\"\n");
        write(&dir.path().join("01-bad.toml"),
              "this is not toml = = = =");
        let out = load_dir(dir.path()).unwrap();
        assert_eq!(out.manifests.len(), 1);
        assert_eq!(out.errors.len(), 1);
    }

    #[test]
    fn supervision_defaults() {
        let s = r#"name = "atrium-x"
                   path = "/""#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.supervision.restart, RestartPolicy::OnFailure);
        assert_eq!(m.supervision.restart_after_secs, 1);
        assert_eq!(m.supervision.max_restarts_per_minute, 5);
    }

    #[test]
    fn supervision_parse() {
        let s = r#"
            name = "atrium-x"
            path = "/"
            [supervision]
            restart = "always"
            restart_after_secs = 5
            max_restarts_per_minute = 10
        "#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.supervision.restart, RestartPolicy::Always);
        assert_eq!(m.supervision.restart_after_secs, 5);
    }

    #[test]
    fn supervision_give_up_fields_default() {
        let s = r#"name = "x"
                   path = "/""#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.supervision.failure_budget_secs, 25);
        assert_eq!(m.supervision.min_lifetime_for_success_secs, 5);
    }

    #[test]
    fn supervision_give_up_fields_parse() {
        let s = r#"
            name = "x"
            path = "/"
            [supervision]
            failure_budget_secs = 10
            min_lifetime_for_success_secs = 2
        "#;
        let m: ServiceManifest = toml::from_str(s).unwrap();
        assert_eq!(m.supervision.failure_budget_secs, 10);
        assert_eq!(m.supervision.min_lifetime_for_success_secs, 2);
    }

    #[test]
    fn load_dir_missing_is_ok() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("not-here");
        let out = load_dir(&nonexistent).unwrap();
        assert!(out.manifests.is_empty());
    }
}

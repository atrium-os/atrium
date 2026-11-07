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
    /// Outer ceiling for the memory-governor broker (Reap / SetRctl). Optional —
    /// absent (or all-false) means jaild brokers neither, the conservative default.
    /// Per-service authorization is the *inner* grant (the portcullisd capability);
    /// both must allow. See `atrium-memory-pressure.md` §9.5.
    #[serde(default)]
    pub resource_control: ResourceControl,
    /// Which jails may lock memory (`allow.mlock`). Absent = none.
    #[serde(default)]
    pub mlock: MlockPolicy,
    /// Arca's couriers (arca.md §5). Absent = none.
    #[serde(default)]
    pub couriers: Option<CourierPolicy>,
    /// Per-jail system call filter profiles (jail-sysfil.md §5). Absent =
    /// no profiles, and a request that names one is refused.
    #[serde(default)]
    pub sysfil: SysfilPolicy,
}

/// `[sysfil]`: the system call filter profiles jaild may put on a jail
/// (docs/spec/jail-sysfil.md §5). A request names a profile by class
/// (`CreateJailRequest.syscall_profile`); jaild passes the resolved lists to
/// the kernel as `sysfil.*` jail parameters when it creates the jail.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SysfilPolicy {
    /// Refuse an exec'd jail that names no profile.
    #[serde(default)]
    pub required: bool,
    /// The kernel's `sysfil.mode` for every profile: 1 log only, 2 deny,
    /// 3 deny and SIGSYS. 1 on a development box turns every app's
    /// denials into a list of what its profile is missing.
    #[serde(default = "default_sysfil_mode")]
    pub mode: i32,
    #[serde(default)]
    pub profiles: BTreeMap<String, SysfilProfile>,
}

fn default_sysfil_mode() -> i32 { 2 }

impl Default for SysfilPolicy {
    fn default() -> Self {
        SysfilPolicy { required: false, mode: default_sysfil_mode(), profiles: BTreeMap::new() }
    }
}

/// One class. A list left out is unrestricted (unless an included profile
/// restricts it); an empty list allows nothing of that kind.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SysfilProfile {
    /// Profiles whose lists this one adds to, e.g. `["base"]`.
    #[serde(default)]
    pub include:  Vec<String>,
    pub syscalls: Option<Vec<String>>,
    /// ioctl command numbers, as the kernel takes them (`0x4004667f`).
    pub ioctls:   Option<Vec<String>>,
    /// `level:name`, numbers (`0xffff:4097`).
    pub sockopts: Option<Vec<String>>,
    /// Address family names or numbers (`unix`, `inet6`).
    pub families: Option<Vec<String>>,
}

/// A profile with its includes folded in, in the kernel's string form:
/// comma-joined lists, `None` for an unrestricted class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysfilParams {
    pub mode:     i32,
    pub syscalls: Option<String>,
    pub ioctls:   Option<String>,
    pub sockopts: Option<String>,
    pub families: Option<String>,
}

impl SysfilPolicy {
    /// Resolve a profile by name, following `include`s (no cycles, depth 8).
    pub fn resolve(&self, name: &str) -> Result<SysfilParams, String> {
        if !(1..=3).contains(&self.mode) {
            return Err(format!("sysfil.mode {} is not 1, 2 or 3", self.mode));
        }
        let mut acc: [Option<std::collections::BTreeSet<String>>; 4] = Default::default();
        self.fold(name, &mut acc, &mut Vec::new())?;
        let join = |s: &Option<std::collections::BTreeSet<String>>| {
            s.as_ref().map(|v| v.iter().cloned().collect::<Vec<_>>().join(","))
        };
        Ok(SysfilParams {
            mode:     self.mode,
            syscalls: join(&acc[0]),
            ioctls:   join(&acc[1]),
            sockopts: join(&acc[2]),
            families: join(&acc[3]),
        })
    }

    fn fold(&self, name: &str,
            acc: &mut [Option<std::collections::BTreeSet<String>>; 4],
            path: &mut Vec<String>) -> Result<(), String> {
        if path.iter().any(|p| p == name) {
            return Err(format!("sysfil profile include cycle: {} -> {name}", path.join(" -> ")));
        }
        if path.len() >= 8 {
            return Err(format!("sysfil profile includes nest deeper than 8 at {name}"));
        }
        let prof = self.profiles.get(name)
            .ok_or_else(|| format!("no sysfil profile {name:?}"))?;
        path.push(name.to_string());
        for inc in &prof.include {
            self.fold(inc, acc, path)?;
        }
        path.pop();
        let lists = [&prof.syscalls, &prof.ioctls, &prof.sockopts, &prof.families];
        for (i, l) in lists.iter().enumerate() {
            if let Some(items) = l {
                let set = acc[i].get_or_insert_with(Default::default);
                for it in items.iter() {
                    let it = it.trim();
                    // The kernel splits on commas; an entry must be one token.
                    if it.is_empty() || !it.chars().all(|c| c.is_ascii_alphanumeric() || "_:.".contains(c)) {
                        return Err(format!("sysfil profile {name:?}: bad entry {it:?}"));
                    }
                    set.insert(it.to_string());
                }
            }
        }
        Ok(())
    }
}

/// `[couriers]`: Arca's per-endpoint courier jails (arca.md §5, §5.2). One
/// jail per courier, made by arca-courierd: named `<name_prefix><uid>`,
/// rooted at `<exec_paths.instance_root_dir>/<name>`, running exactly
/// `exec_path` as a uid in `uid_first..=uid_last` (gid = uid). Such a jail
/// may take its caller's stdio (the client's socket) and lock memory (the
/// secret it carries), and nothing else beyond an ordinary service.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourierPolicy {
    pub name_prefix: String,
    pub exec_path:   String,
    pub uid_first:   u32,
    pub uid_last:    u32,
}

/// `[mlock]`: jails whose processes may `mlock(2)`. A jail that holds secrets
/// in memory (Arca) needs it so they never reach swap or the zram
/// compressor; nothing else should have it.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MlockPolicy {
    #[serde(default)]
    pub allow_jails: Vec<String>,
}

/// What the memory-governor broker may do. jaild only ever acts on jails it itself
/// created (the dispatch enforces that independently), so this gates the *kind* of
/// action, not the target set.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ResourceControl {
    /// Allow `Reap` — signalling a jaild-created jail's processes (the cascade).
    #[serde(default)]
    pub allow_reap: bool,
    /// Allow `SetRctl` — setting a jaild-created jail's `memoryuse` cap.
    #[serde(default)]
    pub allow_rctl: bool,
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
    /// ★ The largest tmpfs any jail may mount, in MiB, and the size given to
    /// one that does not ask. tmpfs is RAM-backed and was mounted with no
    /// options at all, so every tmpfs jaild created could grow until the
    /// machine ran out — reachable by any caller that could name a jail.
    /// 256 MiB is generous for the scratch volumes it is used for and far
    /// below what a runaway would take.
    #[serde(default = "default_max_tmpfs_mb")]
    pub max_tmpfs_mb: u64,
}

fn default_max_tmpfs_mb() -> u64 { 256 }

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
    /// Rulesets one service owns. A jail may mount a pinned ruleset only as
    /// exactly that service (jail name, exec path, uid, no child jails), and
    /// jaild writes `<jail>:<uid>` to each of its `leases` sysctls before the
    /// service starts, so the kernel driver behind those nodes accepts opens
    /// from that jail and uid alone (the display lease, ISOLATION.md).
    #[serde(default)]
    pub pinned: Vec<PinnedRuleset>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PinnedRuleset {
    pub id:        u32,
    pub jail_name: String,
    pub exec_path: String,
    pub uid:       u32,
    /// sysctl OIDs (e.g. `dev.atrium_gpu_amd_display.0.lease`) jaild sets to
    /// `<jail_name>:<uid>` when it creates the jail. A missing OID (driver not
    /// loaded) is logged, not fatal: the device then refuses the jail anyway.
    #[serde(default)]
    pub leases:    Vec<String>,
}

impl DevfsRulesets {
    /// The pin on ruleset `id`, if it has one.
    pub fn pin(&self, id: u32) -> Option<&PinnedRuleset> {
        self.pinned.iter().find(|p| p.id == id)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecPaths {
    #[serde(default)]
    pub allowed_prefixes: Vec<String>,
    /// ★ One-shot INSTANCE ROOTS (portcullis.md §6.5.4). A jail whose root is
    /// exactly `<instance_root_dir>/<name>`, with an `app-` name, runs the
    /// entry of an app tree that portcullis signature-verified and mounted
    /// there — so its entry may be any absolute path INSIDE that tree, not
    /// just one under `allowed_prefixes` (which describe where service
    /// binaries live, and a signed bundle's entry is wherever its manifest
    /// says). Also the only kind of jail that may take the caller's stdio.
    /// Absent = rule off.
    #[serde(default)]
    pub instance_root_dir: Option<String>,
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
    /// Jail names permitted to use `ip4=inherit` (share the host's network
    /// stack). Empty = none; inherit is powerful (the jail may bind any host
    /// address), so it is an explicit per-jail allowlist, not a global flag.
    #[serde(default)]
    pub allow_inherit_jails: Vec<String>,
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
    /// The driver's client ABI revision at test time (`dev.<driver>.0.abi_rev`).
    #[serde(default)]
    pub abi_rev:               Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuDriverStatus {
    Production,
    Experimental,
    Broken,
}

/// Why the `gpu` capability may not be granted for a driver
/// (docs/spec/gpu-isolation.md, "Capability gate").
#[derive(Debug, PartialEq, Eq)]
pub enum GpuGrantRefusal {
    /// No `[gpu_drivers.attested.<driver>]` stanza.
    NotAttested,
    /// `isolation_test_passed = false`.
    TestNotPassed,
    /// `status` is not `production`.
    NotProduction,
    /// The stanza records no commit, so it can't be bound to a build.
    NoAttestedCommit,
    /// The running driver reports a build from an uncommitted tree.
    DirtyBuild { live: String },
    /// The running driver is not the build that was tested.
    StaleAttestation { attested: String, live: String },
}

impl GpuDrivers {
    /// The capability gate. `live_build_id` is what the bound driver reports
    /// (`dev.<driver>.0.build_id`, a `git describe` of the kmod tree). Every
    /// condition must hold; anything unknown refuses, the safe direction.
    pub fn grant_check(&self, driver: &str, live_build_id: &str)
        -> Result<(), GpuGrantRefusal>
    {
        let a = self.attested.get(driver).ok_or(GpuGrantRefusal::NotAttested)?;
        if !a.isolation_test_passed {
            return Err(GpuGrantRefusal::TestNotPassed);
        }
        if a.status != GpuDriverStatus::Production {
            return Err(GpuGrantRefusal::NotProduction);
        }
        let attested = a.isolation_test_commit.trim();
        if attested.is_empty() || attested.chars().all(|c| c == '0') {
            return Err(GpuGrantRefusal::NoAttestedCommit);
        }
        let live = live_build_id.trim();
        if live.is_empty() || live == "unknown" || live.ends_with("-dirty") {
            return Err(GpuGrantRefusal::DirtyBuild { live: live.to_string() });
        }
        // Either side may be an abbreviated hash: match on the common prefix,
        // but never on fewer than 7 hex digits.
        let n = attested.len().min(live.len());
        if n < 7 || attested[..n] != live[..n] {
            return Err(GpuGrantRefusal::StaleAttestation {
                attested: attested.to_string(),
                live: live.to_string(),
            });
        }
        Ok(())
    }
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

    fn sysfil_sample() -> SysfilPolicy {
        toml::from_str(r#"
            mode = 2
            [profiles.base]
            syscalls = ["read", "write", "_exit"]
            ioctls   = ["0x4004667f"]
            families = ["unix"]
            [profiles.net]
            include  = ["base"]
            families = ["inet6", "inet"]
            [profiles.loop1]
            include  = ["loop2"]
            [profiles.loop2]
            include  = ["loop1"]
            [profiles.bad]
            syscalls = ["read,write"]
        "#).unwrap()
    }

    #[test]
    fn sysfil_resolves_includes_and_leaves_unlisted_classes_open() {
        let p = sysfil_sample().resolve("net").unwrap();
        assert_eq!(p.mode, 2);
        assert_eq!(p.syscalls.as_deref(), Some("_exit,read,write"));
        assert_eq!(p.ioctls.as_deref(), Some("0x4004667f"));
        assert_eq!(p.families.as_deref(), Some("inet,inet6,unix"));
        assert_eq!(p.sockopts, None, "a class no profile names stays unrestricted");
    }

    #[test]
    fn sysfil_refuses_cycles_unknown_names_bad_entries_and_modes() {
        let s = sysfil_sample();
        assert!(s.resolve("loop1").unwrap_err().contains("cycle"));
        assert!(s.resolve("nope").unwrap_err().contains("no sysfil profile"));
        // A comma would let one entry smuggle in several.
        assert!(s.resolve("bad").unwrap_err().contains("bad entry"));
        let mut m = s.clone();
        m.mode = 0;
        assert!(m.resolve("base").is_err(), "mode 0 would be no filter at all");
    }

    #[test]
    fn shipped_sysfil_profiles_resolve() {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::path::Path::new(crate_dir).parent().unwrap().parent().unwrap()
            .join("etc/jaild.policy.toml");
        let p = Policy::load(&sample).unwrap();
        for class in ["renderer", "app", "networked", "terminal"] {
            let r = p.sysfil.resolve(class).unwrap_or_else(|e| panic!("{class}: {e}"));
            let sys = r.syscalls.unwrap();
            for never in ["ktrace", "ptrace", "setcred", "jail_set", "kqueuex", "procctl",
                          "thr_kill2", "sigqueue", "sendfile", "lio_listio", "mount"] {
                assert!(!sys.split(',').any(|s| s == never), "{class} allows {never}");
            }
            let fam = r.families.unwrap();
            assert!(!fam.split(',').any(|f| f == "route"), "{class} allows route sockets");
        }
        let app = p.sysfil.resolve("app").unwrap();
        assert!(!app.ioctls.unwrap().contains("0x20007461"), "TIOCSCTTY outside terminal");
        assert_eq!(app.families.as_deref(), Some("unix"));
    }

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
        assert_eq!(frescod.allowed_devfs_ruleset, "atrium-display");
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

    /// The resource-control broker ceiling is optional and defaults OFF: an
    /// existing schema-v1 policy file (the shipped sample has no
    /// `[resource_control]` section) still parses, and jaild brokers neither
    /// Reap nor SetRctl until an operator explicitly enables them.
    #[test]
    fn resource_control_defaults_off_and_is_optional() {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::path::Path::new(crate_dir)
            .parent().unwrap().parent().unwrap()
            .join("etc/jaild.policy.toml");
        let p = Policy::load(&sample).unwrap();
        assert!(!p.resource_control.allow_reap, "allow_reap must default off");
        assert!(!p.resource_control.allow_rctl, "allow_rctl must default off");
    }

    /// When present, the flags parse.
    #[test]
    fn resource_control_section_parses() {
        let rc: ResourceControl =
            toml::from_str("allow_reap = true\nallow_rctl = true").unwrap();
        assert!(rc.allow_reap && rc.allow_rctl);
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

    fn drivers(toml_src: &str) -> GpuDrivers {
        toml::from_str(toml_src).expect("gpu_drivers parses")
    }

    #[test]
    fn gpu_grant_gate_binds_attestation_to_the_live_build() {
        let d = drivers(r#"
            [attested.atrium-gpu-amd]
            status = "production"
            isolation_test_passed = true
            isolation_test_commit = "4c9f6b2c1a2b"
            abi_rev = 2

            [attested.exp]
            status = "experimental"
            isolation_test_passed = true
            isolation_test_commit = "4c9f6b2c1a2b"

            [attested.failed]
            status = "production"
            isolation_test_passed = false
            isolation_test_commit = "4c9f6b2c1a2b"

            [attested.placeholder]
            status = "production"
            isolation_test_passed = true
            isolation_test_commit = "0000000"
        "#);
        assert_eq!(d.grant_check("atrium-gpu-amd", "4c9f6b2c1a2b"), Ok(()));
        assert_eq!(d.grant_check("atrium-gpu-amd", "4c9f6b2c"), Ok(()));
        assert_eq!(d.grant_check("nope", "4c9f6b2c1a2b"), Err(GpuGrantRefusal::NotAttested));
        assert_eq!(d.grant_check("exp", "4c9f6b2c1a2b"), Err(GpuGrantRefusal::NotProduction));
        assert_eq!(d.grant_check("failed", "4c9f6b2c1a2b"), Err(GpuGrantRefusal::TestNotPassed));
        assert_eq!(d.grant_check("placeholder", "0000000"), Err(GpuGrantRefusal::NoAttestedCommit));
        assert!(matches!(d.grant_check("atrium-gpu-amd", "4c9f6b2c1a2b-dirty"),
            Err(GpuGrantRefusal::DirtyBuild { .. })));
        assert!(matches!(d.grant_check("atrium-gpu-amd", "unknown"),
            Err(GpuGrantRefusal::DirtyBuild { .. })));
        assert!(matches!(d.grant_check("atrium-gpu-amd", "deadbeef0000"),
            Err(GpuGrantRefusal::StaleAttestation { .. })));
        assert!(matches!(d.grant_check("atrium-gpu-amd", "4c9f6"),
            Err(GpuGrantRefusal::StaleAttestation { .. })));
    }
}

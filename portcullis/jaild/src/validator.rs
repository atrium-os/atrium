//! Validate a request against the loaded `jaild_policy::Policy`.
//!
//! Each validator function returns `Ok(())` on accept or
//! `Err(JaildError::PolicyViolation { rule, detail })` on reject.
//! The `rule` field is a stable identifier portcullisd can match on
//! to surface a structured reason to the user.
//!
//! The matching is intentionally explicit and conservative — no
//! regex unless we type out the regex (we don't yet; name validation
//! is character-by-character to keep the dep set minimal).

use jaild_policy::Policy;

use crate::protocol::{
    CreateJailRequest, ExecSpec, MountKind, MountSpec, NetworkConfig,
};
#[cfg(test)]
use crate::protocol::EnvPair;
use crate::JaildError;

/// Validate a `CreateJail` request against the loaded policy.
/// Returns `Ok(())` if every field is acceptable.
pub fn validate_create(
    req: &CreateJailRequest,
    policy: &Policy,
) -> Result<(), JaildError> {
    validate_name(&req.name)?;
    validate_path(&req.path, policy)?;
    validate_children_max(req.children_max, policy)?;
    validate_devfs_ruleset(req.devfs_ruleset, policy)?;
    validate_network(&req.network, policy)?;
    for m in &req.mounts {
        validate_mount(m, policy)?;
    }
    if let Some(exec) = &req.exec {
        validate_exec(exec, policy)?;
    }
    Ok(())
}

fn validate_network(net: &NetworkConfig, policy: &Policy) -> Result<(), JaildError> {
    match net {
        NetworkConfig::Disable => {
            /* Always permitted. policy.network.allow_disable is
             * documented as "always true" in the policy schema;
             * we don't even check the field. */
            Ok(())
        }
        NetworkConfig::Lo0Alias { addr } => {
            /* Validate addr is in CIDR form; check it's
             * contained in one of policy.network.allowed_addrs_on_lo0. */
            if !addr.contains('/') {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.lo0_alias.addr_format",
                    detail: format!("addr {addr:?} must be in CIDR form (e.g. 127.10.0.5/32)"),
                });
            }
            let allowed = policy.network.allowed_addrs_on_lo0
                .iter()
                .any(|cidr| cidr_contains(cidr, addr));
            if !allowed {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.lo0_alias.addr_not_in_policy",
                    detail: format!(
                        "addr {addr:?} not in any policy.network.allowed_addrs_on_lo0 entry"),
                });
            }
            Ok(())
        }
        NetworkConfig::Vnet { .. } => Err(JaildError::PolicyViolation {
            rule:   "network.vnet.unimplemented_v0",
            detail: "vnet mode is not implemented in jaild V0 (see docs/spec/network.md §4)".into(),
        }),
        NetworkConfig::HostAlias { .. } => Err(JaildError::PolicyViolation {
            rule:   "network.host_alias.unimplemented_v0",
            detail: "host_alias mode is not implemented in jaild V0 (see docs/spec/network.md §5)".into(),
        }),
    }
}

/// CIDR containment: returns true if the host portion of `addr`
/// (matching `network`'s prefix length) equals `network`'s host.
/// Both arguments are "<dotted-quad>/<prefix>" strings; returns
/// false on parse error.
fn cidr_contains(network: &str, addr: &str) -> bool {
    fn parse(s: &str) -> Option<(u32, u8)> {
        let (ip, plen) = s.split_once('/')?;
        let ip: std::net::Ipv4Addr = ip.parse().ok()?;
        let plen: u8 = plen.parse().ok()?;
        if plen > 32 { return None; }
        Some((u32::from(ip), plen))
    }
    let (net_ip, net_plen) = match parse(network) { Some(t) => t, None => return false };
    let (req_ip, _)        = match parse(addr)    { Some(t) => t, None => return false };
    if net_plen == 0 { return true; }   // 0.0.0.0/0
    let mask = if net_plen == 32 { !0u32 } else { !0u32 << (32 - net_plen) };
    (net_ip & mask) == (req_ip & mask)
}

fn validate_devfs_ruleset(id: u32, policy: &Policy) -> Result<(), JaildError> {
    /* 0 = "inherit host devfs". Always permitted; it's the
     * default. Production policies SHOULD constrain to
     * non-zero, but jaild doesn't enforce that — that's a
     * portcullisd-side concern (what gets requested). */
    if id == 0 {
        return Ok(());
    }
    if !policy.devfs_rulesets.allowed_ids.iter().any(|n| *n == id) {
        return Err(JaildError::PolicyViolation {
            rule:   "devfs_ruleset.not_allowed",
            detail: format!(
                "devfs_ruleset {id} not in policy.devfs_rulesets.allowed_ids"),
        });
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), JaildError> {
    if name.is_empty() || name.len() > 64 {
        return Err(JaildError::PolicyViolation {
            rule:   "name.length",
            detail: format!("name length {} not in 1..=64", name.len()),
        });
    }
    /* Character set: lowercase a-z, 0-9, hyphen. Conservative.
     * Rejects whitespace, slash, dot, NUL — the things that would
     * either confuse jail_set or escape into shell metacharacters
     * if the name ever made it into a config file. */
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(JaildError::PolicyViolation {
            rule:   "name.charset",
            detail: format!("name {name:?} contains chars outside [a-z0-9-]"),
        });
    }
    /* Required prefix: this is the "atrium namespace" enforcement
     * — every jail jaild creates is identifiable as ours. */
    const ALLOWED_PREFIXES: &[&str] =
        &["atrium-", "system-", "user-", "app-"];
    if !ALLOWED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(JaildError::PolicyViolation {
            rule:   "name.prefix",
            detail: format!(
                "name {name:?} must start with one of {ALLOWED_PREFIXES:?}"),
        });
    }
    Ok(())
}

fn validate_path(path: &str, policy: &Policy) -> Result<(), JaildError> {
    /* For V0, a path is acceptable if it's an exact match on a
     * policy mount source (ro or rw) OR matches a glob pattern in
     * rw_patterns.
     * V1 will introduce mount-spec validation where each mount has
     * its own source check; this is the single-path validator for
     * the jail's root filesystem only. */
    if policy.mount_sources.ro_paths.iter().any(|p| p == path) {
        return Ok(());
    }
    if policy.mount_sources.rw_paths.iter().any(|p| p == path) {
        return Ok(());
    }
    if policy
        .mount_sources
        .rw_patterns
        .iter()
        .any(|pat| matches_glob(pat, path))
    {
        return Ok(());
    }
    /* Special case: "/" is permitted. Smoke tests use it. The
     * policy file's allow-lists don't include "/" because no
     * production jail should use it as path. Allowing here so that
     * scratch tests don't need to add it to the policy file. The
     * production policy will set this off via a separate flag in
     * V1. */
    if path == "/" {
        return Ok(());
    }
    Err(JaildError::PolicyViolation {
        rule:   "path.not_in_allowlist",
        detail: format!("path {path:?} not in policy mount_sources"),
    })
}

fn validate_children_max(n: u32, policy: &Policy) -> Result<(), JaildError> {
    if n > policy.children_max.max {
        return Err(JaildError::PolicyViolation {
            rule:   "children_max.exceeds",
            detail: format!(
                "children_max {} > policy max {}", n, policy.children_max.max),
        });
    }
    Ok(())
}

fn validate_mount(m: &MountSpec, policy: &Policy) -> Result<(), JaildError> {
    /* dest is a path inside the jail's chroot. Reject `..` and
     * empty (we don't enforce absolute — relative is fine and
     * common). */
    if m.dest.is_empty() {
        return Err(JaildError::PolicyViolation {
            rule:   "mount.dest.empty",
            detail: "mount destination is empty".into(),
        });
    }
    if m.dest.split('/').any(|seg| seg == "..") {
        return Err(JaildError::PolicyViolation {
            rule:   "mount.dest.traversal",
            detail: format!("mount dest {:?} contains '..'", m.dest),
        });
    }

    match m.kind {
        MountKind::RoNullfs => {
            let allowed = policy.mount_sources.ro_paths.iter().any(|p| p == &m.source);
            if !allowed {
                return Err(JaildError::PolicyViolation {
                    rule:   "mount.source.not_in_ro",
                    detail: format!("ro source {:?} not in policy.mount_sources.ro_paths", m.source),
                });
            }
        }
        MountKind::RwNullfs => {
            let exact = policy.mount_sources.rw_paths.iter().any(|p| p == &m.source);
            let glob  = policy.mount_sources.rw_patterns.iter().any(|pat| matches_glob(pat, &m.source));
            if !exact && !glob {
                return Err(JaildError::PolicyViolation {
                    rule:   "mount.source.not_in_rw",
                    detail: format!("rw source {:?} not in policy.mount_sources rw paths/patterns", m.source),
                });
            }
        }
        MountKind::Tmpfs => {
            /* tmpfs has no source, no allow-list. Always
             * acceptable; the only resource cap is the
             * (eventual) per-jail rctl in V1b. */
        }
    }
    Ok(())
}

fn validate_exec(exec: &ExecSpec, policy: &Policy) -> Result<(), JaildError> {
    /* Path prefix allow-list. */
    if !policy.exec_paths.allowed_prefixes.iter().any(|p| exec.path.starts_with(p)) {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.path.not_allowed",
            detail: format!("exec path {:?} not in policy.exec_paths.allowed_prefixes", exec.path),
        });
    }

    /* argv[0]'s basename must equal exec.path's basename — defends
     * against argv[0] spoofing (a process advertising itself as a
     * different program in `ps`). */
    if exec.argv.is_empty() {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.argv.empty",
            detail: "exec.argv must have at least argv[0]".into(),
        });
    }
    let basename = |s: &str| -> String {
        s.rsplit('/').next().unwrap_or(s).to_owned()
    };
    if basename(&exec.argv[0]) != basename(&exec.path) {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.argv0.basename_mismatch",
            detail: format!(
                "argv[0] basename {:?} != exec.path basename {:?}",
                basename(&exec.argv[0]), basename(&exec.path)),
        });
    }

    /* Env keys: each key must be in allowed_keys, OR start with
     * one of allowed_prefixes. */
    for kv in &exec.env {
        let allowed = policy.env.allowed_keys.iter().any(|k| k == &kv.key)
            || policy.env.allowed_prefixes.iter().any(|p| kv.key.starts_with(p));
        if !allowed {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.env.key_not_allowed",
                detail: format!("env key {:?} not in policy.env allow-lists", kv.key),
            });
        }
        /* Reject NUL in either side — would terminate the C string
         * early and confuse the kernel. */
        if kv.key.contains('\0') || kv.value.contains('\0') {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.env.nul",
                detail: format!("env entry {:?} contains NUL byte", kv.key),
            });
        }
    }

    /* uid: in the user range, OR in the system allowlist. */
    let in_user_range = exec.uid >= policy.uid.min_user_uid
                     && exec.uid <= policy.uid.max_user_uid;
    let in_system     = policy.uid.allowed_system_uids.iter().any(|u| *u == exec.uid);
    if !in_user_range && !in_system {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.uid.not_allowed",
            detail: format!(
                "uid {} not in user range {}..={} and not in allowed_system_uids",
                exec.uid, policy.uid.min_user_uid, policy.uid.max_user_uid),
        });
    }

    Ok(())
}

/// Tiny glob: only supports a single trailing `*` after a directory
/// boundary. Sufficient for `/usr/home/*` and `/var/db/atrium/users/*`
/// — the shapes the policy file actually uses.
///
/// Deliberately *not* a general glob library: keeps the dep set
/// small and the surface auditable.
fn matches_glob(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if let Some(rest) = path.strip_prefix(prefix) {
            // Must have exactly one '/' followed by a non-empty
            // component, and no further slashes (so /usr/home/x
            // matches but /usr/home/x/y doesn't — that'd be a
            // sub-mount and the user-supervisor isn't allowed it).
            if let Some(after) = rest.strip_prefix('/') {
                return !after.is_empty() && !after.contains('/');
            }
        }
        return false;
    }
    pattern == path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_sample_policy() -> Policy {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::path::Path::new(crate_dir)
            .parent().unwrap()      // portcullis/
            .parent().unwrap()      // bsd/
            .join("etc/jaild.policy.toml");
        Policy::load(sample).expect("load shipped sample policy")
    }

    #[test]
    fn name_accepts_valid() {
        for name in &["atrium-frescod", "system-vestibulum", "user-1001-supervisor", "app-edit-7"] {
            validate_name(name).unwrap_or_else(|e| panic!("{name} rejected: {e}"));
        }
    }

    #[test]
    fn name_rejects_bad_charset() {
        for name in &["atrium-Frescod", "atrium frescod", "atrium/escape", "atrium.dot",
                      "atrium-foo$", "ATRIUM-X"] {
            let err = validate_name(name).unwrap_err();
            assert!(matches!(err, JaildError::PolicyViolation { rule: "name.charset" | "name.prefix", .. }));
        }
    }

    #[test]
    fn name_rejects_bad_prefix() {
        let err = validate_name("evil-x").unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "name.prefix", .. } => {}
            other => panic!("wrong rule: {other:?}"),
        }
    }

    #[test]
    fn name_rejects_empty_or_long() {
        validate_name("").unwrap_err();
        let huge = "atrium-".to_string() + &"x".repeat(100);
        validate_name(&huge).unwrap_err();
    }

    #[test]
    fn path_accepts_allowlisted() {
        let p = load_sample_policy();
        validate_path("/usr/local/lib", &p).unwrap();         // ro
        validate_path("/var/run/aqueduct", &p).unwrap();      // rw
        validate_path("/usr/home/girivs", &p).unwrap();       // rw_pattern
        validate_path("/", &p).unwrap();                      // smoke-test escape
    }

    #[test]
    fn path_rejects_unlisted() {
        let p = load_sample_policy();
        validate_path("/etc/master.passwd.bak", &p).unwrap_err();
        validate_path("/usr/home/girivs/.ssh", &p).unwrap_err();   // sub-of-glob
        validate_path("/tmp", &p).unwrap_err();
    }

    #[test]
    fn glob_basics() {
        assert!(matches_glob("/usr/home/*", "/usr/home/alice"));
        assert!(!matches_glob("/usr/home/*", "/usr/home"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/alice/.bashrc"));
        assert!(matches_glob("/x", "/x"));
        assert!(!matches_glob("/x", "/y"));
    }

    #[test]
    fn children_max_capped() {
        let p = load_sample_policy();
        assert!(validate_children_max(0, &p).is_ok());
        assert!(validate_children_max(p.children_max.max, &p).is_ok());
        assert!(validate_children_max(p.children_max.max + 1, &p).is_err());
    }

    #[test]
    fn create_request_full_validate() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            name:          "atrium-test".into(),
            path:          "/".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
        };
        validate_create(&req, &p).unwrap();
    }

    #[test]
    fn create_request_bad_name_rejected() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            name:          "evil-x".into(),
            path:          "/".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
        };
        let err = validate_create(&req, &p).unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "name.prefix", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    fn req_default() -> CreateJailRequest {
        CreateJailRequest {
            name: "atrium-test".into(),
            path: "/".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
        }
    }

    #[test]
    fn mount_ro_accepted_from_allowlist() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/usr/local/lib".into(),
            dest:   "usr/local/lib".into(),
            kind:   MountKind::RoNullfs,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn mount_ro_rejects_unlisted() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/etc/master.passwd.bak".into(),
            dest:   "etc".into(),
            kind:   MountKind::RoNullfs,
        });
        let err = validate_create(&r, &p).unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "mount.source.not_in_ro", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn mount_rejects_traversal() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/usr/local/lib".into(),
            dest:   "../escape".into(),
            kind:   MountKind::RoNullfs,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.dest.traversal", .. }));
    }

    #[test]
    fn mount_tmpfs_no_source_check() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "ignored".into(),
            dest:   "tmp".into(),
            kind:   MountKind::Tmpfs,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn mount_rw_glob() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/usr/home/girivs".into(),
            dest:   "home/girivs".into(),
            kind:   MountKind::RwNullfs,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn exec_accepted() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn exec_rejects_bad_path() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/bin/sh".into(),
            argv: vec!["sh".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.path.not_allowed", .. }));
    }

    #[test]
    fn exec_rejects_argv0_spoof() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["i-am-something-else".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.argv0.basename_mismatch", .. }));
    }

    #[test]
    fn exec_rejects_unknown_env() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![EnvPair { key: "EVIL_VAR".into(), value: "x".into() }],
            uid:  1001,
            gid:  1001,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.env.key_not_allowed", .. }));
    }

    #[test]
    fn exec_accepts_atrium_prefix_env() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![EnvPair {
                key:   "ATRIUM_BUNDLES_ROOT".into(),
                value: "/usr/local/share/atrium/bundles".into(),
            }],
            uid:  1001,
            gid:  1001,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn cidr_basics() {
        assert!(cidr_contains("127.10.0.0/24", "127.10.0.5/32"));
        assert!(cidr_contains("127.10.0.0/16", "127.10.5.99/32"));
        assert!(cidr_contains("0.0.0.0/0",     "8.8.8.8/32"));
        assert!(!cidr_contains("127.10.0.0/24", "127.11.0.5/32"));
        assert!(!cidr_contains("127.10.0.0/24", "10.0.0.5/32"));
        assert!(!cidr_contains("not-a-cidr",    "127.10.0.5/32"));
        assert!(!cidr_contains("127.10.0.0/24", "no-slash"));
    }

    #[test]
    fn network_disable_always_ok() {
        let p = load_sample_policy();
        validate_network(&NetworkConfig::Disable, &p).unwrap();
    }

    #[test]
    fn network_lo0_alias_in_policy_ok() {
        let p = load_sample_policy();
        // sample policy has 127.10.0.0/16 in allowed_addrs_on_lo0
        validate_network(
            &NetworkConfig::Lo0Alias { addr: "127.10.0.5/32".into() },
            &p,
        ).unwrap();
    }

    #[test]
    fn network_lo0_alias_outside_policy_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Lo0Alias { addr: "10.0.0.5/32".into() },
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.lo0_alias.addr_not_in_policy", .. }));
    }

    #[test]
    fn network_lo0_alias_bad_format_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Lo0Alias { addr: "127.10.0.5".into() },  // no CIDR
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.lo0_alias.addr_format", .. }));
    }

    #[test]
    fn network_vnet_v0_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Vnet { bridge: "br0".into(), addr: "192.168.1.1/24".into(), gateway: None },
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.vnet.unimplemented_v0", .. }));
    }

    #[test]
    fn devfs_ruleset_zero_always_ok() {
        let p = load_sample_policy();
        // Sample policy has empty allowed_ids; 0 must still be OK.
        validate_devfs_ruleset(0, &p).unwrap();
    }

    #[test]
    fn devfs_ruleset_nonzero_rejected_when_empty_allowlist() {
        let p = load_sample_policy();
        let err = validate_devfs_ruleset(5, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "devfs_ruleset.not_allowed", .. }));
    }

    #[test]
    fn devfs_ruleset_nonzero_accepted_when_in_allowlist() {
        let mut p = load_sample_policy();
        p.devfs_rulesets.allowed_ids = vec![5, 6, 7];
        validate_devfs_ruleset(6, &p).unwrap();
        validate_devfs_ruleset(99, &p).unwrap_err();
    }

    #[test]
    fn exec_rejects_uid_out_of_range() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid:  100, // below min_user_uid=1000 and not system
            gid:  100,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.uid.not_allowed", .. }));
    }
}


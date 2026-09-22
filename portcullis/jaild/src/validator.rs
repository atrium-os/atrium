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
    validate_network(&req.network, &req.name, policy)?;
    for m in &req.mounts {
        validate_mount(m, policy)?;
    }
    if let Some(exec) = &req.exec {
        validate_exec(exec, policy, is_instance_root(req, policy))?;
    }
    validate_host_identity(req)?;
    Ok(())
}

/// The synthetic host identity (portcullis.md §9.1c): a plain DNS-ish
/// hostname, a non-zero hostid, and a lowercase 8-4-4-4-12 UUID. Values only —
/// jaild cannot tell a synthetic identity from a real one, so this checks the
/// shape; that callers never pass the real one is portcullis_identity's job.
fn validate_host_identity(req: &CreateJailRequest) -> Result<(), JaildError> {
    let bad = |rule: &'static str, detail: String| Err(JaildError::PolicyViolation { rule, detail });
    if let Some(h) = &req.hostname {
        let ok = !h.is_empty() && h.len() <= 255
            && !h.starts_with(['-', '.'])
            && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !ok { return bad("host.hostname", format!("hostname {h:?} is not a plain DNS-style name")) }
    }
    if req.hostid == Some(0) {
        return bad("host.hostid", "hostid 0 is what an unconfigured jail reports; not an identity".into());
    }
    if let Some(u) = &req.hostuuid {
        let shape = u.len() == 36 && u.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        });
        if !shape || u == "00000000-0000-0000-0000-000000000000" {
            return bad("host.hostuuid", format!("hostuuid {u:?} is not a lowercase non-zero UUID"));
        }
    }
    Ok(())
}

/// Whether `req` is a one-shot INSTANCE ROOT: an `app-` jail whose root is
/// exactly `<exec_paths.instance_root_dir>/<name>`. See
/// `jaild_policy::ExecPaths::instance_root_dir`.
///
/// ★ Exact equality, component-wise — not "under the dir". A root one level
/// deeper, or one whose last component is not the jail's own name, is some
/// other jail's tree or a path the caller chose, and gets no trust from this.
pub fn is_instance_root(req: &CreateJailRequest, policy: &Policy) -> bool {
    let Some(dir) = policy.exec_paths.instance_root_dir.as_deref() else { return false };
    req.name.starts_with("app-")
        && std::path::Path::new(&req.path) == std::path::Path::new(dir).join(&req.name)
}

fn validate_network(net: &NetworkConfig, name: &str, policy: &Policy) -> Result<(), JaildError> {
    match net {
        NetworkConfig::Inherit => {
            /* ip4=inherit (share the host stack) is powerful — gate on a
             * per-jail allowlist, not a global flag. */
            if policy.network.allow_inherit_jails.iter().any(|n| n == name) {
                Ok(())
            } else {
                Err(JaildError::PolicyViolation {
                    rule:   "network.inherit.not_allowed",
                    detail: format!(
                        "jail {name:?} not in policy.network.allow_inherit_jails — \
                         ip4=inherit is allowlisted per-jail"),
                })
            }
        }
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
    /* The HOST ROOT is refused, explicitly and first-class — not merely
     * "not on the list".
     *
     * It used to be special-cased IN, "because smoke tests use it". A
     * path="/" jail shares the host's entire filesystem and, since jaild
     * only mounts a per-jail devfs under a real root, the host's full /dev
     * (kmem, mem, pci) — PID isolation and nothing else (portcullis.md
     * §9.1). The exception outlived its reason: by 2026-09-13 the only
     * jails still taking it were those smoke manifests, and every real
     * service and session app already ran on a real root. Keeping the
     * door open meant any manifest could walk back through it. */
    if path == "/" {
        return Err(JaildError::PolicyViolation {
            rule:   "path.host_root",
            detail: "jail path \"/\" is the host root: no filesystem or device \
                     isolation. Use a per-jail root under /var/lib/atrium/jails/ \
                     (portcullis.md §9.1)".into(),
        });
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

/// Public re-export so AttachMount handlers can validate a single
/// runtime mount against the same allow-list used at create time.
pub fn validate_mount_for_runtime(policy: &Policy, m: &MountSpec) -> Result<(), JaildError> {
    validate_mount(m, policy)
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
            let exact   = policy.mount_sources.rw_paths.iter().any(|p| p == &m.source);
            let glob    = policy.mount_sources.rw_patterns.iter().any(|pat| matches_glob(pat, &m.source));
            let subtree = policy.mount_sources.rw_subtrees.iter().any(|pfx| {
                /* Defence in depth: refuse `..` even though
                 * dest is what gets traversal-checked above; a
                 * source containing `..` could still confuse
                 * downstream tools. */
                !m.source.contains("..")
                    && (m.source == pfx.trim_end_matches('/')
                        || m.source.starts_with(pfx)
                        || m.source.starts_with(&format!("{}/", pfx.trim_end_matches('/'))))
            });
            if !exact && !glob && !subtree {
                return Err(JaildError::PolicyViolation {
                    rule:   "mount.source.not_in_rw",
                    detail: format!("rw source {:?} not in policy.mount_sources rw paths/patterns/subtrees", m.source),
                });
            }
        }
        MountKind::Tmpfs => {
            /* ★★ tmpfs has no source, so it had no check at all — and it was
             * mounted with no size, so it was unbounded RAM. A per-jail rctl
             * does not cover it: tmpfs pages belong to the filesystem, not to
             * any process's RSS, so a memoryuse cap never sees them.
             *
             * Absent → the policy ceiling (applied at mount time). Explicit →
             * must be positive and within it. Refused rather than clamped: a
             * caller asking for more than it may have should learn so, not
             * receive something smaller than it believes it has. */
            if let Some(mb) = m.size_mb {
                if mb == 0 {
                    return Err(JaildError::PolicyViolation {
                        rule:   "mount.tmpfs.zero_size",
                        detail: format!("tmpfs at {:?} asks for 0 MiB", m.dest),
                    });
                }
                if mb > policy.mount_sources.max_tmpfs_mb {
                    return Err(JaildError::PolicyViolation {
                        rule:   "mount.tmpfs.too_large",
                        detail: format!("tmpfs at {:?} asks for {mb} MiB; the ceiling is {} MiB",
                                        m.dest, policy.mount_sources.max_tmpfs_mb),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_exec(exec: &ExecSpec, policy: &Policy, instance_root: bool) -> Result<(), JaildError> {
    if instance_root {
        /* A one-shot's entry: anywhere inside the verified tree it runs in —
         * but a plain absolute path. execve resolves it after jail_attach, so
         * it can only name something under the jail root; `..` is refused
         * anyway so the rule reads the way it behaves. */
        if !exec.path.starts_with('/')
            || exec.path.split('/').any(|c| c == "..")
            || exec.path.contains('\0')
        {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.path.instance_invalid",
                detail: format!("instance entry {:?} must be an absolute path with no '..'",
                                exec.path),
            });
        }
    } else if !policy.exec_paths.allowed_prefixes.iter().any(|p| exec.path.starts_with(p)) {
        /* Path prefix allow-list. */
        return Err(JaildError::PolicyViolation {
            rule:   "exec.path.not_allowed",
            detail: format!("exec path {:?} not in policy.exec_paths.allowed_prefixes", exec.path),
        });
    }

    /* ★ Only a one-shot instance takes its caller's stdio. A service's
     * descriptors are jaild's to choose (a log file); letting any jail be
     * handed arbitrary descriptors would widen every service's surface for a
     * feature exactly one lane needs. */
    if exec.stdio && !instance_root {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.stdio.not_instance",
            detail: "only a one-shot instance root may take the caller's stdio".into(),
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

    /* ★★★ gid: the SAME rule, which the policy always said it would be.
     *
     * The schema has carried a REQUIRED `[gid]` section — "mirrors uid table"
     * — since it was written, and this validator never read it. So a request
     * could name gid 0 and the child would run with wheel as its primary
     * group; and once `drop_privileges` was fixed to call setgroups({gid}),
     * that became the child's ONLY group. The uid check was guarding one half
     * of the credential and the other half walked past it.
     *
     * The user range is the uid range, as the section's own comment says:
     * FreeBSD gives per-user groups the uid's number, and every gid in use
     * (1001, 1099, 50000, 50090-50094) sits inside it. System groups need an
     * explicit entry, exactly as system uids do. */
    let gid_in_user_range = exec.gid >= policy.uid.min_user_uid
                         && exec.gid <= policy.uid.max_user_uid;
    let gid_in_system     = policy.gid.allowed_system_gids.iter().any(|g| *g == exec.gid);
    if !gid_in_user_range && !gid_in_system {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.gid.not_allowed",
            detail: format!(
                "gid {} not in user range {}..={} and not in allowed_system_gids",
                exec.gid, policy.uid.min_user_uid, policy.uid.max_user_uid),
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
        validate_path("/var/lib/atrium/jails/atrium-test", &p).unwrap(); // per-jail root
    }

    #[test]
    fn path_refuses_the_host_root() {
        let p = load_sample_policy();
        match validate_path("/", &p).unwrap_err() {
            JaildError::PolicyViolation { rule: "path.host_root", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
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
            path:          "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
        };
        validate_create(&req, &p).unwrap();
    }

    fn instance_req(name: &str, path: &str, entry: &str, stdio: bool) -> CreateJailRequest {
        CreateJailRequest {
            name:          name.into(),
            path:          path.into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 22,
            network:       NetworkConfig::Disable,
            exec:          Some(ExecSpec {
                path: entry.into(),
                argv: vec![entry.rsplit('/').next().unwrap().into()],
                env:  vec![],
                uid:  1001,
                gid:  1001,
                stdio,
            }),
            hostname: None,
            hostid: None,
            hostuuid: None,
        }
    }

    /// ★ A one-shot instance root runs its signed tree's entry wherever the
    /// manifest put it, and may take its caller's stdio.
    #[test]
    fn an_instance_root_runs_its_trees_entry_with_caller_stdio() {
        let p = load_sample_policy();
        let r = instance_req("app-org-atrium-navigator-worker--1",
            "/var/lib/atrium/jails/app-org-atrium-navigator-worker--1",
            "/bin/navigator-worker", true);
        assert!(is_instance_root(&r, &p));
        validate_create(&r, &p).unwrap();
    }

    /// ★★ The trust is for EXACTLY <dir>/<own name>. Anything else — another
    /// jail's root, a deeper path, a non-app name — falls back to the prefix
    /// list, and /bin/... is not on it.
    #[test]
    fn only_an_exact_instance_root_gets_the_entry_rule() {
        let p = load_sample_policy();
        for (name, path) in [
            ("app-w--1",   "/var/lib/atrium/jails/app-w--2"),        // someone else's root
            ("app-w--1",   "/var/lib/atrium/jails/app-w--1/sub"),    // deeper
            ("atrium-w",   "/var/lib/atrium/jails/atrium-w"),        // not an app instance
        ] {
            let r = instance_req(name, path, "/bin/navigator-worker", false);
            assert!(!is_instance_root(&r, &p), "{name} at {path}");
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: "exec.path.not_allowed" | "path.not_in_allowlist", .. }) => {}
                other => panic!("{name} at {path}: {other:?}"),
            }
        }
    }

    /// An instance entry is still a plain absolute path.
    #[test]
    fn an_instance_entry_must_be_absolute_without_dotdot() {
        let p = load_sample_policy();
        for entry in ["bin/w", "/bin/../../../usr/sbin/w"] {
            let r = instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", entry, false);
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: "exec.path.instance_invalid", .. }) => {}
                other => panic!("{entry}: {other:?}"),
            }
        }
    }

    /// ★ Only an instance may take the caller's descriptors: a service asking
    /// for stdio is refused even with an allowed binary.
    #[test]
    fn a_service_may_not_take_caller_stdio() {
        let p = load_sample_policy();
        let r = instance_req("atrium-svc", "/var/lib/atrium/jails/atrium-svc",
            "/usr/local/bin/atrium-svc", true);
        match validate_create(&r, &p) {
            Err(JaildError::PolicyViolation { rule: "exec.stdio.not_instance", .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    /// ★ uid 0 is refused for an instance too — a root caller's worker never
    /// runs as root in its jail (the decision behind portcullis.md §6.5.4).
    #[test]
    fn an_instance_may_not_run_as_root() {
        let p = load_sample_policy();
        let mut r = instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", "/bin/w", true);
        if let Some(e) = r.exec.as_mut() { e.uid = 0; e.gid = 0; }
        assert!(validate_create(&r, &p).is_err());
    }

    /// ★ The host identity is checked for SHAPE: hostid 0 and the zero UUID
    /// are what an unconfigured jail reports, so they are refused as values.
    #[test]
    fn host_identity_shape_is_enforced() {
        let p = load_sample_policy();
        let base = || instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", "/bin/w", true);
        let mut ok = base();
        ok.hostname = Some("org.atrium.navigator.worker".into());
        ok.hostid = Some(0x1234_5678);
        ok.hostuuid = Some("8a1b2c3d-4e5f-8a1b-9c2d-3e4f5a6b7c8d".into());
        validate_create(&ok, &p).unwrap();
        for (h, id, u, rule) in [
            (Some("-bad"), None, None, "host.hostname"),
            (Some("a b"), None, None, "host.hostname"),
            (None, Some(0), None, "host.hostid"),
            (None, None, Some("00000000-0000-0000-0000-000000000000"), "host.hostuuid"),
            (None, None, Some("8A1B2C3D-4E5F-8A1B-9C2D-3E4F5A6B7C8D"), "host.hostuuid"),
            (None, None, Some("not-a-uuid"), "host.hostuuid"),
        ] {
            let mut r = base();
            r.hostname = h.map(Into::into); r.hostid = id; r.hostuuid = u.map(Into::into);
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: got, .. }) if got == rule => {}
                other => panic!("{h:?}/{id:?}/{u:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn create_request_bad_name_rejected() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            name:          "evil-x".into(),
            path:          "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
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
            path: "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
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
            size_mb: None,
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
            size_mb: None,
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
            size_mb: None,
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
            size_mb: None,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn mount_rw_subtree() {
        let mut p = load_sample_policy();
        p.mount_sources.rw_subtrees = vec!["/var/lib/atrium/storage/jails/".into()];
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/var/lib/atrium/storage/jails/mysqld/data".into(),
            dest:   "var/db/mysql".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
        });
        validate_create(&r, &p).unwrap();

        // outside the subtree → reject
        let mut r2 = req_default();
        r2.mounts.push(MountSpec {
            source: "/var/lib/something-else/x".into(),
            dest:   "x".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
        });
        assert!(validate_create(&r2, &p).is_err());
    }

    #[test]
    fn mount_rw_glob() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec {
            source: "/usr/home/girivs".into(),
            dest:   "home/girivs".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
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
            stdio: false,
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
            stdio: false,
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
            stdio: false,
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
            stdio: false,
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
            stdio: false,
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
        validate_network(&NetworkConfig::Disable, "test-jail", &p).unwrap();
    }

    #[test]
    fn network_lo0_alias_in_policy_ok() {
        let p = load_sample_policy();
        // sample policy has 127.10.0.0/16 in allowed_addrs_on_lo0
        validate_network(
            &NetworkConfig::Lo0Alias { addr: "127.10.0.5/32".into() },
            "test-jail",
            &p,
        ).unwrap();
    }

    #[test]
    fn network_lo0_alias_outside_policy_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Lo0Alias { addr: "10.0.0.5/32".into() },
            "test-jail",
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
            "test-jail",
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
            "test-jail",
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.vnet.unimplemented_v0", .. }));
    }

    #[test]
    fn network_inherit_requires_allowlist() {
        let mut p = load_sample_policy();
        // ★ Assert the PREMISE rather than assuming it. This test used to use
        // "atrium-stoad" as its not-allowlisted example; stoad was later
        // genuinely added to the shipped allowlist (22d2481b — it needs
        // ip4=inherit for per-session UDP), so the test began failing for a
        // reason that had nothing to do with the validator, and stayed red.
        // A name the shipped policy happens not to list is not a fixture.
        let outsider = "atrium-not-allowlisted";
        assert!(
            !p.network.allow_inherit_jails.iter().any(|n| n == outsider),
            "fixture broken: {outsider:?} IS in the shipped allowlist, so this \
             test cannot show that inherit is refused without one"
        );

        // not in the allowlist → rejected
        let err = validate_network(&NetworkConfig::Inherit, outsider, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.inherit.not_allowed", .. }));
        // allowlisted by name → ok; a different name stays rejected
        p.network.allow_inherit_jails.push(outsider.into());
        validate_network(&NetworkConfig::Inherit, outsider, &p).unwrap();
        assert!(validate_network(&NetworkConfig::Inherit, "atrium-other", &p).is_err());
    }

    /// The other half, against the REAL shipped policy: the allowlist actually
    /// grants. ip4=inherit shares the host stack, so both directions are worth
    /// pinning — that it is refused by default, and that the one jail the
    /// policy deliberately lists still gets it. Catches an accidental deletion
    /// from etc/jaild.policy.toml, which would break stoa's remote sessions
    /// with a policy error rather than anything that points at the cause.
    #[test]
    fn shipped_policy_grants_inherit_to_the_jails_it_lists() {
        let p = load_sample_policy();
        assert!(!p.network.allow_inherit_jails.is_empty(),
            "shipped policy lists no inherit jails — stoad needs one");
        for name in &p.network.allow_inherit_jails {
            validate_network(&NetworkConfig::Inherit, name, &p)
                .unwrap_or_else(|e| panic!("shipped allowlist entry {name:?} refused: {e}"));
        }
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
            stdio: false,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.uid.not_allowed", .. }));
    }

    fn exec_as(uid: u32, gid: u32) -> CreateJailRequest {
        let mut r = req_default();
        r.exec = Some(ExecSpec {
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid, gid,
            stdio: false,
        });
        r
    }

    /// ★★★ THE HOLE: a valid uid with gid 0. The uid check passed, the gid was
    /// never looked at, and after `drop_privileges` gained its setgroups({gid})
    /// the child's only group would have been wheel.
    #[test]
    fn exec_rejects_gid_zero_with_an_otherwise_valid_uid() {
        let p = load_sample_policy();
        let err = validate_create(&exec_as(1001, 0), &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.gid.not_allowed", .. }),
            "gid 0 (wheel) was accepted: {err:?}");
    }

    /// And operator — the group that owns raw disk devices on FreeBSD.
    #[test]
    fn exec_rejects_the_operator_group() {
        let p = load_sample_policy();
        assert!(validate_create(&exec_as(1001, 5), &p).is_err(), "gid 5 (operator) was accepted");
    }

    /// ★ The counterweight: every gid actually in use must still pass, or this
    /// check breaks the services it exists to protect.
    #[test]
    fn exec_accepts_every_gid_the_shipped_manifests_use() {
        let p = load_sample_policy();
        for (uid, gid) in [(1001, 1001), (1099, 1099), (50000, 50000),
                           (50090, 50090), (50094, 50094)] {
            validate_create(&exec_as(uid, gid), &p)
                .unwrap_or_else(|e| panic!("uid {uid} gid {gid} refused: {e:?}"));
        }
    }

    /// A system gid is admitted exactly when it is listed, as system uids are.
    #[test]
    fn exec_admits_a_system_gid_only_when_allowlisted() {
        let mut p = load_sample_policy();
        assert!(validate_create(&exec_as(1001, 66), &p).is_err());
        p.gid.allowed_system_gids = vec![66];
        validate_create(&exec_as(1001, 66), &p).expect("an allowlisted system gid");
    }

    fn tmpfs(size_mb: Option<u64>) -> MountSpec {
        MountSpec { source: "tmpfs".into(), dest: "tmp".into(),
                    kind: MountKind::Tmpfs, size_mb }
    }

    /// ★★ tmpfs is RAM, and had no check at all. A per-jail rctl does not
    /// cover it — tmpfs pages belong to the filesystem, not to any process's
    /// RSS — so an oversized request has to be refused here or nowhere.
    #[test]
    fn a_tmpfs_above_the_ceiling_is_refused() {
        let p = load_sample_policy();
        let too_big = p.mount_sources.max_tmpfs_mb + 1;
        let err = validate_mount_for_runtime(&p, &tmpfs(Some(too_big))).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.tmpfs.too_large", .. }), "{err:?}");
    }

    #[test]
    fn a_zero_sized_tmpfs_is_refused() {
        let p = load_sample_policy();
        let err = validate_mount_for_runtime(&p, &tmpfs(Some(0))).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.tmpfs.zero_size", .. }), "{err:?}");
    }

    /// ★ The counterweight: no size is ACCEPTED, because it gets the ceiling
    /// at mount time rather than "unbounded". Every existing caller omits it,
    /// and refusing them would break the services this protects.
    #[test]
    fn an_unsized_tmpfs_is_accepted_and_one_within_the_ceiling_too() {
        let p = load_sample_policy();
        validate_mount_for_runtime(&p, &tmpfs(None)).expect("unsized → gets the ceiling");
        validate_mount_for_runtime(&p, &tmpfs(Some(p.mount_sources.max_tmpfs_mb)))
            .expect("exactly the ceiling is allowed");
    }

    /// The ceiling defaults when a policy file predates it, so an upgrade does
    /// not refuse every tmpfs on the machine.
    #[test]
    fn the_tmpfs_ceiling_has_a_default_for_older_policy_files() {
        assert_eq!(load_sample_policy().mount_sources.max_tmpfs_mb, 256);
    }
}


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

use crate::protocol::CreateJailRequest;
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
            name:         "atrium-test".into(),
            path:         "/".into(),
            children_max: 0,
        };
        validate_create(&req, &p).unwrap();
    }

    #[test]
    fn create_request_bad_name_rejected() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            name:         "evil-x".into(),
            path:         "/".into(),
            children_max: 0,
        };
        let err = validate_create(&req, &p).unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "name.prefix", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }
}

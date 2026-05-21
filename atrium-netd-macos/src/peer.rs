//! Per-app identification for the network broker.
//!
//! When `$INSULA_INSTALL_ROOT` is set, the broker
//! attempts to identify each connecting peer as an
//! installed Insula app and enforce that app's
//! manifest `[network]` hosts allowlist. If
//! identification fails (peer is not an Insula app, or
//! install root is unset), the broker falls back to
//! the broker-wide allowlist (the v0 default).
//!
//! Mechanism:
//!   1. `SO_PEERPID` via `getsockopt` gives us the
//!      kernel-attested pid of the connecting client.
//!   2. `proc_pidpath` gives us the canonical
//!      executable path of that pid.
//!   3. Walking `<install_root>/apps/*/bundle/` matches
//!      the exe path to an installed app id.
//!   4. We load that app's manifest and use its
//!      `[network]` section as the allowlist.
//!
//! All four steps can fail benignly — failure means
//! "treat this peer as unidentified" rather than
//! "deny." The broker-wide allowlist (if set) still
//! applies as a backstop.

use insula_manifest::{Manifest, NetworkProto};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// What we learned about the peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Kernel-attested peer pid. -1 if `SO_PEERPID`
    /// failed.
    #[allow(dead_code)] // useful for logging / future audit
    pub pid: i32,
    /// Canonical executable path, if we could resolve it.
    #[allow(dead_code)] // useful for logging / future audit
    pub exe: Option<PathBuf>,
    /// Installed app id, if `exe` matches an Insula bundle
    /// under the configured install root.
    pub app_id: Option<String>,
    /// The matched app's parsed manifest.
    pub manifest: Option<Manifest>,
}

/// Identify a peer connecting on `stream`. Returns
/// best-effort info; any field may be `None` if the
/// kernel / filesystem doesn't cooperate.
///
/// `install_root` may be `None` (no per-app
/// enforcement is configured) — in that case only the
/// pid + exe are populated.
pub fn identify(stream: &UnixStream, install_root: Option<&Path>) -> PeerInfo {
    let pid = peer_pid(stream).unwrap_or(-1);
    let exe = if pid > 0 { pid_executable_path(pid) } else { None };
    let mut info = PeerInfo {
        pid,
        exe: exe.clone(),
        app_id: None,
        manifest: None,
    };
    if let (Some(root), Some(exe)) = (install_root, exe) {
        if let Some(app_id) = app_id_for_exe(root, &exe) {
            let manifest = manifest_for_app(root, &app_id);
            info.app_id = Some(app_id);
            info.manifest = manifest;
        }
    }
    info
}

/// macOS-specific peer-pid query.
///
/// `getsockopt(LOCAL_PEERPID)` returns the kernel-
/// attested pid of the process that connected to a
/// unix socket. This is the secure identifier — the
/// app cannot forge it.
pub fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 && pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// macOS `proc_pidpath` wrapper. Returns the absolute
/// canonical executable path of `pid`, or `None` if
/// the process is gone / unreadable.
pub fn pid_executable_path(pid: i32) -> Option<PathBuf> {
    // Apple's constant PROC_PIDPATHINFO_MAXSIZE = 4096
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::proc_pidpath(
            pid,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let s = std::str::from_utf8(&buf).ok()?;
    Some(PathBuf::from(s))
}

/// Match an exe path to an installed app id by walking
/// `<install_root>/apps/*/bundle/`. Returns the first
/// match (there should only be one).
///
/// Both the exe and each bundle dir are canonicalized
/// before comparison so symlink-typical macOS paths
/// (`/var/folders/...` ↔ `/private/var/folders/...`)
/// don't cause false negatives.
pub fn app_id_for_exe(install_root: &Path, exe: &Path) -> Option<String> {
    let exe_canon = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let apps_dir = install_root.join("apps");
    if !apps_dir.is_dir() {
        return None;
    }
    for entry in std::fs::read_dir(&apps_dir).ok()? {
        let entry = entry.ok()?;
        let app_id = entry.file_name().to_string_lossy().to_string();
        let bundle = entry.path().join("bundle");
        let bundle_canon = bundle
            .canonicalize()
            .unwrap_or(bundle.clone());
        if exe_canon.starts_with(&bundle_canon) {
            return Some(app_id);
        }
    }
    None
}

/// Read + parse the manifest for an installed app.
pub fn manifest_for_app(install_root: &Path, app_id: &str) -> Option<Manifest> {
    let path = install_root
        .join("apps")
        .join(app_id)
        .join("bundle")
        .join("manifest.toml");
    let src = std::fs::read_to_string(&path).ok()?;
    Manifest::parse(&src).ok()
}

/// Per-connection enforcement verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The peer is identified as an Insula app and the
    /// host appears in its manifest `[network].hosts`
    /// (or `raw-network = true` is declared).
    AllowedByManifest,
    /// The peer is identified as an Insula app, but the
    /// host is not in its allowlist.
    DeniedByManifest,
    /// The peer is not an Insula app (or the broker has
    /// no install root); the broker-wide allowlist
    /// decides. Returned by future helpers that don't
    /// have a manifest in hand; the current
    /// [`check_against_manifest`] always returns one of
    /// the first two.
    #[allow(dead_code)]
    FallThrough,
}

/// Apply a manifest's `[network]` allowlist to a
/// requested (host, port, proto) tuple.
pub fn check_against_manifest(
    manifest: &Manifest,
    host: &str,
    port: u16,
    proto: NetworkProto,
) -> Verdict {
    let Some(net) = &manifest.network else {
        // No [network] section in the manifest -> no
        // outbound is permitted for this app.
        return Verdict::DeniedByManifest;
    };
    if net.raw_network {
        return Verdict::AllowedByManifest;
    }
    for h in &net.hosts {
        if h.name == host && h.port == port && h.proto == proto {
            return Verdict::AllowedByManifest;
        }
    }
    Verdict::DeniedByManifest
}

#[cfg(test)]
mod tests {
    use super::*;
    use insula_manifest::Manifest;

    fn manifest_with_hosts(hosts_toml: &str) -> Manifest {
        let src = format!(
            r#"
[app]
name = "com.example.test"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/test"

[network]
{}
"#,
            hosts_toml
        );
        Manifest::parse(&src).unwrap()
    }

    #[test]
    fn check_allows_exact_match() {
        let m = manifest_with_hosts(
            r#"hosts = [
              { name = "api.example.com", port = 443, proto = "tcp" }
            ]"#,
        );
        assert_eq!(
            check_against_manifest(&m, "api.example.com", 443, NetworkProto::Tcp),
            Verdict::AllowedByManifest
        );
    }

    #[test]
    fn check_denies_wrong_port() {
        let m = manifest_with_hosts(
            r#"hosts = [
              { name = "api.example.com", port = 443, proto = "tcp" }
            ]"#,
        );
        assert_eq!(
            check_against_manifest(&m, "api.example.com", 80, NetworkProto::Tcp),
            Verdict::DeniedByManifest
        );
    }

    #[test]
    fn check_denies_wrong_proto() {
        let m = manifest_with_hosts(
            r#"hosts = [
              { name = "api.example.com", port = 443, proto = "tcp" }
            ]"#,
        );
        assert_eq!(
            check_against_manifest(&m, "api.example.com", 443, NetworkProto::Udp),
            Verdict::DeniedByManifest
        );
    }

    #[test]
    fn check_denies_unlisted_host() {
        let m = manifest_with_hosts(
            r#"hosts = [
              { name = "api.example.com", port = 443, proto = "tcp" }
            ]"#,
        );
        assert_eq!(
            check_against_manifest(&m, "evil.com", 443, NetworkProto::Tcp),
            Verdict::DeniedByManifest
        );
    }

    #[test]
    fn check_raw_network_allows_anything() {
        let m = manifest_with_hosts("raw-network = true");
        assert_eq!(
            check_against_manifest(&m, "evil.com", 443, NetworkProto::Tcp),
            Verdict::AllowedByManifest
        );
    }

    #[test]
    fn check_denies_when_no_network_section() {
        // Manifest without a [network] section: build
        // a minimal one without that section.
        let src = r#"
[app]
name = "com.example.test"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/test"
"#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(
            check_against_manifest(&m, "api.example.com", 443, NetworkProto::Tcp),
            Verdict::DeniedByManifest
        );
    }

    #[test]
    fn app_id_for_exe_matches_installed_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let app_dir = tmp.path().join("apps").join("com.example.x");
        std::fs::create_dir_all(app_dir.join("bundle/bin")).unwrap();
        let exe_path = app_dir.join("bundle/bin/insula-hello");
        std::fs::write(&exe_path, b"binary").unwrap();

        let r = app_id_for_exe(tmp.path(), &exe_path);
        assert_eq!(r.as_deref(), Some("com.example.x"));
    }

    #[test]
    fn app_id_for_exe_misses_outside_install_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("apps")).unwrap();
        // /bin/echo is definitely outside the temp install root.
        let r = app_id_for_exe(tmp.path(), Path::new("/bin/echo"));
        assert_eq!(r, None);
    }
}

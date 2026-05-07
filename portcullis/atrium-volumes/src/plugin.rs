//! Per-backend plugin trait + V0 implementations:
//!
//!   tessera (default): mkdir + chown on Tessera mount
//!   plain:             mkdir + chown on whatever's mounted
//!   tmpfs:             sentinel host-path; jaild does the mount
//!
//! zfs is V1 (separate commit; shells out to `zfs(8)`).
//!
//! Spec: `docs/spec/atrium-volumes.md` §6.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use log::debug;

use crate::ffi;
use crate::policy::{BackendConfig, BackendKind};
use crate::protocol::VolumeSpec;

pub trait BackendPlugin: Send + Sync {
    fn kind(&self) -> &'static str;

    /// What this backend supports. Used by validator and by
    /// `Response::Backends`.
    fn features(&self) -> &'static [&'static str];

    /// Provision a persistent volume. Idempotent: a re-call for
    /// an existing volume returns the existing host path.
    /// Returns the host path the caller (atrium-volumes' server)
    /// can record in state and pass back as a mount source.
    fn provision(
        &self,
        backend: &BackendConfig,
        jail_name: &str,
        spec: &VolumeSpec,
    ) -> io::Result<String>;

    /// Destroy. The supplied `host_path` was returned by
    /// provision earlier (atrium-volumes verifies this against
    /// its state file before calling). Should refuse if path
    /// is somewhere unexpected.
    fn destroy(
        &self,
        backend: &BackendConfig,
        host_path: &str,
    ) -> io::Result<()>;
}

// =====================================================================
// Tessera plugin (default).
//
// Tessera-the-FS handles CAS dedup transparently at the chunk
// layer; atrium-volumes does not touch any of that. From this
// daemon's perspective, the operations are mkdir + chown + chmod.
// =====================================================================

pub struct TesseraPlugin;

impl BackendPlugin for TesseraPlugin {
    fn kind(&self) -> &'static str { "tessera" }

    fn features(&self) -> &'static [&'static str] {
        &["dedup", "snapshot"]
    }

    fn provision(
        &self,
        backend: &BackendConfig,
        jail_name: &str,
        spec: &VolumeSpec,
    ) -> io::Result<String> {
        let path = compose_host_path(backend, jail_name, &spec.name)?;
        ensure_dir(&path, spec.mode)?;
        ffi::chown(Path::new(&path), spec.owner_uid, spec.owner_gid)?;
        debug!("tessera: provisioned {} (mode={:#o} {}:{})",
            path, spec.mode, spec.owner_uid, spec.owner_gid);
        Ok(path)
    }

    fn destroy(&self, _backend: &BackendConfig, host_path: &str) -> io::Result<()> {
        rm_rf_safe(host_path)
    }
}

// =====================================================================
// Plain plugin.
//
// Universal compatibility — works on any POSIX-mounted directory.
// No features (no quota, no snapshot). Same code as tessera at
// V0; the difference is what `features()` reports.
// =====================================================================

pub struct PlainPlugin;

impl BackendPlugin for PlainPlugin {
    fn kind(&self) -> &'static str { "plain" }

    fn features(&self) -> &'static [&'static str] { &[] }

    fn provision(
        &self,
        backend: &BackendConfig,
        jail_name: &str,
        spec: &VolumeSpec,
    ) -> io::Result<String> {
        let path = compose_host_path(backend, jail_name, &spec.name)?;
        ensure_dir(&path, spec.mode)?;
        ffi::chown(Path::new(&path), spec.owner_uid, spec.owner_gid)?;
        debug!("plain: provisioned {}", path);
        Ok(path)
    }

    fn destroy(&self, _backend: &BackendConfig, host_path: &str) -> io::Result<()> {
        rm_rf_safe(host_path)
    }
}

// =====================================================================
// Tmpfs plugin.
//
// Doesn't actually allocate — tmpfs volumes are mounted by jaild
// at jail-create time. The "host path" is a sentinel string
// jaild's mount path interprets specially.
//
// Kept as a plugin for symmetry; could be collapsed into a
// short-circuit in the dispatcher later.
// =====================================================================

pub struct TmpfsPlugin;

impl BackendPlugin for TmpfsPlugin {
    fn kind(&self) -> &'static str { "tmpfs" }

    fn features(&self) -> &'static [&'static str] { &[] }

    fn provision(
        &self,
        _backend: &BackendConfig,
        jail_name: &str,
        spec: &VolumeSpec,
    ) -> io::Result<String> {
        Ok(format!("tmpfs::{jail_name}/{}", spec.name))
    }

    fn destroy(&self, _backend: &BackendConfig, _host_path: &str) -> io::Result<()> {
        // No allocation to release.
        Ok(())
    }
}

// =====================================================================
// Helpers.
// =====================================================================

fn compose_host_path(
    backend: &BackendConfig,
    jail_name: &str,
    volume_name: &str,
) -> io::Result<String> {
    let root = backend.root.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput,
            format!("backend {:?} ({:?}) has no root configured",
                backend.name, backend.kind))
    })?;
    /* Defence in depth: refuse jail/volume names with traversal
     * characters. The name validator upstream should already
     * have caught this, but path manipulation is the unsafe
     * boundary. */
    if jail_name.contains('/') || jail_name.contains("..") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("jail name {jail_name:?} contains forbidden chars")));
    }
    if volume_name.contains('/') || volume_name.contains("..") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("volume name {volume_name:?} contains forbidden chars")));
    }
    let mut p = PathBuf::from(root);
    p.push("jails");
    p.push(jail_name);
    p.push(volume_name);
    Ok(p.to_string_lossy().into_owned())
}

fn ensure_dir(path: &str, mode: u32) -> io::Result<()> {
    /* Idempotent mkdir -p with the requested mode. If the
     * directory already exists, we ensure perms still match. */
    let pp = Path::new(path);
    if !pp.exists() {
        fs::create_dir_all(pp)?;
    }
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(pp, perms)?;
    Ok(())
}

fn rm_rf_safe(host_path: &str) -> io::Result<()> {
    /* Refuse paths outside our managed area as a final defence —
     * caller should already have validated against state, but
     * this catches programmer errors. We require host_path to
     * contain "/jails/" somewhere; that's the convention
     * compose_host_path produces. */
    if !host_path.contains("/jails/") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("refusing rm-rf on suspicious path {host_path:?}")));
    }
    let pp = Path::new(host_path);
    if pp.exists() {
        fs::remove_dir_all(pp)?;
    }
    Ok(())
}

/// Construct the right plugin for a backend kind.
pub fn plugin_for(kind: BackendKind) -> Option<Box<dyn BackendPlugin>> {
    match kind {
        BackendKind::Tessera => Some(Box::new(TesseraPlugin)),
        BackendKind::Plain   => Some(Box::new(PlainPlugin)),
        BackendKind::Tmpfs   => Some(Box::new(TmpfsPlugin)),
        BackendKind::Zfs     => None,   // V1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn spec_data() -> VolumeSpec {
        /* chown to our own uid/gid; non-root callers can't chown
         * to anything else. In the VM smoke (running as root)
         * any uid works. */
        let uid = unsafe_getuid();
        let gid = unsafe_getgid();
        VolumeSpec {
            name:      "data".into(),
            kind:      crate::protocol::VolumeKind::Persistent,
            backend:   None,
            mount_at:  "/var/db/mysql".into(),
            mode:      0o700,
            owner_uid: uid,
            owner_gid: gid,
            size_max:  None,
            cas_root:  None,
        }
    }

    fn unsafe_getuid() -> u32 {
        #[allow(unsafe_code)]
        unsafe { libc::getuid() }
    }
    fn unsafe_getgid() -> u32 {
        #[allow(unsafe_code)]
        unsafe { libc::getgid() }
    }

    #[test]
    fn plain_provision_round_trip() {
        let dir = tempdir().unwrap();
        let backend = BackendConfig {
            name: "default".into(),
            kind: BackendKind::Plain,
            root: Some(dir.path().to_string_lossy().into_owned()),
            default: true,
        };
        let p = PlainPlugin;
        let host_path = p.provision(&backend, "mysqld", &spec_data())
            .unwrap_or_else(|e| panic!("provision: {e}"));
        let host = std::path::Path::new(&host_path);
        assert!(host.exists());
        assert!(host.is_dir());
        let meta = std::fs::metadata(host).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn plain_destroy_round_trip() {
        let dir = tempdir().unwrap();
        let backend = BackendConfig {
            name: "default".into(),
            kind: BackendKind::Plain,
            root: Some(dir.path().to_string_lossy().into_owned()),
            default: true,
        };
        let p = PlainPlugin;
        let host_path = p.provision(&backend, "mysqld", &spec_data()).unwrap();
        assert!(std::path::Path::new(&host_path).exists());
        p.destroy(&backend, &host_path).unwrap();
        assert!(!std::path::Path::new(&host_path).exists());
    }

    #[test]
    fn destroy_refuses_paths_outside_managed_area() {
        let p = PlainPlugin;
        let backend = BackendConfig {
            name: "x".into(),
            kind: BackendKind::Plain,
            root: Some("/tmp".into()),
            default: true,
        };
        let err = p.destroy(&backend, "/etc/passwd").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn tmpfs_returns_sentinel() {
        let backend = BackendConfig {
            name: "tmpfs".into(),
            kind: BackendKind::Tmpfs,
            root: None,
            default: false,
        };
        let p = TmpfsPlugin;
        let path = p.provision(&backend, "mysqld", &spec_data()).unwrap();
        assert!(path.starts_with("tmpfs::"));
    }

    #[test]
    fn compose_path_rejects_traversal() {
        let backend = BackendConfig {
            name: "x".into(),
            kind: BackendKind::Plain,
            root: Some("/var/lib".into()),
            default: true,
        };
        assert!(compose_host_path(&backend, "../escape", "data").is_err());
        assert!(compose_host_path(&backend, "ok",         "../bad").is_err());
        assert!(compose_host_path(&backend, "ok/inner",   "data").is_err());
    }

    #[test]
    fn provision_idempotent() {
        let dir = tempdir().unwrap();
        let backend = BackendConfig {
            name: "default".into(),
            kind: BackendKind::Plain,
            root: Some(dir.path().to_string_lossy().into_owned()),
            default: true,
        };
        let p = PlainPlugin;
        let h1 = p.provision(&backend, "mysqld", &spec_data()).unwrap();
        let h2 = p.provision(&backend, "mysqld", &spec_data()).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn plugin_dispatch() {
        assert_eq!(plugin_for(BackendKind::Tessera).unwrap().kind(), "tessera");
        assert_eq!(plugin_for(BackendKind::Plain).unwrap().kind(),   "plain");
        assert_eq!(plugin_for(BackendKind::Tmpfs).unwrap().kind(),   "tmpfs");
        assert!(plugin_for(BackendKind::Zfs).is_none());
    }
}

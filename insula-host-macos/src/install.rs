//! Install + launch-installed flow.
//!
//! v0 install on macOS:
//!
//!   1. Take an [`InsulaBundle`] (bundle on disk).
//!   2. Choose an install location:
//!      `<install_root>/apps/<app-id>/`
//!      with the per-app container directory at
//!      `<install_root>/apps/<app-id>/container/`.
//!   3. Recursively copy the bundle into the install
//!      location. (No content-addressing yet — v0 takes
//!      the easy path; a future Tessera-backed install
//!      dedups across apps.)
//!   4. Create the empty container directory.
//!   5. Return an [`InstalledApp`] handle the launcher
//!      uses.
//!
//! No code signing, no notarization, no `.app` wrapping
//! yet — that's the production polish (Phase 1C per
//! `insula-host-macos.md` §12.3). v0 demonstrates the
//! install/launch separation, not the macOS-flavored
//! packaging.

use crate::{launch, Error, LaunchOptions, SandboxedChild};
use insula_bundle::InsulaBundle;
use insula_manifest::Manifest;
use std::path::{Path, PathBuf};

/// An Insula app installed at a known location and
/// ready to launch.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    /// The app's canonical identifier (mirrors
    /// `manifest.app.name`).
    pub app_id: String,

    /// Absolute path to the installed bundle's binary.
    pub binary_path: PathBuf,

    /// Absolute path to the per-app sandbox container.
    pub container_dir: PathBuf,

    /// Parsed manifest. Carried by value so launches
    /// don't need to re-read it from disk.
    pub manifest: Manifest,
}

/// Install a bundle into `install_root`.
///
/// `install_root` is typically something like
/// `~/Library/Application Support/atrium-insula/`. The
/// resulting layout is:
///
/// ```text
/// <install_root>/apps/<app-id>/
///   bundle/             ← contents of the bundle directory
///     manifest.toml
///     bin/...
///     assets/...
///   container/          ← per-app sandbox container
///     (initially empty; the app fills it at runtime)
/// ```
///
/// Re-installing an already-present app-id overwrites
/// the `bundle/` directory but preserves `container/`
/// — same shape as iOS / macOS app updates.
pub fn install(
    bundle: &InsulaBundle,
    install_root: &Path,
) -> Result<InstalledApp, Error> {
    install_with_mode(bundle, install_root, InstallMode::Copy)
}

/// How the bundle directory is materialized under the
/// install root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Production semantics: recursively copy the bundle
    /// tree into `<install_root>/apps/<id>/bundle/`.
    /// Subsequent edits to the source dir do NOT affect
    /// what the installed app runs.
    Copy,
    /// Dev semantics: symlink
    /// `<install_root>/apps/<id>/bundle` -> the source
    /// directory's canonical absolute path. Subsequent
    /// edits to the source ARE reflected on the next
    /// launch (no reinstall needed).
    ///
    /// Only valid for bundle directories, never archives.
    /// The `insula install --link` CLI flag picks this.
    Link,
}

/// Most-general install entry: behaves like `install`
/// but lets the caller choose between Copy (production)
/// and Link (dev iteration) modes.
pub fn install_with_mode(
    bundle: &InsulaBundle,
    install_root: &Path,
    mode: InstallMode,
) -> Result<InstalledApp, Error> {
    let app_id = bundle.app_id();
    let app_root = install_root.join("apps").join(app_id);
    let bundle_dst = app_root.join("bundle");
    let container_dir = app_root.join("container");

    // Remove any prior bundle/ (preserve container/).
    // For Link mode, `remove_dir_all` correctly removes
    // a symlink without following it (it deletes the
    // link, not the target).
    if bundle_dst.exists() || bundle_dst.is_symlink() {
        if bundle_dst.is_symlink() {
            std::fs::remove_file(&bundle_dst)
                .map_err(|e| Error::UnsupportedFeature(format!(
                    "removing prior bundle symlink at {}: {}",
                    bundle_dst.display(), e
                )))?;
        } else {
            std::fs::remove_dir_all(&bundle_dst)
                .map_err(|e| Error::UnsupportedFeature(format!(
                    "removing prior bundle at {}: {}",
                    bundle_dst.display(), e
                )))?;
        }
    }

    std::fs::create_dir_all(&container_dir)
        .map_err(|e| Error::UnsupportedFeature(format!(
            "mkdir container/ at {}: {}", container_dir.display(), e
        )))?;
    // Ensure the parent (`apps/<id>/`) exists; we'll
    // create the bundle entry itself below per-mode.
    if let Some(parent) = bundle_dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::UnsupportedFeature(format!(
                "mkdir {}: {}", parent.display(), e
            )))?;
    }

    match mode {
        InstallMode::Copy => {
            std::fs::create_dir_all(&bundle_dst)
                .map_err(|e| Error::UnsupportedFeature(format!(
                    "mkdir bundle/ at {}: {}", bundle_dst.display(), e
                )))?;
            copy_dir_recursive(&bundle.root, &bundle_dst)?;
        }
        InstallMode::Link => {
            let src_abs = bundle.root.canonicalize()
                .map_err(|e| Error::UnsupportedFeature(format!(
                    "canonicalize {}: {}", bundle.root.display(), e
                )))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&src_abs, &bundle_dst)
                .map_err(|e| Error::UnsupportedFeature(format!(
                    "symlink {} -> {}: {}",
                    bundle_dst.display(), src_abs.display(), e
                )))?;
            #[cfg(not(unix))]
            return Err(Error::UnsupportedFeature(
                "InstallMode::Link is unix-only".to_string()
            ));
        }
    }

    let binary_path = bundle_dst.join(&bundle.manifest.bundle.entry);

    Ok(InstalledApp {
        app_id: app_id.to_string(),
        binary_path,
        container_dir,
        manifest: bundle.manifest.clone(),
    })
}

/// Launch an [`InstalledApp`] using the existing
/// sandboxed-launch path, with the app's own
/// `binary_path` + `container_dir`.
pub fn launch_installed(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
) -> Result<SandboxedChild, Error> {
    launch_installed_with_log(app, args, capture_output, None)
}

/// As [`launch_installed`] but also threads through an
/// `insula-logd` socket path. The launched app will
/// have `$ATRIUM_LOG_SOCKET` set in its environment and
/// an SBPL grant for the socket path.
pub fn launch_installed_with_log(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    launch_installed_full(app, args, capture_output, log_socket, None)
}

/// As [`launch_installed_with_log`] but also accepts a
/// vestibulum socket (deprecated; prefer
/// [`launch_installed_all`]).
pub fn launch_installed_full(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
    vestibulum_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    launch_installed_all(
        app, args, capture_output, log_socket, vestibulum_socket, None,
    )
}

/// As [`launch_installed_full_v2`] but without praeco
/// (kept for source-compat with v1 callers).
pub fn launch_installed_all(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
    vestibulum_socket: Option<&std::path::Path>,
    netd_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    launch_installed_v2(
        app, args, capture_output, log_socket, vestibulum_socket, netd_socket, None,
    )
}

/// Source-compat shim: launch with up to four sockets
/// (logd / vestibulum / netd / praeco).
pub fn launch_installed_v2(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
    vestibulum_socket: Option<&std::path::Path>,
    netd_socket: Option<&std::path::Path>,
    praeco_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    launch_installed_v3(
        app, args, capture_output, log_socket, vestibulum_socket,
        netd_socket, praeco_socket, None,
    )
}

/// Source-compat shim: launch with up to five sockets
/// (logd / vestibulum / netd / praeco / tabellarius).
pub fn launch_installed_v3(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
    vestibulum_socket: Option<&std::path::Path>,
    netd_socket: Option<&std::path::Path>,
    praeco_socket: Option<&std::path::Path>,
    tabellarius_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    launch_installed_v4(
        app, args, capture_output, log_socket, vestibulum_socket,
        netd_socket, praeco_socket, tabellarius_socket, None,
    )
}

/// Most-general launch entry: threads all six Insula
/// platform sockets (logd / vestibulum / netd / praeco
/// / tabellarius / fresco) through to the child via
/// env + SBPL grant. Pass `None` for any to skip.
pub fn launch_installed_v4(
    app: &InstalledApp,
    args: &[&str],
    capture_output: bool,
    log_socket: Option<&std::path::Path>,
    vestibulum_socket: Option<&std::path::Path>,
    netd_socket: Option<&std::path::Path>,
    praeco_socket: Option<&std::path::Path>,
    tabellarius_socket: Option<&std::path::Path>,
    fresco_socket: Option<&std::path::Path>,
) -> Result<SandboxedChild, Error> {
    let opts = LaunchOptions {
        binary_path: &app.binary_path,
        container_dir: &app.container_dir,
        args,
        capture_output,
        log_socket,
        vestibulum_socket,
        netd_socket,
        praeco_socket,
        tabellarius_socket,
        fresco_socket,
    };
    launch(&app.manifest, &opts)
}

/// Recursive directory copy. Standard library has no
/// equivalent that handles permissions / arbitrary
/// trees; this is a minimal implementation sufficient
/// for v0 install.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), Error> {
    if !src.is_dir() {
        return Err(Error::UnsupportedFeature(format!(
            "copy source is not a dir: {}", src.display()
        )));
    }
    for entry in std::fs::read_dir(src).map_err(|e| {
        Error::UnsupportedFeature(format!("read_dir {}: {}", src.display(), e))
    })? {
        let entry = entry.map_err(|e| {
            Error::UnsupportedFeature(format!("read_dir entry: {}", e))
        })?;
        let src_child = entry.path();
        let dst_child = dst.join(entry.file_name());

        let ty = entry.file_type().map_err(|e| {
            Error::UnsupportedFeature(format!("file_type: {}", e))
        })?;

        if ty.is_dir() {
            std::fs::create_dir_all(&dst_child).map_err(|e| {
                Error::UnsupportedFeature(format!(
                    "mkdir {}: {}", dst_child.display(), e
                ))
            })?;
            copy_dir_recursive(&src_child, &dst_child)?;
        } else if ty.is_file() {
            std::fs::copy(&src_child, &dst_child).map_err(|e| {
                Error::UnsupportedFeature(format!(
                    "copy {} -> {}: {}",
                    src_child.display(), dst_child.display(), e
                ))
            })?;
            // Preserve executable bit on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&src_child)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0o644);
                let mut perms = std::fs::metadata(&dst_child)
                    .map_err(|e| Error::UnsupportedFeature(
                        format!("stat copy dst: {}", e)
                    ))?
                    .permissions();
                perms.set_mode(mode);
                let _ = std::fs::set_permissions(&dst_child, perms);
            }
        }
        // Skip symlinks / other special files for v0.
    }
    Ok(())
}

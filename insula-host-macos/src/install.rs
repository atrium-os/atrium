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
    let app_id = bundle.app_id();
    let app_root = install_root.join("apps").join(app_id);
    let bundle_dst = app_root.join("bundle");
    let container_dir = app_root.join("container");

    // Remove any prior bundle/ (preserve container/).
    if bundle_dst.exists() {
        std::fs::remove_dir_all(&bundle_dst)
            .map_err(|e| Error::UnsupportedFeature(format!(
                "removing prior bundle at {}: {}", bundle_dst.display(), e
            )))?;
    }

    std::fs::create_dir_all(&bundle_dst)
        .map_err(|e| Error::UnsupportedFeature(format!(
            "mkdir bundle/ at {}: {}", bundle_dst.display(), e
        )))?;
    std::fs::create_dir_all(&container_dir)
        .map_err(|e| Error::UnsupportedFeature(format!(
            "mkdir container/ at {}: {}", container_dir.display(), e
        )))?;

    copy_dir_recursive(&bundle.root, &bundle_dst)?;

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
    let opts = LaunchOptions {
        binary_path: &app.binary_path,
        container_dir: &app.container_dir,
        args,
        capture_output,
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

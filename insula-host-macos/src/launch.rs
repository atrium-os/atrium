//! Sandboxed launch of an Insula app on macOS.
//!
//! Uses Apple's `sandbox-exec` command-line tool to
//! apply a generated SBPL profile (see [`crate::sbpl`])
//! to a child process at exec time.
//!
//! # Why `sandbox-exec` and not `sandbox_init` directly
//!
//! `sandbox_init_with_parameters` is a private SPI in
//! libsandbox — undocumented, no stable ABI. `sandbox-
//! exec` is a thin supported wrapper around it. For a v0
//! host adapter, using `sandbox-exec` avoids:
//!
//! - Hand-rolling unsafe FFI to a private library.
//! - Requiring the target binary to be specially signed.
//! - Coupling Insula's host adapter to one macOS version's
//!   private-SPI shape.
//!
//! A production host adapter likely calls `sandbox_init`
//! directly for lower overhead, after the demo phase
//! establishes that the SBPL we generate is correct.
//!
//! # Future hardening
//!
//! Once an Insula-team developer-ID exists for code
//! signing, the launch path can shift to:
//! `posix_spawn` + the target binary self-applies sandbox
//! via `sandbox_init` early in `main()`, with the SBPL
//! embedded in the bundle. That removes the `sandbox-
//! exec` intermediate process.

use crate::{sbpl, Error};
use insula_manifest::Manifest;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Launch options for [`launch`].
#[derive(Debug, Clone)]
pub struct LaunchOptions<'a> {
    /// Absolute path to the executable to launch.
    pub binary_path: &'a Path,

    /// Absolute path to the app's sandbox container
    /// directory (the SBPL profile references this as
    /// `(param "CONTAINER_DIR")`).
    pub container_dir: &'a Path,

    /// Extra args to pass to the child process.
    pub args: &'a [&'a str],

    /// Whether to capture child stdout/stderr (`true`)
    /// or inherit from the parent (`false`).
    pub capture_output: bool,

    /// Optional: path to an `insula-logd` socket the
    /// app should route its log forwarding to. When
    /// set, the launcher:
    ///
    /// 1. Sets `$ATRIUM_LOG_SOCKET` in the child's env
    ///    so libatrium opens it during `atrium_init`.
    /// 2. Emits an SBPL grant in the generated profile
    ///    for the socket path (otherwise App Sandbox
    ///    blocks the unix-socket connect).
    pub log_socket: Option<&'a Path>,
}

impl<'a> LaunchOptions<'a> {
    /// Convenience constructor with sensible defaults.
    pub fn new(binary_path: &'a Path, container_dir: &'a Path) -> Self {
        Self {
            binary_path,
            container_dir,
            args: &[],
            capture_output: false,
            log_socket: None,
        }
    }
}

/// A spawned sandboxed Insula app.
///
/// Owns the temp file holding the SBPL profile until
/// the child exits (the file is deleted on drop, after
/// the child has read it). The wrapped [`Child`] is
/// the actual process; use it to wait / kill / read
/// output.
pub struct SandboxedChild {
    /// The running child process.
    pub child: Child,

    /// Held until drop — the SBPL temp file. `sandbox-
    /// exec` reads this *once* at startup so it can be
    /// deleted any time after the child is past the
    /// initial sandbox-apply step, but keeping it for
    /// the child's lifetime is simpler and the cost is
    /// negligible.
    _profile_tempfile: tempfile::NamedTempFile,
}

/// Spawn an Insula app inside a manifest-derived
/// sandbox.
///
/// Steps:
///   1. Generate the SBPL profile from the manifest
///      (see [`crate::sbpl::render_profile`]).
///   2. Write it to a temp file.
///   3. `exec sandbox-exec -f tempfile -D CONTAINER_DIR=... -D BINARY_PATH=... binary [args...]`.
///
/// Returns the spawned process wrapped in
/// [`SandboxedChild`]; the SBPL temp file lives as long
/// as the returned wrapper.
pub fn launch(
    manifest: &Manifest,
    opts: &LaunchOptions,
) -> Result<SandboxedChild, Error> {
    let profile = sbpl::render_profile_with(manifest, opts.log_socket);

    let mut profile_file = tempfile::Builder::new()
        .prefix("insula-")
        .suffix(".sb")
        .tempfile()
        .map_err(|e| {
            Error::UnsupportedFeature(format!("temp file: {}", e))
        })?;

    use std::io::Write;
    profile_file
        .write_all(profile.as_bytes())
        .map_err(|e| Error::UnsupportedFeature(format!("write profile: {}", e)))?;
    profile_file
        .as_file_mut()
        .sync_all()
        .map_err(|e| Error::UnsupportedFeature(format!("sync profile: {}", e)))?;

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-f").arg(profile_file.path());
    cmd.arg("-D").arg(format!("CONTAINER_DIR={}", opts.container_dir.display()));
    cmd.arg("-D").arg(format!("BINARY_PATH={}", opts.binary_path.display()));
    cmd.arg(opts.binary_path);
    for a in opts.args {
        cmd.arg(a);
    }

    if let Some(sock) = opts.log_socket {
        cmd.env("ATRIUM_LOG_SOCKET", sock);
    }

    if opts.capture_output {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }

    let child = cmd
        .spawn()
        .map_err(|e| Error::UnsupportedFeature(format!("spawn sandbox-exec: {}", e)))?;

    Ok(SandboxedChild {
        child,
        _profile_tempfile: profile_file,
    })
}

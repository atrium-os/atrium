//! Daemon lifecycle management for `insula` CLI.
//!
//! Two daemons currently:
//!
//! - `insula-logd` — log forwarding (see
//!   `insula-logd/src/main.rs`)
//! - `vestibulum-macos` — keychain service (see
//!   `vestibulum-macos/src/main.rs`)
//!
//! Both are managed identically: spawned in the
//! background with a pid file + socket under
//! `<install_root>/run/<name>.{pid,sock,log}`.
//!
//! State is *per install root*: tests use a tempdir
//! install root and never collide with the real
//! user's daemon state.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One of the daemons we manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Daemon {
    /// `insula-logd` — log forwarding.
    Logd,
    /// `vestibulum-macos` — keychain.
    Vestibulum,
    /// `atrium-netd-macos` — network broker.
    Netd,
    /// `praeco-macos` — notifications.
    Praeco,
    /// `tabellarius-macos` — push delivery.
    Tabellarius,
}

impl Daemon {
    /// Short name used in pid/socket file paths.
    pub fn slug(self) -> &'static str {
        match self {
            Daemon::Logd => "insula-logd",
            Daemon::Vestibulum => "vestibulum-macos",
            Daemon::Netd => "atrium-netd-macos",
            Daemon::Praeco => "praeco-macos",
            Daemon::Tabellarius => "tabellarius-macos",
        }
    }

    /// Binary name as found in PATH (or via
    /// `INSULA_<NAME>_BIN` override).
    pub fn binary_name(self) -> &'static str {
        match self {
            Daemon::Logd => "insula-logd",
            Daemon::Vestibulum => "vestibulum-macos",
            Daemon::Netd => "atrium-netd-macos",
            Daemon::Praeco => "praeco-macos",
            Daemon::Tabellarius => "tabellarius-macos",
        }
    }

    /// Env var the daemon respects for choosing its
    /// listen socket.
    pub fn socket_env(self) -> &'static str {
        match self {
            Daemon::Logd => "INSULA_LOGD_SOCKET",
            Daemon::Vestibulum => "INSULA_VESTIBULUMD_SOCKET",
            Daemon::Netd => "INSULA_NETD_SOCKET",
            Daemon::Praeco => "INSULA_PRAECOD_SOCKET",
            Daemon::Tabellarius => "INSULA_TABELLARIUSD_SOCKET",
        }
    }

    /// Optional env var the daemon respects for its
    /// log file (logd + praeco currently).
    pub fn log_env(self) -> Option<&'static str> {
        match self {
            Daemon::Logd => Some("INSULA_LOGD_LOG_FILE"),
            Daemon::Vestibulum => None,
            Daemon::Netd => None,
            Daemon::Praeco => Some("INSULA_PRAECOD_LOG_FILE"),
            Daemon::Tabellarius => None,
        }
    }

    /// Env-var override an integrator can set to point
    /// the CLI at a binary outside PATH (useful in
    /// development / tests).
    pub fn binary_override_env(self) -> &'static str {
        match self {
            Daemon::Logd => "INSULA_LOGD_BIN",
            Daemon::Vestibulum => "INSULA_VESTIBULUMD_BIN",
            Daemon::Netd => "INSULA_NETD_BIN",
            Daemon::Praeco => "INSULA_PRAECOD_BIN",
            Daemon::Tabellarius => "INSULA_TABELLARIUSD_BIN",
        }
    }

    pub const ALL: [Daemon; 5] = [
        Daemon::Logd, Daemon::Vestibulum, Daemon::Netd,
        Daemon::Praeco, Daemon::Tabellarius,
    ];
}

/// All paths the CLI cares about for one daemon under
/// a given install root.
pub struct DaemonPaths {
    pub pid_file: PathBuf,
    pub socket: PathBuf,
    pub log_file: PathBuf,
}

pub fn paths_for(install_root: &Path, d: Daemon) -> DaemonPaths {
    let run = install_root.join("run");
    DaemonPaths {
        pid_file: run.join(format!("{}.pid", d.slug())),
        socket: run.join(format!("{}.sock", d.slug())),
        log_file: run.join(format!("{}.log", d.slug())),
    }
}

/// Is the process whose pid is stored in `pid_file`
/// alive? Returns the pid if alive, None if not (file
/// missing, stale, or kill(0) fails).
pub fn alive(pid_file: &Path) -> Option<i32> {
    let s = std::fs::read_to_string(pid_file).ok()?;
    let pid: i32 = s.trim().parse().ok()?;
    // kill(pid, 0) → 0 if alive, -1 + ESRCH if not.
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        Some(pid)
    } else {
        None
    }
}

/// Resolve the daemon binary path. Honors the per-
/// daemon override env var first; otherwise relies on
/// PATH lookup at spawn time (so we just return the
/// binary name and let Command::new(...) resolve it).
pub fn binary_path(d: Daemon) -> PathBuf {
    if let Some(p) = std::env::var_os(d.binary_override_env()) {
        return PathBuf::from(p);
    }
    PathBuf::from(d.binary_name())
}

/// Spawn the daemon in the background. Writes its pid
/// to the pid file. Returns the pid.
///
/// Idempotent — if the daemon is already alive,
/// returns the existing pid.
pub fn start(install_root: &Path, d: Daemon) -> Result<i32, String> {
    let paths = paths_for(install_root, d);

    if let Some(pid) = alive(&paths.pid_file) {
        return Ok(pid);
    }

    std::fs::create_dir_all(install_root.join("run")).map_err(|e| {
        format!("mkdir run/: {}", e)
    })?;

    let bin = binary_path(d);
    let mut cmd = Command::new(&bin);
    cmd.env(d.socket_env(), &paths.socket);
    if let Some(log_env) = d.log_env() {
        cmd.env(log_env, &paths.log_file);
    }
    // The network broker needs to know the install
    // root to resolve peer pid → app id → manifest
    // for per-app `[network]` enforcement.
    if matches!(d, Daemon::Netd) {
        cmd.env("INSULA_INSTALL_ROOT", install_root);
    }
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.stdin(Stdio::null());

    let child = cmd.spawn().map_err(|e| {
        format!("spawn {} (from {}): {}", d.binary_name(), bin.display(), e)
    })?;

    let pid = child.id() as i32;
    std::fs::write(&paths.pid_file, pid.to_string()).map_err(|e| {
        format!("write pid file: {}", e)
    })?;

    // Best-effort wait for the daemon to bind its
    // socket. Up to 3 seconds; this matches what the
    // existing daemon integration tests do.
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        if paths.socket.exists() {
            return Ok(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Even if we timed out, the daemon may still be
    // starting; return the pid and let the caller
    // decide whether to retry.
    Ok(pid)
}

/// Send SIGTERM to the daemon if its pid file is
/// alive; clean up pid file + socket.
pub fn stop(install_root: &Path, d: Daemon) -> Result<(), String> {
    let paths = paths_for(install_root, d);
    if let Some(pid) = alive(&paths.pid_file) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Brief wait for graceful exit; then SIGKILL
        // if still alive.
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(500) {
            if alive(&paths.pid_file).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if alive(&paths.pid_file).is_some() {
            unsafe { libc::kill(pid, libc::SIGKILL); }
        }
    }
    let _ = std::fs::remove_file(&paths.pid_file);
    let _ = std::fs::remove_file(&paths.socket);
    Ok(())
}

/// Get the current state of one daemon as a tuple
/// `(running, pid, socket_exists)`.
pub fn status(install_root: &Path, d: Daemon) -> (bool, Option<i32>, bool) {
    let paths = paths_for(install_root, d);
    let pid = alive(&paths.pid_file);
    (pid.is_some(), pid, paths.socket.exists())
}

/// Get the daemon's socket path if the daemon is
/// running, for the auto-routing path in
/// `insula launch`.
pub fn socket_if_running(install_root: &Path, d: Daemon) -> Option<PathBuf> {
    let paths = paths_for(install_root, d);
    if alive(&paths.pid_file).is_some() && paths.socket.exists() {
        Some(paths.socket)
    } else {
        None
    }
}

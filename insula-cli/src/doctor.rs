//! `insula doctor` — operational health check.
//!
//! Probes a set of "is this install healthy?" checks
//! across the install root, the five managed daemons,
//! and the publisher trust store. Each check reports
//! one of:
//!
//!   - **ok**       — green; nothing to do
//!   - **warn**     — yellow; configurable / optional
//!                    state that's worth noticing
//!                    (e.g. "no trusted publishers
//!                    registered") but doesn't break
//!                    the install
//!   - **error**    — red; the install is broken in a
//!                    way that will likely surface as
//!                    a failure on the next operation
//!
//! Exit code is 1 if any error is reported, 0
//! otherwise. Warns alone don't fail the command.
//!
//! Intended as the first thing a user runs when
//! "things aren't working", before going deeper with
//! `insula daemons status` / `insula daemons logs`.

use crate::daemons::{self, Daemon};
use aqueduct::Connection;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Warn,
    Error,
}

struct Check {
    name: String,
    severity: Severity,
    detail: String,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check { name: name.into(), severity: Severity::Ok, detail: detail.into() }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check { name: name.into(), severity: Severity::Warn, detail: detail.into() }
    }
    fn err(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check { name: name.into(), severity: Severity::Error, detail: detail.into() }
    }

    fn marker(&self) -> &'static str {
        match self.severity {
            Severity::Ok => "ok   ",
            Severity::Warn => "warn ",
            Severity::Error => "error",
        }
    }
}

pub fn cmd_doctor(args: &[String], install_root: &Path) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!(
            "doctor: takes no arguments, got {:?}", args
        ));
    }

    let mut checks: Vec<Check> = Vec::new();

    // 1. Install root must exist + be writable.
    checks.push(check_install_root(install_root));

    // 2. run/ subdir exists (created lazily on first
    //    daemon start; not an error if missing on a
    //    fresh install).
    let run_dir = install_root.join("run");
    if run_dir.is_dir() {
        checks.push(Check::ok(
            "run-dir",
            format!("exists at {}", run_dir.display()),
        ));
    } else {
        checks.push(Check::warn(
            "run-dir",
            "missing (will be created on first daemon start)".to_string(),
        ));
    }

    // 3. Each daemon: binary locatable + (if a pid
    //    file is present) the process is alive + the
    //    socket connects.
    for d in Daemon::ALL {
        check_daemon(d, install_root, &mut checks);
    }

    // 4. Trusted-publishers store.
    checks.push(check_trusted_publishers(install_root));

    // 5. Apps directory + per-app container perms.
    check_apps(install_root, &mut checks);

    // ----- print -----
    for c in &checks {
        println!("[{}] {:18}  {}", c.marker(), c.name, c.detail);
    }
    println!();
    let n_ok = checks.iter().filter(|c| c.severity == Severity::Ok).count();
    let n_warn = checks.iter().filter(|c| c.severity == Severity::Warn).count();
    let n_err = checks.iter().filter(|c| c.severity == Severity::Error).count();
    println!("summary: {} ok, {} warn, {} error", n_ok, n_warn, n_err);

    if n_err > 0 {
        Err(format!("{} check(s) failed", n_err))
    } else {
        Ok(())
    }
}

fn check_install_root(install_root: &Path) -> Check {
    if !install_root.is_dir() {
        return Check::err(
            "install-root",
            format!("missing: {}", install_root.display()),
        );
    }
    // Try a write probe.
    let probe = install_root.join(".doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::ok(
                "install-root",
                format!("{} (writable)", install_root.display()),
            )
        }
        Err(e) => Check::err(
            "install-root",
            format!("{} not writable: {}", install_root.display(), e),
        ),
    }
}

fn check_daemon(d: Daemon, install_root: &Path, out: &mut Vec<Check>) {
    let bin = daemons::binary_path(d);
    let bin_ok = bin.is_file();
    let (running, _pid, sock_present) = daemons::status(install_root, d);
    let paths = daemons::paths_for(install_root, d);

    // (a) Binary availability.
    if bin_ok {
        out.push(Check::ok(
            format!("{}/binary", d.slug()),
            format!("at {}", bin.display()),
        ));
    } else {
        // It's a warn, not an error: the binary may
        // come from $PATH at spawn time and we don't
        // have a portable way to verify that without
        // actually spawning.
        out.push(Check::warn(
            format!("{}/binary", d.slug()),
            format!(
                "not found at {} (set {} or rely on $PATH)",
                bin.display(), d.binary_override_env()
            ),
        ));
    }

    // (b) Liveness + socket reachability. If the
    //     daemon isn't running, that's information,
    //     not a failure — `insula launch` will
    //     auto-spawn it. We only report an error if
    //     the pid file claims it's running but the
    //     process is dead or the socket is missing.
    if running {
        // Try a connect to confirm the socket isn't
        // wedged. The connection drops immediately;
        // we just want to know if accept() succeeds.
        if sock_present {
            match Connection::connect(&paths.socket) {
                Ok(_) => out.push(Check::ok(
                    format!("{}/socket", d.slug()),
                    format!("running, accepts connections at {}", paths.socket.display()),
                )),
                Err(e) => out.push(Check::err(
                    format!("{}/socket", d.slug()),
                    format!(
                        "pid alive but socket {} won't accept: {}",
                        paths.socket.display(), e
                    ),
                )),
            }
        } else {
            out.push(Check::err(
                format!("{}/socket", d.slug()),
                format!(
                    "pid alive but socket {} missing",
                    paths.socket.display()
                ),
            ));
        }
    } else {
        out.push(Check::ok(
            format!("{}/state", d.slug()),
            "stopped (will auto-spawn on demand)".to_string(),
        ));
    }
}

fn check_trusted_publishers(install_root: &Path) -> Check {
    let dir = install_root.join("trusted-publishers");
    if !dir.is_dir() {
        return Check::warn(
            "trusted-publishers",
            "directory missing (no publishers trusted yet; \
             unsigned installs will require --allow-unsigned)".to_string(),
        );
    }
    let n = std::fs::read_dir(&dir)
        .map(|it| it.filter_map(Result::ok)
                 .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("pub"))
                 .count())
        .unwrap_or(0);
    if n == 0 {
        return Check::warn(
            "trusted-publishers",
            "store exists but contains no .pub files".to_string(),
        );
    }
    Check::ok(
        "trusted-publishers",
        format!("{} publisher(s) trusted", n),
    )
}

fn check_apps(install_root: &Path, out: &mut Vec<Check>) {
    let apps_dir = install_root.join("apps");
    if !apps_dir.is_dir() {
        out.push(Check::warn(
            "apps",
            "no apps installed".to_string(),
        ));
        return;
    }
    let entries: Vec<_> = match std::fs::read_dir(&apps_dir) {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(e) => {
            out.push(Check::err(
                "apps",
                format!("read_dir {}: {}", apps_dir.display(), e),
            ));
            return;
        }
    };
    let n = entries.len();
    if n == 0 {
        out.push(Check::warn("apps", "no apps installed".to_string()));
        return;
    }
    out.push(Check::ok("apps", format!("{} app(s) installed", n)));

    // Per-app structural check: bundle/manifest.toml
    // exists, container/ exists.
    for entry in entries {
        let p = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        let mut issues = Vec::new();
        if !p.join("bundle").join("manifest.toml").is_file() {
            issues.push("missing bundle/manifest.toml");
        }
        if !p.join("container").is_dir() {
            issues.push("missing container/");
        }
        if issues.is_empty() {
            out.push(Check::ok(
                format!("apps/{}", id),
                "layout looks healthy".to_string(),
            ));
        } else {
            out.push(Check::err(
                format!("apps/{}", id),
                issues.join("; "),
            ));
        }
    }
}

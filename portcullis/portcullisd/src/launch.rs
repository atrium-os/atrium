//! Per-app jail lifecycle, daemon-side. Mirrors what the CLI used
//! to do directly in cmd_launch — moved here so the daemon owns
//! every privileged operation and the CLI becomes a thin client.
//!
//! Phase 4.4 step 1 scope: launched apps inherit the daemon's
//! stdio. App output therefore lands in the daemon log, not on the
//! requesting client's terminal. SCM_RIGHTS pty passing arrives in
//! step 2 and replaces stdio inheritance with proper terminal handoff.

use std::fs;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts};
use portcullis_toml::Manifest;

const APPS_DIR:     &str = "/var/lib/atrium/apps";
const OVERLAYS_DIR: &str = "/var/lib/atrium/overlays";
const JAILS_DIR:    &str = "/var/lib/atrium/jails";

#[derive(Debug)]
pub enum LaunchError {
    /// `(stage, message)` — `stage` is "manifest", "build", "mount",
    /// "jail-c", "teardown", etc., useful for the client to show
    /// where in the lifecycle things broke.
    Failed(&'static str, String),
}

impl LaunchError {
    pub fn stage(&self) -> &'static str {
        match self { LaunchError::Failed(s, _) => s }
    }
    pub fn message(&self) -> String {
        match self { LaunchError::Failed(_, m) => m.clone() }
    }
}

pub struct LaunchOutcome {
    /// Exit code from `jail -c` (the wrapped app's exit code by way
    /// of jail(8) propagation). `None` if the jail terminated by
    /// signal — Unix has no numeric exit code in that case.
    pub exit_code: Option<i32>,
}

/// Run an app inside its per-app jail. Synchronous: returns when
/// the jail has exited and been torn down.
///
/// `stdio = None` → jail(8) inherits the daemon's stdio (output
/// goes to the daemon log). `stdio = Some([in, out, err])` →
/// jail(8) runs with those three fds as 0/1/2, so a launching
/// client's terminal becomes the app's terminal.
///
/// Caller is responsible for the policy check — by the time we get
/// here, the launch is already approved (or the caller passed
/// bypass_policy and accepts dev-mode semantics).
pub fn launch_with_stdio(
    app_id: &str,
    stdio: Option<[OwnedFd; 3]>,
) -> Result<LaunchOutcome, LaunchError> {
    let tree = PathBuf::from(APPS_DIR).join(app_id);
    if !tree.exists() {
        return Err(LaunchError::Failed("manifest",
            format!("app id {app_id:?} not installed at {}", tree.display())));
    }

    let manifest_path = tree.join("atrium.toml");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|e| LaunchError::Failed("manifest",
                 format!("{}: {e}", manifest_path.display())))?;
    let manifest = Manifest::from_str(&text)
        .map_err(|e| LaunchError::Failed("manifest",
                 format!("parse error: {e}")))?;
    let report = portcullis_toml::validate(&manifest);
    if !report.is_ok() {
        let msg = report.errors.join("; ");
        return Err(LaunchError::Failed("manifest", msg));
    }

    let overlay_dir = PathBuf::from(OVERLAYS_DIR).join(app_id);
    let jail_path   = PathBuf::from(JAILS_DIR).join(app_id);

    let opts = BuildOpts {
        root_path:    jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    std::env::var_os("HOME")
                          .map(PathBuf::from)
                          .unwrap_or_else(|| PathBuf::from("/")),
        user_name:    std::env::var("USER").unwrap_or_else(|_| "atrium".into()),
        devfs_ruleset: 99,
    };
    let jc = build(&manifest, &opts)
        .map_err(|e| LaunchError::Failed("build", e.to_string()))?;

    fs::create_dir_all(&overlay_dir)
        .map_err(|e| LaunchError::Failed("mount",
                 format!("create {}: {e}", overlay_dir.display())))?;
    fs::create_dir_all(&jail_path)
        .map_err(|e| LaunchError::Failed("mount",
                 format!("create {}: {e}", jail_path.display())))?;

    /* Stale-mount cleanup. Silent — common case is "nothing to
     * unmount" and the noise was clutter in the CLI version. */
    let _ = umount_silent(&jail_path);
    let _ = umount_silent(&jail_path);

    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro",
                                    tree.to_str().unwrap(),
                                    jail_path.to_str().unwrap()]) {
        return Err(LaunchError::Failed("mount", format!("nullfs: {e}")));
    }
    if let Err(e) = run("mount", &["-t", "unionfs",
                                    overlay_dir.to_str().unwrap(),
                                    jail_path.to_str().unwrap()]) {
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount", format!("unionfs: {e}")));
    }

    let conf_path = std::env::temp_dir().join(format!(
        "portcullisd-{}-{}.conf", std::process::id(), app_id.replace('.', "_")));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        teardown(&jail_path, &jc.name);
        return Err(LaunchError::Failed("jail-c",
                   format!("write {}: {e}", conf_path.display())));
    }

    /* Stdio: if the caller passed three fds (the requesting
     * client's stdin/stdout/stderr, handed over via SCM_RIGHTS),
     * point jail(8) at them so the launched app talks to the
     * client's terminal. Otherwise inherit the daemon's stdio
     * (app output → daemon log). */
    let mut cmd = Command::new("jail");
    cmd.arg("-c").arg("-f").arg(&conf_path).arg(&jail_name_from_app_id(app_id));
    if let Some([sin, sout, serr]) = stdio {
        cmd.stdin (Stdio::from(sin));
        cmd.stdout(Stdio::from(sout));
        cmd.stderr(Stdio::from(serr));
    }
    let status = cmd.status();
    let _ = fs::remove_file(&conf_path);

    teardown(&jail_path, &jc.name);

    match status {
        Ok(s) => Ok(LaunchOutcome { exit_code: s.code() }),
        Err(e) => Err(LaunchError::Failed("jail-c",
                  format!("could not invoke jail(8): {e}"))),
    }
}

fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

fn umount(p: &Path) -> std::io::Result<()> {
    let st = Command::new("umount").arg(p).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("umount {} failed: {st}", p.display())));
    }
    Ok(())
}

fn umount_silent(p: &Path) -> std::io::Result<()> {
    let _ = Command::new("umount")
        .arg(p)
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(())
}

/// jail -r is idempotent if exec.start exit already removed the
/// jail; umounts are in reverse order: devfs (mount.devfs in
/// jail.conf), then unionfs (upper), then nullfs (lower).
fn teardown(jail_path: &Path, jail_name: &str) {
    let _ = Command::new("jail").arg("-r").arg(jail_name).status();
    let _ = umount(&jail_path.join("dev"));
    let _ = umount(jail_path);
    let _ = umount(jail_path);
}

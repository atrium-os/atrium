//! Per-app jail lifecycle, daemon-side. Mirrors what the CLI used
//! to do directly in cmd_launch — moved here so the daemon owns
//! every privileged operation and the CLI becomes a thin client.
//!
//! Two-phase launch (Phase 4.5):
//!   1. If `<overlay>/.atrium-firstrun-done` is absent and the app
//!      manifest has a `[setup]` block, build a *setup jail* with
//!      merged caps (runtime + setup overrides), run setup.command,
//!      write the sentinel on success, then continue.
//!   2. Build the runtime jail with plain `[capabilities]` and run
//!      the app.
//!
//! Both phases share the same overlay mount, so anything the setup
//! script writes to /usr/local, /etc, /var, etc. lands in the
//! overlay and is visible to the runtime app — that's the whole
//! point of separating the two phases (e.g., `pkg install` during
//! setup with `network = "full"`, then runtime sees the installed
//! files with `network = "none"`).

use std::fs;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts, JailConfig};
use portcullis_toml::{merge_capabilities, Manifest};

const APPS_DIR:     &str = "/var/lib/atrium/apps";
const OVERLAYS_DIR: &str = "/var/lib/atrium/overlays";
const JAILS_DIR:    &str = "/var/lib/atrium/jails";
const SENTINEL:     &str = ".atrium-firstrun-done";

#[derive(Debug)]
pub enum LaunchError {
    /// `(stage, message)` — `stage` is "manifest", "build", "mount",
    /// "jail-c", "setup", "teardown", etc., useful for the client to
    /// show where in the lifecycle things broke.
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
    /// Exit code from the runtime jail (the wrapped app's exit code
    /// by way of jail(8) propagation). `None` if the jail terminated
    /// by signal — Unix has no numeric exit code in that case.
    pub exit_code: Option<i32>,
}

pub fn launch_with_stdio(
    app_id: &str,
    user:   &str,
    stdio:  Option<[OwnedFd; 3]>,
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
    /* manifest TRUST gate — the SAME shared check as the Launch path, so a
     * session/stdio-launched app is signature-verified identically (any
     * user-app launch vector goes through one gate, not a per-path copy). */
    crate::manifest_trust::verify(&tree, &text)
        .map_err(|m| LaunchError::Failed("signature", m))?;
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

    /* The connecting user becomes the in-jail uid via exec.jail_user
     * (set by portcullis-jail::build from opts.user_name). $HOME is
     * the user's actual home on the host, used by ~/-prefixed
     * filesystem capabilities. */
    let user_home = match std::ffi::CString::new(user).ok().and_then(|cuser| {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let r = unsafe {
            libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(),
                             buf.len(), &mut result)
        };
        if r != 0 || result.is_null() { None }
        else {
            let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
            Some(PathBuf::from(dir.to_string_lossy().into_owned()))
        }
    }) {
        Some(h) => h,
        None    => PathBuf::from(format!("/home/{user}")),
    };
    let opts = BuildOpts {
        root_path:    jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    user_home.clone(),
        user_name:    user.to_string(),
        devfs_ruleset: 99,
    };

    /* Mount overlay once; both setup and runtime jails see the same
     * union, so setup writes to /usr/local etc. are visible to the
     * runtime app. */
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

    /* mount.devfs (set in jail.conf) needs the /dev mountpoint to
     * exist inside jail.path. App trees don't ship /dev/, so create
     * it now via the unionfs upper layer (writes land in overlay/dev). */
    if let Err(e) = fs::create_dir_all(jail_path.join("dev")) {
        let _ = umount(&jail_path);
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount",
            format!("mkdir jail/dev: {e}")));
    }

    /* jail(8) with exec.jail_user does an implicit chdir to the
     * user's home (looked up via host passwd because of
     * exec.system_jail_user=true). App trees don't ship that
     * dir, so create it inside the jail (writes land in overlay).
     * Empty dir is fine — apps that need real $HOME contents
     * declare a `filesystem` capability that bind-mounts them.
     * Path varies by user: /home/<user>, /root, etc. */
    let home_in_jail = jail_path.join(
        user_home.strip_prefix("/").unwrap_or(&user_home));
    if let Err(e) = fs::create_dir_all(&home_in_jail) {
        let _ = umount(&jail_path);
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount",
            format!("mkdir {}: {e}", home_in_jail.display())));
    }

    /* Phase 4.5: first-run setup. Sentinel lives in the overlay so
     * it persists across launches; portcullis reinstall removes it
     * to force a re-run. */
    let sentinel_path = overlay_dir.join(SENTINEL);
    if !sentinel_path.exists() {
        if let Some(setup) = &manifest.setup {
            let setup_caps = match &setup.capabilities {
                Some(ovr) => merge_capabilities(&manifest.capabilities, ovr),
                None      => manifest.capabilities.clone(),
            };
            /* Synthesise a setup-time manifest with merged caps and
             * setup.command as the entry. Same opts → same root_path
             * → setup runs in the overlay we just mounted. */
            let mut setup_manifest = clone_manifest_with(
                &manifest, setup_caps, setup.command.clone());
            setup_manifest.app.id = format!("{}_setup", manifest.app.id);
            let setup_jc = build(&setup_manifest, &opts)
                .map_err(|e| {
                    full_teardown(&jail_path, None);
                    LaunchError::Failed("build",
                        format!("setup-jail build: {e}"))
                })?;
            ensure_mountpoints(&setup_jc)
                .map_err(|e| { full_teardown(&jail_path, None); e })?;
            let setup_exit = run_one_jail(&setup_jc, &jail_path, /* stdio
                         inherits daemon intentionally — Phase 4.5
                         step-1 simplification. Step-2 will pipe setup
                         output through the same tty as the runtime
                         app. */ None)
                .map_err(|e| { full_teardown(&jail_path, None); e })?;
            if setup_exit != Some(0) {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("setup",
                    format!("setup script exited {:?} (sentinel not written; \
                             next launch will retry)", setup_exit)));
            }
            /* Sentinel write goes directly to the overlay-on-host;
             * unionfs writes from inside the jail land there too,
             * but the daemon owning this is more honest. */
            if let Err(e) = fs::write(&sentinel_path, b"ok\n") {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("setup",
                    format!("write sentinel: {e}")));
            }
        } else {
            /* No [setup] block → still write sentinel so we don't
             * stat() the file every launch from now on. */
            let _ = fs::write(&sentinel_path, b"ok\n");
        }
    }

    /* Runtime phase. Build and run the actual app jail with plain
     * runtime capabilities. */
    let jc = build(&manifest, &opts)
        .map_err(|e| {
            full_teardown(&jail_path, None);
            LaunchError::Failed("build", e.to_string())
        })?;
    if let Err(e) = ensure_mountpoints(&jc) {
        full_teardown(&jail_path, None);
        return Err(e);
    }
    let outcome = run_one_jail(&jc, &jail_path, stdio);

    full_teardown(&jail_path, Some(&jc.name));

    outcome.map(|exit_code| LaunchOutcome { exit_code })
}

/// Clone a manifest with substituted capabilities + entry. Used to
/// build the setup-phase manifest from the runtime one without
/// disturbing the original.
fn clone_manifest_with(
    base:  &Manifest,
    caps:  portcullis_toml::Capabilities,
    entry: String,
) -> Manifest {
    Manifest {
        app: portcullis_toml::AppSection {
            id:          base.app.id.clone(),
            name:        base.app.name.clone(),
            version:     base.app.version.clone(),
            entry,
            description: base.app.description.clone(),
            icon:        base.app.icon.clone(),
        },
        capabilities: caps,
        setup:        None,           /* setup never recurses */
        resources:    None,
        supervision:  None,
    }
}

/// Create every capability mount's destination (the mountpoint) before jail(8)
/// runs. jail(8) does not create mountpoints, and the read-only app tree can't
/// carry them, so without this each `mount +=` (fresco.sock, forum-ctl.sock,
/// clipboard.sock, a filesystem dir, …) fails and the jail never starts. We stat
/// the source to pick the mountpoint type: a directory source → a directory
/// mountpoint; anything else (a unix socket or file, e.g. the compositor socket)
/// → a regular-file mountpoint. Writes land in the unionfs overlay, leaving the
/// read-only app tree untouched. Mirrors the existing dev/ + home/ creation above.
fn ensure_mountpoints(jc: &JailConfig) -> Result<(), LaunchError> {
    for m in &jc.mounts {
        if let Some(parent) = m.dst.parent() {
            fs::create_dir_all(parent).map_err(|e| LaunchError::Failed(
                "mount", format!("mkdir mountpoint parent {}: {e}", parent.display())))?;
        }
        let src_is_dir = fs::metadata(&m.src).map(|md| md.is_dir()).unwrap_or(false);
        if src_is_dir {
            fs::create_dir_all(&m.dst).map_err(|e| LaunchError::Failed(
                "mount", format!("mkdir mountpoint {}: {e}", m.dst.display())))?;
        } else if !m.dst.exists() {
            fs::File::create(&m.dst).map_err(|e| LaunchError::Failed(
                "mount", format!("touch mountpoint {}: {e}", m.dst.display())))?;
        }
    }
    Ok(())
}

/// Run one jail-c against `jail_path`, capture exit, jail -r before
/// returning. Caller owns the over-arching mount lifecycle.
fn run_one_jail(
    jc:        &JailConfig,
    jail_path: &Path,
    stdio:     Option<[OwnedFd; 3]>,
) -> Result<Option<i32>, LaunchError> {
    let conf_path = std::env::temp_dir().join(format!(
        "portcullisd-{}-{}.conf", std::process::id(), jc.name));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        return Err(LaunchError::Failed("jail-c",
                   format!("write {}: {e}", conf_path.display())));
    }
    let mut cmd = Command::new("jail");
    cmd.arg("-c").arg("-f").arg(&conf_path).arg(&jc.name);
    if let Some([sin, sout, serr]) = stdio {
        cmd.stdin (Stdio::from(sin));
        cmd.stdout(Stdio::from(sout));
        cmd.stderr(Stdio::from(serr));
    }
    let status = cmd.status();
    let _ = fs::remove_file(&conf_path);
    /* Per-jail teardown: jail -r releases the jail's specific mounts
     * (mount.devfs, capability mounts) without touching the
     * overarching overlay union. */
    let _ = Command::new("jail").arg("-r").arg(&jc.name).status();
    let _ = umount(&jail_path.join("dev"));

    let code = match status {
        Ok(s) => s.code(),
        Err(e) => return Err(LaunchError::Failed("jail-c",
                  format!("could not invoke jail(8): {e}"))),
    };
    /* Setup phase: a non-zero exit aborts the launch (sentinel
     * stays absent so next launch retries). For the runtime phase
     * the caller passes the code through verbatim — non-zero is
     * just "the app exited non-zero", not a failure. So this fn
     * returns Ok always for now and the caller distinguishes by
     * looking at jc.name (or by passing an "is_setup" flag).
     *
     * Today setup failures are detected at the call site by
     * checking `code != Some(0)` after this returns. */
    Ok(code)
}

/// Final teardown: optional jail -r (idempotent if already removed),
/// then unmount in reverse order: unionfs, then nullfs.
fn full_teardown(jail_path: &Path, jail_name: Option<&str>) {
    if let Some(n) = jail_name {
        let _ = Command::new("jail").arg("-r").arg(n).status();
    }
    let _ = umount(&jail_path.join("dev"));   /* belt-and-braces */
    let _ = umount(jail_path);                 /* unionfs */
    let _ = umount(jail_path);                 /* nullfs */
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

/// Suppress "unused" warning for jail_name_from_app_id which is used
/// transitively via build()/render() but not directly here anymore.
#[allow(dead_code)]
fn _silence(s: &str) -> String { jail_name_from_app_id(s) }

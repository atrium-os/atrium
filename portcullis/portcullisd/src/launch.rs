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

    /* PRIVILEGE INVARIANT (portcullis.md §9.0): the app runs under a dedicated,
     * non-root, non-human per-app uid (50000+) — NEVER root, NEVER the connecting
     * human's uid. The connecting human (`user`) is recorded as the OWNER in the
     * launch registry; the process executes as the per-app uid that services
     * peer-cred back to (owner, app_id). $HOME is the app uid's own (nologin)
     * home, not the human's. */
    let (app_uid, run_as_user) = resolve_app_uid(user, app_id)?;
    eprintln!("portcullisd: launching {app_id} as uid {app_uid} ({run_as_user}); owner={user}");
    let user_home = match std::ffi::CString::new(run_as_user.as_str()).ok().and_then(|cuser| {
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
        None    => PathBuf::from("/nonexistent"),
    };
    let opts = BuildOpts {
        root_path:    jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    user_home.clone(),
        user_name:    run_as_user.clone(),
        devfs_ruleset: 99,
    };

    /* Mount overlay once; both setup and runtime jails see the same
     * union, so setup writes to /usr/local etc. are visible to the
     * runtime app. */
    fs::create_dir_all(&overlay_dir)
        .map_err(|e| LaunchError::Failed("mount",
                 format!("create {}: {e}", overlay_dir.display())))?;
    /* Before anything is mounted over it, and before the app can write:
     * close the dedup existence oracle on the overlay. */
    arm_overlay_dedup(&overlay_dir)?;
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
            sdk_version: base.app.sdk_version.clone(),
            description: base.app.description.clone(),
            icon:        base.app.icon.clone(),
        },
        /* The setup phase runs a script entry, not the bundled app — bundle/arch
         * facts don't apply, so it carries no [bundle] section. */
        bundle:       None,
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

/// Resolve the dedicated per-app uid the app executes as (PRIVILEGE INVARIANT,
/// portcullis.md §9.0). The connecting human is the OWNER (recorded in the launch
/// registry so services can peer-cred a connection back to `(owner, app_id)`); the
/// app itself runs as a 50000+ nologin "nobody" uid — never root, never the human.
///
/// Re-launches REUSE the app's existing uid (stable identity, no registry/passwd
/// leak); otherwise a fresh uid is allocated, given a nologin passwd entry, and
/// registered. Returns `(uid, run-as username)`.
///
/// NOTE: creating the passwd entry here mirrors the launch path already shelling
/// out (mount/jail/umount). Longer-term this privileged mutation belongs in jaild
/// (the audited broker) or at app-install time, not in portcullisd per-launch.
fn resolve_app_uid(owner: &str, app_id: &str) -> Result<(u32, String), LaunchError> {
    let reg = portcullis_peer::DEFAULT_REGISTRY;
    let fail = |s: String| LaunchError::Failed("uid", s);
    if let Some(dir) = Path::new(reg).parent() {
        let _ = fs::create_dir_all(dir);
    }
    let uid = match portcullis_peer::uid_for_app(reg, owner, app_id) {
        Some(uid) => uid,
        None => {
            let uid = portcullis_peer::allocate(reg);
            portcullis_peer::register(reg, uid, owner, app_id)
                .map_err(|e| fail(format!("register uid {uid}: {e}")))?;
            uid
        }
    };
    // Ensure a host passwd entry exists (jail(8) resolves exec.jail_user via it).
    let run_as = match portcullis_peer::username(uid) {
        Some(name) => name,
        None => {
            let name = portcullis_peer::app_username(uid);
            run("pw", &["useradd", &name, "-u", &uid.to_string(),
                        "-d", "/nonexistent", "-s", "/usr/sbin/nologin"])
                .map_err(|e| fail(format!("create passwd {name} (uid {uid}): {e}")))?;
            name
        }
    };
    Ok((uid, run_as))
}

/* ── dedup-oracle containment on overlays (tessera-quotas.md §3.6.2) ──
 *
 * An overlay is the one place a jailed app writes to the shared Tessera
 * volume, so it is where the dedup existence oracle leaks: write a candidate
 * file, fsync, read free space — unchanged means those bytes already exist
 * somewhere on the system. Measured on a GLOBAL domain, 4 MiB duplicate costs
 * 20 K vs 4156 K for novel content: a 208x, noise-free signal.
 *
 * `deferred` closes it. The write always allocates, so free space moves by the
 * full size regardless of content and the observer learns nothing; the
 * duplicate extents go into the dead-extent log and the GC reclaims them
 * later. Dedup is preserved — measured, 5 duplicates consumed 20700 K and
 * recovered 20740 K after the drain, files intact. That is why this is not
 * "give every app its own volume": a volume is the dedup boundary, and every
 * bundle shares a base, so per-app volumes would store it once per app.
 *
 * NEITHER PREREQUISITE IS A DEFAULT. Domains initialise to GLOBAL and
 * kern.tessera.dedup_deferred_enable is 0, so this must be armed explicitly —
 * which is the whole reason this function exists.
 */

/* FreeBSD _IOW('T', n, uint64_t): IOC_IN | (sizeof<<16) | ('T'<<8) | n */
const TESSERA_IOC_QUOTA_SET:    libc::c_ulong = 0x8008_5401;
const TESSERA_IOC_DEDUP_POLICY: libc::c_ulong = 0x8008_5403;
const TESSERA_DEDUP_DEFERRED:   u64 = 1;

/* The quota exists to CREATE THE DOMAIN — the kmod refuses a dedup policy on a
 * directory that is not already a quota root (EINVAL), because a policy on a
 * plain directory would silently apply to whatever domain it inherits, i.e.
 * the whole filesystem. It is not a meaningful cap on overlay growth; sizing
 * that is a separate question (storage.md). f_bavail is clamped to real pool
 * free space, so a limit above the volume size is harmless. */
const OVERLAY_DOMAIN_QUOTA_BYTES: u64 = 64 * 1024 * 1024 * 1024;

fn is_tessera(dir: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 { return false; }
    let name: Vec<u8> = sfs.f_fstypename.iter()
        .take_while(|&&ch| ch != 0).map(|&ch| ch as u8).collect();
    name == b"tessera"
}

fn arm_overlay_dedup(overlay_dir: &Path) -> Result<(), LaunchError> {
    /* Not on Tessera (a dev box on ZFS, say). Nothing to arm — but do NOT
     * pass silently: the containment simply is not in place, and a warning
     * that says so is the difference between a known gap and a believed-closed
     * one. */
    if !is_tessera(overlay_dir) {
        eprintln!("portcullisd: WARNING: {} is not on Tessera — the dedup existence oracle (tessera-quotas.md §3.6.2) is NOT closed for this app",
            overlay_dir.display());
        return Ok(());
    }

    /* Host-wide prerequisite. The kmod refuses to arm `deferred` without it,
     * deliberately: gating the publish pre-check without the dead-extent log
     * would leave the oracle exactly as open while paying the extra write. */
    let enabled = Command::new("sysctl")
        .args(["-n", "kern.tessera.dedup_deferred_enable"])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "1").unwrap_or(false);
    if !enabled {
        run("sysctl", &["kern.tessera.dedup_deferred_enable=1"]).map_err(|e| {
            LaunchError::Failed("mount",
                format!("enable deferred dedup: {e} (required by tessera-quotas.md §3.6.2)"))
        })?;
        eprintln!("portcullisd: enabled kern.tessera.dedup_deferred_enable");
    }

    let f = fs::File::open(overlay_dir).map_err(|e| {
        LaunchError::Failed("mount", format!("open {}: {e}", overlay_dir.display()))
    })?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);

    let mut limit: u64 = OVERLAY_DOMAIN_QUOTA_BYTES;
    if unsafe { libc::ioctl(fd, TESSERA_IOC_QUOTA_SET, &mut limit) } != 0 {
        return Err(LaunchError::Failed("mount", format!(
            "quota-set on {}: {} — cannot create the dedup domain",
            overlay_dir.display(), std::io::Error::last_os_error())));
    }
    let mut pol: u64 = TESSERA_DEDUP_DEFERRED;
    if unsafe { libc::ioctl(fd, TESSERA_IOC_DEDUP_POLICY, &mut pol) } != 0 {
        /* HARD FAILURE, deliberately. Launching with the oracle open would be
         * a silent security regression, and the failure is actionable. */
        return Err(LaunchError::Failed("mount", format!(
            "dedup-policy=deferred on {}: {} — refusing to launch with the \
dedup existence oracle open (tessera-quotas.md §3.6.2)",
            overlay_dir.display(), std::io::Error::last_os_error())));
    }
    eprintln!("portcullisd: {} armed deferred dedup (oracle closed)",
        overlay_dir.display());
    Ok(())
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

/// The installed-app catalog, as the daemon is willing to describe it.
///
/// Reads the app tree that only the TCB can see, so a jailed launcher never
/// needs `/var/lib/atrium/apps` mounted — it asks for this instead. Apps whose
/// manifest is unreadable or unparseable are skipped rather than reported: a
/// broken bundle is not something a launcher can act on, and listing it would
/// only offer the human a button that cannot work.
pub fn catalog() -> Vec<portcullis_ipc::CatalogEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        out.push(portcullis_ipc::CatalogEntry {
            id:          m.app.id.clone(),
            name:        m.app.name.clone(),
            description: m.app.description.clone(),
            icon:        m.app.icon.clone(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Does `app_id`'s installed manifest hold the `app-launch` capability?
///
/// The gate for [`catalog`]: the caller is identified by peer uid through the
/// launch registry, so this reads the capability off the INSTALLED manifest, not
/// off anything the caller sent. An app that is not in the registry at all (not
/// Portcullis-launched) resolves to no app-id and is refused by the caller.
pub fn app_may_launch_apps(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.app_launch == Some(true)
}

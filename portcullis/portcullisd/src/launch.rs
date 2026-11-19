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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts, JailConfig};
use portcullis_toml::{merge_capabilities, Manifest};

const APPS_DIR:     &str = "/var/lib/atrium/apps";
pub const JAILS_DIR:    &str = "/var/lib/atrium/jails";
const FILES_APP: &str = "org.atrium.scrinium";
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
    launch_instance(app_id, None, user, stdio)
}

// Files has one jail across all personas. Its launch thread owns both the
// restart and teardown, so a second launch cannot mount over a dying jail.
#[derive(Default)]
struct FilesRun {
    owner: String,
    refresh: bool,
    logging_out: bool,
}
static FILES_RUN: Mutex<Option<FilesRun>> = Mutex::new(None);

pub fn refresh_files_groups(owner: &str) {
    if let Some(run) = FILES_RUN.lock().unwrap().as_mut() {
        if run.owner == owner && !run.logging_out {
            run.refresh = true;
        }
    }
}

pub fn stop_files_for_logout(owner: &str) {
    if let Some(run) = FILES_RUN.lock().unwrap().as_mut() {
        if run.owner == owner {
            run.logging_out = true;
        }
    }
}

type BundleIdentity = (String, Option<[u8; 32]>);

/// Launch `(user, app_id)`'s instance in `persona` (personas.md §3; `None`:
/// Personal). A named persona's instance is its own jail
/// (`<id>@<persona>` root, `<name>__p_<persona>` jail), its own uid (registry
/// line `persona=<name>`), its own overlay (`persona-apps/<persona>/<id>`) and its own
/// synthetic host identity, so the two instances share nothing an app could
/// use to link them. The caller has checked the persona is the person's.
pub fn launch_instance(
    app_id:  &str,
    persona: Option<&str>,
    user:    &str,
    stdio:   Option<[OwnedFd; 3]>,
) -> Result<LaunchOutcome, LaunchError> {
    if app_id != FILES_APP || persona.is_some() {
        return launch_inner(app_id, persona, user, stdio, None, None);
    }
    {
        let mut active = FILES_RUN.lock().unwrap();
        if active.is_some() || running_jid_instance(app_id, None).is_some() {
            return Err(LaunchError::Failed("jail-c", "Files is already running or changing jails".into()));
        }
        *active = Some(FilesRun { owner: user.into(), ..FilesRun::default() });
    }

    let mut bundle = None;
    loop {
        // Keep the original launch's stdio available for every replacement.
        let copy_stdio = || -> std::io::Result<Option<[OwnedFd; 3]>> {
            stdio.as_ref().map(|[input, output, error]| {
                Ok([input.try_clone()?, output.try_clone()?, error.try_clone()?])
            }).transpose()
        };
        let result = copy_stdio()
            .map_err(|e| LaunchError::Failed("fdpass", e.to_string()))
            .and_then(|fds| launch_inner(app_id, persona, user, fds, None, Some(&mut bundle)));
        let mut active = FILES_RUN.lock().unwrap();
        let run = active.as_mut().unwrap();
        if result.is_err() || run.logging_out || !std::mem::take(&mut run.refresh) {
            *active = None;
            return result;
        }
        eprintln!("portcullisd: restarting Files for {user} with the current persona folders");
    }
}

/// Run dev app `app_id` under the debugger (dev-tools.md §7.2,
/// dev-tools-plan.md M10): `lldb-server gdbserver` is its parent in its own
/// jail, speaking over `conn`, a connection a debugger in the workspace
/// made. Only `dev.` apps (D9): an installed app is refused.
pub fn launch_debug(app_id: &str, user: &str, conn: OwnedFd) -> Result<LaunchOutcome, LaunchError> {
    if !app_id.starts_with("dev.") {
        return Err(LaunchError::Failed("debug", format!("{app_id} is not a dev build: installed apps are never debugged")));
    }
    launch_inner(app_id, None, user, None, Some(conn), None)
}

/// What a debug run mounts at [`DEBUG_IN_JAIL`]: lldb-server and sh from
/// the base (copied, so nothing else of /usr/bin goes in), and the script
/// that starts lldb-server on the debugger's connection. The base's /lib
/// and /usr/lib go in beside them, read-only, for their libraries, which
/// the loader is told of with `-o LIBRARY_PATH=`: it isn't put in the
/// environment, so the app lldb-server starts loads its own as ever.
const DEBUG_TREE: &str = "/var/lib/atrium/debug";
const DEBUG_IN_JAIL: &str = "atrium/debug";
/* ★ jail(8) closes every descriptor above 2, so the connection arrives as
 * stdin; the script moves it to 3 and gives the program /dev/null. And
 * jail(8) runs exec.start without a shell when it has no shell characters,
 * as here. */
const DEBUG_LD: &str = "/libexec/ld-elf.so.1 -o LIBRARY_PATH=/atrium/debug/lib:/atrium/debug/usr-lib";
const DEBUG_START: &str = "exec 3<&0 0</dev/null\nexec /libexec/ld-elf.so.1 -o LIBRARY_PATH=/atrium/debug/lib:/atrium/debug/usr-lib /atrium/debug/bin/lldb-server gdbserver --fd 3 -- \"$@\"\n";

fn debug_tree() -> Result<PathBuf, String> {
    let t = Path::new(DEBUG_TREE);
    let bin = t.join("bin");
    fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let _ = fs::remove_file(bin.join("env"));
    for (from, to) in [("/usr/bin/lldb-server", "lldb-server"), ("/bin/sh", "sh")] {
        let dst = bin.join(to);
        let fresh = fs::metadata(from).and_then(|a| fs::metadata(&dst).map(|b| (a, b)))
            .is_ok_and(|(a, b)| a.len() == b.len() && b.modified().ok() >= a.modified().ok());
        if !fresh {
            fs::copy(from, &dst).map_err(|e| format!("{from}: {e} (is lldb in the base?)"))?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
        }
    }
    fs::write(t.join("start"), DEBUG_START).map_err(|e| format!("start: {e}"))?;
    for d in ["lib", "usr-lib"] {
        fs::create_dir_all(t.join(d)).map_err(|e| e.to_string())?;
    }
    Ok(t.to_path_buf())
}

/// The app's filter with the `develop` class's added (ptrace, fork and
/// what lldb-server does), for a debug run only.
fn with_develop(app: Option<jaild_policy::SysfilParams>) -> Result<Option<jaild_policy::SysfilParams>, LaunchError> {
    let (Some(a), Some(d)) = (app, sysfil_for_class(DEVELOP_CLASS)?) else { return Ok(None) };
    let join = |x: &Option<String>, y: &Option<String>| match (x, y) {
        (None, _) | (_, None) => None,
        (Some(x), Some(y)) => {
            let mut v: Vec<&str> = x.split(',').chain(y.split(',')).filter(|s| !s.is_empty()).collect();
            v.sort();
            v.dedup();
            Some(v.join(","))
        }
    };
    Ok(Some(jaild_policy::SysfilParams {
        mode: a.mode,
        syscalls: join(&a.syscalls, &d.syscalls),
        ioctls: join(&a.ioctls, &d.ioctls),
        sockopts: join(&a.sockopts, &d.sockopts),
        families: join(&a.families, &d.families),
    }))
}

fn launch_inner(
    app_id:  &str,
    persona: Option<&str>,
    user:    &str,
    stdio:   Option<[OwnedFd; 3]>,
    debug:   Option<OwnedFd>,
    files_bundle: Option<&mut Option<BundleIdentity>>,
) -> Result<LaunchOutcome, LaunchError> {
    let mut stdio = stdio;
    if persona.is_some_and(|p| !portcullis_peer::valid_persona(p)) {
        return Err(LaunchError::Failed("persona", "invalid persona name".into()));
    }
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
    let publisher = crate::manifest_trust::verify_publisher(&tree, &text)
        .map_err(|m| LaunchError::Failed("signature", m))?;
    let manifest = Manifest::from_str(&text)
        .map_err(|e| LaunchError::Failed("manifest",
                 format!("parse error: {e}")))?;
    let refresh_groups = files_bundle.is_some();
    if let Some(pinned) = files_bundle {
        // A changed manifest or publisher must go through a new launch
        // request and its normal policy review.
        match pinned {
            Some((original, key)) if *original != text || *key != publisher => {
                return Err(LaunchError::Failed("manifest", "Files changed while running; open it again".into()));
            }
            None => *pinned = Some((text, publisher)),
            _ => {}
        }
    }
    let report = portcullis_toml::validate(&manifest);
    if !report.is_ok() {
        let msg = report.errors.join("; ");
        return Err(LaunchError::Failed("manifest", msg));
    }

    let jail_path   = PathBuf::from(JAILS_DIR).join(instance_key(app_id, persona));
    if persona.is_some() {
        if let Some(why) = not_instanced_manifest(&manifest) {
            return Err(LaunchError::Failed("persona", format!("{app_id} runs once for every persona: {why}")));
        }
    }

    /* PRIVILEGE INVARIANT (portcullis.md §9.0): the app runs under a dedicated,
     * non-root, non-human per-app uid (50000+) — NEVER root, NEVER the connecting
     * human's uid. The connecting human (`user`) is recorded as the OWNER in the
     * launch registry; the process executes as the per-app uid that services
     * peer-cred back to (owner, app_id). $HOME is the app uid's own (nologin)
     * home, not the human's. */
    let (app_uid, run_as_user) = resolve_instance_uid(user, app_id, publisher.as_ref(), persona)?;
    eprintln!("portcullisd: launching {app_id} as uid {app_uid} ({run_as_user}); owner={user} persona={}",
              persona.unwrap_or("personal"));
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
    /* ★ The app's synthetic host identity (portcullis.md §9.1c) — derived from
     * the BASE app id, so the setup phase below sees the same hostid/UUID the
     * app will: a licence registered during setup must still match at run. */
    let host_identity = portcullis_identity::for_app(&instance_key(&manifest.app.id, persona))
        .map_err(|e| LaunchError::Failed("identity", e.to_string()))?;
    let sysfil = if debug.is_some() { with_develop(app_sysfil(&manifest)?)? } else { app_sysfil(&manifest)? };
    let debug_tree = match &debug {
        Some(_) => Some(debug_tree().map_err(|e| LaunchError::Failed("debug", e))?),
        None => None,
    };
    let opts = BuildOpts {
        root_path:    jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    user_home.clone(),
        user_name:    run_as_user.clone(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        /* Named launches stay single-instance: this path owns
         * /var/lib/atrium/jails/<id> and one persistent overlay, so a second
         * concurrent jail here would share both. Concurrency belongs to the
         * one-shot piped path, which gets its own root per instance. */
        instance: persona.map(|p| format!("p-{p}")),
        persist: true,
        host_identity,
        sysfil,
    };

    /* Mount overlay once; both setup and runtime jails see the same
     * union, so setup writes to /usr/local etc. are visible to the
     * runtime app. */

    /* STEP 0 (portcullis.md §4.2): bring up the app's own overlay VOLUME, so
     * the jail's statfs reports its own quota instead of the pool. That is
     * what closes channel 1 of the dedup existence oracle, and it is the
     * reason the overlay is a volume rather than a directory: quota-scoped
     * statfs is per-mount and cannot be per-path.
     *
     * Stays mounted across launches — it holds the app's persistent state —
     * until `portcullis remove`, or the person's logout (E6: its image is in
     * their key group, which locks then). */
    let overlay_quota = manifest.resources.as_ref()
        .and_then(|r| r.storage.as_deref())
        .and_then(portcullis_overlay::parse_size)
        .unwrap_or(portcullis_overlay::DEFAULT_QUOTA_BYTES);
    // The person's app data in their key group (E6), before it mounts.
    if portcullis_overlay::home_of(user).is_some() {
        portcullisd::home::ensure_app_data(user, persona)
            .map_err(|m| LaunchError::Failed("app data", m))?;
    }
    // The person's own overlay, in their home (homes.md §3).
    let place = match persona {
        None => portcullis_overlay::place(Some(user), app_id),
        Some(p) => portcullis_overlay::persona_place(user, app_id, p).ok_or_else(||
            LaunchError::Failed("overlay", format!("no home place for {user}'s {p} instance of {app_id}")))?,
    };
    let overlay_dir = place.dir.clone();
    let backing = portcullis_overlay::ensure_mounted(&place, app_id, overlay_quota)
        .map_err(|m| LaunchError::Failed("mount", m))?;
    if backing == portcullis_overlay::Backing::Directory {
        /* No volume, so the structural mitigation is not in place. Fall back
         * to the behavioural one: a per-directory dedup domain on `deferred`,
         * which costs a full write per duplicate but keeps the observable
         * free-space number content-independent. */
        fs::create_dir_all(&overlay_dir)
            .map_err(|e| LaunchError::Failed("mount",
                     format!("create {}: {e}", overlay_dir.display())))?;
        arm_overlay_dedup(&overlay_dir)?;
    }
    /* homes.md §2: the overlay is the app uid's, 0700. Inside the jail it is
     * the union's root, so the app can make its own /var, /tmp and $HOME
     * there (the signed tree's folders stay root's); outside, nobody but
     * root and the app reads it, other apps of the same person included. */
    std::os::unix::fs::chown(&overlay_dir, Some(app_uid), Some(app_uid))
        .and_then(|_| fs::set_permissions(&overlay_dir, fs::Permissions::from_mode(0o700)))
        .map_err(|e| LaunchError::Failed("mount",
                 format!("own {}: {e}", overlay_dir.display())))?;
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
    // Made by root through the union; the app's own (homes.md §2).
    let _ = std::os::unix::fs::chown(&home_in_jail, Some(app_uid), Some(app_uid));

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
                         app. */ None, None)
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
    /* ★ The document channel (opening-files.md §6.2): an app that opens
     * types gets its own directory of the channel hub, and only its own,
     * at /atrium/sockets/opens. Made before the mount, so the source exists. */
    let mut jc = jc;
    /* A persona whose `vpn` names a profile (vpn.md §9.7): the instance's
     * network goes in that profile's route domain, whether or not the
     * tunnel is up. Down, the table is empty and the instance reaches
     * nothing (the kill switch, §5.3). No routing table, no launch: never
     * the host's table instead. */
    if jc.needs_routed_net.is_some() {
        let fib = portcullisd::route_domain::for_persona(user, persona)
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("network", e) })?;
        if let Some(f) = fib {
            eprintln!("portcullisd: {app_id} in {}: route domain fib {f}", persona.unwrap_or_default());
        }
        jc.route_fib = fib;
    }
    // The terminal gets its consoles the same way (homes.md §4).
    /* A named persona's instance has a channel of its own, keyed
     * `<id>@<persona>`: what reaches it comes from its own persona
     * (personas.md §3; crossing is P3, off by default). */
    if !manifest.opens.is_empty() || !manifest.links.is_empty() || manifest.capabilities.consoles == Some(true) {
        let dir = portcullisd::opens::Hub::global().listen(&portcullisd::opens::channel_key(app_id, persona))
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("opens", format!("document channel: {e}")) })?;
        jc.add_mount(&dir, &jail_path.join("atrium/sockets/opens"), "nullfs", &["rw"]);
    }
    /* ★ The person's own files (homes.md §2, scrinium.md §9 H1): only for an
     * app granted `home-files` (the file manager). Its own uid gets the
     * person's files group; the five folders go in at /atrium/home. */
    if manifest.capabilities.home_files == Some(true) {
        let person = portcullisd::home::ensure_person(user)
            .and_then(|(home, gid)| portcullisd::home::grant_group(user, &run_as_user).map(|_| (home, gid)));
        match person {
            Ok((home, gid)) => {
                for (src, dst) in portcullisd::home::mounts(&home, &jail_path) {
                    jc.add_mount(&src, &dst, "nullfs", &["rw"]);
                }
                /* The named personas' folders (personas.md §4.1): the file
                 * manager spans them, with each one's group; they go in at
                 * /atrium/personas/<persona>/. One persona failing leaves
                 * the others. */
                let named: Vec<String> = portcullis_policy::Policy::load(&portcullis_policy::Policy::user_path(user)).ok()
                    .filter(|p| p.check_personas().is_ok()).map(|p| p.personas.keys().cloned().collect()).unwrap_or_default();
                let mut any = false;
                for p in &named {
                    match portcullisd::home::ensure_persona(user, p).and_then(|_| portcullisd::home::grant_persona_group(user, p, &run_as_user)) {
                        Ok(()) => any = true,
                        Err(e) => eprintln!("portcullisd: {app_id}: {p}'s folders: {e}"),
                    }
                }
                if any {
                    jc.add_mount(&home.join(portcullisd::home::PERSONAS_DIR), &jail_path.join(portcullisd::home::PERSONAS_IN_JAIL), "nullfs", &["rw"]);
                }
                /* And the workspaces (homes.md §5): each one's `work`; their
                 * `.state` is root's alone. */
                match crate::workspace::root_dir(&home, gid) {
                    Ok(ws) => jc.add_mount(&ws, &jail_path.join(crate::workspace::IN_FILE_MANAGER), "nullfs", &["rw"]),
                    Err(e) => eprintln!("portcullisd: {app_id}: no workspaces place: {e}"),
                }
            }
            Err(e) => {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("home-files", e));
            }
        }
    }
    /* An IDE (`workspace-exec`, homes.md W6): /work, and the `work` of each
     * workspace the person picked in it. */
    if manifest.capabilities.workspace_exec == Some(true) {
        if let Err(e) = crate::ide::launch_mounts(user, app_id, &run_as_user, &jail_path, &mut jc) {
            full_teardown(&jail_path, None);
            return Err(LaunchError::Failed("workspace-exec", e));
        }
    }
    /* ★ A dev build runs from its bundle's own path (M10), mounted there in
     * its jail too: the kernel names what a jailed process maps by the
     * host's path (the nullfs root's lower files), and lldb needs the
     * program where the process says it was started. The workspace that
     * made it sees the bundle at the same path (workspace::show_bundle), so
     * a debugger there, or a core, finds the very files. */
    let mut entry_at = format!("/{}", manifest.entry());
    if portcullisd::manifest_trust::is_dev(app_id) && persona.is_none() {
        let at = Path::new(APPS_DIR).join(app_id);
        jc.add_mount(&tree, &jail_path.join(at.strip_prefix("/").unwrap_or(&at)), "nullfs", &["ro"]);
        entry_at = format!("{}/{}", at.display(), manifest.entry());
        jc.set("exec.start", portcullis_jail::Value::String(entry_at.clone()));
    }
    /* ★ Under the debugger (M10): lldb-server is the entry, the app its
     * child; debugging is allowed in this jail alone, as in a workspace. */
    if let (Some(conn), Some(t)) = (debug, &debug_tree) {
        let at = jail_path.join(DEBUG_IN_JAIL);
        jc.add_mount(t, &at, "nullfs", &["ro"]);
        jc.add_mount(Path::new("/lib"), &at.join("lib"), "nullfs", &["ro"]);
        jc.add_mount(Path::new("/usr/lib"), &at.join("usr-lib"), "nullfs", &["ro"]);
        jc.set("allow.unprivileged_proc_debug", portcullis_jail::Value::Bool(true));
        jc.set("exec.start", portcullis_jail::Value::String(format!(
            "{DEBUG_LD} /{DEBUG_IN_JAIL}/bin/sh /{DEBUG_IN_JAIL}/start {entry_at}")));
        let err = |e: std::io::Error| { full_teardown(&jail_path, None); LaunchError::Failed("debug", e.to_string()) };
        let out = std::os::fd::AsFd::as_fd(&std::io::stderr()).try_clone_to_owned().map_err(err)?;
        let out2 = out.try_clone().map_err(err)?;
        stdio = Some([conn, out, out2]);
        eprintln!("portcullisd: {app_id} starts under lldb-server");
    }
    if let Err(e) = ensure_mountpoints(&jc) {
        full_teardown(&jail_path, None);
        return Err(e);
    }
    let outcome = run_one_jail(&jc, &jail_path, stdio, refresh_groups.then_some(app_uid));

    let clean = full_teardown(&jail_path, Some(&jc.name));
    if refresh_groups && !clean {
        return Err(LaunchError::Failed("teardown", "Files' old mounts are still in use".into()));
    }

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
            updates:     base.app.updates.clone(),
        },
        /* The setup phase runs a script entry, not the bundled app — bundle/arch
         * facts don't apply, so it carries no [bundle] section. */
        bundle:       None,
        capabilities: caps,
        setup:        None,           /* setup never recurses */
        resources:    None,
        supervision:  None,
        opens:        Vec::new(),
        links:        Vec::new(),
        /* The setup script is no app: no workers, roles or IPC of its own. */
        workers:      Vec::new(),
        roles:        None,
        ipc:          None,
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
pub fn ensure_mountpoints(jc: &JailConfig) -> Result<(), LaunchError> {
    portcullis_mounts::ensure_mountpoints(jc).map_err(|e| LaunchError::Failed("mount", e))
}

/// Run one jail-c against `jail_path`, capture exit, jail -r before
/// returning. Caller owns the over-arching mount lifecycle.
fn run_one_jail(
    jc:        &JailConfig,
    jail_path: &Path,
    stdio:     Option<[OwnedFd; 3]>,
    refresh_uid: Option<u32>,
) -> Result<Option<i32>, LaunchError> {
    /* ★★ A `full` network comes from jaild, the one allocator (network.md §0):
     * a point-to-point epair with the app's derived MAC — never the host's
     * stack. Allocated for THIS jail and released when it is gone, on every
     * path out of here. */
    let mut jc = jc.clone();
    let networked = jc.needs_routed_net.clone();
    if let Some(mac) = &networked {
        /* The app's finer grants (network.md §0.1), hostnames resolved now. */
        let grants = match &jc.routed_net_spec {
            Some((id, spec)) => portcullis_oneshot::net_grants(spec, id)
                .map_err(|e| LaunchError::Failed("network", e))?,
            None => None,
        };
        let (b, app, host) = jaild::client::allocate_net_in(Path::new(JAILD_SOCK), &jc.name, mac, grants, jc.route_fib)
            .map_err(|e| LaunchError::Failed("network", e))?;
        jc.attach_routed_net(&b, &app, &host).map_err(|e| LaunchError::Failed("network", e))?;
        /* Names resolve through the host's resolvers, over NAT. Written into the
         * overlay, never the signed tree. */
        let etc = jail_path.join("etc");
        let _ = fs::create_dir_all(&etc);
        let _ = portcullis_oneshot::write_resolv_conf(&etc, jc.route_fib);
    }
    let outcome = run_one_jail_inner(&jc, jail_path, stdio, refresh_uid);
    if networked.is_some() {
        jaild::client::release_net(Path::new(JAILD_SOCK), &jc.name);
    }
    outcome
}

/// Create a jail that stays (`persist`, an entry that returns at once):
/// a workspace (homes.md §5), which lives until stopped, not until a
/// process exits. Its network, if any, is jaild's as for an app, and is
/// released by [`remove_persistent`].
pub fn create_persistent(jc: &JailConfig, jail_path: &Path) -> Result<(), LaunchError> {
    let mut jc = jc.clone();
    if let Some(mac) = jc.needs_routed_net.clone() {
        /* The jail isn't running, so a network jaild still holds for its name
         * is left from one that ended without a stop (a crash, a reboot of
         * portcullisd): released, or the allocation is refused. */
        jaild::client::release_net(Path::new(JAILD_SOCK), &jc.name);
        let (b, app, host) = jaild::client::allocate_net_in(Path::new(JAILD_SOCK), &jc.name, &mac, None, jc.route_fib)
            .map_err(|e| LaunchError::Failed("network", e))?;
        jc.attach_routed_net(&b, &app, &host).map_err(|e| LaunchError::Failed("network", e))?;
        let etc = jail_path.join("etc");
        let _ = fs::create_dir_all(&etc);
        let _ = portcullis_oneshot::write_resolv_conf(&etc, jc.route_fib);
    }
    let release = |jc: &JailConfig| if jc.needs_routed_net.is_some() {
        jaild::client::release_net(Path::new(JAILD_SOCK), &jc.name);
    };
    if let Err(e) = portcullis_mounts::ensure_devfs_isolation(&jc).map_err(|e| LaunchError::Failed("devfs", e))
        .and_then(|_| portcullis_mounts::ensure_network_ready(&jc).map_err(|e| LaunchError::Failed("network", e)))
    {
        release(&jc);
        return Err(e);
    }
    let conf_path = std::env::temp_dir().join(format!("portcullisd-{}-{}.conf", std::process::id(), jc.name));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        release(&jc);
        return Err(LaunchError::Failed("jail-c", format!("write {}: {e}", conf_path.display())));
    }
    let out = Command::new("jail").arg("-q").arg("-c").arg("-f").arg(&conf_path).arg(&jc.name)
        .stdin(Stdio::null()).output();
    let _ = fs::remove_file(&conf_path);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            release(&jc);
            Err(LaunchError::Failed("jail-c", String::from_utf8_lossy(&o.stderr).trim().to_string()))
        }
        Err(e) => {
            release(&jc);
            Err(LaunchError::Failed("jail-c", format!("could not invoke jail(8): {e}")))
        }
    }
}

/// End a jail made by [`create_persistent`] and take down its mounts.
pub fn remove_persistent(name: &str, jail_path: &Path, networked: bool) {
    let _ = Command::new("jail").arg("-q").arg("-r").arg(name).stderr(Stdio::null()).status();
    portcullis_mounts::wait_jail_gone(name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    if networked {
        jaild::client::release_net(Path::new(JAILD_SOCK), name);
    }
    let _ = umount(&jail_path.join("dev"));
    full_teardown(jail_path, None);
}

const JAILD_SOCK: &str = "/var/run/atrium/jaild.sock";
const JAILD_POLICY: &str = "/etc/atrium/jaild.policy.toml";

/// The app jail's system call filter (docs/spec/jail-sysfil.md §5.2): the
/// manifest's class, resolved from jaild's policy, so a jail(8) launch and a
/// jaild launch of the same app get the same lists. `None` on a kernel
/// without sysfil, which would refuse the parameters, unless the policy
/// says a filter is required.
pub fn app_sysfil(manifest: &Manifest) -> Result<Option<jaild_policy::SysfilParams>, LaunchError> {
    let fail = |s: String| LaunchError::Failed("sysfil", s);
    let policy = jaild_policy::Policy::load(JAILD_POLICY)
        .map_err(|e| fail(format!("{JAILD_POLICY}: {e}")))?;
    if !jaild::ffi::sysfil_supported() {
        if policy.sysfil.required {
            return Err(fail("this kernel has no system call filter and policy.sysfil.required is set".into()));
        }
        return Ok(None);
    }
    let class = portcullis_jail::sysfil_class(&manifest.capabilities);
    // ★ `develop` (ptrace, ktrace, procctl) is portcullisd's for workspaces
    // alone (dev-tools-plan.md M0): an app's manifest naming it is refused.
    if class == DEVELOP_CLASS {
        return Err(fail(format!("{}: class {class:?} is for workspaces only", manifest.app.id)));
    }
    policy.sysfil.resolve(&class).map(Some)
        .map_err(|e| fail(format!("{}: class {class:?}: {e}", manifest.app.id)))
}

/// The class portcullisd gives workspace jails (dev-tools-plan.md M0).
pub const DEVELOP_CLASS: &str = "develop";
/// The class of a console's shell: the empty console's jail and the console
/// jail beside an app's (dev-tools-plan.md M0, P1).
pub const TERMINAL_CLASS: &str = "terminal";

/// The filter for a jail portcullisd makes itself (a workspace, a console),
/// by class name, not from a manifest: [`app_sysfil`] refuses `develop`.
pub fn sysfil_for_class(class: &str) -> Result<Option<jaild_policy::SysfilParams>, LaunchError> {
    let fail = |s: String| LaunchError::Failed("sysfil", s);
    let policy = jaild_policy::Policy::load(JAILD_POLICY)
        .map_err(|e| fail(format!("{JAILD_POLICY}: {e}")))?;
    if !jaild::ffi::sysfil_supported() {
        if policy.sysfil.required {
            return Err(fail("this kernel has no system call filter and policy.sysfil.required is set".into()));
        }
        return Ok(None);
    }
    policy.sysfil.resolve(class).map(Some).map_err(|e| fail(format!("class {class:?}: {e}")))
}

fn run_one_jail_inner(
    jc:        &JailConfig,
    jail_path: &Path,
    stdio:     Option<[OwnedFd; 3]>,
    refresh_uid: Option<u32>,
) -> Result<Option<i32>, LaunchError> {
    /* ★★★ Refuse a devfs that would hide nothing — see ensure_devfs_isolation. */
    portcullis_mounts::ensure_devfs_isolation(jc)
        .map_err(|e| LaunchError::Failed("devfs", e))?;
    /* ★★ And a networked config whose network was never attached. */
    portcullis_mounts::ensure_network_ready(jc)
        .map_err(|e| LaunchError::Failed("network", e))?;
    let conf_path = std::env::temp_dir().join(format!(
        "portcullisd-{}-{}.conf", std::process::id(), jc.name));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        return Err(LaunchError::Failed("jail-c",
                   format!("write {}: {e}", conf_path.display())));
    }
    let mut cmd = Command::new("jail");
    /* ★★ `-q`: jail(8) prints "<name>: created" on STDOUT, and stdout here is
     * the CALLER's (received over SCM_RIGHTS) — the app's own output stream.
     * Measured 2026-09-22: a daemon launch's output began with
     * "test_e2e_nettest: created". The one-shot lane hit the same corruption
     * and fixed it the same way. */
    cmd.arg("-q").arg("-c").arg("-f").arg(&conf_path).arg(&jc.name);
    if let Some([sin, sout, serr]) = stdio {
        cmd.stdin (Stdio::from(sin));
        cmd.stdout(Stdio::from(sout));
        cmd.stderr(Stdio::from(serr));
    }
    let status = match refresh_uid {
        Some(uid) => wait_for_files(&mut cmd, &jc.name, uid),
        None => cmd.status(),
    };
    let _ = fs::remove_file(&conf_path);
    /* Per-jail teardown: jail -r releases the jail's specific mounts
     * (mount.devfs, capability mounts) without touching the
     * overarching overlay union. */
    /* The jail usually removed itself when its last process exited, so this
     * is a safety net; its "not found" is expected, not news for the log. */
    let _ = Command::new("jail").arg("-q").arg("-r").arg(&jc.name)
        .stderr(Stdio::null()).status();
    /* ★★ A dying jail pins its root; unmount only once it is gone. */
    portcullis_mounts::wait_jail_gone(&jc.name, portcullis_mounts::JAIL_GONE_TIMEOUT);
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

fn files_should_stop() -> bool {
    FILES_RUN.lock().unwrap().as_ref().is_some_and(|run| run.refresh || run.logging_out)
}

fn wait_for_files(cmd: &mut Command, jail: &str, uid: u32) -> std::io::Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    let uid = uid.to_string();
    let signal = |sig: &str| {
        Command::new("pkill").args([sig, "-j", jail, "-U", &uid])
            .status().is_ok_and(|s| s.success())
    };
    let mut term_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if let Some(at) = term_at {
            if at.elapsed() >= Duration::from_secs(3) {
                if !signal("-KILL") && child.try_wait()?.is_none() {
                    eprintln!("portcullisd: could not signal Files in {jail} after TERM");
                }
                return child.wait();
            }
        } else if files_should_stop() && signal("-TERM") {
            // A refresh during startup stays pending until the app is
            // actually in the jail and can receive the signal.
            term_at = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Final teardown: optional jail -r (idempotent if already removed), then
/// unmount everything the launch mounted — deepest first, convergently.
///
/// The mechanics moved to `portcullis-mounts` when the one-shot lane turned
/// out to have a near-copy of them and the CLI's fallback a worse one. The
/// history is worth keeping here because it is why the loop has the shape it
/// does: capability mounts live UNDER the jail path
/// (<jail>/atrium/sockets/fresco, …), so they held the overlay busy and the
/// whole stack survived —
///
///     umount: unmount of /var/lib/atrium/jails/<app> failed: Device busy
///
/// `jail -r` is documented to drop a jail's own mounts and demonstrably does
/// not always. Each relaunch then stacked a fresh set on top of the
/// survivors: 12 mounts with no jail running, no process, no open file.
///
/// ★ And a stacked pile cannot be cleaned up afterwards by path: a path
/// resolves through the topmost layer, so buried mounts answer "not a file
/// system root directory" and are reachable only by fsid. So the fix has to
/// be never to leave the pile, not to clean it up later.
///
/// ★ NOT forced, unlike the one-shot lane: an application's mounts may be
/// genuinely busy, and forcing would take a filesystem away from something
/// still using it. This path reports survivors instead.
pub fn full_teardown(jail_path: &Path, jail_name: Option<&str>) -> bool {
    if let Some(n) = jail_name {
        let _ = Command::new("jail").arg("-q").arg("-r").arg(n)
            .stderr(Stdio::null()).status();
    }
    // ★ A console jail on the same root (M0/P1) holds its mounts busy and
    // would outlive them: it goes first.
    remove_jails_at(jail_path);
    let mut left = portcullis_mounts::converge(&[jail_path], portcullis_mounts::Force::No);
    // A process that was in the jail can still hold the root for a moment
    // after the jail is gone (a console's shell being reaped): retry briefly
    // before calling a mount a survivor.
    let start = std::time::Instant::now();
    while !left.is_empty() && start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        left = portcullis_mounts::converge(&[jail_path], portcullis_mounts::Force::No);
    }
    if start.elapsed() >= std::time::Duration::from_millis(100) {
        eprintln!("portcullisd: {} unmounted after {} ms of retries{}", jail_path.display(), start.elapsed().as_millis(),
                  if left.is_empty() { "" } else { " (not all)" });
    }
    portcullis_mounts::warn_survivors("portcullisd", &left);
    left.is_empty()
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
pub fn resolve_app_uid(owner: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Result<(u32, String), LaunchError> {
    resolve_instance_uid(owner, app_id, publisher, None)
}

/// [`resolve_app_uid`] for the instance in `persona`: each persona's instance
/// has its own uid (personas.md §3), reused across its own launches only.
pub fn resolve_instance_uid(owner: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>) -> Result<(u32, String), LaunchError> {
    let reg = portcullis_peer::DEFAULT_REGISTRY;
    let fail = |s: String| LaunchError::Failed("uid", s);
    if let Some(dir) = Path::new(reg).parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _guard = portcullis_peer::lock_registry(reg).map_err(|e| fail(e.to_string()))?;
    /* Reuse only a binding with the SAME publisher: an app id re-installed under
     * another publisher's key is another app (arca.md §4.1) and gets a new uid. */
    let uid = match portcullis_peer::uid_for_app_instance(reg, owner, app_id, publisher, persona) {
        Some(uid) => uid,
        None => {
            let uid = portcullis_peer::allocate(reg);
            portcullis_peer::register_instance(reg, uid, owner, app_id, publisher, persona)
                .map_err(|e| fail(format!("register uid {uid}: {e}")))?;
            uid
        }
    };
    let run_as = ensure_passwd(uid).map_err(fail)?;
    Ok((uid, run_as))
}

/// Ensure a host passwd entry exists for a dedicated 50000+ uid (jail(8)
/// resolves exec.jail_user via it, and portcullisd refuses a peer whose uid
/// isn't in passwd). Returns the username. Used for apps and session components.
pub fn ensure_passwd(uid: u32) -> Result<String, String> {
    match portcullis_peer::username(uid) {
        Some(name) => Ok(name),
        None => {
            let name = portcullis_peer::app_username(uid);
            run("pw", &["useradd", &name, "-u", &uid.to_string(),
                        "-d", "/nonexistent", "-s", "/usr/sbin/nologin"])
                .map_err(|e| format!("create passwd {name} (uid {uid}): {e}"))?;
            Ok(name)
        }
    }
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

pub fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
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

pub fn umount_silent(p: &Path) -> std::io::Result<()> {
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
pub fn catalog(user: Option<&str>) -> Vec<portcullis_ipc::CatalogEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    /* Each app's uid for this person, so the launcher can match windows
     * (their owner uid) to apps (forum.md §6.1). Loaded once per catalog. */
    let registry = user.and_then(|_| portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).ok());
    let colours = user.map(portcullisd::persona_colours::read).unwrap_or_default();
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        // Dev builds are Run from their workspace, never from the grid.
        if portcullisd::manifest_trust::is_dev(&m.app.id) {
            continue;
        }
        out.push(portcullis_ipc::CatalogEntry {
            id:          m.app.id.clone(),
            name:        m.app.name.clone(),
            description: m.app.description.clone(),
            icon:        m.app.icon.clone(),
            uid:         registry.as_ref().zip(user).and_then(|(r, u)| r.uid_for(u, &m.app.id)),
            icon_svg:    m.app.icon.as_deref()
                .and_then(|i| portcullis_toml::icon::read_bundle_svg(&e.path(), i)),
            console:     no_console(&m).is_none(),
            instances:   registry.as_ref().zip(user).map(|(r, u)| r.named_instances(u, &m.app.id).into_iter()
                .map(|(uid, p)| portcullis_ipc::CatalogInstance { persona: p.to_string(), uid, colour: colours.get(p).cloned() })
                .collect()).unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// `app_id`'s installed manifest, if it reads and parses.
fn installed(app_id: &str) -> Option<Manifest> {
    if app_id.is_empty() || app_id.contains(['/', '\0']) || app_id.starts_with('.') {
        return None;
    }
    let text = fs::read_to_string(PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml")).ok()?;
    Manifest::from_str(&text).ok()
}

/// Is `app_id` a file manager (`removable-files` in its INSTALLED
/// manifest)? The only caller of OpenApps and OpenFile (opening-files.md
/// §6.2), as of dispensatord's CONNECT.
/// Why an app has no console (homes.md §4, decision H5), or `None` when
/// it may have one: the installer granted it (part of the system), or it
/// asks for a system-only capability.
pub fn no_console(m: &Manifest) -> Option<String> {
    let name = if m.app.name.is_empty() { &m.app.id } else { &m.app.name };
    let system = portcullis_policy::Policy::load_system(Path::new(portcullis_policy::SYSTEM_PATH))
        .map(|p| p.grants.contains_key(&m.app.id))
        .unwrap_or(true); // an unreadable system policy: assume the worst
    if system {
        return Some(format!("{name} is part of the system: it has no console"));
    }
    let caps = m.capabilities.system_only();
    if !caps.is_empty() {
        return Some(format!("{name} is part of the system ({}): it has no console", caps.join(", ")));
    }
    None
}

/// `app_id`'s installed manifest, for the console's checks.
pub fn installed_manifest(app_id: &str) -> Option<Manifest> {
    installed(app_id)
}

/// May this app open attachments (its INSTALLED manifest holds
/// `open-attachments`, mail.md §8.2)?
pub fn app_opens_attachments(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.open_attachments == Some(true))
}

/// Is this the Navigator (its INSTALLED manifest holds `navigator-broker`)?
/// The only caller of SaveDownload.
pub fn app_is_navigator(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.navigator_broker == Some(true))
}

/// Is this the terminal (its INSTALLED manifest holds `consoles`)?
pub fn app_shows_consoles(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.consoles == Some(true))
}

/// Is this the settings app (its INSTALLED manifest holds `app-grants`,
/// curia.md §13)? The only caller of Access, RemoveAccess, ApproveAccess.
pub fn app_manages_grants(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.app_grants == Some(true))
}

/// Every installed app's tree and manifest text, for Settings' Apps page:
/// trees whose manifest names another id are left out, as they can't be
/// launched by it.
pub fn installed_texts() -> Vec<(PathBuf, String, Manifest)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() == Some(m.app.id.as_str()) {
            out.push((e.path(), text, m));
        }
    }
    out
}

pub fn app_is_file_manager(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.removable_files == Some(true))
}

/// How `app_id` opens `mime` (its INSTALLED `[[opens]]`), and its name.
pub fn app_opens(app_id: &str, mime: &str) -> Option<(portcullis_toml::OpenMode, String)> {
    let m = installed(app_id)?;
    m.opens(mime).map(|mode| (mode, if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }))
}


/// An installed app's name (for `atrium-app:` links).
pub fn app_name(app_id: &str) -> Option<String> {
    let m = installed(app_id)?;
    Some(if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() })
}

/// Every installed app that takes links of `scheme`: (id, name), by name.
pub fn apps_taking_link(scheme: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() != Some(m.app.id.as_str()) || !m.takes_link(scheme) {
            continue;
        }
        // Web links are the Navigator's alone, whatever a tree says (D5).
        if atrium_links::WEB.contains(&scheme) && m.capabilities.navigator_broker != Some(true) {
            continue;
        }
        out.push((m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }));
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Every installed share-target `person` may share to (share-sheet.md
/// §4.3: dev builds only their own person's): (id, name), by name.
pub fn share_targets_for(person: &str) -> Vec<(String, String)> {
    let mine = format!("dev.{person}.");
    let mut out: Vec<(String, String)> = installed_texts().into_iter()
        .filter(|(_, _, m)| m.roles.as_ref().is_some_and(|r| r.implements.iter().any(|i| i == "share-target")))
        .filter(|(_, _, m)| !m.app.id.starts_with("dev.") || m.app.id.starts_with(&mine))
        .map(|(_, _, m)| (m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }))
        .collect();
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Every link scheme an installed app takes (not `atrium-app`), sorted.
pub fn link_schemes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() != Some(m.app.id.as_str()) {
            continue;
        }
        for s in m.links.iter().flat_map(|l| l.schemes.iter()) {
            if s != atrium_links::APP && atrium_links::declarable(s).is_ok() && !out.contains(s) {
                out.push(s.clone());
            }
        }
    }
    out.retain(|s| !apps_taking_link(s).is_empty());
    out.sort();
    out
}

/// Every installed app that opens `mime`: (id, name, mode), by name.
pub fn apps_opening(mime: &str) -> Vec<(String, String, portcullis_toml::OpenMode)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        // The tree's name is the id the app is launched by.
        if e.file_name().to_str() != Some(m.app.id.as_str()) {
            continue;
        }
        if let Some(mode) = m.opens(mime) {
            out.push((m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }, mode));
        }
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Does `app_id`'s installed manifest hold the `app-launch` capability?
///
/// The gate for [`catalog`]: the caller is identified by peer uid through the
/// launch registry, so this reads the capability off the INSTALLED manifest, not
/// off anything the caller sent. An app that is not in the registry at all (not
/// Portcullis-launched) resolves to no app-id and is refused by the caller.
/// Is this an IDE (its INSTALLED manifest holds `workspace-exec`, W6)?
pub fn app_execs_in_workspaces(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.workspace_exec == Some(true))
}

/// Does this app's INSTALLED manifest hold `workspaces` (homes.md §5)?
pub fn app_manages_workspaces(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.workspaces == Some(true))
}

pub fn app_may_launch_apps(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.app_launch == Some(true)
}

/// Does this app's INSTALLED manifest hold `net-fetch` (docs/spec/nuntius.md
/// §2)? Read from the app tree, never from anything the caller sends.
pub fn app_may_fetch(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.net_fetch == Some(true)
}

/// Does this app's INSTALLED manifest hold `network-portal`
/// (atrium-net.md §8.1)? Read from the app tree, never from the caller.
/// The installed app that opens the captive portal's sign-in window: the
/// one whose manifest holds `network-portal` (the Navigator); the first by
/// id if more do.
pub fn portal_app() -> Option<String> {
    let mut v: Vec<String> = installed_texts().into_iter()
        .filter(|(_, _, m)| m.capabilities.network_portal == Some(true)).map(|(_, _, m)| m.app.id).collect();
    v.sort();
    v.into_iter().next()
}

pub fn app_may_open_portal(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.network_portal == Some(true)
}

/// Does this app's INSTALLED manifest hold `protected-media`
/// (docs/spec/hospes.md §3)? Read from the app tree, never from the caller.
pub fn app_may_play_protected(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.protected_media == Some(true)
}

/// The site list this app's INSTALLED manifest narrows its fetcher to
/// (`net-fetch-sites`, nuntius.md §4), in the fetcher's environment form;
/// `Ok(None)` is unrestricted. A list that does not parse is an error, so
/// the fetcher is refused rather than started unlimited.
pub fn app_fetch_sites(app_id: &str) -> Result<Option<String>, String> {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m = Manifest::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    fetch_sites_of(m.capabilities.net_fetch_sites.as_deref())
}

/// `app_fetch_sites` without the file.
pub fn fetch_sites_of(list: Option<&[String]>) -> Result<Option<String>, String> {
    list.map(|l| nuntius::Sites::parse(l).map(|s| s.to_env_value())).transpose()
}

/// The jid of a running jail for `app_id`, or `None`.
///
/// Asks jls(8) rather than tracking state in-process: portcullisd can be
/// restarted while jails keep running, so the kernel's view is the only one
/// that is always right.
pub fn running_jid(app_id: &str) -> Option<i32> {
    let name = jail_name_from_app_id(app_id);
    let out = Command::new("jls").args(["-j", &name, "jid"]).output().ok()?;
    if !out.status.success() {
        return None;   /* jls exits non-zero when the jail does not exist */
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The key of an instance: `<id>` for Personal, `<id>@<persona>` for a named
/// persona. Names the jail root and seeds the synthetic host identity.
pub fn instance_key(app_id: &str, persona: Option<&str>) -> String {
    match persona {
        None => app_id.to_string(),
        Some(p) => format!("{app_id}@{p}"),
    }
}

/// As [`running_jid`], for the instance in `persona`.
pub fn running_jid_instance(app_id: &str, persona: Option<&str>) -> Option<i32> {
    let name = portcullis_jail::jail_name_for_instance(app_id, persona.map(|p| format!("p-{p}")).as_deref());
    let out = Command::new("jls").args(["-j", &name, "jid"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The apps of `user`'s persona `persona` with a process running now (any
/// of the persona's uids), by app id.
pub fn running_in_persona(user: &str, persona: &str) -> Vec<String> {
    let reg = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).unwrap_or_default();
    let mut v: Vec<String> = reg.persona_uids(user, persona).into_iter()
        .filter(|(uid, _)| Command::new("pgrep").args(["-U", &uid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false))
        .map(|(_, app)| app.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Every app uid of `user` (each persona's instances and workers; not
/// session components, which ostiarius tears down itself).
pub fn user_uids(user: &str) -> Vec<u32> {
    portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).unwrap_or_default().user_uids(user)
}

/// End the processes of `uids`: TERM, up to 3 s for them to go, then KILL.
/// Each app's launch then tears its jail down as usual.
pub fn end_processes(uids: &[u32]) {
    let alive = |u: u32| Command::new("pgrep").args(["-U", &u.to_string()]).output().map(|o| o.status.success()).unwrap_or(false);
    let live: Vec<u32> = uids.iter().copied().filter(|u| alive(*u)).collect();
    if live.is_empty() {
        return;
    }
    for u in &live {
        let _ = Command::new("pkill").args(["-TERM", "-U", &u.to_string()]).status();
    }
    for _ in 0..30 {
        if !live.iter().any(|u| alive(*u)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for u in &live {
        let _ = Command::new("pkill").args(["-KILL", "-U", &u.to_string()]).status();
    }
}

/// The uid of `user`'s instance of `app_id` in `persona`, if one was ever
/// registered (any publisher).
pub fn instance_uid(user: &str, app_id: &str, persona: Option<&str>) -> Option<u32> {
    portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).ok()?
        .uid_for_persona(user, app_id, persona)
}

/// Why `app_id` has no per-persona instances, if it has none: system apps
/// that span personas (personas.md §3: the file manager, the settings app,
/// apps that reach every workspace) run once and show both sides.
pub fn not_instanced(app_id: &str) -> Option<String> {
    match installed(app_id) {
        Some(m) => not_instanced_manifest(&m),
        None => None, // launching reports the missing app
    }
}

fn not_instanced_manifest(m: &Manifest) -> Option<String> {
    let c = &m.capabilities;
    let spans = [
        (c.home_files == Some(true), "home-files"),
        (c.removable_files == Some(true), "removable-files"),
        (c.app_grants == Some(true), "app-grants"),
        (c.workspaces == Some(true), "workspaces"),
        (c.workspace_exec == Some(true), "workspace-exec"),
    ];
    spans.iter().find(|(on, _)| *on).map(|(_, cap)| format!("it holds {cap}"))
}

/// Does this app's manifest say only one instance may run?
///
/// Absent `[supervision] instances`, treat it as single: that is what every
/// current manifest declares, and quietly allowing a second instance of an app
/// that did not ask for it is the worse default.
pub fn is_single_instance(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return true };
    let Ok(m) = Manifest::from_str(&text) else { return true };
    !matches!(
        m.supervision.as_ref().and_then(|s| s.instances),
        Some(portcullis_toml::InstancesPolicy::Multi)
    )
}

#[cfg(test)]
mod fetch_sites_tests {
    use super::fetch_sites_of;

    #[test]
    fn a_site_list_is_checked_before_the_fetcher_starts() {
        assert_eq!(fetch_sites_of(None), Ok(None));
        let l = vec!["Example.com".to_string(), "*.cdn.example.net".into()];
        assert_eq!(fetch_sites_of(Some(&l)), Ok(Some("example.com,*.cdn.example.net".into())));
        assert_eq!(fetch_sites_of(Some(&[])), Ok(Some(String::new())), "an empty list is no sites, not unlimited");
        assert!(fetch_sites_of(Some(&["10.0.0.1".to_string()])).is_err());
        assert!(fetch_sites_of(Some(&["a.com,b.com".to_string()])).is_err());
    }
}

/// Names of console jails start with this (dev-tools-plan.md M0, P1).
pub const CONSOLE_JAIL_PREFIX: &str = "con-";

/// Remove the console jails whose root is `path` (the jail beside an app's,
/// dev-tools-plan.md M0): their shells end, and the root's mounts can go.
/// Only console jails: an app's own jail has a network to release first.
pub fn remove_jails_at(path: &Path) {
    let Ok(o) = Command::new("jls").args(["-N", "name", "path"]).stderr(Stdio::null()).output() else { return };
    let want = path.to_string_lossy();
    let mut removed = Vec::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let mut f = line.split_whitespace();
        if let (Some(name), Some(p)) = (f.next(), f.next()) {
            if p == want && name.starts_with(CONSOLE_JAIL_PREFIX) {
                let _ = Command::new("jail").arg("-q").arg("-r").arg(name).stderr(Stdio::null()).status();
                removed.push(name.to_string());
            }
        }
    }
    // ★ A dying console jail pins the root as the workspace's own would
    // (portcullis_mounts::wait_jail_gone): wait for each before the caller
    // unmounts, or the root survives the teardown and Delete refuses.
    for name in removed {
        portcullis_mounts::wait_jail_gone(&name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    }
}

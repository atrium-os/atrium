//! A one-shot jail wired to the caller's pipes.
//!
//! ★★ A CRATE, NOT A CLI SUBCOMMAND, because `portcullisd` must be able to
//! do exactly this and a daemon with its own copy would drift from the CLI's
//! the way the trust gate's three copies already had. `run()` is the whole
//! operation; the CLI parses arguments into it and the daemon calls it
//! directly after receiving the caller's descriptors.
//!
//! ★★ A JAIL PER UNIT OF WORK, not per application.
//!
//! Everything else in this CLI launches an *application*: one jail per app id,
//! a persistent overlay, single-instance, living as long as the person is
//! using it. A worker pool is the other shape. The Navigator's backend runs one
//! jailed document worker PER DOCUMENT (atrium-navigator-backend.md §2), talks
//! to it over stdin/stdout, and throws it away when the document closes.
//!
//! Three differences from `launch`, each deliberate:
//!
//!   - **Per-instance name and root.** `<id>__<tag>` and
//!     `/var/lib/atrium/jails/<id>__<tag>`. Without this, two workers collide
//!     on the jail name, and `jail -c` on an existing name does not fail — it
//!     RECONFIGURES the running jail, so two documents would quietly end up
//!     inside one.
//!   - **No persistent overlay.** A worker holds nothing worth keeping, so the
//!     writable upper layer is tmpfs, discarded with the jail. That also keeps
//!     the overlay quota/dedup machinery out of a lane that churns jails.
//!   - **Signatures are REQUIRED**, not merely default-checked. A lane that
//!     launches jails continuously turns "unsigned is allowed when trust is
//!     unconfigured" from a one-off into a standing condition, so this path
//!     asks for `Demand::Required` whatever the machine is set to.
//!
//! Stdio needs no mechanism: `jail -c -f` inherits this process's descriptors,
//! and when the caller is a broker those descriptors are pipes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use portcullis_jail::{build, BuildOpts};

/// What to run, and as which instance.
pub struct Spec {
    /// App id, or a path to an app tree.
    pub target: String,
    /// Distinct per concurrent run; `None` is single-instance, like `launch`.
    pub instance: Option<String>,
    /// Size of the discarded writable layer.
    pub tmpfs_mb: u32,
    /// ★★★ AN OPT-IN STATIC MEMORY CAP — AND `None` IS THE RIGHT DEFAULT.
    ///
    /// `memoryuse` is RSS, and RSS can only be enforced by KILLING: you
    /// cannot cleanly fail a page fault, so rctl offers `sigkill`/`sigterm`
    /// for it and `deny` only for virtual/swap (atrium-memory-pressure.md
    /// §"the per-jail HARD CAP"). There is no soft version of this knob.
    ///
    /// ★ AND ATRIUM ALREADY HAS THE ADAPTIVE ANSWER. `memfed` water-fills RAM
    /// across jails by weight and pushes each one's `memoryuse` cap
    /// dynamically, **never below current RSS**, so an over-budget jail is
    /// frozen rather than killed — and it acts through the jaild broker, not
    /// by shelling rctl wherever it happens to be convenient. A static number
    /// set here does not merely duplicate that: it FIGHTS it, because a cap
    /// this lane pins can kill a worker memfed would have frozen.
    ///
    /// So the steady-state answer for a worker jail is the federation, not a
    /// constant. This stays as a deliberate safety net for a deployment whose
    /// jails the federation cannot see — see the gap in portcullis.md §6.5.2e
    /// — and it is off unless someone asks for it.
    pub memory_mb: Option<u64>,
    /// ★ Refuse to run rather than run UNCAPPED when the kernel cannot
    /// enforce a limit. Off by default so an existing machine keeps working —
    /// the same shape as `require_signatures` — because a deployment that
    /// cares must be able to demand it, and one that does not must not be
    /// broken by a release.
    pub require_memory_limit: bool,
    /// The user the entry runs as. ★ Its home is RESOLVED from the host's
    /// passwd, not supplied: `exec.system_jail_user` makes jail(8) chdir into
    /// that user's passwd home inside the jail, so any other answer creates
    /// the wrong directory and the entry dies before it runs. The CLI used to
    /// pass `$HOME` and the daemon guessed `/home/<user>`; the first was right
    /// only because root's `$HOME` happens to match, and the second was simply
    /// wrong — measured, as `chdir /root: No such file or directory`.
    pub user_name: String,
}

/// Whether the kernel can enforce an rctl rule at all.
///
/// ★ `kern.racct.enable` is a LOADER TUNABLE, not a runtime switch: a machine
/// that did not boot with it cannot be given resource limits without a
/// reboot. So this is a fact to report, never something to "turn on" — and a
/// caller that silently proceeded would be running a worker pool it believes
/// is capped and is not.
pub fn racct_enabled() -> bool {
    std::process::Command::new("sysctl").arg("-n").arg("kern.racct.enable")
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

/// Apply a memoryuse cap to a jail. Returns whether one is now in force.
///
/// ★ `sigkill`, matching jaild's own rule (`jaild/src/ffi.rs`): a worker that
/// exceeds its cap is killed, not merely denied an allocation. A denied
/// allocation inside a parser is an error path that hostile input chose, and
/// the broker already survives a worker dying — it is the failure mode the
/// whole design is built around.
/// ★ NOTE THE LAYERING VIOLATION THIS ACCEPTS. Atrium's privsep design has
/// jaild as the single actor for rctl — a jailed governor cannot rctl another
/// jail, so `SetRctl` is brokered. This lane creates its jails with `jail -c`
/// directly, and jaild refuses to set a rule on a jail it did not create
/// ("rctl.unknown_jail"), so brokering is not available here yet. It becomes
/// available when the lane moves to jaild's `CreateJail` (portcullis.md
/// §6.5.4), which is the same change that removes the intervening `/bin/sh`.
/// Recorded rather than quietly shelled out.
fn apply_memory_cap(jail_name: &str, mb: u64) -> Result<(), String> {
    let rule = format!("jail:{jail_name}:memoryuse:sigkill={mb}M");
    let out = std::process::Command::new("rctl").arg("-a").arg(&rule)
        .output().map_err(|e| format!("rctl: {e}"))?;
    if out.status.success() { return Ok(()) }
    Err(format!("rctl -a {rule}: {}", String::from_utf8_lossy(&out.stderr).trim()))
}

/// The passwd home of `user`, which is where jail(8) will chdir.
fn passwd_home(user: &str) -> String {
    let Ok(cuser) = std::ffi::CString::new(user) else { return "/".into() };
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe {
        libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result)
    };
    if r != 0 || result.is_null() { return "/".into() }
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    dir.to_string_lossy().into_owned()
}

/// Why a one-shot run did not happen. `Exit` is the jailed process's own
/// outcome and is not a failure of this crate.
#[derive(Debug)]
pub enum OneShot {
    /// The jailed entry ran; `ok` is whether it succeeded. ★ Not a code:
    /// jail(8) collapses every nonzero exec.start status to 1.
    Exit { ok: bool },
    Refused(String),
    Failed(String),
}

const APPS_DIR: &str = "/var/lib/atrium/apps";
const JAILS_DIR: &str = "/var/lib/atrium/jails";

/// Run `spec` in a one-shot jail whose stdio is this process's own.
///
/// ★ Stdio needs no mechanism here: the jail's entry inherits this process's
/// descriptors. When the caller spawned us with pipes — a broker, or the
/// daemon holding a client's fds received over SCM_RIGHTS — the jailed
/// process reads and writes them directly.
pub fn run(spec: &Spec) -> OneShot { run_with_stdio(spec, None) }

/// As `run`, but give the jailed entry these descriptors instead of this
/// process's.
///
/// ★ THE DAEMON NEEDS THIS AND THE CLI DOES NOT. When `portcullisd` creates
/// the jail, its own stdio is the daemon log — the client's descriptors
/// arrived over SCM_RIGHTS and are what the jailed process must actually
/// speak on. Inheriting would send a worker's protocol frames to the system
/// log and leave the broker waiting forever.
pub fn run_with_stdio(spec: &Spec, stdio: Option<[std::os::fd::OwnedFd; 3]>) -> OneShot {
    let target = spec.target.as_str();
    let instance = spec.instance.clone();
    let tmpfs_mb = spec.tmpfs_mb;
    let tree = resolve_tree(target);
    let manifest_path = tree.join("atrium.toml");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => return OneShot::Failed(format!("read {}: {e}", manifest_path.display())),
    };

    // ★ Demand::Required — see the module header. This is the one caller in
    // the CLI that asks for more than the machine's configured policy.
    if let Err(e) = portcullis_trust::Trust::load()
        .verify(&tree, &text, portcullis_trust::Demand::Required)
    {
        return OneShot::Refused(e);
    }

    let manifest = match portcullis_toml::Manifest::from_str(&text) {
        Ok(m) => m,
        Err(e) => return OneShot::Failed(format!("{}: {e:?}", manifest_path.display())),
    };

    let jail_name = portcullis_jail::jail_name_for_instance(
        &manifest.app.id, instance.as_deref());
    let jail_path = PathBuf::from(JAILS_DIR).join(&jail_name);

    // ★ jail(8) CHDIRS INTO THE RUN USER'S HOME inside the jail, taken from
    // the HOST's passwd because exec.system_jail_user is set. A worker needs
    // no home, but it gets one anyway: without the directory existing in the
    // jail's namespace, exec.start fails with `chdir: No such file or
    // directory` before the entry ever runs. Created empty in the tmpfs upper
    // layer, so it costs nothing and vanishes with the jail.
    let user_home = passwd_home(&spec.user_name);

    let opts = BuildOpts {
        root_path: jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from(&user_home),
        user_name: spec.user_name.clone(),
        devfs_ruleset: 99,
        instance: instance.clone(),
    };
    let jc = match build(&manifest, &opts) {
        Ok(jc) => jc,
        Err(e) => return OneShot::Failed(format!("build: {e}")),
    };

    // ★★ A LIVE JAIL WITH THIS NAME IS A REFUSAL, NOT SOMETHING TO CLEAN UP.
    //
    // The pre-teardown below exists for the leftovers of a run that DIED —
    // killed between its mount and its cleanup — because stacking a fresh
    // nullfs on an abandoned pile is how a mount stack becomes unrecoverable
    // without a reboot. It cannot tell that from a jail that is still
    // running, and measured against a live one it killed it: two runs with
    // the same tag ended with the first reader's process taking SIGTERM and
    // the second failing anyway. Both lost, for a reason neither could act on.
    //
    // So liveness is checked first, and a duplicate tag is refused with the
    // name a caller needs to fix it. Distinctness is the caller's to own —
    // this just stops it being silently violated.
    if let Some(jid) = running_jid(&jail_name) {
        // ★★ AN EMPTY JAIL IS A HUSK, NOT AN INSTANCE. `persist = true` keeps
        // a jail object alive after its processes are gone, so a launcher
        // that was SIGKILLed — which is exactly what a broker does to a
        // worker it is retiring — leaves a named, process-less jail and its
        // mounts behind. Refusing on that would make one killed worker
        // poison its instance tag until a human noticed; measured, it did.
        //
        // So: processes inside decide. Some means a live instance and a
        // genuine duplicate; none means wreckage this run should clear.
        if jailed_process_count(jid) > 0 {
            return OneShot::Refused(format!(
                "jail {jail_name} is already running (jid {jid}); \
                 instance tags must be distinct while they overlap"));
        }
        eprintln!("portcullis: reclaiming abandoned jail {jail_name} (jid {jid}, no processes)");
    }

    // Now safe: anything left under this name belongs to a run that is gone.
    teardown(&jail_path, &jail_name);

    if let Err(e) = std::fs::create_dir_all(&jail_path) {
        return OneShot::Failed(format!("mkdir {}: {e}", jail_path.display()));
    }
    if let Err(e) = mount_layers(&tree, &jail_path, &jail_name, tmpfs_mb) {
        teardown(&jail_path, &jail_name);
        return OneShot::Failed(e.to_string());
    }
    // ★ Mountpoints jail(8) will not create. Shared with the application
    // path: the two had grown separate copies of the same three lines, and
    // the lane that lacked it supported capability-bearing workers only in
    // principle.
    if let Err(e) = portcullis_mounts::ensure_mountpoints(&jc) {
        teardown(&jail_path, &jail_name);
        return OneShot::Failed(e);
    }

    for dir in ["dev", user_home.trim_start_matches('/')] {
        if dir.is_empty() { continue }
        if let Err(e) = std::fs::create_dir_all(jail_path.join(dir)) {
            teardown(&jail_path, &jail_name);
            return OneShot::Failed(format!("mkdir {dir}: {e}"));
        }
    }

    let conf_path = std::env::temp_dir()
        .join(format!("portcullis-exec-{}-{}.conf", std::process::id(), jail_name));
    if let Err(e) = std::fs::File::create(&conf_path)
        .and_then(|mut f| f.write_all(jc.render_jail_conf().as_bytes()))
    {
        teardown(&jail_path, &jail_name);
        return OneShot::Failed(format!("write {}: {e}", conf_path.display()));
    }

    // stdio is inherited: no .stdin()/.stdout() calls, deliberately. When the
    // caller spawned us with pipes, the jailed process reads and writes them.
    //
    // ★★ `-q` IS LOAD-BEARING, NOT COSMETIC. jail(8) prints "<name>: created"
    // and "<name>: removed" on STDOUT, and stdout here is the caller's pipe —
    // the same one the jailed process speaks its protocol on. Measured: a
    // broker reading length-prefixed frames got
    // `malformed frame: bad length in "org_atrium_navigator_worker__1: created"`
    // and the session never opened. Anything this command emits on stdout is
    // indistinguishable from the payload; everything it has to say goes to
    // stderr instead.
    //
    // ★ The status below is jail(8)'s, not the entry's. Measured: a child
    // exiting 7 makes jail(8) exit 1. Success and failure survive; the code
    // does not, and the usage text says so rather than implying a fidelity
    // this path cannot provide.
    // ★ The cap goes on AFTER the jail exists and BEFORE anything runs in it:
    // an rctl rule names a jail, so there is nothing to name until `jail -c`
    // has created it — but `jail -c` also runs the entry. The jail is created
    // by the same command that starts the entry, so the rule is applied to
    // the NAME, which rctl accepts ahead of the jail existing. Applying it
    // here means a worker is capped from its first instruction.
    if let Some(mb) = spec.memory_mb {
        if racct_enabled() {
            if let Err(e) = apply_memory_cap(&jail_name, mb) {
                teardown(&jail_path, &jail_name);
                return OneShot::Failed(format!("memory cap: {e}"));
            }
        } else if spec.require_memory_limit {
            teardown(&jail_path, &jail_name);
            return OneShot::Refused(
                "a memory limit was required but kern.racct.enable is 0; \
                 RACCT is a loader tunable, so this machine cannot enforce one \
                 until it reboots with kern.racct.enable=1".into());
        }
    }

    let mut cmd = Command::new("jail");
    cmd.arg("-q").arg("-c").arg("-f").arg(&conf_path).arg(&jail_name);
    if let Some([sin, sout, serr]) = stdio {
        cmd.stdin(std::process::Stdio::from(sin));
        cmd.stdout(std::process::Stdio::from(sout));
        cmd.stderr(std::process::Stdio::from(serr));
    }
    let status = cmd.status();

    let outcome = match status {
        Ok(s) => OneShot::Exit { ok: s.success() },
        Err(e) => OneShot::Failed(format!("jail: {e}")),
    };

    let _ = std::fs::remove_file(&conf_path);
    teardown(&jail_path, &jail_name);
    let _ = std::fs::remove_dir(&jail_path);
    outcome
}

/// The jid of a live jail with this name, if any. `jls` is the kernel's own
/// answer; tracking liveness in a file would be a second source of truth that
/// a killed process could leave wrong.
fn running_jid(name: &str) -> Option<u32> {
    let out = Command::new("jls").arg("-j").arg(name).arg("jid").output().ok()?;
    if !out.status.success() { return None }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// How many processes are inside a jail. Zero means the jail object outlived
/// whatever it was created for.
fn jailed_process_count(jid: u32) -> usize {
    match Command::new("ps").arg("-J").arg(jid.to_string()).arg("-o").arg("pid=").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines()
            .filter(|l| !l.trim().is_empty()).count(),
        // ★ Unknown reads as OCCUPIED. If the process table cannot be read,
        // the safe assumption is that something is in there — tearing down a
        // jail that might hold a live reader is the worse mistake.
        Err(_) => 1,
    }
}

fn resolve_tree(target: &str) -> PathBuf {
    if target.contains('/') || target.starts_with('.') {
        PathBuf::from(target)
    } else {
        PathBuf::from(APPS_DIR).join(target)
    }
}

/// Where a one-shot jail's writable layer lives while it runs.
fn upper_dir(jail_name: &str) -> PathBuf {
    PathBuf::from("/var/run/portcullis-exec").join(jail_name)
}

/// Read-only app tree, with a tmpfs upper layer UNIONED over it.
///
/// ★★ THE UNION IS THE POINT, AND THE FIRST VERSION GOT IT WRONG. Mounting
/// tmpfs directly onto the jail root does not layer over the tree — it MASKS
/// it, so the jail booted with an empty root and `exec /bin/sh` failed with
/// "No such file or directory" while every mount had succeeded. A stacking
/// filesystem and a second mount at the same point look identical in
/// `mount -p`; only running it tells you which one you built.
///
/// ★ The upper layer is tmpfs rather than a persistent overlay because a
/// worker has no state worth keeping between documents — and because a lane
/// that creates and destroys jails continuously must not accumulate on-disk
/// overlays that something else then has to garbage-collect. It is mounted
/// OUTSIDE the jail root so the union has two distinct directories to join.
fn mount_layers(tree: &Path, jail_path: &Path, jail_name: &str, tmpfs_mb: u32)
    -> std::io::Result<()>
{
    let upper = upper_dir(jail_name);
    std::fs::create_dir_all(&upper)?;
    sh("mount", &["-t", "nullfs", "-o", "ro",
                   &tree.to_string_lossy(), &jail_path.to_string_lossy()])?;
    let size = format!("size={tmpfs_mb}m");
    sh("mount", &["-t", "tmpfs", "-o", &size, "tmpfs", &upper.to_string_lossy()])?;
    sh("mount", &["-t", "unionfs", &upper.to_string_lossy(),
                   &jail_path.to_string_lossy()])
}

/// Run a host command, failing loudly. Named `sh` rather than `run` since
/// `run` is this crate's own operation.
fn sh(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

/// Stop the jail and unwind its mounts.
///
/// ★ TWO ROOTS. The writable layer is mounted OUTSIDE the jail root, so a
/// teardown that swept only the root left one tmpfs per run alive in
/// /var/run — invisible to anyone looking at the jail, and accumulating
/// exactly as fast as the pool churns.
///
/// ★ Force, unlike the application path: by this point the jail is gone and
/// nothing should hold these mounts, so forcing costs nothing and guarantees
/// the next run does not stack on a survivor.
fn teardown(jail_path: &Path, jail_name: &str) {
    let _ = Command::new("jail").arg("-r").arg(jail_name)
        .stderr(std::process::Stdio::null()).status();
    let upper = upper_dir(jail_name);
    // ★ Remove the rctl rule with the jail. Rules are keyed by NAME, and the
    // name is reused by the next worker with that instance tag: a rule left
    // behind would silently apply someone else's cap to it, and rules
    // accumulate in the kernel with nothing to show for them.
    let _ = Command::new("rctl").arg("-r").arg(format!("jail:{jail_name}:memoryuse:"))
        .stderr(std::process::Stdio::null()).status();
    let left = portcullis_mounts::converge(
        &[jail_path, upper.as_path()], portcullis_mounts::Force::Yes);
    portcullis_mounts::warn_survivors("portcullis", &left);
    let _ = std::fs::remove_dir(&upper);
}


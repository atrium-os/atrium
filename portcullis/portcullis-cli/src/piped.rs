//! `portcullis exec` — a one-shot jail wired to the caller's pipes.
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
use std::process::{Command, ExitCode};

use portcullis_jail::{build, BuildOpts};

const APPS_DIR: &str = "/var/lib/atrium/apps";
const JAILS_DIR: &str = "/var/lib/atrium/jails";

pub fn usage() -> ! {
    eprintln!("\
usage:
    portcullis exec [--instance <tag>] [--tmpfs-size <n>] <app-id|app-tree>

        Run the app's entry in a ONE-SHOT jail whose stdin/stdout/stderr are
        this process's own — so a parent that spawned portcullis with pipes
        talks to the jailed process directly.

        Each --instance gets its own jail name and root, so many may run
        concurrently from one app. The writable layer is tmpfs and is
        discarded on exit; nothing persists between runs.

        Signatures are REQUIRED on this path regardless of
        /etc/atrium/trust.toml, because a worker pool launches continuously.

        Exits 0 if the jailed process succeeded, 1 if it did not.
        NOT the child's own code: jail(8) collapses every nonzero
        exec.start status to 1, so success and failure are
        distinguishable here and the exact code is not.");
    std::process::exit(2)
}

pub fn cmd_exec(args: &[String]) -> ExitCode {
    let (mut instance, mut tmpfs_mb, mut target) = (None::<String>, 64u32, None::<&str>);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--instance" => { i += 1; instance = args.get(i).cloned(); }
            "--tmpfs-size" => {
                i += 1;
                tmpfs_mb = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(n) => n, None => usage(),
                };
            }
            s if s.starts_with("--") => usage(),
            s => target = Some(Box::leak(s.to_string().into_boxed_str())),
        }
        i += 1;
    }
    let Some(target) = target else { usage() };

    // ★ An instance tag is REQUIRED to be distinct, and the caller owns
    // distinctness. A default of "the app id" would put every worker back in
    // one jail, which is the bug this command exists to avoid — so there is
    // no default: absent means single-instance, exactly like `launch`.
    let tree = resolve_tree(target);
    let manifest_path = tree.join("atrium.toml");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("portcullis: read {}: {e}", manifest_path.display());
                    return ExitCode::from(2) }
    };

    // ★ Demand::Required — see the module header. This is the one caller in
    // the CLI that asks for more than the machine's configured policy.
    if let Err(e) = portcullis_trust::Trust::load()
        .verify(&tree, &text, portcullis_trust::Demand::Required)
    {
        eprintln!("portcullis: REFUSED — {e}");
        return ExitCode::from(1);
    }

    let manifest = match portcullis_toml::Manifest::from_str(&text) {
        Ok(m) => m,
        Err(e) => { eprintln!("portcullis: {}: {e}", manifest_path.display());
                    return ExitCode::from(2) }
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
    let user_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());

    let opts = BuildOpts {
        root_path: jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from(&user_home),
        user_name: std::env::var("USER").unwrap_or_else(|_| "root".into()),
        devfs_ruleset: 99,
        instance: instance.clone(),
    };
    let jc = match build(&manifest, &opts) {
        Ok(jc) => jc,
        Err(e) => { eprintln!("portcullis: build: {e}"); return ExitCode::from(1) }
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
            eprintln!("portcullis: REFUSED — jail {jail_name} is already running (jid {jid}); \
                       instance tags must be distinct while they overlap");
            return ExitCode::from(1);
        }
        eprintln!("portcullis: reclaiming abandoned jail {jail_name} (jid {jid}, no processes)");
    }

    // Now safe: anything left under this name belongs to a run that is gone.
    teardown(&jail_path, &jail_name);

    if let Err(e) = std::fs::create_dir_all(&jail_path) {
        eprintln!("portcullis: mkdir {}: {e}", jail_path.display());
        return ExitCode::from(1);
    }
    if let Err(e) = mount_layers(&tree, &jail_path, &jail_name, tmpfs_mb) {
        eprintln!("portcullis: {e}");
        teardown(&jail_path, &jail_name);
        return ExitCode::from(1);
    }
    for dir in ["dev", user_home.trim_start_matches('/')] {
        if dir.is_empty() { continue }
        if let Err(e) = std::fs::create_dir_all(jail_path.join(dir)) {
            eprintln!("portcullis: mkdir {dir}: {e}");
            teardown(&jail_path, &jail_name);
            return ExitCode::from(1);
        }
    }

    let conf_path = std::env::temp_dir()
        .join(format!("portcullis-exec-{}-{}.conf", std::process::id(), jail_name));
    if let Err(e) = std::fs::File::create(&conf_path)
        .and_then(|mut f| f.write_all(jc.render_jail_conf().as_bytes()))
    {
        eprintln!("portcullis: write {}: {e}", conf_path.display());
        teardown(&jail_path, &jail_name);
        return ExitCode::from(1);
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
    let status = Command::new("jail")
        .arg("-q").arg("-c").arg("-f").arg(&conf_path).arg(&jail_name)
        .status();

    let code = match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("portcullis: jail: {e}"); 1 }
    };

    let _ = std::fs::remove_file(&conf_path);
    teardown(&jail_path, &jail_name);
    let _ = std::fs::remove_dir(&jail_path);
    ExitCode::from(code as u8)
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
    run("mount", &["-t", "nullfs", "-o", "ro",
                   &tree.to_string_lossy(), &jail_path.to_string_lossy()])?;
    let size = format!("size={tmpfs_mb}m");
    run("mount", &["-t", "tmpfs", "-o", &size, "tmpfs", &upper.to_string_lossy()])?;
    run("mount", &["-t", "unionfs", &upper.to_string_lossy(),
                   &jail_path.to_string_lossy()])
}

fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

/// Every mount at or under `root`, deepest first — the only safe unmount order
/// for a stack.
fn mounts_under(root: &Path) -> Vec<PathBuf> {
    let out = match Command::new("mount").arg("-p").output() {
        Ok(o) => o.stdout,
        Err(_) => return Vec::new(),
    };
    let root = root.to_string_lossy().to_string();
    let mut v: Vec<PathBuf> = String::from_utf8_lossy(&out).lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .filter(|p| *p == root || p.starts_with(&format!("{root}/")))
        .map(PathBuf::from)
        .collect();
    v.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    v
}

/// ★ CONVERGENT, AND IT SAYS SO WHEN IT FAILS. A worker pool churns jails far
/// faster than app launches do, so a leaked mount does not get noticed once —
/// it compounds. Repeating until the set is empty is the only way to unwind a
/// stack whose members can be busy in any order; giving up silently would
/// leave a root that the NEXT run mounts on top of.
fn teardown(jail_path: &Path, jail_name: &str) {
    let _ = Command::new("jail").arg("-r").arg(jail_name)
        .stderr(std::process::Stdio::null()).status();
    let upper = upper_dir(jail_name);
    // ★ BOTH TREES. The writable layer is mounted outside the jail root, so a
    // teardown that swept only under the root would leave one tmpfs per run
    // alive in /var/run — invisible to anyone looking at the jail, and
    // accumulating exactly as fast as the pool churns.
    for _ in 0..16 {
        let mut mounts = mounts_under(jail_path);
        mounts.extend(mounts_under(&upper));
        if mounts.is_empty() { break }
        for m in mounts {
            let _ = Command::new("umount").arg("-f").arg(&m)
                .stderr(std::process::Stdio::null()).status();
        }
    }
    let mut left = mounts_under(jail_path);
    left.extend(mounts_under(&upper));
    if !left.is_empty() {
        eprintln!("portcullis: WARNING {} mounts survive under {} / {}: {:?}",
                  left.len(), jail_path.display(), upper.display(), left);
    }
    let _ = std::fs::remove_dir(&upper);
}

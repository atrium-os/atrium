//! Mount mechanics shared by every jail lane.
//!
//! ★★ THERE WERE THREE COPIES, AT THREE DIFFERENT QUALITIES — which is worse
//! than three identical ones, because the weakest was on the path nobody was
//! looking at:
//!
//!   - `portcullisd`'s application launch had the good one: re-enumerate each
//!     pass, stop when a pass makes no progress, and WARN loudly about
//!     survivors. It got that way by being debugged, after capability mounts
//!     under the jail path held the overlay busy and every relaunch stacked a
//!     fresh set on top of the survivors — 12 mounts with no jail, no
//!     process, no open file.
//!   - The one-shot lane had a near-copy: convergent, but it spun all sixteen
//!     passes instead of noticing it had stopped making progress, and it knew
//!     about a second root the other did not.
//!   - The CLI's local fallback still had the ORIGINAL: unmount `dev`, then
//!     the jail path twice and hope. That is precisely the version the first
//!     one was fixed away from, left behind on a path that is only reached
//!     when the daemon is down — i.e. when nobody is watching.
//!
//! This is their UNION, not their intersection. Every behaviour any of them
//! had is here, and the differences that were real — force-unmounting,
//! multiple roots — became parameters rather than being averaged away.
//!
//! ★ A STACKED PILE CANNOT BE CLEANED UP AFTERWARDS BY PATH: a path resolves
//! through the topmost layer, so buried mounts answer "not a file system root
//! directory" and are reachable only by fsid. The fix has to be never to
//! leave the pile, which is why teardown converges and why it complains when
//! it cannot.

use std::path::{Path, PathBuf};
use std::process::Command;

pub use portcullis_jail::JailConfig;

/// Whether to force-unmount.
///
/// ★ A REAL DIFFERENCE BETWEEN THE LANES, kept as a choice rather than
/// averaged. A one-shot worker's jail is already gone by teardown time and
/// nothing should hold its mounts, so forcing costs nothing and guarantees
/// the next run does not stack. An application's mounts may be genuinely
/// busy, and forcing there can take a filesystem away from something still
/// using it — so that path asks politely and reports what survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force { No, Yes }

/// Parse `mount -p` output for the mount points at or under any of `roots`,
/// deepest first.
///
/// ★ Pure, and separated from running `mount(8)` precisely so it can be
/// tested on a host with none of these filesystems — the subtle part is the
/// prefix rule, not the subprocess.
pub fn parse_mounts(output: &str, roots: &[&Path]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = output
        .lines()
        // fstab-style: field 2 is the mount point.
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(PathBuf::from)
        // ★ `Path::starts_with` is COMPONENT-wise, so `/a/bc` is not under
        // `/a/b`. A string prefix test would have unmounted a sibling whose
        // name merely began with the root's.
        .filter(|p| roots.iter().any(|r| p == *r || p.starts_with(r)))
        .collect();
    // Deepest first: nested mounts ahead of the parents containing them.
    //
    // ★★ NOT deduplicated. A one-shot jail root is TWO mounts at ONE path —
    // the read-only tree and the union over it — and each entry is one layer
    // to pop (`umount <path>` always takes the topmost). Deduplicating made
    // `converge` count paths instead of layers, so a pass that DID remove the
    // union looked like no progress (2 paths before, 2 after) and it gave up
    // with the tree and the tmpfs still mounted. Measured 2026-09-22: every
    // early exit after the mounts leaked both; the normal exit path happened
    // to avoid it, so nothing had ever exercised it until the devfs refusal.
    v.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    v
}

/// Every mount point at or under any of `roots`, deepest first.
pub fn under(roots: &[&Path]) -> Vec<PathBuf> {
    let out = match Command::new("mount").arg("-p").output() {
        Ok(o) => o.stdout,
        // ★ Unreadable reads as EMPTY, and the caller's convergence loop then
        // exits reporting nothing survived — which is why `converge` returns
        // survivors for the caller to check rather than deciding success here.
        Err(_) => return Vec::new(),
    };
    parse_mounts(&String::from_utf8_lossy(&out), roots)
}

/// Unmount everything under `roots` until nothing is left or no progress is
/// made. Returns whatever survived.
///
/// Re-enumerating each pass is what makes it converge: unmounting a layer can
/// reveal another mounted at the same path underneath it. Stopping on no
/// progress is what keeps it from spinning against something genuinely held.
pub fn converge(roots: &[&Path], force: Force) -> Vec<PathBuf> {
    for _ in 0..16 {
        let ms = under(roots);
        if ms.is_empty() { return Vec::new() }
        let before = ms.len();
        for m in &ms {
            let mut c = Command::new("umount");
            if force == Force::Yes { c.arg("-f"); }
            let _ = c.arg(m).stderr(std::process::Stdio::null()).status();
        }
        if under(roots).len() == before { break }
    }
    under(roots)
}

/// Wait (bounded) until no jail named `name` exists, dying ones included.
///
/// ★★ A DYING JAIL PINS ITS ROOT. It holds a reference to the root vnode until
/// it is finally freed, so its mounts cannot be unmounted — and a teardown that
/// tried anyway left the pile STACKED: measured, a relaunch inside the window
/// left 4 mounts on one app root. A networked jail dies slowly (TCP TIME_WAIT
/// in its own stack held one for 2×MSL = 60 s), so every teardown waits here
/// between `jail -r` and unmounting. Returns false at the deadline; the caller
/// then reports survivors rather than guessing.
pub fn wait_jail_gone(name: &str, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        let exists = Command::new("jls").args(["-d", "-j", name, "jid"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false);
        if !exists { return true }
        if start.elapsed() >= timeout {
            eprintln!("portcullis: jail {name} is still dying after {}s — its mounts \
                       stay pinned until it is freed", timeout.as_secs());
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// How long a teardown waits for a dying jail. App stacks run with a 1 s MSL
/// (TIME_WAIT 2 s — network.md §0), so this is several times the expected
/// worst case, not a guess at it.
pub const JAIL_GONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Report survivors. ★ Gate on the mounts being GONE, not on having called
/// umount: a silent leak is invisible until the stack is unrecoverable.
pub fn warn_survivors(who: &str, survivors: &[PathBuf]) {
    if survivors.is_empty() { return }
    eprintln!("{who}: WARNING {} mount(s) survived teardown \
               — a relaunch will stack on top of them:", survivors.len());
    for m in survivors.iter().take(6) {
        eprintln!("{who}:   {}", m.display());
    }
}

/// Refuse a jail whose devfs would hide nothing. Call immediately before every
/// `jail -c`.
///
/// ★★★ A ruleset the kernel has not loaded does not fail the mount — the
/// kernel creates it EMPTY and the jail gets the host's entire /dev. Measured
/// 2026-09-22: every app and one-shot worker (ruleset 99, never defined) and
/// every session jail (100, likewise) saw raw disks, mem/kmem and bpf, and a
/// root process in one read the host's disk. So "has rules" is checked, not
/// "exists": once any jail has mounted an unloaded number, it is listed.
/// Same check as jaild's own (jaild/src/ffi.rs), for the jail(8) paths jaild
/// does not see.
pub fn ensure_devfs_isolation(jc: &JailConfig) -> Result<(), String> {
    use portcullis_jail::Value;
    let devfs = jc.params.iter().any(|(k, v)| k == "mount.devfs" && matches!(v, Value::Bool(true)));
    if !devfs { return Ok(()) }
    let id = jc.params.iter().find_map(|(k, v)| match (k.as_str(), v) {
        ("devfs_ruleset", Value::Number(n)) => Some(*n),
        _ => None,
    });
    // ★ mount.devfs with NO ruleset, or 0, is the host's full /dev — the same
    // exposure by another name, so it is refused too.
    let Some(id) = id.filter(|n| *n > 0) else {
        return Err(format!("jail {} mounts devfs with no ruleset: it would see the \
                            host's entire /dev", jc.name));
    };
    let out = Command::new("devfs").args(["rule", "-s", &id.to_string(), "show"]).output()
        .map_err(|e| format!("cannot verify devfs ruleset {id}: {e}"))?;
    if out.status.success() && !out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(());
    }
    Err(format!("devfs ruleset {id} has no rules loaded in the kernel; jail {} would \
                 see the host's entire /dev. Install etc/atrium.devfs.rules and \
                 `service devfs restart`", jc.name))
}

/// Refuse a jail whose `full` network was never attached. ★ Call before every
/// `jail -c`, beside [`ensure_devfs_isolation`]: build() no longer puts a
/// networked app on the host's stack, so an unattached config would run with
/// no network at all — or, if someone "fixed" it by hand, on the host's.
pub fn ensure_network_ready(jc: &JailConfig) -> Result<(), String> {
    if jc.needs_routed_net.is_some() && !jc.routed_net_attached {
        return Err(format!("jail {} needs its network from jaild (AllocateNet) \
                            before it can start", jc.name));
    }
    Ok(())
}

/// Create the destinations a jail's mounts need. ★ `jail(8)` does not create
/// mountpoints; without this a capability mount fails with "No such file or
/// directory" after everything else has succeeded.
///
/// Directory-or-file is decided by stat'ing the SOURCE: a nullfs mount of a
/// file onto a directory fails, and so does the reverse.
pub fn ensure_mountpoints(jc: &JailConfig) -> Result<(), String> {
    for m in &jc.mounts {
        if let Some(parent) = m.dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir mountpoint parent {}: {e}", parent.display()))?;
        }
        let src_is_dir = std::fs::metadata(&m.src).map(|md| md.is_dir()).unwrap_or(false);
        if src_is_dir {
            std::fs::create_dir_all(&m.dst)
                .map_err(|e| format!("mkdir mountpoint {}: {e}", m.dst.display()))?;
        } else if !m.dst.exists() {
            std::fs::File::create(&m.dst)
                .map_err(|e| format!("touch mountpoint {}: {e}", m.dst.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
/dev/gpt/atrium-root / ufs rw 1 1
/root/wapp /var/lib/atrium/jails/app ro,nullfs 0 0
tmpfs /var/run/portcullis-exec/app tmpfs rw 0 0
/var/run/portcullis-exec/app /var/lib/atrium/jails/app unionfs rw 0 0
devfs /var/lib/atrium/jails/app/dev devfs rw 0 0
/atrium/sockets/fresco /var/lib/atrium/jails/app/atrium/sockets/fresco nullfs rw 0 0
/root/other /var/lib/atrium/jails/app-sibling ro,nullfs 0 0
";

    fn p(s: &str) -> PathBuf { PathBuf::from(s) }

    #[test]
    fn finds_every_mount_under_the_root() {
        let got = parse_mounts(SAMPLE, &[Path::new("/var/lib/atrium/jails/app")]);
        assert!(got.contains(&p("/var/lib/atrium/jails/app")));
        assert!(got.contains(&p("/var/lib/atrium/jails/app/dev")));
        assert!(got.contains(&p("/var/lib/atrium/jails/app/atrium/sockets/fresco")));
    }

    /// ★★ THE PREFIX TRAP. `app-sibling` starts with the string `app` but is
    /// not under it. A string prefix test would unmount another jail's stack.
    #[test]
    fn a_sibling_whose_name_starts_with_the_root_is_not_under_it() {
        let got = parse_mounts(SAMPLE, &[Path::new("/var/lib/atrium/jails/app")]);
        assert!(!got.contains(&p("/var/lib/atrium/jails/app-sibling")),
            "a sibling was swept into the root's teardown: {got:?}");
    }

    /// Deepest first, or a parent unmounts before the nested mount it holds.
    #[test]
    fn nested_mounts_come_before_their_parents() {
        let got = parse_mounts(SAMPLE, &[Path::new("/var/lib/atrium/jails/app")]);
        let deep = got.iter().position(|x| x.ends_with("sockets/fresco")).unwrap();
        let root = got.iter().position(|x| x == &p("/var/lib/atrium/jails/app")).unwrap();
        assert!(deep < root, "parent would unmount first: {got:?}");
    }

    /// ★ The one-shot lane's writable layer lives OUTSIDE the jail root, so a
    /// teardown that swept only the root left one tmpfs per run alive in
    /// /var/run — invisible to anyone looking at the jail, accumulating as
    /// fast as the pool churned.
    #[test]
    fn several_roots_are_swept_together() {
        let got = parse_mounts(SAMPLE, &[
            Path::new("/var/lib/atrium/jails/app"),
            Path::new("/var/run/portcullis-exec/app"),
        ]);
        assert!(got.contains(&p("/var/run/portcullis-exec/app")), "{got:?}");
        assert!(got.contains(&p("/var/lib/atrium/jails/app/dev")), "{got:?}");
    }

    #[test]
    fn unrelated_mounts_are_left_alone() {
        let got = parse_mounts(SAMPLE, &[Path::new("/var/lib/atrium/jails/app")]);
        assert!(!got.contains(&p("/")), "the root filesystem was selected: {got:?}");
    }

    #[test]
    fn an_empty_or_garbled_table_selects_nothing() {
        assert!(parse_mounts("", &[Path::new("/x")]).is_empty());
        assert!(parse_mounts("garbage\nalso garbage\n", &[Path::new("/x")]).is_empty());
    }

    /// ★★ Two mounts at one path are two layers, and both must be counted —
    /// otherwise popping the top one reads as "no progress" and teardown stops
    /// with the bottom one (and anything it pins) still mounted.
    #[test]
    fn stacked_mounts_at_one_path_are_each_counted() {
        let t = "\
/var/lib/atrium/apps/w /var/lib/atrium/jails/w nullfs ro 0 0
tmpfs /var/run/portcullis-exec/w tmpfs rw 0 0
/var/run/portcullis-exec/w /var/lib/atrium/jails/w unionfs rw 0 0
";
        let roots = [Path::new("/var/lib/atrium/jails/w"), Path::new("/var/run/portcullis-exec/w")];
        let v = parse_mounts(t, &roots);
        assert_eq!(v.len(), 3, "each layer is one entry: {v:?}");
        assert_eq!(v.iter().filter(|p| p.ends_with("jails/w")).count(), 2, "{v:?}");
    }

    fn jail(devfs: bool, ruleset: Option<i64>) -> JailConfig {
        use portcullis_jail::Value;
        let mut jc = JailConfig::new("t".into(), PathBuf::from("/j"));
        jc.set("mount.devfs", Value::Bool(devfs));
        if let Some(n) = ruleset { jc.set("devfs_ruleset", Value::Number(n)); }
        jc
    }

    /// ★★ devfs with no ruleset, or ruleset 0, is the host's full /dev under
    /// another name — refused without asking the kernel anything.
    #[test]
    fn a_devfs_with_no_ruleset_is_refused() {
        let e = ensure_devfs_isolation(&jail(true, None)).unwrap_err();
        assert!(e.contains("no ruleset"), "{e}");
        let e = ensure_devfs_isolation(&jail(true, Some(0))).unwrap_err();
        assert!(e.contains("no ruleset"), "{e}");
    }

    /// A jail that mounts no devfs has nothing to isolate.
    #[test]
    fn no_devfs_mount_needs_no_ruleset() {
        assert!(ensure_devfs_isolation(&jail(false, None)).is_ok());
    }

    /// ★ Unverifiable is refused, not assumed fine. Off FreeBSD there is no
    /// `devfs(8)`, which is exactly the "cannot tell" case — the check must
    /// fail closed there, never open.
    #[cfg(not(target_os = "freebsd"))]
    #[test]
    fn a_ruleset_that_cannot_be_verified_is_refused() {
        assert!(ensure_devfs_isolation(&jail(true, Some(22))).is_err());
    }
}

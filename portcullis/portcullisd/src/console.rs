//! App consoles (docs/spec/homes.md §4): a shell in an app's own jail, as
//! the app's uid for the person who asked, on a new pty. portcullisd keeps
//! nothing but the shell's process descriptor; the pty's master side goes to
//! the terminal app on its document channel, and the terminal never enters
//! the app's jail.
//!
//! The shell is the toolbox's: FreeBSD's statically linked `/rescue`
//! binary, copied once to [`TOOLBOX`] with links named for the tools a
//! console needs, and mounted read-only at `/atrium/console` in the jail,
//! first (and only) on `PATH`. An app's root usually has no shell of its own.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The toolbox on the host.
pub const TOOLBOX: &str = "/var/db/atrium/console";
/// Where it is in the jail.
pub const IN_JAIL: &str = "atrium/console";
/// The terminal app that shows consoles.
pub const TERMINAL: &str = "org.atrium.terminal";
/// What a console is on the document channel: `MIME; app=<id>`, the
/// app the terminal may ask for more of (another window).
pub const MIME: &str = "application/x-atrium-console";
/// The console's terminal, as termcap capabilities (the toolbox's
/// `termcap` file).
const VT100: &str = "co#80:li#24:am:bs:cl=\\E[H\\E[J:cm=\\E[%i%d;%dH:ce=\\E[K:cd=\\E[J:up=\\E[A:do=^J:nd=\\E[C:le=^H:ho=\\E[H:so=\\E[7m:se=\\E[m:";
/// The crunched binary the tools are links to.
const RESCUE: &str = "/rescue/rescue";

/// The toolbox's tools (homes.md §4: sh, ls, cat, cp, mv, rm, ps, vi and
/// what a shell session leans on). Nothing that administers the host:
/// they would fail as the app's uid in its jail anyway.
pub const TOOLS: &[&str] = &[
    "sh", "ls", "cat", "cp", "mv", "rm", "mkdir", "rmdir", "ln", "chmod", "ps", "kill",
    "pgrep", "pkill", "vi", "ex", "less", "more", "ed", "id", "whoami", "pwd", "echo",
    "test", "[", "head", "tail", "sed", "tee", "expr", "date", "sleep", "df", "realpath",
    "tar", "gzip", "gunzip", "zcat", "xz", "unxz",
];

/// Make the toolbox (once): a copy of the rescue binary, root's and
/// read-only, and a link per tool.
pub fn ensure_toolbox() -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(TOOLBOX);
    let bin = dir.join("rescue");
    if !bin.exists() {
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        let tmp = dir.join(".rescue.new");
        std::fs::copy(RESCUE, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555))?;
        std::fs::rename(&tmp, &bin)?;
    }
    // The shell's line editing wants its terminal described; the jail has
    // no database. Ours: the one entry, as termcap, and the base system's
    // terminfo entry when there is one.
    let tc = dir.join("termcap");
    if !tc.exists() {
        std::fs::write(&tc, format!("vt100|atrium console:{}\n", VT100))?;
        std::fs::set_permissions(&tc, std::fs::Permissions::from_mode(0o444))?;
    }
    let ti = dir.join("terminfo/v/vt100");
    if !ti.exists() {
        if let Some(src) = ["/usr/share/terminfo/v/vt100", "/usr/share/misc/terminfo/v/vt100"]
            .iter().find(|p| Path::new(p).exists())
        {
            std::fs::create_dir_all(ti.parent().unwrap())?;
            std::fs::copy(src, &ti)?;
        }
    }
    for t in TOOLS {
        let l = dir.join(t);
        if std::fs::symlink_metadata(&l).is_err() {
            std::os::unix::fs::symlink("rescue", &l)?;
        }
    }
    Ok(dir)
}

/// Mount the toolbox in the jail at `jail_path` (once per jail run; the
/// jail's teardown unmounts everything under its path).
pub fn mount_toolbox(jail_path: &Path) -> Result<(), String> {
    let dir = ensure_toolbox().map_err(|e| format!("the console's toolbox: {e}"))?;
    let at = jail_path.join(IN_JAIL);
    let mounted = Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines()
            .any(|l| l.split_whitespace().nth(1) == Some(at.to_str().unwrap_or(""))))
        .unwrap_or(false);
    if mounted {
        return Ok(());
    }
    // Made by root through the union: lands in the overlay, root's.
    std::fs::create_dir_all(&at).map_err(|e| format!("mkdir {}: {e}", at.display()))?;
    let st = Command::new("mount").args(["-t", "nullfs", "-o", "ro"]).arg(&dir).arg(&at).status()
        .map_err(|e| format!("mount: {e}"))?;
    if !st.success() {
        return Err(format!("mount the toolbox at {}: {st}", at.display()));
    }
    Ok(())
}

/// The jail's processes all run as `uid`: it is this person's instance.
pub fn jail_runs_as(jid: i32, uid: u32) -> bool {
    let Ok(o) = Command::new("ps").args(["-J", &jid.to_string(), "-o", "uid="]).output() else { return false };
    let uids: Vec<u32> = String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|s| s.parse().ok()).collect();
    !uids.is_empty() && uids.iter().all(|u| *u == uid)
}

/// An account's primary group.
pub fn gid_of(uid: u32) -> Option<u32> {
    let o = Command::new("pw").args(["usershow", &uid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&o.stdout).trim().split(':').nth(3)?.parse().ok()
}

/// Consoles running now, by (person, app): the terminal may ask for more
/// only in an app it already has one in.
static LIVE: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Does `owner` have a console in `app_id` now?
pub fn live(owner: &str, app_id: &str) -> bool {
    LIVE.lock().unwrap().iter().any(|(o, a)| o == owner && a == app_id)
}

/// What the shell is started with.
pub struct Spec<'a> {
    /// The person it runs for.
    pub owner: &'a str,
    pub jid: i32,
    pub uid: u32,
    pub gid: u32,
    /// `$HOME` in the jail, also where it starts.
    pub home: &'a str,
    /// For the prompt.
    pub app_id: &'a str,
    pub cols: u16,
    pub rows: u16,
}

/// Start the shell. Returns the pty's master side; the shell's process is
/// reaped by a thread of ours when it ends (no stoad: the terminal holds
/// the pty, and closing it ends the shell).
pub fn spawn(s: &Spec) -> io::Result<OwnedFd> {
    let (master, procdesc, pid) = spawn_raw(s)?;
    let (app, owner) = (s.app_id.to_string(), s.owner.to_string());
    LIVE.lock().unwrap().push((owner.clone(), app.clone()));
    std::thread::Builder::new().name(format!("console-{pid}")).spawn(move || {
        // The descriptor reports POLLHUP when the shell has ended; closing
        // it then reaps (PD_NOWAITPID).
        let mut p = libc::pollfd { fd: procdesc.as_raw_fd(), events: libc::POLLHUP, revents: 0 };
        loop {
            // SAFETY: one pollfd that outlives the call.
            let r = unsafe { libc::poll(&mut p, 1, -1) };
            if r > 0 || (r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted) {
                break;
            }
        }
        eprintln!("portcullisd: {app}'s console (pid {pid}) ended");
        drop(procdesc);
        let mut l = LIVE.lock().unwrap();
        if let Some(i) = l.iter().position(|(o, a)| *o == owner && *a == app) {
            l.remove(i);
        }
    })?;
    Ok(master)
}

/// Start the shell: (pty master, process descriptor, pid). Whoever holds
/// the descriptor reaps it (stoad, for a Stoa console).
pub fn spawn_raw(s: &Spec) -> io::Result<(OwnedFd, OwnedFd, i32)> {
    let c = |v: &str| CString::new(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL"));
    // Everything the child needs, made before the fork.
    let path = c(&format!("/{IN_JAIL}/sh"))?;
    let argv = [c("sh")?, c("-i")?];
    let env = [
        c(&format!("PATH=/{IN_JAIL}"))?,
        c(&format!("HOME={}", s.home))?,
        c("TERM=vt100")?,
        // The jail has no terminal database: the toolbox carries the one
        // entry, so the shell's line editing works and doesn't complain.
        c(&format!("TERMCAP=/{IN_JAIL}/termcap"))?,
        c(&format!("TERMINFO=/{IN_JAIL}/terminfo"))?,
        c(&format!("PS1={} $ ", s.app_id))?,
        c("ENV=")?,
    ];
    let home = c(s.home)?;
    let root = c("/")?;
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());

    let (master, slave) = jaild::ffi::openpty_pair(s.cols, s.rows)?;
    // SAFETY: both fds are fresh from openpty and owned here alone.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let pd = jaild::ffi::pdfork()?;
    if pd.pid == 0 {
        // ====== child: nothing that allocates or locks from here on ======
        // SAFETY: async-signal-safe calls only, on what was made above.
        unsafe {
            libc::close(master.as_raw_fd());
            if libc::login_tty(slave.as_raw_fd()) != 0 { libc::_exit(101) }
            if libc::jail_attach(s.jid) != 0 { libc::_exit(102) }
            if libc::chdir(home.as_ptr()) != 0 && libc::chdir(root.as_ptr()) != 0 { libc::_exit(103) }
            let groups = [s.gid as libc::gid_t];
            if libc::setgroups(1, groups.as_ptr()) != 0 || libc::setgid(s.gid) != 0 || libc::setuid(s.uid) != 0 {
                libc::_exit(104)
            }
            // A drop that didn't happen is fatal (jaild::ffi::verify_dropped).
            if libc::getuid() != s.uid || libc::geteuid() != s.uid || libc::getgid() != s.gid || libc::setuid(0) == 0 {
                libc::_exit(105)
            }
            libc::execve(path.as_ptr(), argp.as_ptr(), envp.as_ptr());
            libc::_exit(106)
        }
    }
    drop(slave);
    // SAFETY: pdfork's descriptor, ours alone.
    let procdesc = unsafe { OwnedFd::from_raw_fd(pd.procdesc_fd) };
    Ok((master, procdesc, pd.pid))
}

// ── Stoa (docs/spec/stoa.md; homes.md §4) ──────────────────────────────
//
// With stoad running, a console's shells live there, not in the terminal:
// closing the window detaches, Terminal on the app reattaches, and a tab is
// another Stoa window. portcullisd still starts every shell (stoad never
// enters a jail) and hands it over; the terminal gets one end of a
// datagram socketpair stoad holds the other end of, and the session key.

/// stoad's control socket.
pub const STOAD: &str = "/var/run/atrium/stoad.sock";
pub const STOAD_ENV: &str = "PORTCULLISD_STOAD";
/// A Stoa console on the document channel:
/// `STOA_MIME; app=<id>; session=<name>; key=<hex>`.
pub const STOA_MIME: &str = "application/x-atrium-stoa";

fn stoad_path() -> PathBuf {
    std::env::var_os(STOAD_ENV).map(PathBuf::from).unwrap_or_else(|| STOAD.into())
}

/// Is stoad there to hold consoles?
pub fn stoad_up() -> bool {
    std::os::unix::net::UnixStream::connect(stoad_path()).is_ok()
}

/// Console sessions of `owner`'s for `app_id` are named `<prefix><n>`.
/// `:` is in neither a user name nor an app id, so one person's prefix is
/// never another's.
pub fn session_prefix(owner: &str, app_id: &str) -> String {
    format!("console.{owner}:{app_id}:")
}

fn read_line(s: &mut std::os::unix::net::UnixStream) -> io::Result<String> {
    use std::io::Read;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b)? {
            0 => break,
            _ if b[0] == b'\n' => break,
            _ => line.push(b[0]),
        }
        if line.len() > 64 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// One command to stoad, with descriptors after its READY.
fn stoad(cmd: &str, fds: &[std::os::fd::RawFd]) -> io::Result<String> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(stoad_path())?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    s.write_all(format!("{cmd}\n").as_bytes())?;
    if !fds.is_empty() {
        let r = read_line(&mut s)?;
        if r != "READY" {
            return Err(io::Error::other(r));
        }
        portcullis_ipc::send_fds_with(&s, fds, b"F")?;
        return read_line(&mut s);
    }
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

/// `owner`'s live console sessions for `app_id`: (name, attached).
pub fn sessions(owner: &str, app_id: &str) -> Vec<(String, bool)> {
    let p = session_prefix(owner, app_id);
    stoad(&format!("CONSOLES {p}"), &[]).unwrap_or_default().lines()
        .filter_map(|l| {
            let mut it = l.split(' ');
            let (n, live, att) = (it.next()?, it.next()?.parse::<usize>().ok()?, it.next()?);
            (n.starts_with(&p) && live > 0).then(|| (n.to_string(), att == "1"))
        })
        .collect()
}

/// A name for a new session of `owner`'s for `app_id`.
pub fn new_session(owner: &str, app_id: &str) -> String {
    let p = session_prefix(owner, app_id);
    let n = sessions(owner, app_id).iter()
        .filter_map(|(s, _)| s.strip_prefix(&p)?.parse::<u32>().ok())
        .max().map_or(1, |m| m + 1);
    format!("{p}{n}")
}

/// Hand stoad a shell as a new window of session `name`.
pub fn adopt(name: &str, app_id: &str, shell: (OwnedFd, OwnedFd, i32)) -> Result<(), String> {
    let (master, procdesc, pid) = shell;
    let r = stoad(&format!("ADOPT {name} {app_id} {pid} 80 24"), &[procdesc.as_raw_fd(), master.as_raw_fd()])
        .map_err(|e| format!("stoad: {e}"))?;
    if r.starts_with("OK") { Ok(()) } else { Err(format!("stoad: {r}")) }
}

/// Attach session `name`: the terminal's end of a new link, and the key.
pub fn attach(name: &str) -> Result<(OwnedFd, String), String> {
    let (ours, theirs) = std::os::unix::net::UnixDatagram::pair().map_err(|e| e.to_string())?;
    let r = stoad(&format!("ATTACH {name}"), &[ours.as_raw_fd()]).map_err(|e| format!("stoad: {e}"))?;
    if r.starts_with("ERR") || r.len() != 64 {
        return Err(format!("stoad: {r}"));
    }
    Ok((OwnedFd::from(theirs), r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toolbox_has_the_spec_tools_and_nothing_that_administers() {
        for t in ["sh", "ls", "cat", "cp", "mv", "rm", "ps", "vi"] {
            assert!(TOOLS.contains(&t), "{t}");
        }
        for t in ["mount", "ifconfig", "kldload", "zfs", "reboot", "chroot", "sysctl"] {
            assert!(!TOOLS.contains(&t), "{t}");
        }
    }

    #[test]
    fn one_persons_sessions_never_match_anothers() {
        let a = session_prefix("a", "b.c.d");
        let b = session_prefix("a.b", "c.d");
        assert!(!format!("{b}1").starts_with(&a) && !format!("{a}1").starts_with(&b));
    }
}

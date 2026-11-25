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

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The toolbox on the host.
pub const TOOLBOX: &str = "/var/db/atrium/console";
/// Where it is in the jail.
pub const IN_JAIL: &str = "atrium/console";
/// The terminal app that shows consoles.
pub const TERMINAL: &str = "org.atrium.terminal";
/// A console's target names a workspace with this before its name
/// (`workspace/<name>`): `/` is in no app id.
pub const WORKSPACE_PREFIX: &str = "workspace/";
/// What a console is on the document channel: `MIME; app=<id>`, the
/// app the terminal may ask for more of (another window).
pub const MIME: &str = "application/x-atrium-console";
/// The console's terminal (the Terminal, stoa-term), as the toolbox's
/// `termcap` entry: xterm-256color's capabilities that stoa-term implements
/// (dev-tools-plan.md M1). Kept under termcap's 1023-byte entry limit.
const TERM_NAME: &str = "xterm-256color";
const TERMCAP: &str = concat!(
    "xterm-256color|atrium console:",
    "am:bs:km:mi:ms:xn:co#80:li#24:it#8:Co#256:pa#32767:",
    "AB=\\E[48;5;%dm:AF=\\E[38;5;%dm:AL=\\E[%dL:DC=\\E[%dP:DL=\\E[%dM:DO=\\E[%dB:IC=\\E[%d@:",
    "LE=\\E[%dD:RI=\\E[%dC:UP=\\E[%dA:SF=\\E[%dS:SR=\\E[%dT:",
    "al=\\E[L:cd=\\E[J:ce=\\E[K:cl=\\E[H\\E[2J:cm=\\E[%i%d;%dH:cs=\\E[%i%d;%dr:ct=\\E[3g:",
    "dc=\\E[P:dl=\\E[M:do=^J:ec=\\E[%dX:ei=\\E[4l:ho=\\E[H:im=\\E[4h:le=^H:nd=\\E[C:up=\\E[A:",
    "sf=^J:sr=\\EM:st=\\EH:ta=^I:cr=^M:sc=\\E7:rc=\\E8:",
    "md=\\E[1m:me=\\E[m:mr=\\E[7m:mh=\\E[2m:so=\\E[7m:se=\\E[27m:us=\\E[4m:ue=\\E[24m:op=\\E[39;49m:",
    "ti=\\E[?1049h:te=\\E[?1049l:vi=\\E[?25l:ve=\\E[?25h:ks=\\E[?1h\\E=:ke=\\E[?1l\\E>:",
    "ku=\\EOA:kd=\\EOB:kr=\\EOC:kl=\\EOD:kh=\\EOH:@7=\\EOF:kb=^?:kD=\\E[3~:kI=\\E[2~:kN=\\E[6~:kP=\\E[5~:",
    "k1=\\EOP:k2=\\EOQ:k3=\\EOR:k4=\\EOS:k5=\\E[15~:k6=\\E[17~:k7=\\E[18~:k8=\\E[19~:k9=\\E[20~:k;=\\E[21~:",
    "ac=``aaffggiijjkkllmmnnooppqqrrssttuuvvwwxxyyzz{{||}}~~:as=\\E(0:ae=\\E(B:\n",
);
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
    if std::fs::read_to_string(&tc).ok().as_deref() != Some(TERMCAP) {
        let tmp = dir.join(".termcap.new");
        std::fs::write(&tmp, TERMCAP)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))?;
        std::fs::rename(&tmp, &tc)?;
    }
    let ti = dir.join("terminfo/x").join(TERM_NAME);
    if !ti.exists() {
        let src = [format!("/usr/share/terminfo/x/{TERM_NAME}"), format!("/usr/share/misc/terminfo/x/{TERM_NAME}")];
        if let Some(src) = src.iter().find(|p| Path::new(p).exists()) {
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

/// The console jail beside an app's (dev-tools-plan.md M0, Shankar's P1,
/// 2026-11-17): a second jail on the same root, so it sees the same files
/// through the app's mounts, but with its own system call class, `terminal`.
/// The app's jail keeps its narrow class; a shell needs `fork` and `wait4`,
/// which no app class has. It has no network and sees none of the app's
/// processes. It makes no mounts of its own (no devfs either: the app's is
/// already on the root), and goes when the app's jail is torn down
/// (`launch::full_teardown` removes jails named [`launch::CONSOLE_JAIL_PREFIX`]
/// on the same root).
pub fn console_jail_name(app_id: &str) -> String {
    format!("{}{}", crate::launch::CONSOLE_JAIL_PREFIX, portcullis_jail::jail_name_from_app_id(app_id))
}

fn jid_of(name: &str) -> Option<i32> {
    jaild::ffi::jail_id_by_name(name)
}

/// The console jail for `app_id` on `jail_path`, `owner`'s instance:
/// joined if it runs, made if not. The toolbox must already be mounted
/// there ([`mount_toolbox`]).
pub fn console_jail(app_id: &str, owner: &str, jail_path: &Path) -> Result<i32, String> {
    use crate::launch;
    let name = console_jail_name(app_id);
    if let Some(jid) = jid_of(&name) {
        return Ok(jid);
    }
    let mid = format!("console.{app_id}");
    let text = format!("[app]\nid = {mid:?}\nname = \"Console\"\nversion = \"1\"\nentry = \"{IN_JAIL}/sh\"\n\n[capabilities]\nnetwork = \"none\"\n");
    let manifest = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("its manifest: {e}"))?;
    let sysfil = launch::sysfil_for_class(launch::TERMINAL_CLASS).map_err(|e| format!("{e:?}"))?;
    let host_identity = portcullis_identity::for_app(&mid).map_err(|e| format!("identity: {e}"))?;
    let opts = portcullis_jail::BuildOpts {
        root_path: jail_path.to_path_buf(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/tmp"),
        user_name: "root".into(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity,
        sysfil,
    };
    let mut jc = portcullis_jail::build(&manifest, &opts).map_err(|e| format!("build: {e}"))?;
    jc.name = name.clone();
    // ★ Nothing mounted by this jail: the root's mounts are the app jail's.
    jc.mounts.clear();
    jc.devfs_actions.clear();
    jc.params.retain(|(k, _)| k != "exec.prestart");
    jc.set("mount.devfs", portcullis_jail::Value::Bool(false));
    launch::create_persistent(&jc, jail_path, jaild::protocol::JailClass::Terminal { owner: owner.into() }, launch::TERMINAL_CLASS)
        .map_err(|e| format!("{e:?}"))?;
    let jid = jid_of(&name).ok_or_else(|| format!("{name} did not start"))?;
    eprintln!("portcullisd: console jail {name} on {} (jid {jid}, class terminal)", jail_path.display());
    Ok(jid)
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
    /// The jail's name: jaild starts the shell there.
    pub jail: &'a str,
    pub uid: u32,
    pub gid: u32,
    /// `$HOME` in the jail, also where it starts.
    pub home: &'a str,
    /// For the prompt.
    pub app_id: &'a str,
    pub cols: u16,
    pub rows: u16,
    /// Supplementary groups beyond `gid` (a workspace's: the person's
    /// files group, so what the file manager hands in is readable).
    pub groups: &'a [u32],
    /// More environment (a workspace's object cache folders).
    pub env: &'a [String],
    /// What runs where: the toolbox's shell in an app's jail, the base
    /// system's in a workspace.
    pub kind: Kind,
}

/// Where the shell and its tools come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An app's jail: the toolbox at /atrium/console, and its termcap.
    App,
    /// A workspace (homes.md §5): the base system, a umask the file manager
    /// can share files under.
    Workspace,
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
/// the descriptor reaps it (stoad, for a Stoa console). jaild starts it, as
/// the identity in `s`, on a new pty.
pub fn spawn_raw(s: &Spec) -> io::Result<(OwnedFd, OwnedFd, i32)> {
    let ws = s.kind == Kind::Workspace;
    let path = if ws { "/bin/sh".to_string() } else { format!("/{IN_JAIL}/sh") };
    let mut env = vec![
        format!("HOME={}", s.home),
        format!("TERM={TERM_NAME}"),
        "ENV=".to_string(),
    ];
    if ws {
        // The workspace's own PATH (its toolchain layers first) when it has one.
        if !s.env.iter().any(|e| e.starts_with("PATH=")) {
            env.push("PATH=/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin".into());
        }
        env.push(format!("PS1={} $ ", s.app_id.strip_prefix(WORKSPACE_PREFIX).unwrap_or(s.app_id)));
        env.push(format!("USER={}", s.owner));
        env.extend(s.env.iter().cloned());
    } else {
        env.push(format!("PATH=/{IN_JAIL}"));
        // The jail has no terminal database: the toolbox carries the one
        // entry, so the shell's line editing works and doesn't complain.
        env.push(format!("TERMCAP=/{IN_JAIL}/termcap"));
        env.push(format!("TERMINFO=/{IN_JAIL}/terminfo"));
        env.push(format!("PS1={} $ ", s.app_id));
        // App jails have /tmp but no /var/tmp: without a recovery folder the
        // toolbox's vi opens on an error that eats the first key.
        env.push("NEXINIT=set recdir=/tmp/vi.recover".into());
    }
    let env = env.into_iter().map(|e| {
        let (k, v) = e.split_once('=').unwrap_or((e.as_str(), ""));
        jaild::protocol::EnvPair { key: k.into(), value: v.into() }
    }).collect();
    // A workspace's shell starts in ~/work; an app's uid has no home.
    let cwd = if ws { format!("{}/work", s.home) } else if s.home.starts_with("/nonexistent") { "/".into() } else { s.home.to_string() };
    let req = jaild::protocol::ExecInJailRequest {
        name: s.jail.into(),
        path,
        argv: vec!["sh".into(), "-i".into()],
        env,
        want_root: false,
        cols: s.cols,
        rows: s.rows,
        identity: Some(jaild::protocol::ExecIdentity { uid: s.uid, gid: s.gid, groups: s.groups.to_vec() }),
        cwd: Some(cwd),
        // What the file manager hands in stays shareable in a workspace.
        umask: ws.then_some(0o002),
        stdio: false,
    };
    let (run, master) = portcullisd::jaild_jail::exec_in(req, None).map_err(io::Error::other)?;
    let master = master.ok_or_else(|| io::Error::other("jaild sent no pty"))?;
    let pid = run.pid;
    Ok((master, run.into_procdesc(), pid))
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

/// stoad lets `person`'s sessions go (stoa.md §5.4a): in one persona or
/// all, written out first unless `erase` (the persona is being removed).
/// Logged, never fatal: stoad not running holds nothing.
pub fn stoad_drop(person: &str, persona: Option<&str>, erase: bool) {
    let Some(uid) = uid_of(person) else { return };
    if !stoad_up() {
        return;
    }
    let cmd = format!("DROP {uid} {} {}", persona.unwrap_or("*"), if erase { "erase" } else { "keep" });
    match stoad(&cmd, &[]) {
        Ok(r) => eprintln!("portcullisd: stoad {cmd}: {}", r.trim()),
        Err(e) => eprintln!("portcullisd: stoad {cmd}: {e}"),
    }
}

fn uid_of(person: &str) -> Option<u32> {
    let c = std::ffi::CString::new(person).ok()?;
    // SAFETY: zeroed passwd and a buffer that outlive the call; getpwnam_r
    // is the thread-safe lookup and fills only what it was given.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwnam_r(c.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
    (r == 0 && !res.is_null()).then_some(pwd.pw_uid)
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

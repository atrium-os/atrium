//! `fenestrad --frescod SOCK --session BIN (--mint-sock PATH | --mint-dir DIR)
//! [--bind ADDR] [--public-ip IP] [--seat PATH] [--grace S]
//! [--jaild SOCK --lan IFACE SUBNET] [--user NAME]`
//!
//! Fenestra's daemon on the host (fenestra.md §3.2). It has no network:
//! it holds the link to frescod's broker (`FRESCOD_FENESTRA_SOCK`) and
//! starts each session as its own process (`fenestra-session`, in its own
//! jail on Atrium), whose stdin is a socket to fenestrad. Between the two
//! it relays [`fenestra_link`] records:
//!
//! - a session's `Want`s go to frescod under a ticket of fenestrad's own,
//!   so sessions cannot see or answer each other's; the `Data` and `End`
//!   that answer one go back only to the session that asked;
//! - `Changed` and `Layout` go to every session;
//! - frescod is told the session is on while any session wants it;
//! - a session's viewer input goes to frescod (bounded and rate-limited by
//!   the session and again by frescod).
//!
//! Each peer may send only its own side's records, and a session whose
//! queue fills (it stopped reading) is ended, never waited on.
//!
//! **Mints** (§11.2) come on `--mint-sock` from `fenestra-shell mint`, which
//! sshd runs as the person: one bounded line, `MINT VIEWER_KEY_HEX
//! [take-over]`. The caller's uid comes from the kernel (`getpeereid`) and
//! must be the user of the seat session (`--seat`, Ostiarius' active
//! session file); anything else, or an unreadable seat, is refused. The
//! whole desktop has one viewer with control: a second mint is refused
//! unless it asks to take over, which ends the first session. A session is
//! started with the viewer's key on fd 3 and answers there with its mint
//! line, which goes back with the address set to `--public-ip`.
//!
//! With `--jaild`, each minted session runs in its own jail (`jail.rs`):
//! its own uid, its own vnet whose only grant is one UDP port from the
//! local network `--lan` names. Without it a session is a plain child
//! (other systems, and gates on a host without jaild).
//!
//! With `--user` (on Atrium, `_fenestra`) fenestrad binds the mint socket
//! as root, then runs as that user, and connects to frescod as it; with
//! `--jaild` too, a root side forked first keeps jaild (`privsep.rs`).
//!
//! `--mint-dir` is the development form: one session through files, and
//! fenestrad exits when it ends.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use fenestra_link::Rec;

#[cfg(target_os = "freebsd")]
mod jail;
mod privsep;
use fenestra_wire::mint::{self, DevMint};

/// Records queued for one session before it counts as stuck.
const SESSION_QUEUE: usize = 4096;
/// Wants one session may have outstanding.
const MAX_WANTS: usize = 256;
/// Sessions at once: the whole desktop has one viewer with control.
const MAX_SESSIONS: usize = 1;
/// Longest mint request line.
const MAX_REQUEST: usize = 512;
/// How long a mint caller, and a new session's answer, may take.
const MINT_TIMEOUT: Duration = Duration::from_secs(15);

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestrad: {why}");
    std::process::exit(1)
}

/// A session's process: a plain child, or (on Atrium) its own jail.
enum Proc {
    Child(Child),
    #[cfg(target_os = "freebsd")]
    Jail(jail::Running),
    /// In a jail the root side made (`--user` with `--jaild`): its port.
    #[cfg(target_os = "freebsd")]
    Remote(u16, Arc<privsep::jailer::Jailer>),
}

impl Proc {
    fn stop(&mut self) {
        match self {
            Proc::Child(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            // Its jail goes when the process does (jail.rs watches it).
            #[cfg(target_os = "freebsd")]
            Proc::Jail(r) => r.kill(),
            #[cfg(target_os = "freebsd")]
            Proc::Remote(port, j) => j.stop(*port),
        }
    }
}

struct Session {
    tx: SyncSender<Rec>,
    child: Proc,
    on: bool,
    /// How many of its wants are outstanding.
    wants: usize,
}

#[derive(Default)]
struct Table {
    sessions: HashMap<u32, Session>,
    /// fenestrad's ticket -> (session, the session's ticket).
    tickets: HashMap<u32, (u32, u32)>,
    next: u32,
    on: bool,
    /// The layout and chrome frescod last sent while on: frescod sends
    /// each only when it changes, so a session that joins later gets
    /// them from here.
    layout: Option<Rec>,
    chrome: Option<Rec>,
}

struct Relay {
    table: Mutex<Table>,
    frescod: Mutex<UnixStream>,
}

impl Relay {
    fn to_frescod(&self, rec: &Rec) -> std::io::Result<()> {
        let mut s = self.frescod.lock().unwrap_or_else(|e| e.into_inner());
        fenestra_link::write(&mut *s, rec)
    }

    /// Tell frescod whether any session is on, when that changed.
    fn sync_on(&self, t: &mut Table) {
        let on = t.sessions.values().any(|s| s.on);
        if on != t.on {
            t.on = on;
            t.layout = None;
            t.chrome = None;
            if let Err(e) = self.to_frescod(&Rec::Session { on }) {
                die(format!("frescod: {e}"));
            }
        }
    }

    fn to_session(t: &mut Table, id: u32, rec: Rec) {
        let Some(s) = t.sessions.get(&id) else { return };
        match s.tx.try_send(rec) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                eprintln!("fenestrad: session {id} stopped reading; ending it");
                if let Some(mut s) = t.sessions.remove(&id) {
                    s.child.stop();
                }
            }
        }
    }

    /// Records from frescod, until it goes.
    fn from_frescod(&self, mut r: UnixStream) -> String {
        loop {
            let rec = match fenestra_link::read(&mut r) {
                Ok(Some(rec)) if rec.from_broker() => rec,
                Ok(Some(rec)) => return format!("frescod sent {rec:?} (not a broker's record)"),
                Ok(None) => return "frescod closed the link".into(),
                Err(e) => return format!("frescod: {e}"),
            };
            let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
            match rec {
                Rec::Layout { .. } | Rec::Chrome(_) => {
                    if matches!(rec, Rec::Layout { .. }) {
                        t.layout = Some(rec.clone());
                    } else {
                        t.chrome = Some(rec.clone());
                    }
                    let ids: Vec<u32> = t.sessions.keys().copied().collect();
                    for id in ids {
                        Relay::to_session(&mut t, id, rec.clone());
                    }
                }
                Rec::Changed { .. } => {
                    let ids: Vec<u32> = t.sessions.keys().copied().collect();
                    for id in ids {
                        Relay::to_session(&mut t, id, rec.clone());
                    }
                }
                Rec::Data { window, ticket, bytes } => {
                    // A want of a session that went: dropped.
                    if let Some(&(id, theirs)) = t.tickets.get(&ticket) {
                        Relay::to_session(&mut t, id, Rec::Data { window, ticket: theirs, bytes });
                    }
                }
                Rec::End { window, ticket, now } => {
                    if let Some((id, theirs)) = t.tickets.remove(&ticket) {
                        if let Some(s) = t.sessions.get_mut(&id) {
                            s.wants = s.wants.saturating_sub(1);
                        }
                        Relay::to_session(&mut t, id, Rec::End { window, ticket: theirs, now });
                    }
                }
                Rec::Session { .. } | Rec::Want { .. } | Rec::Input(_) => unreachable!("from_broker() refused it"),
            }
            // A session ended for not reading may have been the one on.
            self.sync_on(&mut t);
        }
    }

    /// Records from session `id`, until it goes.
    fn from_session(&self, id: u32, mut r: UnixStream) -> String {
        loop {
            let rec = match fenestra_link::read(&mut r) {
                Ok(Some(rec)) if !rec.from_broker() => rec,
                Ok(Some(rec)) => return format!("sent {rec:?} (a broker's record)"),
                Ok(None) => return "closed".into(),
                Err(e) => return e.to_string(),
            };
            let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
            let was_on = t.on;
            let Some(s) = t.sessions.get_mut(&id) else { return "ended".into() };
            match rec {
                Rec::Session { on } => {
                    let joins = on && !s.on && was_on;
                    s.on = on;
                    self.sync_on(&mut t);
                    if joins {
                        for rec in [t.layout.clone(), t.chrome.clone()].into_iter().flatten() {
                            Relay::to_session(&mut t, id, rec);
                        }
                    }
                }
                Rec::Want { window, ticket, base } => {
                    if s.wants >= MAX_WANTS {
                        return "too many wants outstanding".into();
                    }
                    s.wants += 1;
                    t.next = t.next.wrapping_add(1);
                    while t.tickets.contains_key(&t.next) {
                        t.next = t.next.wrapping_add(1);
                    }
                    let ours = t.next;
                    t.tickets.insert(ours, (id, ticket));
                    drop(t);
                    if let Err(e) = self.to_frescod(&Rec::Want { window, ticket: ours, base }) {
                        die(format!("frescod: {e}"));
                    }
                }
                Rec::Input(i) => {
                    drop(t);
                    if let Err(e) = self.to_frescod(&Rec::Input(i)) {
                        die(format!("frescod: {e}"));
                    }
                }
                _ => unreachable!("from_broker() refused it"),
            }
        }
    }

    fn end_session(&self, id: u32) {
        let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut s) = t.sessions.remove(&id) {
            s.child.stop();
        }
        t.tickets.retain(|_, (s, _)| *s != id);
        self.sync_on(&mut t);
    }
}

struct Config {
    bin: String,
    bind: Option<String>,
    public_ip: Option<std::net::IpAddr>,
    seat: String,
    grace: Option<u64>,
    /// Sessions in jails through jaild (`--jaild SOCK --lan IFACE SUBNET`).
    #[cfg(target_os = "freebsd")]
    jails: Option<Jailing>,
}

/// Who asks jaild for a session's jail.
#[cfg(target_os = "freebsd")]
enum Jailing {
    /// fenestrad itself, as root.
    Here(jail::Jails),
    /// The root side, for fenestrad running as `--user`.
    Root(Arc<privsep::jailer::Jailer>),
}

enum MintBy {
    /// The development mint: the session reads and writes this directory.
    Dir(String),
    /// The viewer's key, handed over on fd 3.
    Key(Vec<u8>),
}

#[cfg(target_os = "freebsd")]
fn jailed(cfg: &Config) -> Option<&Jailing> {
    cfg.jails.as_ref()
}

#[cfg(not(target_os = "freebsd"))]
fn jailed(_: &Config) -> Option<()> {
    None
}

/// Start session `id`; returns its mint (the `Key` form) once it answers.
/// Its relay threads run until it ends, then it is taken out of the table.
fn start_session(relay: &Arc<Relay>, cfg: &Config, id: u32, by: MintBy) -> Result<Option<DevMint>, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&cfg.bin);
    if let Some(b) = &cfg.bind {
        cmd.arg("--bind").arg(b);
    }
    if let Some(g) = cfg.grace {
        cmd.arg("--grace").arg(g.to_string());
    }
    let mut mint_ours = None;
    match &by {
        MintBy::Dir(d) => {
            cmd.arg("--mint-dir").arg(d);
        }
        MintBy::Key(_) => {
            let (a, b) = UnixStream::pair().map_err(|e| e.to_string())?;
            let fd = b.as_raw_fd();
            cmd.arg("--mint-fd").arg("3");
            // SAFETY: runs in the child between fork and exec, calling only
            // dup2 and fcntl, which are async-signal-safe; `b` stays open in
            // the parent until spawn returns, so `fd` is valid in the child.
            // dup2 onto itself would keep close-on-exec, so that case clears
            // the flag instead.
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
                    let r = if fd == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(fd, 3) };
                    if r < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            mint_ours = Some((a, b));
        }
    }
    let child = match (jailed(cfg), &mint_ours) {
        #[cfg(target_os = "freebsd")]
        (Some(j), Some((_, b))) => {
            use std::os::fd::AsFd;
            match j {
                Jailing::Here(j) => {
                    let r = j.start(theirs.as_fd(), b.as_fd(), &jail_args(cfg.grace)).map_err(|e| e.to_string())?;
                    eprintln!("fenestrad: session {id} in jail {}", r.port);
                    Proc::Jail(r)
                }
                Jailing::Root(j) => {
                    let port = j.start(theirs.as_fd(), b.as_fd()).map_err(|e| e.to_string())?;
                    eprintln!("fenestrad: session {id} in jail {port}");
                    Proc::Remote(port, j.clone())
                }
            }
        }
        _ => Proc::Child(
            cmd.stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
                .stdout(Stdio::null())
                .spawn()
                .map_err(|e| format!("{}: {e}", cfg.bin))?,
        ),
    };
    let (tx, rx) = mpsc::sync_channel::<Rec>(SESSION_QUEUE);
    relay.table.lock().unwrap_or_else(|e| e.into_inner()).sessions.insert(id, Session { tx, child, on: false, wants: 0 });
    let mut w = ours.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        for rec in rx.iter() {
            if fenestra_link::write(&mut w, &rec).is_err() {
                break;
            }
        }
        let _ = w.shutdown(std::net::Shutdown::Both);
    });
    eprintln!("fenestrad: session {id} started ({})", cfg.bin);
    {
        let relay = relay.clone();
        std::thread::spawn(move || {
            let why = relay.from_session(id, ours);
            eprintln!("fenestrad: session {id} ended: {why}");
            relay.end_session(id);
        });
    }
    let (MintBy::Key(key), Some((mut a, b))) = (by, mint_ours) else { return Ok(None) };
    drop(b);
    let answer = (|| {
        a.set_read_timeout(Some(MINT_TIMEOUT)).map_err(|e| e.to_string())?;
        a.write_all(format!("viewer_key {}\n", mint::hex(&key)).as_bytes()).map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::BufReader::new((&mut a).take(mint::MAX_LINE as u64 + 1)).read_line(&mut line).map_err(|e| e.to_string())?;
        let mut m = DevMint::parse_line(line.trim_end()).map_err(|e| format!("the session's mint: {e}"))?;
        if let Some(ip) = cfg.public_ip {
            m.addr.set_ip(ip);
        }
        Ok(m)
    })();
    if answer.is_err() {
        relay.end_session(id);
    }
    answer.map(Some)
}

/// A jailed session's arguments after its binary.
#[cfg(target_os = "freebsd")]
fn jail_args(grace: Option<u64>) -> Vec<String> {
    let mut args = vec!["--mint-fd".to_string(), "3".to_string()];
    if let Some(g) = grace {
        args.extend(["--grace".to_string(), g.to_string()]);
    }
    args
}

/// One mint request on the mint socket: the answer line for the caller.
fn mint_request(relay: &Arc<Relay>, cfg: &Config, next: &mut u32, mut c: UnixStream) -> String {
    let refuse = |why: &str| format!("FENESTRA_REFUSED {why}\n");
    // Who is asking is the kernel's answer, and must be the seat's user.
    let Ok((uid, _)) = portcullis_peer::uid_gid(c.as_raw_fd()) else { return refuse("no peer credentials") };
    let Some(user) = portcullis_peer::username(uid) else { return refuse("no such user") };
    if portcullis_peer::seat::active_at(&cfg.seat).as_deref() != Some(user.as_str()) {
        return refuse("not the user of the seat session");
    }
    if c.set_read_timeout(Some(MINT_TIMEOUT)).is_err() {
        return refuse("socket");
    }
    let mut line = String::new();
    if std::io::BufReader::new((&mut c).take(MAX_REQUEST as u64 + 1)).read_line(&mut line).is_err() || line.len() > MAX_REQUEST || !line.ends_with('\n') {
        return refuse("a request that is not one short line");
    }
    let w: Vec<&str> = line.split_ascii_whitespace().collect();
    let (key, take_over) = match w.as_slice() {
        ["MINT", k] => (*k, false),
        ["MINT", k, "take-over"] => (*k, true),
        _ => return refuse("not a mint request"),
    };
    let Ok(key) = mint::unhex(key) else { return refuse("a viewer key that is not hex") };
    if key.is_empty() || key.len() > 128 {
        return refuse("a viewer key of the wrong size");
    }
    let held: Vec<u32> = relay.table.lock().unwrap_or_else(|e| e.into_inner()).sessions.keys().copied().collect();
    if held.len() >= MAX_SESSIONS {
        if !take_over {
            return refuse("a viewer already has this desktop (take-over ends that session)");
        }
        for id in held {
            eprintln!("fenestrad: session {id} taken over by {user}");
            relay.end_session(id);
        }
    }
    *next += 1;
    match start_session(relay, cfg, *next, MintBy::Key(key)) {
        Ok(Some(m)) => {
            eprintln!("fenestrad: minted session {} for {user} on {}", *next, m.addr);
            format!("FENESTRA_MINT {}\n", m.line())
        }
        Ok(None) => refuse("no mint"),
        Err(e) => {
            eprintln!("fenestrad: session {}: {e}", *next);
            refuse("the session did not start")
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = arg(&args, "--frescod").unwrap_or_else(|| die("--frescod SOCK is needed"));
    // Before any thread: getpwnam, and the fork for the root side.
    let user = arg(&args, "--user").map(|n| privsep::lookup(&n).unwrap_or_else(|e| die(e)));
    // SAFETY: a getter of this process's uid.
    if user.is_some() && unsafe { libc::geteuid() } != 0 {
        die("--user needs fenestrad started as root");
    }
    let grace: Option<u64> = arg(&args, "--grace").map(|s| s.parse().unwrap_or_else(|_| die("--grace SECONDS")));
    #[cfg(target_os = "freebsd")]
    let jaild_at = arg(&args, "--jaild").map(|sock| {
        let lan = args.iter().position(|a| a == "--lan").and_then(|i| Some((args.get(i + 1)?.clone(), args.get(i + 2)?.clone())));
        let (iface, subnet) = lan.unwrap_or_else(|| die("--jaild needs --lan IFACE SUBNET (where viewers come from)"));
        (sock, iface, subnet)
    });
    // The root side first, so it holds nothing but what it is given here.
    #[cfg(target_os = "freebsd")]
    let jails = jaild_at.map(|(sock, iface, subnet)| match &user {
        Some(_) => Jailing::Root(Arc::new(privsep::jailer::split(move || jail::Jails::new(&sock, &iface, &subnet), jail_args(grace)))),
        None => Jailing::Here(jail::Jails::new(&sock, &iface, &subnet)),
    });
    // The mint socket's directory is root's: bound before the drop.
    let mint_listener = arg(&args, "--mint-sock").map(|path| {
        // A fresh socket in fenestrad's own directory; any local user
        // may connect, and the uid check in mint_request decides.
        let l = UnixListener::bind(&path).unwrap_or_else(|e| die(format!("{path}: {e}")));
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap_or_else(|e| die(format!("{path}: {e}")));
        (path, l)
    });
    if let Some(u) = &user {
        privsep::drop_to(u).unwrap_or_else(|e| die(format!("dropping to {}: {e}", u.name)));
        eprintln!("fenestrad: running as {} (uid {})", u.name, u.uid);
    }
    // After the drop: frescod sees this user's uid on the link (it must be
    // in its FRESCOD_FENESTRA_UIDS), never root's.
    let f = UnixStream::connect(&sock).unwrap_or_else(|e| die(format!("{sock}: {e}")));
    let cfg = Config {
        bin: arg(&args, "--session").unwrap_or_else(|| die("--session BIN is needed")),
        bind: arg(&args, "--bind"),
        public_ip: arg(&args, "--public-ip").map(|s| s.parse().unwrap_or_else(|_| die("--public-ip IP"))),
        seat: arg(&args, "--seat").unwrap_or_else(|| portcullis_peer::seat::ACTIVE_SESSION.into()),
        grace,
        #[cfg(target_os = "freebsd")]
        jails,
    };
    #[cfg(not(target_os = "freebsd"))]
    if arg(&args, "--jaild").is_some() {
        die("--jaild is for Atrium only");
    }

    let fr = f.try_clone().unwrap_or_else(|e| die(e));
    let relay = Arc::new(Relay { table: Mutex::new(Table::default()), frescod: Mutex::new(f) });
    {
        let relay = relay.clone();
        std::thread::spawn(move || {
            let why = relay.from_frescod(fr);
            die(why);
        });
    }

    match (mint_listener, arg(&args, "--mint-dir")) {
        (Some((path, l)), None) => {
            eprintln!("fenestrad: taking mints on {path}");
            let mut next = 0u32;
            // One request at a time, each bounded by MINT_TIMEOUT.
            for c in l.incoming() {
                let Ok(c) = c else { continue };
                let Ok(mut out) = c.try_clone() else { continue };
                let answer = mint_request(&relay, &cfg, &mut next, c);
                if answer.starts_with("FENESTRA_REFUSED") {
                    eprint!("fenestrad: mint {answer}");
                }
                let _ = out.write_all(answer.as_bytes());
            }
        }
        (None, Some(dir)) => {
            if let Err(e) = start_session(&relay, &cfg, 1, MintBy::Dir(dir)) {
                die(e);
            }
            while !relay.table.lock().unwrap_or_else(|e| e.into_inner()).sessions.is_empty() {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        _ => die("one of --mint-sock PATH or --mint-dir DIR is needed"),
    }
}

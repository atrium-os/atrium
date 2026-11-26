//! `fenestrad --frescod SOCK --session BIN --mint-dir DIR [--bind ADDR]`
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
//! queue fills (it stopped reading) is ended, never waited on. In R2
//! fenestrad runs one session, from the development mint in `--mint-dir`,
//! and exits when it ends.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use fenestra_link::Rec;

/// Records queued for one session before it counts as stuck.
const SESSION_QUEUE: usize = 4096;
/// Wants one session may have outstanding.
const MAX_WANTS: usize = 256;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestrad: {why}");
    std::process::exit(1)
}

struct Session {
    tx: SyncSender<Rec>,
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
                t.sessions.remove(&id);
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
                Rec::Changed { .. } | Rec::Layout { .. } => {
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
            let Some(s) = t.sessions.get_mut(&id) else { return "ended".into() };
            match rec {
                Rec::Session { on } => {
                    s.on = on;
                    self.sync_on(&mut t);
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
        t.sessions.remove(&id);
        t.tickets.retain(|_, (s, _)| *s != id);
        self.sync_on(&mut t);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = arg(&args, "--frescod").unwrap_or_else(|| die("--frescod SOCK is needed"));
    let bin = arg(&args, "--session").unwrap_or_else(|| die("--session BIN is needed"));
    let mint = arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed"));
    let bind = arg(&args, "--bind");

    let f = UnixStream::connect(&sock).unwrap_or_else(|e| die(format!("{sock}: {e}")));
    let fr = f.try_clone().unwrap_or_else(|e| die(e));
    let relay = Arc::new(Relay { table: Mutex::new(Table::default()), frescod: Mutex::new(f) });
    {
        let relay = relay.clone();
        std::thread::spawn(move || {
            let why = relay.from_frescod(fr);
            die(why);
        });
    }

    // The one session of R2.
    let id = 1u32;
    let (ours, theirs) = UnixStream::pair().unwrap_or_else(|e| die(e));
    let mut cmd = Command::new(&bin);
    cmd.arg("--mint-dir").arg(&mint);
    if let Some(b) = &bind {
        cmd.arg("--bind").arg(b);
    }
    let mut child = cmd
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(format!("{bin}: {e}")));
    let (tx, rx) = mpsc::sync_channel::<Rec>(SESSION_QUEUE);
    relay.table.lock().unwrap_or_else(|e| e.into_inner()).sessions.insert(id, Session { tx, on: false, wants: 0 });
    let mut w = ours.try_clone().unwrap_or_else(|e| die(e));
    std::thread::spawn(move || {
        for rec in rx.iter() {
            if fenestra_link::write(&mut w, &rec).is_err() {
                let _ = w.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
        let _ = w.shutdown(std::net::Shutdown::Both);
    });
    eprintln!("fenestrad: session {id} started ({bin})");
    let why = relay.from_session(id, ours);
    eprintln!("fenestrad: session {id} ended: {why}");
    relay.end_session(id);
    let _ = child.kill();
    let _ = child.wait();
}

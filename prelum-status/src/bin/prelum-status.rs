//! prelum-status: the Forum status item while a job prints, and Praeco's
//! notices (printing.md §6.6). A session component, one per person; prelumd
//! knows it by its app id in the launch registry and gives it the person's
//! jobs (`WATCH`), their printers' names, and Cancel.
//!
//! Environment: `PRELUM_SOCKET` (/atrium/sockets/print/print.sock),
//! `PRAECO_SOCKET` (/var/run/atrium/praecod.sock), `FORUM_STATUS_SOCKET`
//! (forum_ctl's default), `PRELUM_OFFLINE_AFTER` (seconds before an offline
//! printer is told, 120).

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use forum_ctl::{FromStatus, StatusValue, ToStatus};
use prelum_proto::print::{client::Client, Job};
use prelum_status::{panel, Action, Key, Notices, ICON};

enum Ev {
    Jobs(Vec<Job>, bool),
    /// prelumd is away (why).
    Lost(String),
    /// A Cancel pressed in the panel.
    Cancel(u64),
    /// The bar's connection closed or refused.
    BarGone(String),
}

fn print_sock() -> PathBuf {
    std::env::var_os("PRELUM_SOCKET").map(PathBuf::from).unwrap_or_else(|| "/atrium/sockets/print/print.sock".into())
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// prelumd's jobs on every change; reconnects when it goes.
fn follow(tx: Sender<Ev>) {
    std::thread::spawn(move || loop {
        let why = match Client::connect(&print_sock()) {
            Ok(mut c) => {
                let mut generation = 0;
                loop {
                    match c.watch(generation) {
                        Ok(w) => {
                            generation = w.generation;
                            if tx.send(Ev::Jobs(w.jobs, c.printed_notice().unwrap_or(false))).is_err() {
                                return;
                            }
                        }
                        Err(e) => break e.to_string(),
                    }
                }
            }
            Err(e) => format!("prelumd: {e}"),
        };
        if tx.send(Ev::Lost(why)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
    });
}

/// The status item's connection: what is set goes out on it; presses come
/// back as `Ev::Cancel`.
struct Bar {
    sock: Option<UnixStream>,
    shown: bool,
    retry_at: Instant,
}

impl Bar {
    fn send(&mut self, m: &ToStatus, tx: &Sender<Ev>) {
        if self.sock.is_none() && Instant::now() >= self.retry_at {
            match UnixStream::connect(forum_ctl::status_socket_path()) {
                Ok(s) => {
                    if let Ok(mut r) = s.try_clone() {
                        let tx = tx.clone();
                        std::thread::spawn(move || loop {
                            let m = forum_ctl::read_frame(&mut r).ok().and_then(|f| forum_ctl::decode::<FromStatus>(&f).ok());
                            let ev = match m {
                                Some(FromStatus::Changed { id, value: StatusValue::Pressed }) => Ev::Cancel(id as u64),
                                Some(FromStatus::Denied { message }) => Ev::BarGone(format!("refused: {message}")),
                                Some(_) => continue,
                                None => Ev::BarGone("closed".into()),
                            };
                            let end = matches!(ev, Ev::BarGone(_));
                            if tx.send(ev).is_err() || end {
                                return;
                            }
                        });
                    }
                    self.sock = Some(s);
                    self.shown = false;
                }
                Err(e) => {
                    eprintln!("prelum-status: the bar: {e}");
                    self.retry_at = Instant::now() + Duration::from_secs(10);
                    return;
                }
            }
        }
        let Some(s) = self.sock.as_mut() else { return };
        let ok = forum_ctl::encode(m).map_err(std::io::Error::other).and_then(|b| forum_ctl::write_frame(s, &b));
        match ok {
            Ok(()) => self.shown = matches!(m, ToStatus::Set { .. }),
            Err(e) => {
                eprintln!("prelum-status: the bar: {e}");
                self.sock = None;
            }
        }
    }
}

fn main() {
    let praeco = PathBuf::from(std::env::var("PRAECO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/praecod.sock".into()));
    let offline_after = std::env::var("PRELUM_OFFLINE_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let (tx, rx) = channel();
    follow(tx.clone());
    let mut notices = Notices::new(offline_after);
    let mut posted: HashMap<Key, u64> = HashMap::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut jobs: Vec<Job> = Vec::new();
    let mut bar = Bar { sock: None, shown: false, retry_at: Instant::now() };
    let mut last_panel = None;
    eprintln!("prelum-status: up; offline is told after {offline_after} s");
    loop {
        let wait = notices.next_due(&jobs, now()).map(|s| Duration::from_secs(s.max(1))).unwrap_or(Duration::from_secs(60));
        match rx.recv_timeout(wait) {
            Ok(Ev::Jobs(j, printed_notice)) => {
                notices.printed_notice = printed_notice;
                // A printer not named yet: ask for the names again.
                if j.iter().any(|j| !names.contains_key(&j.printer)) {
                    if let Ok(ps) = Client::connect(&print_sock()).map_err(|e| e.to_string()).and_then(|mut c| c.printers().map_err(|e| e.to_string())) {
                        names = ps.into_iter().map(|p| (p.id, p.name)).collect();
                    }
                }
                jobs = j;
            }
            Ok(Ev::Lost(why)) => {
                eprintln!("prelum-status: {why}");
                continue;
            }
            Ok(Ev::Cancel(job)) => {
                let r = Client::connect(&print_sock()).map_err(|e| e.to_string()).and_then(|mut c| c.cancel(job).map_err(|e| e.to_string()));
                match r {
                    Ok(_) => eprintln!("prelum-status: cancelled job {job}"),
                    Err(e) => eprintln!("prelum-status: cancel job {job}: {e}"),
                }
                continue;
            }
            Ok(Ev::BarGone(why)) => {
                eprintln!("prelum-status: the bar: {why}");
                bar.sock = None;
                bar.retry_at = Instant::now() + Duration::from_secs(10);
                last_panel = None;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for a in notices.update(&jobs, &names, now()) {
            match a {
                Action::Post { key, urgency, title, body } => match praeco::client::post(&praeco, urgency, &title, &body) {
                    Ok(id) => {
                        eprintln!("prelum-status: posted {id} ({urgency}): {title}");
                        posted.insert(key, id);
                    }
                    Err(e) => eprintln!("prelum-status: praecod: {e}: {title}"),
                },
                Action::Withdraw(key) => {
                    if let Some(id) = posted.remove(&key) {
                        match praeco::client::withdraw(&praeco, id) {
                            Ok(()) => eprintln!("prelum-status: withdrew {id}"),
                            Err(e) => eprintln!("prelum-status: praecod: withdraw {id}: {e}"),
                        }
                    }
                }
            }
        }
        // The item: up while a job is active, the same panel not sent twice.
        let p = panel(&jobs, &names);
        if p != last_panel || (p.is_some() && bar.sock.is_none()) {
            match &p {
                Some((tooltip, content)) => {
                    let m = ToStatus::Set { glyph: '\u{e0ce}', svg: Some(ICON.into()), tint: None, tooltip: tooltip.clone(), content: content.clone() };
                    bar.send(&m, &tx);
                    if bar.shown {
                        eprintln!("prelum-status: item: {tooltip}");
                    }
                }
                None if bar.shown => {
                    bar.send(&ToStatus::Remove, &tx);
                    eprintln!("prelum-status: item removed");
                }
                None => {}
            }
            last_panel = if bar.sock.is_some() { p } else { None };
        }
    }
}

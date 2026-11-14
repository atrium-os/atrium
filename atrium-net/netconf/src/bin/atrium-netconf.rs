//! atrium-netconf: network settings for apps (docs/spec/atrium-net.md §7.1).
//!
//! Serves `CLASS_NETCONF` on one socket per capability under `--dir`
//! (`/atrium/sockets/<capability>/<capability>.sock`) and turns each request
//! into atrium-net control lines. It holds two connections to atrium-net's
//! control socket, one for requests and one subscribed to its events, opened
//! as root; then it drops to its own uid and enters capability mode, so app
//! input is decoded by a process that can do nothing but talk to atrium-net.
//! If atrium-net goes away it exits (the service manager starts it again).
//!
//!   atrium-netconf [--ctl PATH] [--dir DIR] [--uid N] [--capsicum] [-v]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use atrium_netconf::server::{serve, Backend};
use atrium_netconf::{lines, status, Fail, Grant, Iface, Join, Nearby, Profile, Res, Routes, Status};
use aura_ipc::{Got, Line, Reader};

fn die(s: &str) -> ! {
    eprintln!("atrium-netconf: {s}");
    std::process::exit(1)
}

struct Peer {
    s: UnixStream,
    r: Reader,
    pending: Vec<Line>,
}

impl Peer {
    fn connect(p: &str) -> Peer {
        let s = UnixStream::connect(p).unwrap_or_else(|e| die(&format!("{p}: {e}")));
        Peer { s, r: Reader::default(), pending: Vec::new() }
    }

    /// The next line; atrium-net gone ends the process.
    fn next(&mut self) -> Option<Line> {
        loop {
            if !self.pending.is_empty() {
                return Some(self.pending.remove(0));
            }
            match self.r.fill(&mut self.s) {
                Ok(v) => self.pending.extend(v.into_iter().filter_map(|g| if let Got::Line(l) = g { Some(l) } else { None })),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return None,
                Err(_) => die("atrium-net closed its control socket"),
            }
        }
    }
}

struct Front {
    v: bool,
    calls: Mutex<Peer>,
    /// Everyone waiting for event lines (a join, a scan).
    waiters: Mutex<Vec<mpsc::Sender<Line>>>,
    subs: Mutex<Vec<mpsc::Sender<Status>>>,
    /// aura-mlme's last signal reading and when.
    rssi: Mutex<Option<(i32, Instant)>>,
    kick: Mutex<mpsc::Sender<()>>,
}

const RSSI_STALE: Duration = Duration::from_secs(15);

impl Front {
    fn log(&self, s: &str) {
        if self.v {
            eprintln!("atrium-netconf: {s}");
        }
    }

    /// A request to atrium-net: its reply lines, the last `ok` left out.
    fn call(&self, l: &Line) -> Res<Vec<Line>> {
        let mut p = self.calls.lock().unwrap();
        if l.send(&mut p.s).is_err() {
            die("atrium-net closed its control socket");
        }
        let end = Instant::now() + Duration::from_secs(15);
        let mut v = Vec::new();
        while Instant::now() < end {
            let Some(x) = p.next() else { continue };
            match x.verb.as_str() {
                "ok" => return Ok(v),
                "error" => {
                    let why = x.get("why").unwrap_or("?");
                    let st = if why == "no-wifi" { status::NO_WIFI } else { status::FAILED };
                    return Err(Fail::new(st, why));
                }
                _ => v.push(x),
            }
        }
        Err(Fail::new(status::UNAVAILABLE, "atrium-net did not answer"))
    }

    fn wait_lines(&self) -> mpsc::Receiver<Line> {
        let (tx, rx) = mpsc::channel();
        self.waiters.lock().unwrap().push(tx);
        rx
    }

    /// One event line from atrium-net.
    fn event(&self, l: Line) {
        if l.verb == "status" {
            if let Some(r) = l.num::<i32>("rssi") {
                *self.rssi.lock().unwrap() = Some((r, Instant::now()));
            }
        }
        if l.verb == "event" && l.get("what") == Some("roam-scan") {
            if let Some(r) = l.num::<i32>("rssi") {
                *self.rssi.lock().unwrap() = Some((r, Instant::now()));
            }
        }
        if l.verb == "state" && l.get("state") != Some("authorized") {
            *self.rssi.lock().unwrap() = None;
        }
        self.waiters.lock().unwrap().retain(|w| w.send(l.clone()).is_ok());
        let _ = self.kick.lock().unwrap().send(());
    }

    fn rssi(&self) -> Option<i32> {
        self.rssi.lock().unwrap().filter(|(_, t)| t.elapsed() < RSSI_STALE * 2).map(|(r, _)| r)
    }

    /// Ask aura-mlme for a fresh signal reading if ours is old, and wait a
    /// moment for it.
    fn refresh_rssi(&self) {
        if self.rssi.lock().unwrap().is_some_and(|(_, t)| t.elapsed() < RSSI_STALE) {
            return;
        }
        let rx = self.wait_lines();
        if self.call(&Line::new("wifi-status")).is_err() {
            return;
        }
        let end = Instant::now() + Duration::from_millis(1500);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(l) if l.verb == "status" => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    fn status_now(&self) -> Res<Status> {
        let v = self.call(&Line::new("status"))?;
        let s = lines::status(&v, None);
        if s.link == atrium_netconf::Link::Wifi {
            self.refresh_rssi();
            return Ok(lines::status(&v, self.rssi()));
        }
        Ok(s)
    }

    /// Subscribers hear the status after every event (and every 10 s).
    fn watch(self: Arc<Self>, kicks: mpsc::Receiver<()>) {
        let mut last: Option<Status> = None;
        loop {
            let _ = kicks.recv_timeout(Duration::from_secs(10));
            while kicks.try_recv().is_ok() {}
            if self.subs.lock().unwrap().is_empty() {
                last = None;
                continue;
            }
            let Ok(s) = self.status_now() else { continue };
            if last.as_ref() == Some(&s) {
                continue;
            }
            self.subs.lock().unwrap().retain(|t| t.send(s.clone()).is_ok());
            last = Some(s);
        }
    }

    fn known(&self) -> Vec<Vec<u8>> {
        self.profiles().map(|v| v.into_iter().map(|p| p.ssid).collect()).unwrap_or_default()
    }
}

impl Backend for Front {
    fn status(&self) -> Res<Status> {
        self.status_now()
    }

    fn subscribe(&self) -> Res<mpsc::Receiver<Status>> {
        let s = self.status_now()?;
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(s);
        self.subs.lock().unwrap().push(tx);
        Ok(rx)
    }

    fn interfaces(&self) -> Res<Vec<Iface>> {
        Ok(self.call(&Line::new("status"))?.iter().filter_map(lines::iface).collect())
    }

    fn routes(&self) -> Res<Routes> {
        Ok(lines::routes(&self.call(&Line::new("status"))?))
    }

    fn profiles(&self) -> Res<Vec<Profile>> {
        Ok(self.call(&Line::new("wifi-list"))?.iter().filter_map(lines::profile).collect())
    }

    fn join(&self, j: Join) -> Res<()> {
        self.log(&format!("join {:?}", String::from_utf8_lossy(&j.ssid)));
        if j.passphrase.is_some() || j.open {
            let mut r = Line::new("wifi-remember").with_hex("ssid", &j.ssid).with("open", j.open as u8);
            if let Some(pw) = &j.passphrase {
                r = r.with_hex("secret", pw.as_bytes());
            }
            self.call(&r)?;
        }
        let rx = self.wait_lines();
        let mut l = Line::new("wifi-join").with_hex("ssid", &j.ssid);
        if j.open {
            l = l.with("security", "open");
        }
        self.call(&l)?;
        let end = Instant::now() + Duration::from_secs(60);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(x) = rx.recv_timeout(left) else { break };
            match (x.verb.as_str(), x.get("state")) {
                ("state", Some("authorized")) => return Ok(()),
                ("state", Some("failed")) => return Err(Fail::new(status::FAILED, x.get("why").unwrap_or("failed"))),
                _ => {}
            }
        }
        Err(Fail::new(status::FAILED, "timed out"))
    }

    fn leave(&self) -> Res<()> {
        self.call(&Line::new("wifi-leave")).map(|_| ())
    }

    fn forget(&self, ssid: Vec<u8>) -> Res<()> {
        self.call(&Line::new("wifi-forget").with_hex("ssid", &ssid)).map(|_| ())
    }

    fn set_profile(&self, p: Profile) -> Res<()> {
        self.call(&lines::remember(&p)).map(|_| ())
    }

    fn nearby(&self) -> Res<Vec<Nearby>> {
        let known = self.known();
        let rx = self.wait_lines();
        self.call(&Line::new("wifi-scan"))?;
        let mut bss = Vec::new();
        let end = Instant::now() + Duration::from_secs(30);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(x) = rx.recv_timeout(left) else { break };
            match x.verb.as_str() {
                "bss" if bss.len() < 512 => bss.push(x),
                "scan-done" if x.get("for").is_none() => return Ok(lines::nearby(&bss, &known)),
                "state" if x.get("state") == Some("failed") && x.get("why") == Some("scan") => {
                    return Err(Fail::new(status::FAILED, "scan failed"));
                }
                _ => {}
            }
        }
        Err(Fail::new(status::FAILED, "scan timed out"))
    }

    fn set_country(&self, cc: String) -> Res<()> {
        self.call(&Line::new("country").with("cc", cc)).map(|_| ())
    }
}

fn listen(dir: &Path, g: Grant) -> UnixListener {
    let d = dir.join(g.name());
    std::fs::create_dir_all(&d).unwrap_or_else(|e| die(&format!("{}: {e}", d.display())));
    let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
    let p: PathBuf = d.join(format!("{}.sock", g.name()));
    let _ = std::fs::remove_file(&p);
    let l = UnixListener::bind(&p).unwrap_or_else(|e| die(&format!("{}: {e}", p.display())));
    // Who reaches the socket is decided by which jails get its directory.
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666));
    l
}

fn drop_to(uid: u32) {
    // SAFETY: plain system calls on integers.
    unsafe {
        let g = uid as libc::gid_t;
        if libc::setgroups(1, &g) != 0 || libc::setgid(g) != 0 || libc::setuid(uid) != 0 {
            die("could not drop to its own uid");
        }
    }
}

#[cfg(target_os = "freebsd")]
fn cap_enter() {
    // SAFETY: no arguments; after it only held descriptors work.
    if unsafe { libc::cap_enter() } != 0 {
        die("cap_enter failed");
    }
}

#[cfg(not(target_os = "freebsd"))]
fn cap_enter() {
    die("--capsicum needs FreeBSD");
}

fn main() {
    let mut ctl = "/atrium/sockets/atrium-net/ctl.sock".to_string();
    let mut dir = PathBuf::from("/atrium/sockets");
    let (mut uid, mut capsicum, mut v) = (None, false, false);
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "--ctl" => ctl = a.next().unwrap_or_else(|| die("--ctl PATH")),
            "--dir" => dir = a.next().map(PathBuf::from).unwrap_or_else(|| die("--dir DIR")),
            "--uid" => uid = Some(a.next().and_then(|n| n.parse().ok()).unwrap_or_else(|| die("--uid N"))),
            "--capsicum" => capsicum = true,
            "-v" => v = true,
            _ => die("usage: atrium-netconf [--ctl PATH] [--dir DIR] [--uid N] [--capsicum] [-v]"),
        }
    }
    let listeners: Vec<(Grant, UnixListener)> = Grant::ALL.iter().map(|&g| (g, listen(&dir, g))).collect();
    let calls = Peer::connect(&ctl);
    let _ = calls.s.set_read_timeout(Some(Duration::from_secs(1)));
    let mut events = Peer::connect(&ctl);
    if Line::new("subscribe").send(&mut events.s).is_err() {
        die("subscribe");
    }
    if let Some(u) = uid {
        drop_to(u);
    }
    if capsicum {
        cap_enter();
    }
    let (kick, kicks) = mpsc::channel();
    let front = Arc::new(Front {
        v,
        calls: Mutex::new(calls),
        waiters: Mutex::new(Vec::new()),
        subs: Mutex::new(Vec::new()),
        rssi: Mutex::new(None),
        kick: Mutex::new(kick),
    });
    front.log(&format!("serving {} for {ctl}{}", dir.display(), if capsicum { ", in capability mode" } else { "" }));
    let f = front.clone();
    std::thread::spawn(move || loop {
        if let Some(l) = events.next() {
            f.event(l);
        }
    });
    let f = front.clone();
    std::thread::spawn(move || f.watch(kicks));
    let mut hs = Vec::new();
    for (g, l) in listeners {
        let f = front.clone();
        hs.push(std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { continue };
                let f = f.clone();
                std::thread::spawn(move || {
                    if let Ok(c) = aqueduct::Connection::wrap(s) {
                        let _ = serve(c, g, &*f);
                    }
                });
            }
        }));
    }
    for h in hs {
        let _ = h.join();
    }
}

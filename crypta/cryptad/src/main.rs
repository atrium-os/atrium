//! `atrium-cryptad`: Crypta's manager (docs/spec/vpn.md §3).
//!
//! It keeps WireGuard profiles (no secrets: the keys are Arca items in the
//! machine store, used through a helper running as `_crypta-key`), and for
//! each tunnel the user turns on:
//!
//! 1. asks atrium-net for a tun (`tunnel-open`; atrium-net keeps one too,
//!    so the tunnel's routes outlive an engine that dies),
//! 2. proposes the tunnel (`tunnel-routes`, `tunnel-apply`): atrium-net
//!    checks it, loads the `atrium/vpn` pf anchor first (block when not
//!    connected), then routes, the gateway's host route and DNS,
//! 3. has jaild start the engine (`atrium-crypta-wg`) in a jail of its own
//!    with the tun as a descriptor, sends it `Start`, and answers its key
//!    requests with Arca's `DH_X25519` on that profile's key only,
//! 4. starts the engine again if it dies (1 s, doubling to 30 s); the
//!    tunnel stays applied meanwhile, so nothing leaks while it is down.
//!
//! It runs as root, like arca-courierd, because jaild serves only root. It
//! has no network of its own and holds no key.
//!
//! Control (root only, one line per request, `ok` or `error why=..` last):
//!
//! - `import name=N [block=0|1]` with the `.conf` as a descriptor
//! - `up name=N`, `down name=N`, `delete name=N`, `set name=N block=0|1`
//! - `list`: one `tunnel` line per profile
//!
//! `atrium-cryptad ctl VERB [k=v ...]` sends one (with stdin as the file for
//! `import`) and prints the answer.

mod fd;
mod key;
mod net;
mod profile;
#[cfg(target_os = "freebsd")]
mod jail;

use aura_ipc::Line;
use crypta_proto::engine::{self, DhShared, FromEngine, Start, ToEngine};
use profile::Profile;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const KEY_UID: u32 = 65047;
const MAX_TUNNELS: usize = 8;
const BACKOFF_MAX_S: u64 = 30;

struct Opts {
    sock: String,
    state: PathBuf,
    net_sock: String,
    jaild: String,
    arca: String,
    key_uid: u32,
    verbose: bool,
}

fn die(s: &str) -> ! {
    eprintln!("atrium-cryptad: {s}");
    std::process::exit(2)
}

fn opts(mut a: impl Iterator<Item = String>) -> Opts {
    let mut o = Opts {
        sock: "/var/run/atrium/crypta.sock".into(),
        state: "/var/db/crypta".into(),
        net_sock: "/atrium/sockets/atrium-net/ctl.sock".into(),
        jaild: "/var/run/atrium/jaild.sock".into(),
        arca: "/atrium/sockets/arca/arcad.sock".into(),
        key_uid: KEY_UID,
        verbose: false,
    };
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_else(|| die(&format!("{k} needs a value")));
        match k.as_str() {
            "-v" => o.verbose = true,
            "--sock" => o.sock = v(),
            "--state" => o.state = v().into(),
            "--net-sock" => o.net_sock = v(),
            "--jaild" => o.jaild = v(),
            "--arca" => o.arca = v(),
            "--key-uid" => o.key_uid = v().parse().unwrap_or_else(|_| die("--key-uid")),
            _ => die("usage: atrium-cryptad [-v] [--sock P] [--state DIR] [--net-sock P] [--jaild P] [--arca P] [--key-uid N] | ctl VERB [k=v ...] | --arca-key"),
        }
    }
    o
}

/// A running engine.
struct Live {
    uid: u32,
    sock: UnixStream,
    rd: engine::Reader,
    pd: OwnedFd,
    started: Instant,
}

struct Run {
    p: Profile,
    iface: String,
    tun: OwnedFd,
    engine: Option<Live>,
    restart_at: Option<Instant>,
    backoff_s: u64,
    handshake: bool,
    stats: (u64, u64, u64),
    starts: u32,
    last_error: String,
}

struct CtlConn {
    s: UnixStream,
    buf: Vec<u8>,
    fds: Vec<OwnedFd>,
}

struct D {
    o: Opts,
    keys: key::Keys,
    net: Option<net::NetCtl>,
    runs: Vec<Run>,
    /// Engine uids whose jails still need taking apart.
    janitor: Vec<u32>,
}

fn hex32(l: &Line, k: &str) -> Option<[u8; 32]> {
    l.hex(k)?.try_into().ok()
}

impl D {
    fn log(&self, s: &str) {
        if self.o.verbose {
            eprintln!("atrium-cryptad: {s}");
        }
    }

    fn net(&mut self) -> Result<&mut net::NetCtl, String> {
        if self.net.is_none() {
            self.net = Some(net::NetCtl::connect(&self.o.net_sock).map_err(|e| format!("atrium-net: {e}"))?);
        }
        Ok(self.net.as_mut().unwrap())
    }

    fn net_call(&mut self, l: &Line) -> Result<net::Answer, String> {
        let r = self.net()?.call(l);
        if let Err(e) = &r {
            if e.starts_with("atrium-net") {
                // The connection is no good any more; the next call makes another.
                self.net = None;
            }
        }
        r
    }

    fn import(&mut self, l: &Line, file: Option<OwnedFd>) -> Result<Vec<Line>, String> {
        let name = l.get("name").filter(|n| profile::valid_name(n)).ok_or("bad-name")?.to_string();
        if Profile::load(&self.o.state, &name).is_some() {
            return Err("exists".into());
        }
        let file = file.ok_or("no-file")?;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        std::fs::File::from(file).take(crypta_proto::conf::MAX_FILE as u64 + 1).read_to_end(&mut bytes).map_err(|_| "unreadable")?;
        let c = crypta_proto::conf::parse(&bytes).map_err(|e| {
            eprintln!("atrium-cryptad: import {name}: {e}");
            match e {
                crypta_proto::conf::Error::ManyPeers => "many-peers".to_string(),
                _ => "bad-file".to_string(),
            }
        })?;
        drop(bytes);
        let k = self.keys.call(&Line::new("import").with("account", &name).with_hex("key", &*c.private_key)).map_err(|e| e.to_string())?;
        let (Some(id), Some(public)) = (k.hex("id").and_then(|i| <[u8; 16]>::try_from(i).ok()), hex32(&k, "pub")) else {
            return Err(format!("arca-{}", k.get("status").unwrap_or("?")));
        };
        let mut items = vec![id];
        let psk = match &c.psk {
            Some(p) => {
                let r = self.keys.call(&Line::new("psk").with("account", format!("{name}-psk")).with_hex("value", &**p)).map_err(|e| e.to_string())?;
                match r.hex("id").and_then(|i| <[u8; 16]>::try_from(i).ok()) {
                    Some(i) => {
                        items.push(i);
                        Some(i)
                    }
                    None => {
                        self.forget(&items);
                        return Err(format!("arca-{}", r.get("status").unwrap_or("?")));
                    }
                }
            }
            None => None,
        };
        let mut p = match Profile::from_conf(&name, &c, id, public, psk) {
            Ok(p) => p,
            Err(u) => {
                self.forget(&items);
                return Err(u.word().into());
            }
        };
        if let Some(b) = l.get("block") {
            p.block = b == "1";
        }
        if let Err(e) = p.save(&self.o.state) {
            self.forget(&items);
            return Err(format!("save: {e}"));
        }
        self.log(&format!("imported {name} ({} allowed, block {})", p.allowed.len(), p.block));
        let mut im = Line::new("imported").with("name", &name).with("public", crypta_proto::b64::encode(&public));
        if !c.ignored.is_empty() {
            im = im.with("ignored", c.ignored.join(","));
        }
        Ok(vec![im])
    }

    fn forget(&mut self, ids: &[[u8; 16]]) {
        for i in ids {
            let _ = self.keys.call(&Line::new("delete").with_hex("id", i));
        }
    }

    fn up(&mut self, name: &str) -> Result<(), String> {
        if self.runs.iter().any(|r| r.p.name == name) {
            return Ok(());
        }
        if self.runs.len() >= MAX_TUNNELS {
            return Err("too-many".into());
        }
        let p = Profile::load(&self.o.state, name).ok_or("no-profile")?;
        let mut a = self.net_call(&Line::new("tunnel-open").with("id", name))?;
        let iface = a.lines.iter().find(|l| l.verb == "tunnel").and_then(|l| l.get("iface")).ok_or("no-iface")?.to_string();
        let tun = a.fds.pop().ok_or("no-tun")?;
        for l in p.proposal().to_lines(name) {
            if let Err(e) = self.net_call(&l) {
                let _ = self.net_call(&Line::new("tunnel-close").with("id", name));
                return Err(e);
            }
        }
        self.log(&format!("{name}: applied on {iface}"));
        let mut r = Run { p, iface, tun, engine: None, restart_at: None, backoff_s: 1, handshake: false, stats: (0, 0, 0), starts: 0, last_error: String::new() };
        if let Err(e) = self.start_engine(&mut r) {
            eprintln!("atrium-cryptad: {name}: engine: {e}");
            r.last_error = e;
            r.restart_at = Some(Instant::now() + Duration::from_secs(r.backoff_s));
        }
        self.runs.push(r);
        Ok(())
    }

    fn pick_uid(&self) -> Option<u32> {
        #[cfg(target_os = "freebsd")]
        {
            (jail::UID_FIRST..=jail::UID_LAST).find(|u| !self.janitor.contains(u) && !self.runs.iter().any(|r| r.engine.as_ref().is_some_and(|e| e.uid == *u)))
        }
        #[cfg(not(target_os = "freebsd"))]
        {
            None
        }
    }

    fn start_engine(&mut self, r: &mut Run) -> Result<(), String> {
        let psk = match r.p.psk {
            Some(i) => {
                let a = self.keys.call(&Line::new("release").with_hex("id", &i)).map_err(|e| e.to_string())?;
                Some(hex32(&a, "value").ok_or_else(|| format!("psk: arca-{}", a.get("status").unwrap_or("?")))?)
            }
            None => None,
        };
        let uid = self.pick_uid().ok_or("no-uid")?;
        r.starts += 1;
        let mut e = launch(&self.o.jaild, uid, r)?;
        let start = ToEngine::Start(Start {
            our_public: r.p.public,
            peer_public: r.p.peer,
            psk,
            endpoint: r.p.endpoint,
            keepalive_s: r.p.keepalive,
            allowed: if r.p.full() { vec![crypta_proto::net::Prefix4::parse("0.0.0.0/0").unwrap()] } else { r.p.allowed.clone() },
        });
        let _ = e.sock.set_write_timeout(Some(Duration::from_secs(2)));
        if let Err(err) = engine::write(&mut e.sock, &start) {
            self.retire(e.uid);
            return Err(format!("start: {err}"));
        }
        self.log(&format!("{}: engine {} started on {}", r.p.name, e.uid, r.iface));
        r.engine = Some(e);
        r.handshake = false;
        Ok(())
    }

    fn retire(&mut self, uid: u32) {
        #[cfg(target_os = "freebsd")]
        jail::release(&self.o.jaild, uid);
        if !self.janitor.contains(&uid) {
            self.janitor.push(uid);
        }
    }

    fn stop_engine(&mut self, k: usize) {
        if let Some(mut e) = self.runs[k].engine.take() {
            let _ = engine::write(&mut e.sock, &ToEngine::Stop);
            // Closing the process descriptor ends the engine if Stop did not.
            drop(e.pd);
            self.retire(e.uid);
        }
    }

    fn down(&mut self, name: &str) -> Result<(), String> {
        let Some(k) = self.runs.iter().position(|r| r.p.name == name) else { return Ok(()) };
        self.stop_engine(k);
        self.runs.remove(k);
        self.net_call(&Line::new("tunnel-close").with("id", name)).map(|_| ())
    }

    fn delete(&mut self, name: &str) -> Result<(), String> {
        let p = Profile::load(&self.o.state, name).ok_or("no-profile")?;
        self.down(name)?;
        let mut ids = vec![p.key];
        ids.extend(p.psk);
        self.forget(&ids);
        std::fs::remove_file(Profile::path(&self.o.state, name)).map_err(|e| e.to_string())
    }

    fn list(&self) -> Vec<Line> {
        Profile::names(&self.o.state)
            .into_iter()
            .map(|n| {
                let mut l = Line::new("tunnel").with("name", &n);
                if let Some(r) = self.runs.iter().find(|r| r.p.name == n) {
                    l = l
                        .with("up", 1)
                        .with("iface", &r.iface)
                        .with("engine", r.engine.is_some() as u8)
                        .with("handshake", r.handshake as u8)
                        .with("starts", r.starts)
                        .with("rx", r.stats.0)
                        .with("tx", r.stats.1)
                        .with("dropped", r.stats.2);
                } else {
                    l = l.with("up", 0);
                }
                if let Some(p) = Profile::load(&self.o.state, &n) {
                    l = l.with("public", crypta_proto::b64::encode(&p.public)).with("endpoint", p.endpoint).with("full", p.full() as u8).with("block", p.block as u8);
                }
                l
            })
            .collect()
    }

    fn ctl(&mut self, l: &Line, file: Option<OwnedFd>) -> Vec<Line> {
        let name = l.get("name").filter(|n| profile::valid_name(n)).map(str::to_string);
        let res: Result<Vec<Line>, String> = match (l.verb.as_str(), name) {
            ("import", _) => self.import(l, file),
            ("up", Some(n)) => self.up(&n).map(|_| vec![]),
            ("down", Some(n)) => self.down(&n).map(|_| vec![]),
            ("delete", Some(n)) => self.delete(&n).map(|_| vec![]),
            // Takes effect at the next `up`.
            ("set", Some(n)) => {
                let flag = |k: &str| match l.get(k) {
                    None => Ok(None),
                    Some("0") => Ok(Some(false)),
                    Some("1") => Ok(Some(true)),
                    _ => Err(()),
                };
                match (Profile::load(&self.o.state, &n), flag("block"), flag("insecure")) {
                    (Some(mut p), Ok(b), Ok(i)) if b.is_some() || i.is_some() => {
                        p.block = b.unwrap_or(p.block);
                        p.insecure = i.unwrap_or(p.insecure);
                        p.save(&self.o.state).map(|_| vec![]).map_err(|e| e.to_string())
                    }
                    _ => Err("bad-request".into()),
                }
            }
            ("list", _) => Ok(self.list()),
            _ => Err("bad-request".into()),
        };
        match res {
            Ok(mut v) => {
                v.push(Line::new("ok"));
                v
            }
            Err(e) => vec![Line::new("error").with("why", e.replace(' ', "-"))],
        }
    }

    /// What an engine said.
    fn engine_frames(&mut self, k: usize) {
        let Some(e) = self.runs[k].engine.as_mut() else { return };
        let mut b = [0u8; 4096];
        let n = match e.sock.read(&mut b) {
            Ok(0) | Err(_) => {
                self.engine_gone(k, "closed its socket");
                return;
            }
            Ok(n) => n,
        };
        e.rd.push(&b[..n]);
        loop {
            let Some(e) = self.runs[k].engine.as_mut() else { return };
            let m = match e.rd.next::<FromEngine>() {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(_) => {
                    // An engine that sends what does not decode is not ours any more.
                    self.stop_engine(k);
                    self.engine_gone(k, "sent a bad frame");
                    return;
                }
            };
            match m {
                FromEngine::Dh { point } => {
                    let key = self.runs[k].p.key;
                    let shared = self
                        .keys
                        .call(&Line::new("dh").with_hex("id", &key).with_hex("point", &point))
                        .ok()
                        .and_then(|a| hex32(&a, "shared"))
                        .map(DhShared);
                    if shared.is_none() {
                        eprintln!("atrium-cryptad: {}: arcad refused DH", self.runs[k].p.name);
                    }
                    if let Some(e) = self.runs[k].engine.as_mut() {
                        let _ = engine::write(&mut e.sock, &ToEngine::Dh(shared));
                    }
                }
                FromEngine::Up => {
                    self.runs[k].handshake = true;
                    self.log(&format!("{}: connected", self.runs[k].p.name));
                }
                FromEngine::Down { why } => {
                    self.runs[k].handshake = false;
                    self.log(&format!("{}: not connected ({})", self.runs[k].p.name, why.chars().take(80).collect::<String>()));
                }
                FromEngine::Stats { rx, tx, dropped } => self.runs[k].stats = (rx, tx, dropped),
            }
        }
    }

    fn engine_gone(&mut self, k: usize, why: &str) {
        let r = &mut self.runs[k];
        let Some(e) = r.engine.take() else { return };
        // An engine that ran a while starts again at once; one that keeps
        // dying waits longer each time.
        if e.started.elapsed() > Duration::from_secs(60) {
            r.backoff_s = 1;
        }
        r.restart_at = Some(Instant::now() + Duration::from_secs(r.backoff_s));
        eprintln!("atrium-cryptad: {}: engine {} {why}; again in {} s", r.p.name, e.uid, r.backoff_s);
        r.backoff_s = (r.backoff_s * 2).min(BACKOFF_MAX_S);
        r.handshake = false;
        drop(e.pd);
        self.retire(e.uid);
    }

    fn tick(&mut self) {
        let now = Instant::now();
        for k in 0..self.runs.len() {
            if self.runs[k].engine.is_none() && self.runs[k].restart_at.is_some_and(|t| t <= now) {
                self.runs[k].restart_at = None;
                let mut r = std::mem::replace(&mut self.runs[k], dummy_run());
                if let Err(e) = self.start_engine(&mut r) {
                    eprintln!("atrium-cryptad: {}: engine: {e}", r.p.name);
                    r.last_error = e;
                    r.restart_at = Some(now + Duration::from_secs(r.backoff_s));
                    r.backoff_s = (r.backoff_s * 2).min(BACKOFF_MAX_S);
                }
                self.runs[k] = r;
            }
        }
        #[cfg(target_os = "freebsd")]
        self.janitor.retain(|&u| !jail::try_teardown(u));
    }
}

#[cfg(target_os = "freebsd")]
fn launch(jaild: &str, uid: u32, r: &Run) -> Result<Live, String> {
    let e = jail::launch(jaild, uid, r.p.endpoint, r.tun.as_fd())?;
    Ok(Live { uid: e.uid, sock: e.sock, rd: engine::Reader::default(), pd: e.procdesc, started: Instant::now() })
}

#[cfg(not(target_os = "freebsd"))]
fn launch(_jaild: &str, uid: u32, r: &Run) -> Result<Live, String> {
    Err(format!("engines run on FreeBSD only (uid {uid}, tun fd {})", r.tun.as_raw_fd()))
}

fn dummy_run() -> Run {
    Run {
        p: Profile {
            name: String::new(),
            key: [0; 16],
            public: [0; 32],
            psk: None,
            peer: [0; 32],
            endpoint: "0.0.0.0:0".parse().unwrap(),
            keepalive: None,
            addrs: vec![],
            allowed: vec![],
            dns: vec![],
            domains: vec![],
            mtu: None,
            block: false,
            insecure: false,
        },
        iface: String::new(),
        tun: std::fs::File::open("/dev/null").expect("/dev/null").into(),
        engine: None,
        restart_at: None,
        backoff_s: 1,
        handshake: false,
        stats: (0, 0, 0),
        starts: 0,
        last_error: String::new(),
    }
}

fn peer_is_root(s: &UnixStream) -> bool {
    aura_ipc::sock::peer_uid(s) == Some(0)
}

fn listen(path: &str) -> UnixListener {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    if let Some(d) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let l = UnixListener::bind(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    l
}

fn poll(fds: &mut [libc::pollfd], ms: i32) {
    // SAFETY: a slice of valid pollfds.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, ms) };
}

fn pfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd { fd, events: libc::POLLIN, revents: 0 }
}

fn daemon(o: Opts) {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        eprintln!("atrium-cryptad: not root: jaild will refuse engines");
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&o.state);
    }
    let keys = key::Keys::start(o.key_uid, &o.arca).unwrap_or_else(|e| die(&format!("key helper: {e}")));
    let listener = listen(&o.sock);
    let mut d = D { o, keys, net: None, runs: Vec::new(), janitor: Vec::new() };
    #[cfg(target_os = "freebsd")]
    {
        d.janitor = jail::sweep(&d.o.jaild);
    }
    d.log("ready");
    let mut ctls: Vec<CtlConn> = Vec::new();
    loop {
        let mut fds = vec![pfd(listener.as_raw_fd())];
        fds.extend(ctls.iter().map(|c| pfd(c.s.as_raw_fd())));
        let eng: Vec<usize> = (0..d.runs.len()).filter(|&k| d.runs[k].engine.is_some()).collect();
        for &k in &eng {
            let e = d.runs[k].engine.as_ref().unwrap();
            fds.push(pfd(e.sock.as_raw_fd()));
            fds.push(libc::pollfd { fd: e.pd.as_raw_fd(), events: libc::POLLHUP, revents: 0 });
        }
        poll(&mut fds, 500);
        if fds[0].revents != 0 {
            if let Ok((s, _)) = listener.accept() {
                if peer_is_root(&s) {
                    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                    ctls.push(CtlConn { s, buf: Vec::new(), fds: Vec::new() });
                }
            }
        }
        let nc = ctls.len().min(fds.len() - 1 - 2 * eng.len());
        let mut gone = Vec::new();
        for j in 0..nc {
            if fds[1 + j].revents == 0 {
                continue;
            }
            let c = &mut ctls[j];
            let mut b = [0u8; 4096];
            match fd::recv(c.s.as_raw_fd(), &mut b) {
                Ok((0, _)) | Err(_) => {
                    gone.push(j);
                    continue;
                }
                Ok((n, f)) => {
                    c.buf.extend_from_slice(&b[..n]);
                    c.fds.extend(f);
                }
            }
            while let Some(p) = ctls[j].buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = ctls[j].buf.drain(..=p).collect();
                let file = ctls[j].fds.pop();
                let reply = match std::str::from_utf8(&raw[..p]).ok().and_then(Line::parse) {
                    Some(l) => d.ctl(&l, file),
                    None => vec![Line::new("error").with("why", "bad-line")],
                };
                for r in reply {
                    if r.send(&mut ctls[j].s).is_err() {
                        gone.push(j);
                        break;
                    }
                }
            }
            if ctls[j].buf.len() > aura_ipc::MAX_LINE {
                gone.push(j);
            }
        }
        gone.sort_unstable();
        gone.dedup();
        for j in gone.into_iter().rev() {
            ctls.remove(j);
        }
        // Engines: what they said, and whether they are still there. The
        // indices were taken before the control requests, which may have
        // stopped some: look each up again by its uid.
        let base = 1 + nc + (fds.len() - 1 - nc - 2 * eng.len());
        let polled: Vec<(u32, bool, bool)> = eng
            .iter()
            .enumerate()
            .filter_map(|(i, &k)| {
                let uid = d.runs.get(k)?.engine.as_ref()?.uid;
                Some((uid, fds[base + 2 * i].revents != 0, fds[base + 2 * i + 1].revents != 0))
            })
            .collect();
        for (uid, readable, exited) in polled {
            let find = |d: &D| d.runs.iter().position(|r| r.engine.as_ref().is_some_and(|e| e.uid == uid));
            if readable {
                if let Some(k) = find(&d) {
                    d.engine_frames(k);
                }
            }
            if exited {
                if let Some(k) = find(&d) {
                    d.engine_gone(k, "exited");
                }
            }
        }
        d.tick();
    }
}

/// `atrium-cryptad ctl VERB [k=v ...]`.
fn ctl_client(o: &Opts, args: &[String]) -> i32 {
    let Some(verb) = args.first() else {
        eprintln!("usage: atrium-cryptad ctl VERB [k=v ...]");
        return 2;
    };
    let mut l = Line::new(verb);
    for a in &args[1..] {
        let Some((k, v)) = a.split_once('=') else {
            eprintln!("{a}: not k=v");
            return 2;
        };
        l = l.with(k, v);
    }
    let mut s = match UnixStream::connect(&o.sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {e}", o.sock);
            return 1;
        }
    };
    let b = l.encode().unwrap_or_default();
    let sent = if verb == "import" {
        let stdin = io::stdin();
        fd::send(s.as_raw_fd(), &b, &[stdin.as_fd().as_raw_fd()]).and_then(|n| s.write_all(&b[n..]))
    } else {
        s.write_all(&b)
    };
    if let Err(e) = sent {
        eprintln!("send: {e}");
        return 1;
    }
    let mut r = aura_ipc::Reader::default();
    loop {
        match r.fill(&mut s) {
            Ok(v) => {
                for g in v {
                    if let aura_ipc::Got::Line(l) = g {
                        let b = l.encode().unwrap_or_default();
                        print!("{}", String::from_utf8_lossy(&b));
                        match l.verb.as_str() {
                            "ok" => return 0,
                            "error" => return 1,
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("cryptad: {e}");
                return 1;
            }
        }
    }
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    if a.first().map(String::as_str) == Some("--arca-key") {
        let sock = std::env::var("ARCA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/arca/arcad.sock".into());
        return key::serve(&sock);
    }
    if a.first().map(String::as_str) == Some("ctl") {
        // Options before `ctl` are not taken; the socket from the environment.
        let mut o = opts(std::iter::empty());
        if let Ok(s) = std::env::var("CRYPTA_SOCK") {
            o.sock = s;
        }
        let rest = a.split_off(1);
        std::process::exit(ctl_client(&o, &rest));
    }
    daemon(opts(a.into_iter()));
}

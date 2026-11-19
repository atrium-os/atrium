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
//! AnyConnect (V4, vpn.md §9):
//!
//! - `create-gp name=N host=H[:P] [portal=1] [user=HEX] [pin=HEX] [ca=B64] [scoped=0]`
//! - `create-ac name=N host=H[:P] [path=] [user=HEX] [group=HEX] [pin=HEX] [ca=B64] [scoped=0]`
//! - `prompt name=N`: what the gateway asks (`text k= v=HEX` lines, then
//!   `field i= kind= name=HEX label=HEX` and `choice i= value=HEX
//!   label=HEX`), or `sso login=HEX final=HEX cookie=HEX`
//! - `answer name=N tI=HEX | hI=HANDLE [keep=1] ... | cancel=1`: field I's
//!   text, or the handle of what was typed into Arca's field
//! - `sso-done name=N ok=0|1`, `rebind name=N`, `banner name=N` (`text` lines)
//! - `csr name=N [cn=HEX]`: a P-256 key in Arca and a request for it
//! - `certificate name=N` with the PEM chain as a descriptor
//!
//! `atrium-cryptad ctl VERB [k=v ...]` sends one (with stdin as the file for
//! `import`) and prints the answer.

mod csr;
mod fd;
mod key;
mod net;
mod profile;
mod hip;
#[cfg(target_os = "freebsd")]
mod jail;

use aura_ipc::Line;
use crypta_proto::engine::{self, DhShared, FromEngine, Start, ToEngine};
use profile::Profile;
use std::io::{self, Read, Write};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const KEY_UID: u32 = 65052;
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
    /// What the chip shows (vpn.md §7.4): each running tunnel's state.
    vpn_state: PathBuf,
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
        vpn_state: "/var/run/atrium/vpn-state".into(),
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
            "--vpn-state" => o.vpn_state = v().into(),
            _ => die("usage: atrium-cryptad [-v] [--sock P] [--state DIR] [--net-sock P] [--jaild P] [--arca P] [--key-uid N] [--vpn-state P] | ctl VERB [k=v ...] | --arca-key"),
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
    /// An AnyConnect engine's secrets helper (vpn.md §9.3).
    helper: Option<Helper>,
}

/// A child that ends with its engine.
struct Helper(std::process::Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// AnyConnect: where a sign-in stands.
#[derive(Default)]
struct AcRun {
    /// Where the gateway's name resolved at `up`.
    gw: Option<SocketAddrV4>,
    gp_selected: Option<(String, u16)>,
    prompt: Option<engine::Prompt>,
    /// `(login, final_url, token_cookie)`.
    sso: Option<(String, String, String)>,
    pushed: Option<engine::Pushed>,
    /// What the person answered the username and group fields with, kept
    /// in the profile once the sign-in succeeds.
    user: Option<String>,
    group: Option<String>,
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
    ac: AcRun,
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
    /// The `vpn-state` text last written.
    shown: Option<String>,
    /// Why a tunnel stopped for good (a gateway's refusal, a push
    /// atrium-net refused), until it is turned on again.
    failed: HashMap<String, String>,
}

/// A gateway's name to an IPv4 address (the machine's resolver).
#[cfg(target_os = "freebsd")]
fn hip_ipv6_forwarding() -> Option<bool> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&value);
    // SAFETY: the named read-only sysctl writes at most len bytes.
    let result = unsafe { libc::sysctlbyname(c"net.inet6.ip6.forwarding".as_ptr(),
        (&mut value as *mut libc::c_int).cast(), &mut len, std::ptr::null(), 0) };
    (result == 0 && len == std::mem::size_of_val(&value)).then_some(value != 0)
}

#[cfg(not(target_os = "freebsd"))]
fn hip_ipv6_forwarding() -> Option<bool> { None }

fn resolve(host: &str, port: u16) -> Result<SocketAddrV4, String> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(SocketAddrV4::new(ip, port));
    }
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .find_map(|a| match a {
            SocketAddr::V4(v) => Some(v),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| "no-v4-address".to_string())
}

/// Free text for a line: hex, at most `max` bytes of it.
fn texts(out: &mut Vec<Line>, k: &str, v: &str) {
    for c in v.as_bytes().chunks(1500).take(8) {
        out.push(Line::new("text").with("k", k).with_hex("v", c));
    }
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

    /// `create`: a new tunnel with a key Arca makes (vpn.md §7.3); the
    /// answer's `public` is what the server's admin needs.
    fn create(&mut self, l: &Line) -> Result<Vec<Line>, String> {
        let mut p = Profile::from_create(l)?;
        if Profile::load(&self.o.state, &p.name).is_some() {
            return Err("exists".into());
        }
        let k = self.keys.call(&Line::new("mint").with("account", &p.name)).map_err(|e| e.to_string())?;
        let (Some(id), Some(public)) = (k.hex("id").and_then(|i| <[u8; 16]>::try_from(i).ok()), hex32(&k, "pub")) else {
            return Err(format!("arca-{}", k.get("status").unwrap_or("?")));
        };
        p.key = id;
        p.public = public;
        if let Err(e) = p.save(&self.o.state) {
            self.forget(&[id]);
            return Err(format!("save: {e}"));
        }
        self.log(&format!("created {} ({} allowed, block {})", p.name, p.allowed.len(), p.block));
        Ok(vec![Line::new("created").with("name", &p.name).with("public", crypta_proto::b64::encode(&public))])
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
        self.failed.remove(name);
        // AnyConnect: the gateway's address now; its settings come with the
        // sign-in, and are applied then (`Pushed`).
        let gw = match &p.ac {
            Some(a) => Some(resolve(&a.host, a.port)?),
            None => None,
        };
        let mut a = self.net_call(&Line::new("tunnel-open").with("id", name))?;
        let iface = a.lines.iter().find(|l| l.verb == "tunnel").and_then(|l| l.get("iface")).ok_or("no-iface")?.to_string();
        let tun = a.fds.pop().ok_or("no-tun")?;
        let wg = if gw.is_none() { p.proposal().to_lines(name) } else { Vec::new() };
        for l in wg {
            if let Err(e) = self.net_call(&l) {
                let _ = self.net_call(&Line::new("tunnel-close").with("id", name));
                return Err(e);
            }
        }
        self.log(&format!("{name}: open on {iface}"));
        let mut r = Run { p, iface, tun, engine: None, restart_at: None, backoff_s: 1, handshake: false, stats: (0, 0, 0), starts: 0, last_error: String::new(), ac: AcRun { gw, ..AcRun::default() } };
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
        if r.p.ac.is_some() {
            return self.start_ac(r);
        }
        let psk = match r.p.psk {
            Some(i) => {
                let a = self.keys.call(&Line::new("release").with_hex("id", &i)).map_err(|e| e.to_string())?;
                Some(hex32(&a, "value").ok_or_else(|| format!("psk: arca-{}", a.get("status").unwrap_or("?")))?)
            }
            None => None,
        };
        let uid = self.pick_uid().ok_or("no-uid")?;
        r.starts += 1;
        let mut e = launch(&self.o.jaild, uid, r, r.p.endpoint, None)?;
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

    /// An AnyConnect engine (vpn.md §9.3): its own secrets helper on a
    /// socket only it holds, and what Arca already keeps for the profile.
    fn start_ac(&mut self, r: &mut Run) -> Result<(), String> {
        let mut a = r.p.ac.clone().ok_or("not-ac")?;
        let discovering = a.gp_portal && r.ac.gp_selected.is_none();
        if let Some((host, port)) = &r.ac.gp_selected {
            a.host = host.clone();
            a.port = *port;
        }
        let gw = r.ac.gw.ok_or("no-gateway")?;
        let mut has = |acc: String| self.keys.call(&Line::new("find").with("account", acc)).is_ok_and(|l| l.verb == "ok");
        let saved = !a.gp_portal && has(format!("{}/password", r.p.name));
        let resume = !a.gp_portal && has(format!("{}/session", r.p.name));
        let uid = self.pick_uid().ok_or("no-uid")?;
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let access = match (&r.ac.gp_selected, &r.p.ac) {
            (Some((host, port)), Some(profile)) if host != &profile.host || *port != profile.port => "selected",
            _ if a.gp_portal => "portal",
            _ => "profile",
        };
        let helper = key::Keys::start_vpn(self.o.key_uid, &self.o.arca, &r.p.name, &a.host, access, theirs).map_err(|e| format!("secrets helper: {e}"))?;
        let helper = Helper(helper);
        r.starts += 1;
        let mut e = launch(&self.o.jaild, uid, r, gw, Some(ours.as_fd()))?;
        drop(ours);
        e.helper = Some(helper);
        let settings = engine::StartAc {
            host: a.host.clone(),
            gateway: gw,
            path: a.path.clone(),
            cas: a.cas.clone(),
            pin: a.pin,
            hostname: a.hostname.clone(),
            mtu: r.p.mtu.unwrap_or(1406),
            username: a.username.clone(),
            group: a.group.clone(),
            saved,
            cert_chain: a.cert_chain.clone(),
            resume,
        };
        let start = if discovering { ToEngine::StartGpPortal(settings) } else if a.gp_portal { ToEngine::StartGpSelected(settings) } else if a.globalprotect { ToEngine::StartGp(settings) } else { ToEngine::StartAc(settings) };
        let _ = e.sock.set_write_timeout(Some(Duration::from_secs(2)));
        if let Err(err) = engine::write(&mut e.sock, &start) {
            self.retire(e.uid);
            return Err(format!("start: {err}"));
        }
        self.log(&format!("{}: corporate engine {} started for {} ({gw}){}{}", r.p.name, e.uid, a.host, if saved { ", saved password" } else { "" }, if resume { ", resuming" } else { "" }));
        r.engine = Some(e);
        r.handshake = false;
        r.ac.prompt = None;
        r.ac.sso = None;
        Ok(())
    }

    /// A tunnel that cannot go on: stopped and closed, and why kept for
    /// `list` until it is turned on again.
    fn fail(&mut self, k: usize, why: &str) {
        let name = self.runs[k].p.name.clone();
        eprintln!("atrium-cryptad: {name}: stopped: {why}");
        self.stop_engine(k);
        self.runs.remove(k);
        let _ = self.net_call(&Line::new("tunnel-close").with("id", &name));
        self.failed.insert(name, why.chars().take(400).collect());
    }

    /// What a gateway pushed, to atrium-net; it checks it again (§9.4).
    fn apply_push(&mut self, k: usize, p: engine::Pushed) -> Result<(), String> {
        let r = &self.runs[k];
        let gw = r.ac.gw.ok_or("no-gateway")?;
        if r.ac.pushed.as_ref() == Some(&p) {
            return Ok(());
        }
        let name = r.p.name.clone();
        for l in r.p.ac_proposal(&p, gw).to_lines(&name) {
            self.net_call(&l)?;
        }
        if !p.ignored.is_empty() {
            self.log(&format!("{name}: ignored {}", p.ignored.join(",")));
        }
        self.log(&format!("{name}: applied {}/{} on {}", p.addr, p.prefix, self.runs[k].iface));
        let r = &mut self.runs[k];
        r.ac.pushed = Some(p);
        r.ac.prompt = None;
        r.ac.sso = None;
        // Signed in: keep what was typed for the username and group.
        let (user, group) = (r.ac.user.take(), r.ac.group.take());
        if let Some(a) = r.p.ac.as_mut() {
            let changed = (user.is_some() && a.username != user) || (group.is_some() && a.group != group);
            if changed {
                a.username = user.or(a.username.take());
                a.group = group.or(a.group.take());
                if let Some(mut saved) = Profile::load(&self.o.state, &r.p.name) {
                    if let (Some(s), Some(n)) = (saved.ac.as_mut(), r.p.ac.as_ref()) {
                        s.username = n.username.clone();
                        s.group = n.group.clone();
                        let _ = saved.save(&self.o.state);
                    }
                }
            }
        }
        Ok(())
    }

    fn ac_run(&mut self, name: &str) -> Result<usize, String> {
        let k = self.runs.iter().position(|r| r.p.name == name).ok_or("not-up")?;
        if self.runs[k].p.ac.is_none() {
            return Err("not-ac".into());
        }
        Ok(k)
    }

    fn to_engine(&mut self, k: usize, m: &ToEngine) -> Result<(), String> {
        let e = self.runs[k].engine.as_mut().ok_or("no-engine")?;
        engine::write(&mut e.sock, m).map_err(|e| e.to_string())
    }

    /// `prompt`: what the gateway is asking, for Settings.
    fn prompt(&mut self, name: &str) -> Result<Vec<Line>, String> {
        let k = self.ac_run(name)?;
        let r = &self.runs[k];
        let mut v = Vec::new();
        if let Some((login, fin, cookie)) = &r.ac.sso {
            v.push(Line::new("sso").with_hex("login", login.as_bytes()).with_hex("final", fin.as_bytes()).with_hex("cookie", cookie.as_bytes()));
            return Ok(v);
        }
        let p = r.ac.prompt.as_ref().ok_or("no-prompt")?;
        for (k, t) in [("title", &p.title), ("message", &p.message), ("error", &p.error), ("banner", &p.banner)] {
            texts(&mut v, k, t);
        }
        for (i, f) in p.fields.iter().enumerate() {
            v.push(Line::new("field").with("i", i).with("kind", f.kind).with_hex("name", f.name.as_bytes()).with_hex("label", f.label.as_bytes()));
            for (val, label) in f.choices.iter().take(64) {
                v.push(Line::new("choice").with("i", i).with_hex("value", val.as_bytes()).with_hex("label", label.as_bytes()));
            }
        }
        Ok(v)
    }

    /// `answer`: the person's answers to the prompt, to the engine.
    fn answer(&mut self, name: &str, l: &Line) -> Result<(), String> {
        let k = self.ac_run(name)?;
        let p = self.runs[k].ac.prompt.clone().ok_or("no-prompt")?;
        let a = if l.get("cancel") == Some("1") {
            None
        } else {
            let keep = l.get("keep") == Some("1");
            let mut out = Vec::new();
            for (i, f) in p.fields.iter().enumerate() {
                let v = if let Some(t) = l.hex(&format!("t{i}")) {
                    // A password reaches the engine only through Arca's
                    // field (vpn.md §9.3), never through cryptad.
                    if f.kind == 1 || f.name == "gp_response" {
                        return Err("password-through-arca".into());
                    }
                    let t = String::from_utf8(t).map_err(|_| "bad-text")?;
                    if f.kind == 0 && f.name == "username" {
                        self.runs[k].ac.user = Some(t.clone());
                    }
                    if f.kind == 3 {
                        if !f.choices.iter().any(|(v, _)| *v == t) {
                            return Err("bad-choice".into());
                        }
                        if f.name == "group_list" {
                            self.runs[k].ac.group = Some(t.clone());
                        }
                    }
                    engine::Value::Text(t)
                } else if let Some(h) = l.hex(&format!("h{i}")).and_then(|h| <[u8; 16]>::try_from(h).ok()) {
                    engine::Value::Held { handle: h, keep: keep && f.kind == 1 }
                } else {
                    return Err("missing-field".into());
                };
                out.push((f.name.clone(), v));
            }
            Some(out)
        };
        if let Some([(field, engine::Value::Text(authority))]) = a.as_deref() {
            if field == "gp_gateway" {
                let profile = self.runs[k].p.ac.as_ref().filter(|a| a.gp_portal).ok_or("not-gp-portal")?;
                let (host, port) = authority.rsplit_once(':').ok_or("bad-gateway")?;
                let port: u16 = port.parse().ok().filter(|p| *p != 0).ok_or("bad-gateway")?;
                if !profile::valid_host(host) {
                    return Err("bad-gateway".into());
                }
                if self.runs[k].ac.gp_selected.is_some() || self.runs[k].ac.pushed.is_some() {
                    return Err("gateway-already-selected".into());
                }
                if host != profile.host || port != profile.port {
                    // The old jail cannot connect to a new origin. Its pin
                    // and client certificate identify the portal only.
                    if profile.pin.is_some() || !profile.cert_chain.is_empty() {
                        return Err("gateway-needs-own-trust-profile".into());
                    }
                    let gateway = resolve(host, port)?;
                    self.stop_engine(k);
                    let mut run = std::mem::replace(&mut self.runs[k], dummy_run());
                    run.ac.gw = Some(gateway);
                    run.ac.gp_selected = Some((host.into(), port));
                    run.ac.prompt = None;
                    let started = self.start_engine(&mut run);
                    self.runs[k] = run;
                    if let Err(err) = started {
                        self.fail(k, &err);
                        return Err(err);
                    }
                    return Ok(());
                }
                let selected = (host.to_string(), port);
                self.to_engine(k, &ToEngine::Answer(a))?;
                self.runs[k].ac.gp_selected = Some(selected);
                self.runs[k].ac.prompt = None;
                return Ok(());
            }
        }
        self.to_engine(k, &ToEngine::Answer(a))?;
        self.runs[k].ac.prompt = None;
        Ok(())
    }

    /// `create-ac`: a corporate gateway; CA certificates (PEM) may come as a
    /// descriptor besides `ca=`.
    fn create_ac(&mut self, l: &Line, file: Option<OwnedFd>) -> Result<Vec<Line>, String> {
        let mut p = Profile::from_create_ac(l)?;
        if Profile::load(&self.o.state, &p.name).is_some() {
            return Err("exists".into());
        }
        if let Some(f) = file {
            let mut text = String::new();
            std::fs::File::from(f).take(64 * 1024).read_to_string(&mut text).map_err(|_| "unreadable")?;
            if !text.trim().is_empty() {
                let cas = profile::certs_of(&text).ok_or("bad-ca")?;
                if let Some(a) = p.ac.as_mut() {
                    a.cas.extend(cas.into_iter().take(8));
                }
            }
        }
        p.save(&self.o.state).map_err(|e| format!("save: {e}"))?;
        // A sign-in jar left by an old profile of this name is not this one's.
        let _ = self.keys.call(&Line::new("forget-jar").with("profile", &p.name));
        self.log(&format!("created {} (corporate VPN, {})", p.name, p.ac.as_ref().map(|a| a.host.as_str()).unwrap_or("")));
        Ok(vec![Line::new("created").with("name", &p.name)])
    }

    /// `csr`: a client key made in Arca, and a request for the IT team.
    fn csr(&mut self, name: &str, l: &Line) -> Result<Vec<Line>, String> {
        let mut p = Profile::load(&self.o.state, name).ok_or("no-profile")?;
        let a = p.ac.as_mut().ok_or("not-ac")?;
        let account = format!("{name}/tls");
        let (id, public) = match self.keys.call(&Line::new("find").with("account", &account)).ok().filter(|l| l.verb == "ok").and_then(|l| l.hex("id")).and_then(|i| <[u8; 16]>::try_from(i).ok()) {
            Some(id) if a.cert_key.is_some() => (id, a.cert_key.clone().unwrap()),
            old => {
                if let Some(id) = old {
                    self.forget(&[id]);
                }
                let m = self.keys.call(&Line::new("mint-tls").with("account", &account)).map_err(|e| e.to_string())?;
                let (Some(id), Some(public)) = (m.hex("id").and_then(|i| <[u8; 16]>::try_from(i).ok()), m.hex("pub").filter(|p| p.len() == 65)) else {
                    return Err(format!("arca-{}", m.get("status").unwrap_or("?")));
                };
                a.cert_key = Some(public.clone());
                a.cert_chain.clear();
                (id, public)
            }
        };
        let cn = match l.hex("cn") {
            Some(c) => String::from_utf8(c).ok().filter(|c| !c.is_empty() && c.len() <= 64 && !c.chars().any(char::is_control)).ok_or("bad-cn")?,
            None => a.username.clone().unwrap_or_else(|| name.to_string()),
        };
        let info = csr::info(&cn, &public);
        let s = self.keys.call(&Line::new("sign").with_hex("id", &id).with_hex("digest", &csr::digest(&info))).map_err(|e| e.to_string())?;
        let sig = s.hex("sig").ok_or_else(|| format!("arca-{}", s.get("status").unwrap_or("?")))?;
        p.save(&self.o.state).map_err(|e| format!("save: {e}"))?;
        Ok(vec![Line::new("csr").with_hex("pem", csr::pem(&info, &sig).as_bytes())])
    }

    /// `certificate`: the chain the IT team signed, for the key `csr` made.
    fn certificate(&mut self, name: &str, file: Option<OwnedFd>) -> Result<Vec<Line>, String> {
        let mut p = Profile::load(&self.o.state, name).ok_or("no-profile")?;
        let a = p.ac.as_mut().ok_or("not-ac")?;
        let key = a.cert_key.clone().ok_or("no-key")?;
        let mut text = String::new();
        std::fs::File::from(file.ok_or("no-file")?).take(64 * 1024).read_to_string(&mut text).map_err(|_| "unreadable")?;
        let chain = profile::certs_of(&text).ok_or("bad-certificate")?;
        if chain.len() > 8 || !csr::certifies(&chain[0], &key) {
            return Err("not-for-this-key".into());
        }
        a.cert_chain = chain;
        p.save(&self.o.state).map_err(|e| format!("save: {e}"))?;
        Ok(vec![])
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
            if e.helper.is_some() {
                // An AnyConnect engine says goodbye to its gateway first:
                // a moment for it to go by itself.
                let _ = e.sock.set_read_timeout(Some(Duration::from_millis(1500)));
                let mut b = [0u8; 4096];
                while matches!(e.sock.read(&mut b), Ok(n) if n > 0) {}
            }
            // Closing the process descriptor ends the engine if Stop did not.
            drop(e.pd);
            self.retire(e.uid);
        }
    }

    fn down(&mut self, name: &str) -> Result<(), String> {
        let Some(k) = self.runs.iter().position(|r| r.p.name == name) else { return Ok(()) };
        self.stop_engine(k);
        let r = self.runs.remove(k);
        if r.p.ac.is_some() {
            // Disconnected by the person: the session is over (vpn.md §9.3).
            if let Some(id) = self.keys.call(&Line::new("find").with("account", format!("{name}/session"))).ok().and_then(|l| l.hex("id")).and_then(|i| <[u8; 16]>::try_from(i).ok()) {
                self.forget(&[id]);
            }
        }
        self.net_call(&Line::new("tunnel-close").with("id", name)).map(|_| ())
    }

    fn delete(&mut self, name: &str) -> Result<(), String> {
        let p = Profile::load(&self.o.state, name).ok_or("no-profile")?;
        self.down(name)?;
        self.failed.remove(name);
        if p.ac.is_some() {
            // Its password, session, token and client key.
            let _ = self.keys.call(&Line::new("forget-all").with("prefix", format!("{name}/")));
            // The identity provider's session, kept in its sign-in jar.
            let _ = self.keys.call(&Line::new("forget-jar").with("profile", name));
        } else {
            let mut ids = vec![p.key];
            ids.extend(p.psk);
            self.forget(&ids);
        }
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
                if let Some(r) = self.runs.iter().find(|r| r.p.name == n) {
                    if r.p.ac.is_some() {
                        let st = if r.ac.sso.is_some() { "sso" } else if r.ac.prompt.is_some() { "prompt" } else if r.ac.pushed.is_some() { "in" } else { "starting" };
                        l = l.with("signin", st);
                        if let Some(p) = &r.ac.pushed {
                            l = l.with("addr", format!("{}/{}", p.addr, p.prefix)).with("full", p.full as u8).with("routes", p.includes.len());
                            if !p.banner.is_empty() {
                                l = l.with("banner", 1);
                            }
                        }
                    }
                }
                if let Some(why) = self.failed.get(&n) {
                    l = l.with_hex("error", why.as_bytes());
                }
                let ac = Profile::load(&self.o.state, &n).filter(|p| p.ac.is_some());
                if let Some(p) = ac {
                    let a = p.ac.as_ref().unwrap();
                    l = l.with("kind", a.kind()).with("host", &a.host).with("port", a.port).with("block", p.block as u8).with("scoped", p.scoped as u8).with("cert", (!a.cert_chain.is_empty()) as u8).with("pinned", a.pin.is_some() as u8);
                    if let Some(u) = &a.username {
                        l = l.with_hex("user", u.as_bytes());
                    }
                    if let Some(g) = &a.group {
                        l = l.with_hex("group", g.as_bytes());
                    }
                } else if let Some(p) = Profile::load(&self.o.state, &n) {
                    l = l.with("kind", "wg").with("public", crypta_proto::b64::encode(&p.public)).with("endpoint", p.endpoint).with("full", p.full() as u8).with("block", p.block as u8).with("scoped", p.scoped as u8);
                    l = l.with("addrs", p.addrs.iter().map(|a| format!("{}/{}", a.addr, a.len)).collect::<Vec<_>>().join(",")).with("routes", p.allowed.len());
                    if !p.dns.is_empty() {
                        l = l.with("dns", p.dns.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(","));
                    }
                }
                l
            })
            .collect()
    }

    fn ctl(&mut self, l: &Line, file: Option<OwnedFd>) -> Vec<Line> {
        let name = l.get("name").filter(|n| profile::valid_name(n)).map(str::to_string);
        let res: Result<Vec<Line>, String> = match (l.verb.as_str(), name) {
            ("import", _) => self.import(l, file),
            ("create", _) => self.create(l),
            ("create-ac" | "create-gp", _) => self.create_ac(l, file),
            ("prompt", Some(n)) => self.prompt(&n),
            // What the gateway said once signed in (vpn.md §9.4: plain text).
            ("banner", Some(n)) => self.ac_run(&n).map(|k| {
                let mut v = Vec::new();
                if let Some(p) = &self.runs[k].ac.pushed {
                    texts(&mut v, "banner", &p.banner);
                }
                v
            }),
            ("answer", Some(n)) => self.answer(&n, l).map(|_| vec![]),
            ("sso-done", Some(n)) => {
                let ok = l.get("ok") == Some("1");
                self.ac_run(&n).and_then(|k| {
                    if self.runs[k].ac.sso.take().is_none() {
                        return Err("no-sso".into());
                    }
                    self.to_engine(k, &ToEngine::SsoDone(ok))
                }).map(|_| vec![])
            }
            ("rebind", Some(n)) => self.ac_run(&n).and_then(|k| self.to_engine(k, &ToEngine::Rebind)).map(|_| vec![]),
            ("csr", Some(n)) => self.csr(&n, l),
            ("certificate", Some(n)) => self.certificate(&n, file),
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
                match (Profile::load(&self.o.state, &n), flag("block"), flag("insecure"), flag("scoped")) {
                    (Some(mut p), Ok(b), Ok(i), Ok(s)) if b.is_some() || i.is_some() || s.is_some() => {
                        p.block = b.unwrap_or(p.block);
                        p.insecure = i.unwrap_or(p.insecure);
                        p.scoped = s.unwrap_or(p.scoped);
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
                FromEngine::Prompt(p) => {
                    self.log(&format!("{}: the gateway asks ({} fields)", self.runs[k].p.name, p.fields.len()));
                    self.runs[k].ac.sso = None;
                    self.runs[k].ac.prompt = Some(p);
                }
                FromEngine::Sso { login, final_url, token_cookie } => {
                    self.log(&format!("{}: single sign-on at {}", self.runs[k].p.name, login.chars().take(120).collect::<String>()));
                    self.runs[k].ac.prompt = None;
                    self.runs[k].ac.sso = Some((login, final_url, token_cookie));
                }
                FromEngine::Pushed(p) => {
                    if self.runs[k].p.ac.as_ref().is_some_and(|a| a.gp_portal)
                        && self.runs[k].ac.gp_selected.is_none()
                    {
                        self.fail(k, "GlobalProtect portal has no selected gateway");
                        return;
                    }
                    if let Err(e) = self.apply_push(k, p) {
                        self.fail(k, &format!("atrium-net refused what the gateway pushed ({e})"));
                        return;
                    }
                    if self.runs[k].p.ac.as_ref().is_some_and(|a| a.globalprotect) {
                        let _ = self.to_engine(k, &ToEngine::Applied);
                    }
                }
                FromEngine::HipFacts => {
                    if self.runs[k].p.ac.as_ref().is_some_and(|a| a.globalprotect) {
                        let pf = self.net_call(&Line::new("hip-firewall")).ok().and_then(|a| {
                            a.lines.iter().find(|l| l.verb == "hip-firewall").and_then(|l| match l.get("pf") {
                                Some("1") => Some(true),
                                Some("0") => Some(false),
                                _ => None,
                            })
                        });
                        let facts = engine::HipFacts {
                            pf,
                            ipv6_forwarding: hip_ipv6_forwarding(),
                            mounted_geli: hip::mounted_geli(),
                            patch_status: hip::patch_status(),
                            screen_locked: hip::screen_locked(),
                        };
                        let _ = self.to_engine(k, &ToEngine::HipFacts(facts));
                    }
                }
                FromEngine::Refused { why } => {
                    self.fail(k, &why);
                    return;
                }
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
        // A single sign-on ends when Arca's courier has caught the
        // gateway's token in the profile's sign-in jar (vpn.md §9.3, SAML):
        // then the engine takes it from Arca and signs in with it.
        for k in 0..self.runs.len() {
            let (Some(_), Some(a)) = (&self.runs[k].ac.sso, &self.runs[k].p.ac) else { continue };
            let acc = format!("{}/sso@{}", self.runs[k].p.name, a.host);
            if self.keys.call(&Line::new("find").with("account", acc)).is_ok_and(|l| l.verb == "ok") {
                self.runs[k].ac.sso = None;
                self.log(&format!("{}: signed on; the gateway's token is in Arca", self.runs[k].p.name));
                if let Err(e) = self.to_engine(k, &ToEngine::SsoDone(true)) {
                    eprintln!("atrium-cryptad: {}: {e}", self.runs[k].p.name);
                }
            }
        }
        #[cfg(target_os = "freebsd")]
        self.janitor.retain(|&u| !jail::try_teardown(u));
        self.publish_state();
    }

    /// `vpn-state` (vpn.md §7.4), world-readable, for frescod's chip: one
    /// `<profile> up|connecting machine|persona` line per running tunnel; a
    /// profile with no line is down. Written only when it changes.
    fn publish_state(&mut self) {
        let text: String = self
            .runs
            .iter()
            .map(|r| format!("{} {} {}\n", r.p.name, if r.handshake { "up" } else { "connecting" }, if r.p.scoped { "persona" } else { "machine" }))
            .collect();
        if self.shown.as_deref() == Some(text.as_str()) {
            return;
        }
        let p = &self.o.vpn_state;
        let tmp = p.with_extension("tmp");
        let w = (|| {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&tmp)?;
            f.write_all(text.as_bytes())?;
            std::fs::rename(&tmp, p)
        })();
        match w {
            Ok(()) => self.shown = Some(text),
            Err(e) => eprintln!("atrium-cryptad: {}: {e}", p.display()),
        }
    }
}

#[cfg(target_os = "freebsd")]
fn launch(jaild: &str, uid: u32, r: &Run, gw: SocketAddrV4, secrets: Option<BorrowedFd<'_>>) -> Result<Live, String> {
    let e = jail::launch(jaild, uid, gw, r.tun.as_fd(), secrets, r.p.ac.as_ref().is_some_and(|a| a.globalprotect))?;
    Ok(Live { uid: e.uid, sock: e.sock, rd: engine::Reader::default(), pd: e.procdesc, started: Instant::now(), helper: None })
}

#[cfg(not(target_os = "freebsd"))]
fn launch(_jaild: &str, uid: u32, r: &Run, gw: SocketAddrV4, _secrets: Option<BorrowedFd<'_>>) -> Result<Live, String> {
    Err(format!("engines run on FreeBSD only (uid {uid}, tun fd {}, {gw})", r.tun.as_raw_fd()))
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
            scoped: false,
            ac: None,
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
        ac: AcRun::default(),
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
    let mut d = D { o, keys, net: None, runs: Vec::new(), janitor: Vec::new(), shown: None, failed: HashMap::new() };
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
    let sent = if verb == "import" || verb == "certificate" {
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
    if a.first().map(String::as_str) == Some("--arca-vpn") {
        // An AnyConnect engine's own helper: its socket is stdin and stdout.
        let sock = std::env::var("ARCA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/arca/arcad.sock".into());
        let (Some(name), Some(host)) = (a.get(1).filter(|n| profile::valid_name(n)), a.get(2)) else { die("--arca-vpn NAME HOST") };
        let access = a.get(3).map(String::as_str).unwrap_or("profile");
        return key::serve_vpn(&sock, name, host, access);
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

//! atrium-crypta-ctl: VPN tunnels for apps (docs/spec/vpn.md §7.2).
//!
//! Serves `CLASS_VPN` on one socket per capability under `--dir`
//! (`/atrium/sockets/<capability>/<capability>.sock`) and turns each request
//! into cryptad control lines. It opens its one connection to cryptad's
//! control socket as root; then it drops to its own uid and enters
//! capability mode, so app input is decoded by a process that can do
//! nothing but talk to cryptad. If cryptad goes away it exits (the service
//! manager starts it again, and it waits for cryptad to return).
//!
//!   atrium-crypta-ctl [--ctl PATH] [--dir DIR] [--uid N] [--capsicum] [-v]

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use aura_ipc::{Got, Line, Reader};
use crypta_ctl::server::{serve, Backend};
use crypta_ctl::{lines, status, words, Answer, Ask, CertRequest, Certificate, CreateAnyConnect, CreateWireGuard, Created, Details, Fail, Grant, ImportWireGuard, Profile, Res, SetProfile, SsoDone, Tunnel};

fn die(s: &str) -> ! {
    eprintln!("atrium-crypta-ctl: {s}");
    std::process::exit(1)
}

struct Peer {
    s: UnixStream,
    r: Reader,
    pending: Vec<Line>,
}

impl Peer {
    /// Connects to cryptad's control socket, waiting while it is not up yet.
    fn connect(p: &str) -> Peer {
        use std::io::ErrorKind::{ConnectionRefused, NotFound};
        let mut pause = Duration::from_millis(5);
        let mut said = false;
        let s = loop {
            match UnixStream::connect(p) {
                Ok(s) => break s,
                Err(e) if matches!(e.kind(), NotFound | ConnectionRefused) => {
                    if !said {
                        eprintln!("atrium-crypta-ctl: waiting for {p} ({e})");
                        said = true;
                    }
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(500));
                }
                Err(e) => die(&format!("{p}: {e}")),
            }
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
        Peer { s, r: Reader::default(), pending: Vec::new() }
    }

    fn next(&mut self) -> Option<Line> {
        loop {
            if !self.pending.is_empty() {
                return Some(self.pending.remove(0));
            }
            match self.r.fill(&mut self.s) {
                Ok(v) => self.pending.extend(v.into_iter().filter_map(|g| if let Got::Line(l) = g { Some(l) } else { None })),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return None,
                Err(_) => die("cryptad closed its control socket"),
            }
        }
    }

    /// `l`, with `fd` attached to its first byte if given.
    fn send(&mut self, l: &Line, fd: Option<RawFd>) -> std::io::Result<()> {
        let b = l.encode().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "line too long"))?;
        let Some(fd) = fd else { return self.s.write_all(&b) };
        let n = send_fd(self.s.as_raw_fd(), &b, fd)?;
        self.s.write_all(&b[n..])
    }
}

fn send_fd(s: RawFd, data: &[u8], fd: RawFd) -> std::io::Result<usize> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; filled below with live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let n = std::mem::size_of::<RawFd>() as u32;
    // SAFETY: the control buffer has room for one descriptor.
    unsafe {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(n) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(n) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
    }
    // SAFETY: msg points at live buffers.
    let r = unsafe { libc::sendmsg(s, &msg, 0) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(r as usize)
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0 as RawFd; 2];
    // SAFETY: p has room for both ends.
    if unsafe { libc::pipe(p.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() just gave us both.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

struct Front {
    v: bool,
    calls: Mutex<Peer>,
    subs: Mutex<Vec<mpsc::Sender<Vec<Tunnel>>>>,
    kick: Mutex<mpsc::Sender<()>>,
}

impl Front {
    fn log(&self, s: &str) {
        if self.v {
            eprintln!("atrium-crypta-ctl: {s}");
        }
    }

    /// A request to cryptad: its reply lines, the last `ok` left out.
    fn call(&self, l: &Line, fd: Option<RawFd>) -> Res<Vec<Line>> {
        let mut p = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        if p.send(l, fd).is_err() {
            die("cryptad closed its control socket");
        }
        // Connecting waits for atrium-net and jaild; an import for Arca.
        let end = Instant::now() + Duration::from_secs(25);
        let mut v = Vec::new();
        while Instant::now() < end {
            let Some(x) = p.next() else { continue };
            match x.verb.as_str() {
                "ok" => return Ok(v),
                "error" => return Err(Fail(status::FAILED, words(x.get("why").unwrap_or("?")))),
                _ => v.push(x),
            }
        }
        Err(Fail::new(status::UNAVAILABLE, "cryptad did not answer"))
    }

    /// A request with `bytes` as a file on a pipe (written while cryptad reads).
    fn call_file(&self, l: &Line, bytes: Vec<u8>) -> Res<Vec<Line>> {
        let (r, w) = pipe().map_err(|e| Fail(status::FAILED, e.to_string()))?;
        let writer = std::thread::spawn(move || {
            let mut f = std::fs::File::from(w);
            let _ = f.write_all(&bytes);
        });
        let res = self.call(l, Some(r.as_raw_fd()));
        drop(r);
        let _ = writer.join();
        let _ = self.kick.lock().unwrap_or_else(|e| e.into_inner()).send(());
        res
    }

    fn change(&self, l: &Line) -> Res<()> {
        let r = self.call(l, None).map(|_| ());
        let _ = self.kick.lock().unwrap_or_else(|e| e.into_inner()).send(());
        r
    }

    fn tunnels(&self) -> Res<Vec<Tunnel>> {
        Ok(self.list()?.into_iter().map(|p| p.tunnel).collect())
    }

    /// Subscribers hear the list after every change made here and
    /// whenever cryptad's own (a handshake, a lost peer) shows, every 2 s.
    fn watch(self: Arc<Self>, kicks: mpsc::Receiver<()>) {
        let mut last: Option<Vec<Tunnel>> = None;
        loop {
            let _ = kicks.recv_timeout(Duration::from_secs(2));
            while kicks.try_recv().is_ok() {}
            if self.subs.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                last = None;
                continue;
            }
            let Ok(s) = self.tunnels() else { continue };
            if last.as_ref() == Some(&s) {
                continue;
            }
            self.subs.lock().unwrap_or_else(|e| e.into_inner()).retain(|t| t.send(s.clone()).is_ok());
            last = Some(s);
        }
    }
}

impl Backend for Front {
    fn status(&self) -> Res<Vec<Tunnel>> {
        self.tunnels()
    }

    fn subscribe(&self) -> Res<mpsc::Receiver<Vec<Tunnel>>> {
        let s = self.tunnels()?;
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(s);
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        Ok(rx)
    }

    fn list(&self) -> Res<Vec<Profile>> {
        Ok(self.call(&Line::new("list"), None)?.iter().filter_map(lines::profile).collect())
    }

    fn connect(&self, name: String) -> Res<()> {
        self.log(&format!("connect {name}"));
        self.change(&Line::new("up").with("name", name))
    }

    fn disconnect(&self, name: String) -> Res<()> {
        self.log(&format!("disconnect {name}"));
        self.change(&Line::new("down").with("name", name))
    }

    fn details(&self, name: String) -> Res<Details> {
        let l = self.call(&Line::new("list"), None)?.into_iter().find(|l| l.get("name") == Some(name.as_str()));
        let mut d = l.as_ref().and_then(lines::details).ok_or_else(|| Fail::new(status::FAILED, "no such tunnel"))?;
        if l.is_some_and(|l| l.get("banner") == Some("1")) {
            d.banner = self.call(&Line::new("banner").with("name", &name), None).ok().and_then(|v| lines::banner(&v));
        }
        Ok(d)
    }

    fn import(&self, i: ImportWireGuard) -> Res<Created> {
        self.log(&format!("import {} ({} bytes)", i.name, i.conf.len()));
        let (r, w) = pipe().map_err(|e| Fail(status::FAILED, e.to_string()))?;
        // The file may be larger than the pipe holds: written while cryptad reads.
        let mut conf = i.conf;
        let writer = std::thread::spawn(move || {
            let mut f = std::fs::File::from(w);
            let _ = f.write_all(&conf);
            conf.iter_mut().for_each(|b| *b = 0);
        });
        let res = self.call(&Line::new("import").with("name", &i.name), Some(r.as_raw_fd()));
        drop(r);
        let _ = writer.join();
        let _ = self.kick.lock().unwrap_or_else(|e| e.into_inner()).send(());
        lines::created(&res?).ok_or_else(|| Fail::new(status::FAILED, "cryptad's answer did not parse"))
    }

    fn create(&self, c: CreateWireGuard) -> Res<Created> {
        self.log(&format!("create {}", c.name));
        let v = self.call(&lines::create(&c), None)?;
        let _ = self.kick.lock().unwrap_or_else(|e| e.into_inner()).send(());
        lines::created(&v).ok_or_else(|| Fail::new(status::FAILED, "cryptad's answer did not parse"))
    }

    fn set(&self, s: SetProfile) -> Res<()> {
        let mut l = Line::new("set").with("name", &s.name);
        if let Some(b) = s.block {
            l = l.with("block", b as u8);
        }
        if let Some(p) = s.personas {
            l = l.with("scoped", p as u8);
        }
        self.log(&format!("set {:?}", s));
        self.change(&l)
    }

    fn remove(&self, name: String) -> Res<()> {
        self.log(&format!("remove {name}"));
        self.change(&Line::new("delete").with("name", name))
    }

    fn create_ac(&self, c: CreateAnyConnect) -> Res<String> {
        self.log(&format!("create-ac {} {}", c.name, c.host));
        self.call_file(&lines::create_ac(&c), c.ca)?;
        Ok(c.name)
    }

    fn prompt(&self, name: String) -> Res<Option<Ask>> {
        match self.call(&Line::new("prompt").with("name", name), None) {
            Ok(v) => Ok(lines::ask(&v)),
            Err(Fail(_, w)) if w == words("no-prompt") => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn answer(&self, a: Answer) -> Res<()> {
        self.log(&format!("answer {} ({} values)", a.name, a.values.as_ref().map_or(0, Vec::len)));
        self.change(&lines::answer(&a))
    }

    fn sso_done(&self, s: SsoDone) -> Res<()> {
        self.change(&Line::new("sso-done").with("name", s.name).with("ok", s.ok as u8))
    }

    fn cert_request(&self, c: CertRequest) -> Res<String> {
        let mut l = Line::new("csr").with("name", &c.name);
        if let Some(cn) = &c.cn {
            l = l.with_hex("cn", cn.as_bytes());
        }
        let v = self.call(&l, None)?;
        v.iter().find(|l| l.verb == "csr").and_then(|l| l.hex("pem")).and_then(|p| String::from_utf8(p).ok()).ok_or_else(|| Fail::new(status::FAILED, "cryptad's answer did not parse"))
    }

    fn certificate(&self, c: Certificate) -> Res<()> {
        self.call_file(&Line::new("certificate").with("name", &c.name), c.pem).map(|_| ())
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
    let mut ctl = "/var/run/atrium/crypta.sock".to_string();
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
            _ => die("usage: atrium-crypta-ctl [--ctl PATH] [--dir DIR] [--uid N] [--capsicum] [-v]"),
        }
    }
    let listeners: Vec<(Grant, UnixListener)> = Grant::ALL.iter().map(|&g| (g, listen(&dir, g))).collect();
    let calls = Peer::connect(&ctl);
    if let Some(u) = uid {
        drop_to(u);
    }
    if capsicum {
        cap_enter();
    }
    let (kick, kicks) = mpsc::channel();
    let front = Arc::new(Front { v, calls: Mutex::new(calls), subs: Mutex::new(Vec::new()), kick: Mutex::new(kick) });
    front.log(&format!("serving {} for {ctl}{}", dir.display(), if capsicum { ", in capability mode" } else { "" }));
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

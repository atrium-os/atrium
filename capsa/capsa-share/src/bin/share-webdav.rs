//! atrium-share-webdav: serve one WebDAV share over HTTPS through FUSE
//! (docs/spec/network-shares.md §4, S2).
//!
//! ```text
//! atrium-share-webdav --host NAME [--addr IP] [--port P] --user U
//!     (--trusted | --pin SHA256:...) --root PATH
//!     (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N)
//!     [--ro] [--uid U --gid G] [--ca PEM] [--control-fd N] [--as-uid N]
//! atrium-share-webdav --host NAME [--addr IP] [--port P] [--ca PEM] --learn
//! ```
//!
//! The order is SFTP's: connect, check the server's certificate (through
//! the trust store for `--trusted`, by the key's digest for `--pin`), and
//! only then ask Arca for the share's password, which Arca gives only for
//! the identity just proved. HTTP has no login: the password goes with
//! every request (Basic, inside TLS), so it stays in this process, in
//! locked memory, for as long as the share is connected.
//!
//! HTTP servers close idle connections, and a process in capability mode
//! can't open new ones. So before anything else the server forks a dialer:
//! it does nothing but connect to the one address and port it was given
//! and pass the socket back. The process that speaks TLS, HTTP and XML to
//! the network is the other one, in capability mode.
//!
//! `--learn` connects, prints what the server's certificate says (trusted
//! or not and why, the key's fingerprint, names and expiry) and stops
//! before anything is sent. Exit status 3: the pinned key changed.

use std::fs::File;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use capsa_fuse::server::serve_with;
use capsa_fuse::Options;
use capsa_share::arca::{Arca, KIND_PASSWORD};
use capsa_share::dialer::Dialer;
use capsa_share::mount_self;
use capsa_share::http::{Client, Dial, Request};
use capsa_share::tls::{self, Identity};
use capsa_share::webdav::{Dav, DavFs, Error as DavError};
use zeroize::Zeroizing;

const NAME: &str = "atrium-share-webdav";

fn usage() -> ExitCode {
    eprintln!("usage: {NAME} --host NAME [--addr IP] [--port P] [--ca PEM] (--learn | --user U (--trusted | --pin SHA256:...) --root PATH (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N) [--ro] [--uid U] [--gid G] [--control-fd N])");
    ExitCode::from(2)
}

fn fail(what: &str, e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{NAME}: {what}: {e}");
    ExitCode::FAILURE
}

#[derive(Default)]
struct Args {
    host: String,
    addr: Option<String>,
    port: u16,
    user: String,
    id: Option<Identity>,
    root: String,
    learn: bool,
    ca: String,
    fuse_fd: Option<i32>,
    mnt: Option<String>,
    password_fd: Option<i32>,
    arca: Option<String>,
    as_uid: Option<u32>,
    control_fd: Option<i32>,
    opts: Options,
}

fn args() -> Option<Args> {
    let mut a = Args { port: 443, root: "/".into(), ca: "/etc/ssl/cert.pem".into(), ..Default::default() };
    a.opts.umask = 0o077;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let fd = |v: Option<String>| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2);
        match k.as_str() {
            "--host" => a.host = it.next()?,
            "--addr" => a.addr = Some(it.next()?),
            "--port" => a.port = it.next()?.parse().ok().filter(|&p| p != 0)?,
            "--user" => a.user = it.next()?,
            "--trusted" => a.id = Some(Identity::Trusted),
            "--pin" => a.id = Some(Identity::Pinned(capsa_share::cert::key_of(&it.next()?)?)),
            "--root" => a.root = it.next()?,
            "--learn" => a.learn = true,
            "--ca" => a.ca = it.next()?,
            "--fuse-fd" => a.fuse_fd = Some(fd(it.next())?),
            "--mnt" => a.mnt = Some(it.next()?),
            "--control-fd" => a.control_fd = Some(fd(it.next())?),
            "--password-fd" => a.password_fd = Some(it.next()?.parse::<i32>().ok().filter(|&n| n >= 0)?),
            "--arca" => a.arca = Some(it.next()?),
            "--as-uid" => a.as_uid = Some(it.next()?.parse().ok()?),
            "--ro" => a.opts.read_only = true,
            "--uid" => a.opts.uid = it.next()?.parse().ok()?,
            "--gid" => a.opts.gid = it.next()?.parse().ok()?,
            _ => return None,
        }
    }
    if a.host.is_empty() || (!a.learn && (a.user.is_empty() || a.id.is_none() || (a.fuse_fd.is_none() == a.mnt.is_none()) || (a.arca.is_some() && a.password_fd.is_some()) || (a.as_uid.is_some() && a.mnt.is_none()))) {
        return None;
    }
    Some(a)
}

/// How the share server gets a connection: from the dialer, or (`--learn`,
/// the parent before the fork) directly.
enum Line {
    Dialer(Dialer),
    Direct(SocketAddr),
}

struct TlsDial {
    line: Line,
    cfg: Arc<rustls::ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
}

impl TlsDial {
    fn tcp(&mut self) -> io::Result<TcpStream> {
        match &mut self.line {
            Line::Direct(a) => TcpStream::connect_timeout(a, Duration::from_secs(15)),
            Line::Dialer(d) => d.tcp(),
        }
    }
}

impl Dial for TlsDial {
    type Stream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
    fn dial(&mut self) -> io::Result<Self::Stream> {
        let mut t = self.tcp()?;
        let _ = t.set_read_timeout(Some(Duration::from_secs(25)));
        let _ = t.set_write_timeout(Some(Duration::from_secs(25)));
        let _ = t.set_nodelay(true);
        let mut c = rustls::ClientConnection::new(self.cfg.clone(), self.name.clone()).map_err(io::Error::other)?;
        // The certificate is checked here, before a byte of a request.
        while c.is_handshaking() {
            c.complete_io(&mut t)?;
        }
        Ok(rustls::StreamOwned::new(c, t))
    }
}

/// Development without Arca: the password from a descriptor, read once.
fn read_password(fd: i32) -> Result<Zeroizing<Vec<u8>>, String> {
    // SAFETY: the caller handed us this descriptor for this one read.
    let mut f = unsafe { File::from_raw_fd(fd) };
    let mut buf = Zeroizing::new(Vec::with_capacity(1025));
    (&mut f).take(1025).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > 1024 {
        return Err("longer than 1024 bytes".into());
    }
    while buf.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        buf.pop();
    }
    Ok(buf)
}

fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> ExitCode {
    let Some(a) = args() else { return usage() };
    let mut early: Option<File> = None;
    if let (Some(uid), Some(dir)) = (a.as_uid, &a.mnt) {
        match mount_self(dir) {
            Ok(f) => early = Some(f),
            Err(e) => return fail("mount", e),
        }
        // SAFETY: plain syscalls on our own credentials.
        if unsafe { libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(uid) != 0 || libc::setuid(uid) != 0 } {
            return fail("--as-uid", io::Error::last_os_error());
        }
    }
    let id = if a.learn { Identity::Learn } else { a.id.unwrap() };
    // The trust store, read now: nothing is opened later.
    let roots = match id {
        Identity::Pinned(_) => Vec::new(),
        _ => match std::fs::read(&a.ca) {
            Ok(r) => r,
            Err(e) if id == Identity::Learn => {
                eprintln!("{NAME}: trust store {}: {e}", a.ca);
                Vec::new()
            }
            Err(e) => return fail("trust store", format!("{}: {e}", a.ca)),
        },
    };
    let (cfg, verifier) = match tls::config(id, &roots) {
        Ok(c) => c,
        Err(e) => return fail("tls", e),
    };
    let name = match tls::server_name(&a.host) {
        Ok(n) => n,
        Err(e) => return fail("host", e),
    };
    // One address, resolved once.
    let dial_host = a.addr.clone().unwrap_or_else(|| a.host.clone());
    let to = match (dial_host.as_str(), a.port).to_socket_addrs().ok().and_then(|mut i| i.next()) {
        Some(t) => t,
        None => return fail("address", format!("{dial_host} doesn't resolve")),
    };

    if a.learn {
        let mut d = TlsDial { line: Line::Direct(to), cfg, name };
        let r = d.dial();
        let seen = verifier.seen.lock().unwrap().clone();
        let Some(seen) = seen else {
            return fail("connect", r.err().map_or("no certificate".to_string(), |e| e.to_string()));
        };
        match &seen.untrusted {
            None => println!("trusted"),
            Some(why) => println!("untrusted {}", why.replace('\n', " ")),
        }
        println!("{}", capsa_share::cert::fingerprint(&seen.summary.spki_sha256));
        println!("key {}", hexs(&seen.summary.spki_sha256));
        println!("subject {}", seen.summary.subject);
        println!("issuer {}", seen.summary.issuer);
        println!("expires {}", seen.summary.not_after);
        return ExitCode::SUCCESS;
    }

    let line = match Dialer::start(to, None) {
        Ok(d) => Line::Dialer(d),
        Err(e) => return fail("dialer", e),
    };
    let host_header = if a.port == 443 { a.host.clone() } else { format!("{}:{}", a.host, a.port) };
    let mut http = Client::new(TlsDial { line, cfg, name }, &host_header);
    // Prove the server first: the handshake checks its certificate, and
    // an OPTIONS without credentials says it speaks WebDAV.
    let root_wire = {
        let mut w = String::new();
        for seg in a.root.split('/').filter(|s| !s.is_empty()) {
            w.push('/');
            w.push_str(&capsa_share::http::encode_segment(seg));
        }
        w.push('/');
        w
    };
    match http.send(&Request { method: "OPTIONS", path: &root_wire, headers: vec![], body: b"", limit: 64 * 1024 }) {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("KEY CHANGED") {
                // A changed key is a hard stop (§5.1): nothing secret was read.
                eprintln!("{NAME}: {msg}");
                return ExitCode::from(3);
            }
            return fail("connect", msg);
        }
    }
    // The server has proved who it is: now the password.
    let pw = if let Some(sock) = &a.arca {
        let r = (|| -> Result<Zeroizing<Vec<u8>>, String> {
            let mut arca = Arca::connect(sock, id.arca_key()).map_err(|e| format!("arca: {e}"))?;
            let items = arca.items().map_err(|e| format!("arca: {e}"))?;
            let item = items.iter().find(|i| i.kind == KIND_PASSWORD && i.account == a.user).ok_or("no saved password for this share")?;
            arca.fill(&item.id).map_err(|e| format!("arca: {e}"))
        })();
        match r {
            Ok(p) => p,
            Err(e) => return fail("login", e),
        }
    } else {
        match a.password_fd.map(read_password).unwrap_or_else(|| Ok(Zeroizing::new(Vec::new()))) {
            Ok(p) => p,
            Err(e) => return fail("password", e),
        }
    };
    http.set_basic(&a.user, &pw);
    drop(pw);
    // SAFETY: best effort; the credential's pages stay out of swap.
    #[cfg(target_os = "freebsd")]
    unsafe {
        libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
    }
    let origin = format!("https://{host_header}");
    let fs = match DavFs::new(Dav::new(http, &origin), &a.root, !a.opts.read_only) {
        Ok(f) => f.exit_on_loss(),
        Err(DavError::Auth(401)) => return fail("login", "the server didn't accept the user name and password"),
        Err(e) => return fail("share root", e),
    };
    let fuse = match (a.fuse_fd, &a.mnt) {
        _ if early.is_some() => early.take().unwrap(),
        // SAFETY: jaild handed us the /dev/fuse descriptor.
        (Some(fd), _) => unsafe { File::from_raw_fd(fd) },
        (None, Some(dir)) => match mount_self(dir) {
            Ok(f) => f,
            Err(e) => return fail("mount", e),
        },
        _ => return usage(),
    };
    // Capability mode: new connections come from the dialer.
    #[cfg(target_os = "freebsd")]
    // SAFETY: cap_enter takes no arguments and only restricts this process.
    if unsafe { libc::cap_enter() } != 0 {
        return fail("cap_enter", io::Error::last_os_error());
    }
    // SAFETY: jaild handed us this socket as `control_fd`; owned once, here.
    let control = a.control_fd.map(|n| unsafe { UnixStream::from_raw_fd(n) });
    match serve_with(fs, fuse, a.opts, control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("/dev/fuse", e),
    }
}

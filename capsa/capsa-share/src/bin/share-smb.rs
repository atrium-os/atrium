//! atrium-share-smb: serve one SMB 3.1.1 share through FUSE
//! (docs/spec/network-shares.md §5.3, S3).
//!
//! ```text
//! atrium-share-smb --host NAME [--addr IP] [--port 445] --user [DOMAIN\]U
//!     --pin SHA256:... --root SHARE[/FOLDER]
//!     (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N)
//!     [--ro] [--uid U --gid G] [--control-fd N] [--as-uid N]
//! atrium-share-smb --host NAME [--addr IP] [--port 445] --learn
//! ```
//!
//! The order is SFTP's: connect and negotiate (SMB 3.1.1, SHA-512
//! pre-authentication integrity, AES-GCM, or nothing), compare the
//! server's GUID with the one the share was saved with, and only then ask
//! Arca for the password, which Arca gives only for that identity. The
//! GUID is a hint, not proof (§5.3: a password protocol can't prove the
//! server); what it does catch is a different machine at the address. The
//! password is used for the one login and wiped. Every message after the
//! login is encrypted.
//!
//! `--learn` negotiates, prints what the server is (dialect, cipher, GUID,
//! and the fingerprint of the GUID that a share is pinned to) or why it is
//! refused (`refused <why>`), and stops before any login. Exit status 3:
//! the GUID changed. With Arca, a lost connection leaves the mount (S4):
//! requests fail at once, and the next one connects again through the
//! dialer, checking the GUID and logging in again; status 3 or 4 ends the
//! share when the server changed or the login is refused.

use std::fs::File;
use std::io::{self, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use capsa_fuse::server::serve_with;
use capsa_fuse::Options;
use capsa_share::arca::{Arca, KIND_PASSWORD};
use capsa_share::cert::{fingerprint, key_of};
use capsa_share::dialer::{Dialer, CONNECT_WAIT};
use capsa_share::mount_self;
use capsa_share::smb::{cipher_name, guid_key, guid_text, smb_name, Conn, Error as SmbError, SmbFs, STATUS_BAD_NETWORK_NAME};
use zeroize::Zeroizing;

const NAME: &str = "atrium-share-smb";

fn usage() -> ExitCode {
    eprintln!("usage: {NAME} --host NAME [--addr IP] [--port P] (--learn | --user [DOMAIN\\]U --pin SHA256:... --root SHARE[/FOLDER] (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N) [--ro] [--uid U] [--gid G] [--control-fd N])");
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
    pin: Option<[u8; 32]>,
    root: String,
    learn: bool,
    fuse_fd: Option<i32>,
    mnt: Option<String>,
    password_fd: Option<i32>,
    arca: Option<String>,
    as_uid: Option<u32>,
    control_fd: Option<i32>,
    opts: Options,
}

fn args() -> Option<Args> {
    let mut a = Args { port: 445, ..Default::default() };
    a.opts.umask = 0o077;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let fd = |v: Option<String>| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2);
        match k.as_str() {
            "--host" => a.host = it.next()?,
            "--addr" => a.addr = Some(it.next()?),
            "--port" => a.port = it.next()?.parse().ok().filter(|&p| p != 0)?,
            "--user" => a.user = it.next()?,
            "--pin" => a.pin = Some(key_of(&it.next()?)?),
            "--root" => a.root = it.next()?,
            "--learn" => a.learn = true,
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
    if a.host.is_empty() || (!a.learn && (a.user.is_empty() || a.pin.is_none() || a.root.trim_matches('/').is_empty() || (a.fuse_fd.is_none() == a.mnt.is_none()) || (a.arca.is_some() && a.password_fd.is_some()) || (a.as_uid.is_some() && a.mnt.is_none()))) {
        return None;
    }
    Some(a)
}

/// Development without Arca: the password from a descriptor, read once.
fn read_password(fd: Option<i32>) -> Result<Zeroizing<Vec<u8>>, String> {
    let Some(fd) = fd else { return Ok(Zeroizing::new(Vec::new())) };
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

/// How long the server may stay silent before a request fails and the
/// connection counts as lost: under fusefs' 30 s per request.
const SILENT: Duration = Duration::from_secs(20);

/// What a connection must reach: the same server, share and account.
struct Session {
    host: String,
    share: String,
    user: String,
    pin: [u8; 32],
}

/// An error and what it means: 1 try again later, 3 a different server,
/// 4 refused for good.
type Fail = (u8, String);

impl Session {
    /// Negotiate over `sock`, check the server's GUID against the pin,
    /// and only then take the password from `password` and log in.
    fn open(&self, sock: TcpStream, password: impl FnOnce([u8; 32]) -> Result<Zeroizing<Vec<u8>>, Fail>) -> Result<Conn<TcpStream>, Fail> {
        // A server that goes silent fails the request rather than hang it.
        let _ = sock.set_read_timeout(Some(SILENT));
        let _ = sock.set_write_timeout(Some(SILENT));
        let _ = sock.set_nodelay(true);
        let mut c = match Conn::negotiate(sock) {
            Ok(c) => c,
            // Below the floor now: not the server the share was saved with.
            Err(SmbError::Refused(why)) => return Err((3, format!("negotiate: refused: {why}"))),
            Err(e) => return Err((1, format!("negotiate: {e}"))),
        };
        let key = guid_key(&c.negotiated.guid);
        if key != self.pin {
            // A different machine at the address: a hard stop (§5.1),
            // before anything secret is read.
            return Err((3, format!("SERVER CHANGED: it now reports GUID {} ({}), not the one this share was saved with", guid_text(&c.negotiated.guid), fingerprint(&key))));
        }
        let pw = password(key)?;
        let r = c.login(&self.user, &pw);
        // Used once (§5.1): gone before anything else happens.
        drop(pw);
        match r {
            Ok(()) => {}
            Err(e @ SmbError::Auth) => return Err((4, format!("login: {e}"))),
            Err(e) => return Err((1, format!("login: {e}"))),
        }
        match c.tree_connect(&format!("\\\\{}\\{}", self.host, self.share)) {
            Ok(()) => Ok(c),
            Err(SmbError::Status(STATUS_BAD_NETWORK_NAME)) => Err((4, format!("share: the server has no share named {}", self.share))),
            Err(e) => Err((1, format!("share: {e}"))),
        }
    }
}

/// The share's password from Arca, for the identity `key` just checked.
fn arca_password(sock: io::Result<UnixStream>, key: [u8; 32], user: &str) -> Result<Zeroizing<Vec<u8>>, Fail> {
    let soft = |e: String| (1, format!("arca: {e}"));
    let mut arca = Arca::from_stream(sock.map_err(|e| soft(e.to_string()))?, key).map_err(|e| soft(e.to_string()))?;
    let items = arca.items().map_err(|e| soft(e.to_string()))?;
    let item = items.iter().find(|i| i.kind == KIND_PASSWORD && i.account == user).ok_or((4, "login: no saved password for this share".to_string()))?;
    arca.fill(&item.id).map_err(|e| soft(e.to_string()))
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
    // The share and the folder in it.
    let mut parts = a.root.split('/').filter(|s| !s.is_empty());
    let share = parts.next().unwrap_or("").to_string();
    let folder: Vec<&str> = parts.collect();
    if !a.learn && (!smb_name(&share) || folder.iter().any(|s| !smb_name(s))) {
        return fail("root", "not a share name and folder SMB can use");
    }
    let dial = a.addr.clone().unwrap_or_else(|| a.host.clone());
    let Some(to) = (dial.as_str(), a.port).to_socket_addrs().ok().and_then(|mut i| i.next()) else {
        return fail("address", format!("{dial} doesn't resolve"));
    };
    if a.learn {
        let sock = match TcpStream::connect_timeout(&to, CONNECT_WAIT) {
            Ok(s) => s,
            Err(e) => return fail("connect", e),
        };
        let _ = sock.set_read_timeout(Some(SILENT));
        let c = match Conn::negotiate(sock) {
            Ok(c) => c,
            Err(SmbError::Refused(why)) => {
                println!("refused {why}");
                return ExitCode::FAILURE;
            }
            Err(e) => return fail("negotiate", e),
        };
        let key = guid_key(&c.negotiated.guid);
        println!("smb 3.1.1 {}", cipher_name(c.negotiated.cipher));
        println!("guid {}", guid_text(&c.negotiated.guid));
        println!("{}", fingerprint(&key));
        println!("key {}", key.iter().map(|b| format!("{b:02x}")).collect::<String>());
        return ExitCode::SUCCESS;
    }
    let s = Session { host: a.host.clone(), share, user: a.user.clone(), pin: a.pin.unwrap_or_default() };
    let fs = if a.arca.is_some() {
        // Every connection, the first and any after a loss, comes from
        // the dialer, so that this process can reconnect from inside
        // capability mode (S4).
        let mut d = match Dialer::start(to, a.arca.clone()) {
            Ok(d) => d,
            Err(e) => return fail("dialer", e),
        };
        let c = match d.tcp().map_err(|e| (1, format!("connect: {e}"))).and_then(|t| s.open(t, |key| arca_password(d.arca(), key, &s.user))) {
            Ok(c) => c,
            Err((3, e)) => {
                eprintln!("{NAME}: {e}");
                return ExitCode::from(3);
            }
            Err((_, e)) => return fail("connect", e),
        };
        let again = move || -> Result<Conn<TcpStream>, String> {
            let t = d.tcp().map_err(|e| format!("connect: {e}"))?;
            match s.open(t, |key| arca_password(d.arca(), key, &s.user)) {
                Ok(c) => Ok(c),
                Err((1, e)) => Err(e),
                // Final: a different server, or the login or share
                // refused for good. The share ends; dispensatord reaps it.
                Err((code, e)) => {
                    eprintln!("{NAME}: {e}: ending the share");
                    std::process::exit(code.into());
                }
            }
        };
        match SmbFs::new(c, &folder.join("/"), !a.opts.read_only) {
            Ok(f) => f.reconnect_with(Box::new(again)),
            Err(e) => return fail("share root", e),
        }
    } else {
        // Development without Arca: one connection, and the share ends
        // with it.
        let t = match TcpStream::connect_timeout(&to, CONNECT_WAIT) {
            Ok(t) => t,
            Err(e) => return fail("connect", e),
        };
        let c = match s.open(t, |_| read_password(a.password_fd).map_err(|e| (1, format!("password: {e}")))) {
            Ok(c) => c,
            Err((3, e)) => {
                eprintln!("{NAME}: {e}");
                return ExitCode::from(3);
            }
            Err((_, e)) => return fail("connect", e),
        };
        match SmbFs::new(c, &folder.join("/"), !a.opts.read_only) {
            Ok(f) => f.exit_on_loss(),
            Err(e) => return fail("share root", e),
        }
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
    // Capability mode: the socket and /dev/fuse are all this process
    // touches from here on.
    #[cfg(target_os = "freebsd")]
    // SAFETY: cap_enter takes no arguments and only restricts this process.
    if unsafe { libc::cap_enter() } != 0 {
        return fail("cap_enter", io::Error::last_os_error());
    }
    // SAFETY: jaild handed us this socket as `control_fd`; owned once, here.
    let control = a.control_fd.map(|n| unsafe { std::os::unix::net::UnixStream::from_raw_fd(n) });
    match serve_with(fs, fuse, a.opts, control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("/dev/fuse", e),
    }
}

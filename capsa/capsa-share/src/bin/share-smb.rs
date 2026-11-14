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
//! the GUID changed.

use std::fs::File;
use std::io::{self, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Duration;

use capsa_fuse::server::serve_with;
use capsa_fuse::Options;
use capsa_share::arca::{Arca, KIND_PASSWORD};
use capsa_share::cert::{fingerprint, key_of};
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
    let sock = match TcpStream::connect_timeout(&to, Duration::from_secs(15)) {
        Ok(s) => s,
        Err(e) => return fail("connect", e),
    };
    // A server that goes silent ends the share rather than hang it.
    let _ = sock.set_read_timeout(Some(Duration::from_secs(25)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(25)));
    let _ = sock.set_nodelay(true);
    let mut c = match Conn::negotiate(sock) {
        Ok(c) => c,
        Err(SmbError::Refused(why)) if a.learn => {
            println!("refused {why}");
            return ExitCode::FAILURE;
        }
        Err(e) => return fail("negotiate", e),
    };
    let key = guid_key(&c.negotiated.guid);
    if a.learn {
        println!("smb 3.1.1 {}", cipher_name(c.negotiated.cipher));
        println!("guid {}", guid_text(&c.negotiated.guid));
        println!("{}", fingerprint(&key));
        println!("key {}", key.iter().map(|b| format!("{b:02x}")).collect::<String>());
        return ExitCode::SUCCESS;
    }
    if Some(key) != a.pin {
        // A different machine at the address: a hard stop (§5.1), before
        // anything secret is read.
        eprintln!("{NAME}: SERVER CHANGED: it now reports GUID {} ({}), not the one this share was saved with", guid_text(&c.negotiated.guid), fingerprint(&key));
        return ExitCode::from(3);
    }
    let pw = if let Some(sock) = &a.arca {
        let r = (|| -> Result<Zeroizing<Vec<u8>>, String> {
            let mut arca = Arca::connect(sock, key).map_err(|e| format!("arca: {e}"))?;
            let items = arca.items().map_err(|e| format!("arca: {e}"))?;
            let item = items.iter().find(|i| i.kind == KIND_PASSWORD && i.account == a.user).ok_or("no saved password for this share")?;
            arca.fill(&item.id).map_err(|e| format!("arca: {e}"))
        })();
        match r {
            Ok(p) => p,
            Err(e) => return fail("login", e),
        }
    } else {
        match read_password(a.password_fd) {
            Ok(p) => p,
            Err(e) => return fail("password", e),
        }
    };
    let r = c.login(&a.user, &pw);
    // Used once (§5.1): gone before anything else happens.
    drop(pw);
    if let Err(e) = r {
        return fail("login", e);
    }
    match c.tree_connect(&format!("\\\\{}\\{share}", a.host)) {
        Ok(()) => {}
        Err(SmbError::Status(STATUS_BAD_NETWORK_NAME)) => return fail("share", format!("the server has no share named {share}")),
        Err(e) => return fail("share", e),
    }
    let fs = match SmbFs::new(c, &folder.join("/"), !a.opts.read_only) {
        Ok(f) => f.exit_on_loss(),
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

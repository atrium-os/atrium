//! atrium-share-sftp: serve one SFTP share over FUSE
//! (docs/spec/network-shares.md §4, S1).
//!
//! ```text
//! atrium-share-sftp --host H [--port P] --user U --pin SHA256:... --root PATH
//!     (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N) [--ro] [--uid U --gid G]
//! atrium-share-sftp --host H [--port P] --learn
//! ```
//!
//! The order is the point: connect, check the host key against the pin,
//! and only then ask Arca for this share's credential: Arca knows the share
//! from this process's uid and answers only for the identity just proved
//! (§5.1). A share's own Ed25519 key is preferred (Arca signs; the key never
//! enters this process), else its password, used for the one login and
//! wiped. `--password-fd` is the development path without Arca. Then log
//! in, start sftp, enter Capsicum, serve. `--learn` connects, prints the server's fingerprint for
//! the user to accept, and stops before anything secret is involved.
//! `--mnt` mounts by itself with jaild's options (root, tests only);
//! `--as-uid N` then mounts first and drops to uid and gid N before
//! connecting, standing in for jaild's launch as a share-server uid.

use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Duration;

use capsa_fuse::server::serve_with;
use capsa_fuse::Options;
use capsa_share::arca::{Arca, KIND_ED25519, KIND_PASSWORD};
use capsa_share::mount_self;
use capsa_share::sftp::Sftp;
use capsa_share::SftpFs;
use capsa_ssh::{Auth, Client, HostKeyPolicy};
use zeroize::Zeroizing;

fn usage() -> ExitCode {
    eprintln!("usage: atrium-share-sftp --host H [--port P] (--learn | --user U --pin SHA256:... --root PATH (--fuse-fd N | --mnt DIR) (--arca SOCKET | --password-fd N) [--ro] [--uid U] [--gid G])");
    ExitCode::from(2)
}

fn fail(what: &str, e: impl std::fmt::Display) -> ExitCode {
    eprintln!("atrium-share-sftp: {what}: {e}");
    ExitCode::FAILURE
}

#[derive(Default)]
struct Args {
    host: String,
    port: u16,
    user: String,
    pin: Option<String>,
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
    let mut a = Args { port: 22, root: ".".into(), ..Default::default() };
    // The share is the session user's alone, as a stick is (§4.4): owner
    // bits only, whatever modes the server reports.
    a.opts.umask = 0o077;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let fd = |v: Option<String>| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2);
        match k.as_str() {
            "--host" => a.host = it.next()?,
            "--port" => a.port = it.next()?.parse().ok()?,
            "--user" => a.user = it.next()?,
            "--pin" => a.pin = Some(it.next()?).filter(|p| p.starts_with("SHA256:")),
            "--root" => a.root = it.next()?,
            "--learn" => a.learn = true,
            "--fuse-fd" => a.fuse_fd = Some(fd(it.next())?),
            "--mnt" => a.mnt = Some(it.next()?),
            // Grants of folders arrive here (device-assignment.md §4.8), as for a stick.
            "--control-fd" => a.control_fd = Some(fd(it.next())?),
            // Any descriptor, stdin included: it is read once and closed.
            "--password-fd" => a.password_fd = Some(it.next()?.parse::<i32>().ok().filter(|&n| n >= 0)?),
            "--arca" => a.arca = Some(it.next()?),
            "--as-uid" => a.as_uid = Some(it.next()?.parse().ok()?),
            "--ro" => a.opts.read_only = true,
            "--uid" => a.opts.uid = it.next()?.parse().ok()?,
            "--gid" => a.opts.gid = it.next()?.parse().ok()?,
            _ => return None,
        }
    }
    if a.host.is_empty() || (!a.learn && (a.user.is_empty() || a.pin.is_none() || (a.fuse_fd.is_none() == a.mnt.is_none()) || (a.arca.is_some() && a.password_fd.is_some()) || (a.as_uid.is_some() && a.mnt.is_none()))) {
        return None;
    }
    Some(a)
}


/// The share's credential from Arca, for the identity this connection
/// proved: its key if it has one for `user`, else its password.
fn login_with_arca(ssh: &mut Client<TcpStream>, sock: &str, user: &str) -> Result<(), String> {
    let mut arca = Arca::connect(sock, ssh.host_key_digest()).map_err(|e| format!("arca: {e}"))?;
    let items = arca.items().map_err(|e| format!("arca: {e}"))?;
    let mine = |k: u16| items.iter().find(|i| i.kind == k && i.account == user).map(|i| i.id);
    if let Some(id) = mine(KIND_ED25519) {
        let public = arca.pubkey(&id).map_err(|e| format!("arca: {e}"))?;
        let mut sign = |m: &[u8]| arca.sign(&id, m).map_err(|e| e.to_string());
        return ssh.login(user, Auth::Ed25519 { public, sign: &mut sign }, "sftp").map_err(|e| e.to_string());
    }
    let id = mine(KIND_PASSWORD).ok_or("no saved credential for this share")?;
    let pw = arca.fill(&id).map_err(|e| format!("arca: {e}"))?;
    ssh.login(user, Auth::Password(&pw), "sftp").map_err(|e| e.to_string())
}

/// Development without Arca: the password from a descriptor, read once.
fn read_password(fd: Option<i32>) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut pw = Zeroizing::new(Vec::new());
    let Some(fd) = fd else { return Ok(pw) };
    // SAFETY: the caller handed us this descriptor for this one read.
    let mut f = unsafe { File::from_raw_fd(fd) };
    let mut buf = Zeroizing::new([0u8; 1025]);
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(e.to_string()),
        }
    }
    if n > 1024 {
        return Err("longer than 1024 bytes".into());
    }
    let mut end = n;
    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
        end -= 1;
    }
    pw.extend_from_slice(&buf[..end]);
    Ok(pw)
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
            return fail("--as-uid", std::io::Error::last_os_error());
        }
    }
    let sock = match TcpStream::connect((a.host.as_str(), a.port)) {
        Ok(s) => s,
        Err(e) => return fail("connect", e),
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(25)));
    let _ = sock.set_nodelay(true);
    let policy = match &a.pin {
        Some(p) if !a.learn => HostKeyPolicy::Pinned(p.clone()),
        _ => HostKeyPolicy::Learn,
    };
    let mut ssh = match Client::connect(sock, &policy) {
        Ok(c) => c,
        Err(e @ capsa_ssh::Error::HostKeyMismatch { .. }) => {
            // A changed key is a hard stop (§5.1): nothing secret was read.
            eprintln!("atrium-share-sftp: {e}");
            return ExitCode::from(3);
        }
        Err(e) => return fail("ssh", e),
    };
    if a.learn {
        // The fingerprint for the user; the digest that Arca pins.
        println!("{}", ssh.host_fingerprint());
        println!("key {}", ssh.host_key_digest().iter().map(|b| format!("{b:02x}")).collect::<String>());
        ssh.disconnect();
        return ExitCode::SUCCESS;
    }
    // The server has proved it holds the pinned key: now the secret.
    if let Some(sock) = &a.arca {
        if let Err(e) = login_with_arca(&mut ssh, sock, &a.user) {
            return fail("login", e);
        }
    } else {
        let pw = match read_password(a.password_fd) {
            Ok(p) => p,
            Err(e) => return fail("password", e),
        };
        if let Err(e) = ssh.login(&a.user, Auth::Password(&pw), "sftp") {
            return fail("login", e);
        }
    }
    let s = match Sftp::start(ssh) {
        Ok(s) => s,
        Err(e) => return fail("sftp", e),
    };
    let fs = match SftpFs::new(s, &a.root, !a.opts.read_only) {
        // A server that stops answering (25 s per read) or a dropped
        // connection ends the share rather than leaving one that fails.
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
    // Capability mode from here on: the socket and /dev/fuse are all this
    // process ever touches again.
    #[cfg(target_os = "freebsd")]
    // SAFETY: cap_enter takes no arguments and only restricts this process.
    if unsafe { libc::cap_enter() } != 0 {
        return fail("cap_enter", std::io::Error::last_os_error());
    }
    // SAFETY: jaild handed us this socket as `control_fd`; owned once, here.
    let control = a.control_fd.map(|n| unsafe { std::os::unix::net::UnixStream::from_raw_fd(n) });
    match serve_with(fs, fuse, a.opts, control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("/dev/fuse", e),
    }
}

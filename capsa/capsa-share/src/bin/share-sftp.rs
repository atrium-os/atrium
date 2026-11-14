//! atrium-share-sftp: serve one SFTP share over FUSE
//! (docs/spec/network-shares.md §4, S1).
//!
//! ```text
//! atrium-share-sftp --host H [--port P] --user U --pin SHA256:... --root PATH
//!     (--fuse-fd N | --mnt DIR) [--password-fd N] [--ro] [--uid U --gid G]
//! atrium-share-sftp --host H [--port P] --learn
//! ```
//!
//! The order is the point: connect, check the host key against the pin,
//! and only then read the password (later: ask Arca for it, which releases
//! it only to the pinned identity, §5.1); log in, start sftp, enter
//! Capsicum, serve. `--learn` connects, prints the server's fingerprint for
//! the user to accept, and stops before anything secret is involved.
//! `--mnt` mounts by itself with jaild's options (root, tests only).

use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Duration;

use capsa_fuse::server::serve_with;
use capsa_fuse::Options;
use capsa_share::sftp::Sftp;
use capsa_share::SftpFs;
use capsa_ssh::{Auth, Client, HostKeyPolicy};
use zeroize::Zeroizing;

fn usage() -> ExitCode {
    eprintln!("usage: atrium-share-sftp --host H [--port P] (--learn | --user U --pin SHA256:... --root PATH (--fuse-fd N | --mnt DIR) [--password-fd N] [--ro] [--uid U] [--gid G])");
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
    opts: Options,
}

fn args() -> Option<Args> {
    let mut a = Args { port: 22, root: ".".into(), ..Default::default() };
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
            // Any descriptor, stdin included: it is read once and closed.
            "--password-fd" => a.password_fd = Some(it.next()?.parse::<i32>().ok().filter(|&n| n >= 0)?),
            "--ro" => a.opts.read_only = true,
            "--uid" => a.opts.uid = it.next()?.parse().ok()?,
            "--gid" => a.opts.gid = it.next()?.parse().ok()?,
            _ => return None,
        }
    }
    if a.host.is_empty() || (!a.learn && (a.user.is_empty() || a.pin.is_none() || (a.fuse_fd.is_none() == a.mnt.is_none()))) {
        return None;
    }
    Some(a)
}

/// Mounts fusefs on `dir` with jaild's options (portcullis/jaild ffi.rs).
#[cfg(target_os = "freebsd")]
fn mount_self(dir: &str) -> std::io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let dev = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse")?;
    let fd = dev.as_raw_fd().to_string();
    let opts: Vec<(&str, Option<&str>)> = vec![
        ("fstype", Some("fusefs")),
        ("fspath", Some(dir)),
        ("from", Some("/dev/fuse")),
        ("fd", Some(&fd)),
        ("allow_other", None),
        ("default_permissions", None),
        ("timeout=", Some("30")),
    ];
    let mut keep: Vec<CString> = Vec::new();
    let mut iov: Vec<libc::iovec> = Vec::new();
    for (k, v) in opts {
        let kc = CString::new(k).unwrap();
        iov.push(libc::iovec { iov_base: kc.as_ptr() as *mut _, iov_len: kc.as_bytes_with_nul().len() });
        keep.push(kc);
        match v {
            Some(v) => {
                let vc = CString::new(v).unwrap();
                iov.push(libc::iovec { iov_base: vc.as_ptr() as *mut _, iov_len: vc.as_bytes_with_nul().len() });
                keep.push(vc);
            }
            None => iov.push(libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }),
        }
    }
    // SAFETY: iov points into `keep`, alive across the call.
    let rc = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as u32, libc::MNT_NOSUID | libc::MNT_NOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(dev)
}

#[cfg(not(target_os = "freebsd"))]
fn mount_self(_dir: &str) -> std::io::Result<File> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "--mnt needs FreeBSD"))
}

fn main() -> ExitCode {
    let Some(a) = args() else { return usage() };
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
        println!("{}", ssh.host_fingerprint());
        ssh.disconnect();
        return ExitCode::SUCCESS;
    }
    // The server has proved it holds the pinned key: now the secret.
    let mut pw = Zeroizing::new(Vec::new());
    if let Some(fd) = a.password_fd {
        // SAFETY: the caller handed us this descriptor for this one read.
        let mut f = unsafe { File::from_raw_fd(fd) };
        let mut buf = Zeroizing::new([0u8; 1025]);
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return fail("password", e),
            }
        }
        if n > 1024 {
            return fail("password", "longer than 1024 bytes");
        }
        let mut end = n;
        while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
            end -= 1;
        }
        pw.extend_from_slice(&buf[..end]);
    }
    if let Err(e) = ssh.login(&a.user, Auth::Password(&pw), "sftp") {
        return fail("login", e);
    }
    drop(pw);
    let s = match Sftp::start(ssh) {
        Ok(s) => s,
        Err(e) => return fail("sftp", e),
    };
    let fs = match SftpFs::new(s, &a.root, !a.opts.read_only) {
        Ok(f) => f,
        Err(e) => return fail("share root", e),
    };
    let fuse = match (a.fuse_fd, &a.mnt) {
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
    match serve_with(fs, fuse, a.opts, None) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("/dev/fuse", e),
    }
}

//! fetcher-standin: portcullisd's `OpenFetcher` and nothing else, for VM checks that run an
//! app outside an app jail (scripts/hospes-h4-vm.sh plays Widevine content as `nobody`, as
//! the H3 check does). It speaks the same handshake as portcullisd (Hello, OpenFetcher,
//! ReadyForFds, three fds) and starts the real `nuntiusd` on the app's channel, under an
//! unprivileged uid. What it does NOT do is jail the fetcher, or check the caller's
//! `net-fetch`: that is portcullisd's job, checked by scripts/nuntius-vm-check.sh. Never
//! install it on a system.
//!
//!   fetcher-standin SOCKET NUNTIUSD TRUST_PEM UID [NUNTIUSD_ARG...]
//!
//! Extra arguments go to nuntiusd (`--grant-private-network` for a relay on loopback).
use std::io::BufReader;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use portcullis_ipc::{read_request, recv_fds, write_response, Request, Response, PROTO_VERSION};

fn serve(conn: UnixStream, nuntiusd: &str, trust: &str, uid: u32, extra: &[String]) -> std::io::Result<()> {
    let mut r = BufReader::new(conn.try_clone()?);
    let mut w = conn;
    match read_request(&mut r)? {
        Request::Hello { .. } => write_response(&mut w, &Response::Hello { version: PROTO_VERSION })?,
        other => return Err(std::io::Error::other(format!("expected Hello, got {other:?}"))),
    }
    match read_request(&mut r)? {
        Request::OpenFetcher => write_response(&mut w, &Response::ReadyForFds)?,
        other => return Err(std::io::Error::other(format!("expected OpenFetcher, got {other:?}"))),
    }
    let fds = recv_fds(&w, 3)?;
    if fds.len() != 3 {
        return Err(std::io::Error::other("expected three fds"));
    }
    let mut fds = fds.into_iter().map(|f| f.into_raw_fd());
    let (i, o, e) = (fds.next().unwrap(), fds.next().unwrap(), fds.next().unwrap());
    // SAFETY: each fd came from recv_fds and is owned by exactly one Stdio here.
    let (si, so, se) = unsafe { (Stdio::from_raw_fd(i), Stdio::from_raw_fd(o), Stdio::from_raw_fd(e)) };
    let mut child = Command::new(nuntiusd).args(["--trust", trust]).args(extra).stdin(si).stdout(so).stderr(se).uid(uid).gid(uid).spawn()?;
    eprintln!("fetcher-standin: nuntiusd {} for peer on fd {}", child.id(), w.as_raw_fd());
    // As portcullisd: the fetcher lives until the app hangs up or it exits.
    let _ = child.wait();
    Ok(())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: fetcher-standin SOCKET NUNTIUSD TRUST_PEM UID [NUNTIUSD_ARG...]");
        std::process::exit(2);
    }
    let uid: u32 = a[4].parse().expect("UID");
    let _ = std::fs::remove_file(&a[1]);
    if let Some(d) = std::path::Path::new(&a[1]).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let l = UnixListener::bind(&a[1]).expect("bind");
    std::fs::set_permissions(&a[1], std::fs::Permissions::from_mode(0o666)).expect("chmod");
    eprintln!("fetcher-standin: listening on {}", a[1]);
    for c in l.incoming().flatten() {
        let (n, t, x) = (a[2].clone(), a[3].clone(), a[5..].to_vec());
        std::thread::spawn(move || {
            if let Err(e) = serve(c, &n, &t, uid, &x) {
                eprintln!("fetcher-standin: {e}");
            }
        });
    }
}

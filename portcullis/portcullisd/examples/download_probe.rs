//! download_probe: a test app for scripts/vm-personas-check.sh (personas.md
//! §4.1, P2). Run as a jailed app, it hands portcullisd a small PDF as a
//! sealed memory file with `SaveDownload`, as the Navigator does after a
//! press on Save, and reports one line on stderr: where portcullisd put it,
//! or why not. Installed with `navigator-broker` it may; without, it is
//! refused.

use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use portcullis_ipc::{read_response, round_trip, send_fds, Request, Response, PROTO_VERSION};

fn sealed(bytes: &[u8]) -> std::io::Result<OwnedFd> {
    let name = std::ffi::CString::new("download").unwrap();
    // SAFETY: a valid C string; memfd_create's own flags.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    f.write_all(bytes)?;
    f.rewind()?;
    // SAFETY: fcntl on a descriptor we own.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f.into())
}

fn save() -> Result<(String, String), String> {
    let fd = sealed(b"%PDF-1.4\n% a download probe\n").map_err(|e| format!("memfd: {e}"))?;
    let path = std::env::var("PORTCULLIS_SOCKET").unwrap_or_else(|_| "/atrium/sockets/portcullis/portcullis.sock".into());
    let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }).map_err(|e| e.to_string())? {
        Response::Hello { .. } => {}
        r => return Err(format!("hello: {r:?}")),
    }
    let ask = Request::SaveDownload { name: "report.pdf".into(), origin: "https://example.org/files/report.pdf".into() };
    match round_trip(&mut s, &ask).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        r => return Err(format!("{r:?}")),
    }
    send_fds(&s, &[fd.as_raw_fd()]).map_err(|e| e.to_string())?;
    match read_response(&mut s).map_err(|e| e.to_string())? {
        Response::Downloaded { name, place } => Ok((name, place)),
        Response::Error { message } => Err(message),
        r => Err(format!("{r:?}")),
    }
}

fn main() {
    match save() {
        Ok((name, place)) => eprintln!("download-probe: saved name={name} place={place}"),
        Err(why) => eprintln!("download-probe: refused: {why}"),
    }
}

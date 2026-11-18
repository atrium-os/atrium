//! A double-clicked `.insula`: the file goes to the install sheet
//! (opifex.md §7.4), handed over as a descriptor with the one-line ask
//! `"SideLoad"`, never as a path. The sheet shows the person what it is
//! and who signed it; the answer is one line: `"Installed"`, `"Declined"`
//! or `{"Failed":{"why":…}}`. Called off the window's thread.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// The sheet's socket in Scrinium's jail (`OPIFEX_SHEET_SOCKET` overrides).
pub fn socket() -> PathBuf {
    std::env::var_os("OPIFEX_SHEET_SOCKET").map(PathBuf::from)
        .unwrap_or_else(|| "/atrium/sockets/opifex-sheet/opifex-sheet.sock".into())
}

/// What the person decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    Declined,
}

/// Write `line` with `fd` attached to its first bytes (SCM_RIGHTS).
fn send_with_fd(s: &UnixStream, line: &[u8], fd: RawFd) -> std::io::Result<()> {
    let mut iov = libc::iovec { iov_base: line.as_ptr() as *mut _, iov_len: line.len() };
    let mut cmsgbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr pointing at our iovec and control buffer, both
    // alive for the call; one SCM_RIGHTS header for one int fits in 64 bytes.
    let n = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(c).cast::<RawFd>(), 1);
        libc::sendmsg(s.as_raw_fd(), &msg, 0)
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    (&*s).write_all(&line[n as usize..])
}

fn answer(line: &str) -> Result<Outcome, String> {
    match serde_json::from_str::<serde_json::Value>(line.trim_end()) {
        Ok(serde_json::Value::String(s)) if s == "Installed" => Ok(Outcome::Installed),
        Ok(serde_json::Value::String(s)) if s == "Declined" => Ok(Outcome::Declined),
        Ok(v) => Err(v.get("Failed").and_then(|f| f.get("why")).and_then(|w| w.as_str())
            .map_or_else(|| "the install sheet answered something else".to_string(), str::to_string)),
        Err(_) => Err("the install sheet didn't answer".into()),
    }
}

/// Hand `file` to the install sheet and wait for the person (the sheet
/// gives up after about two minutes).
pub fn side_load(file: &std::fs::File) -> Result<Outcome, String> {
    let s = UnixStream::connect(socket()).map_err(|e| format!("Atrium's installer isn't reachable ({e})"))?;
    s.set_read_timeout(Some(Duration::from_secs(150))).map_err(|e| e.to_string())?;
    send_with_fd(&s, b"\"SideLoad\"\n", file.as_raw_fd()).map_err(|e| format!("the installer: {e}"))?;
    let mut back = String::new();
    BufReader::new(&s).read_line(&mut back).map_err(|e| format!("the installer: {e}"))?;
    answer(&back)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(answer("\"Installed\"\n"), Ok(Outcome::Installed));
        assert_eq!(answer("\"Declined\""), Ok(Outcome::Declined));
        assert_eq!(answer(r#"{"Failed":{"why":"not signed"}}"#), Err("not signed".into()));
        assert!(answer("").is_err());
    }

    /// The sheet's end, as opifex-sheetd reads it: the line, and the file beside it.
    #[test]
    fn the_file_goes_with_the_line() {
        let (a, b) = UnixStream::pair().unwrap();
        let p = std::env::temp_dir().join(format!("scrinium-install-{}", std::process::id()));
        std::fs::write(&p, b"INSB bytes").unwrap();
        let f = std::fs::File::open(&p).unwrap();
        send_with_fd(&a, b"\"SideLoad\"\n", f.as_raw_fd()).unwrap();
        let mut buf = [0u8; 64];
        let mut cmsg = [0u64; 8];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let (n, fd) = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&cmsg) as _;
            let n = libc::recvmsg(b.as_raw_fd(), &mut msg, 0);
            let c = libc::CMSG_FIRSTHDR(&msg);
            let mut fd: RawFd = -1;
            std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c).cast::<RawFd>(), &mut fd, 1);
            (n, fd)
        };
        assert_eq!(&buf[..n as usize], b"\"SideLoad\"\n");
        let mut got = String::new();
        std::io::Read::read_to_string(&mut unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) }, &mut got).unwrap();
        assert_eq!(got, "INSB bytes");
        let _ = std::fs::remove_file(&p);
    }
}

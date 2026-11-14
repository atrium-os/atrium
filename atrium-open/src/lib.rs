//! An app's document channel (docs/spec/opening-files.md §6.2).
//!
//! When the user opens a file in Scrinium, portcullisd hands the app one
//! descriptor for it, already narrowed (read, seek, fstat; and write,
//! ftruncate, fsync for a type the app edits), with the file's own name
//! for the title bar and its type. The app never learns the path, the
//! folder or the place.
//!
//! The channel is a socket portcullisd puts into the jail of every app
//! whose manifest has `[[opens]]`, at [`SOCKET`]. The app connects once,
//! at start, and reads [`Opened`]s from it for as long as it runs: the
//! first is the file it was started for, the rest are files opened while
//! it was running. An app started any other way (from Applications) finds
//! the socket too and simply gets nothing until a file is opened in it.
//!
//! On the wire each open is one `sendmsg`: a two-byte little-endian length,
//! then `OPEN\0<type>\0<r|w>\0<name>`, with the descriptor in
//! `SCM_RIGHTS` on the first byte.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Where the channel is inside an app's jail.
pub const SOCKET: &str = "/atrium/sockets/opens/opens.sock";
/// Overrides [`SOCKET`] (tests, a development run outside a jail).
pub const SOCKET_ENV: &str = "ATRIUM_OPENS_SOCKET";
/// The longest message: a type and a file name fit many times over.
pub const MAX: usize = 4096;
/// The longest name passed on: a file name, not a path.
pub const MAX_NAME: usize = 255;

/// A file the user opened in this app.
#[derive(Debug)]
pub struct Opened {
    /// The file's own name ("Notes.odt"), for the title bar.
    pub name: String,
    /// Its IANA media type, as Atrium's type table names it.
    pub mime: String,
    /// The app may change the file in place (its manifest edits this type,
    /// and the place is writable); otherwise Save asks where to put a copy.
    pub write: bool,
    pub file: File,
}

/// `OPEN\0type\0r|w\0name`, the name made safe (no NUL, no `/`, at most
/// [`MAX_NAME`] bytes).
pub fn encode(name: &str, mime: &str, write: bool) -> Vec<u8> {
    let mut n: String = name.chars().map(|c| if c == '\0' || c == '/' { '_' } else { c }).collect();
    while n.len() > MAX_NAME {
        n.pop();
    }
    let mut v = Vec::with_capacity(8 + mime.len() + n.len());
    v.extend_from_slice(b"OPEN\0");
    v.extend_from_slice(mime.replace('\0', "").as_bytes());
    v.push(0);
    v.push(if write { b'w' } else { b'r' });
    v.push(0);
    v.extend_from_slice(n.as_bytes());
    v
}

/// The (name, type, write) of a message, or `None` if it isn't one.
pub fn decode(b: &[u8]) -> Option<(String, String, bool)> {
    let rest = b.strip_prefix(b"OPEN\0")?;
    let mut it = rest.splitn(3, |&c| c == 0);
    let mime = std::str::from_utf8(it.next()?).ok()?;
    let write = match it.next()? {
        b"w" => true,
        b"r" => false,
        _ => return None,
    };
    let name = std::str::from_utf8(it.next()?).ok()?;
    if mime.is_empty() || name.is_empty() || name.len() > MAX_NAME || name.contains('/') {
        return None;
    }
    Some((name.to_string(), mime.to_string(), write))
}

/// Send one open: `fd` with its name and type.
pub fn send(s: &UnixStream, name: &str, mime: &str, write: bool, fd: RawFd) -> io::Result<()> {
    let body = encode(name, mime, write);
    if body.len() > MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an open message over 4 KiB"));
    }
    let mut data = (body.len() as u16).to_le_bytes().to_vec();
    data.extend_from_slice(&body);
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    // SAFETY: CMSG_SPACE is a size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: a zeroed msghdr is a valid empty one; every pointer set below
    // points into `iov`, `data` or `cbuf`, which outlive the sendmsg.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(c).cast::<RawFd>(), 1);
        let n = libc::sendmsg(s.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != data.len() {
            return Err(io::Error::other("a short write on the document channel"));
        }
    }
    Ok(())
}

/// The app's end of the channel.
pub struct Channel {
    s: UnixStream,
}

impl Channel {
    /// The channel portcullisd made for this app ([`SOCKET`], or
    /// [`SOCKET_ENV`]); `Ok(None)` when there is none (the app opens no
    /// types, or runs outside Portcullis).
    pub fn connect() -> io::Result<Option<Channel>> {
        let path = std::env::var_os(SOCKET_ENV).map(std::path::PathBuf::from).unwrap_or_else(|| SOCKET.into());
        if !path.exists() {
            return Ok(None);
        }
        Self::connect_to(&path).map(Some)
    }

    pub fn connect_to(path: &Path) -> io::Result<Channel> {
        Ok(Channel { s: UnixStream::connect(path)? })
    }

    pub fn from_stream(s: UnixStream) -> Channel {
        Channel { s }
    }

    /// The next file opened in this app: waits up to `wait` (`None`:
    /// for ever). `Ok(None)` when nothing came; `UnexpectedEof` once
    /// portcullisd has gone.
    pub fn recv(&mut self, wait: Option<Duration>) -> io::Result<Option<Opened>> {
        loop {
            let ms = wait.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as i32);
            let mut p = libc::pollfd { fd: self.s.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd we own.
            let r = unsafe { libc::poll(&mut p, 1, ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if r == 0 {
                return Ok(None);
            }
            return self.read_one();
        }
    }

    fn read_one(&mut self) -> io::Result<Option<Opened>> {
        let mut len = [0u8; 2];
        let mut iov = libc::iovec { iov_base: len.as_mut_ptr().cast(), iov_len: 2 };
        // Room for a few descriptors: a sender that puts in more than one
        // gets them closed, not left in a truncated message.
        // SAFETY: a size computation.
        let space = unsafe { libc::CMSG_SPACE((4 * std::mem::size_of::<RawFd>()) as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut fds: Vec<OwnedFd> = Vec::new();
        // SAFETY: as in `send`; the descriptors the kernel put in the
        // control buffer are ours and each is wrapped once.
        let n = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let n = libc::recvmsg(self.s.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_WAITALL);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    let p = libc::CMSG_DATA(c).cast::<RawFd>();
                    for i in 0..k {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(p.add(i))));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
            n
        };
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "portcullisd closed the document channel"));
        }
        if n < 2 {
            self.s.read_exact(&mut len[n as usize..])?;
        }
        let want = u16::from_le_bytes(len) as usize;
        if want > MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "an open message over 4 KiB"));
        }
        let mut body = vec![0u8; want];
        self.s.read_exact(&mut body)?;
        let Some((name, mime, write)) = decode(&body) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an open message"));
        };
        if fds.len() != 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("an open with {} descriptors", fds.len())));
        }
        Ok(Some(Opened { name, mime, write, file: File::from(fds.pop().unwrap()) }))
    }
}

impl AsRawFd for Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.s.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn a_message_says_name_type_and_mode() {
        let b = encode("Notes.odt", "application/vnd.oasis.opendocument.text", true);
        assert_eq!(decode(&b), Some(("Notes.odt".into(), "application/vnd.oasis.opendocument.text".into(), true)));
        assert_eq!(decode(&encode("a/b\0c.flac", "audio/flac", false)), Some(("a_b_c.flac".into(), "audio/flac".into(), false)));
        let long = "é".repeat(200);
        let (n, _, _) = decode(&encode(&long, "text/plain", false)).unwrap();
        assert!(n.len() <= MAX_NAME && long.starts_with(&n));
        for bad in [&b"OPEN\0audio/flac\0x\0a"[..], b"OPEN\0\0r\0a", b"OPEN\0audio/flac\0r\0", b"SHUT\0audio/flac\0r\0a", b"OPEN\0audio/flac\0r"] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn opens_arrive_with_their_files() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ch = Channel::from_stream(b);
        assert!(ch.recv(Some(Duration::from_millis(10))).unwrap().is_none(), "nothing yet");
        let dir = std::env::temp_dir().join(format!("atrium-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, text) in ["first", "second"].iter().enumerate() {
            let p = dir.join(format!("{i}.txt"));
            std::fs::write(&p, text).unwrap();
            let f = File::open(&p).unwrap();
            send(&a, &format!("{i}.txt"), "text/plain", i == 1, f.as_raw_fd()).unwrap();
        }
        for (i, text) in ["first", "second"].iter().enumerate() {
            let mut o = ch.recv(Some(Duration::from_secs(1))).unwrap().expect("an open");
            assert_eq!((o.name.as_str(), o.mime.as_str(), o.write), (format!("{i}.txt").as_str(), "text/plain", i == 1));
            let mut s = String::new();
            o.file.rewind().unwrap();
            o.file.read_to_string(&mut s).unwrap();
            assert_eq!(&s, text);
            assert!(o.file.write_all(b"x").is_err(), "opened read-only by the sender");
        }
        drop(a);
        assert_eq!(ch.recv(Some(Duration::from_secs(1))).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_channel_outside_portcullis() {
        std::env::set_var(SOCKET_ENV, "/nonexistent/opens.sock");
        assert!(Channel::connect().unwrap().is_none());
    }
}

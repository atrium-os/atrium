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
//!
//! Apps that take links (`[[links]]`, docs/spec/opening-links.md §5) get
//! them on the same channel as `LINK\0<url>`, with no descriptor: a link
//! the person pressed for in another app, passed by Monstrator and
//! portcullisd. [`Channel::recv`] passes over links; an app that takes
//! them reads [`Channel::recv_any`].

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
/// The longest message: a type and a file name fit many times over, and a
/// link of `atrium_links::MAX_URL` (8 KiB).
pub const MAX: usize = 10240;
/// The longest link passed on.
pub const MAX_LINK: usize = 8192;
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
    /// Opaque Scrinium authority for replacing this workbook.
    pub commit: Option<[u8; 16]>,
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

/// `LINK\0url`, or `None` for a link too long or with a NUL.
pub fn encode_link(url: &str) -> Option<Vec<u8>> {
    if url.is_empty() || url.len() > MAX_LINK || url.contains('\0') {
        return None;
    }
    let mut v = b"LINK\0".to_vec();
    v.extend_from_slice(url.as_bytes());
    Some(v)
}

/// `XLINK\0from\0url`: a link the person sent here from another of their
/// personas (personas.md §6), `from` its name (`personal` for Personal).
pub fn encode_crossed_link(url: &str, from: &str) -> Option<Vec<u8>> {
    let ok = !from.is_empty() && from.len() <= 32 && from.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return None;
    }
    let link = encode_link(url)?;
    let mut v = b"XLINK\0".to_vec();
    v.extend_from_slice(from.as_bytes());
    v.push(0);
    v.extend_from_slice(&link[5..]);
    Some(v)
}

/// The link and its persona of an `XLINK` message.
pub fn decode_crossed_link(b: &[u8]) -> Option<(String, String)> {
    let rest = b.strip_prefix(b"XLINK\0")?;
    let i = rest.iter().position(|c| *c == 0)?;
    let from = std::str::from_utf8(&rest[..i]).ok()?;
    let mut link = b"LINK\0".to_vec();
    link.extend_from_slice(&rest[i + 1..]);
    let url = decode_link(&link)?;
    encode_crossed_link(&url, from).map(|_| (url, from.to_string()))
}

/// The link of a `LINK` message.
pub fn decode_link(b: &[u8]) -> Option<String> {
    let u = std::str::from_utf8(b.strip_prefix(b"LINK\0")?).ok()?;
    (!u.is_empty() && u.len() <= MAX_LINK && !u.contains('\0')).then(|| u.to_string())
}

/// Send one link (no descriptor).
pub fn send_link(s: &UnixStream, url: &str) -> io::Result<()> {
    send_body(s, encode_link(url))
}

/// Send one link the person sent from persona `from` (no descriptor).
pub fn send_crossed_link(s: &UnixStream, url: &str, from: &str) -> io::Result<()> {
    send_body(s, encode_crossed_link(url, from))
}

fn send_body(mut s: &UnixStream, body: Option<Vec<u8>>) -> io::Result<()> {
    use std::io::Write;
    let body = body.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a link to pass on"))?;
    let mut data = (body.len() as u16).to_le_bytes().to_vec();
    data.extend_from_slice(&body);
    s.write_all(&data)
}

/// What came on the channel.
#[derive(Debug)]
pub enum Message {
    File(Opened),
    /// A link the person opened in this app (opening-links.md §5).
    Link(String),
    /// A link the person sent here from another of their personas
    /// (personas.md §6): `from` is its name, `personal` for Personal. A
    /// Work app opens it as a stranger would (no sign-ins, no cookies).
    Crossed { url: String, from: String },
}

/// Send one open: `fd` with its name and type.
pub fn send(s: &UnixStream, name: &str, mime: &str, write: bool, fd: RawFd) -> io::Result<()> {
    send_with_commit(s, name, mime, write, fd, None)
}

pub fn send_with_commit(s: &UnixStream, name: &str, mime: &str, write: bool, fd: RawFd, commit: Option<[u8; 16]>) -> io::Result<()> {
    let mut body = encode(name, mime, write);
    if let Some(token) = commit {
        if !write { return Err(io::Error::other("read-only save grant")); }
        let mut tagged = b"WORKBOOK\0".to_vec();
        tagged.extend_from_slice(&token);
        tagged.append(&mut body);
        body = tagged;
    }
    if body.len() > MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a message over 10 KiB"));
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
            match self.recv_any(wait)? {
                Some(Message::File(o)) => return Ok(Some(o)),
                Some(Message::Link(_) | Message::Crossed { .. }) => continue,
                None => return Ok(None),
            }
        }
    }

    /// The next file or link: as [`Channel::recv`], links included.
    pub fn recv_any(&mut self, wait: Option<Duration>) -> io::Result<Option<Message>> {
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

    fn read_one(&mut self) -> io::Result<Option<Message>> {
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
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a message over 10 KiB"));
        }
        let mut body = vec![0u8; want];
        self.s.read_exact(&mut body)?;
        if body.starts_with(b"XLINK\0") {
            if !fds.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a link with descriptors"));
            }
            return decode_crossed_link(&body).map(|(url, from)| Some(Message::Crossed { url, from }))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a link message"));
        }
        if body.starts_with(b"LINK\0") {
            if !fds.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a link with descriptors"));
            }
            return decode_link(&body).map(|u| Some(Message::Link(u)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a link message"));
        }
        let (commit, document) = match body.strip_prefix(b"WORKBOOK\0") {
            Some(rest) => {
                let (token, document) = rest.split_at_checked(16).ok_or_else(|| io::Error::other("short save grant"))?;
                (Some(token.try_into().unwrap()), document)
            }
            None => (None, body.as_slice()),
        };
        let Some((name, mime, write)) = decode(document) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an open message"));
        };
        if commit.is_some() && !write {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "read-only save grant"));
        }
        if fds.len() != 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("an open with {} descriptors", fds.len())));
        }
        Ok(Some(Message::File(Opened { name, mime, write, commit, file: File::from(fds.pop().unwrap()) })))
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
    fn links_come_on_the_same_channel() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ch = Channel::from_stream(b);
        send_link(&a, "mailto:ana@example.org").unwrap();
        match ch.recv_any(Some(Duration::from_secs(1))).unwrap() {
            Some(Message::Link(u)) => assert_eq!(u, "mailto:ana@example.org"),
            other => panic!("{other:?}"),
        }
        assert!(send_link(&a, &format!("mailto:{}", "a".repeat(MAX_LINK))).is_err(), "too long");
        assert!(encode_link("a\0b").is_none());
        // One the person sent from another persona says which.
        send_crossed_link(&a, "https://intra.example/", "personal").unwrap();
        match ch.recv_any(Some(Duration::from_secs(1))).unwrap() {
            Some(Message::Crossed { url, from }) => assert_eq!((url.as_str(), from.as_str()), ("https://intra.example/", "personal")),
            other => panic!("{other:?}"),
        }
        assert!(encode_crossed_link("https://a/", "Work\0").is_none() && encode_crossed_link("https://a/", "").is_none());
        assert_eq!(decode_crossed_link(&encode_crossed_link("https://a/", "work").unwrap()), Some(("https://a/".into(), "work".into())));
        // recv() passes links over to the next file.
        send_link(&a, "mailto:b@example.org").unwrap();
        let p = std::env::temp_dir().join(format!("atrium-open-link-{}", std::process::id()));
        std::fs::write(&p, "x").unwrap();
        send(&a, "x.txt", "text/plain", false, File::open(&p).unwrap().as_raw_fd()).unwrap();
        assert_eq!(ch.recv(Some(Duration::from_secs(1))).unwrap().unwrap().name, "x.txt");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn no_channel_outside_portcullis() {
        std::env::set_var(SOCKET_ENV, "/nonexistent/opens.sock");
        assert!(Channel::connect().unwrap().is_none());
    }
}

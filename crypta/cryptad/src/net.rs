//! atrium-net's control socket (root only): tunnel proposals (vpn.md §5).
//!
//! One request line at a time; atrium-net answers with zero or more lines
//! and then `ok` or `error why=..`. `tunnel-open` sends the tun's
//! descriptor with its first answer line.

use aura_ipc::Line;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct NetCtl {
    s: UnixStream,
    buf: Vec<u8>,
}

/// What a request got: the lines before `ok`, and any descriptors.
pub struct Answer {
    pub lines: Vec<Line>,
    pub fds: Vec<OwnedFd>,
}

impl NetCtl {
    pub fn connect(path: &str) -> io::Result<NetCtl> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(15)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(NetCtl { s, buf: Vec::new() })
    }

    /// Send `l`; `Ok` with what came before `ok`, `Err` with atrium-net's
    /// reason on `error`, or the I/O error.
    pub fn call(&mut self, l: &Line) -> Result<Answer, String> {
        l.send(&mut self.s).and_then(|_| self.s.flush()).map_err(|e| format!("atrium-net: {e}"))?;
        let mut ans = Answer { lines: Vec::new(), fds: Vec::new() };
        loop {
            while let Some(p) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=p).collect();
                let Some(r) = std::str::from_utf8(&raw[..p]).ok().and_then(Line::parse) else { return Err("atrium-net: a bad line".into()) };
                match r.verb.as_str() {
                    "ok" => return Ok(ans),
                    "error" => return Err(r.get("why").unwrap_or("refused").to_string()),
                    _ => ans.lines.push(r),
                }
            }
            if self.buf.len() > aura_ipc::MAX_LINE {
                return Err("atrium-net: line too long".into());
            }
            let mut b = [0u8; 4096];
            let (n, fds) = crate::fd::recv(self.s.as_raw_fd(), &mut b).map_err(|e| format!("atrium-net: {e}"))?;
            if n == 0 {
                return Err("atrium-net closed the connection".into());
            }
            ans.fds.extend(fds);
            self.buf.extend_from_slice(&b[..n]);
        }
    }
}

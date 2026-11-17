//! Sending one message over the courier's submission session (mail.md
//! §9.3, §5.2): the courier has logged in; its first bytes are the
//! server's post-login EHLO reply, which says what the server offers.

use std::io::{Read, Write};

use mail_proto::smtp::{Caps, Refusal, Reply, ReplyDecoder, Step, Submission};

/// What to send: the envelope and the bytes as written.
pub struct Outgoing<'a> {
    pub from: &'a str,
    pub rcpts: &'a [String],
    pub bytes: &'a [u8],
    /// Hold on the server this many seconds (send later, RFC 4865).
    pub hold_for: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// Recipients the server refused (the others got it), with its reply.
    pub rejected: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The server can't hold it (no FUTURERELEASE, or not this long):
    /// the engine holds it instead.
    HoldNotOffered,
    /// Not sent; `transient`: try again later.
    Failed { why: String, transient: bool },
}

fn read_reply<S: Read>(s: &mut S, dec: &mut ReplyDecoder) -> Result<Reply, SendError> {
    loop {
        if let Some(r) = dec.next() {
            return r.map_err(|e| SendError::Failed { why: format!("the server's reply: {e:?}"), transient: true });
        }
        let mut b = [0u8; 4096];
        let n = s.read(&mut b).map_err(|e| SendError::Failed { why: format!("connection: {e}"), transient: true })?;
        if n == 0 {
            return Err(SendError::Failed { why: "the connection closed".into(), transient: true });
        }
        dec.feed(&b[..n]);
    }
}

fn say(r: &Reply) -> String {
    format!("{} {}", r.code, r.lines.join(" ")).chars().take(300).collect()
}

/// The server's capabilities from the courier's greeting.
pub fn greeting<S: Read>(s: &mut S, dec: &mut ReplyDecoder) -> Result<Caps, SendError> {
    let r = read_reply(s, dec)?;
    if r.code != 250 {
        return Err(SendError::Failed { why: format!("couldn't sign in: {}", say(&r)), transient: r.transient() || r.code == 421 });
    }
    Ok(Caps::from_ehlo(&r))
}

/// Send `o` on `s` (one courier session), then QUIT.
pub fn submit<S: Read + Write>(mut s: S, o: &Outgoing<'_>) -> Result<Sent, SendError> {
    let mut dec = ReplyDecoder::default();
    let caps = greeting(&mut s, &mut dec)?;
    let rcpts: Vec<&str> = o.rcpts.iter().map(String::as_str).collect();
    let mut sub = Submission::new(&caps, o.from, &rcpts, o.bytes, o.hold_for).map_err(|e| match e {
        Refusal::HoldNotOffered => SendError::HoldNotOffered,
        Refusal::TooBig { size, limit } => SendError::Failed { why: format!("the server takes at most {} MB; this is {} MB", limit >> 20, size >> 20), transient: false },
        other => SendError::Failed { why: format!("{other:?}"), transient: false },
    })?;
    let io = |e: std::io::Error| SendError::Failed { why: format!("connection: {e}"), transient: true };
    s.write_all(&sub.start()).map_err(io)?;
    s.flush().map_err(io)?;
    loop {
        let r = read_reply(&mut s, &mut dec)?;
        match sub.on_reply(r) {
            Step::Send(b) => {
                s.write_all(&b).map_err(io)?;
                s.flush().map_err(io)?;
            }
            Step::Sent { rejected } => {
                let _ = s.write_all(b"QUIT\r\n");
                return Ok(Sent { rejected: rejected.into_iter().map(|(a, r)| (a, say(&r))).collect() });
            }
            Step::Failed { stage, reply, transient } => {
                let _ = s.write_all(b"QUIT\r\n");
                return Err(SendError::Failed { why: format!("{stage}: {}", say(&reply)), transient });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny submission server: answers each command line, takes DATA.
    struct Fake {
        out: Vec<u8>,
        inbuf: Vec<u8>,
        data: bool,
        got: Vec<String>,
        message: Vec<u8>,
        refuse: &'static str,
    }

    impl Fake {
        fn new(ehlo: &str, refuse: &'static str) -> Fake {
            Fake { out: ehlo.as_bytes().to_vec(), inbuf: Vec::new(), data: false, got: Vec::new(), message: Vec::new(), refuse }
        }
        fn work(&mut self) {
            loop {
                if self.data {
                    let Some(p) = self.inbuf.windows(5).position(|w| w == b"\r\n.\r\n") else { return };
                    self.message = self.inbuf[..p + 2].to_vec();
                    self.inbuf.drain(..p + 5);
                    self.data = false;
                    self.out.extend_from_slice(b"250 2.0.0 queued\r\n");
                    continue;
                }
                let Some(p) = self.inbuf.windows(2).position(|w| w == b"\r\n") else { return };
                let line = String::from_utf8_lossy(&self.inbuf[..p]).into_owned();
                self.inbuf.drain(..p + 2);
                let reply: &[u8] = if line.starts_with("DATA") {
                    self.data = true;
                    b"354 go\r\n"
                } else if !self.refuse.is_empty() && line.contains(self.refuse) {
                    b"550 5.1.1 no such user\r\n"
                } else if line == "QUIT" {
                    b"221 bye\r\n"
                } else {
                    b"250 ok\r\n"
                };
                self.got.push(line);
                self.out.extend_from_slice(reply);
            }
        }
    }

    impl Read for &mut Fake {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let n = b.len().min(self.out.len());
            b[..n].copy_from_slice(&self.out[..n]);
            self.out.drain(..n);
            Ok(n)
        }
    }
    impl Write for &mut Fake {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.inbuf.extend_from_slice(b);
            self.work();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sends() {
        let rcpts = ["a@x.example".to_string(), "gone@x.example".to_string()];
        let msg = b"Subject: hi\r\n\r\n.leading dot\r\n";
        let mut f = Fake::new("250-mail.example\r\n250-PIPELINING\r\n250 SIZE 1000000\r\n", "gone@");
        let r = submit(&mut f, &Outgoing { from: "me@x.example", rcpts: &rcpts, bytes: msg, hold_for: None }).unwrap();
        assert_eq!(r.rejected.len(), 1);
        assert_eq!(r.rejected[0].0, "gone@x.example");
        assert!(f.got[0].starts_with("MAIL FROM:<me@x.example>"), "{:?}", f.got);
        assert_eq!(f.message, b"Subject: hi\r\n\r\n..leading dot\r\n");
        assert_eq!(f.got.last().unwrap(), "QUIT");
        // No FUTURERELEASE: the engine holds it.
        let mut f = Fake::new("250 mail.example\r\n", "");
        let e = submit(&mut f, &Outgoing { from: "me@x.example", rcpts: &rcpts[..1], bytes: msg, hold_for: Some(600) }).unwrap_err();
        assert_eq!(e, SendError::HoldNotOffered);
        // Every recipient refused: not sent.
        let mut f = Fake::new("250 mail.example\r\n", "@x.example>");
        let e = submit(&mut f, &Outgoing { from: "me@y.example", rcpts: &rcpts, bytes: msg, hold_for: None }).unwrap_err();
        assert!(matches!(e, SendError::Failed { transient: false, .. }), "{e:?}");
    }
}

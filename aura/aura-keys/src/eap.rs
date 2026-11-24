//! 802.1X joins (wifi.md §7.4): the EAP conversation runs in
//! `atrium-aura-eap`, one process per join, started here with a socket as
//! its only descriptor; it enters capability mode before reading a byte.
//! aura-keys carries EAP between EAPOL and that socket and takes the MSK
//! back. The password reaches the helper inside the profile, for this join.

use aura_eap::helper::{self, Reader};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use zeroize::Zeroizing;

/// EAPOL packet types (802.1X-2010 §11.3.2).
pub const EAPOL_EAP: u8 = 0;
pub const EAPOL_LOGOFF: u8 = 2;
pub const EAPOL_KEY: u8 = 3;
const EAPOL_VERSION: u8 = 2;

pub struct Run {
    child: Child,
    sock: UnixStream,
    rd: Reader,
}

/// What the helper said.
pub enum Event {
    /// An EAPOL frame to send to the AP.
    Send(Vec<u8>),
    Done(Zeroizing<[u8; 64]>),
    Failed(String),
    /// The server's DNS names and chain, for "Trust this server".
    Trust(Vec<String>, Vec<Vec<u8>>),
    /// EAP-TLS: sign this SHA-256 digest with the network's key in Arca.
    Sign([u8; 32]),
    Log(String),
}

impl Run {
    pub fn start(path: &str, profile: &[u8]) -> io::Result<Run> {
        let (ours, theirs) = UnixStream::pair()?;
        let child = Command::new(path)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .env_clear()
            .spawn()?;
        let mut r = Run { child, sock: ours, rd: Reader::default() };
        helper::write(&mut r.sock, helper::CONFIG, profile)?;
        r.sock.set_nonblocking(true)?;
        Ok(r)
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// An EAP packet from the AP (the body of an EAPOL type 0 frame).
    pub fn packet(&mut self, eap: &[u8]) -> io::Result<()> {
        self.sock.set_nonblocking(false)?;
        let r = helper::write(&mut self.sock, helper::PACKET, eap);
        self.sock.set_nonblocking(true)?;
        r
    }

    /// The signature asked for with `Event::Sign`; empty when Arca refused.
    pub fn signed(&mut self, sig: &[u8]) -> io::Result<()> {
        self.sock.set_nonblocking(false)?;
        let r = helper::write(&mut self.sock, helper::SIGNED, sig);
        self.sock.set_nonblocking(true)?;
        r
    }

    /// Whatever the helper has written; an error when it went away
    /// without finishing.
    pub fn read(&mut self) -> io::Result<Vec<Event>> {
        let mut buf = [0u8; 8192];
        let mut eof = false;
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => self.rd.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut out = Vec::new();
        loop {
            match self.rd.next() {
                Ok(Some((tag, d))) => out.push(match tag {
                    helper::SEND => Event::Send(eapol(EAPOL_EAP, &d)),
                    helper::DONE if d.len() == 64 => {
                        let mut m = Zeroizing::new([0u8; 64]);
                        m.copy_from_slice(&d);
                        Event::Done(m)
                    }
                    helper::FAILED => Event::Failed(word(&d)),
                    helper::TRUST => match helper::decode_seen(&d) {
                        Some((names, chain)) => Event::Trust(names, chain),
                        None => Event::Failed("bad-certificate-report".into()),
                    },
                    helper::DROPPED => Event::Log(word(&d)),
                    helper::SIGN => match <[u8; 32]>::try_from(&d[..]) {
                        Ok(digest) => Event::Sign(digest),
                        Err(_) => Event::Failed("bad-helper-frame".into()),
                    },
                    _ => Event::Failed("bad-helper-frame".into()),
                }),
                Ok(None) => break,
                Err(()) => {
                    out.push(Event::Failed("bad-helper-frame".into()));
                    break;
                }
            }
        }
        let ended = out.iter().any(|e| matches!(e, Event::Done(_) | Event::Failed(_) | Event::Trust(..)));
        if eof && !ended {
            out.push(Event::Failed("helper-exited".into()));
        }
        Ok(out)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A word from the helper for a reply line: letters, digits and dashes.
fn word(b: &[u8]) -> String {
    b.iter().take(40).map(|&c| if c.is_ascii_alphanumeric() || c == b'-' { c as char } else { '-' }).collect()
}

/// An EAPOL frame of `typ` around `body`.
pub fn eapol(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(4 + body.len());
    f.extend_from_slice(&[EAPOL_VERSION, typ]);
    f.extend_from_slice(&(body.len() as u16).to_be_bytes());
    f.extend_from_slice(body);
    f
}

/// (type, body) of an EAPOL frame, the body cut to its length field.
pub fn parse_eapol(f: &[u8]) -> Option<(u8, &[u8])> {
    if f.len() < 4 {
        return None;
    }
    let n = u16::from_be_bytes([f[2], f[3]]) as usize;
    Some((f[1], f.get(4..4 + n)?))
}

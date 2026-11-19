//! One IPP request over HTTP/1.1 (RFC 8010 §4): a POST of
//! `application/ipp` to the printer's path, the document (if any) after the
//! IPP message in the same body. A new connection per request, closed after
//! the answer: the TLS key is checked on every one, and nothing depends on a
//! printer's keep-alive handling.
//!
//! The answer's head and body are read with the Navigator's bounded HTTP
//! reader (the code Nuntius uses); the IPP message with our own decoder.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use navigator_fetch::http::{read_head, Body};
use prelum_ipp_codec::{Limits, Message};
use zeroize::Zeroizing;

use crate::tls;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A printer busy printing may stop reading the document for a while.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(300);
pub const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the printer is and who it must be.
pub struct Target {
    pub addr: SocketAddr,
    /// `Host:` header: the URI's host and port.
    pub host: String,
    /// The HTTP path (`/ipp/print`).
    pub path: String,
    pub tls: Option<Arc<rustls::ClientConfig>>,
    pub verifier: Option<Arc<tls::Verifier>>,
}

#[derive(Debug)]
pub enum Error {
    /// No connection: offline, refused, no route.
    Unreachable(String),
    /// The printer's TLS key is not the pinned one; what it showed.
    KeyChanged(Option<[u8; 32]>),
    /// The printer answered with an HTTP error.
    Http(u16),
    /// Anything else on the connection.
    Io(String),
    /// Not a well-formed IPP answer.
    Ipp(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unreachable(e) => write!(f, "can't reach the printer: {e}"),
            Error::KeyChanged(_) => write!(f, "the printer's key changed"),
            Error::Http(s) => write!(f, "the printer answered HTTP {s}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Ipp(e) => write!(f, "the printer's answer is not valid IPP: {e}"),
        }
    }
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(b),
            Stream::Tls(s) => s.read(b),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(b),
            Stream::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn unreachable(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(e.kind(), ConnectionRefused | TimedOut | WouldBlock | HostUnreachable | NetworkUnreachable | AddrNotAvailable)
        // EHOSTDOWN: 64 on FreeBSD, 112 on Linux.
        || matches!(e.raw_os_error(), Some(64) | Some(112))
}

impl Target {
    fn open(&self) -> Result<Stream, Error> {
        let tcp = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT).map_err(|e| {
            if unreachable(&e) {
                Error::Unreachable(e.to_string())
            } else {
                Error::Io(e.to_string())
            }
        })?;
        let _ = tcp.set_nodelay(true);
        tcp.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
        tcp.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
        let Some(cfg) = &self.tls else { return Ok(Stream::Plain(tcp)) };
        let name = rustls::pki_types::ServerName::IpAddress(self.addr.ip().into());
        let conn = rustls::ClientConnection::new(cfg.clone(), name).map_err(|e| Error::Io(e.to_string()))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        // The handshake now, so a changed key stops us before the request.
        while s.conn.is_handshaking() {
            if let Err(e) = s.conn.complete_io(&mut s.sock) {
                if let Some(v) = &self.verifier {
                    let seen = *v.seen.lock().unwrap();
                    if v.pinned().is_some() && seen.is_some() && seen != v.pinned() {
                        return Err(Error::KeyChanged(seen));
                    }
                }
                return Err(if unreachable(&e) { Error::Unreachable(e.to_string()) } else { Error::Io(format!("TLS: {e}")) });
            }
        }
        Ok(Stream::Tls(Box::new(s)))
    }

    /// Send `msg` (and a document, read to its end, chunked) and return the
    /// printer's answer.
    pub fn call(&self, msg: &Message, doc: Option<&mut dyn Read>) -> Result<Message, Error> {
        let mut s = self.open()?;
        let ipp = Zeroizing::new(msg.encode());
        let io = |e: io::Error| Error::Io(e.to_string());
        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/ipp\r\nUser-Agent: Atrium-Prelum/1\r\nConnection: close\r\n",
            self.path, self.host
        );
        match doc {
            None => {
                head.push_str(&format!("Content-Length: {}\r\n\r\n", ipp.len()));
                let mut all = Zeroizing::new(Vec::with_capacity(head.len() + ipp.len()));
                all.extend_from_slice(head.as_bytes());
                all.extend_from_slice(&ipp);
                s.write_all(&all).map_err(io)?;
            }
            Some(doc) => {
                head.push_str("Transfer-Encoding: chunked\r\n\r\n");
                s.write_all(head.as_bytes()).map_err(io)?;
                write_chunk(&mut s, &ipp).map_err(io)?;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match doc.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(Error::Io(format!("document: {e}"))),
                    };
                    write_chunk(&mut s, &buf[..n]).map_err(io)?;
                }
                s.write_all(b"0\r\n\r\n").map_err(io)?;
            }
        }
        s.flush().map_err(io)?;
        let limits = Limits::default();
        // Skip interim answers (100 Continue).
        let (head, rest) = loop {
            let (h, rest) = read_head(&mut s, false).map_err(|e| Error::Io(format!("{e:?}")))?;
            if (100..200).contains(&h.status) {
                if !rest.is_empty() {
                    return Err(Error::Io("data after an interim answer".into()));
                }
                continue;
            }
            break (h, rest);
        };
        if head.status != 200 {
            return Err(Error::Http(head.status));
        }
        let mut body = Body::new(&head, rest);
        let mut out = Vec::new();
        while let Some(piece) = body.next(&mut s, 64 * 1024).map_err(|e| Error::Io(format!("{e:?}")))? {
            out.extend_from_slice(&piece);
            if out.len() > limits.max_bytes {
                return Err(Error::Ipp("answer too large".into()));
            }
        }
        Message::decode(&out, &limits).map(|(m, _)| m).map_err(|e| Error::Ipp(e.to_string()))
    }
}

fn write_chunk(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    if b.is_empty() {
        return Ok(());
    }
    w.write_all(format!("{:x}\r\n", b.len()).as_bytes())?;
    w.write_all(b)?;
    w.write_all(b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    use prelum_ipp_codec::ops;

    /// A TLS server with a fresh key that answers one IPP request with an
    /// empty success, and reports how many request bytes it read.
    fn server() -> (SocketAddr, [u8; 32], mpsc::Receiver<usize>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["printer.local".into()]).unwrap().self_signed(&key).unwrap();
        let spki = tls::spki_sha256(cert.der()).unwrap();
        let cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap())
                .unwrap(),
        );
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg.clone()).unwrap(), s);
                let mut got = Vec::new();
                let mut b = [0u8; 4096];
                loop {
                    match t.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.extend_from_slice(&b[..n]);
                            if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                                let len: usize = String::from_utf8_lossy(&got[..i])
                                    .lines()
                                    .find_map(|l| l.strip_prefix("Content-Length: ").and_then(|v| v.parse().ok()))
                                    .unwrap_or(0);
                                if got.len() >= i + 4 + len {
                                    let reply = Message { version: (2, 0), code: 0, request_id: 1, groups: vec![] }.encode();
                                    let _ = t.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\n\r\n", reply.len()).as_bytes());
                                    let _ = t.write_all(&reply);
                                    let _ = t.flush();
                                    break;
                                }
                            }
                        }
                    }
                }
                let _ = tx.send(got.len());
            }
        });
        (addr, spki, rx)
    }

    fn target(addr: SocketAddr, pin: Option<[u8; 32]>) -> Target {
        let (cfg, v) = tls::config(pin).unwrap();
        Target { addr, host: addr.to_string(), path: "/ipp/print".into(), tls: Some(cfg), verifier: Some(v) }
    }

    #[test]
    fn the_pinned_key_is_checked_before_any_request_byte() {
        let (addr, spki, rx) = server();
        let msg = ops::get_printer_attributes(1, "ipps://x/ipp/print", &["all"]);
        // Learning: anything is accepted, and the key is what the server has.
        let t = target(addr, None);
        assert!(t.call(&msg, None).is_ok());
        assert_eq!(*t.verifier.as_ref().unwrap().seen.lock().unwrap(), Some(spki));
        assert!(rx.recv().unwrap() > 0);
        // Pinned to that key: fine.
        assert!(target(addr, Some(spki)).call(&msg, None).is_ok());
        assert!(rx.recv().unwrap() > 0);
        // Pinned to another key: stopped in the handshake, nothing sent.
        match target(addr, Some([1; 32])).call(&msg, None) {
            Err(Error::KeyChanged(Some(k))) => assert_eq!(k, spki),
            other => panic!("{other:?}"),
        }
        assert_eq!(rx.recv().unwrap(), 0, "the server read request bytes from a client that should have stopped");
    }

    #[test]
    fn nobody_there_is_unreachable() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let t = Target { addr, host: addr.to_string(), path: "/".into(), tls: None, verifier: None };
        assert!(matches!(t.call(&ops::get_printer_attributes(1, "ipp://x/", &["all"]), None), Err(Error::Unreachable(_))));
    }
}

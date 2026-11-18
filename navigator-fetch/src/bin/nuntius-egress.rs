//! nuntius-egress: a workspace's way out in the `registries` network mode
//! (dev-tools.md §5.2, dev-tools-plan.md M8).
//!
//! A forward proxy that does CONNECT and nothing else, so TLS stays end to
//! end between the tool in the workspace (cargo, git, pkg, fetch) and the
//! registry. portcullisd starts it in a one-shot jail of its own, with the
//! network (`org.atrium.nuntius-egress`, as `_egress`), one per running
//! workspace, and hands it a listening socket it bound on 127.0.0.1:3128 in
//! the workspace's own network stack; the workspace has no other way out.
//!
//! - stdin: that listening socket.
//! - `ATRIUM_EGRESS_HOSTS`: the hosts the workspace lists, comma-separated,
//!   matched exactly (no wildcards), port 443 only.
//!
//! Every address a listed name resolves to is held to Nuntius's address
//! policy (`navigator_fetch::addr`): loopback, private, link-local (the
//! metadata endpoint among them), CGNAT and the rest are refused, so a
//! listed name that points inside is no way in. Each decision is a line on
//! stderr.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use navigator_fetch::addr::Policy;

const PORT: u16 = 443;
const MAX_HEAD: usize = 8 * 1024;
const MAX_CONNS: usize = 64;

/// A request head's verdict.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    Connect(String),
    Refuse(u16, &'static str),
}

/// `CONNECT host:443 HTTP/1.1` against the list.
fn judge(head: &str, hosts: &[String]) -> Ask {
    let line = head.lines().next().unwrap_or("");
    let mut w = line.split(' ');
    let (Some(method), Some(target), Some(ver)) = (w.next(), w.next(), w.next()) else {
        return Ask::Refuse(400, "not a request");
    };
    if !ver.starts_with("HTTP/1.") {
        return Ask::Refuse(400, "not HTTP/1");
    }
    if method != "CONNECT" {
        return Ask::Refuse(405, "only CONNECT");
    }
    let Some((host, port)) = target.rsplit_once(':') else { return Ask::Refuse(400, "no port") };
    if port.parse::<u16>().ok() != Some(PORT) {
        return Ask::Refuse(403, "only port 443");
    }
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if !hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Ask::Refuse(403, "not a listed host");
    }
    Ask::Connect(host)
}

/// The addresses `host` resolves to, every one admitted, or why not.
fn resolve(host: &str, policy: &Policy) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = (host, PORT).to_socket_addrs().map_err(|e| format!("resolve: {e}"))?.collect();
    if addrs.is_empty() {
        return Err("resolves to nothing".into());
    }
    // ★ All of them, not just the one we'd pick: a name with one public and
    // one private address is a name that points inside.
    for a in &addrs {
        if let Err(r) = policy.admit(a.ip()) {
            return Err(format!("{} is {r:?}: refused", a.ip()));
        }
    }
    Ok(addrs)
}

fn read_head(c: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") && !buf.ends_with(b"\n\n") {
        if buf.len() >= MAX_HEAD {
            return Err(io::Error::other("head too long"));
        }
        if c.read(&mut b)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
        buf.push(b[0]);
    }
    String::from_utf8(buf).map_err(|_| io::Error::other("head is not UTF-8"))
}

fn refuse(c: &mut TcpStream, code: u16, why: &str) {
    let text = match code { 400 => "Bad Request", 405 => "Method Not Allowed", 502 => "Bad Gateway", _ => "Forbidden" };
    let _ = write!(c, "HTTP/1.1 {code} {text}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{why}\n",
                   why.len() + 1);
}

fn serve(mut c: TcpStream, hosts: &[String], policy: &Policy) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(30)));
    let head = match read_head(&mut c) {
        Ok(h) => h,
        Err(_) => return,
    };
    let host = match judge(&head, hosts) {
        Ask::Connect(h) => h,
        Ask::Refuse(code, why) => {
            eprintln!("egress: refused {:?}: {why}", head.lines().next().unwrap_or("").chars().take(120).collect::<String>());
            return refuse(&mut c, code, why);
        }
    };
    let addrs = match resolve(&host, policy) {
        Ok(a) => a,
        Err(why) => {
            eprintln!("egress: refused {host}: {why}");
            return refuse(&mut c, 403, &why);
        }
    };
    let up = addrs.iter().find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(10)).ok());
    let Some(up) = up else {
        eprintln!("egress: {host}: no address answered");
        return refuse(&mut c, 502, "no address answered");
    };
    eprintln!("egress: {host} -> {}", up.peer_addr().map(|a| a.to_string()).unwrap_or_default());
    if c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
        return;
    }
    let _ = c.set_read_timeout(None);
    splice(c, up);
}

/// Bytes both ways until either side is done.
fn splice(a: TcpStream, b: TcpStream) {
    let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else { return };
    let t = std::thread::spawn(move || {
        let mut a = a;
        let mut b = b;
        let _ = io::copy(&mut a, &mut b);
        let _ = b.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut b2, &mut a2);
    let _ = a2.shutdown(Shutdown::Write);
    let _ = t.join();
}

fn hosts_from(v: &str) -> Vec<String> {
    v.split(',').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect()
}

fn main() {
    let hosts = Arc::new(hosts_from(&std::env::var("ATRIUM_EGRESS_HOSTS").unwrap_or_default()));
    // ★ Never the private-network grant: a workspace's registries are on
    // the internet.
    let policy = Policy { grant_private_network: false };
    // SAFETY: portcullisd hands us the listening socket as stdin, and
    // nothing else here owns descriptor 0.
    let l = unsafe { TcpListener::from_raw_fd(0) };
    eprintln!("egress: up, {} hosts", hosts.len());
    let live = Arc::new(AtomicUsize::new(0));
    for c in l.incoming() {
        let Ok(mut c) = c else { continue };
        if live.load(Ordering::Acquire) >= MAX_CONNS {
            refuse(&mut c, 502, "too many connections");
            continue;
        }
        live.fetch_add(1, Ordering::AcqRel);
        let (hosts, live) = (hosts.clone(), live.clone());
        let _ = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            serve(c, &hosts, &policy);
            live.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_connects_to_443() {
        let h = hosts_from("index.crates.io, static.crates.io,");
        assert_eq!(judge("CONNECT index.crates.io:443 HTTP/1.1\r\nHost: x\r\n\r\n", &h), Ask::Connect("index.crates.io".into()));
        assert_eq!(judge("CONNECT INDEX.crates.io:443 HTTP/1.1\r\n\r\n", &h), Ask::Connect("index.crates.io".into()));
        assert_eq!(judge("CONNECT evil.example:443 HTTP/1.1\r\n\r\n", &h), Ask::Refuse(403, "not a listed host"));
        assert_eq!(judge("CONNECT index.crates.io:22 HTTP/1.1\r\n\r\n", &h), Ask::Refuse(403, "only port 443"));
        assert_eq!(judge("GET http://index.crates.io/ HTTP/1.1\r\n\r\n", &h), Ask::Refuse(405, "only CONNECT"));
        assert_eq!(judge("CONNECT index.crates.io.evil:443 HTTP/1.1\r\n\r\n", &h), Ask::Refuse(403, "not a listed host"));
        assert_eq!(judge("garbage", &h), Ask::Refuse(400, "not a request"));
    }

    #[test]
    fn inside_addresses_are_refused_even_when_listed() {
        let p = Policy::default();
        assert!(resolve("127.0.0.1", &p).unwrap_err().contains("refused"));
        assert!(resolve("169.254.169.254", &p).unwrap_err().contains("refused"));
        assert!(resolve("10.0.0.1", &p).unwrap_err().contains("refused"));
        assert!(resolve("localhost", &p).is_err());
    }
}

//! local_port_ask: Navigator's side of `OpenLocalPort`, for
//! scripts/vm-local-server-check.sh (background-apps.md §4.4).
//!
//!   local_port_ask <port>                      ask portcullisd for a socket
//!   local_port_ask <port> <fetchd> <url>       and fetch <url> on it through
//!                                              navigator-fetchd started on a
//!                                              Unix socket, as navigatord does
//!
//! Prints one line: `refused: <why>`, or `ok <app name> <addr>`; with a
//! fetcher, then the fetcher's answer (its head line, and the body).

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use portcullis_ipc::{read_response, recv_fds, round_trip, write_request, Request, Response, PROTO_VERSION};

fn ask(port: u16) -> Result<(String, String, TcpStream), String> {
    let path = std::env::var("PORTCULLIS_SOCKET").unwrap_or_else(|_| "/atrium/sockets/portcullis/portcullis.sock".into());
    let mut s = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }).map_err(|e| e.to_string())? {
        Response::Hello { .. } => {}
        r => return Err(format!("hello: {r:?}")),
    }
    write_request(&mut s, &Request::OpenLocalPort { port }).map_err(|e| e.to_string())?;
    let (name, addr) = match read_response(&mut s).map_err(|e| e.to_string())? {
        Response::LocalPort { app_name, addr, .. } => (app_name, addr),
        Response::Error { message } => return Err(message),
        r => return Err(format!("{r:?}")),
    };
    let fd = recv_fds(&s, 1).map_err(|e| e.to_string())?.pop().ok_or("no socket")?;
    let sock = TcpStream::from(fd);
    let want: SocketAddr = addr.parse().map_err(|_| "no address")?;
    if sock.peer_addr().ok() != Some(want) {
        return Err(format!("the socket isn't to {addr}"));
    }
    Ok((name, addr, sock))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let Some(port) = a.get(1).and_then(|p| p.parse::<u16>().ok()) else {
        eprintln!("usage: local_port_ask <port> [<fetchd> <url>]");
        std::process::exit(2);
    };
    let (name, addr, sock) = match ask(port) {
        Ok(x) => x,
        Err(why) => {
            println!("refused: {why}");
            return;
        }
    };
    println!("ok {name} {addr}");
    let (Some(fetchd), Some(url)) = (a.get(2), a.get(3)) else { return };
    let (ours, theirs) = UnixStream::pair().expect("socketpair");
    let mut child = Command::new(fetchd).stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stdout(Stdio::null()).spawn().expect("start fetchd");
    portcullis_ipc::send_fds_with(&ours, &[sock.as_raw_fd()], format!("G {url}\n").as_bytes()).expect("send");
    drop(sock);
    let mut r = BufReader::new(&ours);
    let mut head = String::new();
    let _ = r.read_line(&mut head);
    print!("{head}");
    if let Some(len) = head.strip_prefix("ok ").and_then(|h| h.split(' ').nth(2)).and_then(|l| l.parse::<usize>().ok()) {
        let mut body = vec![0u8; len.min(1 << 20)];
        if r.read_exact(&mut body).is_ok() {
            println!("{}", String::from_utf8_lossy(&body));
        }
    }
    drop(r);
    drop(ours);
    let _ = child.wait();
}

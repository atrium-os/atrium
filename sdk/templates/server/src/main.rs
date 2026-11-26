//! {{title}}: a small HTTP server on port 8080 (template: server).
//!
//! It answers every request with one page. A request is bytes from the
//! network, so it is read with limits: at most 8 KiB of request head, a
//! few seconds to send it, and a few connections at a time. A real server
//! that parses more than this should parse in a worker (app-composition.md).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PORT: u16 = 8080;
/// The most of a request's head that is read.
const MAX_HEAD: usize = 8 * 1024;
/// Connections served at once; others are closed at once.
const MAX_CONNS: usize = 16;

const PAGE: &str = "<!doctype html><title>{{title}}</title><h1>{{title}}</h1><p>Served from Atrium.</p>\n";

fn serve(mut s: TcpStream) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    // Read until the blank line that ends the head, never past MAX_HEAD.
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            s.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
        let n = s.read(&mut buf[..(MAX_HEAD - head.len()).min(1024)])?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
        PAGE.len()
    );
    s.write_all(answer.as_bytes())
}

fn main() {
    let listener = match TcpListener::bind(("0.0.0.0", PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{{name}}: port {PORT}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("{{name}}: listening on port {PORT}");
    let busy = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(s) = conn else { continue };
        if busy.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            busy.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let busy = busy.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve(s) {
                eprintln!("{{name}}: {e}");
            }
            busy.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

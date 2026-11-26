//! A stand-in web server for scripts/vm-local-server-check.sh
//! (background-apps.md §4.4): on 9000 and 9090 (all addresses) it answers
//! each request with `served <port> <path> cookie=<Cookie or ->`, setting a
//! cookie; `/redirect` redirects to `/hello`, `/away` off this computer,
//! and `/page` answers HTML whose title is that line (so navigatord's
//! `Opened` shows it).

use std::io::{Read, Write};
use std::net::TcpListener;

fn answer(port: u16, req: &str) -> String {
    let path = req.split(' ').nth(1).unwrap_or("/");
    let cookie = req.lines().find_map(|l| l.strip_prefix("Cookie: ").or_else(|| l.strip_prefix("cookie: "))).unwrap_or("-");
    match path {
        "/redirect" => "HTTP/1.1 302 Found\r\nLocation: /hello\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "/away" => "HTTP/1.1 302 Found\r\nLocation: http://example.com/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "/page" | "/go" => {
            let line = format!("served {port} {path} cookie={}", cookie.trim());
            let body = format!("<!doctype html><html><head><title>{line}</title></head><body><p>{line}</p></body></html>");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: s={port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
        _ => {
            let body = format!("served {port} {path} cookie={}", cookie.trim());
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: s={port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
    }
}

fn main() {
    let mut threads = Vec::new();
    for port in [9000u16, 9090] {
        threads.push(std::thread::spawn(move || {
            let l = match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("local_http_probe: bind {port}: {e}");
                    std::process::exit(1);
                }
            };
            for mut c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    let mut got = 0;
                    while got < buf.len() {
                        match c.read(&mut buf[got..]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                        if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&buf[..got]).to_string();
                    let _ = c.write_all(answer(port, &req).as_bytes());
                });
            }
        }));
    }
    for t in threads {
        let _ = t.join();
    }
}
